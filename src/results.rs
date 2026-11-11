//! Benchmark run record writer — pinned results JSON schema (ARCHITECTURE §11).
//! Field names here are the contract with python/plot_results.py (EGM blocker #3).

use crate::config::Config;
use crate::types::Metrics;
use crate::util::{stats_json, Json};

pub fn write_run_json(
    cfg: &Config,
    system: &str,
    seed: u64,
    trace: &str,
    m: &Metrics,
    elapsed_s: f64,
    integrity_failures: u64,
    requests_issued: u64,
    path: &str,
) -> std::io::Result<()> {
    let mut j = Json::new();
    // Config echo (all §10 keys).
    let mut c = Json::new();
    c.kv_u64("dram_pool_bytes", cfg.dram_pool_bytes)
        .kv_f64("dram_high_watermark", cfg.dram_high_watermark)
        .kv_f64("dram_low_watermark", cfg.dram_low_watermark)
        .kv_u64("hbm_window_bytes", cfg.hbm_window_bytes)
        .kv_u64("gpu_file_bytes", cfg.gpu_file_bytes)
        .kv_u64("gpu_file_pool", cfg.gpu_file_pool)
        .kv_str("ssd_dir", &cfg.ssd_dir)
        .kv_str("baseline_ssd_dir", &cfg.baseline_ssd_dir)
        .kv_u64("bucket_bytes", cfg.bucket_bytes)
        .kv_u64("bucket_count", cfg.bucket_count)
        .kv_u64("block_tokens", cfg.block_tokens)
        .kv_u64("bytes_per_token", cfg.bytes_per_token)
        .kv_u64("tau_tokens", cfg.tau_tokens)
        .kv_u64("l_agent_tokens", cfg.l_agent_tokens)
        .kv_u64("gap_bridge_bytes", cfg.gap_bridge_bytes)
        .kv_f64("ssd_high_watermark", cfg.ssd_high_watermark)
        .kv_f64("ssd_low_watermark", cfg.ssd_low_watermark)
        .kv_u64("ssd_workers", cfg.ssd_workers as u64)
        .kv_u64("dram_workers", cfg.dram_workers as u64)
        .kv_u64("queue_cap", cfg.queue_cap as u64)
        .kv_u64("staging_pool_bytes", cfg.staging_pool_bytes)
        .kv_u64("recompute_us_per_token", cfg.recompute_us_per_token)
        .kv_u64("index_capacity", cfg.index_capacity)
        .kv_f64("time_scale", cfg.time_scale)
        .kv_u64("csv_window_us", cfg.csv_window_us);
    j.kv_nested("config", &c.build())
        .kv_str("system", system)
        .kv_u64("seed", seed)
        .kv_str("trace", trace);

    // Metrics (field names = Metrics fields = results-JSON keys).
    // Metrics object built manually for mixed int/float field types.
    let mut parts: Vec<String> = Vec::new();
    let kv_u = |parts: &mut Vec<String>, k: &str, v: u64| {
        parts.push(format!("\"{k}\":{v}"));
    };
    let kv_f = |parts: &mut Vec<String>, k: &str, v: f64| {
        parts.push(format!("\"{k}\":{v}"));
    };
    kv_f(&mut parts, "delivered_hit_rate", m.delivered_hit_rate());
    kv_u(&mut parts, "hit_hbm", m.hit_hbm);
    kv_u(&mut parts, "hit_dram", m.hit_dram);
    kv_u(&mut parts, "hit_ssd", m.hit_ssd);
    kv_u(&mut parts, "misses", m.misses);
    kv_u(&mut parts, "pending_racing_gets", m.pending_racing_gets);
    kv_f(&mut parts, "read_amp", m.read_amp());
    kv_f(&mut parts, "write_amp", m.write_amp());
    kv_u(&mut parts, "physical_read_ops", m.physical_read_ops);
    kv_u(&mut parts, "logical_block_gets", m.logical_block_gets);
    kv_u(&mut parts, "physical_read_bytes", m.physical_read_bytes);
    kv_u(&mut parts, "logical_read_bytes", m.logical_read_bytes);
    kv_u(&mut parts, "physical_write_bytes", m.physical_write_bytes);
    kv_u(&mut parts, "logical_write_bytes", m.logical_write_bytes);
    let coalesce = {
        let mut cj = Json::new();
        cj.kv_u64("merged_ranges", m.merged_ranges)
            .kv_u64("bridged_gaps", m.bridged_gaps)
            .kv_u64("overread_bytes", m.overread_bytes);
        cj.build()
    };
    parts.push(format!("\"coalesce\":{coalesce}"));
    parts.push(format!("\"queue_wait_us\":{}", stats_json(&m.qwait_samples)));
    parts.push(format!("\"prep_us\":{}", stats_json(&m.prep_samples)));
    parts.push(format!("\"latency_us\":{}", stats_json(&m.lat_samples)));
    kv_f(
        &mut parts,
        "throughput_blocks_per_s",
        m.throughput_blocks_per_s(elapsed_s),
    );
    kv_u(&mut parts, "mem_high_water_bytes", m.mem_high_water_bytes());
    kv_u(&mut parts, "dram_spill_puts", m.dram_spill_puts);
    kv_u(
        &mut parts,
        "ssd_over_high_watermark_us",
        m.ssd_over_high_watermark_us,
    );
    kv_u(&mut parts, "modeled_recompute_us", m.modeled_recompute_us);
    kv_u(&mut parts, "reclaim_events", m.reclaim_events);
    kv_u(&mut parts, "reclaim_payload_ops", m.reclaim_payload_ops);
    kv_u(&mut parts, "delivered_requests", m.delivered_requests);
    kv_u(&mut parts, "requests_issued", requests_issued);
    kv_u(&mut parts, "integrity_failures", integrity_failures);
    kv_f(&mut parts, "elapsed_s", elapsed_s);
    j.kv_nested("metrics", &format!("{{{}}}", parts.join(",")));

    let json = j.build();
    if let Some(d) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, json)?;
    Ok(())
}
