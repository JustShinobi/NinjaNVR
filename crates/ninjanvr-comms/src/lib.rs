use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        "audio_dbfs": -90,
        "audio_active": false,
        "objects": [],
        "states": {},
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

    /// Publishes `<camera>/audio/dbfs` and `<camera>/audio/active` (`"ON"` / `"OFF"`) over MQTT and WebSocket,
    /// and updates `audio_dbfs` and `audio_active` inside the `camera_activity` snapshot.
    pub fn publish_audio_activity(&self, camera: &str, dbfs: i64, active: bool) {
        let snapshot_str = {
            let mut map = self
                .camera_states
                .write()
                .expect("camera_states lock poisoned");
            let entry = map
                .entry(camera.to_string())
                .or_insert_with(default_camera_activity_entry);
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("audio_dbfs".to_string(), json!(dbfs));
                obj.insert("audio_active".to_string(), json!(active));
            }
            serde_json::to_string(&*map).unwrap_or_else(|_| "{}".to_string())
        };
        self.publish_topic(&format!("{camera}/audio/dbfs"), dbfs.to_string());
        self.publish_topic(
            &format!("{camera}/audio/active"),
            if active { "ON" } else { "OFF" },
        );
        self.publish_ws_only("camera_activity", snapshot_str);
    }

    /// Publishes `<camera>/state/<roi_name>` over MQTT (`<prefix>/<camera>/state/<roi_name>`) and `/ws`,
    /// and updates `camera_activity.<camera>.states.<roi_name>` in the real-time activity snapshot.
    pub fn publish_roi_state(&self, camera: &str, roi_name: &str, state: &str) {
        let snapshot_str = {
            let mut map = self
                .camera_states
                .write()
                .expect("camera_states lock poisoned");
            let entry = map
                .entry(camera.to_string())
                .or_insert_with(default_camera_activity_entry);
            if let Some(obj) = entry.as_object_mut() {
                let states_entry = obj.entry("states".to_string()).or_insert_with(|| json!({}));
                if !states_entry.is_object() {
                    *states_entry = json!({});
                }
                if let Some(states_obj) = states_entry.as_object_mut() {
                    states_obj.insert(roi_name.to_string(), json!(state));
                }
            }
            serde_json::to_string(&*map).unwrap_or_else(|_| "{}".to_string())
        };
        self.publish_topic(&format!("{camera}/state/{roi_name}"), state);
        self.publish_ws_only("camera_activity", snapshot_str);
    }

    /// Returns a cloned map of the current ROI states (`roi_name -> state`) for `camera`.
    pub fn get_camera_roi_states(&self, camera: &str) -> BTreeMap<String, String> {
        let map = self
            .camera_states
            .read()
            .expect("camera_states lock poisoned");
        let Some(entry) = map.get(camera) else {
            return BTreeMap::new();
        };
        let Some(states_obj) = entry.get("states").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        let mut out = BTreeMap::new();
        for (k, v) in states_obj {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
        out
    }

    /// Returns a cloned JSON snapshot of all current camera activity states.
    pub fn get_camera_activity_snapshot(&self) -> Value {
        let map = self
            .camera_states
            .read()
            .expect("camera_states lock poisoned");
        serde_json::to_value(&*map).unwrap_or_else(|_| json!({}))
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
                if !cam_cfg.state_rois.is_empty() {
                    let states_entry = obj.entry("states".to_string()).or_insert_with(|| json!({}));
                    if let Some(states_obj) = states_entry.as_object_mut() {
                        for (roi_name, roi_cfg) in &cam_cfg.state_rois {
                            let initial_state = roi_cfg
                                .classes
                                .first()
                                .cloned()
                                .unwrap_or_else(|| "closed".to_string());
                            states_obj
                                .entry(roi_name.clone())
                                .or_insert_with(|| json!(initial_state));
                        }
                    }
                }
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
        if let Ok(saved) = std::fs::read_to_string(&path) {
            let trimmed = saved.trim().to_ascii_lowercase();
            if !trimmed.is_empty() {
                let norm = if trimmed == "off" {
                    "disarmed".to_string()
                } else {
                    trimmed
                };
                if let Ok(mut g) = self.active_profile.write() {
                    *g = norm;
                }
            }
        }
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
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
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
                if let Some(states_obj) = entry.get("states").and_then(|v| v.as_object()) {
                    for (roi_name, state_val) in states_obj {
                        if let Some(state_str) = state_val.as_str() {
                            msgs.push(
                                json!({
                                    "topic": format!("{cam}/state/{roi_name}"),
                                    "payload": state_str
                                })
                                .to_string(),
                            );
                        }
                    }
                }
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

const DEFAULT_ACTIVITY_LEDGER_CAPACITY: usize = 300;
const ACTIVITY_LEDGER_WINDOW_SECS: f64 = 300.0;
const DEFAULT_MAX_LOG_LINES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActivitySample {
    pub timestamp: f64,
    pub motion_px: u32,
    pub active_objects: u32,
    pub regions: u32,
    pub dbfs: i64,
}

#[derive(Clone)]
pub struct ActivityLedger {
    rings: Arc<RwLock<HashMap<String, VecDeque<ActivitySample>>>>,
    max_samples: usize,
}

impl Default for ActivityLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityLedger {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_ACTIVITY_LEDGER_CAPACITY)
    }

    pub fn with_capacity(max_samples: usize) -> Self {
        Self {
            rings: Arc::new(RwLock::new(HashMap::new())),
            max_samples: max_samples.max(1),
        }
    }

    pub fn record_sample(
        &self,
        camera: &str,
        timestamp: f64,
        motion_px: u32,
        active_objects: u32,
        regions: u32,
    ) {
        self.record_sample_with_audio(camera, timestamp, motion_px, active_objects, regions, 0);
    }

    pub fn record_sample_with_audio(
        &self,
        camera: &str,
        timestamp: f64,
        motion_px: u32,
        active_objects: u32,
        regions: u32,
        dbfs: i64,
    ) {
        let mut guard = self.rings.write().expect("activity_ledger lock poisoned");
        let q = guard.entry(camera.to_string()).or_default();
        q.push_back(ActivitySample {
            timestamp,
            motion_px,
            active_objects,
            regions,
            dbfs,
        });
        let cutoff = timestamp - ACTIVITY_LEDGER_WINDOW_SECS;
        while let Some(front) = q.front() {
            if front.timestamp < cutoff || q.len() > self.max_samples {
                q.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn sample_count(&self, camera: &str) -> usize {
        let guard = self.rings.read().expect("activity_ledger lock poisoned");
        guard.get(camera).map_or(0, VecDeque::len)
    }

    pub fn samples(&self, camera: &str) -> Vec<ActivitySample> {
        let guard = self.rings.read().expect("activity_ledger lock poisoned");
        guard
            .get(camera)
            .map(|q| q.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn aggregate_range(&self, camera: &str, start_time: f64, end_time: f64) -> (i64, i64, i64) {
        let (m, o, r, _) = self.aggregate_range_with_audio(camera, start_time, end_time);
        (m, o, r)
    }

    pub fn aggregate_range_with_audio(
        &self,
        camera: &str,
        start_time: f64,
        end_time: f64,
    ) -> (i64, i64, i64, i64) {
        let guard = self.rings.read().expect("activity_ledger lock poisoned");
        let Some(q) = guard.get(camera) else {
            return (0, 0, 0, 0);
        };
        let mut motion_sum = 0i64;
        let mut obj_sum = 0i64;
        let mut reg_sum = 0i64;
        let mut peak_dbfs = 0i64;
        for s in q {
            if s.timestamp >= start_time && s.timestamp <= end_time {
                motion_sum += s.motion_px as i64;
                obj_sum += s.active_objects as i64;
                reg_sum += s.regions as i64;
                if s.dbfs != 0 && (peak_dbfs == 0 || s.dbfs > peak_dbfs) {
                    peak_dbfs = s.dbfs;
                }
            }
        }
        (motion_sum, obj_sum, reg_sum, peak_dbfs)
    }
}

fn format_utc_timestamps() -> (String, String, String) {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let nanos = dur.subsec_nanos();
    let millis = nanos / 1_000_000;
    let days = secs.div_euclid(86400);
    let day_secs = secs.rem_euclid(86400);
    let hh = day_secs / 3600;
    let mm = (day_secs % 3600) / 60;
    let ss = day_secs % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    let outer = format!("{year:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{nanos:09}");
    let inner = format!("{year:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}");
    let short_ms = format!("{hh:02}:{mm:02}:{ss:02}.{millis:03}");
    (outer, inner, short_ms)
}

#[derive(Clone)]
pub struct LogBufferHub {
    buffers: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
    stream_tx: broadcast::Sender<(String, String)>,
    max_lines: usize,
}

impl Default for LogBufferHub {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBufferHub {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_LOG_LINES)
    }

    pub fn with_capacity(max_lines: usize) -> Self {
        let (stream_tx, _) = broadcast::channel(256);
        let mut map = HashMap::new();
        map.insert("ninjanvr".to_string(), VecDeque::new());
        map.insert("go2rtc".to_string(), VecDeque::new());
        map.insert("nginx".to_string(), VecDeque::new());
        Self {
            buffers: Arc::new(Mutex::new(map)),
            stream_tx,
            max_lines: max_lines.max(1),
        }
    }

    pub fn push_ninjanvr(&self, severity: &str, section: &str, message: &str) {
        let (outer_ts, inner_ts, _) = format_utc_timestamps();
        let sev_pad = format!("{:<7}", severity.to_ascii_uppercase());
        let sec_pad = format!("{:<30}", section);
        let line = format!("{outer_ts}  [{inner_ts}] {sec_pad} {sev_pad} : {message}");
        self.push_raw("ninjanvr", line);
    }

    pub fn push_go2rtc(&self, level: &str, section: &str, message: &str) {
        let (outer_ts, _, short_ms) = format_utc_timestamps();
        let line = format!("{outer_ts}  {short_ms} {level} [{section}] {message}");
        self.push_raw("go2rtc", line);
    }

    pub fn push_nginx(&self, method: &str, path: &str, status: u16) {
        let (outer_ts, inner_ts, _) = format_utc_timestamps();
        let line = format!(
            "{outer_ts}  127.0.0.1 - - [{inner_ts} +0000] \"{method} {path} HTTP/1.1\" {status} 0 \"-\" \"NinjaNVR\""
        );
        self.push_raw("nginx", line);
    }

    pub fn push_raw(&self, service: &str, line: impl Into<String>) {
        let line = line.into();
        {
            let mut guard = self.buffers.lock().expect("log mutex poisoned");
            let q = guard.entry(service.to_string()).or_default();
            q.push_back(line.clone());
            while q.len() > self.max_lines {
                q.pop_front();
            }
        }
        let _ = self.stream_tx.send((service.to_string(), line));
    }

    pub fn get_logs(&self, service: &str) -> Vec<String> {
        self.query_lines(service, None, None).1
    }

    pub fn query_lines(
        &self,
        service: &str,
        start: Option<i64>,
        end: Option<i64>,
    ) -> (usize, Vec<String>) {
        let guard = self.buffers.lock().expect("log mutex poisoned");
        let Some(q) = guard.get(service) else {
            return (0, Vec::new());
        };
        let total = q.len();
        if total == 0 {
            return (0, Vec::new());
        }
        let start_idx = match start {
            Some(s) if s < 0 => total.saturating_sub((-s) as usize),
            Some(s) => (s as usize).min(total),
            None => 0,
        };
        let end_idx = match end {
            Some(e) if e >= 0 => (e as usize).min(total),
            _ => total,
        };
        if start_idx >= end_idx {
            return (total, Vec::new());
        }
        let slice = q
            .iter()
            .skip(start_idx)
            .take(end_idx - start_idx)
            .cloned()
            .collect();
        (total, slice)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(String, String)> {
        self.stream_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninjanvr_config::NinjaConfig;
    use std::path::PathBuf;

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

    #[tokio::test]
    async fn init_from_config_and_apply_set_command_update_flags_and_broadcast_ws() {
        let yaml = r#"
cameras:
  porch:
    enabled: true
    detect:
      enabled: true
    record:
      enabled: false
    review:
      alerts:
        enabled: true
      detections:
        enabled: false
    snapshots:
      enabled: false
  garage:
    enabled: false
    detect:
      enabled: false
    record:
      enabled: true
    review:
      alerts:
        enabled: false
    snapshots:
      enabled: true
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let hub = CommsHub::new();
        hub.init_from_config(&cfg);

        // 1. Verify initial CameraRuntimeFlags populated from config
        assert_eq!(
            hub.get_camera_flags("porch"),
            CameraRuntimeFlags {
                enabled: true,
                detect: true,
                record: false,
                alerts: true,
                snapshots: false,
            }
        );
        assert_eq!(
            hub.get_camera_flags("garage"),
            CameraRuntimeFlags {
                enabled: false,
                detect: false,
                record: true,
                alerts: false,
                snapshots: true,
            }
        );
        assert_eq!(
            hub.get_camera_flags("unknown_cam"),
            CameraRuntimeFlags::default()
        );

        let mut rx = hub.subscribe_ws();

        // 2. Test apply_set_command for detect, record, alerts, snapshots, and enabled
        // across both ON/OFF and true/false payloads
        let cases = [
            ("porch/detect/set", "OFF", "porch/detect/state", "OFF"),
            ("porch/record/set", "ON", "porch/record/state", "ON"),
            ("porch/alerts/set", "false", "porch/alerts/state", "OFF"),
            ("porch/snapshots/set", "true", "porch/snapshots/state", "ON"),
            ("porch/enabled/set", "OFF", "porch/enabled/state", "OFF"),
        ];

        for (cmd_topic, cmd_payload, expected_state_topic, expected_state_payload) in cases {
            hub.apply_set_command(cmd_topic, cmd_payload);

            let state_msg: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
            assert_eq!(state_msg.topic, expected_state_topic);
            assert_eq!(state_msg.payload, expected_state_payload);

            let activity_msg: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
            assert_eq!(activity_msg.topic, "camera_activity");
        }

        assert_eq!(
            hub.get_camera_flags("porch"),
            CameraRuntimeFlags {
                enabled: false,
                detect: false,
                record: true,
                alerts: false,
                snapshots: true,
            }
        );

        // Re-enable using true/ON and verify set_camera_flags helper
        hub.apply_set_command("porch/enabled/set", "true");
        let _ = rx.recv().await.unwrap();
        let _ = rx.recv().await.unwrap();
        hub.apply_set_command("porch/detect/set", "ON");
        let _ = rx.recv().await.unwrap();
        let _ = rx.recv().await.unwrap();
        hub.apply_set_command("porch/record/set", "false");
        let _ = rx.recv().await.unwrap();
        let _ = rx.recv().await.unwrap();
        hub.apply_set_command("porch/alerts/set", "ON");
        let _ = rx.recv().await.unwrap();
        let _ = rx.recv().await.unwrap();
        hub.apply_set_command("porch/snapshots/set", "OFF");
        let _ = rx.recv().await.unwrap();
        let _ = rx.recv().await.unwrap();

        assert_eq!(
            hub.get_camera_flags("porch"),
            CameraRuntimeFlags {
                enabled: true,
                detect: true,
                record: false,
                alerts: true,
                snapshots: false,
            }
        );

        let updated_flags = hub.set_camera_flags(
            "porch",
            Some(false),
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        );
        assert_eq!(
            updated_flags,
            CameraRuntimeFlags {
                enabled: false,
                detect: false,
                record: true,
                alerts: false,
                snapshots: true,
            }
        );
    }

    #[tokio::test]
    async fn tamper_and_availability_deduplicate_transitions_and_audio_updates_snapshot() {
        let hub = CommsHub::new();
        let mut rx = hub.subscribe_ws();

        // 1. set_camera_tamper deduplication: OFF -> ON emits once; repeated ON returns false; ON -> OFF emits recovery
        assert!(!hub.set_camera_tamper("alley", false));
        assert!(hub.set_camera_tamper("alley", true));

        let tamper_on: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(tamper_on.topic, "alley/tamper");
        assert_eq!(tamper_on.payload, "ON");
        let act_on: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(act_on.topic, "camera_activity");
        let act_on_val: Value = serde_json::from_str(&act_on.payload).unwrap();
        assert_eq!(act_on_val["alley"]["tamper"], true);

        // Repeated true is deduplicated
        assert!(!hub.set_camera_tamper("alley", true));
        assert!(rx.try_recv().is_err());

        // Recovery transition ON -> OFF
        assert!(hub.set_camera_tamper("alley", false));
        let tamper_off: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(tamper_off.topic, "alley/tamper");
        assert_eq!(tamper_off.payload, "OFF");
        let act_off: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        let act_off_val: Value = serde_json::from_str(&act_off.payload).unwrap();
        assert_eq!(act_off_val["alley"]["tamper"], false);
        assert!(!hub.set_camera_tamper("alley", false));

        // 2. set_camera_availability deduplication
        assert!(!hub.set_camera_availability("alley", true));
        assert!(hub.set_camera_availability("alley", false));
        let avail_off: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(avail_off.topic, "alley/available");
        assert_eq!(avail_off.payload, "offline");
        let status_off: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(status_off.topic, "alley/status");
        assert_eq!(status_off.payload, "offline");
        let _ = rx.recv().await.unwrap();
        assert!(!hub.set_camera_availability("alley", false));
        assert!(rx.try_recv().is_err());

        assert!(hub.set_camera_availability("alley", true));
        let avail_on: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(avail_on.topic, "alley/available");
        assert_eq!(avail_on.payload, "online");
        let status_on: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(status_on.topic, "alley/status");
        assert_eq!(status_on.payload, "online");
        let _ = rx.recv().await.unwrap();
        assert!(!hub.set_camera_availability("alley", true));

        // 3. publish_audio_activity updates audio_dbfs and audio_active in camera_activity snapshot
        hub.publish_audio_activity("alley", -24, true);
        let dbfs_msg: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(dbfs_msg.topic, "alley/audio/dbfs");
        assert_eq!(dbfs_msg.payload, "-24");
        let active_msg: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(active_msg.topic, "alley/audio/active");
        assert_eq!(active_msg.payload, "ON");
        let audio_act: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(audio_act.topic, "camera_activity");
        let audio_snap: Value = serde_json::from_str(&audio_act.payload).unwrap();
        assert_eq!(audio_snap["alley"]["audio_dbfs"], -24);
        assert_eq!(audio_snap["alley"]["audio_active"], true);

        let direct_snap = hub.get_camera_activity_snapshot();
        assert_eq!(direct_snap["alley"]["audio_dbfs"], -24);
        assert_eq!(direct_snap["alley"]["audio_active"], true);

        // 4. Verify publish_json, publish_mqtt_bytes, update_camera_activity_with_night, and with_mqtt
        hub.publish_json("stats", &json!({"uptime": 42}));
        let stats_msg: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(stats_msg.topic, "stats");
        assert!(stats_msg.payload.contains("\"uptime\":42"));

        hub.publish_mqtt_bytes("alley/snapshot", vec![0xFF, 0xD8, 0xFF], true);
        hub.update_camera_activity_with_night("alley", true, vec![], 500, true);
        let night_msg: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(night_msg.topic, "camera_activity");
        let night_snap: Value = serde_json::from_str(&night_msg.payload).unwrap();
        assert_eq!(night_snap["alley"]["night_mode"], true);

        let disabled_mqtt_hub = CommsHub::with_mqtt(&MqttConfig::default());
        assert_eq!(disabled_mqtt_hub.get_active_profile(), "armed");
    }

    #[test]
    fn profile_persistence_writes_and_restores_active_profile_from_disk() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_profile_test_{}", std::process::id()));
        let profile_path = tmp_dir.join(".active_profile");

        let hub1 = CommsHub::new();
        hub1.set_profile_persist_path(profile_path.clone());
        assert_eq!(hub1.set_active_profile("home"), "home");
        assert_eq!(
            std::fs::read_to_string(&profile_path).unwrap().trim(),
            "home"
        );

        // Restoring into a fresh CommsHub instance via set_profile_persist_path
        let hub2 = CommsHub::new();
        hub2.set_profile_persist_path(profile_path.clone());
        assert_eq!(hub2.get_active_profile(), "home");

        // Verify normalization of "off" -> "disarmed" and "" -> "armed"
        assert_eq!(hub2.set_active_profile("OFF"), "disarmed");
        assert_eq!(
            std::fs::read_to_string(&profile_path).unwrap().trim(),
            "disarmed"
        );
        let hub3 = CommsHub::new();
        hub3.set_profile_persist_path(profile_path.clone());
        assert_eq!(hub3.get_active_profile(), "disarmed");
        assert_eq!(hub3.set_active_profile("   "), "armed");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn activity_ledger_evicts_ring_buffer_and_log_buffer_hub_filters_services() {
        // 1. ActivityLedger ring-buffer capacity eviction and window eviction
        let ledger = ActivityLedger::with_capacity(3);
        ledger.record_sample_with_audio("front_door", 1000.0, 10, 1, 1, -45);
        ledger.record_sample_with_audio("front_door", 1001.0, 20, 2, 1, -30);
        ledger.record_sample_with_audio("front_door", 1002.0, 30, 3, 2, -20);
        assert_eq!(ledger.sample_count("front_door"), 3);

        // 4th sample exceeds capacity 3 -> evicts oldest (1000.0)
        ledger.record_sample_with_audio("front_door", 1003.0, 40, 4, 2, -15);
        assert_eq!(ledger.sample_count("front_door"), 3);
        let samples = ledger.samples("front_door");
        assert_eq!(samples[0].timestamp, 1001.0);
        assert_eq!(samples[2].timestamp, 1003.0);

        let (motion_sum, obj_sum, reg_sum, peak_dbfs) =
            ledger.aggregate_range_with_audio("front_door", 1000.0, 1003.0);
        assert_eq!(motion_sum, 20 + 30 + 40);
        assert_eq!(obj_sum, 2 + 3 + 4);
        assert_eq!(reg_sum, 1 + 2 + 2);
        assert_eq!(peak_dbfs, -15);

        // Advancing timestamp by > 300s evicts stale samples older than cutoff
        ledger.record_sample("front_door", 1400.0, 5, 0, 1);
        assert_eq!(ledger.sample_count("front_door"), 1);
        assert_eq!(
            ledger.aggregate_range("front_door", 1390.0, 1410.0),
            (5, 0, 1)
        );

        // 2. LogBufferHub push_ninjanvr, push_go2rtc, push_nginx, capacity eviction, and service filtering
        let log_hub = LogBufferHub::with_capacity(3);
        let mut log_rx = log_hub.subscribe();

        log_hub.push_ninjanvr("info", "ninjanvr.pipeline", "Pipeline started");
        log_hub.push_ninjanvr("warn", "ninjanvr.storage", "High disk watermark");
        log_hub.push_go2rtc("INF", "rtsp", "listen addr=:8554");
        log_hub.push_nginx("GET", "/api/config", 200);

        let (svc1, line1) = log_rx.recv().await.unwrap();
        assert_eq!(svc1, "ninjanvr");
        assert!(line1.contains("Pipeline started"));
        assert!(line1.contains("INFO"));

        let nvr_logs = log_hub.get_logs("ninjanvr");
        assert_eq!(nvr_logs.len(), 2);
        assert!(nvr_logs[0].contains("ninjanvr.pipeline"));
        assert!(nvr_logs[1].contains("ninjanvr.storage"));

        let go2rtc_logs = log_hub.get_logs("go2rtc");
        assert_eq!(go2rtc_logs.len(), 1);
        assert!(go2rtc_logs[0].contains("[rtsp] listen addr=:8554"));

        let nginx_logs = log_hub.get_logs("nginx");
        assert_eq!(nginx_logs.len(), 1);
        assert!(nginx_logs[0].contains("\"GET /api/config HTTP/1.1\" 200"));

        assert!(log_hub.get_logs("unknown_service").is_empty());

        // Test LogBufferHub ring-buffer eviction when exceeding capacity (3)
        log_hub.push_ninjanvr("info", "ninjanvr.3", "msg 3");
        log_hub.push_ninjanvr("info", "ninjanvr.4", "msg 4");
        let (total, tail) = log_hub.query_lines("ninjanvr", Some(-2), None);
        assert_eq!(total, 3);
        assert_eq!(tail.len(), 2);
        assert!(tail[0].contains("msg 3"));
        assert!(tail[1].contains("msg 4"));
    }

    #[tokio::test]
    async fn publish_roi_state_updates_camera_activity_states_and_emits_ws_broadcasts() {
        let yaml = r#"
cameras:
  driveway:
    enabled: true
    state_rois:
      gate:
        crop: [0.1, 0.2, 0.5, 0.8]
        classes: [closed, open]
        interval_sec: 5
        hysteresis: 3
        anomaly_state: open
        anomaly_timeout_sec: 180
      parking_spot:
        crop: [0.5, 0.4, 0.9, 0.9]
        classes: [empty, occupied]
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let hub = CommsHub::new();
        hub.init_from_config(&cfg);

        // Initial defaults seeded from config
        let init_states = hub.get_camera_roi_states("driveway");
        assert_eq!(init_states.get("gate").map(String::as_str), Some("closed"));
        assert_eq!(
            init_states.get("parking_spot").map(String::as_str),
            Some("empty")
        );

        let mut rx = hub.subscribe_ws();
        hub.publish_roi_state("driveway", "gate", "open");

        let topic_msg: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(topic_msg.topic, "driveway/state/gate");
        assert_eq!(topic_msg.payload, "open");

        let act_msg: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(act_msg.topic, "camera_activity");
        let act_val: Value = serde_json::from_str(&act_msg.payload).unwrap();
        assert_eq!(act_val["driveway"]["states"]["gate"], "open");
        assert_eq!(act_val["driveway"]["states"]["parking_spot"], "empty");

        // Subsequent update_camera_activity preserves states map
        hub.update_camera_activity("driveway", true, vec![], 900);
        let next_act: WsMessage = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        let next_val: Value = serde_json::from_str(&next_act.payload).unwrap();
        assert_eq!(next_val["driveway"]["states"]["gate"], "open");

        // initial_ws_snapshots includes <camera>/state/<roi_name>
        let snaps = hub.initial_ws_snapshots(&["driveway".to_string()]);
        assert!(
            snaps
                .iter()
                .any(|s| s.contains("driveway/state/gate") && s.contains("open"))
        );
    }

    #[test]
    fn log_buffer_hub_query_lines_handles_positive_slices_bounds_and_min_capacity() {
        // Capacity 0 clamps to 1
        let min_hub = LogBufferHub::with_capacity(0);
        min_hub.push_raw("ninjanvr", "line-1".to_string());
        min_hub.push_raw("ninjanvr", "line-2".to_string());
        let (tot_min, lines_min) = min_hub.query_lines("ninjanvr", None, None);
        assert_eq!(tot_min, 1);
        assert_eq!(lines_min, vec!["line-2".to_string()]);

        let hub = LogBufferHub::with_capacity(10);
        for i in 0..5 {
            hub.push_raw("ninjanvr", format!("entry-{i}"));
        }

        // Bounded positive slice [1, 4) -> entries 1, 2, 3
        let (total, slice) = hub.query_lines("ninjanvr", Some(1), Some(4));
        assert_eq!(total, 5);
        assert_eq!(slice, vec!["entry-1", "entry-2", "entry-3"]);

        // start >= end returns (total, empty)
        let (total2, empty_slice) = hub.query_lines("ninjanvr", Some(4), Some(2));
        assert_eq!(total2, 5);
        assert!(empty_slice.is_empty());

        // start > total returns (total, empty)
        let (total3, over_slice) = hub.query_lines("ninjanvr", Some(99), None);
        assert_eq!(total3, 5);
        assert!(over_slice.is_empty());

        // Known empty service and unknown service return (0, empty)
        assert_eq!(hub.query_lines("go2rtc", None, None), (0, vec![]));
        assert_eq!(
            hub.query_lines("nonexistent", Some(0), Some(5)),
            (0, vec![])
        );
    }

    #[test]
    fn activity_ledger_handles_missing_camera_disjoint_windows_and_negative_dbfs_peaks() {
        let min_ledger = ActivityLedger::with_capacity(0);
        min_ledger.record_sample_with_audio("cam1", 100.0, 10, 1, 1, -30);
        min_ledger.record_sample_with_audio("cam1", 101.0, 20, 2, 1, -20);
        assert_eq!(min_ledger.sample_count("cam1"), 1);

        let ledger = ActivityLedger::new();
        assert_eq!(ledger.sample_count("unknown_cam"), 0);
        assert!(ledger.samples("unknown_cam").is_empty());
        assert_eq!(
            ledger.aggregate_range_with_audio("unknown_cam", 0.0, 100.0),
            (0, 0, 0, 0)
        );

        ledger.record_sample_with_audio("porch", 1_000.0, 120, 1, 1, -48);
        ledger.record_sample_with_audio("porch", 1_005.0, 200, 2, 2, -14);
        ledger.record_sample_with_audio("porch", 1_008.0, 0, 1, 1, -32);

        // Disjoint time window returns all zeros
        assert_eq!(
            ledger.aggregate_range_with_audio("porch", 500.0, 600.0),
            (0, 0, 0, 0)
        );

        // Overlapping window selects the loudest (least negative: -14 dBFS) peak
        let (motion, objects, regions, peak_dbfs) =
            ledger.aggregate_range_with_audio("porch", 1_000.0, 1_010.0);
        assert_eq!(motion, 320);
        assert_eq!(objects, 4);
        assert_eq!(regions, 4);
        assert_eq!(peak_dbfs, -14);
    }

    #[test]
    fn apply_set_command_ignores_malformed_topics_and_maps_feature_aliases() {
        let yaml = r#"
cameras:
  porch:
    enabled: true
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let hub = CommsHub::new();
        hub.init_from_config(&cfg);

        let before = hub.get_camera_flags("porch");
        assert!(before.enabled);
        assert!(before.detect);
        assert!(before.record);
        assert!(before.alerts);
        assert!(before.snapshots);

        // Non-/set topics and wrong path depth are ignored
        hub.apply_set_command("porch/detect/state", "OFF");
        hub.apply_set_command("a/b/c/set", "OFF");
        assert!(hub.get_camera_flags("porch").detect);

        // Feature aliases: recordings -> record, review_alerts -> alerts, snapshots, enabled, profile/set
        hub.apply_set_command("porch/recordings/set", "OFF");
        hub.apply_set_command("porch/review_alerts/set", "false");
        hub.apply_set_command("porch/snapshots/set", "OFF");
        hub.apply_set_command("porch/enabled/set", "OFF");
        hub.apply_set_command("profile/set", "night_perimeter");

        let after = hub.get_camera_flags("porch");
        assert!(!after.enabled);
        assert!(!after.record);
        assert!(!after.alerts);
        assert!(!after.snapshots);
        assert_eq!(hub.get_active_profile(), "night_perimeter");

        assert!(hub.get_camera_roi_states("unknown_camera").is_empty());
    }
}
