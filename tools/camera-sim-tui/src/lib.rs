use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HealthSnapshot {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub connection: String,
    #[serde(default, rename = "bind")]
    pub command_bind: String,
    #[serde(default)]
    pub sessions: usize,
    #[serde(default)]
    pub media_root: String,
    #[serde(default)]
    pub metrics: ServiceMetrics,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ServiceMetrics {
    #[serde(default)]
    pub uptime_ms: u64,
    #[serde(default)]
    pub idle_ms: u64,
    #[serde(default)]
    pub bytes_read: u64,
    #[serde(default)]
    pub bytes_written: u64,
    #[serde(default)]
    pub bytes_transferred: u64,
    #[serde(default)]
    pub liveview_frames: u64,
    #[serde(default)]
    pub memory_allocated_bytes: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TraceSnapshot {
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub cursor: u64,
    #[serde(default)]
    pub events: Vec<LifecycleTraceEvent>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LifecycleTraceEvent {
    pub sequence: u64,
    pub elapsed_ms: u64,
    pub kind: String,
    pub local_endpoint: Option<String>,
    pub peer_endpoint: Option<String>,
    pub target_endpoint: Option<String>,
    pub payload_hex: Option<String>,
    #[serde(default)]
    pub payload_length: Option<usize>,
    #[serde(default)]
    pub payload_truncated: bool,
    pub outcome: Option<String>,
    #[serde(default)]
    pub outcome_truncated: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub error_truncated: bool,
}

impl LifecycleTraceEvent {
    pub fn display_line(&self) -> String {
        let peer = self.peer_endpoint.as_deref().unwrap_or("unknown peer");
        let target = self.target_endpoint.as_deref().unwrap_or("unknown target");
        match self.kind.as_str() {
            "pcss.discovery.received" => format!("PCSS advertisement received from {peer}"),
            "pcss.discovery.rejected" => format!("PCSS advertisement rejected from {peer}"),
            "pcss.callback.connect_started" => {
                format!("PCSS callback to {target} initiated")
            }
            "pcss.callback.connected" => format!("PCSS callback to {target} connected"),
            "pcss.callback.connect_failed" => {
                format!("PCSS callback to {target} failed")
            }
            "pcss.notify.sent" => format!("PCSS NOTIFY sent to {target}"),
            "pcss.notify.failed" => format!("PCSS NOTIFY to {target} failed"),
            "pcss.callback_ack.received" => {
                format!("PCSS callback acknowledgement received from {peer}")
            }
            "pcss.callback_ack.invalid" | "pcss.callback_ack.failed" => {
                format!("PCSS callback acknowledgement failed from {peer}")
            }
            "ptpip.command.accepted" => format!("PTP/IP command connection accepted from {peer}"),
            "ptpip.init_request.received" => format!(
                "PTP/IP InitCommandRequest {}",
                self.outcome.as_deref().unwrap_or("received")
            ),
            "ptpip.init_request.rejected" => format!(
                "PTP/IP InitCommandRequest {} rejected",
                self.outcome.as_deref().unwrap_or("")
            ),
            "ptpip.init_fail.sent" => format!(
                "PTP/IP InitFail {} sent",
                outcome_field(self.outcome.as_deref(), "reason").unwrap_or("reason=unknown")
            ),
            "ptpip.init_ack.sent" => "PTP/IP InitCommandAck sent".to_string(),
            "ptpip.first_operation.received" => format!(
                "First PTP operation received ({})",
                self.outcome.as_deref().unwrap_or("unknown")
            ),
            "ptpip.first_operation.rejected" => "First PTP operation rejected".to_string(),
            other => format!("{other} {}", self.outcome.as_deref().unwrap_or(""))
                .trim_end()
                .to_string(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
            || self.kind.ends_with(".failed")
            || self.kind.ends_with(".invalid")
            || self.kind.ends_with(".rejected")
    }
}

fn outcome_field<'a>(outcome: Option<&'a str>, key: &str) -> Option<&'a str> {
    outcome?
        .split(';')
        .find(|field| field.starts_with(&format!("{key}=")))
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CameraSnapshot {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub session_open: bool,
    #[serde(default)]
    pub props: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub property_labels: BTreeMap<String, String>,
    #[serde(default)]
    pub media: MediaSnapshot,
    #[serde(default)]
    pub transfer_queues: TransferQueuesSnapshot,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MediaSnapshot {
    #[serde(default)]
    pub objects: usize,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TransferQueuesSnapshot {
    #[serde(default)]
    pub standard: Option<QueueSnapshot>,
    #[serde(default)]
    pub camera_initiated: Option<QueueSnapshot>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct QueueSnapshot {
    #[serde(default)]
    pub queued: usize,
    #[serde(default)]
    pub completed: usize,
    #[serde(default)]
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HttpBinding {
    pub method: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionDescriptor {
    pub id: String,
    pub label: String,
    pub hotkey: Option<char>,
    pub description: String,
    pub http: HttpBinding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKind {
    Manifest { body: String },
    Patch { body: String },
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub descriptor: ActionDescriptor,
    pub kind: ActionKind,
}

impl Action {
    pub fn id(&self) -> &str {
        &self.descriptor.id
    }

    pub fn request_body(&self) -> Option<&str> {
        match &self.kind {
            ActionKind::Manifest { body } | ActionKind::Patch { body } => Some(body),
            ActionKind::Quit => None,
        }
    }

    pub fn is_quit(&self) -> bool {
        matches!(self.kind, ActionKind::Quit)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestActionCatalog {
    pub revision: String,
    #[serde(default)]
    pub actions: Vec<ManifestAction>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestAction {
    pub action_id: String,
    pub connection: String,
    pub mode: String,
    #[serde(default)]
    pub supported_roles: Vec<String>,
    #[serde(default)]
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub triggers: serde_json::Value,
    #[serde(default)]
    pub availability: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ActionRegistry {
    actions: Vec<Action>,
    catalog: ManifestActionCatalog,
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self::core()
    }
}

impl ActionRegistry {
    pub fn core() -> Self {
        Self {
            actions: vec![
                patch_action(
                    "operator:session-open",
                    "/operator/actions/session-open",
                    "Session",
                    's',
                    "Open a generic simulator session",
                    r#"{"phase":"sessionOpen","session_open":true}"#,
                ),
                patch_action(
                    "operator:live-view",
                    "/operator/actions/live-view",
                    "Live View",
                    'v',
                    "Move the simulator to live-view ready state",
                    r#"{"phase":"liveView","session_open":true}"#,
                ),
                patch_action(
                    "operator:streaming",
                    "/operator/actions/streaming",
                    "Stream",
                    'r',
                    "Move the simulator to active streaming state",
                    r#"{"phase":"streaming","session_open":true}"#,
                ),
                patch_action(
                    "operator:image-import",
                    "/operator/actions/image-import",
                    "Import",
                    'i',
                    "Move the simulator to image import state",
                    r#"{"phase":"imageImport","session_open":true}"#,
                ),
                patch_action(
                    "operator:disconnect",
                    "/operator/actions/disconnect",
                    "Disconnect",
                    'd',
                    "Return the simulator to disconnected state",
                    r#"{"phase":"disconnected","session_open":false}"#,
                ),
                Action {
                    descriptor: ActionDescriptor {
                        id: "operator:quit".into(),
                        label: "Quit".into(),
                        hotkey: Some('q'),
                        description: "Exit the operator console".into(),
                        http: HttpBinding {
                            method: "POST".into(),
                            path: "/operator/actions/quit".into(),
                        },
                    },
                    kind: ActionKind::Quit,
                },
            ],
            catalog: ManifestActionCatalog::default(),
        }
    }

    pub fn from_catalog(catalog: ManifestActionCatalog) -> Self {
        let mut registry = Self::core();
        let mut key_index = 1u32;
        for entry in &catalog.actions {
            if !entry.supported_roles.iter().any(|role| role == "responder") {
                continue;
            }
            let hotkey = char::from_digit(key_index, 10);
            key_index = key_index.saturating_add(1);
            registry.actions.insert(
                registry.actions.len().saturating_sub(1),
                Action {
                    descriptor: ActionDescriptor {
                        id: entry.action_id.clone(),
                        label: entry.action_id.clone(),
                        hotkey,
                        description: format!(
                            "Manifest responder action on {} in {}",
                            entry.connection, entry.mode
                        ),
                        http: HttpBinding {
                            method: "POST".into(),
                            path: format!("/actions/{}", entry.action_id),
                        },
                    },
                    kind: ActionKind::Manifest {
                        body: serde_json::json!({
                            "catalogRevision": catalog.revision,
                            "mode": entry.mode,
                            "role": "responder",
                            "parameters": [],
                        })
                        .to_string(),
                    },
                },
            );
        }
        registry.catalog = catalog;
        registry
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub fn descriptors(&self) -> Vec<ActionDescriptor> {
        self.actions
            .iter()
            .map(|action| action.descriptor.clone())
            .collect()
    }

    pub fn by_hotkey(&self, key: char) -> Option<Action> {
        let key = key.to_ascii_lowercase();
        self.actions
            .iter()
            .find(|action| action.descriptor.hotkey == Some(key))
            .cloned()
    }

    pub fn by_http_path(&self, method: &str, path: &str) -> Option<Action> {
        self.actions
            .iter()
            .find(|action| {
                action.descriptor.http.method.eq_ignore_ascii_case(method)
                    && action.descriptor.http.path == path
            })
            .cloned()
    }

    pub fn actions_json(&self) -> String {
        serde_json::to_string(&self.catalog).expect("action catalog serializes")
    }

    pub fn operator_actions_json(&self) -> String {
        let descriptors = self
            .actions
            .iter()
            .filter(|action| action.id().starts_with("operator:"))
            .map(|action| action.descriptor.clone())
            .collect::<Vec<_>>();
        serde_json::json!({ "actions": descriptors }).to_string()
    }

    pub fn parity_report(&self) -> Result<()> {
        let mut hotkeys = BTreeSet::new();
        let mut routes = BTreeSet::new();
        for action in &self.actions {
            let route = (
                action.descriptor.http.method.to_ascii_uppercase(),
                action.descriptor.http.path.as_str(),
            );
            if action.descriptor.http.path.is_empty() || !routes.insert(route) {
                bail!("action HTTP routes are empty or duplicated");
            }
            if self
                .by_http_path(&action.descriptor.http.method, &action.descriptor.http.path)
                .as_ref()
                .map(Action::id)
                != Some(action.id())
            {
                bail!("HTTP action lookup drifted from the fetched registry");
            }
            if let Some(hotkey) = action.descriptor.hotkey {
                if !hotkeys.insert(hotkey) {
                    bail!("visible action hotkeys are duplicated");
                }
                if self.by_hotkey(hotkey).as_ref().map(Action::id) != Some(action.id()) {
                    bail!("hotkey action lookup drifted from the HTTP action");
                }
            }
        }
        Ok(())
    }
}

fn patch_action(
    id: &str,
    path: &str,
    label: &str,
    hotkey: char,
    description: &str,
    body: &str,
) -> Action {
    Action {
        descriptor: ActionDescriptor {
            id: id.into(),
            label: label.into(),
            hotkey: Some(hotkey),
            description: description.into(),
            http: HttpBinding {
                method: "POST".into(),
                path: path.into(),
            },
        },
        kind: ActionKind::Patch { body: body.into() },
    }
}

#[derive(Debug, Clone)]
pub struct ControlClient {
    addr: String,
}

impl ControlClient {
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    pub fn health(&self) -> Result<HealthSnapshot> {
        let body = self.request_json("GET", "/healthz", None)?;
        serde_json::from_str(&body).context("parse /healthz JSON")
    }

    pub fn state(&self) -> Result<CameraSnapshot> {
        let body = self.request_json("GET", "/state", None)?;
        serde_json::from_str(&body).context("parse /state JSON")
    }

    pub fn trace(&self, after: u64) -> Result<TraceSnapshot> {
        let body = self.request_json("GET", &format!("/trace?after={after}"), None)?;
        serde_json::from_str(&body).context("parse /trace JSON")
    }

    pub fn action_catalog(&self) -> Result<ManifestActionCatalog> {
        let body = self.request_json("GET", "/actions", None)?;
        serde_json::from_str(&body).context("parse /actions JSON")
    }

    pub fn invoke_action(&self, action_id: &str, body: &str) -> Result<String> {
        self.request_json("POST", &format!("/actions/{action_id}"), Some(body))
    }

    pub fn patch_state(&self, body: &str) -> Result<String> {
        self.request_json("PATCH", "/state", Some(body))
    }

    pub fn subscribe_callback(&self, callback_url: &str) -> Result<String> {
        let body = serde_json::json!({ "url": callback_url }).to_string();
        self.request_json("POST", "/callbacks", Some(&body))
    }

    fn request_json(&self, method: &str, path: &str, body: Option<&str>) -> Result<String> {
        let response = http_request(&self.addr, method, path, body)?;
        if !response.status.starts_with("200 ") {
            bail!("{} {} failed: {}", method, path, response.status);
        }
        Ok(response.body)
    }
}

pub struct HttpResponse {
    pub status: String,
    pub body: String,
}

pub fn http_request(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<HttpResponse> {
    let mut stream = TcpStream::connect(addr).with_context(|| format!("connect {addr}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("set read timeout")?;
    let body = body.unwrap_or("");
    let content_headers = if body.is_empty() {
        String::new()
    } else {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{content_headers}Connection: close\r\n\r\n{body}"
    )?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    let (headers, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed HTTP response"))?;
    let status = headers.lines().next().unwrap_or("").to_string();
    let status = status
        .strip_prefix("HTTP/1.1 ")
        .unwrap_or(status.as_str())
        .to_string();
    Ok(HttpResponse {
        status,
        body: body.to_string(),
    })
}

pub fn callback_url_for(bound: SocketAddr) -> String {
    let host = match bound.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_string(),
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    };
    format!("http://{host}:{}/state", bound.port())
}

#[cfg(test)]
mod tests {
    use super::{
        ActionRegistry, CameraSnapshot, HealthSnapshot, LifecycleTraceEvent, ManifestAction,
        ManifestActionCatalog, TraceSnapshot,
    };
    use std::collections::BTreeSet;

    #[test]
    fn visible_hotkeys_and_http_actions_stay_in_parity() {
        ActionRegistry::core().parity_report().unwrap();
    }

    #[test]
    fn hotkeys_and_http_paths_resolve_same_action_ids() {
        let registry = ActionRegistry::core();
        let hotkey_ids = registry
            .actions()
            .iter()
            .filter_map(|action| {
                action
                    .descriptor
                    .hotkey
                    .and_then(|key| registry.by_hotkey(key).map(|found| found.id().to_string()))
            })
            .collect::<BTreeSet<_>>();
        let path_ids = registry
            .actions()
            .iter()
            .filter_map(|action| {
                registry
                    .by_http_path(&action.descriptor.http.method, &action.descriptor.http.path)
                    .map(|found| found.id().to_string())
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(hotkey_ids, path_ids);
    }

    #[test]
    fn actions_surface_is_self_describing_json() {
        let registry = ActionRegistry::from_catalog(ManifestActionCatalog {
            revision: "revision".into(),
            actions: vec![ManifestAction {
                action_id: "shutter".into(),
                connection: "wireless-tether".into(),
                mode: "shooting/stills".into(),
                supported_roles: vec!["initiator".into(), "responder".into()],
                ..ManifestAction::default()
            }],
        });
        registry.parity_report().unwrap();
        let json = registry.actions_json();
        assert!(json.contains("\"actionId\":\"shutter\""));
        assert!(registry.by_http_path("POST", "/actions/shutter").is_some());
        assert!(registry
            .by_http_path("POST", "/operator/actions/streaming")
            .is_some());
    }

    #[test]
    fn catalogs_larger_than_the_numeric_hotkey_set_remain_http_addressable() {
        let actions = (0..12)
            .map(|index| ManifestAction {
                action_id: format!("action-{index}"),
                connection: "wireless-tether".into(),
                mode: "shooting/stills".into(),
                supported_roles: vec!["responder".into()],
                ..ManifestAction::default()
            })
            .collect();
        let registry = ActionRegistry::from_catalog(ManifestActionCatalog {
            revision: "revision".into(),
            actions,
        });
        registry.parity_report().unwrap();
        assert!(registry
            .by_http_path("POST", "/actions/action-11")
            .is_some());
        assert!(registry
            .actions()
            .iter()
            .find(|action| action.id() == "action-11")
            .unwrap()
            .descriptor
            .hotkey
            .is_none());
    }

    #[test]
    fn health_and_state_snapshots_accept_metrics_and_property_labels() {
        let health: HealthSnapshot = serde_json::from_value(serde_json::json!({
            "ok": true,
            "instance_id": "local",
            "profile": "fuji/gfx100ii",
            "connection": "app",
            "bind": "127.0.0.1:55740",
            "sessions": 1,
            "media_root": "fixtures",
            "metrics": {
                "uptime_ms": 2000,
                "idle_ms": 150,
                "bytes_read": 10,
                "bytes_written": 20,
                "bytes_transferred": 30,
                "liveview_frames": 60,
                "memory_allocated_bytes": 4096
            }
        }))
        .unwrap();
        assert_eq!(health.metrics.bytes_transferred, 30);
        assert_eq!(health.metrics.liveview_frames, 60);

        let state: CameraSnapshot = serde_json::from_value(serde_json::json!({
            "phase": "streaming",
            "session_open": true,
            "props": { "0xd02a": 2000 },
            "property_labels": { "0xd02a": "stillIso" },
            "media": { "objects": 3 },
            "transfer_queues": {
                "standard": { "queued": 2, "completed": 1, "total": 3 },
                "camera_initiated": { "queued": 1, "completed": 2, "total": 3 }
            }
        }))
        .unwrap();
        assert_eq!(state.property_labels["0xd02a"], "stillIso");
        assert_eq!(state.transfer_queues.standard.unwrap().queued, 2);
        assert_eq!(state.transfer_queues.camera_initiated.unwrap().completed, 2);
    }

    #[test]
    fn pcss_trace_events_render_operator_boundaries() {
        let trace: TraceSnapshot = serde_json::from_value(serde_json::json!({
            "instance_id": "test",
            "cursor": 2,
            "events": [
                {
                    "sequence": 1,
                    "elapsed_ms": 5,
                    "kind": "pcss.discovery.received",
                    "peer_endpoint": "192.0.2.10:53000",
                    "payload_hex": "44495343",
                    "outcome": "accepted"
                },
                {
                    "sequence": 2,
                    "elapsed_ms": 12,
                    "kind": "ptpip.init_fail.sent",
                    "outcome": "attempt=1;reason=0x2019"
                }
            ]
        }))
        .unwrap();

        assert_eq!(trace.cursor, 2);
        assert_eq!(
            trace.events[0].display_line(),
            "PCSS advertisement received from 192.0.2.10:53000"
        );
        assert_eq!(
            trace.events[1].display_line(),
            "PTP/IP InitFail reason=0x2019 sent"
        );
        assert!(!trace.events[0].is_error());

        let failed = LifecycleTraceEvent {
            kind: "pcss.callback.connect_failed".into(),
            target_endpoint: Some("192.0.2.20:51560".into()),
            error: Some("connection refused".into()),
            ..LifecycleTraceEvent::default()
        };
        assert!(failed.is_error());
        assert_eq!(
            failed.display_line(),
            "PCSS callback to 192.0.2.20:51560 failed"
        );
    }
}
