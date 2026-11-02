use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ninjanvr_config::MqttConfig;
use rumqttc::{AsyncClient, Event as MqttEvent, LastWill, MqttOptions, Packet, QoS};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsMessage {
    pub topic: String,
    pub payload: String,
}

fn default_camera_activity_entry() -> Value {
    json!({
        "enabled": true,
        "online": true,
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
    })
}

#[derive(Clone)]
pub struct CommsHub {
    ws_tx: broadcast::Sender<String>,
    mqtt_tx: mpsc::UnboundedSender<(String, Vec<u8>, bool)>,
    camera_states: Arc<RwLock<BTreeMap<String, Value>>>,
}

impl Default for CommsHub {
    fn default() -> Self {
        Self::new()
    }
}

impl CommsHub {
    pub fn new() -> Self {
        let (ws_tx, _) = broadcast::channel(512);
        let (mqtt_tx, _rx) = mpsc::unbounded_channel();
        Self {
            ws_tx,
            mqtt_tx,
            camera_states: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    pub fn with_mqtt(mqtt_cfg: &MqttConfig) -> Self {
        let (ws_tx, _) = broadcast::channel(512);
        let (mqtt_tx, mqtt_rx) = mpsc::unbounded_channel();
        let hub = Self {
            ws_tx,
            mqtt_tx,
            camera_states: Arc::new(RwLock::new(BTreeMap::new())),
        };
        if mqtt_cfg.enabled {
            let cfg_clone = mqtt_cfg.clone();
            let hub_clone = hub.clone();
            tokio::spawn(async move {
                run_mqtt_loop(cfg_clone, mqtt_rx, hub_clone).await;
            });
        }
        hub
    }

    pub fn subscribe_ws(&self) -> broadcast::Receiver<String> {
        self.ws_tx.subscribe()
    }

    pub fn publish_ws_only(&self, topic: &str, payload: impl Into<String>) {
        let msg = json!({
            "topic": topic,
            "payload": payload.into(),
        });
        if let Ok(serialized) = serde_json::to_string(&msg) {
            let _ = self.ws_tx.send(serialized);
        }
    }

    pub fn publish_topic(&self, topic: &str, payload: impl Into<String>) {
        let payload_str = payload.into();
        self.publish_ws_only(topic, payload_str.clone());
        let _ = self
            .mqtt_tx
            .send((topic.to_string(), payload_str.into_bytes(), false));
    }

    pub fn publish_json(&self, topic: &str, value: &Value) {
        if let Ok(payload) = serde_json::to_string(value) {
            self.publish_topic(topic, payload);
        }
    }

    pub fn publish_mqtt_bytes(&self, topic: &str, bytes: Vec<u8>, retain: bool) {
        let _ = self.mqtt_tx.send((topic.to_string(), bytes, retain));
    }

    /// Updates per-camera online/offline connectivity state and publishes `<camera>/available`
    /// and `<camera>/status` (`"online"` / `"offline"`) over MQTT and WebSocket when changed.
    /// Returns `true` if the state transitioned.
    pub fn set_camera_availability(&self, camera: &str, online: bool) -> bool {
        let (transitioned, snapshot_str) = {
            let mut map = self
                .camera_states
                .write()
                .expect("camera_states lock poisoned");
            let entry = map
                .entry(camera.to_string())
                .or_insert_with(default_camera_activity_entry);
            let prev = entry.get("online").and_then(|v| v.as_bool());
            let changed = prev != Some(online);
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("online".to_string(), json!(online));
            }
            let s = serde_json::to_string(&*map).unwrap_or_else(|_| "{}".to_string());
            (changed, s)
        };
        if transitioned {
            let status_str = if online { "online" } else { "offline" };
            self.publish_topic(&format!("{camera}/available"), status_str);
            self.publish_topic(&format!("{camera}/status"), status_str);
            self.publish_ws_only("camera_activity", snapshot_str);
        }
        transitioned
    }

    /// Updates live motion/object activity for a camera and broadcasts `camera_activity` to `/ws`.
    pub fn update_camera_activity(
        &self,
        camera: &str,
        motion: bool,
        objects: Vec<Value>,
        max_area: u32,
    ) {
        let snapshot_str = {
            let mut map = self
                .camera_states
                .write()
                .expect("camera_states lock poisoned");
            let entry = map
                .entry(camera.to_string())
                .or_insert_with(default_camera_activity_entry);
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("motion".to_string(), json!(motion));
                obj.insert("count".to_string(), json!(objects.len()));
                obj.insert("max_Area".to_string(), json!(max_area));
                obj.insert("objects".to_string(), Value::Array(objects));
            }
            serde_json::to_string(&*map).unwrap_or_else(|_| "{}".to_string())
        };
        self.publish_ws_only("camera_activity", snapshot_str);
    }

