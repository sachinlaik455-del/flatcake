//! Minimal HTTP/1.1 REST API on the same port as the JSON-line protocol.
//! Protocol detection is done in service.rs handle_connection.

use crate::platform::ServiceState;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

pub fn handle_http_rest(
    mut reader: BufReader<TcpStream>,
    mut stream: TcpStream,
    first_line: &str,
    state: Arc<ServiceState>,
) {
    let mut parts = first_line.trim().split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let full_path = parts.next().unwrap_or("").to_string();
    let path = full_path.split('?').next().unwrap_or("");
    let mut auth = String::new();
    let mut content_len = 0usize;
    loop {
        let mut hdr = String::new();
        if reader.read_line(&mut hdr).is_err() || hdr.trim().is_empty() {
            break;
        }
        let lower = hdr.to_lowercase();
        if lower.starts_with("authorization: bearer ") {
            auth = hdr[22..].trim().to_string();
        } else if lower.starts_with("content-length:") {
            content_len = hdr[15..].trim().parse().unwrap_or(0);
        }
    }
    if content_len > 64 * 1024 {
        send_json(&mut stream, 413, "{\"error\":\"payload too large\"}");
        return;
    }
    if auth != state.token {
        send_json(&mut stream, 401, "{\"error\":\"unauthorized\"}");
        return;
    }
    let (status, json) = route(&method, path, &state);
    send_json(&mut stream, status, &json);
}

fn send_json(stream: &mut TcpStream, status: u16, json: &str) {
    let reason = match status {
        200 => "OK", 201 => "Created", 400 => "Bad Request",
        401 => "Unauthorized", 404 => "Not Found", 405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
        json.len()
    );
}

fn route(method: &str, path: &str, state: &Arc<ServiceState>) -> (u16, String) {
    let path = path.split('?').next().unwrap_or(path);
    let req = |m: &str| format!("{{\"method\":\"{m}\"}}");
    match (method, path) {
        ("GET", "/api/v1/status") => (200, super::service::handle_request(state, &req("status"))),
        ("GET", "/api/v1/metrics") => (200, super::service::handle_request(state, &req("metrics"))),
        ("GET", "/api/v1/kv-cache") => (200, super::service::handle_request(state, &req("kv-cache"))),
        ("GET", "/api/v1/runtimes") => (200, super::service::handle_request(state, &req("runtimes"))),
        ("GET", "/api/v1/diagnostics") => (200, super::service::handle_request(state, &req("diagnostics"))),
        ("GET", "/api/v1/config") => (200, super::service::handle_request(state, &req("config"))),
        ("POST", "/api/v1/shutdown") => (200, super::service::handle_request(state, &req("shutdown"))),
        _ => (404, "{\"error\":\"not found\"}".to_string()),
    }
}
