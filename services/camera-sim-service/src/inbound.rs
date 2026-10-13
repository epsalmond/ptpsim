//! Resource limits for peer-controlled input.
//!
//! Command tasks reserve 4 MiB before spawning. Sixteen reservations bound
//! retained frames, decoder copies, and collected payloads to 64 MiB. Control,
//! event, live-view, and callback tasks have independent admission limits.
//! Local media and live-view fixture storage are outside this inbound budget.
//!
//! Initial handshakes expire five seconds after accept. Established connections
//! may idle indefinitely. A frame expires five seconds after its first byte;
//! cancelling a read preserves its prefix, body, and original deadline. A whole
//! initiator data phase expires after ten seconds, including its initial wait.

use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{timeout_at, Instant};

pub const HANDSHAKE_BYTES: usize = 4096;
pub const OPERATION_BYTES: usize = 64 * 1024;
pub const DATA_BYTES: usize = 1024 * 1024;
pub const DATA_FRAME_BYTES: usize = DATA_BYTES + 12;
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const DATA_PHASE_TIMEOUT: Duration = Duration::from_secs(10);
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5);
pub const COMMAND_CONNECTIONS: usize = 16;
pub const CONTROL_CONNECTIONS: usize = 8;
pub const EVENT_CONNECTIONS: usize = 16;
pub const LIVEVIEW_CONNECTIONS: usize = 8;
pub const CALLBACK_CONNECTIONS: usize = 8;
pub const COMMAND_RESERVATION_BYTES: usize = 4 * 1024 * 1024;
pub const COMMAND_BUDGET_BYTES: usize = COMMAND_CONNECTIONS * COMMAND_RESERVATION_BYTES;

/// A nonwaiting admission pool. Its owned permit also bounds retained buffers
/// for the entire task lifetime, including decoder and observation work.
#[derive(Clone)]
pub(crate) struct Admission(Arc<Semaphore>);

impl Admission {
    pub(crate) fn new(capacity: usize) -> Self {
        Self(Arc::new(Semaphore::new(capacity)))
    }

    pub(crate) fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.0.clone().try_acquire_owned().ok()
    }
}

#[derive(Clone, Copy)]
pub struct FrameReadPolicy {
    pub max_frame_bytes: usize,
    pub started_frame_timeout: Duration,
    pub initial_wait_deadline: Option<Instant>,
}

impl FrameReadPolicy {
    pub fn established(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes,
            started_frame_timeout: FRAME_TIMEOUT,
            initial_wait_deadline: None,
        }
    }

    pub fn until(max_frame_bytes: usize, deadline: Instant) -> Self {
        Self {
            initial_wait_deadline: Some(deadline),
            ..Self::established(max_frame_bytes)
        }
    }
}

/// One frame's state. Every completed read commits its bytes before the next
/// await, so dropping the future cannot discard a partially received frame.
#[derive(Default)]
pub struct FrameReader {
    prefix: [u8; 4],
    prefix_len: usize,
    body: Vec<u8>,
    received: usize,
    deadline: Option<Instant>,
}

impl FrameReader {
    pub fn started_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub async fn read_frame<R: AsyncRead + Unpin>(
        &mut self,
        stream: &mut R,
        policy: FrameReadPolicy,
    ) -> io::Result<Option<Vec<u8>>> {
        while self.prefix_len < 4 {
            let deadline = earlier(self.deadline, policy.initial_wait_deadline);
            let count = read_until(stream, &mut self.prefix[self.prefix_len..], deadline).await?;
            if count == 0 {
                if self.prefix_len == 0 {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial frame prefix",
                ));
            }
            self.prefix_len += count;
            self.deadline
                .get_or_insert_with(|| Instant::now() + policy.started_frame_timeout);
        }
        let length = u32::from_le_bytes(self.prefix) as usize;
        validate_frame_length(length, policy.max_frame_bytes)?;
        if self.body.is_empty() {
            self.body = vec![0; length];
            self.body[..4].copy_from_slice(&self.prefix);
            self.received = 4;
        }
        while self.received < length {
            let deadline = earlier(self.deadline, policy.initial_wait_deadline);
            let count = read_until(stream, &mut self.body[self.received..], deadline).await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial frame body",
                ));
            }
            self.received += count;
        }
        let frame = std::mem::take(&mut self.body);
        *self = Self::default();
        Ok(Some(frame))
    }
}

/// Reject the advertised total before allocating its body.
pub fn validate_frame_length(length: usize, maximum: usize) -> io::Result<()> {
    if !(8..=maximum).contains(&length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "inbound frame length exceeds role limit",
        ));
    }
    Ok(())
}

pub fn pcss_host_matches_peer(host: &str, peer: IpAddr) -> bool {
    host.parse::<IpAddr>()
        .is_ok_and(|host| host == peer.to_canonical())
}

fn earlier(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

async fn read_until<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
    deadline: Option<Instant>,
) -> io::Result<usize> {
    if let Some(deadline) = deadline {
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        timeout_at(deadline, reader.read(buffer))
            .await
            .map_err(|_| timed_out())?
    } else {
        reader.read(buffer).await
    }
}

