use ninjanvr_vision::BoundingBox;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedObject {
    pub id: String,
    pub label: String,
    pub score: f32,
    pub top_score: f32,
    pub bbox: BoundingBox,
    pub stationary: bool,
    pub motionless_count: u32,
    pub zones: Vec<String>,
}
