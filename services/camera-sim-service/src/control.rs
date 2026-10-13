//! Minimal control HTTP surface. Hand-rolled HTTP/1.1 to avoid a web-framework
//! dependency in the deployable image. Bound to a private/loopback address by
//! the operator.
//!
//! Requests permit at most 16 KiB of headers and 64 KiB in total. Content-Length
//! accepts decimal digits only; duplicate lengths and Transfer-Encoding are
//! rejected. Request reads and response writes each have a five-second absolute
//! deadline. Bodies are bytes and need not be UTF-8.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use camera_config::{
    parse_hex_code, ActionInvocationRequest, ActionOutcome, ActionRole, ExecutionContext,
    ObservationRecorder, SocketRole,
};
use camera_sim::{Engine, FaultSpec, PreparedResponderMutation, StateOverlay};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, Mutex, Notify};
use tokio::time::{timeout_at, Instant};

use crate::state_callback::Registry;
use crate::state_json::snapshot_json;
use crate::trace::TraceLog;
use crate::{DeclaredAuxListener, Metrics};

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Health {
    pub instance_id: String,
    pub profile: String,
    pub connection: String,
    pub command_addr: SocketAddr,
    pub knock_addr: Option<SocketAddr>,
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
            "knock_bind": self.knock_addr.map(|address| address.to_string()),
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

#[derive(Clone)]
pub struct Context {
    pub(crate) health: Health,
    pub(crate) engine: Arc<Mutex<Engine>>,
    pub(crate) state_notify: Arc<Notify>,
    pub(crate) callbacks: Registry,
    pub(crate) trace: TraceLog,
    pub(crate) observations: ObservationRecorder,
    pub(crate) metrics: Metrics,
    pub(crate) shutdown: broadcast::Sender<()>,
    pub(crate) declared_aux: Vec<(SocketRole, Arc<DeclaredAuxListener>)>,
    pub(crate) declared_aux_sync: Arc<Mutex<()>>,
}

pub async fn handle(mut stream: TcpStream, context: Context) {
    let req = match read_request(&mut stream, &context.metrics).await {
        Ok(Some(req)) => req,
        Ok(None) => return,
        Err(resp) => {
            write_response(&mut stream, &resp.status, &resp.body, &context.metrics).await;
            return;
        }
    };

    let resp = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/healthz") => {
            let sessions = if context.engine.lock().await.state().session_open {
                1
            } else {
                0
            };
            Response::ok(context.health.json(sessions))
        }
        ("GET", "/state") => {
            let body = {
                let engine = context.engine.lock().await;
                snapshot_json(&engine)
            };
            Response::ok(body)
        }
        ("GET", path) if path == "/trace" || path.starts_with("/trace?") => {
            match trace_after(path) {
                Ok(after) => Response::ok(context.trace.json(&context.health.instance_id, after)),
                Err(error) => Response::bad_request(error),
            }
        }
        ("GET", "/faults") => {
            let engine = context.engine.lock().await;
            match serde_json::to_string(&serde_json::json!({
                "ok": true,
                "faults": engine.faults(),
                "lastApplied": engine.last_applied_fault(),
            })) {
                Ok(body) => Response::ok(body),
                Err(error) => Response::server_error(error.to_string()),
            }
        }
        ("POST", "/faults") => match serde_json::from_slice::<FaultSpec>(&req.body) {
            Ok(spec) => {
                let mut engine = context.engine.lock().await;
                match engine.install_fault(spec) {
                    Ok(id) => Response::ok(serde_json::json!({ "ok": true, "id": id }).to_string()),
                    Err(error) => Response::bad_request(error),
                }
            }
            Err(error) => Response::bad_request(format!("invalid JSON fault request: {error}")),
        },
        ("DELETE", "/faults") => {
            context.engine.lock().await.clear_faults();
            Response::ok(r#"{"ok":true}"#.to_string())
        }
        ("DELETE", path) if path.starts_with("/faults/") => {
            let id = path.trim_start_matches("/faults/");
            match id.parse::<u64>() {
                Ok(id) if context.engine.lock().await.remove_fault(id) => {
                    Response::ok(r#"{"ok":true}"#.to_string())
                }
                Ok(_) => Response::not_found(),
                Err(_) => Response::bad_request("fault id must be an unsigned integer".into()),
            }
        }
        ("GET", path) if path == "/observations" || path.starts_with("/observations?") => {
            match cursor_after(path, "observations") {
                Ok(after) => match context.observations.export_json(after) {
                    Ok(body) => Response::ok(body),
                    Err(error) => Response::server_error(error.to_string()),
                },
                Err(error) => Response::bad_request(error),
            }
        }
        ("GET", "/actions") => {
            let catalog = context.engine.lock().await.manifest().action_catalog();
            match serde_json::to_string(&catalog) {
                Ok(body) => Response::ok(body),
                Err(error) => Response::server_error(error.to_string()),
            }
        }
        ("POST", "/control/advance-property") => {
            match advance_property(&context, &req.body).await {
                Ok(body) => {
                    context.state_notify.notify_one();
                    context.metrics.touch();
                    Response::ok(body)
                }
                Err(error) => Response::bad_request(error),
            }
        }
        ("POST", path) if path.starts_with("/actions/") => {
            let action_id = path.trim_start_matches("/actions/");
            if action_id.is_empty() || action_id.contains('/') {
                Response::not_found()
            } else {
                match invoke_action(&context, action_id, &req.body).await {
                    Ok(body) => Response::ok(body),
                    Err(ActionInvokeError::BadRequest(error)) => Response::bad_request(error),
                    Err(ActionInvokeError::Internal(error)) => Response::server_error(error),
                }
            }
        }
        ("PATCH", "/state") => match apply_state_patch(&context, &req.body).await {
            Ok(body) => {
                context.state_notify.notify_one();
                context.metrics.touch();
                Response::ok(body)
            }
            Err(e) => Response::bad_request(e),
        },
        ("POST", "/callbacks") => match subscribe_callback(&context.callbacks, &req.body).await {
            Ok(body) => {
                context.state_notify.notify_one();
                context.metrics.touch();
                Response::ok(body)
            }
            Err(e) => Response::bad_request(e),
        },
        ("POST", "/shutdown") => {
            let _ = context.shutdown.send(());
            Response::ok(r#"{"shutting_down":true}"#.to_string())
        }
        _ => Response::not_found(),
    };

    write_response(&mut stream, &resp.status, &resp.body, &context.metrics).await;
}

fn trace_after(path: &str) -> Result<u64, String> {
    cursor_after(path, "trace")
}

fn cursor_after(path: &str, endpoint: &str) -> Result<u64, String> {
    let Some((_, query)) = path.split_once('?') else {
        return Ok(0);
    };
    let mut after = 0;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("{endpoint} query parameters must use key=value"));
        };
        if key != "after" {
            return Err(format!("unknown {endpoint} query parameter '{key}'"));
        }
        after = value
            .parse::<u64>()
            .map_err(|_| format!("{endpoint} 'after' cursor must be an unsigned integer"))?;
    }
    Ok(after)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActionRequestBody {
    catalog_revision: String,
    mode: String,
    role: ActionRole,
    #[serde(default)]
    parameters: Vec<camera_config::ActionArgument>,
}

