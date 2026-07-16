use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use camera_protocol_ffi::{ConnectionActivityEvent, StepReport};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TraceFormat {
    Jsonl,
    Text,
}

pub struct TraceWriter {
    started: Instant,
    format: TraceFormat,
    output: Mutex<Box<dyn Write + Send>>,
    next_frame_id: AtomicU64,
    failure: Mutex<Option<String>>,
}

impl TraceWriter {
    pub fn new(format: TraceFormat, output: Box<dyn Write + Send>) -> Self {
        Self {
            started: Instant::now(),
            format,
            output: Mutex::new(output),
            next_frame_id: AtomicU64::new(1),
            failure: Mutex::new(None),
        }
    }

    pub fn wire(&self, direction: &str, channel: &str, bytes: &[u8]) -> io::Result<u64> {
        let frame_id = self.next_frame_id.fetch_add(1, Ordering::Relaxed);
        self.emit(json!({
            "kind": "wire",
            "elapsedMs": self.elapsed_ms(),
            "direction": direction,
            "channel": channel,
            "frameId": frame_id,
            "offset": 0,
            "length": bytes.len(),
            "hex": encode_hex(bytes),
        }))?;
        Ok(frame_id)
    }

    pub fn begin_wire_frame(&self, direction: &str, channel: &str, length: u64) -> io::Result<u64> {
        let frame_id = self.next_frame_id.fetch_add(1, Ordering::Relaxed);
        self.emit(json!({
            "kind": "wireStart",
            "elapsedMs": self.elapsed_ms(),
            "direction": direction,
            "channel": channel,
            "frameId": frame_id,
            "length": length,
        }))?;
        Ok(frame_id)
    }

    pub fn wire_chunk(
        &self,
        direction: &str,
        channel: &str,
        frame_id: u64,
        offset: u64,
        bytes: &[u8],
    ) -> io::Result<()> {
        self.emit(json!({
            "kind": "wireChunk",
            "elapsedMs": self.elapsed_ms(),
            "direction": direction,
            "channel": channel,
            "frameId": frame_id,
            "offset": offset,
            "length": bytes.len(),
            "hex": encode_hex(bytes),
        }))
    }

    pub fn live_view(&self, length: usize) -> io::Result<()> {
        self.emit(json!({
            "kind": "liveView",
            "elapsedMs": self.elapsed_ms(),
            "length": length,
            "ready": true,
        }))
    }

    pub fn step(&self, report: &StepReport) -> io::Result<()> {
        self.emit(json!({
            "kind": "step",
            "elapsedMs": self.elapsed_ms(),
            "path": report.step_path,
            "verb": report.verb,
            "outcome": format!("{:?}", report.outcome),
            "operation": report.operation,
            "property": report.property,
            "response": report.response_code,
            "transactionId": report.transaction_id,
            "tolerant": report.tolerant,
            "attempts": report.attempts,
            "error": report.error,
            "activityId": report.activity_id,
            "activityVersion": report.activity_version,
        }))
    }

    pub fn activity(&self, event: &ConnectionActivityEvent) -> io::Result<()> {
        self.emit(json!({
            "kind": "activity",
            "elapsedMs": self.elapsed_ms(),
            "event": format!("{event:?}"),
        }))
    }

    pub fn session(&self, state: &str, detail: Value) -> io::Result<()> {
        self.emit(json!({
            "kind": "session",
            "elapsedMs": self.elapsed_ms(),
            "state": state,
            "detail": detail,
        }))
    }

    pub fn checkpoint(&self, name: &str, detail: Value) -> io::Result<()> {
        self.emit(json!({
            "kind": "checkpoint",
            "elapsedMs": self.elapsed_ms(),
            "name": name,
            "detail": detail,
        }))
    }

    pub fn outcome(&self, status: &str, detail: Value) -> io::Result<()> {
        self.emit(json!({
            "kind": "outcome",
            "elapsedMs": self.elapsed_ms(),
            "status": status,
            "detail": detail,
        }))
    }

    fn elapsed_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    fn emit(&self, value: Value) -> io::Result<()> {
        if let Some(detail) = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, detail));
        }
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = (|| {
            match self.format {
                TraceFormat::Jsonl => serde_json::to_writer(&mut *output, &value)?,
                TraceFormat::Text => write_text(&mut *output, &value)?,
            }
            output.write_all(b"\n")?;
            output.flush()
        })();
        drop(output);
        if let Err(error) = &result {
            *self
                .failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
        }
        result
    }
}

fn write_text(mut output: impl Write, value: &Value) -> io::Result<()> {
    let elapsed = value.get("elapsedMs").and_then(Value::as_u64).unwrap_or(0);
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("record");
    write!(output, "{elapsed:>8}ms {kind}")?;
    if let Some(direction) = value.get("direction").and_then(Value::as_str) {
        write!(output, " {direction}")?;
    }
    if let Some(channel) = value.get("channel").and_then(Value::as_str) {
        write!(output, " {channel}")?;
    }
    if let Some(path) = value.get("path").and_then(Value::as_str) {
        write!(output, " {path}")?;
    }
    if let Some(hex) = value.get("hex").and_then(Value::as_str) {
        write!(output, " {hex}")?;
    } else {
        write!(output, " {value}")?;
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "injected"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn hex_is_lowercase_and_complete() {
        assert_eq!(encode_hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn trace_failures_are_sticky() {
        let trace = TraceWriter::new(TraceFormat::Jsonl, Box::new(FailingWriter));
        assert!(trace.session("first", json!({})).is_err());
        let second = trace.session("second", json!({})).unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::BrokenPipe);
        assert!(second.to_string().contains("injected"));
    }
}
