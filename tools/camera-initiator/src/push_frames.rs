//! Bounded retention of payloads from an already-open push channel.
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use camera_config::PayloadMetadataBuilder;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWriteExt};

use crate::transport::FrameReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PushFrameLimits {
    pub frame_count: u32,
    pub max_frame_bytes: usize,
    pub max_total_bytes: u64,
    pub timeout_ms: u64,
}

impl PushFrameLimits {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.frame_count > 0, "frameCount must be positive");
        anyhow::ensure!(
            self.max_frame_bytes >= 18,
            "maxFrameBytes must be at least 18"
        );
        anyhow::ensure!(
            self.max_total_bytes >= 18,
            "maxTotalBytes must be at least 18"
        );
        anyhow::ensure!(self.timeout_ms > 0, "timeoutMs must be positive");
        anyhow::ensure!(
            Instant::now()
                .checked_add(Duration::from_millis(self.timeout_ms))
                .is_some(),
            "timeoutMs exceeds host deadline range"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PushFrameTermination {
    Cancelled,
    FrameCount,
    HostTimeout,
    PeerClosed,
    TruncatedFrame,
    OversizedFrame,
    ByteLimit,
    MalformedFrame,
    IoError,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushFrameReport {
    pub frames: Vec<RetainedPushFrame>,
    pub wire_bytes: u64,
    pub termination: PushFrameTermination,
    pub error: Option<String>,
}

impl PushFrameReport {
    pub(crate) fn failed(termination: PushFrameTermination, error: impl Into<String>) -> Self {
        Self {
            frames: Vec::new(),
            wire_bytes: 0,
            termination,
            error: Some(error.into()),
        }
    }
}

/// Create one handle per sampling step. Clones share completed-frame metadata.
#[derive(Debug, Clone, Default)]
pub struct PushFrameProgress {
    state: Arc<Mutex<PushFrameProgressState>>,
}

#[derive(Debug, Default)]
struct PushFrameProgressState {
    frames: Vec<RetainedPushFrame>,
    wire_bytes: u64,
}

impl PushFrameProgress {
    /// Snapshot after dropping the sampling future. Pending reads and writes
    /// cannot establish exact byte counts or completed artifact metadata.
    pub fn cancelled_report(&self) -> PushFrameReport {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        PushFrameReport {
            frames: state.frames.clone(),
            wire_bytes: state.wire_bytes,
            termination: PushFrameTermination::Cancelled,
            error: Some("host canceled sampling; wireBytes is a lower bound; an in-flight artifact may be absent, partial, or complete".into()),
        }
    }

    fn record_wire_bytes(&self, wire_bytes: u64) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .wire_bytes = wire_bytes;
    }

    fn record_frame(&self, frame: RetainedPushFrame) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .frames
            .push(frame);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetainedPushFrame {
    pub path: String,
    pub length: u64,
    pub sha256: String,
    pub received_unix_ms: u64,
    pub received_elapsed_ms: u64,
    pub jpeg: JpegGeometry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JpegGeometry {
    pub width: u16,
    pub height: u16,
    pub precision: u8,
    pub components: u8,
}

/// Check marker structure and geometry without decoding entropy-coded samples.
pub fn jpeg_geometry(bytes: &[u8]) -> Result<JpegGeometry, String> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return Err("JPEG has no SOI".into());
    }
    let mut offset = 2;
    let mut geometry = None;
    let mut in_scan = false;
    let mut had_scan = false;
    while offset < bytes.len() {
        if in_scan && bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        if bytes[offset] != 0xff {
            return Err("expected JPEG marker".into());
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes.get(offset).ok_or("truncated JPEG marker")?;
        offset += 1;
        if in_scan && (marker == 0 || (0xd0..=0xd7).contains(&marker)) {
            continue;
        }
        in_scan = false;
        if marker == 0xd9 {
            if offset != bytes.len() {
                return Err("bytes after JPEG EOI".into());
            }
            if !had_scan {
                return Err("JPEG has no scan".into());
            }
            return geometry.ok_or_else(|| "JPEG has no SOF geometry".into());
        }
        if marker == 0 || marker == 0xd8 || (0xd0..=0xd7).contains(&marker) {
            return Err("unexpected JPEG marker".into());
        }
        if marker == 1 {
            continue;
        }
        let size = bytes
            .get(offset..offset + 2)
            .ok_or("truncated JPEG segment length")?;
        let size = u16::from_be_bytes([size[0], size[1]]) as usize;
        if size < 2 {
            return Err("invalid JPEG segment length".into());
        }
        let segment = bytes
            .get(offset + 2..offset + size)
            .ok_or("truncated JPEG segment")?;
        offset += size;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if geometry.is_some() || segment.len() < 6 {
                return Err("invalid or repeated JPEG SOF".into());
            }
            let value = JpegGeometry {
                precision: segment[0],
                height: u16::from_be_bytes([segment[1], segment[2]]),
                width: u16::from_be_bytes([segment[3], segment[4]]),
                components: segment[5],
            };
            if value.width == 0
                || value.height == 0
                || value.precision == 0
                || value.components == 0
                || segment.len() != 6 + 3 * value.components as usize
            {
                return Err("invalid JPEG SOF geometry".into());
            }
            geometry = Some(value);
        }
        if marker == 0xda {
            if geometry.is_none()
                || segment.is_empty()
                || segment[0] == 0
                || segment.len() != 4 + 2 * segment[0] as usize
            {
                return Err("invalid JPEG scan header".into());
            }
            had_scan = true;
            in_scan = true;
        }
    }
    Err("JPEG has no EOI".into())
}

pub(crate) async fn sample_push_frames<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
    limits: PushFrameLimits,
    transport_max: usize,
    step_dir: &Path,
    report_root: &Path,
    progress: PushFrameProgress,
) -> PushFrameReport {
    use PushFrameTermination as End;
    if let Err(error) = limits.validate() {
        return PushFrameReport::failed(End::MalformedFrame, error.to_string());
    }
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(limits.timeout_ms);
    let mut report = PushFrameReport {
        frames: Vec::new(),
        wire_bytes: 0,
        termination: End::FrameCount,
        error: None,
    };
    for index in 0..limits.frame_count {
        let remaining = limits.max_total_bytes - report.wire_bytes;
        let max_frame = limits.max_frame_bytes.min(transport_max);
        if remaining < 8 {
            report.termination = End::ByteLimit;
            report.error = Some("total wire byte limit reached".into());
            break;
        }
        let max = max_frame.min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let packet = match tokio::time::timeout_at(deadline, reader.read_frame(max)).await {
            Err(_) => {
                report.wire_bytes += reader.buffered_len() as u64;
                report.termination = End::HostTimeout;
                report.error = Some("host sampling deadline expired".into());
                break;
            }
            Ok(Err(error)) => {
                report.wire_bytes += reader.buffered_len() as u64;
                report.termination = match error.kind() {
                    std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                        if reader.buffered_len() == 0 =>
                    {
                        End::PeerClosed
                    }
                    std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted => End::TruncatedFrame,
                    std::io::ErrorKind::InvalidData
                        if reader
                            .declared_length()
                            .is_some_and(|length| length > max_frame) =>
                    {
                        End::OversizedFrame
                    }
                    std::io::ErrorKind::InvalidData
                        if reader
                            .declared_length()
                            .is_some_and(|length| length as u64 > remaining) =>
                    {
                        End::ByteLimit
                    }
                    std::io::ErrorKind::InvalidData => End::MalformedFrame,
                    _ => End::IoError,
                };
                report.error = Some(error.to_string());
                break;
            }
            Ok(Ok(packet)) => packet,
        };
        report.wire_bytes += packet.len() as u64;
        progress.record_wire_bytes(report.wire_bytes);
        let received_elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let received_unix_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_millis().min(u64::MAX as u128) as u64,
            Err(error) => {
                report.termination = End::IoError;
                report.error = Some(error.to_string());
                break;
            }
        };
        let jpeg = match protocol_primitives::liveview::parse_frame(&packet) {
            Some(jpeg) => jpeg,
            None => {
                report.termination = End::MalformedFrame;
                report.error = Some("invalid live-view framing".into());
                break;
            }
        };
        let geometry = match jpeg_geometry(jpeg) {
            Ok(geometry) => geometry,
            Err(error) => {
                report.termination = End::MalformedFrame;
                report.error = Some(error);
                break;
            }
        };
        let path = step_dir.join(format!("{index:04}.jpg"));
        let relative = match path.strip_prefix(report_root) {
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(error) => {
                report.termination = End::IoError;
                report.error = Some(error.to_string());
                break;
            }
        };
        let write = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await?;
            file.write_all(jpeg).await?;
            file.flush().await?;
            file.sync_all().await
        };
        match tokio::time::timeout_at(deadline, write).await {
            Err(_) => {
                report.termination = End::HostTimeout;
                report.error = Some("host deadline expired during artifact write".into());
                break;
            }
            Ok(Err(error)) => {
                report.termination = End::IoError;
                report.error = Some(format!("create/write {}: {error}", path.display()));
                break;
            }
            Ok(Ok(())) => {}
        }
        let mut metadata = PayloadMetadataBuilder::new();
        metadata.update(jpeg);
        let metadata = metadata.metadata();
        let retained = RetainedPushFrame {
            path: relative,
            length: metadata.length,
            sha256: metadata.sha256,
            received_unix_ms,
            received_elapsed_ms,
            jpeg: geometry,
        };
        progress.record_frame(retained.clone());
        report.frames.push(retained);
    }
    progress.record_wire_bytes(report.wire_bytes);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::net::{TcpListener, TcpStream};

    // One component, one scan, with stuffed entropy bytes and a restart marker.
    fn jpeg() -> Vec<u8> {
        vec![
            0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 1, 224, 2, 128, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1,
            1, 0, 0, 63, 0, 1, 0xff, 0, 2, 0xff, 0xd0, 3, 0xff, 0xd9,
        ]
    }
    struct Artifacts(std::path::PathBuf);
    impl Artifacts {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "initiator-push-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Artifacts {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn limits(count: u32) -> PushFrameLimits {
        PushFrameLimits {
            frame_count: count,
            max_frame_bytes: 4096,
            max_total_bytes: 8192,
            timeout_ms: 1000,
        }
    }
    async fn socket() -> (TcpStream, FrameReader<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let writer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (reader, _) = listener.accept().await.unwrap();
        (writer, FrameReader::new(reader))
    }
    #[test]
    fn jpeg_structure_extracts_geometry_and_rejects_incomplete_boundaries() {
        let valid = jpeg();
        assert_eq!(
            jpeg_geometry(&valid).unwrap(),
            JpegGeometry {
                width: 640,
                height: 480,
                precision: 8,
                components: 1
            }
        );
        for end in 0..valid.len() {
            assert!(
                jpeg_geometry(&valid[..end]).is_err(),
                "accepted prefix {end}"
            );
        }
        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(jpeg_geometry(&trailing).is_err());
        let mut zero_width = valid.clone();
        zero_width[9..11].fill(0);
        assert!(jpeg_geometry(&zero_width).is_err());
        let mut bad_sof = valid.clone();
        bad_sof[5] = 10;
        assert!(jpeg_geometry(&bad_sof).is_err());
        assert!(jpeg_geometry(&[0xff, 0xd8, 0xff, 0xd9]).is_err());
    }
    #[tokio::test]
    async fn push_sockets_retain_exact_count_metadata_and_leave_next_packet() {
        let artifacts = Artifacts::new();
        let (mut writer, mut reader) = socket().await;
        let jpeg = jpeg();
        let frame = protocol_primitives::liveview::frame_packet(&jpeg, 1);
        let mut all = frame.repeat(3);
        // Exercise the shared unwrap's offset adjustment too.
        let mut adjusted = frame.clone();
        adjusted.splice(18..18, [0xaa, 0xbb]);
        adjusted[12..16].copy_from_slice(&2u32.to_le_bytes());
        let length = adjusted.len() as u32;
        adjusted[..4].copy_from_slice(&length.to_le_bytes());
        all.splice(frame.len()..2 * frame.len(), adjusted);
        writer.write_all(&all[..2]).await.unwrap();
        writer.write_all(&all[2..]).await.unwrap();
        let report = sample_push_frames(
            &mut reader,
            limits(2),
            4096,
            &artifacts.0,
            &artifacts.0,
            PushFrameProgress::default(),
        )
        .await;
        assert_eq!(report.termination, PushFrameTermination::FrameCount);
        assert_eq!(report.frames.len(), 2);
        assert_eq!(report.wire_bytes, (2 * frame.len() + 2) as u64);
        let mut metadata = PayloadMetadataBuilder::new();
        metadata.update(&jpeg);
        for item in &report.frames {
            assert_eq!(item.length, jpeg.len() as u64);
            assert_eq!(item.sha256, metadata.metadata().sha256);
            assert!(item.received_unix_ms > 0);
            assert_eq!(std::fs::read(artifacts.0.join(&item.path)).unwrap(), jpeg);
        }
        assert!(report.frames[0].received_elapsed_ms <= report.frames[1].received_elapsed_ms);
        assert_eq!(reader.read_frame(4096).await.unwrap(), frame);
    }
    #[tokio::test]
    async fn push_sockets_classify_closure_truncation_malformed_and_limits() {
        use PushFrameTermination as End;
        let frame = protocol_primitives::liveview::frame_packet(&jpeg(), 1);
        let mut malformed = frame.clone();
        malformed[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut bad_jpeg = frame.clone();
        bad_jpeg.pop();
        let len = bad_jpeg.len() as u32;
        bad_jpeg[..4].copy_from_slice(&len.to_le_bytes());
        let cases = [
            (vec![], limits(1), End::PeerClosed),
            (vec![50, 0], limits(1), End::TruncatedFrame),
            (
                frame[..frame.len() - 1].to_vec(),
                limits(1),
                End::TruncatedFrame,
            ),
            (3u32.to_le_bytes().to_vec(), limits(1), End::MalformedFrame),
            (malformed, limits(1), End::MalformedFrame),
            (bad_jpeg, limits(1), End::MalformedFrame),
            (
                5000u32.to_le_bytes().to_vec(),
                limits(1),
                End::OversizedFrame,
            ),
            (
                frame.clone(),
                PushFrameLimits {
                    max_total_bytes: 18,
                    ..limits(1)
                },
                End::ByteLimit,
            ),
            (
                frame.clone(),
                PushFrameLimits {
                    max_total_bytes: frame.len() as u64,
                    ..limits(2)
                },
                End::ByteLimit,
            ),
            (frame.clone(), limits(2), End::PeerClosed),
        ];
        for (bytes, limits, expected) in cases {
            let artifacts = Artifacts::new();
            let (mut writer, mut reader) = socket().await;
            writer.write_all(&bytes).await.unwrap();
            writer.shutdown().await.unwrap();
            let report = sample_push_frames(
                &mut reader,
                limits,
                4096,
                &artifacts.0,
                &artifacts.0,
                PushFrameProgress::default(),
            )
            .await;
            assert_eq!(report.termination, expected, "{report:?}");
            assert!(report.wire_bytes <= limits.max_total_bytes);
            assert!(report.wire_bytes <= bytes.len() as u64);
            if expected == End::OversizedFrame {
                assert_eq!(report.wire_bytes, 4);
            }
            if expected == End::TruncatedFrame {
                assert_eq!(report.wire_bytes, bytes.len() as u64);
            }
            if limits.frame_count == 2 {
                assert_eq!(report.frames.len(), 1);
            } else {
                assert!(report.frames.is_empty());
            }
        }
    }
    struct SignalNextFrameRead {
        stream: TcpStream,
        remaining: usize,
        next_read: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl AsyncRead for SignalNextFrameRead {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            context: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.remaining == 0 {
                if let Some(sender) = self.next_read.take() {
                    let _ = sender.send(());
                }
            }
            let before = buffer.filled().len();
            let result = std::pin::Pin::new(&mut self.stream).poll_read(context, buffer);
            if matches!(result, std::task::Poll::Ready(Ok(()))) {
                self.remaining = self
                    .remaining
                    .saturating_sub(buffer.filled().len() - before);
            }
            result
        }
    }

    #[tokio::test]
    async fn cancelled_push_sample_preserves_completed_frame_progress() {
        let artifacts = Artifacts::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut writer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let jpeg = jpeg();
        let packet = protocol_primitives::liveview::frame_packet(&jpeg, 0);
        writer.write_all(&packet).await.unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut reader = FrameReader::new(SignalNextFrameRead {
            stream,
            remaining: packet.len(),
            next_read: Some(sender),
        });
        let progress = PushFrameProgress::default();
        let mut sampling = Box::pin(sample_push_frames(
            &mut reader,
            limits(2),
            4096,
            &artifacts.0,
            &artifacts.0,
            progress.clone(),
        ));
        // The next read is reached only after the first artifact is synced and
        // its metadata is published. The peer keeps the socket open and idle.
        tokio::select! {
            result = &mut sampling => panic!("sample ended before cancellation: {result:?}"),
            reached = receiver => reached.expect("sampler starts its next frame read"),
        }
        drop(sampling);
        let snapshot = progress.cancelled_report();
        assert_eq!(snapshot.termination, PushFrameTermination::Cancelled);
        assert_eq!(snapshot.frames.len(), 1);
        assert_eq!(snapshot.wire_bytes, packet.len() as u64);
        assert!(snapshot.error.as_ref().unwrap().contains("lower bound"));
        let retained = &snapshot.frames[0];
        assert_eq!(
            std::fs::read(artifacts.0.join(&retained.path)).unwrap(),
            jpeg
        );
        assert_eq!(retained.length, jpeg.len() as u64);
        let mut metadata = PayloadMetadataBuilder::new();
        metadata.update(&jpeg);
        assert_eq!(retained.sha256, metadata.metadata().sha256);
        assert!(retained.received_unix_ms > 0);
        assert_eq!((retained.jpeg.width, retained.jpeg.height), (640, 480));
        assert!(!artifacts.0.join("0001.jpg").exists());
    }

    #[tokio::test]
    async fn push_sockets_timeout_without_peer_close_and_preserve_existing_artifacts() {
        let artifacts = Artifacts::new();
        let (mut writer, mut reader) = socket().await;
        let report = sample_push_frames(
            &mut reader,
            PushFrameLimits {
                timeout_ms: 20,
                ..limits(1)
            },
            4096,
            &artifacts.0,
            &artifacts.0,
            PushFrameProgress::default(),
        )
        .await;
        assert_eq!(report.termination, PushFrameTermination::HostTimeout);
        std::fs::write(artifacts.0.join("0000.jpg"), b"preserve").unwrap();
        writer
            .write_all(&protocol_primitives::liveview::frame_packet(&jpeg(), 0))
            .await
            .unwrap();
        let report = sample_push_frames(
            &mut reader,
            limits(1),
            4096,
            &artifacts.0,
            &artifacts.0,
            PushFrameProgress::default(),
        )
        .await;
        assert_eq!(report.termination, PushFrameTermination::IoError);
        assert!(report.frames.is_empty());
        assert_eq!(
            std::fs::read(artifacts.0.join("0000.jpg")).unwrap(),
            b"preserve"
        );
    }
}
