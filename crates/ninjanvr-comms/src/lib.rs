use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ninjanvr_config::MqttConfig;
use rumqttc::{
    AsyncClient, Event as MqttEvent, LastWill, MqttOptions, Packet, QoS, TlsConfiguration,
    Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsMessage {
    pub topic: String,
    pub payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraRuntimeFlags {
    pub enabled: bool,
    pub detect: bool,
    pub record: bool,
    pub alerts: bool,
    pub snapshots: bool,
}

impl Default for CameraRuntimeFlags {
    fn default() -> Self {
        Self {
            enabled: true,
            detect: true,
            record: true,
            alerts: true,
            snapshots: true,
        }
    }
}

fn default_camera_activity_entry() -> Value {
    json!({
        "enabled": true,
        "online": true,
        "night_mode": false,
        "tamper": false,
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
    active_profile: Arc<RwLock<String>>,
    profile_persist_path: Arc<RwLock<Option<std::path::PathBuf>>>,
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
            active_profile: Arc::new(RwLock::new("armed".to_string())),
            profile_persist_path: Arc::new(RwLock::new(None)),
        }
    }

    pub fn with_mqtt(mqtt_cfg: &MqttConfig) -> Self {
        let (ws_tx, _) = broadcast::channel(512);
        let (mqtt_tx, mqtt_rx) = mpsc::unbounded_channel();
        let hub = Self {
            ws_tx,
            mqtt_tx,
            camera_states: Arc::new(RwLock::new(BTreeMap::new())),
            active_profile: Arc::new(RwLock::new("armed".to_string())),
            profile_persist_path: Arc::new(RwLock::new(None)),
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

    /// Updates per-camera tamper / blind-feed state and publishes `<camera>/tamper` (`"ON"` / `"OFF"`)
    /// over MQTT and WebSocket when changed. Returns `true` if the state transitioned.
    pub fn set_camera_tamper(&self, camera: &str, tampered: bool) -> bool {
        let (transitioned, snapshot_str) = {
            let mut map = self
                .camera_states
                .write()
                .expect("camera_states lock poisoned");
            let entry = map
                .entry(camera.to_string())
                .or_insert_with(default_camera_activity_entry);
            let prev = entry
                .get("tamper")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let changed = prev != tampered;
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("tamper".to_string(), json!(tampered));
            }
            let s = serde_json::to_string(&*map).unwrap_or_else(|_| "{}".to_string());
            (changed, s)
        };
        if transitioned {
            self.publish_topic(
                &format!("{camera}/tamper"),
                if tampered { "ON" } else { "OFF" },
            );
            self.publish_ws_only("camera_activity", snapshot_str);
        }
        transitioned
    }

    /// Publishes `<camera>/audio/dbfs` and `<camera>/audio/active` (`"ON"` / `"OFF"`) over MQTT and WebSocket.
    pub fn publish_audio_activity(&self, camera: &str, dbfs: i64, active: bool) {
        self.publish_topic(&format!("{camera}/audio/dbfs"), dbfs.to_string());
        self.publish_topic(
            &format!("{camera}/audio/active"),
            if active { "ON" } else { "OFF" },
        );
    }

    /// Returns the currently active operational security profile (`"armed"`, `"home"`, or `"disarmed"`).
    pub fn get_active_profile(&self) -> String {
        self.active_profile
            .read()
            .expect("active_profile lock poisoned")
            .clone()
    }

    /// Initializes per-camera runtime flags from `NinjaConfig` so `/api/:camera/state`
    /// and `/ws` snapshots reflect the configured defaults on startup.
    pub fn init_from_config(&self, cfg: &ninjanvr_config::NinjaConfig) {
        let mut map = self
            .camera_states
            .write()
            .expect("camera_states lock poisoned");
        for (cam_name, cam_cfg) in &cfg.cameras {
            let entry = map
                .entry(cam_name.clone())
                .or_insert_with(default_camera_activity_entry);
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("enabled".to_string(), json!(cam_cfg.enabled));
            }
            if let Some(cfg_obj) = entry.get_mut("config").and_then(|c| c.as_object_mut()) {
                cfg_obj.insert("enabled".to_string(), json!(cam_cfg.enabled));
                cfg_obj.insert("detect".to_string(), json!(cam_cfg.detect.enabled));
                cfg_obj.insert("record".to_string(), json!(cam_cfg.record.enabled));
                cfg_obj.insert("alerts".to_string(), json!(cam_cfg.review.alerts.enabled));
                cfg_obj.insert(
                    "detections".to_string(),
                    json!(cam_cfg.review.detections.enabled),
                );
                cfg_obj.insert("snapshots".to_string(), json!(cam_cfg.snapshots.enabled));
                cfg_obj.insert("audio".to_string(), json!(cam_cfg.audio.enabled));
            }
        }
    }

    /// Returns the live runtime toggle flags (`enabled`, `detect`, `record`, `alerts`, `snapshots`) for `camera`.
    pub fn get_camera_flags(&self, camera: &str) -> CameraRuntimeFlags {
        let map = self
            .camera_states
            .read()
            .expect("camera_states lock poisoned");
        let Some(entry) = map.get(camera) else {
            return CameraRuntimeFlags::default();
        };
        let cfg = &entry["config"];
        CameraRuntimeFlags {
            enabled: cfg["enabled"].as_bool().unwrap_or(true),
            detect: cfg["detect"].as_bool().unwrap_or(true),
            record: cfg["record"].as_bool().unwrap_or(true),
            alerts: cfg["alerts"].as_bool().unwrap_or(true),
            snapshots: cfg["snapshots"].as_bool().unwrap_or(true),
        }
    }

    /// Updates one or more runtime toggle flags for `camera` and broadcasts the updated state over `/ws` and MQTT.
    pub fn set_camera_flags(
        &self,
        camera: &str,
        enabled: Option<bool>,
        detect: Option<bool>,
        record: Option<bool>,
        alerts: Option<bool>,
        snapshots: Option<bool>,
    ) -> CameraRuntimeFlags {
        if let Some(v) = enabled {
            self.apply_set_command(
                &format!("{camera}/enabled/set"),
                if v { "ON" } else { "OFF" },
            );
        }
        if let Some(v) = detect {
            self.apply_set_command(
                &format!("{camera}/detect/set"),
                if v { "ON" } else { "OFF" },
            );
        }
        if let Some(v) = record {
            self.apply_set_command(
                &format!("{camera}/recordings/set"),
                if v { "ON" } else { "OFF" },
            );
        }
        if let Some(v) = alerts {
            self.apply_set_command(
                &format!("{camera}/alerts/set"),
                if v { "ON" } else { "OFF" },
            );
        }
        if let Some(v) = snapshots {
            self.apply_set_command(
                &format!("{camera}/snapshots/set"),
                if v { "ON" } else { "OFF" },
            );
        }
        self.get_camera_flags(camera)
    }

    pub fn set_profile_persist_path(&self, path: std::path::PathBuf) {
        if let Ok(mut g) = self.profile_persist_path.write() {
            *g = Some(path);
        }
    }

    /// Sets the active operational security profile (`"armed"`, `"home"`, `"disarmed"`, or custom profile)
    /// and broadcasts `profile/state` over MQTT and WebSocket.
    pub fn set_active_profile(&self, profile: &str) -> String {
        let trimmed = profile.trim().to_ascii_lowercase();
        let norm = match trimmed.as_str() {
            "" => "armed".to_string(),
            "off" => "disarmed".to_string(),
            other => other.to_string(),
        };
        {
            let mut g = self
                .active_profile
                .write()
                .expect("active_profile lock poisoned");
            *g = norm.clone();
        }
        if let Ok(guard) = self.profile_persist_path.read()
            && let Some(ref p) = *guard
        {
            let _ = std::fs::write(p, &norm);
        }
        self.publish_topic("profile/state", norm.clone());
        norm
    }

    /// Updates live motion/object activity for a camera and broadcasts `camera_activity` to `/ws`.
    pub fn update_camera_activity(
        &self,
        camera: &str,
        motion: bool,
        objects: Vec<Value>,
        max_area: u32,
    ) {
        self.update_camera_activity_with_night(camera, motion, objects, max_area, false);
    }

    pub fn update_camera_activity_with_night(
        &self,
        camera: &str,
        motion: bool,
        objects: Vec<Value>,
        max_area: u32,
        night_mode: bool,
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
                obj.insert("night_mode".to_string(), json!(night_mode));
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
        if base == "profile" {
            self.set_active_profile(payload);
            return;
        }
        let parts: Vec<&str> = base.split('/').collect();
        if parts.len() == 2 {
            let cam = parts[0];
            let feature = parts[1];
            let is_on = payload.eq_ignore_ascii_case("ON") || payload.eq_ignore_ascii_case("true");
            let cfg_key = match feature {
                "recordings" | "record" => Some("record"),
                "detect" => Some("detect"),
                "enabled" => Some("enabled"),
                "snapshots" => Some("snapshots"),
                "audio" => Some("audio"),
                "alerts" | "review_alerts" => Some("alerts"),
                "detections" | "review_detections" => Some("detections"),
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
                    ("alerts/state", "alerts"),
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
                "topic": "profile/state",
                "payload": self.get_active_profile()
            })
            .to_string(),
        );
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

/// Builds `MqttOptions` from `MqttConfig`, automatically configuring MQTTS (`Transport::Tls`)
/// with either custom `tls_ca_certs` or system Root CAs (`/etc/ssl/certs/ca-certificates.crt`)
/// and optional client mTLS (`tls_client_cert` / `tls_client_key`).
pub fn build_mqtt_options(cfg: &MqttConfig) -> MqttOptions {
    let prefix = cfg.topic_prefix.trim_end_matches('/').to_string();
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
    if cfg.should_use_tls() {
        let ca = cfg
            .tls_ca_certs
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .or_else(|| std::fs::read("/etc/ssl/certs/ca-certificates.crt").ok())
            .or_else(|| std::fs::read("/etc/pki/tls/certs/ca-bundle.crt").ok())
            .unwrap_or_default();
        let client_auth = match (&cfg.tls_client_cert, &cfg.tls_client_key) {
            (Some(cert_path), Some(key_path)) => {
                if let (Ok(cert_bytes), Ok(key_bytes)) =
                    (std::fs::read(cert_path), std::fs::read(key_path))
                {
                    Some((cert_bytes, key_bytes))
                } else {
                    None
                }
            }
            _ => None,
        };
        mqtt_opts.set_transport(Transport::Tls(TlsConfiguration::Simple {
            ca,
            alpn: None,
            client_auth,
        }));
    }
    mqtt_opts
}

async fn run_mqtt_loop(
    cfg: MqttConfig,
    mut rx: mpsc::UnboundedReceiver<(String, Vec<u8>, bool)>,
    hub: CommsHub,
) {
    let prefix = cfg.topic_prefix.trim_end_matches('/').to_string();
    let mut backoff_secs = 2u64;
    loop {
        let mqtt_opts = build_mqtt_options(&cfg);
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
                            backoff_secs = 2;
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
                            let _ = client
                                .subscribe(format!("{prefix}/profile/set"), QoS::AtMostOnce)
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
                            debug!("MQTT broker unreachable ({err}); retrying in {backoff_secs}s");
                            break;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(60);
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
        assert!(snaps.iter().any(|s| s.contains("profile/state")));
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

        // Verify operational security profile transitions via MQTT command
        assert_eq!(hub.get_active_profile(), "armed");
        hub.apply_set_command("profile/set", "home");
        assert_eq!(hub.get_active_profile(), "home");
    }

    #[test]
    fn build_mqtt_options_enables_tls_on_8883_and_custom_ca() {
        let mut cfg = MqttConfig::default();
        assert!(!cfg.should_use_tls());
        let opts_plain = build_mqtt_options(&cfg);
        assert!(matches!(opts_plain.transport(), Transport::Tcp));

        cfg.port = 8883;
        assert!(cfg.should_use_tls());
        let opts_tls = build_mqtt_options(&cfg);
        assert!(matches!(opts_tls.transport(), Transport::Tls(_)));
    }
}
