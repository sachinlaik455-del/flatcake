//! Deterministic synthetic workload generator  + trace parsing.
//! Canonical generator is THIS module via `fckv gen-workload` (DECISION D17).
//! Session 1 = globally shared prompt. Real sessions start at 2. `shared_with`
//! names a session whose blocks are PREPENDED to the request (hybrid requests, OQ-14).

use crate::util::{splitmix64, Json};
use std::fs::File;
use std::io::{BufWriter, Write};

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Put,
    Get,
}

#[derive(Clone, Debug)]
pub struct TraceRecord {
    pub ts_us: u64,
    pub op: Op,
    pub session: u64,
    pub turn: u64,
    pub layers: u32,
    pub start_blk: u64,
    pub n_blk: u32,
    pub shared: u32,
    pub reply_tokens: u64,
    pub ctx_tokens: u64,
    pub seed: u64,
    pub shared_with: u64,
}

impl TraceRecord {
    pub fn to_jsonl(&self) -> String {
        let mut j = Json::new();
        j.kv_u64("ts_us", self.ts_us)
            .kv_str(
                "op",
                match self.op {
                    Op::Put => "put",
                    Op::Get => "get",
                },
            )
            .kv_u64("session", self.session)
            .kv_u64("turn", self.turn)
            .kv_u64("layers", self.layers as u64)
            .kv_u64("start_blk", self.start_blk)
            .kv_u64("n_blk", self.n_blk as u64)
            .kv_u64("shared", self.shared as u64)
            .kv_u64("reply_tokens", self.reply_tokens)
            .kv_u64("ctx_tokens", self.ctx_tokens)
            .kv_u64("seed", self.seed)
            .kv_u64("shared_with", self.shared_with);
        j.build()
    }

    pub fn parse_line(line: &str) -> Result<TraceRecord, String> {
        // Minimal JSON object parse for our own frozen schema (flat, all scalars).
        let get = |key: &str| -> Result<u64, String> {
            let pat = format!("\"{key}\":");
            let start = line
                .find(&pat)
                .ok_or_else(|| format!("missing field {key}"))?;
            let rest = &line[start + pat.len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit() && c != '-')
                .unwrap_or(rest.len());
            rest[..end]
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("bad value for {key}"))
        };
        let opv = {
            let pat = "\"op\":\"";
            let s = line.find(pat).ok_or("missing op")?;
            let rest = &line[s + pat.len()..];
            let e = rest.find('"').ok_or("bad op")?;
            rest[..e].to_string()
        };
        Ok(TraceRecord {
            ts_us: get("ts_us")?,
            op: match opv.as_str() {
                "put" => Op::Put,
                "get" => Op::Get,
                other => return Err(format!("unknown op {other}")),
            },
            session: get("session")?,
            turn: get("turn")?,
            layers: get("layers")? as u32,
            start_blk: get("start_blk")?,
            n_blk: get("n_blk")? as u32,
            shared: get("shared")? as u32,
            reply_tokens: get("reply_tokens")?,
            ctx_tokens: get("ctx_tokens")?,
            seed: get("seed")?,
            shared_with: get("shared_with")?,
        })
    }
}

pub fn parse_trace(path: &str) -> Result<Vec<TraceRecord>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(TraceRecord::parse_line)
        .collect()
}

#[derive(Clone)]
pub struct GenParams {
    pub regime: String, // agentic | oneshot | mixed
    pub sessions: u64,
    pub turns: u64,           // agentic turns per session
    pub layers: u32,
    pub blocks_per_turn: u32, // blocks per layer per turn
    pub shared_blocks: u32,   // shared prompt blocks per layer
    pub fanout: u32,          // sub-agent prefix replays per agentic turn
    pub reuse_rate: f64,      // oneshot: fraction of sessions that re-access shared prompt
    pub qps: f64,
    pub seed: u64,
    pub out: String,
    pub block_tokens: u64,
    pub bytes_per_token: u64,
    pub gpu_file_bytes: u64,
    pub ssd_budget_bytes: u64,
}

fn put_record(
    ts: u64,
    session: u64,
    turn: u64,
    layers: u32,
    start_blk: u64,
    n_blk: u32,
    shared: u32,
    reply: u64,
    ctx: u64,
    seed: u64,
) -> TraceRecord {
    TraceRecord {
        ts_us: ts,
        op: Op::Put,
        session,
        turn,
        layers,
        start_blk,
        n_blk,
        shared,
        reply_tokens: reply,
        ctx_tokens: ctx,
        seed,
        shared_with: 0,
    }
}

fn get_record(
    ts: u64,
    session: u64,
    turn: u64,
    layers: u32,
    n_blk: u32,
    shared: u32,
    reply: u64,
    ctx: u64,
    seed: u64,
    shared_with: u64,
) -> TraceRecord {
    TraceRecord {
        ts_us: ts,
        op: Op::Get,
        session,
        turn,
        layers,
        start_blk: 0,
        n_blk,
        shared,
        reply_tokens: reply,
        ctx_tokens: ctx,
        seed,
        shared_with,
    }
}

