//! Shared canonical observation recorder used by initiator and responder paths.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    ActionInvocationRecord, ActionOutcome, ActionRole, BundleHeader, ClockPoint, Confidence,
    EpistemicClass, EpistemicMetadata, ExecutionContext, LifecycleMarker, LifecycleRecord,
    LossCounters, ObservationCommon, ObservationLine, PayloadMetadata, PayloadRange,
    StateTransition, MAX_INLINE_PAYLOAD_BYTES, OBSERVATION_SCHEMA_VERSION,
};

#[derive(Debug, Error)]
pub enum RecorderError {
    #[error("observation recorder I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("observation recorder JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("observation recorder contract failed: {0}")]
    Contract(String),
}

#[derive(Debug)]
struct RecorderState {
    lines: Vec<ObservationLine>,
    next_ordinal: u64,
}

#[derive(Debug, Clone)]
pub struct ObservationRecorder {
    started: Instant,
    path: Option<PathBuf>,
    run_id: String,
    state: Arc<Mutex<RecorderState>>,
}

#[derive(Debug, Clone)]
pub struct PayloadMetadataBuilder {
    length: u64,
    hasher: Sha256,
    inline: Option<Vec<u8>>,
    stream_ranges: Vec<PayloadRange>,
    pending_range_length: usize,
    pending_range_hasher: Sha256,
}

impl Default for PayloadMetadataBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PayloadMetadataBuilder {
    const STREAM_RANGE_BYTES: usize = 64 * 1024;

    pub fn new() -> Self {
        Self {
            length: 0,
            hasher: Sha256::new(),
            inline: Some(Vec::new()),
            stream_ranges: Vec::new(),
            pending_range_length: 0,
            pending_range_hasher: Sha256::new(),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        if self.inline.as_ref().is_some_and(|inline| {
            inline.len().saturating_add(bytes.len()) <= MAX_INLINE_PAYLOAD_BYTES as usize
        }) {
            self.inline
                .as_mut()
                .expect("inline payload remains available")
                .extend_from_slice(bytes);
        } else {
            self.inline = None;
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let available = Self::STREAM_RANGE_BYTES - self.pending_range_length;
            let take = available.min(remaining.len());
            let chunk = &remaining[..take];
            self.pending_range_hasher.update(chunk);
            self.pending_range_length += take;
            self.length += take as u64;
            remaining = &remaining[take..];
            if self.pending_range_length == Self::STREAM_RANGE_BYTES {
                self.finish_pending_range();
            }
        }
    }

    pub fn metadata(&self) -> PayloadMetadata {
        let mut stream_ranges = self.stream_ranges.clone();
        if self.pending_range_length > 0 {
            stream_ranges.push(PayloadRange {
                offset: self.length - self.pending_range_length as u64,
                length: self.pending_range_length as u64,
                sha256: format!("{:x}", self.pending_range_hasher.clone().finalize()),
            });
        }
        PayloadMetadata {
            length: self.length,
            sha256: format!("{:x}", self.hasher.clone().finalize()),
            inline_hex: self.inline.as_deref().map(hex),
            stream_ranges: if self.inline.is_none() {
                stream_ranges
            } else {
                Vec::new()
            },
            ranges: Vec::new(),
        }
    }

    fn finish_pending_range(&mut self) {
        self.stream_ranges.push(PayloadRange {
            offset: self.length - self.pending_range_length as u64,
            length: self.pending_range_length as u64,
            sha256: format!("{:x}", self.pending_range_hasher.clone().finalize()),
        });
        self.pending_range_length = 0;
        self.pending_range_hasher = Sha256::new();
    }
}

impl ObservationRecorder {
    /// Open or create one append-only bundle. Existing content is checked
    /// record-by-record before any append; a partial or foreign bundle fails
    /// closed instead of being silently replaced.
    pub fn open(path: Option<PathBuf>, header: BundleHeader) -> Result<Self, RecorderError> {
        require_header(&header)?;
        let lines = match path.as_deref().filter(|path| path.exists()) {
            Some(path) => load_existing(path, &header)?,
            None => {
                if let Some(path) = path.as_deref() {
                    write_new(path, &ObservationLine::BundleHeader(header.clone()))?;
                }
                vec![ObservationLine::BundleHeader(header.clone())]
            }
        };
        let next_ordinal = lines
            .last()
            .map(ObservationLine::ordinal)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| RecorderError::Contract("ordinal overflow".into()))?;
        Ok(Self {
            started: Instant::now(),
            path,
            run_id: header.run_id,
            state: Arc::new(Mutex::new(RecorderState {
                lines,
                next_ordinal,
            })),
        })
    }