    /// Applies a `<camera>/<feature>/set` command from WebSocket or MQTT, updates live state,
    /// and broadcasts `<camera>/<feature>/state` + `camera_activity`.
    pub fn apply_set_command(&self, topic: &str, payload: &str) {
        let Some(base) = topic.strip_suffix("/set") else {
            return;
        };
        let parts: Vec<&str> = base.split('/').collect();
        if parts.len() == 2 {
            let cam = parts[0];
            let feature = parts[1];
            let is_on = payload.eq_ignore_ascii_case("ON") || payload.eq_ignore_ascii_case("true");
            let cfg_key = match feature {
                "recordings" => Some("record"),
                "detect" => Some("detect"),
                "enabled" => Some("enabled"),
                "snapshots" => Some("snapshots"),
                "audio" => Some("audio"),
                "review_alerts" => Some("alerts"),
                "review_detections" => Some("detections"),
                _ => None,
            };
            let snapshot_str = {
                let mut map = self
                    .camera_states
                    .write()
                    .expect("camera_states lock poisoned");
                let entry = map
                    .entry(cam.to_string())
                    .or_insert_with(default_camera_activity_entry);
                if let Some(key) = cfg_key
                    && let Some(cfg_obj) = entry.get_mut("config").and_then(|c| c.as_object_mut())
                {
                    cfg_obj.insert(key.to_string(), json!(is_on));
                }
                if feature == "enabled"
                    && let Some(obj) = entry.as_object_mut()
                {
                    obj.insert("enabled".to_string(), json!(is_on));
                }
                serde_json::to_string(&*map).unwrap_or_else(|_| "{}".to_string())
            };
            let state_topic = format!("{base}/state");
            self.publish_topic(&state_topic, if is_on { "ON" } else { "OFF" });
            self.publish_ws_only("camera_activity", snapshot_str);
        }
    }

    pub fn initial_ws_snapshots(&self, cameras: &[String]) -> Vec<String> {
        let mut msgs = Vec::new();
        let activity_str = {
            let mut map = self
                .camera_states
                .write()
                .expect("camera_states lock poisoned");
            for cam in cameras {
                map.entry(cam.clone())
                    .or_insert_with(default_camera_activity_entry);
            }
            for (cam, entry) in map.iter() {
                let cfg = &entry["config"];
                let motion_on = entry["motion"].as_bool().unwrap_or(false);
                for (sub, key) in [
                    ("enabled/state", "enabled"),
                    ("detect/state", "detect"),
                    ("recordings/state", "record"),
                    ("snapshots/state", "snapshots"),
                    ("audio/state", "audio"),
                ] {
                    let on = cfg[key].as_bool().unwrap_or(sub != "audio/state");
                    msgs.push(
                        json!({
                            "topic": format!("{cam}/{sub}"),
                            "payload": if on { "ON" } else { "OFF" }
                        })
                        .to_string(),
                    );
                }
                msgs.push(
                    json!({
                        "topic": format!("{cam}/motion"),
                        "payload": if motion_on { "ON" } else { "OFF" }
                    })
                    .to_string(),
                );
            }
            serde_json::to_string(&*map).unwrap_or_else(|_| "{}".to_string())
        };
        msgs.push(
            json!({
                "topic": "camera_activity",
                "payload": activity_str
            })
            .to_string(),
        );
        msgs
    }
}

