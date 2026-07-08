use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
}

struct Inner {
    started_at: Instant,
    last_activity: Mutex<Instant>,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    liveview_frames: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
pub struct MetricsSnapshot {
    pub uptime_ms: u64,
    pub idle_ms: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub bytes_transferred: u64,
    pub liveview_frames: u64,
    pub memory_allocated_bytes: u64,
}

impl Default for Metrics {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            inner: Arc::new(Inner {
                started_at: now,
                last_activity: Mutex::new(now),
                bytes_read: AtomicU64::new(0),
                bytes_written: AtomicU64::new(0),
                liveview_frames: AtomicU64::new(0),
            }),
        }
    }
}

impl Metrics {
    pub fn record_read(&self, bytes: usize) {
        self.inner
            .bytes_read
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn record_write(&self, bytes: usize) {
        self.inner
            .bytes_written
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn record_liveview_frame(&self, bytes: usize) {
        self.inner.liveview_frames.fetch_add(1, Ordering::Relaxed);
        self.record_write(bytes);
        self.touch();
    }

    pub fn touch(&self) {
        if let Ok(mut last_activity) = self.inner.last_activity.lock() {
            *last_activity = Instant::now();
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let now = Instant::now();
        let last_activity = self
            .inner
            .last_activity
            .lock()
            .map(|instant| *instant)
            .unwrap_or(self.inner.started_at);
        let bytes_read = self.inner.bytes_read.load(Ordering::Relaxed);
        let bytes_written = self.inner.bytes_written.load(Ordering::Relaxed);
        MetricsSnapshot {
            uptime_ms: now
                .saturating_duration_since(self.inner.started_at)
                .as_millis() as u64,
            idle_ms: now.saturating_duration_since(last_activity).as_millis() as u64,
            bytes_read,
            bytes_written,
            bytes_transferred: bytes_read.saturating_add(bytes_written),
            liveview_frames: self.inner.liveview_frames.load(Ordering::Relaxed),
            memory_allocated_bytes: process_memory_bytes().unwrap_or(0),
        }
    }
}

fn process_memory_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb = status.lines().find_map(|line| {
        let value = line.strip_prefix("VmRSS:")?.trim();
        value
            .split_whitespace()
            .next()
            .and_then(|raw| raw.parse::<u64>().ok())
    })?;
    Some(kb.saturating_mul(1024))
}