    pub fn record_lifecycle(
        &self,
        context: ExecutionContext,
        marker: LifecycleMarker,
        transition: Option<StateTransition>,
        attempt: Option<u32>,
        detail: BTreeMap<String, String>,
    ) -> Result<u64, RecorderError> {
        self.append(context, |common| {
            ObservationLine::Lifecycle(LifecycleRecord {
                common,
                marker,
                transition,
                attempt,
                detail,
            })
        })
    }

    pub fn record_action(
        &self,
        context: ExecutionContext,
        catalog_revision: String,
        action_id: String,
        role: ActionRole,
        parameters: BTreeMap<String, serde_json::Value>,
        outcome: ActionOutcome,
    ) -> Result<u64, RecorderError> {
        self.append(context, |common| {
            ObservationLine::ActionInvocation(ActionInvocationRecord {
                common,
                catalog_revision,
                action_id,
                role,
                parameters,
                outcome,
            })
        })
    }

    pub fn append<F>(&self, context: ExecutionContext, make: F) -> Result<u64, RecorderError>
    where
        F: FnOnce(ObservationCommon) -> ObservationLine,
    {
        let mut state = self
            .state
            .lock()
            .expect("observation recorder lock poisoned");
        let ordinal = state.next_ordinal;
        let line = make(ObservationCommon {
            schema: OBSERVATION_SCHEMA_VERSION.to_string(),
            run_id: self.run_id.clone(),
            record_id: format!("record-{ordinal:016x}"),
            ordinal,
            context,
            time: ClockPoint {
                clock: "process-monotonic".into(),
                value: self.started.elapsed().as_millis() as u64,
            },
            physical_context: BTreeMap::new(),
            artifact_ranges: Vec::new(),
            epistemic: direct_epistemic(),
        });
        if line.schema() != OBSERVATION_SCHEMA_VERSION
            || line.run_id() != self.run_id
            || line.ordinal() != ordinal
        {
            return Err(RecorderError::Contract(
                "record builder changed canonical identity fields".into(),
            ));
        }
        append_line(self.path.as_deref(), &line)?;
        state.lines.push(line);
        state.next_ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| RecorderError::Contract("ordinal overflow".into()))?;
        Ok(ordinal)
    }

    pub fn export_json(&self, after: u64) -> Result<String, RecorderError> {
        let state = self
            .state
            .lock()
            .expect("observation recorder lock poisoned");
        let header = state
            .lines
            .first()
            .expect("recorder always contains a header");
        let records = state
            .lines
            .iter()
            .skip(1)
            .filter(|line| line.ordinal() > after)
            .collect::<Vec<_>>();
        let cursor = state.next_ordinal.saturating_sub(1);
        Ok(serde_json::to_string(&serde_json::json!({
            "schema": OBSERVATION_SCHEMA_VERSION,
            "runId": self.run_id,
            "cursor": cursor,
            "header": header,
            "records": records,
        }))?)
    }

    pub fn cursor(&self) -> u64 {
        self.state
            .lock()
            .expect("observation recorder lock poisoned")
            .next_ordinal
            .saturating_sub(1)
    }
}

pub fn payload_metadata(bytes: &[u8]) -> PayloadMetadata {
    let mut builder = PayloadMetadataBuilder::new();
    builder.update(bytes);
    builder.metadata()
}

pub fn direct_epistemic() -> EpistemicMetadata {
    EpistemicMetadata {
        class: EpistemicClass::DirectObservation,
        confidence: Confidence::Exact,
        alternatives: Vec::new(),
        falsifier: None,
        unknowns: Vec::new(),
    }
}

pub const fn no_loss() -> LossCounters {
    LossCounters {
        dropped_records: 0,
        dropped_bytes: 0,
        truncated_payloads: 0,
    }
}

fn require_header(header: &BundleHeader) -> Result<(), RecorderError> {
    if header.schema != OBSERVATION_SCHEMA_VERSION
        || header.ordinal != 0
        || header.run_id.is_empty()
        || header.record_id.is_empty()
    {
        return Err(RecorderError::Contract(
            "header must use camera-observation/v1 with nonempty IDs and ordinal zero".into(),
        ));
    }
    Ok(())
}

fn load_existing(
    path: &Path,
    expected: &BundleHeader,
) -> Result<Vec<ObservationLine>, RecorderError> {
    let text = std::fs::read_to_string(path)?;
    let mut lines = Vec::new();
    for raw in text.lines().filter(|line| !line.trim().is_empty()) {
        lines.push(serde_json::from_str::<ObservationLine>(raw)?);
    }
    let Some(ObservationLine::BundleHeader(actual)) = lines.first() else {
        return Err(RecorderError::Contract(
            "existing bundle does not begin with a header".into(),
        ));
    };
    if actual != expected {
        return Err(RecorderError::Contract(
            "existing bundle header does not match this recorder".into(),
        ));
    }
    for (index, line) in lines.iter().enumerate() {
        if line.schema() != OBSERVATION_SCHEMA_VERSION
            || line.run_id() != expected.run_id
            || line.ordinal() != index as u64
        {
            return Err(RecorderError::Contract(format!(
                "existing bundle record {index} has incoherent identity"
            )));
        }
    }
    Ok(lines)
}

