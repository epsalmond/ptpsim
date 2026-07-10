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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HttpBinding {
    pub method: &'static str,
    pub path: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ActionDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    pub hotkey: Option<char>,
    pub description: &'static str,
    pub http: HttpBinding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Patch { body: &'static str },
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    pub descriptor: ActionDescriptor,
    pub kind: ActionKind,
}

impl Action {
    pub fn id(self) -> &'static str {
        self.descriptor.id
    }

    pub fn patch_body(self) -> Option<&'static str> {
        match self.kind {
            ActionKind::Patch { body } => Some(body),
            ActionKind::Quit => None,
        }
    }

    pub fn is_quit(self) -> bool {
        matches!(self.kind, ActionKind::Quit)
    }
}

#[derive(Debug, Clone)]
pub struct ActionRegistry {
    actions: Vec<Action>,
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
                    "session-open",
                    "/actions/session-open",
                    "Session",
                    's',
                    "Open a generic simulator session",
                    r#"{"phase":"sessionOpen","session_open":true}"#,
                ),
                patch_action(
                    "live-view",
                    "/actions/live-view",
                    "Live View",
                    'v',
                    "Move the simulator to live-view ready state",
                    r#"{"phase":"liveView","session_open":true}"#,
                ),
                patch_action(
                    "streaming",
                    "/actions/streaming",
                    "Stream",
                    'r',
                    "Move the simulator to active streaming state",
                    r#"{"phase":"streaming","session_open":true}"#,
                ),
                patch_action(
                    "image-import",
                    "/actions/image-import",
                    "Import",
                    'i',
                    "Move the simulator to image import state",
                    r#"{"phase":"imageImport","session_open":true}"#,
                ),
                patch_action(
                    "disconnect",
                    "/actions/disconnect",
                    "Disconnect",
                    'd',
                    "Return the simulator to disconnected state",
                    r#"{"phase":"disconnected","session_open":false}"#,
                ),
                Action {
                    descriptor: ActionDescriptor {
                        id: "quit",
                        label: "Quit",
                        hotkey: Some('q'),
                        description: "Exit the operator console",
                        http: HttpBinding {
                            method: "POST",
                            path: "/actions/quit",
                        },
                    },
                    kind: ActionKind::Quit,
                },
            ],
        }
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub fn descriptors(&self) -> Vec<ActionDescriptor> {
        self.actions
            .iter()
            .map(|action| action.descriptor)
            .collect()
    }

    pub fn by_hotkey(&self, key: char) -> Option<Action> {
        let key = key.to_ascii_lowercase();
        self.actions
            .iter()
            .copied()
            .find(|action| action.descriptor.hotkey == Some(key))
    }

    pub fn by_http_path(&self, method: &str, path: &str) -> Option<Action> {
        self.actions.iter().copied().find(|action| {
            action.descriptor.http.method.eq_ignore_ascii_case(method)
                && action.descriptor.http.path == path
        })
    }

    pub fn actions_json(&self) -> String {
        serde_json::json!({ "actions": self.descriptors() }).to_string()
    }

    pub fn parity_report(&self) -> Result<()> {
        let hotkey_ids = self
            .actions
            .iter()
            .filter(|action| action.descriptor.hotkey.is_some())
            .map(|action| action.descriptor.id)
            .collect::<BTreeSet<_>>();
        let http_ids = self
            .actions
            .iter()
            .filter(|action| !action.descriptor.http.path.is_empty())
            .map(|action| action.descriptor.id)
            .collect::<BTreeSet<_>>();
        if hotkey_ids != http_ids {
            bail!("visible hotkeys and HTTP actions drifted");
        }
        Ok(())
    }
}

fn patch_action(
    id: &'static str,
    path: &'static str,
    label: &'static str,
    hotkey: char,
    description: &'static str,
    body: &'static str,
) -> Action {
    Action {
        descriptor: ActionDescriptor {
            id,
            label,
            hotkey: Some(hotkey),
            description,
            http: HttpBinding {
                method: "POST",
                path,
            },
        },
        kind: ActionKind::Patch { body },
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
    use super::{ActionRegistry, CameraSnapshot, HealthSnapshot};
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
                    .and_then(|key| registry.by_hotkey(key).map(|found| found.id()))
            })
            .collect::<BTreeSet<_>>();
        let path_ids = registry
            .actions()
            .iter()
            .filter_map(|action| {
                registry
                    .by_http_path(action.descriptor.http.method, action.descriptor.http.path)
                    .map(|found| found.id())
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(hotkey_ids, path_ids);
    }

    #[test]
    fn actions_surface_is_self_describing_json() {
        let json = ActionRegistry::core().actions_json();
        assert!(json.contains("\"id\":\"streaming\""));
        assert!(json.contains("\"path\":\"/actions/streaming\""));
        assert!(json.contains("\"hotkey\":\"r\""));
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
}
