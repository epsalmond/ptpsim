//! Live-view frame sources. The engine pulls frames independently of the
//! command channel, so control operations never stall the stream.

/// A source of JPEG frames for the live-view socket.
pub trait FrameSource: Send {
    fn next_frame(&mut self) -> Option<Vec<u8>>;
}

/// Repeats a single JPEG forever — the simplest deterministic source for tests.
pub struct StaticFrameSource {
    jpeg: Vec<u8>,
}

impl StaticFrameSource {
    pub fn new(jpeg: Vec<u8>) -> Self {
        StaticFrameSource { jpeg }
    }
}

impl FrameSource for StaticFrameSource {
    fn next_frame(&mut self) -> Option<Vec<u8>> {
        Some(self.jpeg.clone())
    }
}

/// Cycles through a fixed set of frames (e.g. a directory of MJPEG frames).
pub struct LoopingFrameSource {
    frames: Vec<Vec<u8>>,
    idx: usize,
}

impl LoopingFrameSource {
    pub fn new(frames: Vec<Vec<u8>>) -> Self {
        LoopingFrameSource { frames, idx: 0 }
    }
}

impl FrameSource for LoopingFrameSource {
    fn next_frame(&mut self) -> Option<Vec<u8>> {
        if self.frames.is_empty() {
            return None;
        }
        let f = self.frames[self.idx % self.frames.len()].clone();
        self.idx += 1;
        Some(f)
    }
}