fn write_new(path: &Path, header: &ObservationLine) -> Result<(), RecorderError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    serde_json::to_writer(&mut file, header)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

fn append_line(path: Option<&Path>, line: &ObservationLine) -> Result<(), RecorderError> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut file = OpenOptions::new().append(true).open(path)?;
    serde_json::to_writer(&mut file, line)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        validate_bundles, BleGattOperation, BleGattRecord, CameraContext, CaptureClock,
        CaptureContext, CaptureInterface, CaptureInterfaceType, ClientContext, ClockType,
        ClockUnit, TransportOutcome,
    };

    fn header(run_id: &str) -> BundleHeader {
        BundleHeader {
            schema: OBSERVATION_SCHEMA_VERSION.into(),
            run_id: run_id.into(),
            record_id: "header".into(),
            ordinal: 0,
            camera: CameraContext {
                manufacturer: "TEST".into(),
                model: "BODY".into(),
                body_id: "sanitized-body".into(),
                firmware: "1".into(),
            },
            client: ClientContext {
                artifact: "test".into(),
                version: "1".into(),
                platform: "test".into(),
            },
            capture: CaptureContext {
                interfaces: vec![CaptureInterface {
                    id: "test".into(),
                    interface_type: CaptureInterfaceType::Synthetic,
                    role: "test".into(),
                }],
                clocks: vec![CaptureClock {
                    id: "process-monotonic".into(),
                    clock_type: ClockType::Monotonic,
                    unit: ClockUnit::Milliseconds,
                }],
                clock_mappings: Vec::new(),
                loss: no_loss(),
                redactions: Vec::new(),
                tool_versions: BTreeMap::from([("test".into(), "1".into())]),
                artifacts: Vec::new(),
            },
            epistemic: direct_epistemic(),
        }
    }

    #[test]
    fn bounded_payload_hashes_the_complete_transfer() {
        let payload = vec![0xab; PayloadMetadataBuilder::STREAM_RANGE_BYTES * 2 + 17];
        let metadata = payload_metadata(&payload);
        assert_eq!(metadata.length, payload.len() as u64);
        assert_eq!(metadata.sha256.len(), 64);
        assert!(metadata.inline_hex.is_none());
        assert_eq!(metadata.stream_ranges.len(), 3);
        assert_eq!(metadata.stream_ranges[0].offset, 0);
        assert_eq!(
            metadata.stream_ranges[0].length,
            PayloadMetadataBuilder::STREAM_RANGE_BYTES as u64
        );
        assert_eq!(
            metadata.stream_ranges[2].offset,
            (PayloadMetadataBuilder::STREAM_RANGE_BYTES * 2) as u64
        );
        assert_eq!(metadata.stream_ranges[2].length, 17);
        assert_eq!(metadata.stream_ranges[0].sha256.len(), 64);

        let mut split = PayloadMetadataBuilder::new();
        for chunk in payload.chunks(997) {
            split.update(chunk);
        }
        assert_eq!(
            split.metadata(),
            metadata,
            "range hashes must not depend on transport read boundaries"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large-transfer.jsonl");
        let recorder = ObservationRecorder::open(Some(path.clone()), header("large-run")).unwrap();
        recorder
            .append(
                ExecutionContext {
                    connection: "app".into(),
                    mode: "image-transfer".into(),
                    state: "streaming".into(),
                },
                |common| {
                    ObservationLine::BleGatt(BleGattRecord {
                        common,
                        connection_instance: "fixture".into(),
                        operation: BleGattOperation::Notify,
                        service: "service".into(),
                        characteristic: "characteristic".into(),
                        outcome: TransportOutcome::Ok,
                        payload: Some(metadata),
                    })
                },
            )
            .unwrap();
        let bundle = std::fs::read_to_string(path).unwrap();
        let validated = validate_bundles(&[&bundle]).unwrap();
        assert_eq!(validated.report.total_nonblank, 2);
        assert_eq!(validated.report.rejected, 0);
    }

    #[test]
    fn restart_preserves_cursor_and_rejects_a_different_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("observations.jsonl");
        let recorder = ObservationRecorder::open(Some(path.clone()), header("run")).unwrap();
        recorder
            .record_lifecycle(
                ExecutionContext {
                    connection: "app".into(),
                    mode: "idle".into(),
                    state: "ready".into(),
                },
                LifecycleMarker::ConnectionOpened,
                None,
                None,
                BTreeMap::new(),
            )
            .unwrap();
        drop(recorder);

        let reopened = ObservationRecorder::open(Some(path.clone()), header("run")).unwrap();
        assert_eq!(reopened.cursor(), 1);
        assert!(ObservationRecorder::open(Some(path), header("other")).is_err());
    }
}
