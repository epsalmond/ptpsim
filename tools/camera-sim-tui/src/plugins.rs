use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const MAX_PLUGIN_PAYLOAD_BYTES: usize = 64 * 1024;
const _PLUGIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PLUGIN_READ_TIMEOUT: Duration = Duration::from_secs(5);
const PLUGIN_STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
const PLUGIN_STARTUP_POLL: Duration = Duration::from_millis(100);

const SUPPORTED_MAJOR: u64 = 1;
const MAX_PANELS: usize = 16;
const MAX_ACTIONS: usize = 32;
const MAX_ROWS: usize = 32;
const MAX_SPANS_PER_ROW: usize = 16;
const MAX_TEXT_LEN: usize = 2048;
const MAX_PATH_LEN: usize = 200;

// --- Manifest types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub id: String,
    pub version: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub panels: Vec<PluginPanelDecl>,
    #[serde(default)]
    pub actions: Vec<PluginActionDecl>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub spawn: Option<PluginSpawnDecl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginPanelDecl {
    pub id: String,
    pub title: String,
    pub priority: i32,
    pub rows: Vec<Vec<PluginSpan>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSpan {
    pub text: String,
    #[serde(default = "default_plain")]
    pub style: String,
}

fn default_plain() -> String {
    "plain".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginActionDecl {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hotkey: Option<String>,
    pub path: String,
    #[serde(default = "default_post")]
    pub method: String,
}

fn default_post() -> String {
    "POST".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSpawnDecl {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub endpoint: String,
}

// --- Validated plugin ---

#[derive(Debug, Clone)]
pub struct ValidPlugin {
    pub manifest: PluginManifest,
    pub endpoint: String,
    // Panels/actions are taken from manifest after validation.
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginPanelState {
    pub id: String,
    pub title: String,
    pub priority: i32,
    pub rows: Vec<Vec<PluginSpan>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginSummary {
    pub id: String,
    pub version: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub endpoint: String,
    pub panels: Vec<PluginPanelState>,
    pub actions: Vec<PluginActionSummary>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginActionSummary {
    pub id: String,
    pub label: String,
    pub hotkey: Option<char>,
    pub path: String,
    #[serde(rename = "httpPath")]
    pub http_path: String,
}

#[derive(Debug)]
pub struct PluginInstance {
    pub valid: ValidPlugin,
    pub child: Option<Child>,
    pub last_error: Option<String>,
    pub panel_state: Vec<PluginPanelState>,
}

impl PluginInstance {
    pub fn summary(&self) -> PluginSummary {
        let actions = self
            .valid
            .manifest
            .actions
            .iter()
            .map(|a| PluginActionSummary {
                id: a.id.clone(),
                label: a.label.clone(),
                hotkey: a.hotkey.as_deref().and_then(|s| s.chars().next()),
                path: a.path.clone(),
                http_path: format!("/plugins/{}/actions/{}", self.valid.manifest.id, a.id),
            })
            .collect();
        PluginSummary {
            id: self.valid.manifest.id.clone(),
            version: self.valid.manifest.version.clone(),
            display_name: self.valid.manifest.display_name.clone(),
            endpoint: self.valid.endpoint.clone(),
            panels: self.panel_state.clone(),
            actions,
            status: if self.last_error.is_some() {
                "error".into()
            } else {
                "ok".into()
            },
            error: self.last_error.clone(),
        }
    }
}

#[derive(Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, PluginInstance>,
    // Process handles to kill on drop
}

impl Drop for PluginRegistry {
    fn drop(&mut self) {
        for inst in self.plugins.values_mut() {
            if let Some(mut child) = inst.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn summaries(&self) -> Vec<PluginSummary> {
        self.plugins.values().map(|p| p.summary()).collect()
    }

    pub fn panel_states_sorted(&self) -> Vec<PluginPanelState> {
        let mut panels: Vec<PluginPanelState> = self
            .plugins
            .values()
            .flat_map(|p| p.panel_state.clone())
            .collect();
        panels.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
        panels
    }

    pub fn plugin_ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    pub fn by_id(&self, id: &str) -> Option<&PluginInstance> {
        self.plugins.get(id)
    }

    pub fn by_hotkey_collision(&self, core_hotkeys: &BTreeSet<char>) -> BTreeMap<char, String> {
        let mut map = BTreeMap::new();
        for inst in self.plugins.values() {
            for action in &inst.valid.manifest.actions {
                if let Some(hk) = action.hotkey.as_deref().and_then(|s| s.chars().next()) {
                    let lower = hk.to_ascii_lowercase();
                    if core_hotkeys.contains(&lower) {
                        continue;
                    }
                    map.entry(lower)
                        .or_insert_with(|| inst.valid.manifest.id.clone());
                }
            }
        }
        map
    }

    pub fn resolve_action(&self, plugin_id: &str, action_id: &str) -> Option<PluginActionDecl> {
        self.plugins.get(plugin_id).and_then(|inst| {
            inst.valid
                .manifest
                .actions
                .iter()
                .find(|a| a.id == action_id)
                .cloned()
        })
    }

    /// Load a validated manifest with an already-resolved endpoint.
    pub fn insert_validated(&mut self, valid: ValidPlugin) -> Result<()> {
        let id = valid.manifest.id.clone();
        if self.plugins.contains_key(&id) {
            bail!("duplicate plugin id {id}");
        }
        let panel_state = valid
            .manifest
            .panels
            .iter()
            .map(|p| PluginPanelState {
                id: p.id.clone(),
                title: p.title.clone(),
                priority: p.priority,
                rows: p.rows.clone(),
            })
            .collect();
        self.plugins.insert(
            id,
            PluginInstance {
                valid,
                child: None,
                last_error: None,
                panel_state,
            },
        );
        Ok(())
    }

    /// Insert with a spawned child handle.
    pub fn insert_spawned(&mut self, valid: ValidPlugin, child: Child) -> Result<()> {
        let id = valid.manifest.id.clone();
        if self.plugins.contains_key(&id) {
            bail!("duplicate plugin id {id}");
        }
        let panel_state = valid
            .manifest
            .panels
            .iter()
            .map(|p| PluginPanelState {
                id: p.id.clone(),
                title: p.title.clone(),
                priority: p.priority,
                rows: p.rows.clone(),
            })
            .collect();
        self.plugins.insert(
            id,
            PluginInstance {
                valid,
                child: Some(child),
                last_error: None,
                panel_state,
            },
        );
        Ok(())
    }

    /// Fetch panel updates from each plugin endpoint with bounded payloads.
    pub fn refresh_panels(&mut self) {
        for inst in self.plugins.values_mut() {
            let endpoint = inst.valid.endpoint.clone();
            let panels_url = endpoint.trim_end_matches('/');
            let path = format!("{panels_url}/panels");
            // Extract addr+path for our minimal http client
            match fetch_plugin_panels(&endpoint) {
                Ok(panels) => {
                    inst.panel_state = panels;
                    inst.last_error = None;
                }
                Err(e) => {
                    inst.last_error = Some(e.to_string());
                    let _ = path; // keep for debugging
                }
            }
        }
    }

    /// Proxy an action to the plugin endpoint.
    pub fn proxy_action(
        &self,
        plugin_id: &str,
        action_id: &str,
        body: Option<&[u8]>,
    ) -> Result<String> {
        let inst = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("unknown plugin {plugin_id}"))?;
        let decl = inst
            .valid
            .manifest
            .actions
            .iter()
            .find(|a| a.id == action_id)
            .ok_or_else(|| anyhow::anyhow!("unknown action {action_id} for plugin {plugin_id}"))?;
        let url = format!(
            "{}/{}",
            inst.valid.endpoint.trim_end_matches('/'),
            decl.path.trim_start_matches('/')
        );
        let (addr, path) = split_endpoint(&url)?;
        let response = http_request_with_limits(&addr, &decl.method, &path, body)?;
        if !response.status.starts_with("200 ") && !response.status.starts_with("204 ") {
            bail!("plugin action failed: {}", response.status);
        }
        if response.body.len() > MAX_PLUGIN_PAYLOAD_BYTES {
            bail!("plugin response exceeds bounds");
        }
        Ok(response.body)
    }

    /// Shutdown all spawned plugins.
    pub fn shutdown(&mut self) {
        for inst in self.plugins.values_mut() {
            if let Some(mut child) = inst.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

// --- Validation ---

pub fn validate_manifest(value: &serde_json::Value) -> Result<ValidPlugin> {
    let manifest: PluginManifest =
        serde_json::from_value(value.clone()).context("deserialize plugin manifest")?;

    // protocolVersion major must be 1
    let major = parse_protocol_major(&manifest.protocol_version)?;
    if major != SUPPORTED_MAJOR {
        bail!("unsupported plugin protocol major {major}, only {SUPPORTED_MAJOR} is accepted");
    }

    // id/version/displayName validation
    validate_plugin_id(&manifest.id).context("plugin id")?;
    validate_version(&manifest.version).context("plugin version")?;
    if manifest.display_name.is_empty() || manifest.display_name.len() > 120 {
        bail!("displayName must be 1..120 chars");
    }

    // endpoint vs spawn mutual exclusivity already enforced by schema oneOf,
    // but we defend again here.
    let endpoint = match (&manifest.endpoint, &manifest.spawn) {
        (Some(ep), None) => ep.clone(),
        (None, Some(spawn)) => spawn.endpoint.clone(),
        (Some(_), Some(_)) => bail!("plugin manifest must have exactly one of endpoint or spawn"),
        (None, None) => bail!("plugin manifest must declare endpoint or spawn"),
    };
    validate_loopback_endpoint(&endpoint)?;

    if let Some(spawn) = &manifest.spawn {
        validate_loopback_endpoint(&spawn.endpoint)?;
        if spawn.program.is_empty() || spawn.program.len() > 512 {
            bail!("spawn.program must be 1..512 chars");
        }
        if spawn.args.len() > 24 {
            bail!("spawn.args exceeds 24 entries");
        }
        for arg in &spawn.args {
            if arg.len() > 512 {
                bail!("spawn arg exceeds 512 chars");
            }
        }
        if spawn.env.len() > 16 {
            bail!("spawn.env exceeds 16 entries");
        }
    }

    if manifest.panels.len() > MAX_PANELS {
        bail!("panels exceed {MAX_PANELS}");
    }
    if manifest.actions.len() > MAX_ACTIONS {
        bail!("actions exceed {MAX_ACTIONS}");
    }

    let mut panel_ids = BTreeSet::new();
    for panel in &manifest.panels {
        validate_plugin_id(&panel.id).context("panel id")?;
        if panel.title.is_empty() || panel.title.len() > 80 {
            bail!("panel title must be 1..80 chars");
        }
        if !panel_ids.insert(panel.id.clone()) {
            bail!("duplicate panel id {}", panel.id);
        }
        if panel.rows.len() > MAX_ROWS {
            bail!("panel {} rows exceed {MAX_ROWS}", panel.id);
        }
        for row in &panel.rows {
            if row.len() > MAX_SPANS_PER_ROW {
                bail!("panel {} row spans exceed {MAX_SPANS_PER_ROW}", panel.id);
            }
            for span in row {
                if span.text.len() > MAX_TEXT_LEN {
                    bail!("panel {} span text exceeds {MAX_TEXT_LEN}", panel.id);
                }
                if !matches!(
                    span.style.as_str(),
                    "plain" | "muted" | "info" | "success" | "warning" | "error"
                ) {
                    bail!("panel {} span style invalid {}", panel.id, span.style);
                }
            }
        }
    }

    let mut action_ids = BTreeSet::new();
    for action in &manifest.actions {
        validate_plugin_id(&action.id).context("action id")?;
        if action.label.is_empty() || action.label.len() > 80 {
            bail!("action label must be 1..80 chars");
        }
        if !action_ids.insert(action.id.clone()) {
            bail!("duplicate action id {}", action.id);
        }
        if action.path.len() > MAX_PATH_LEN || !action.path.starts_with('/') {
            bail!("action path must be /-prefixed and <= {MAX_PATH_LEN}");
        }
        if action.method != "POST" {
            bail!("action method must be POST");
        }
        if let Some(hk) = &action.hotkey {
            if hk.len() != 1 || !hk.chars().next().unwrap().is_ascii_alphanumeric() {
                bail!("action hotkey must be single alphanumeric");
            }
        }
    }

    Ok(ValidPlugin { manifest, endpoint })
}

fn parse_protocol_major(version: &str) -> Result<u64> {
    let major_str = version
        .split('.')
        .next()
        .ok_or_else(|| anyhow::anyhow!("protocolVersion missing major"))?;
    let major: u64 = major_str
        .parse()
        .context("protocolVersion major is not numeric")?;
    Ok(major)
}

fn validate_plugin_id(id: &str) -> Result<()> {
    if id.len() < 2 || id.len() > 64 {
        bail!("id length must be 2..64");
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => bail!("id must start with lowercase letter"),
    }
    for c in id.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            bail!("id must be lowercase alphanum or hyphen");
        }
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<()> {
    // semver basic check: at least X.Y.Z
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() < 3 {
        bail!("version must be semver X.Y.Z");
    }
    // Validate each numeric part parses
    for part in parts.iter().take(3) {
        let core = part.split(['-', '+']).next().unwrap();
        core.parse::<u64>().context("version numeric part")?;
    }
    Ok(())
}

fn validate_loopback_endpoint(endpoint: &str) -> Result<()> {
    if !endpoint.starts_with("http://127.0.0.1:") && !endpoint.starts_with("http://[::1]:") {
        bail!("endpoint must be loopback http://127.0.0.1:<port> or http://[::1]:<port>");
    }
    // Extract port and validate 1..65535
    let port_str = endpoint
        .rsplit(':')
        .next()
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    let port: u16 = port_str.parse().context("endpoint port is not numeric")?;
    if port == 0 {
        bail!("endpoint port must be non-zero");
    }
    Ok(())
}

// --- Minimal HTTP helpers with bounds/timeouts ---

struct HttpResponse {
    status: String,
    body: String,
}

fn http_request_with_limits(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
) -> Result<HttpResponse> {
    let mut stream = TcpStream::connect(addr).context("connect plugin endpoint")?;
    stream.set_read_timeout(Some(PLUGIN_READ_TIMEOUT)).ok();
    stream.set_write_timeout(Some(PLUGIN_READ_TIMEOUT)).ok();
    let body = body.unwrap_or(b"");
    if body.len() > MAX_PLUGIN_PAYLOAD_BYTES {
        bail!("plugin request payload exceeds bounds");
    }
    let content_headers = if body.is_empty() {
        String::new()
    } else {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        )
    };
    stream.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\n{content_headers}Connection: close\r\n\r\n"
        )
        .as_bytes(),
    )?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let start = Instant::now();
    loop {
        if start.elapsed() > PLUGIN_READ_TIMEOUT {
            bail!("plugin read timeout");
        }
        if raw.len() > MAX_PLUGIN_PAYLOAD_BYTES + 8192 {
            bail!("plugin response exceeds bounds");
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => bail!("plugin read failed: {e}"),
        }
    }
    let text = String::from_utf8(raw).context("plugin response not UTF-8")?;
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed plugin HTTP response"))?;
    let status = headers.lines().next().unwrap_or("").to_string();
    let status = status
        .strip_prefix("HTTP/1.1 ")
        .unwrap_or(status.as_str())
        .to_string();
    if body.len() > MAX_PLUGIN_PAYLOAD_BYTES {
        bail!("plugin response body exceeds bounds");
    }
    Ok(HttpResponse {
        status,
        body: body.to_string(),
    })
}

fn split_endpoint(url: &str) -> Result<(String, String)> {
    let without_scheme = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("endpoint must be http://"))?;
    let (host_port, path) = match without_scheme.find('/') {
        Some(idx) => (&without_scheme[..idx], &without_scheme[idx..]),
        None => (without_scheme, "/"),
    };
    // host_port is 127.0.0.1:PORT or [::1]:PORT
    Ok((host_port.to_string(), path.to_string()))
}

fn fetch_plugin_panels(endpoint: &str) -> Result<Vec<PluginPanelState>> {
    let url = format!("{}/panels", endpoint.trim_end_matches('/'));
    let (addr, path) = split_endpoint(&url)?;
    let resp = http_request_with_limits(&addr, "GET", &path, None)?;
    if !resp.status.starts_with("200 ") {
        bail!("plugin panels fetch failed: {}", resp.status);
    }
    if resp.body.len() > MAX_PLUGIN_PAYLOAD_BYTES {
        bail!("plugin panels payload exceeds bounds");
    }
    let value: serde_json::Value =
        serde_json::from_str(&resp.body).context("parse plugin panels JSON")?;
    let panels = value
        .get("panels")
        .ok_or_else(|| anyhow::anyhow!("plugin panels missing panels field"))?;
    let parsed: Vec<PluginPanelState> =
        serde_json::from_value(panels.clone()).context("parse panels array")?;
    for panel in &parsed {
        if panel.rows.len() > MAX_ROWS {
            bail!("plugin panel {} rows exceed bounds", panel.id);
        }
        for row in &panel.rows {
            if row.len() > MAX_SPANS_PER_ROW {
                bail!("plugin panel {} row exceeds bounds", panel.id);
            }
        }
    }
    Ok(parsed)
}

pub fn load_manifest_from_file(path: &str) -> Result<ValidPlugin> {
    let data =
        std::fs::read_to_string(path).with_context(|| format!("read plugin manifest {path}"))?;
    if data.len() > MAX_PLUGIN_PAYLOAD_BYTES {
        bail!("plugin manifest file exceeds bounds");
    }
    let value: serde_json::Value =
        serde_json::from_str(&data).context("parse plugin manifest JSON")?;
    validate_manifest(&value)
}

pub fn fetch_manifest_from_endpoint(endpoint: &str) -> Result<ValidPlugin> {
    let url = format!("{}/manifest", endpoint.trim_end_matches('/'));
    let (addr, path) = split_endpoint(&url)?;
    let resp = http_request_with_limits(&addr, "GET", &path, None)?;
    if !resp.status.starts_with("200 ") {
        bail!("plugin manifest fetch failed: {}", resp.status);
    }
    if resp.body.len() > MAX_PLUGIN_PAYLOAD_BYTES {
        bail!("plugin manifest payload exceeds bounds");
    }
    let value: serde_json::Value =
        serde_json::from_str(&resp.body).context("parse plugin manifest JSON")?;
    validate_manifest(&value)
}

pub fn spawn_plugin_and_discover(manifest: &PluginManifest) -> Result<(ValidPlugin, Child)> {
    let spawn = manifest
        .spawn
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("manifest has no spawn"))?;
    let mut cmd = Command::new(&spawn.program);
    cmd.args(&spawn.args);
    for (k, v) in &spawn.env {
        cmd.env(k, v);
    }
    // Detach stdio to avoid blocking
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    let child = cmd.spawn().context("spawn plugin command")?;
    let endpoint = spawn.endpoint.clone();
    // Wait for endpoint to become reachable
    let start = Instant::now();
    let mut last_err = String::new();
    while start.elapsed() < PLUGIN_STARTUP_TIMEOUT {
        match fetch_manifest_from_endpoint(&endpoint) {
            Ok(valid) => return Ok((valid, child)),
            Err(e) => {
                last_err = e.to_string();
                std::thread::sleep(PLUGIN_STARTUP_POLL);
            }
        }
        // If child died early, fail fast
        // We check via try_wait without consuming child
        // We need mutable reference; clone child handling is tricky outside.
        // For now, continue polling.
    }
    // Kill child on failure
    let mut child = child;
    let _ = child.kill();
    let _ = child.wait();
    bail!("plugin endpoint did not become ready in time: {last_err}")
}

pub fn validate_manifest_json_str(json: &str) -> Result<ValidPlugin> {
    if json.len() > MAX_PLUGIN_PAYLOAD_BYTES {
        bail!("manifest JSON exceeds bounds");
    }
    let value: serde_json::Value = serde_json::from_str(json).context("parse JSON")?;
    validate_manifest(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_attached_value() -> serde_json::Value {
        json!({
            "protocolVersion": "1.0.0",
            "id": "demo-plugin",
            "version": "0.1.0",
            "displayName": "Demo",
            "panels": [{
                "id": "demo-panel",
                "title": "Demo Panel",
                "priority": 10,
                "rows": [[{"text": "hello", "style": "plain"}]]
            }],
            "actions": [{
                "id": "demo-action",
                "label": "Do Thing",
                "path": "/do",
                "method": "POST",
                "hotkey": "x"
            }],
            "endpoint": "http://127.0.0.1:8765"
        })
    }

    #[test]
    fn valid_attached_accepts() {
        validate_manifest(&valid_attached_value()).unwrap();
    }

    #[test]
    fn rejects_unknown_major_version() {
        let mut v = valid_attached_value();
        v["protocolVersion"] = json!("2.0.0");
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn rejects_malformed_missing_id() {
        let mut v = valid_attached_value();
        v.as_object_mut().unwrap().remove("id");
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn rejects_non_loopback_endpoint() {
        let mut v = valid_attached_value();
        v["endpoint"] = json!("http://192.168.1.1:8765");
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn rejects_both_endpoint_and_spawn() {
        let mut v = valid_attached_value();
        v["spawn"] = json!({"program": "echo", "endpoint": "http://127.0.0.1:8766"});
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn rejects_duplicate_panel_id() {
        let mut v = valid_attached_value();
        v["panels"] = json!([
            {"id": "dup", "title": "A", "priority": 1, "rows": []},
            {"id": "dup", "title": "B", "priority": 2, "rows": []}
        ]);
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn rejects_oversized_text() {
        let mut v = valid_attached_value();
        v["panels"] = json!([{
            "id": "p1", "title": "T", "priority": 1,
            "rows": [[{"text": "x".repeat(3000), "style": "plain"}]]
        }]);
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn rejects_invalid_hotkey() {
        let mut v = valid_attached_value();
        v["actions"] =
            json!([{"id": "a1", "label": "L", "path": "/a", "method": "POST", "hotkey": "!!"}]);
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn core_hotkeys_win_collisions() {
        let mut registry = PluginRegistry::new();
        registry
            .insert_validated(validate_manifest(&valid_attached_value()).unwrap())
            .unwrap();
        let mut core = BTreeSet::new();
        core.insert('x');
        let map = registry.by_hotkey_collision(&core);
        assert!(map.is_empty(), "core hotkey should suppress plugin hotkey");
        let empty = BTreeSet::new();
        let map2 = registry.by_hotkey_collision(&empty);
        assert_eq!(map2.get(&'x'), Some(&"demo-plugin".to_string()));
    }

    #[test]
    fn bounded_payload_rejected() {
        let big = "x".repeat(MAX_PLUGIN_PAYLOAD_BYTES + 1);
        assert!(validate_manifest_json_str(&big).is_err());
    }

    #[test]
    fn namespace_isolation_plugin_not_via_actions() {
        // Plugin actions live under /plugins/{id}/actions/{aid}, not /actions/{id}
        let manifest = valid_attached_value();
        let valid = validate_manifest(&manifest).unwrap();
        let mut registry = PluginRegistry::new();
        registry.insert_validated(valid).unwrap();
        assert!(registry
            .resolve_action("demo-plugin", "demo-action")
            .is_some());
        assert!(registry.resolve_action("demo-plugin", "missing").is_none());
        // Camera action namespace is separate; plugin action should not be found via camera catalog lookup
        // This is enforced by TUI routing, not registry alone, but registry correctly scopes by plugin id.
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn attached_discovery_and_panel_action_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://127.0.0.1:{}", addr.port());
        let endpoint_clone = endpoint.clone();
        let running = Arc::new(AtomicBool::new(true));
        let running2 = Arc::clone(&running);
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                if !running2.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                    .ok();
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = if req.contains("GET /manifest") {
                    let manifest = json!({
                        "protocolVersion": "1.0.0",
                        "id": "e2e-plugin",
                        "version": "0.1.0",
                        "displayName": "E2E",
                        "panels": [{"id": "p1", "title": "P", "priority": 5, "rows": [[{"text": "hello", "style": "info"}]]}],
                        "actions": [{"id": "act1", "label": "Act", "path": "/do", "method": "POST", "hotkey": "z"}],
                        "endpoint": endpoint_clone
                    });
                    ("200 OK", manifest.to_string())
                } else if req.contains("GET /panels") {
                    ("200 OK", r#"{"panels":[{"id":"p1","title":"P","priority":5,"rows":[[{"text":"hello","style":"info"}]]}]}"#.to_string())
                } else if req.contains("POST /do") {
                    ("200 OK", r#"{"ok":true}"#.to_string())
                } else {
                    ("404 Not Found", r#"{"error":"not found"}"#.to_string())
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        std::thread::sleep(std::time::Duration::from_millis(50));
        let fetched = fetch_manifest_from_endpoint(&endpoint).expect("fetch manifest");
        assert_eq!(fetched.manifest.id, "e2e-plugin");
        let mut registry = PluginRegistry::new();
        registry.insert_validated(fetched).unwrap();
        assert_eq!(registry.len(), 1);
        registry.refresh_panels();
        let panels = registry.panel_states_sorted();
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].id, "p1");
        let result = registry
            .proxy_action("e2e-plugin", "act1", None)
            .expect("proxy");
        assert!(result.contains("ok"));
        assert!(registry
            .proxy_action("e2e-plugin", "missing", None)
            .is_err());
        // Connection error observable
        let mut bad_registry = PluginRegistry::new();
        let bad_manifest = json!({
            "protocolVersion": "1.0.0",
            "id": "bad-plugin",
            "version": "0.1.0",
            "displayName": "Bad",
            "panels": [],
            "actions": [{"id": "bad-act", "label": "Bad", "path": "/do", "method": "POST"}],
            "endpoint": "http://127.0.0.1:1"
        });
        let bad_valid = validate_manifest(&bad_manifest).unwrap();
        bad_registry.insert_validated(bad_valid).unwrap();
        assert!(bad_registry
            .proxy_action("bad-plugin", "bad-act", None)
            .is_err());
        bad_registry.refresh_panels();
        assert!(bad_registry
            .by_id("bad-plugin")
            .unwrap()
            .last_error
            .is_some());

        running.store(false, Ordering::Relaxed);
        let _ = TcpStream::connect(addr);
        let _ = handle.join();
    }
}
