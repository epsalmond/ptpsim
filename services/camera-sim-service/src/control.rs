//! Minimal control HTTP surface. Hand-rolled HTTP/1.1 to avoid a web-framework
//! dependency in the deployable image. Endpoints: `GET /healthz` (the shape the
//! management sidecar polls into the `vcam_pool` inventory) and `POST /shutdown`
//! (graceful stop). Bound to a private/loopback address by the operator.

use std::net::SocketAddr;
use std::sync::Arc;

use camera_sim::{Engine, StateOverlay};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, Mutex, Notify};

use crate::state_callback::Registry;
use crate::state_json::snapshot_json;
use crate::Metrics;

const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct Health {
    pub instance_id: String,
    pub profile: String,
    pub connection: String,
    pub command_addr: SocketAddr,
    pub media_root: String,
    pub(crate) metrics: Metrics,
}

impl Health {
    fn json(&self, sessions: usize) -> String {
        let metrics = self.metrics.snapshot();
        serde_json::json!({
            "ok": true,
            "instance_id": self.instance_id,
            "profile": self.profile,
            "connection": self.connection,
            "bind": self.command_addr.to_string(),
            "sessions": sessions,
            "media_root": self.media_root,
            "metrics": {
                "uptime_ms": metrics.uptime_ms,
                "idle_ms": metrics.idle_ms,
                "bytes_read": metrics.bytes_read,
                "bytes_written": metrics.bytes_written,
                "bytes_transferred": metrics.bytes_transferred,
                "liveview_frames": metrics.liveview_frames,
                "memory_allocated_bytes": metrics.memory_allocated_bytes,
            },
        })
        .to_string()
    }
}

pub async fn handle(
    mut stream: TcpStream,
    health: Health,
    engine: Arc<Mutex<Engine>>,
    state_notify: Arc<Notify>,
    callbacks: Registry,
    metrics: Metrics,
    shutdown: broadcast::Sender<()>,
) {
    let req = match read_request(&mut stream, &metrics).await {
        Ok(Some(req)) => req,
        Ok(None) => return,
        Err(resp) => {
            write_response(&mut stream, &resp.status, &resp.body, &metrics).await;
            return;
        }
    };

    let resp = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/healthz") => {
            let sessions = if engine.lock().await.state().session_open {
                1
            } else {
                0
            };
            Response::ok(health.json(sessions))
        }
        ("GET", "/state") => {
            let body = {
                let engine = engine.lock().await;
                snapshot_json(&engine)
            };
            Response::ok(body)
        }
        ("PATCH", "/state") => match apply_state_patch(&health, &engine, &req.body).await {
            Ok(body) => {
                state_notify.notify_one();
                metrics.touch();
                Response::ok(body)
            }
            Err(e) => Response::bad_request(e),
        },
        ("POST", "/callbacks") => match subscribe_callback(&callbacks, &req.body).await {
            Ok(body) => {
                state_notify.notify_one();
                metrics.touch();
                Response::ok(body)
            }
            Err(e) => Response::bad_request(e),
        },
        ("POST", "/shutdown") => {
            let _ = shutdown.send(());
            Response::ok(r#"{"shutting_down":true}"#.to_string())
        }
        _ => Response::not_found(),
    };

    write_response(&mut stream, &resp.status, &resp.body, &metrics).await;
}

async fn apply_state_patch(
    health: &Health,
    engine: &Arc<Mutex<Engine>>,
    body: &[u8],
) -> Result<String, String> {
    let overlay: StateOverlay =
        serde_json::from_slice(body).map_err(|e| format!("invalid JSON state overlay: {e}"))?;
    overlay.validate_context(&health.profile, &health.connection)?;
    let applied = engine.lock().await.apply_state_overlay(&overlay)?;
    Ok(serde_json::json!({ "ok": true, "applied": applied }).to_string())
}

async fn subscribe_callback(callbacks: &Registry, body: &[u8]) -> Result<String, String> {
    let payload: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("invalid JSON callback body: {e}"))?;
    let url = payload
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "callback body must contain string field 'url'".to_string())?;
    let callbacks = callbacks.add_url(url).await?;
    Ok(serde_json::json!({ "ok": true, "callbacks": callbacks }).to_string())
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct Response {
    status: String,
    body: String,
}

impl Response {
    fn ok(body: String) -> Self {
        Self {
            status: "200 OK".to_string(),
            body,
        }
    }

    fn bad_request(error: String) -> Self {
        Self {
            status: "400 Bad Request".to_string(),
            body: serde_json::json!({ "error": error }).to_string(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: "404 Not Found".to_string(),
            body: r#"{"error":"not found"}"#.to_string(),
        }
    }
}

async fn write_response(stream: &mut TcpStream, status: &str, body: &str, metrics: &Metrics) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(resp.as_bytes()).await.is_ok() {
        metrics.record_write(resp.len());
    }
    let _ = stream.flush().await;
}

async fn read_request(
    stream: &mut TcpStream,
    metrics: &Metrics,
) -> Result<Option<Request>, Response> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| Response::bad_request(format!("read request: {e}")))?;
        if n == 0 {
            return Ok(None);
        }
        metrics.record_read(n);
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST_BYTES {
            return Err(Response {
                status: "413 Payload Too Large".to_string(),
                body: r#"{"error":"request too large"}"#.to_string(),
            });
        }
        if let Some(req) = parse_request_if_complete(&buf)? {
            return Ok(Some(req));
        }
    }
}

fn parse_request_if_complete(buf: &[u8]) -> Result<Option<Request>, Response> {
    let s = std::str::from_utf8(buf)
        .map_err(|e| Response::bad_request(format!("request is not UTF-8: {e}")))?;
    let Some(split) = s.find("\r\n\r\n") else {
        return Ok(None);
    };
    let headers = &s[..split];
    let mut lines = headers.lines();
    let line = lines.next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    if method.is_empty() || path.is_empty() {
        return Err(Response::bad_request("malformed request line".to_string()));
    }
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>())
        })
        .transpose()
        .map_err(|e| Response::bad_request(format!("invalid Content-Length: {e}")))?
        .unwrap_or(0);
    let body_start = split + 4;
    if buf.len() < body_start + content_length {
        return Ok(None);
    }
    Ok(Some(Request {
        method,
        path,
        body: buf[body_start..body_start + content_length].to_vec(),
    }))
}
