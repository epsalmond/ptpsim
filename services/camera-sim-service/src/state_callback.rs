//! Optional `--state-callback <URL>` (#126): push a JSON snapshot of camera
//! state to an external observer (the client application dev panel's `POST /state`) every
//! time it changes. Fire-and-forget, debounced, failures logged at debug — it
//! never touches the PTP responder path.
//!
//! Shape mirrors the hand-rolled HTTP in `control.rs`: no web framework and no
//! HTTP-client dependency (which would re-bloat the deployable image). The
//! single hook is one `Notify::notify_one()` in the command loop; everything
//! else lives in the one `state_callback_loop` task spawned here.

use std::sync::Arc;
use std::time::Duration;

use camera_media_store::ObjectQuery;
use camera_sim::{Engine, Phase};
use ptp_core::dataset::PropValue;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, Mutex, Notify};

/// Coalesce a burst of changes into one push; also caps push rate under heavy
/// polling. Low enough to feel live in the dev panel.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// A dead/slow observer must never let POST tasks pile up.
const POST_TIMEOUT: Duration = Duration::from_secs(2);

fn phase_str(p: Phase) -> &'static str {
    match p {
        Phase::Disconnected => "disconnected",
        Phase::SessionOpen => "sessionOpen",
        Phase::ImageImport => "imageImport",
        Phase::LiveView => "liveView",
        Phase::Streaming => "streaming",
        Phase::Closed => "closed",
    }
}

/// Build the camera-state JSON body. Snake_case + `serde_json::json!` to match
/// `control.rs`'s `Health::json`. Returns the serialized String directly so the
/// push loop can dedup by string compare (no `Serialize` derive needed). Keyed
/// iteration (`BTreeMap`) makes the output deterministic for a given state.
fn snapshot_json(engine: &Engine) -> String {
    let state = engine.state();
    let props: serde_json::Map<String, serde_json::Value> = state
        .props
        .iter()
        .map(|(&code, val)| {
            let v = match val {
                PropValue::U8(x) => serde_json::json!(x),
                PropValue::U16(x) => serde_json::json!(x),
                PropValue::U32(x) => serde_json::json!(x),
                PropValue::U64(x) => serde_json::json!(x),
                PropValue::Str(s) => serde_json::json!(s),
            };
            (format!("0x{code:04x}"), v)
        })
        .collect();
    serde_json::json!({
        "phase": phase_str(state.phase),
        "session_open": state.session_open,
        "props": props,
        "media": { "objects": engine.store().handles(ObjectQuery::default()).len() },
    })
    .to_string()
}

/// A parsed `http://host[:port][/path]` target. Parsed once at startup so the
/// hot push path does no string work beyond the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    host: String,
    port: u16,
    path: String,
}

/// Parse `http://host[:port][/path]`. Returns None for non-http or malformed
/// input (the caller logs once and disables the callback). Handles bracketed
/// IPv6 (`http://[::1]:8770/state`) and bare host / host:port.
pub fn parse_url(url: &str) -> Option<Target> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if let Some(after_lb) = authority.strip_prefix('[') {
        // [ipv6]:port  or  [ipv6]
        let (h, tail) = after_lb.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if tail.is_empty() => 80,
            None => return None,
        };
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (authority.to_string(), 80),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some(Target { host, port, path })
}

/// Raw-TCP HTTP/1.1 POST of `body` to the target, bounded by `POST_TIMEOUT`.
/// Connection: close, response drained best-effort. Mirrors `control.rs`'s
/// hand-rolled request/response strings.
async fn post_json(target: &Target, body: &str) -> std::io::Result<()> {
    tokio::time::timeout(POST_TIMEOUT, async {
        let mut stream = TcpStream::connect((target.host.as_str(), target.port)).await?;
        let req = format!(
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            target.path,
            target.host,
            target.port,
            body.len(),
            body,
        );
        stream.write_all(req.as_bytes()).await?;
        stream.flush().await?;
        // Best-effort drain so the receiver can flush its response before close.
        let mut sink = [0u8; 256];
        let _ = stream.read(&mut sink).await;
        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "state-callback POST timed out",
        )
    })?
}

/// The single push task (spawned only when `--state-callback` is set). Wakes on
/// the shared `dirty` notify, debounces, snapshots the live engine state, and
/// POSTs it when it differs from the last push. Sequential ⇒ at most one POST in
/// flight; `Notify` coalesces changes that land during a POST. Ends on shutdown.
pub async fn state_callback_loop(
    dirty: Arc<Notify>,
    engine: Arc<Mutex<Engine>>,
    target: Target,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut last: Option<String> = None;
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = dirty.notified() => {
                tokio::time::sleep(DEBOUNCE).await; // coalesce a burst
                let body = {
                    let e = engine.lock().await;
                    snapshot_json(&e)
                };
                if last.as_deref() == Some(body.as_str()) {
                    continue; // unchanged since last push (e.g. read-only ops)
                }
                match post_json(&target, &body).await {
                    Ok(()) => {
                        tracing::debug!(bytes = body.len(), "state-callback pushed");
                        last = Some(body);
                    }
                    Err(e) => tracing::debug!(error = %e, "state-callback POST failed"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_url, Target};

    fn t(host: &str, port: u16, path: &str) -> Target {
        Target {
            host: host.into(),
            port,
            path: path.into(),
        }
    }

    #[test]
    fn parses_host_port_path() {
        assert_eq!(
            parse_url("http://127.0.0.1:8770/state"),
            Some(t("127.0.0.1", 8770, "/state"))
        );
    }

    #[test]
    fn defaults_port_80_and_root_path() {
        assert_eq!(
            parse_url("http://localhost/state"),
            Some(t("localhost", 80, "/state"))
        );
        assert_eq!(
            parse_url("http://127.0.0.1:9000"),
            Some(t("127.0.0.1", 9000, "/"))
        );
        assert_eq!(
            parse_url("http://example.com"),
            Some(t("example.com", 80, "/"))
        );
    }

    #[test]
    fn parses_bracketed_ipv6() {
        assert_eq!(
            parse_url("http://[::1]:8770/state"),
            Some(t("::1", 8770, "/state"))
        );
        assert_eq!(parse_url("http://[::1]"), Some(t("::1", 80, "/")));
    }

    #[test]
    fn rejects_non_http_and_malformed() {
        assert_eq!(parse_url("https://127.0.0.1/state"), None); // only http:// (local dev panel)
        assert_eq!(parse_url("ftp://x"), None);
        assert_eq!(parse_url("127.0.0.1:8770/state"), None); // missing scheme
        assert_eq!(parse_url("http://"), None);
        assert_eq!(parse_url("http://host:notaport/x"), None);
    }
}