pub(crate) fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "inbound deadline expired")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn reservations_cover_retained_input_copies() {
        // An operation frame and decoded request coexist with a data frame,
        // decoded data, and an exact-capacity payload accumulator. Metadata
        // records hashes rather than duplicating the collected payload.
        let peak = 2 * OPERATION_BYTES + DATA_FRAME_BYTES + 2 * DATA_BYTES;
        assert!(peak < COMMAND_RESERVATION_BYTES);
        assert_eq!(COMMAND_BUDGET_BYTES, 64 * 1024 * 1024);
    }

    #[tokio::test]
    async fn admission_releases_on_completion_error_or_abort() {
        let admission = Admission::new(2);
        let first = admission.try_acquire().unwrap();
        let second = admission.try_acquire().unwrap();
        assert!(admission.try_acquire().is_none());
        drop(first);
        let replacement = admission.try_acquire().unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            let _permit = replacement;
            std::future::pending::<()>().await;
        });
        tasks.shutdown().await;
        drop(second);
        assert!(admission.try_acquire().is_some());
        assert!(admission.try_acquire().is_some());
        let control = Admission::new(CONTROL_CONNECTIONS);
        let command = Admission::new(COMMAND_CONNECTIONS);
        let _command_permits: Vec<_> = (0..COMMAND_CONNECTIONS)
            .map(|_| command.try_acquire().unwrap())
            .collect();
        assert!(command.try_acquire().is_none());
        assert!(control.try_acquire().is_some());
    }

    #[tokio::test]
    async fn every_role_rejects_excess_tasks_then_reuses_released_capacity() {
        for capacity in [
            COMMAND_CONNECTIONS,
            CONTROL_CONNECTIONS,
            EVENT_CONNECTIONS,
            LIVEVIEW_CONNECTIONS,
            CALLBACK_CONNECTIONS,
        ] {
            let admission = Admission::new(capacity);
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..capacity {
                let permit = admission.try_acquire().unwrap();
                tasks.spawn(async move {
                    let _permit = permit;
                    std::future::pending::<io::Result<()>>().await
                });
            }
            assert!(admission.try_acquire().is_none());
            assert_eq!(tasks.len(), capacity);
            // Listener rearming and shutdown use this same JoinSet operation.
            tasks.shutdown().await;
            let permits: Vec<_> = (0..capacity)
                .map(|_| admission.try_acquire().unwrap())
                .collect();
            assert!(admission.try_acquire().is_none());
            drop(permits);
            let permit = admission.try_acquire().unwrap();
            tasks.spawn(async move {
                let _permit = permit;
                Err::<(), _>(io::Error::new(io::ErrorKind::InvalidData, "malformed peer"))
            });
            assert!(tasks.join_next().await.unwrap().unwrap().is_err());
            let permits: Vec<_> = (0..capacity)
                .map(|_| admission.try_acquire().unwrap())
                .collect();
            assert_eq!(permits.len(), capacity);
        }
    }

    #[tokio::test]
    async fn oversized_prefix_is_rejected_without_body_allocation() {
        for maximum in [HANDSHAKE_BYTES, OPERATION_BYTES, DATA_FRAME_BYTES] {
            let bytes = ((maximum + 1) as u32).to_le_bytes();
            let mut input = bytes.as_slice();
            let mut reader = FrameReader::default();
            let error = reader
                .read_frame(&mut input, FrameReadPolicy::established(maximum))
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(reader.body.capacity(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_is_allowed_but_started_frames_have_one_deadline() {
        let (mut writer, mut input) = tokio::io::duplex(32);
        let mut reader = FrameReader::default();
        let policy = FrameReadPolicy::established(HANDSHAKE_BYTES);
        {
            let frame = reader.read_frame(&mut input, policy);
            tokio::pin!(frame);
            tokio::select! {
                result = &mut frame => panic!("idle frame completed: {result:?}"),
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
        }
        writer.write_all(&[8]).await.unwrap();
        {
            let frame = reader.read_frame(&mut input, policy);
            tokio::pin!(frame);
            tokio::select! {
                result = &mut frame => panic!("partial frame completed: {result:?}"),
                _ = tokio::time::sleep(Duration::from_secs(4)) => {}
            }
        }
        writer.write_all(&[0]).await.unwrap();
        let error = reader.read_frame(&mut input, policy).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn handshake_deadline_includes_waiting_for_first_byte() {
        let (_writer, mut input) = tokio::io::duplex(32);
        let mut reader = FrameReader::default();
        let policy = FrameReadPolicy::until(HANDSHAKE_BYTES, Instant::now() + HANDSHAKE_TIMEOUT);
        let started = Instant::now();
        let error = reader.read_frame(&mut input, policy).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), HANDSHAKE_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_prefix_and_body_resume_without_losing_bytes() {
        let frame = [12, 0, 0, 0, 6, 0, 0, 0, 1, 2, 3, 4];
        let (mut writer, mut input) = tokio::io::duplex(32);
        let mut reader = FrameReader::default();
        let policy = FrameReadPolicy::established(HANDSHAKE_BYTES);
        for fragment in [&frame[..2], &frame[2..9]] {
            writer.write_all(fragment).await.unwrap();
            {
                let read = reader.read_frame(&mut input, policy);
                tokio::pin!(read);
                tokio::select! {
                    result = &mut read => panic!("partial frame completed: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                }
            }
        }
        writer.write_all(&frame[9..]).await.unwrap();
        assert_eq!(
            reader.read_frame(&mut input, policy).await.unwrap(),
            Some(frame.to_vec())
        );
        assert_eq!(reader.body.capacity(), 0);
    }

    #[test]
    fn callback_identity_requires_datagram_source() {
        let peer = "127.0.0.1".parse().unwrap();
        assert!(pcss_host_matches_peer("127.0.0.1", peer));
        assert!(!pcss_host_matches_peer("127.0.0.2", peer));
        assert!(!pcss_host_matches_peer("localhost", peer));
    }
}