/// Generate the trace; returns (record_count, total_bytes_written_by_puts).
pub fn generate(p: &GenParams) -> Result<(u64, u64), String> {
    let block_bytes = p.block_tokens * p.bytes_per_token;
    let tok_per_turn = p.blocks_per_turn as u64 * p.block_tokens;
    let mut recs: Vec<TraceRecord> = Vec::new();
    let mut st = p.seed;

    // Session-size and SSD-budget guards.
    if p.regime != "oneshot" {
        let session_bytes = p.turns * tok_per_turn * p.layers as u64 * p.bytes_per_token;
        if session_bytes > p.gpu_file_bytes {
            return Err(format!(
                "agentic session would need {session_bytes} B > gpu_file_bytes {}; reduce turns/blocks_per_turn",
                p.gpu_file_bytes
            ));
        }
    }
    let session_bytes_total = match p.regime.as_str() {
        "oneshot" => p.sessions * tok_per_turn * p.layers as u64 * p.bytes_per_token,
        "shift" => {
            let agentic = p.sessions / 2;
            (p.sessions - agentic) * tok_per_turn * p.layers as u64 * p.bytes_per_token
                + agentic * p.turns * tok_per_turn * p.layers as u64 * p.bytes_per_token
        }
        _ => p.sessions * p.turns * tok_per_turn * p.layers as u64 * p.bytes_per_token,
    };
    if session_bytes_total > p.ssd_budget_bytes {
        return Err(format!(
            "workload needs {session_bytes_total} B of SSD > budget {}; reduce sessions/turns",
            p.ssd_budget_bytes
        ));
    }


    // Shared prompt (session 1): refcount > 1 => DRAM (paper Table 2 row 1).
    let shared_ctx = p.shared_blocks as u64 * p.layers as u64 * p.block_tokens;
    let s0 = splitmix64(&mut st);
    recs.push(put_record(
        0,
        1,
        1,
        p.layers,
        0,
        p.shared_blocks,
        1,
        0,
        shared_ctx,
        s0,
    ));

    let total_bytes = match p.regime.as_str() {
        "agentic" => {
            let mut written = 0u64;
            // Round-robin across sessions so several sessions are active concurrently.
            for turn in 1..=p.turns {
                for s in 2..=p.sessions + 1 {
                    let reply = 500; // short replies; the growing ctx makes the scope SSD-bound
                    let ctx_after = turn * tok_per_turn * p.layers as u64;
                    if turn > 1 {
                        let own_blocks = ((turn - 1) * p.blocks_per_turn as u64) as u32;
                        let ctx = (turn - 1) * tok_per_turn * p.layers as u64;
                        for _ in 0..=p.fanout {
                            recs.push(get_record(
                                0,
                                s,
                                turn,
                                p.layers,
                                own_blocks,
                                0,
                                reply,
                                ctx,
                                0,
                                1, // hybrid request: shared prompt + own history 
                            ));
                        }
                    }
                    let seed = splitmix64(&mut st);
                    recs.push(put_record(
                        0,
                        s,
                        turn,
                        p.layers,
                        (turn - 1) * p.blocks_per_turn as u64,
                        p.blocks_per_turn,
                        0,
                        reply,
                        ctx_after,
                        seed,
                    ));
                    written += p.blocks_per_turn as u64 * p.layers as u64 * block_bytes;
                }
            }
            written
        }
        "oneshot" => {
            let mut written = 0u64;
            for s in 2..=p.sessions + 1 {
                let seed = splitmix64(&mut st);
                let reply = 300;
                let ctx = tok_per_turn * p.layers as u64;
                recs.push(put_record(
                    0, s, 1, p.layers, 0, p.blocks_per_turn, 0, reply, ctx, seed,
                ));
                written += p.blocks_per_turn as u64 * p.layers as u64 * block_bytes;
                // Small fraction re-access ONLY the shared prompt later (long reuse
                // distance). The get targets SESSION 1 — the shared prompt's own
                // blocks (a real reuse of existing data, not re-reads of never-put
                // session-s keys).
                let r = (splitmix64(&mut st) % 1000) as f64 / 1000.0;
                if r < p.reuse_rate {
                    recs.push(get_record(
                        0,
                        1,
                        1,
                        p.layers,
                        p.shared_blocks,
                        1,
                        reply,
                        shared_ctx,
                        0,
                        0,
                    ));
                }
            }
            written
        }
        "shift" => {
            // Regime-shift trace : first half of the timeline is one-shot
            // sessions; agentic sessions activate only afterwards.
            let mut written = 0u64;
            let agentic = p.sessions / 2;
            let oneshot_n = p.sessions - agentic;
            // Phase 1: one-shot sessions (including reuse_rate shared-prompt re-gets).
            for s in 2..=oneshot_n + 1 {
                let seed = splitmix64(&mut st);
                let reply = 300;
                let ctx = tok_per_turn * p.layers as u64;
                recs.push(put_record(
                    0, s, 1, p.layers, 0, p.blocks_per_turn, 0, reply, ctx, seed,
                ));
                written += p.blocks_per_turn as u64 * p.layers as u64 * block_bytes;
                let r = (splitmix64(&mut st) % 1000) as f64 / 1000.0;
                if r < p.reuse_rate {
                    // Target SESSION 1 (the shared prompt's actual blocks).
                    recs.push(get_record(
                        0, 1, 1, p.layers, p.shared_blocks, 1, reply, shared_ctx, 0, 0,
                    ));
                }
            }
            // Phase 2: agentic sessions activate (round-robin turns after all puts).
            for turn in 1..=p.turns {
                for s in oneshot_n + 2..=p.sessions + 1 {
                    let reply = 500;
                    let ctx_after = turn * tok_per_turn * p.layers as u64;
                    if turn > 1 {
                        let own_blocks = ((turn - 1) * p.blocks_per_turn as u64) as u32;
                        let ctx = (turn - 1) * tok_per_turn * p.layers as u64;
                        recs.push(get_record(
                            0, s, turn, p.layers, own_blocks, 0, reply, ctx, 0, 1,
                        ));
                    }
                    let seed = splitmix64(&mut st);
                    recs.push(put_record(
                        0,
                        s,
                        turn,
                        p.layers,
                        (turn - 1) * p.blocks_per_turn as u64,
                        p.blocks_per_turn,
                        0,
                        reply,
                        ctx_after,
                        seed,
                    ));
                    written += p.blocks_per_turn as u64 * p.layers as u64 * block_bytes;
                }
            }
            written
        }
        "mixed" => {
            // Half the sessions agentic, half one-shot, interleaved by ts order below.
            let mut written = 0u64;
            let agentic = p.sessions / 2;
            let mut one_recs = Vec::new();
            for turn in 1..=p.turns {
                for s in 2..=agentic + 1 {
                    let reply = 500;
                    let ctx_after = turn * tok_per_turn * p.layers as u64;
                    if turn > 1 {
                        let own_blocks = ((turn - 1) * p.blocks_per_turn as u64) as u32;
                        let ctx = (turn - 1) * tok_per_turn * p.layers as u64;
                        recs.push(get_record(
                            u64::MAX, // sorted below
                            s,
                            turn,
                            p.layers,
                            own_blocks,
                            0,
                            reply,
                            ctx,
                            0,
                            1,
                        ));
                    }
                    let seed = splitmix64(&mut st);
                    recs.push(put_record(
                        u64::MAX,
                        s,
                        turn,
                        p.layers,
                        (turn - 1) * p.blocks_per_turn as u64,
                        p.blocks_per_turn,
                        0,
                        reply,
                        ctx_after,
                        seed,
                    ));
                    written += p.blocks_per_turn as u64 * p.layers as u64 * block_bytes;
                }
            }
            for s in agentic + 2..=p.sessions + 1 {
                let seed = splitmix64(&mut st);
                one_recs.push(put_record(
                    u64::MAX,
                    s,
                    1,
                    p.layers,
                    0,
                    p.blocks_per_turn,
                    0,
                    300,
                    tok_per_turn * p.layers as u64,
                    seed,
                ));
                written += p.blocks_per_turn as u64 * p.layers as u64 * block_bytes;
                let r = (splitmix64(&mut st) % 1000) as f64 / 1000.0;
                if r < p.reuse_rate {
                    one_recs.push(get_record(
                        u64::MAX,
                        1, // the shared prompt's own blocks (session 1)
                        1,
                        p.layers,
                        p.shared_blocks,
                        1,
                        300,
                        shared_ctx,
                        0,
                        0,
                    ));
                }
            }
            recs.extend(one_recs);
            // Pace: assign ts by final order (interleaved deterministically).
            for (i, r) in recs.iter_mut().enumerate() {
                r.ts_us = (i as f64 / p.qps * 1_000_000.0) as u64;
            }
            written
        }
        other => return Err(format!("unknown regime {other}")),
    };

    // Uniform open-loop pacing: ts_us from record position (ARCHITECTURE §9).
    for (i, r) in recs.iter_mut().enumerate() {
        r.ts_us = (i as f64 / p.qps * 1_000_000.0) as u64;
    }

    // Write JSONL.
    if let Some(dir) = std::path::Path::new(&p.out).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
    }
    let f = File::create(&p.out).map_err(|e| format!("create {}: {e}", p.out))?;
    let mut w = BufWriter::new(f);
    for r in &recs {
        writeln!(w, "{}", r.to_jsonl()).map_err(|e| format!("write: {e}"))?;
    }
    w.flush().map_err(|e| format!("flush: {e}"))?;
    Ok((recs.len() as u64, total_bytes))
}
