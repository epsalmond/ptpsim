//! Opt-in BLE socket (`ptpsim-ble/1`): serves the in-memory GATT responder to
//! a client that has no Bluetooth radio. The protocol, the scenario format and
//! the limits are specified in `docs/TRANSPORTS.md` "BLE socket".
//!
//! The wire carries opaque UUIDs and bytes only. Every served value comes from
//! a scenario file that resolves symbolic GATT names through a manufacturer
//! index, so no camera knowledge lives in this module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use camera_config::index::{CccdMode, ResolvedManufacturerIndex};
use camera_sim::{BleError, BleEvent, BleResponder, SharedLink};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};

pub const PROTOCOL: &str = "ptpsim-ble";
pub const PROTOCOL_VERSION: u64 = 1;
pub const SCENARIO_SCHEMA: &str = "ptpsim-ble-scenario/v1";
/// The only scenario provenance accepted today: every served value is
/// scripted for simulation.
pub const SCENARIO_PROVENANCE: &str = "test-scripted";

const MAX_LINE_BYTES: usize = 64 * 1024;
/// A client that closes its connection and immediately reconnects can arrive
/// before the server has seen the close. Wait this long for the earlier
/// connection to finish before refusing the new one as busy.
const BUSY_GRACE: Duration = Duration::from_millis(500);

/// The advertisement a scanning client sees for one simulated peripheral.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BleAdvert {
    pub service_uuids: Vec<String>,
    /// The raw advertised field: 2-byte little-endian company ID, then the
    /// payload.
    pub manufacturer_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedPeripheral {
    /// Stable peripheral identifier, an uppercase RFC 4122 UUID string.
    pub id: String,
    pub advert: BleAdvert,
}

type ResponderFactory = Arc<dyn Fn() -> BleResponder + Send + Sync>;