async fn run_mqtt_loop(
    cfg: MqttConfig,
    mut rx: mpsc::UnboundedReceiver<(String, Vec<u8>, bool)>,
    hub: CommsHub,
) {
    let prefix = cfg.topic_prefix.trim_end_matches('/').to_string();
    loop {
        let mut mqtt_opts = MqttOptions::new(&cfg.client_id, &cfg.host, cfg.port);
        mqtt_opts.set_keep_alive(Duration::from_secs(30));
        mqtt_opts.set_last_will(LastWill::new(
            format!("{prefix}/available"),
            "offline",
            QoS::AtMostOnce,
            true,
        ));
        if let (Some(u), Some(p)) = (&cfg.user, &cfg.password) {
            mqtt_opts.set_credentials(u, p);
        }

        let (client, mut eventloop) = AsyncClient::new(mqtt_opts, 256);

        loop {
            tokio::select! {
                Some((sub_topic, payload, retain)) = rx.recv() => {
                    let full_topic = format!("{prefix}/{}", sub_topic.trim_start_matches('/'));
                    let _ = client.publish(full_topic, QoS::AtMostOnce, retain, payload).await;
                }
                ev = eventloop.poll() => {
                    match ev {
                        Ok(MqttEvent::Incoming(Packet::ConnAck(_))) => {
                            info!("Connected to MQTT broker at {}:{}", cfg.host, cfg.port);
                            let _ = client
                                .publish(
                                    format!("{prefix}/available"),
                                    QoS::AtMostOnce,
                                    true,
                                    "online",
                                )
                                .await;
                            let _ = client
                                .subscribe(format!("{prefix}/+/+/set"), QoS::AtMostOnce)
                                .await;
                        }
                        Ok(MqttEvent::Incoming(Packet::Publish(pub_pkt))) => {
                            if let Some(rel) = pub_pkt.topic.strip_prefix(&format!("{prefix}/"))
                                && rel.ends_with("/set")
                            {
                                let payload_str = String::from_utf8_lossy(&pub_pkt.payload).to_string();
                                debug!("MQTT command on {}: {payload_str}", pub_pkt.topic);
                                hub.apply_set_command(rel, &payload_str);
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            debug!("MQTT broker unreachable ({err}); retrying in 30s");
                            break;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

pub type SharedCommsHub = Arc<CommsHub>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn comms_hub_broadcasts_topics_and_updates_camera_activity() {
        let hub = CommsHub::new();
        let mut rx = hub.subscribe_ws();
        hub.publish_topic("front_door/motion", "ON");
        let received = rx.recv().await.unwrap();
        assert!(received.contains("front_door/motion"));
        assert!(received.contains("ON"));

        hub.update_camera_activity(
            "front_door",
            true,
            vec![json!({"id": "1", "label": "person"})],
            1200,
        );
        let act_msg = rx.recv().await.unwrap();
        assert!(act_msg.contains("camera_activity"));
        assert!(act_msg.contains("1200"));

        let snaps = hub.initial_ws_snapshots(&["front_door".to_string()]);
        assert!(!snaps.is_empty());
        assert!(snaps.last().unwrap().contains("1200"));

        // Verify camera availability state transitions publish online/offline topics
        assert!(hub.set_camera_availability("front_door", false));
        let avail_msg = rx.recv().await.unwrap();
        assert!(avail_msg.contains("front_door/available"));
        assert!(avail_msg.contains("offline"));
        let status_msg = rx.recv().await.unwrap();
        assert!(status_msg.contains("front_door/status"));
        assert!(status_msg.contains("offline"));
        let _ = rx.recv().await.unwrap(); // camera_activity

        // Duplicate state does not re-trigger transition
        assert!(!hub.set_camera_availability("front_door", false));
        assert!(hub.set_camera_availability("front_door", true));
    }
}
