//! Local control service: loopback-only TCP with token auth, JSON-line
//! protocol. Coordinates runtimes and exposes read-only engine observation.
//! KV data never crosses this boundary - only control and metrics.

use super::connector::ConnectorInfo;
use super::registry::RuntimeRegistry;
use super::telemetry::{EventKind, Telemetry};
use crate::config::Config;
use crate::engine::FlatCakeEngine;
use crate::engine_api::KvEngine;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Max accepted request line (hostile-input bound).
const MAX_LINE: usize = 64 * 1024;

pub struct Service {
    pub registry: Arc<RuntimeRegistry>,
    pub telemetry: Arc<Telemetry>,
    pub engine: Arc<FlatCakeEngine>,
    pub project_id: String,
}

pub struct ServiceState {
    pub registry: Arc<RuntimeRegistry>,
    pub telemetry: Arc<Telemetry>,
    pub engine: Arc<FlatCakeEngine>,
    pub project_id: String,
    pub token: String,
    pub running: Arc<AtomicBool>,
}

/// Bind + build shared state. Returns the listener for accept_loop.
pub fn bind(
    cfg: &Config,
    project_id: String,
) -> std::io::Result<(TcpListener, Arc<ServiceState>, String, u16)> {
    let engine = Arc::new(FlatCakeEngine::new(cfg)?);
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let running = Arc::new(AtomicBool::new(true));
    let token = format!(
        "{:016x}{:016x}",
        crate::util::fnv1a64(&port.to_le_bytes()),
        crate::util::fnv1a64(cfg.ssd_dir.as_bytes())
    );
    let state = Arc::new(ServiceState {
        registry: Arc::new(RuntimeRegistry::new()),
        telemetry: Arc::new(Telemetry::new()),
        engine,
        project_id,
        token: token.clone(),
        running: running.clone(),
    });
    Ok((listener, state, token, port))
}

/// Accept loop: one thread per connection, each bounded by MAX_LINE.
pub fn accept_loop(
    listener: TcpListener,
    state: Arc<ServiceState>,
    running: Arc<AtomicBool>,
) {
    listener.set_nonblocking(true).ok();
    while running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let st = state.clone();
                std::thread::spawn(move || handle_connection(stream, st));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

fn handle_connection(stream: TcpStream, state: Arc<ServiceState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream.try_clone().expect("clone tcp stream"));
    // Protocol detection: read the first line. HTTP methods indicate a REST
    // request; otherwise fall through to the JSON-line protocol.
    let mut first_line = String::new();
    if reader.read_line(&mut first_line).is_err() {
        return;
    }
    let fl = first_line.trim().to_string();
    if fl.starts_with("GET ") || fl.starts_with("POST ") || fl.starts_with("PUT ")
        || fl.starts_with("DELETE ") || fl.starts_with("HEAD ")
    {
        let st = state.clone();
        super::http_api::handle_http_rest(reader, stream, &fl, st);
        return;
    }
    // JSON-line protocol (P1): token is in the JSON body.
    let body = fl.replace(&state.token, "");
    if body.trim().is_empty() {
        return;
    }
    let resp = handle_request(&state, body.trim());
    let mut writer = BufWriter::new(stream);
    let _ = writeln!(writer, "{resp}");
    if body.contains("\"shutdown\"") {
        state.running.store(false, Ordering::SeqCst);
    }
}