/// What the socket serves: the advertised peripherals, the connect result,
/// and a factory that builds a fresh scripted responder for each accepted
/// connection.
#[derive(Clone)]
pub struct BleScenario {
    peripherals: Vec<SimulatedPeripheral>,
    mtu: u16,
    peripheral_name: Option<String>,
    factory: ResponderFactory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioError(String);

impl std::fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScenarioError {}

fn scenario_error(message: impl Into<String>) -> ScenarioError {
    ScenarioError(message.into())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScenarioFile {
    schema: String,
    provenance: String,
    index: PathBuf,
    model: String,
    mtu: u16,
    #[serde(default)]
    peripheral_name: Option<String>,
    #[serde(default)]
    arming: Option<ArmingFile>,
    peripherals: Vec<PeripheralFile>,
    #[serde(default)]
    reads: BTreeMap<String, ValueFile>,
    #[serde(default)]
    notifications: Vec<NotificationFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArmingFile {
    #[serde(default)]
    prep: Option<String>,
    #[serde(default)]
    launch: Option<String>,
    #[serde(default)]
    device_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeripheralFile {
    id: String,
    advert: AdvertFile,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdvertFile {
    #[serde(default)]
    service_uuids: Vec<String>,
    manufacturer_data: String,
}

/// `{ hex: "..." }` or `{ utf8: "..." }`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueFile {
    #[serde(default)]
    hex: Option<String>,
    #[serde(default)]
    utf8: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotificationFile {
    gatt: String,
    #[serde(default)]
    after_fenced_write: Option<FencedWriteFile>,
    #[serde(default)]
    hex: Option<String>,
    #[serde(default)]
    utf8: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FencedWriteFile {
    gatt: String,
    ordinal: u32,
}

struct ScriptedNotification {
    uuid: String,
    after_fenced_write: Option<(String, u32)>,
    payload: Vec<u8>,
}

impl BleScenario {
    /// A scenario from already-resolved values. `factory` runs once per
    /// accepted connection.
    pub fn new(
        peripherals: Vec<SimulatedPeripheral>,
        mtu: u16,
        peripheral_name: Option<String>,
        factory: impl Fn() -> BleResponder + Send + Sync + 'static,
    ) -> Self {
        let peripherals = peripherals
            .into_iter()
            .map(|peripheral| SimulatedPeripheral {
                id: peripheral.id.to_ascii_uppercase(),
                advert: BleAdvert {
                    service_uuids: peripheral
                        .advert
                        .service_uuids
                        .iter()
                        .map(|uuid| uuid.to_ascii_uppercase())
                        .collect(),
                    manufacturer_data: peripheral.advert.manufacturer_data,
                },
            })
            .collect();
        BleScenario {
            peripherals,
            mtu,
            peripheral_name,
            factory: Arc::new(factory),
        }
    }

    /// Parse a `ptpsim-ble-scenario/v1` file. Symbolic GATT names resolve
    /// through the named model in the scenario's manufacturer index; the
    /// responder's arming and device-name writes are wired to `link`.
    pub fn load(path: &Path, link: SharedLink) -> Result<Self, ScenarioError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| scenario_error(format!("read {}: {e}", path.display())))?;
        let file: ScenarioFile = serde_yaml::from_str(&text)
            .map_err(|e| scenario_error(format!("parse {}: {e}", path.display())))?;
        if file.schema != SCENARIO_SCHEMA {
            return Err(scenario_error(format!(
                "unsupported scenario schema '{}' (expected {SCENARIO_SCHEMA})",
                file.schema
            )));
        }
        if file.provenance != SCENARIO_PROVENANCE {
            return Err(scenario_error(format!(
                "unsupported scenario provenance '{}' (expected {SCENARIO_PROVENANCE})",
                file.provenance
            )));
        }
        let index_path = path.parent().unwrap_or(Path::new(".")).join(&file.index);
        let index_text = std::fs::read_to_string(&index_path)
            .map_err(|e| scenario_error(format!("read {}: {e}", index_path.display())))?;
        let index = ResolvedManufacturerIndex::from_yaml(&index_text)
            .map_err(|e| scenario_error(format!("load {}: {e}", index_path.display())))?;
        let model = index
            .models
            .into_iter()
            .find(|model| model.id == file.model)
            .ok_or_else(|| scenario_error(format!("model '{}' not in the index", file.model)))?;
        let gatt = model
            .ble
            .ok_or_else(|| scenario_error(format!("model '{}' has no BLE block", file.model)))?
            .gatt;
        let resolve = |name: &str| {
            gatt.get(name).cloned().ok_or_else(|| {
                scenario_error(format!(
                    "unknown GATT name '{name}' for model '{}'",
                    file.model
                ))
            })
        };

        let catalog: Vec<String> = gatt.values().cloned().collect();
        let reads = file
            .reads
            .iter()
            .map(|(name, value)| {
                let what = format!("read '{name}'");
                Ok((resolve(name)?, value_bytes(&value.hex, &value.utf8, &what)?))
            })
            .collect::<Result<Vec<_>, ScenarioError>>()?;
        let notifications = file
            .notifications
            .iter()
            .map(|entry| {
                let what = format!("notification on '{}'", entry.gatt);
                let payload = value_bytes(&entry.hex, &entry.utf8, &what)?;
                let after_fenced_write = match &entry.after_fenced_write {
                    Some(fence) => Some((resolve(&fence.gatt)?, fence.ordinal)),
                    None => None,
                };
                Ok(ScriptedNotification {
                    uuid: resolve(&entry.gatt)?,
                    after_fenced_write,
                    payload,
                })
            })
            .collect::<Result<Vec<_>, ScenarioError>>()?;
        let (arm, device_name) = match &file.arming {
            None => (None, None),
            Some(arming) => {
                let arm = match (&arming.prep, &arming.launch) {
                    (Some(prep), Some(launch)) => Some((resolve(prep)?, resolve(launch)?)),
                    (None, None) => None,
                    _ => {
                        return Err(scenario_error(
                            "arming needs both prep and launch, or neither",
                        ))
                    }
                };
                let device_name = arming.device_name.as_deref().map(resolve).transpose()?;
                (arm, device_name)
            }
        };
        let peripherals = file
            .peripherals
            .iter()
            .map(|peripheral| {
                let id = canonical_uuid(&peripheral.id).ok_or_else(|| {
                    scenario_error(format!("peripheral id '{}' is not a UUID", peripheral.id))
                })?;
                let service_uuids = peripheral
                    .advert
                    .service_uuids
                    .iter()
                    .map(|uuid| {
                        canonical_uuid(uuid).ok_or_else(|| {
                            scenario_error(format!("advert service UUID '{uuid}' is not a UUID"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let manufacturer_data = decode_hex(&peripheral.advert.manufacturer_data)
                    .ok_or_else(|| {
                        scenario_error(format!("peripheral '{id}': invalid manufacturerData hex"))
                    })?;
                Ok(SimulatedPeripheral {
                    id,
                    advert: BleAdvert {
                        service_uuids,
                        manufacturer_data,
                    },
                })
            })
            .collect::<Result<Vec<_>, ScenarioError>>()?;

        let mtu = file.mtu;
        let served_name = file.peripheral_name.clone();
        let factory = move || {
            let mut responder = BleResponder::new(catalog.iter().cloned()).with_mtu_cap(mtu);
            if let Some(name) = &served_name {
                responder = responder.with_peripheral_name(name);
            }
            for (uuid, bytes) in &reads {
                responder = responder.serve_read(uuid, bytes);
            }
            for entry in &notifications {
                responder = match &entry.after_fenced_write {
                    Some((write_uuid, ordinal)) => responder.queue_notification_after_fenced_write(
                        &entry.uuid,
                        write_uuid,
                        *ordinal,
                        &entry.payload,
                    ),
                    None => responder.queue_notification(&entry.uuid, &entry.payload),
                };
            }
            if let Some((prep, launch)) = &arm {
                responder = responder.link_arming(link.clone(), prep, launch);
            }
            if let Some(name_uuid) = &device_name {
                responder = responder.link_device_name(link.clone(), name_uuid);
            }
            responder
        };
        Ok(BleScenario::new(
            peripherals,
            mtu,
            file.peripheral_name,
            factory,
        ))
    }

    pub fn peripherals(&self) -> &[SimulatedPeripheral] {
        &self.peripherals
    }

    pub fn mtu(&self) -> u16 {
        self.mtu
    }
}

fn value_bytes(
    hex: &Option<String>,
    utf8: &Option<String>,
    what: &str,
) -> Result<Vec<u8>, ScenarioError> {
    match (hex, utf8) {
        (Some(hex), None) => {
            decode_hex(hex).ok_or_else(|| scenario_error(format!("{what}: invalid hex")))
        }
        (None, Some(text)) => Ok(text.as_bytes().to_vec()),
        _ => Err(scenario_error(format!(
            "{what} needs exactly one of hex or utf8"
        ))),
    }
}

/// Uppercase 8-4-4-4-12 hex form, or `None` when `text` is not a UUID.
fn canonical_uuid(text: &str) -> Option<String> {
    let groups: Vec<&str> = text.split('-').collect();
    let shaped = groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.chars().all(|c| c.is_ascii_hexdigit()));
    shaped.then(|| text.to_ascii_uppercase())
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The service-side BLE socket: one scenario, at most one connected client,
/// and the history of every finished responder's log. Cloning shares state.
#[derive(Clone)]
pub struct BleSocket {
    inner: Arc<Inner>,
}

struct Inner {
    scenario: BleScenario,
    busy: AtomicBool,
    released: Notify,
    /// The connected client's responder. Locked only for synchronous
    /// responder calls, never across an await.
    active: Mutex<Option<BleResponder>>,
    history: Mutex<Vec<BleEvent>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Holds the single-client slot. Dropping it moves the responder's log into
/// the history and frees the slot, including when the connection task is
/// aborted at shutdown.
struct ActiveClient {
    inner: Arc<Inner>,
}

impl Drop for ActiveClient {
    fn drop(&mut self) {
        if let Some(responder) = lock(&self.inner.active).take() {
            lock(&self.inner.history).extend_from_slice(responder.log());
        }
        self.inner.busy.store(false, Ordering::SeqCst);
        self.inner.released.notify_waiters();
    }
}

#[derive(Default)]
struct Connection {
    /// Subscribed characteristics in subscription order.
    subscribed: Vec<String>,
    peripheral: Option<String>,
}

struct WireError {
    code: &'static str,
    detail: String,
}

fn wire_error(code: &'static str, detail: impl Into<String>) -> WireError {
    WireError {
        code,
        detail: detail.into(),
    }
}

impl From<BleError> for WireError {
    fn from(error: BleError) -> Self {
        match &error {
            BleError::NotConnected => wire_error("not_connected", ""),
            BleError::ServicesNotDiscovered => wire_error("services_not_discovered", ""),
            BleError::NotExposed(uuid) => wire_error("not_exposed", uuid.clone()),
            BleError::NoPeripheralName => wire_error("no_peripheral_name", ""),
            BleError::MtuRequestFailed => wire_error("mtu_request_failed", ""),
            BleError::UnexpectedWrite { .. } | BleError::ScriptOutOfOrder { .. } => {
                wire_error("script_violation", error.to_string())
            }
            BleError::PeerDisconnectNotObserved => wire_error("peer_disconnect_not_observed", ""),
        }
    }
}

fn failure(id: Value, error: &WireError) -> Value {
    json!({"id": id, "ok": false, "error": {"code": error.code, "detail": error.detail}})
}

fn uuid_arg(request: &Value, name: &str) -> Result<String, WireError> {
    request
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_ascii_uppercase)
        .ok_or_else(|| wire_error("bad_request", format!("missing string argument '{name}'")))
}

fn bytes_arg(request: &Value, name: &str) -> Result<Vec<u8>, WireError> {
    let text = request
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| wire_error("bad_request", format!("missing hex argument '{name}'")))?;
    decode_hex(text).ok_or_else(|| wire_error("bad_request", format!("'{name}' is not hex")))
}

impl BleSocket {
    pub fn new(scenario: BleScenario) -> Self {
        BleSocket {
            inner: Arc::new(Inner {
                scenario,
                busy: AtomicBool::new(false),
                released: Notify::new(),
                active: Mutex::new(None),
                history: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn scenario(&self) -> &BleScenario {
        &self.inner.scenario
    }

    /// Every finished connection's responder log in order, then the connected
    /// client's log so far.
    pub fn log(&self) -> Vec<BleEvent> {
        let mut log = lock(&self.inner.history).clone();
        if let Some(responder) = lock(&self.inner.active).as_ref() {
            log.extend_from_slice(responder.log());
        }
        log
    }

    async fn claim(&self) -> Option<ActiveClient> {
        let deadline = Instant::now() + BUSY_GRACE;
        loop {
            let released = self.inner.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if self
                .inner
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                *lock(&self.inner.active) = Some((self.inner.scenario.factory)());
                return Some(ActiveClient {
                    inner: self.inner.clone(),
                });
            }
            if tokio::time::timeout_at(deadline, released).await.is_err() {
                return None;
            }
        }
    }

    /// Serve one client connection until it closes.
    pub async fn serve<S>(&self, stream: S) -> std::io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let (read, mut write) = tokio::io::split(stream);
        let mut reader = BufReader::new(read);
        let hello = json!({"event": "hello", "protocol": PROTOCOL, "version": PROTOCOL_VERSION});
        send(&mut write, &[hello]).await?;
        let Some(_client) = self.claim().await else {
            tracing::info!(target: "ble", "refused a concurrent client: busy");
            send(&mut write, &[json!({"event": "error", "code": "busy"})]).await?;
            return write.shutdown().await;
        };
        tracing::info!(target: "ble", "client connected; responder built from the scenario");

        let mut connection = Connection::default();
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = (&mut reader)
                .take(MAX_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
                .await?;
            if read == 0 {
                break;
            }
            if line.len() > MAX_LINE_BYTES {
                let error = wire_error("bad_request", "request line too long");
                send(&mut write, &[failure(Value::Null, &error)]).await?;
                break;
            }
            let messages = self.handle(&mut connection, &line);
            send(&mut write, &messages).await?;
        }
        tracing::info!(target: "ble", "client disconnected");
        Ok(())
    }

    /// One request: its response, then the events it made eligible.
    fn handle(&self, connection: &mut Connection, line: &[u8]) -> Vec<Value> {
        let request: Value = match serde_json::from_slice(line) {
            Ok(request) => request,
            Err(error) => {
                let error = wire_error("bad_request", format!("invalid JSON: {error}"));
                return vec![failure(Value::Null, &error)];
            }
        };
        let Some(id) = request.get("id").and_then(Value::as_u64) else {
            let error = wire_error("bad_request", "missing or invalid id");
            return vec![failure(Value::Null, &error)];
        };
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        let mut events = Vec::new();
        let response = match self.dispatch(connection, op, &request, &mut events) {
            Ok(mut result) => {
                let logged = Value::Object(result.clone());
                tracing::info!(target: "ble", id, op, request = %request, result = %logged, "ok");
                result.insert("id".into(), id.into());
                result.insert("ok".into(), true.into());
                Value::Object(result)
            }
            Err(error) => {
                tracing::info!(target: "ble", id, op, request = %request, code = error.code, detail = %error.detail, "error");
                failure(id.into(), &error)
            }
        };
        let mut messages = vec![response];
        messages.extend(events);
        messages.extend(self.drain_notifications(connection));
        messages
    }

    fn with_responder<T>(
        &self,
        call: impl FnOnce(&mut BleResponder) -> Result<T, BleError>,
    ) -> Result<T, WireError> {
        let mut active = lock(&self.inner.active);
        let responder = active
            .as_mut()
            .ok_or_else(|| wire_error("not_connected", "no responder for this connection"))?;
        call(responder).map_err(WireError::from)
    }

    fn dispatch(
        &self,
        connection: &mut Connection,
        op: &str,
        request: &Value,
        events: &mut Vec<Value>,
    ) -> Result<Map<String, Value>, WireError> {
        let mut result = Map::new();
        match op {
            "scan_start" => {
                for peripheral in &self.inner.scenario.peripherals {
                    events.push(json!({
                        "event": "advert",
                        "peripheral": peripheral.id,
                        "service_uuids": peripheral.advert.service_uuids,
                        "manufacturer_data": encode_hex(&peripheral.advert.manufacturer_data),
                        "local_name": null,
                        "service_data": {},
                        "tx_power": null,
                    }));
                }
            }
            "scan_stop" => {}
            "connect" => {
                let id = uuid_arg(request, "peripheral")?;
                if !self.inner.scenario.peripherals.iter().any(|p| p.id == id) {
                    return Err(wire_error("unknown_peripheral", id));
                }
                // A client reuses its link, so a repeated connect to the
                // connected peripheral leaves discovery and subscriptions
                // intact. A new TCP connection is what rebuilds the responder.
                if connection.peripheral.as_deref() != Some(id.as_str()) {
                    self.with_responder(|responder| {
                        responder.connect();
                        Ok(())
                    })?;
                    connection.peripheral = Some(id);
                }
                result.insert("mtu".into(), self.inner.scenario.mtu.into());
                result.insert(
                    "peripheral_name".into(),
                    self.inner.scenario.peripheral_name.clone().into(),
                );
            }
            "discover" => {
                let characteristics = self.with_responder(|responder| {
                    responder.discover_services()?;
                    Ok(responder
                        .characteristics()
                        .map(String::from)
                        .collect::<Vec<_>>())
                })?;
                result.insert("characteristics".into(), characteristics.into());
            }
            "read" => {
                let uuid = uuid_arg(request, "uuid")?;
                let value = self.with_responder(|responder| responder.read(&uuid))?;
                result.insert("value".into(), encode_hex(&value).into());
            }
            "write" => {
                let uuid = uuid_arg(request, "uuid")?;
                let value = bytes_arg(request, "value")?;
                self.with_responder(|responder| responder.write(&uuid, &value))?;
            }
            "write_fenced" => {
                let uuid = uuid_arg(request, "uuid")?;
                let value = bytes_arg(request, "value")?;
                let notify_uuid = uuid_arg(request, "notify_uuid")?;
                self.with_responder(|responder| {
                    responder.write_with_notification_fence(&uuid, &value, &notify_uuid)
                })?;
            }
            "subscribe" => {
                let uuid = uuid_arg(request, "uuid")?;
                self.with_responder(|responder| responder.subscribe(&uuid, CccdMode::Notify))?;
                if !connection.subscribed.contains(&uuid) {
                    connection.subscribed.push(uuid);
                }
            }
            "await_disconnect" => {
                self.with_responder(BleResponder::await_disconnect)?;
                connection.subscribed.clear();
                events.push(json!({
                    "event": "disconnected",
                    "peripheral": connection.peripheral.take(),
                    "reason": "peer_disconnect",
                }));
            }
            "disconnect" => {
                connection.subscribed.clear();
                connection.peripheral = None;
            }
            other => return Err(wire_error("bad_request", format!("unknown op '{other}'"))),
        }
        Ok(result)
    }

    /// Every eligible notification on the subscribed characteristics. Passes
    /// repeat until none yields, so an ordered script that alternates
    /// characteristics drains in script order.
    fn drain_notifications(&self, connection: &Connection) -> Vec<Value> {
        let mut events = Vec::new();
        let mut active = lock(&self.inner.active);
        let Some(responder) = active.as_mut() else {
            return events;
        };
        loop {
            let before = events.len();
            for uuid in &connection.subscribed {
                while let Some(payload) = responder.take_notification(uuid) {
                    let value = encode_hex(&payload);
                    tracing::info!(target: "ble", uuid = %uuid, value = %value, "notification");
                    events.push(json!({"event": "notification", "uuid": uuid, "value": value}));
                }
            }
            if events.len() == before {
                return events;
            }
        }
    }
}

async fn send<W: AsyncWrite + Unpin>(write: &mut W, messages: &[Value]) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut bytes, message)?;
        bytes.push(b'\n');
    }
    write.write_all(&bytes).await?;
    write.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_canonicalized_and_validated() {
        assert_eq!(
            canonical_uuid("0000fff1-0000-1000-8000-00805f9b34fb").as_deref(),
            Some("0000FFF1-0000-1000-8000-00805F9B34FB")
        );
        assert_eq!(canonical_uuid("FFF1"), None);
        assert_eq!(canonical_uuid("0000FFF1-0000-1000-8000-00805F9B34FG"), None);
    }

    #[test]
    fn hex_round_trips_and_rejects_bad_input() {
        assert_eq!(decode_hex("0a0B"), Some(vec![0x0a, 0x0b]));
        assert_eq!(encode_hex(&[0x0a, 0x0b]), "0a0b");
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex(""), Some(Vec::new()));
    }

    #[test]
    fn script_errors_map_to_script_violation_with_text() {
        let error = WireError::from(BleError::ScriptOutOfOrder {
            expected: "a".into(),
            actual: "b".into(),
        });
        assert_eq!(error.code, "script_violation");
        assert!(error.detail.contains("expected a"));
    }
}