enum ActionInvokeError {
    BadRequest(String),
    Internal(String),
}

async fn invoke_action(
    context: &Context,
    action_id: &str,
    body: &[u8],
) -> Result<String, ActionInvokeError> {
    let request: ActionRequestBody = serde_json::from_slice(body).map_err(|error| {
        ActionInvokeError::BadRequest(format!("invalid JSON action request: {error}"))
    })?;
    let invocation = ActionInvocationRequest {
        catalog_revision: request.catalog_revision.clone(),
        action_id: action_id.to_string(),
        connection: context.health.connection.clone(),
        mode: request.mode.clone(),
        role: request.role,
        parameters: request.parameters.clone(),
    };
    let supplied = request
        .parameters
        .iter()
        .map(|argument| (argument.name.clone(), serde_json::json!(argument.value)))
        .collect();
    let mut engine = context.engine.lock().await;
    let resolved = match engine.manifest().resolve_action_invocation(&invocation) {
        Ok(resolved) => resolved,
        Err(error) => {
            context
                .observations
                .record_action(
                    execution_context(&context.health.connection, &request.mode, &engine),
                    request.catalog_revision,
                    action_id.to_string(),
                    request.role,
                    supplied,
                    ActionOutcome::Rejected,
                )
                .map_err(|error| ActionInvokeError::Internal(error.to_string()))?;
            return Err(ActionInvokeError::BadRequest(format!(
                "{}: {error}",
                error.code()
            )));
        }
    };
    let mutation = resolved.responder_mutation.as_ref().ok_or_else(|| {
        ActionInvokeError::Internal("resolved responder action has no mutation".into())
    })?;
    let prepared = engine
        .prepare_responder_mutation(mutation, &resolved.parameters)
        .map_err(ActionInvokeError::Internal)?;
    let (affected_objects, affected_properties) = match &prepared {
        PreparedResponderMutation::EnqueueObjects { affected, .. } => (*affected, Vec::new()),
        PreparedResponderMutation::PropertyTransition(transition) => (
            0,
            vec![serde_json::json!({
                "target": format!("0x{:04x}", transition.target),
                "initial": transition.initial,
                "terminal": transition.terminal,
                "settleAfterPolls": transition.settle_after_polls,
            })],
        ),
    };
    let parameters = resolved
        .parameters
        .iter()
        .map(|(name, value)| (name.clone(), serde_json::json!(value)))
        .collect();
    context
        .observations
        .record_action(
            execution_context(&context.health.connection, &request.mode, &engine),
            request.catalog_revision,
            action_id.to_string(),
            request.role,
            parameters,
            ActionOutcome::Succeeded,
        )
        .map_err(|error| ActionInvokeError::Internal(error.to_string()))?;
    engine
        .apply_responder_mutation(&prepared)
        .map_err(ActionInvokeError::Internal)?;
    context.state_notify.notify_one();
    context.metrics.touch();
    Ok(serde_json::json!({
        "ok": true,
        "actionId": action_id,
        "role": request.role,
        "affectedObjects": affected_objects,
        "affectedProperties": affected_properties,
        "queue": engine.transfer_queue_stats(),
    })
    .to_string())
}

