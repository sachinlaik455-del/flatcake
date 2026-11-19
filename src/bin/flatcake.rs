//! flatcake — terminal-first CLI for the FlatCake platform service.
//! Talks to a running service over loopback TCP using the discovery file.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

fn discovery_path(cfg: &fckv::config::Config) -> String {
    format!("{}/flatcake-service.json", cfg.ssd_dir)
}

fn read_discovery(cfg: &fckv::config::Config) -> Result<(u16, String), String> {
    let path = discovery_path(cfg);
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("service not running (no discovery file at {path})"))?;
    let port = text
        .split("\"port\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse::<u16>().ok())
        .ok_or("bad discovery file")?;
    let token = text
        .split("\"token\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .ok_or("bad discovery file")?
        .to_string();
    Ok((port, token))
}

fn query(port: u16, token: &str, method: &str) -> Result<String, String> {
    let req = format!("{{\"token\":\"{token}\",\"method\":\"{method}\"}}");
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|_| "service not running".to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    writeln!(stream, "{req}").map_err(|e| e.to_string())?;
    let mut resp = String::new();
    BufReader::new(stream)
        .read_line(&mut resp)
        .map_err(|e| e.to_string())?;
    Ok(resp.trim().to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let json = args.iter().any(|a| a == "--json");

    if cmd == "serve" {
        serve(&args[2..]);
        return;
    }

    let cfg = load_cfg();
    let (port, token) = match read_discovery(&cfg) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("flatcake: {e}");
            std::process::exit(1);
        }
    };
    let method = match cmd {
        "status" => "status",
        "runtimes" => "runtimes",
        "inspect" | "kv-cache" => "kv-cache",
        "metrics" => "metrics",
        "diagnostics" => "diagnostics",
        "config" => "config",
        "shutdown" => "shutdown",
        "--help" | "-h" | "help" | "" => {
            print_help();
            return;
        }
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            std::process::exit(2);
        }
    };
    match query(port, &token, method) {
        Ok(resp) => {
            if json {
                println!("{resp}");
            } else {
                println!("{}", pretty(resp));
            }
        }
        Err(e) => {
            eprintln!("flatcake: {e}");
            std::process::exit(1);
        }
    }
}

fn load_cfg() -> fckv::config::Config {
    match fckv::config::load("fckv.conf") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: {e} - using defaults");
            fckv::config::Config::default()
        }
    }
}

fn serve(_args: &[String]) {
    let cfg = load_cfg();
    let project = fckv::platform::project::project_id(None);
    let (listener, state, token, port) =
        match fckv::platform::service::bind(&cfg, project.clone()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("service bind failed: {e}");
                std::process::exit(1);
            }
        };
    let discovery = format!("{}/flatcake-service.json", cfg.ssd_dir);
    if let Err(e) = fckv::platform::service::write_discovery(&discovery, port, &token) {
        eprintln!("discovery write failed: {e}");
        std::process::exit(1);
    }
    eprintln!("flatcake service: project={project} port={port} discovery={discovery}");
    eprintln!("press Ctrl-C to stop");
    fckv::platform::service::accept_loop(listener, state.clone(), Arc::new(
        std::sync::atomic::AtomicBool::new(true),
    ));
    let _ = state;
}

fn pretty(s: String) -> String {
    let mut out = String::new();
    let mut indent = 0usize;
    for c in s.chars() {
        match c {
            '{' | '[' => {
                indent += 1;
                out.push(c);
                out.push('\n');
                out.push_str(&"  ".repeat(indent));
            }
            '}' | ']' => {
                out.push('\n');
                indent = indent.saturating_sub(1);
                out.push_str(&"  ".repeat(indent));
                out.push(c);
            }
            ',' => {
                out.push(c);
                out.push('\n');
                out.push_str(&"  ".repeat(indent));
            }
            _ => out.push(c),
        }
    }
    out
}

fn print_help() {
    eprintln!("flatcake — FlatCake platform CLI");
    eprintln!("usage: flatcake <command> [--json]");
    eprintln!("commands:");
    eprintln!("  serve        start the platform service (embedded engine)");
    eprintln!("  status       show service status");
    eprintln!("  runtimes     list connected runtimes");
    eprintln!("  inspect      inspect the current project's cache (alias: kv-cache)");
    eprintln!("  metrics      show engine metrics");
    eprintln!("  diagnostics  show diagnostics counters");
    eprintln!("  config       show active configuration");
    eprintln!("  kv-cache     KV-cache inspection (for /kv-cache adapters)");
    eprintln!("  shutdown     stop the service");
}
