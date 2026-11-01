use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsMessage {
    pub topic: String,
    pub payload: String,
}

#[derive(Clone)]
pub struct CommsHub {
    ws_tx: broadcast::Sender<String>,
}

impl Default for CommsHub {
    fn default() -> Self {
        Self::new()
    }
}

impl CommsHub {
    pub fn new() -> Self {
        let (ws_tx, _) = broadcast::channel(512);
        Self { ws_tx }
    }

    pub fn subscribe_ws(&self) -> broadcast::Receiver<String> {
        self.ws_tx.subscribe()
    }

    pub fn publish_topic(&self, topic: &str, payload: impl Into<String>) {
        let msg = json!({
            "topic": topic,
            "payload": payload.into(),
        });
        if let Ok(serialized) = serde_json::to_string(&msg) {
            let _ = self.ws_tx.send(serialized);
        }
    }

    pub fn publish_json(&self, topic: &str, value: &Value) {
        if let Ok(payload) = serde_json::to_string(value) {
            self.publish_topic(topic, payload);
        }
    }

    pub fn initial_ws_snapshots(&self, cameras: &[String]) -> Vec<String> {
        let mut msgs = Vec::new();
        let mut activity = serde_json::Map::new();
        for cam in cameras {
            activity.insert(
                cam.clone(),
                json!({
                    "enabled": true,
                    "motion": false,
                    "objects": [],
                    "count": 0,
                    "max_Area": 0,
                    "config": {
                        "record": true,
                        "detect": true,
                        "enabled": true,
                        "snapshots": true,
                        "audio": false,
                        "audio_transcription": false,
                        "notifications": false,
                        "notifications_suspended": 0,
                        "autotracking": false,
                        "alerts": true,
                        "detections": true,
                        "object_descriptions": false,
                        "review_descriptions": false
                    }
                }),
            );
            for (sub, val) in [
                ("enabled/state", "ON"),
                ("detect/state", "ON"),
                ("recordings/state", "ON"),
                ("snapshots/state", "ON"),
                ("audio/state", "OFF"),
                ("motion", "OFF"),
            ] {
                msgs.push(
                    json!({
                        "topic": format!("{cam}/{sub}"),
                        "payload": val
                    })
                    .to_string(),
                );
            }
        }
        msgs.push(
            json!({
                "topic": "camera_activity",
                "payload": Value::Object(activity).to_string()
            })
            .to_string(),
        );
        msgs
    }
}

pub type SharedCommsHub = Arc<CommsHub>;