fn execution_context(connection: &str, mode: &str, engine: &Engine) -> ExecutionContext {
    ExecutionContext {
        connection: connection.to_string(),
        mode: mode.to_string(),
        state: engine.phase().state_name().to_string(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AdvancePropertyRequest {
    code: String,
}

async fn advance_property(context: &Context, body: &[u8]) -> Result<String, String> {
    let request: AdvancePropertyRequest = serde_json::from_slice(body)
        .map_err(|error| format!("invalid JSON advance-property request: {error}"))?;
    let code = parse_hex_code(&request.code).ok_or_else(|| {
        format!(
            "invalid property code '{}' in advance-property request",
            request.code
        )
    })?;
    context
        .engine
        .lock()
        .await
        .advance_property_to_next_enum(code)?;
    Ok(serde_json::json!({
        "ok": true,
        "code": format!("{code:#06x}"),
    })
    .to_string())
}

async fn apply_state_patch(context: &Context, body: &[u8]) -> Result<String, String> {
    let overlay: StateOverlay =
        serde_json::from_slice(body).map_err(|e| format!("invalid JSON state overlay: {e}"))?;
    overlay.validate_context(&context.health.profile, &context.health.connection)?;
    let _declared_aux_sync = context.declared_aux_sync.lock().await;
    let (applied, aux_readiness) = {
        let mut engine = context.engine.lock().await;
        let applied = engine.apply_state_overlay(&overlay)?;
        let aux_readiness = context
            .declared_aux
            .iter()
            .map(|(role, listener)| (Arc::clone(listener), engine.channel_ready(*role)))
            .collect::<Vec<_>>();
        (applied, aux_readiness)
    };
    for (listener, ready) in aux_readiness {
        listener
            .set_ready(ready)
            .await
            .map_err(|error| error.to_string())?;
    }
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

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RequestError {
    TooLarge,
    Invalid(&'static str),
}

impl RequestError {
    fn response(self) -> Response {
        match self {
            Self::TooLarge => Response {
                status: "413 Payload Too Large".to_string(),
                body: r#"{"error":"request too large"}"#.to_string(),
            },
            Self::Invalid(message) => Response::bad_request(message.to_string()),
        }
    }
}

#[derive(Debug)]
struct Response {
    status: String,
    body: String,
}

impl Response {
    fn request_timeout() -> Self {
        Self {
            status: "408 Request Timeout".to_string(),
            body: r#"{"error":"request deadline exceeded"}"#.to_string(),
        }
    }

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

    fn server_error(error: String) -> Self {
        Self {
            status: "500 Internal Server Error".to_string(),
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
    write_response_before(
        stream,
        status,
        body,
        metrics,
        Instant::now() + RESPONSE_TIMEOUT,
    )
    .await;
}

async fn write_response_before<W: AsyncWrite + Unpin>(
    stream: &mut W,
    status: &str,
    body: &str,
    metrics: &Metrics,
    deadline: Instant,
) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = timeout_at(deadline, async {
        if stream.write_all(resp.as_bytes()).await.is_ok() {
            metrics.record_write(resp.len());
            let _ = stream.flush().await;
        }
    })
    .await;
}

async fn read_request(
    stream: &mut TcpStream,
    metrics: &Metrics,
) -> Result<Option<Request>, Response> {
    read_request_before(stream, metrics, Instant::now() + REQUEST_TIMEOUT).await
}

async fn read_request_before<R: AsyncRead + Unpin>(
    stream: &mut R,
    metrics: &Metrics,
    deadline: Instant,
) -> Result<Option<Request>, Response> {
    timeout_at(deadline, async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if Instant::now() >= deadline {
                return Err(Response::request_timeout());
            }
            // Never buffer beyond the parser's total request allowance.
            let available = chunk.len().min(MAX_REQUEST_BYTES - buf.len());
            let n = stream
                .read(&mut chunk[..available])
                .await
                .map_err(|e| Response::bad_request(format!("read request: {e}")))?;
            if Instant::now() >= deadline {
                return Err(Response::request_timeout());
            }
            if n == 0 {
                return if buf.is_empty() {
                    Ok(None)
                } else {
                    Err(Response::bad_request(
                        "client closed before full request".to_string(),
                    ))
                };
            }
            metrics.record_read(n);
            buf.extend_from_slice(&chunk[..n]);
            if let Some(req) = parse_request_if_complete(&buf).map_err(RequestError::response)? {
                return Ok(Some(req));
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err(Response::request_timeout()))
}

/// Parse one bounded request. Incomplete bodies are left unallocated.
/// Duplicate Content-Length fields are invalid even when their values agree.
pub fn parse_request_if_complete(buf: &[u8]) -> Result<Option<Request>, RequestError> {
    if buf.len() > MAX_REQUEST_BYTES {
        return Err(RequestError::TooLarge);
    }
    let Some(split) = buf.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
        return if buf.len() >= MAX_HEADER_BYTES {
            Err(RequestError::TooLarge)
        } else {
            Ok(None)
        };
    };
    let body_start = split.checked_add(4).ok_or(RequestError::TooLarge)?;
    if body_start > MAX_HEADER_BYTES {
        return Err(RequestError::TooLarge);
    }
    let headers = std::str::from_utf8(&buf[..split])
        .map_err(|_| RequestError::Invalid("request headers are not UTF-8"))?;
    let mut lines = headers.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if !is_token(method)
        || path.is_empty()
        || !path
            .bytes()
            .all(|byte| byte.is_ascii() && !byte.is_ascii_control())
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || parts.next().is_some()
    {
        return Err(RequestError::Invalid("malformed request line"));
    }
    let mut content_length = None;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or(RequestError::Invalid("malformed header"))?;
        if !is_token(name)
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(RequestError::Invalid("malformed header"));
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(RequestError::Invalid("Transfer-Encoding is unsupported"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(RequestError::Invalid("duplicate Content-Length"));
            }
            let value = value.trim_matches([' ', '\t']);
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(RequestError::Invalid("invalid Content-Length"));
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| RequestError::Invalid("invalid Content-Length"))?,
            );
        }
    }
    let body_end = body_start
        .checked_add(content_length.unwrap_or(0))
        .ok_or(RequestError::TooLarge)?;
    if body_end > MAX_REQUEST_BYTES {
        return Err(RequestError::TooLarge);
    }
    if buf.len() < body_end {
        return Ok(None);
    }
    Ok(Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        body: buf[body_start..body_end].to_vec(),
    }))
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

#[cfg(test)]
mod http_tests {
    use super::*;

    #[test]
    fn http_rejects_overflowing_declared_length() {
        let request = format!(
            "POST /state HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            usize::MAX
        );
        assert!(parse_request_if_complete(request.as_bytes()).is_err());
    }

    #[test]
    fn http_rejects_duplicate_lengths_and_transfer_encoding() {
        for headers in [
            "Content-Length: 1\r\nContent-Length: 1",
            "Content-Length: 1\r\ncontent-length: 2",
            "Transfer-Encoding: chunked",
            "Content-Length: 1\r\nTransfer-Encoding: identity",
        ] {
            let request = format!("POST /state HTTP/1.1\r\n{headers}\r\n\r\n");
            assert!(
                parse_request_if_complete(request.as_bytes()).is_err(),
                "{headers}"
            );
        }
    }

    #[test]
    fn http_requires_decimal_content_lengths() {
        for length in [
            "",
            "+1",
            "-1",
            "1.0",
            "0x10",
            "1, 1",
            "1 1",
            "184467440737095516150",
        ] {
            let request = format!("POST /state HTTP/1.1\r\nContent-Length: {length}\r\n\r\n");
            assert!(
                parse_request_if_complete(request.as_bytes()).is_err(),
                "{length}"
            );
        }
        let request = b"POST /state HTTP/1.1\r\ncOnTeNt-LeNgTh: \t003 \t\r\n\r\nabc";
        assert_eq!(
            parse_request_if_complete(request).unwrap().unwrap().body,
            b"abc"
        );
    }

    #[test]
    fn http_rejects_malformed_request_lines_and_headers() {
        for header in [
            "GET /state",
            "GET  /state HTTP/1.1",
            "GET /state HTTP/2.0",
            "GET /state HTTP/1.1 extra",
            "GET /state HTTP/1.1\r\nContent-Length 2",
            "GET /state HTTP/1.1\r\nContent-Length : 2",
            "GET /state HTTP/1.1\r\nX: a\nb",
        ] {
            let request = format!("{header}\r\n\r\n");
            assert!(
                parse_request_if_complete(request.as_bytes()).is_err(),
                "{header}"
            );
        }
        for version in ["HTTP/1.0", "HTTP/1.1"] {
            let request = format!("GET /healthz {version}\r\n\r\n");
            let request = parse_request_if_complete(request.as_bytes())
                .unwrap()
                .unwrap();
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/healthz");
            assert!(request.body.is_empty());
        }
    }

    fn request_at_size(total: usize) -> Vec<u8> {
        let mut length = total;
        loop {
            let header = format!("POST /state HTTP/1.1\r\nContent-Length: {length}\r\n\r\n");
            let adjusted = total - header.len();
            if adjusted == length {
                let mut request = header.into_bytes();
                request.resize(total, b'a');
                return request;
            }
            length = adjusted;
        }
    }

    #[test]
    fn http_enforces_announced_total_before_body_arrives() {
        let request = request_at_size(MAX_REQUEST_BYTES);
        let body_start = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert!(parse_request_if_complete(&request[..body_start])
            .unwrap()
            .is_none());
        let parsed = parse_request_if_complete(&request).unwrap().unwrap();
        assert_eq!(parsed.body.len(), MAX_REQUEST_BYTES - body_start);
        let oversized = request_at_size(MAX_REQUEST_BYTES + 1);
        let body_start = oversized
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        assert_eq!(
            parse_request_if_complete(&oversized[..body_start]),
            Err(RequestError::TooLarge)
        );
        assert_eq!(
            parse_request_if_complete(&oversized),
            Err(RequestError::TooLarge)
        );
    }

    #[test]
    fn http_bounds_headers_including_terminator() {
        let mut header = b"GET /state HTTP/1.1\r\nX-Pad: ".to_vec();
        header.resize(MAX_HEADER_BYTES - 4, b'a');
        header.extend_from_slice(b"\r\n\r\n");
        assert!(parse_request_if_complete(&header).unwrap().is_some());
        header.insert(MAX_HEADER_BYTES - 4, b'a');
        assert_eq!(
            parse_request_if_complete(&header),
            Err(RequestError::TooLarge)
        );
        assert_eq!(
            parse_request_if_complete(&vec![b'a'; MAX_HEADER_BYTES]),
            Err(RequestError::TooLarge)
        );
        assert!(parse_request_if_complete(&vec![b'a'; MAX_HEADER_BYTES - 1])
            .unwrap()
            .is_none());
    }

    #[test]
    fn http_accepts_every_fragment_of_headers_and_binary_body() {
        let mut request = b"POST /state HTTP/1.1\r\nContent-Length: 7\r\n\r\n".to_vec();
        let body = [0xe2, 0x82, 0xac, 0xff, 0x00, 0x0d, 0x0a];
        request.extend_from_slice(&body);
        for split in 0..request.len() {
            assert!(
                parse_request_if_complete(&request[..split])
                    .unwrap()
                    .is_none(),
                "split {split}"
            );
        }
        assert_eq!(
            parse_request_if_complete(&request).unwrap().unwrap().body,
            body
        );
    }

    #[tokio::test(start_paused = true)]
    async fn http_read_deadline_expires_despite_byte_drips() {
        let (mut reader, mut writer) = tokio::io::duplex(64);
        let producer = tokio::spawn(async move {
            for byte in b"GET /healthz HTTP/1.1\r\n\r\n" {
                if writer.write_all(&[*byte]).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let started = Instant::now();
        let result =
            read_request_before(&mut reader, &Metrics::default(), started + REQUEST_TIMEOUT).await;
        assert_eq!(result.unwrap_err().status, "408 Request Timeout");
        assert_eq!(Instant::now() - started, REQUEST_TIMEOUT);
        producer.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn http_read_accepts_fragmented_binary_body() {
        let request = b"POST /state HTTP/1.1\r\nContent-Length: 3\r\n\r\n\xe2\x82\xac";
        let (mut reader, mut writer) = tokio::io::duplex(64);
        let producer = tokio::spawn(async move {
            for byte in request {
                writer.write_all(&[*byte]).await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let result = read_request_before(
            &mut reader,
            &Metrics::default(),
            Instant::now() + REQUEST_TIMEOUT,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.body, [0xe2, 0x82, 0xac]);
        producer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn http_read_rejects_announced_oversize_without_body() {
        let request = format!(
            "POST /state HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST_BYTES
        );
        let (mut reader, mut writer) = tokio::io::duplex(1024);
        writer.write_all(request.as_bytes()).await.unwrap();
        let started = Instant::now();
        let result =
            read_request_before(&mut reader, &Metrics::default(), started + REQUEST_TIMEOUT).await;
        assert_eq!(result.unwrap_err().status, "413 Payload Too Large");
        assert_eq!(Instant::now(), started);
    }

    #[tokio::test(start_paused = true)]
    async fn http_response_write_has_absolute_deadline() {
        let (mut writer, _unread_peer) = tokio::io::duplex(1);
        let started = Instant::now();
        let metrics = Metrics::default();
        write_response_before(
            &mut writer,
            "400 Bad Request",
            "{}",
            &metrics,
            started + RESPONSE_TIMEOUT,
        )
        .await;
        assert_eq!(Instant::now() - started, RESPONSE_TIMEOUT);
        assert_eq!(metrics.snapshot().bytes_written, 0);
    }

    #[tokio::test]
    async fn http_distinguishes_empty_eof_from_truncated_request() {
        let mut empty = &b""[..];
        assert!(read_request_before(
            &mut empty,
            &Metrics::default(),
            Instant::now() + REQUEST_TIMEOUT
        )
        .await
        .unwrap()
        .is_none());
        let mut truncated = &b"GET /state"[..];
        assert_eq!(
            read_request_before(
                &mut truncated,
                &Metrics::default(),
                Instant::now() + REQUEST_TIMEOUT
            )
            .await
            .unwrap_err()
            .status,
            "400 Bad Request"
        );
    }
}
