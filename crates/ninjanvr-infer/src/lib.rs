use ninjanvr_vision::BoundingBox;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionResult {
    pub label: String,
    pub score: f32,
    pub bbox: BoundingBox,
}