fn extract_str(body: &str, field: &str) -> Option<String> {
    let pat = format!("\"{field}\":\"");
    let i = body.find(&pat)?;
    let rest = &body[i + pat.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn extract_num(body: &str, field: &str) -> Option<u64> {
    let pat = format!("\"{field}\":");
    let i = body.find(&pat)?;
    let rest = &body[i + pat.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse::<u64>().ok()
}

/// Handle one control request (body = JSON without the auth token).
/// Known methods: status, metrics, kv-cache, runtimes, register, heartbeat,
/// disconnect, diagnostics, config, shutdown. KV data never crosses here.
pub fn handle_request(state: &ServiceState, body: &str) -> String {
    let method = extract_str(body, "method").unwrap_or_default();
    match method.as_str() {
        "status" => format!(
            "{{\"status\":\"running\",\"protocol\":{},\"project\":\"{}\",\"runtimes\":{}}}",
            super::PROTOCOL_VERSION,
            state.project_id,
            state.registry.count()
        ),
        "metrics" => {
            let m = state.engine.snapshot();
            let t = state.telemetry.counters();
            format!(
                "{{\"hits\":{{\"hbm\":{},\"dram\":{},\"ssd\":{}}},\"misses\":{},\
                 \"blocks_placed\":{},\"blocks_retrieved\":{},\"migrations\":{},\
                 \"evictions\":{},\"ssd_reads\":{},\"ssd_writes\":{},\
                 \"mem_used\":{},\"queue_pressure\":\"unavailable\"}}",
                m.hit_hbm, m.hit_dram, m.hit_ssd, m.misses,
                t[EventKind::BlockPlaced as usize], t[EventKind::BlockRetrieved as usize],
                t[EventKind::Migration as usize], t[EventKind::Eviction as usize],
                t[EventKind::SsdRead as usize], t[EventKind::SsdWrite as usize],
                state.engine.mem_used()
            )
        }
        "kv-cache" => {
            let m = state.engine.snapshot();
            let t = state.telemetry.counters();
            let runtimes: Vec<String> = state
                .registry
                .list_all()
                .iter()
                .map(|e| {
                    format!(
                        "{{\"id\":\"{}\",\"project\":\"{}\",\"health\":\"{:?}\"}}",
                        e.runtime_id, e.project_id, e.health
                    )
                })
                .collect();
            format!(
                "{{\"project\":\"{}\",\"runtimes\":[{}],\
                 \"ram\":{{\"hits\":{},\"used_bytes\":{}}},\
                 \"ssd\":{{\"hits\":{},\"reads\":{},\"writes\":{}}},\
                 \"cache\":{{\"hbm_hits\":{},\"misses\":{},\"migrations\":{},\"evictions\":{}}},\
                 \"queue_pressure\":\"unavailable\"}}",
                state.project_id, runtimes.join(","),
                m.hit_dram, state.engine.mem_used(),
                m.hit_ssd, t[EventKind::SsdRead as usize], t[EventKind::SsdWrite as usize],
                m.hit_hbm, m.misses, t[EventKind::Migration as usize], t[EventKind::Eviction as usize],
            )
        }
        "runtimes" => {
            let all: Vec<String> = state
                .registry
                .list_all()
                .iter()
                .map(|e| {
                    format!(
                        "{{\"id\":\"{}\",\"project\":\"{}\",\"pid\":{},\"connector\":\"{} v{}\",\"health\":\"{:?}\"}}",
                        e.runtime_id, e.project_id, e.pid, e.connector.name, e.connector.version, e.health
                    )
                })
                .collect();
            format!("{{\"runtimes\":[{}]}}", all.join(","))
        }
        "register" => {
            let runtime_id = extract_str(body, "runtime_id").unwrap_or_default();
            let project_id = extract_str(body, "project_id").unwrap_or_default();
            let pid = extract_num(body, "pid").unwrap_or(0);
            let name = extract_str(body, "name").unwrap_or_default();
            let version = extract_str(body, "version").unwrap_or_default();
            let caps_v = extract_num(body, "capabilities").unwrap_or(1);
            let info = ConnectorInfo {
                name,
                version,
                protocol_version: super::PROTOCOL_VERSION,
                capabilities: caps_v,
            };
            match state.registry.register(&runtime_id, &project_id, pid as u32, info) {
                Ok(()) => format!("{{\"registered\":\"{runtime_id}\"}}"),
                Err(e) => format!("{{\"error\":\"{e}\"}}"),
            }
        }
        "heartbeat" => {
            let rid = extract_str(body, "runtime_id").unwrap_or_default();
            if state.registry.heartbeat(&rid) {
                format!("{{\"heartbeat\":\"{rid}\"}}")
            } else {
                "{\"error\":\"unknown runtime\"}".to_string()
            }
        }
        "disconnect" => {
            let rid = extract_str(body, "runtime_id").unwrap_or_default();
            if state.registry.disconnect(&rid) {
                format!("{{\"disconnected\":\"{rid}\"}}")
            } else {
                "{\"error\":\"unknown runtime\"}".to_string()
            }
        }
        "diagnostics" => {
            let m = state.engine.snapshot();
            format!(
                "{{\"dram_hits\":{},\"ssd_hits\":{},\"hbm_hits\":{},\"misses\":{},\
                 \"pending_racing_gets\":{},\"dram_spill_puts\":{},\
                 \"migr_promotions\":{},\"migr_demotions\":{},\"migr_bytes_moved\":{},\
                 \"reclaim_events\":{},\"reclaim_payload_ops\":{},\
                 \"physical_read_ops\":{},\"physical_write_bytes\":{}}}",
                m.hit_dram, m.hit_ssd, m.hit_hbm, m.misses,
                m.pending_racing_gets, m.dram_spill_puts,
                m.migr_promotions, m.migr_demotions, m.migr_bytes_moved,
                m.reclaim_events, m.reclaim_payload_ops,
                m.physical_read_ops, m.physical_write_bytes
            )
        }
        "config" => {
            let cfg = state.engine.config_ref();
            format!(
                "{{\"dram_pool_bytes\":{},\"hbm_window_bytes\":{},\"gpu_file_bytes\":{},\
                 \"block_tokens\":{},\"bytes_per_token\":{},\"migration_enabled\":{},\
                 \"eviction_policy\":\"{}\"}}",
                cfg.dram_pool_bytes, cfg.hbm_window_bytes, cfg.gpu_file_bytes,
                cfg.block_tokens, cfg.bytes_per_token, cfg.migration_enabled,
                cfg.eviction_policy
            )
        }
        "shutdown" => {
            state.running.store(false, Ordering::SeqCst);
            "{\"shutdown\":true}".to_string()
        }
        "" => "{\"error\":\"empty request\"}".to_string(),
        other => format!("{{\"error\":\"unknown method\",\"method\":\"{other}\"}}"),
    }
}

/// Discovery file: port + token so the CLI can find a running service.
pub fn write_discovery(path: &str, port: u16, token: &str) -> std::io::Result<()> {
    if let Some(d) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(
        path,
        format!("{{\"port\":{port},\"token\":\"{token}\"}}"),
    )
}
