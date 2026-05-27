//! Minimal control HTTP surface. Hand-rolled HTTP/1.1 to avoid a web-framework
//! dependency in the deployable image. Endpoints: `GET /healthz` (the shape the
//! management sidecar polls into the `vcam_pool` inventory) and `POST /shutdown`
//! (graceful stop). Bound to a private/loopback address by the operator.

use std::net::SocketAddr;
use std::sync::Arc;

use camera_sim::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, Mutex};

#[derive(Clone)]
pub struct Health {
    pub instance_id: String,
    pub profile: String,
    pub command_addr: SocketAddr,
    pub media_root: String,
}

impl Health {
    fn json(&self, sessions: usize) -> String {
        format!(
            r#"{{"ok":true,"instance_id":"{}","profile":"{}","bind":"{}","sessions":{},"media_root":"{}"}}"#,
            self.instance_id, self.profile, self.command_addr, sessions, self.media_root
        )
    }
}

pub async fn handle(
    mut stream: TcpStream,
    health: Health,
    engine: &Arc<Mutex<Engine>>,
    shutdown: broadcast::Sender<()>,
) {
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf).await {
        Ok(n) => n,
        Err(_) => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let line = req.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let (status, body) = match (method, path) {
        ("GET", "/healthz") => {
            let sessions = if engine.lock().await.state().session_open {
                1
            } else {
                0
            };
            ("200 OK", health.json(sessions))
        }
        ("POST", "/shutdown") => {
            let _ = shutdown.send(());
            ("200 OK", r#"{"shutting_down":true}"#.to_string())
        }
        _ => ("404 Not Found", r#"{"error":"not found"}"#.to_string()),
    };

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}
