use std::sync::Arc;

/// Represents a decoded YUV420p (I420) frame in memory, shared zero-copy via `Arc<YuvFrame>`.
#[derive(Debug, Clone)]
pub struct YuvFrame {
    pub camera: Arc<str>,
    pub timestamp: f64,
    pub width: u32,
    pub height: u32,
    pub data: Arc<Vec<u8>>,
}

impl YuvFrame {
    pub fn y_plane(&self) -> &[u8] {
        let y_len = (self.width as usize) * (self.height as usize);
        &self.data[..y_len.min(self.data.len())]
    }
}
