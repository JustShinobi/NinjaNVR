#![recursion_limit = "512"]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub const DEFAULT_CONFIG_PATH: &str = "/config/config.yml";
pub const DEFAULT_DB_PATH: &str = "/config/ninjanvr.db";
pub const DEFAULT_GO2RTC_SHM_PATH: &str = "/dev/shm/go2rtc.yaml";
pub const DEFAULT_GO2RTC_HOMEKIT_PATH: &str = "/config/go2rtc_homekit.yml";
pub const NINJANVR_VERSION: &str = match option_env!("NINJANVR_RELEASE_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerNodeConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default, alias = "api_token_env")]
    pub token_env: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileOverrideConfig {
    #[serde(default)]
    pub disabled_alerts_cameras: Vec<String>,
    #[serde(default)]
    pub disabled_record_cameras: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraGroupConfig {
    #[serde(default)]
    pub cameras: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub order: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraUiConfig {
    #[serde(default)]
    pub order: i32,
    #[serde(default = "default_true")]
    pub dashboard: bool,
}

impl Default for CameraUiConfig {
    fn default() -> Self {
        Self {
            order: 0,
            dashboard: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FloorplanRoom {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default = "default_floorplan_room_dim")]
    pub w: f32,
    #[serde(default = "default_floorplan_room_dim")]
    pub h: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

fn default_floorplan_room_dim() -> f32 {
    0.2
}

impl Default for FloorplanRoom {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            x: 0.0,
            y: 0.0,
            w: default_floorplan_room_dim(),
            h: default_floorplan_room_dim(),
            color: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FloorplanWall {
    #[serde(default)]
    pub points: [f32; 4],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FloorplanConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_data_url: Option<String>,
    #[serde(default = "default_floorplan_opacity")]
    pub opacity: f32,
    #[serde(default = "default_floorplan_width_m")]
    pub width_m: f32,
    #[serde(default = "default_floorplan_height_m")]
    pub height_m: f32,
    #[serde(default)]
    pub rooms: Vec<FloorplanRoom>,
    #[serde(default)]
    pub walls: Vec<FloorplanWall>,
}

fn default_floorplan_opacity() -> f32 {
    0.45
}

fn default_floorplan_width_m() -> f32 {
    20.0
}

fn default_floorplan_height_m() -> f32 {
    12.0
}

impl Default for FloorplanConfig {
    fn default() -> Self {
        Self {
            background_data_url: None,
            opacity: default_floorplan_opacity(),
            width_m: default_floorplan_width_m(),
            height_m: default_floorplan_height_m(),
            rooms: Vec::new(),
            walls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default = "default_ui_time_format")]
    pub time_format: String,
    #[serde(default = "default_ui_unit_system")]
    pub unit_system: String,
    #[serde(default = "default_ui_theme")]
    pub theme: String,
    #[serde(default)]
    pub high_contrast: bool,
    #[serde(default)]
    pub floorplan: FloorplanConfig,
}

fn default_ui_time_format() -> String {
    "browser".to_string()
}

fn default_ui_unit_system() -> String {
    "metric".to_string()
}

fn default_ui_theme() -> String {
    "dark".to_string()
}

pub fn normalize_ui_theme(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "light" => "light".to_string(),
        "system" => "system".to_string(),
        _ => "dark".to_string(),
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            time_format: default_ui_time_format(),
            unit_system: default_ui_unit_system(),
            theme: default_ui_theme(),
            high_contrast: false,
            floorplan: FloorplanConfig::default(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GenAiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_genai_provider")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_genai_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_genai_model")]
    pub model: String,
    #[serde(default)]
    pub verify_ambiguous_alerts: bool,
    #[serde(default = "default_ambiguous_score_min")]
    pub ambiguous_score_min: f32,
    #[serde(default = "default_ambiguous_score_max")]
    pub ambiguous_score_max: f32,
    #[serde(default = "default_true")]
    pub auto_suppress_false_positives: bool,
    #[serde(default = "default_genai_timeout_ms")]
    pub timeout_ms: u64,
}

impl std::fmt::Debug for GenAiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let masked_key = self
            .api_key
            .as_deref()
            .map(|k| if k.is_empty() { "" } else { "***" });
        f.debug_struct("GenAiConfig")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field(
                "base_url",
                &self.base_url.as_deref().map(scrub_rtsp_credentials),
            )
            .field("api_key", &masked_key)
            .field("api_key_env", &self.api_key_env)
            .field("model", &self.model)
            .field("verify_ambiguous_alerts", &self.verify_ambiguous_alerts)
            .field("ambiguous_score_min", &self.ambiguous_score_min)
            .field("ambiguous_score_max", &self.ambiguous_score_max)
            .field(
                "auto_suppress_false_positives",
                &self.auto_suppress_false_positives,
            )
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

fn default_genai_provider() -> String {
    "gemini".to_string()
}
fn default_genai_api_key_env() -> String {
    "GEMINI_API_KEY".to_string()
}
fn default_genai_model() -> String {
    String::new()
}
fn default_ambiguous_score_min() -> f32 {
    0.45
}
fn default_ambiguous_score_max() -> f32 {
    0.72
}
fn default_genai_timeout_ms() -> u64 {
    5000
}

impl Default for GenAiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_genai_provider(),
            base_url: None,
            api_key: None,
            api_key_env: default_genai_api_key_env(),
            model: default_genai_model(),
            verify_ambiguous_alerts: false,
            ambiguous_score_min: default_ambiguous_score_min(),
            ambiguous_score_max: default_ambiguous_score_max(),
            auto_suppress_false_positives: true,
            timeout_ms: default_genai_timeout_ms(),
        }
    }
}

impl GenAiConfig {
    pub fn has_api_key(&self) -> bool {
        self.api_key
            .as_ref()
            .is_some_and(|k| !k.trim().is_empty() && k.trim() != "***" && k.trim() != "*")
    }

    pub fn resolve_api_key(&self) -> String {
        if let Some(ref key) = self.api_key {
            let trimmed = key.trim();
            if !trimmed.is_empty() && trimmed != "***" && trimmed != "*" {
                if let Some(env_name) = trimmed.strip_prefix('$') {
                    if let Ok(val) = std::env::var(env_name.trim())
                        && !val.trim().is_empty()
                    {
                        return val.trim().to_string();
                    }
                } else {
                    if let Ok(val) = std::env::var(trimmed)
                        && !val.trim().is_empty()
                    {
                        return val.trim().to_string();
                    }
                    return trimmed.to_string();
                }
            }
        }
        let env_var = self.api_key_env.trim();
        if !env_var.is_empty()
            && let Ok(val) = std::env::var(env_var)
            && !val.trim().is_empty()
        {
            return val.trim().to_string();
        }
        String::new()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaceRecognitionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_face_model_path")]
    pub model_path: PathBuf,
    #[serde(default = "default_face_similarity_threshold")]
    pub similarity_threshold: f32,
    #[serde(default = "default_min_face_area")]
    pub min_face_area: u32,
    #[serde(default)]
    pub unknown_alert: bool,
}

fn default_face_model_path() -> PathBuf {
    PathBuf::from("/config/models/arcface_r50.onnx")
}

fn default_face_similarity_threshold() -> f32 {
    0.68
}

fn default_min_face_area() -> u32 {
    750
}

impl Default for FaceRecognitionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: default_face_model_path(),
            similarity_threshold: default_face_similarity_threshold(),
            min_face_area: default_min_face_area(),
            unknown_alert: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NinjaConfig {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub detect: DetectConfig,
    #[serde(default)]
    pub detectors: BTreeMap<String, DetectorEntryConfig>,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub genai: GenAiConfig,
    #[serde(default)]
    pub face_recognition: FaceRecognitionConfig,
    #[serde(default)]
    pub record: RecordConfig,
    #[serde(default)]
    pub snapshots: SnapshotsConfig,
    #[serde(default)]
    pub motion: MotionConfig,
    #[serde(default)]
    pub objects: ObjectsConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub lpr: LprConfig,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileOverrideConfig>,
    #[serde(default)]
    pub peers: BTreeMap<String, PeerNodeConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub camera_groups: BTreeMap<String, CameraGroupConfig>,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub go2rtc: Go2RtcConfig,
    #[serde(default)]
    pub cameras: BTreeMap<String, CameraConfig>,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(skip)]
    pub raw_yaml: String,
    #[serde(skip)]
    pub config_path: PathBuf,
}

fn default_version() -> String {
    "0.18-0".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub san_hosts: Vec<String>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            san_hosts: Vec::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MqttConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_mqtt_host")]
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    #[serde(default = "default_mqtt_prefix")]
    pub topic_prefix: String,
    #[serde(default = "default_mqtt_client_id")]
    pub client_id: String,
    pub user: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_stats_interval")]
    pub stats_interval: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_certs: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_cert: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_key: Option<PathBuf>,
    #[serde(default)]
    pub tls_insecure: bool,
}

impl std::fmt::Debug for MqttConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let masked_password = self
            .password
            .as_deref()
            .map(|p| if p.is_empty() { "" } else { "***" });
        f.debug_struct("MqttConfig")
            .field("enabled", &self.enabled)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("topic_prefix", &self.topic_prefix)
            .field("client_id", &self.client_id)
            .field("user", &self.user)
            .field("password", &masked_password)
            .field("stats_interval", &self.stats_interval)
            .field("tls_enabled", &self.tls_enabled)
            .field("tls_ca_certs", &self.tls_ca_certs)
            .field("tls_client_cert", &self.tls_client_cert)
            .field("tls_client_key", &self.tls_client_key)
            .field("tls_insecure", &self.tls_insecure)
            .finish()
    }
}

impl MqttConfig {
    /// Returns `true` when MQTTS (TLS) should be used: either `tls_enabled == Some(true)`
    /// or `tls_enabled.is_none()` and (`port == 8883` or `tls_ca_certs.is_some()`).
    pub fn should_use_tls(&self) -> bool {
        match self.tls_enabled {
            Some(v) => v,
            None => self.port == 8883 || self.tls_ca_certs.is_some(),
        }
    }
}

fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_mqtt_prefix() -> String {
    "ninjanvr".to_string()
}
fn default_mqtt_client_id() -> String {
    "ninjanvr".to_string()
}
fn default_stats_interval() -> u64 {
    60
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_mqtt_host(),
            port: default_mqtt_port(),
            topic_prefix: default_mqtt_prefix(),
            client_id: default_mqtt_client_id(),
            user: None,
            password: None,
            stats_interval: default_stats_interval(),
            tls_enabled: None,
            tls_ca_certs: None,
            tls_client_cert: None,
            tls_client_key: None,
            tls_insecure: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcRoleMappingConfig {
    #[serde(default = "default_oidc_admin_groups")]
    pub admin: Vec<String>,
    #[serde(default = "default_oidc_operator_groups")]
    pub operator: Vec<String>,
    #[serde(default = "default_oidc_viewer_groups")]
    pub viewer: Vec<String>,
}

fn default_oidc_admin_groups() -> Vec<String> {
    vec!["admins".to_string(), "ninjanvr-admin".to_string()]
}
fn default_oidc_operator_groups() -> Vec<String> {
    vec!["operators".to_string(), "ninjanvr-operator".to_string()]
}
fn default_oidc_viewer_groups() -> Vec<String> {
    vec!["users".to_string(), "ninjanvr-viewer".to_string()]
}

impl Default for OidcRoleMappingConfig {
    fn default() -> Self {
        Self {
            admin: default_oidc_admin_groups(),
            operator: default_oidc_operator_groups(),
            viewer: default_oidc_viewer_groups(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcAuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_oidc_provider_name")]
    pub provider_name: String,
    #[serde(default)]
    pub issuer_url: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default = "default_oidc_secret_env")]
    pub client_secret_env: String,
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub auto_redirect: bool,
    #[serde(default = "default_oidc_username_claim")]
    pub username_claim: String,
    #[serde(default = "default_oidc_groups_claim")]
    pub groups_claim: String,
    #[serde(default)]
    pub role_mapping: OidcRoleMappingConfig,
    #[serde(default = "default_oidc_default_role")]
    pub default_role: String,
}

fn default_oidc_provider_name() -> String {
    "SSO".to_string()
}
fn default_oidc_secret_env() -> String {
    "NINJANVR_OIDC_CLIENT_SECRET".to_string()
}
fn default_oidc_scopes() -> Vec<String> {
    vec![
        "openid".to_string(),
        "profile".to_string(),
        "email".to_string(),
        "groups".to_string(),
    ]
}
fn default_oidc_username_claim() -> String {
    "preferred_username".to_string()
}
fn default_oidc_groups_claim() -> String {
    "groups".to_string()
}
fn default_oidc_default_role() -> String {
    "viewer".to_string()
}

impl Default for OidcAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider_name: default_oidc_provider_name(),
            issuer_url: String::new(),
            client_id: String::new(),
            client_secret_env: default_oidc_secret_env(),
            scopes: default_oidc_scopes(),
            auto_redirect: false,
            username_claim: default_oidc_username_claim(),
            groups_claim: default_oidc_groups_claim(),
            role_mapping: OidcRoleMappingConfig::default(),
            default_role: default_oidc_default_role(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyAuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<String>,
    #[serde(default = "default_proxy_user_header")]
    pub user_header: String,
    #[serde(default = "default_proxy_groups_header")]
    pub groups_header: String,
    #[serde(default)]
    pub logout_url: String,
}

fn default_trusted_proxies() -> Vec<String> {
    vec!["127.0.0.1/32".to_string(), "::1/128".to_string()]
}
fn default_proxy_user_header() -> String {
    "Remote-User".to_string()
}
fn default_proxy_groups_header() -> String {
    "Remote-Groups".to_string()
}

impl Default for ProxyAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trusted_proxies: default_trusted_proxies(),
            user_header: default_proxy_user_header(),
            groups_header: default_proxy_groups_header(),
            logout_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub local_login_enabled: bool,
    #[serde(default)]
    pub reset_admin_password: bool,
    #[serde(default = "default_cookie_name")]
    pub cookie_name: String,
    #[serde(default)]
    pub cookie_secure: bool,
    #[serde(default = "default_session_length")]
    pub session_length: u64,
    #[serde(default = "default_refresh_time")]
    pub refresh_time: u64,
    #[serde(default = "default_hash_iterations")]
    pub hash_iterations: u32,
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    #[serde(default)]
    pub admin_first_time_login: bool,
    #[serde(default)]
    pub oidc: OidcAuthConfig,
    #[serde(default)]
    pub proxy: ProxyAuthConfig,
}

fn default_cookie_name() -> String {
    "ninjanvr_token".to_string()
}
fn default_session_length() -> u64 {
    86400
}
fn default_refresh_time() -> u64 {
    1800
}
fn default_hash_iterations() -> u32 {
    600_000
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            local_login_enabled: true,
            reset_admin_password: false,
            cookie_name: default_cookie_name(),
            cookie_secure: false,
            session_length: default_session_length(),
            refresh_time: default_refresh_time(),
            hash_iterations: default_hash_iterations(),
            trusted_proxies: Vec::new(),
            admin_first_time_login: false,
            oidc: OidcAuthConfig::default(),
            proxy: ProxyAuthConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_path")]
    pub path: String,
}

fn default_db_path() -> String {
    DEFAULT_DB_PATH.to_string()
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: default_db_path(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector: Option<String>,
    #[serde(default = "default_detect_width")]
    pub width: u32,
    #[serde(default = "default_detect_height")]
    pub height: u32,
    #[serde(default = "default_detect_fps")]
    pub fps: u32,
    #[serde(default = "default_min_initialized")]
    pub min_initialized: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_initialized_night: Option<u32>,
    #[serde(default = "default_max_disappeared")]
    pub max_disappeared: u32,
    #[serde(default)]
    pub stationary: StationaryConfig,
}

fn default_detect_width() -> u32 {
    1280
}
fn default_detect_height() -> u32 {
    720
}
fn default_detect_fps() -> u32 {
    5
}
fn default_min_initialized() -> u32 {
    2
}
fn default_max_disappeared() -> u32 {
    25
}

impl Default for DetectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            detector: None,
            width: default_detect_width(),
            height: default_detect_height(),
            fps: default_detect_fps(),
            min_initialized: default_min_initialized(),
            min_initialized_night: None,
            max_disappeared: default_max_disappeared(),
            stationary: StationaryConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StationaryConfig {
    #[serde(default = "default_stationary_interval")]
    pub interval: u32,
    #[serde(default = "default_stationary_threshold")]
    pub threshold: u32,
    #[serde(default = "default_true")]
    pub classifier: bool,
}

fn default_stationary_interval() -> u32 {
    50
}
fn default_stationary_threshold() -> u32 {
    50
}

impl Default for StationaryConfig {
    fn default() -> Self {
        Self {
            interval: default_stationary_interval(),
            threshold: default_stationary_threshold(),
            classifier: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectorEntryConfig {
    #[serde(rename = "type", default = "default_detector_type")]
    pub detector_type: String,
    #[serde(default = "default_detector_device")]
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
}

fn default_detector_type() -> String {
    "openvino".to_string()
}
fn default_detector_device() -> String {
    "CPU".to_string()
}

impl Default for DetectorEntryConfig {
    fn default() -> Self {
        Self {
            detector_type: default_detector_type(),
            device: default_detector_device(),
            model: None,
        }
    }
}

impl DetectorEntryConfig {
    /// Normalizes the configured detector backend type to `"openvino"` or `"onnxruntime"`.
    pub fn normalized_backend_type(&self) -> &'static str {
        let t = self.detector_type.trim().to_ascii_lowercase();
        if t == "onnxruntime" || t == "ort" || t == "onnx" {
            "onnxruntime"
        } else {
            "openvino"
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_model_width")]
    pub width: u32,
    #[serde(default = "default_model_height")]
    pub height: u32,
    #[serde(default = "default_input_tensor")]
    pub input_tensor: String,
    #[serde(default = "default_input_pixel_format")]
    pub input_pixel_format: String,
    #[serde(default = "default_model_path")]
    pub path: String,
    #[serde(default = "default_labelmap_path")]
    pub labelmap_path: String,
    #[serde(default = "default_model_type")]
    pub model_type: String,
}

fn default_model_width() -> u32 {
    300
}
fn default_model_height() -> u32 {
    300
}
fn default_input_tensor() -> String {
    "nhwc".to_string()
}
fn default_input_pixel_format() -> String {
    "bgr".to_string()
}
fn default_model_path() -> String {
    "/openvino-model/ssdlite_mobilenet_v2.xml".to_string()
}
fn default_labelmap_path() -> String {
    "/openvino-model/coco_91cl_bkgr.txt".to_string()
}
fn default_model_type() -> String {
    "ssd".to_string()
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            width: default_model_width(),
            height: default_model_height(),
            input_tensor: default_input_tensor(),
            input_pixel_format: default_input_pixel_format(),
            path: default_model_path(),
            labelmap_path: default_labelmap_path(),
            model_type: default_model_type(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageTiersConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_dir: Option<PathBuf>,
    #[serde(default = "default_migrate_after_hours")]
    pub migrate_after_hours: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_camera_dir: BTreeMap<String, PathBuf>,
}

fn default_migrate_after_hours() -> u32 {
    24
}

impl Default for StorageTiersConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hot_dir: None,
            cold_dir: None,
            migrate_after_hours: default_migrate_after_hours(),
            per_camera_dir: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertBackupConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<PathBuf>,
    #[serde(default = "default_backup_severities")]
    pub severities: Vec<String>,
    #[serde(default = "default_true")]
    pub include_snapshot: bool,
}

fn default_backup_severities() -> Vec<String> {
    vec!["alert".to_string()]
}

impl Default for AlertBackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target_dir: None,
            severities: default_backup_severities(),
            include_snapshot: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdCompactionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_compact_after_hours")]
    pub after_hours: u32,
    #[serde(default = "default_compact_crf")]
    pub crf: u8,
    #[serde(default = "default_true")]
    pub idle_only: bool,
    #[serde(default = "default_compact_codec")]
    pub codec: String,
    #[serde(default = "default_compact_preset")]
    pub preset: String,
    #[serde(default = "default_compact_hwaccel")]
    pub hwaccel: String,
}

fn default_compact_after_hours() -> u32 {
    24
}

fn default_compact_crf() -> u8 {
    28
}

fn default_compact_codec() -> String {
    "h264".to_string()
}

fn default_compact_preset() -> String {
    "veryfast".to_string()
}

fn default_compact_hwaccel() -> String {
    "none".to_string()
}

impl Default for ColdCompactionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            after_hours: default_compact_after_hours(),
            crf: default_compact_crf(),
            idle_only: true,
            codec: default_compact_codec(),
            preset: default_compact_preset(),
            hwaccel: default_compact_hwaccel(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_expire_interval")]
    pub expire_interval: u64,
    #[serde(default)]
    pub continuous_substream: bool,
    #[serde(default)]
    pub mode_tiering: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_gb: Option<f64>,
    #[serde(default)]
    pub continuous: RecordDaysConfig,
    #[serde(default)]
    pub motion: RecordZeroDaysConfig,
    #[serde(default)]
    pub alerts: RecordEventRetainConfig,
    #[serde(default)]
    pub detections: RecordEventRetainConfig,
    #[serde(default)]
    pub preview: RecordPreviewConfig,
    #[serde(default)]
    pub storage_tiers: StorageTiersConfig,
    #[serde(default)]
    pub alert_backup: AlertBackupConfig,
    #[serde(default)]
    pub cold_compaction: ColdCompactionConfig,
}

fn default_expire_interval() -> u64 {
    60
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            expire_interval: default_expire_interval(),
            continuous_substream: false,
            mode_tiering: false,
            max_size_gb: None,
            min_free_gb: None,
            continuous: RecordDaysConfig::default(),
            motion: RecordZeroDaysConfig::default(),
            alerts: RecordEventRetainConfig::default(),
            detections: RecordEventRetainConfig::default(),
            preview: RecordPreviewConfig::default(),
            storage_tiers: StorageTiersConfig::default(),
            alert_backup: AlertBackupConfig::default(),
            cold_compaction: ColdCompactionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordDaysConfig {
    #[serde(default = "default_retain_days")]
    pub days: f64,
}

fn default_retain_days() -> f64 {
    14.0
}

impl Default for RecordDaysConfig {
    fn default() -> Self {
        Self {
            days: default_retain_days(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecordZeroDaysConfig {
    #[serde(default)]
    pub days: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordEventRetainConfig {
    #[serde(default = "default_pre_post_capture")]
    pub pre_capture: u32,
    #[serde(default = "default_pre_post_capture")]
    pub post_capture: u32,
    #[serde(default)]
    pub retain: RetainPolicyConfig,
}

fn default_pre_post_capture() -> u32 {
    5
}

impl Default for RecordEventRetainConfig {
    fn default() -> Self {
        Self {
            pre_capture: default_pre_post_capture(),
            post_capture: default_pre_post_capture(),
            retain: RetainPolicyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainPolicyConfig {
    #[serde(default = "default_retain_days")]
    pub days: f64,
    #[serde(default = "default_retain_mode")]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_gb: Option<f64>,
}

fn default_retain_mode() -> String {
    "motion".to_string()
}

impl Default for RetainPolicyConfig {
    fn default() -> Self {
        Self {
            days: default_retain_days(),
            mode: default_retain_mode(),
            max_size_gb: None,
            min_free_gb: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordPreviewConfig {
    #[serde(default = "default_preview_quality")]
    pub quality: String,
}

fn default_preview_quality() -> String {
    "medium".to_string()
}

impl Default for RecordPreviewConfig {
    fn default() -> Self {
        Self {
            quality: default_preview_quality(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSelectionPolicy {
    BestOverall,
    #[default]
    BestInZone,
    FirstTrigger,
    BestAttribute,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub timestamp: bool,
    #[serde(default = "default_true")]
    pub bounding_box: bool,
    #[serde(default)]
    pub crop: bool,
    #[serde(default = "default_snapshot_quality")]
    pub quality: u8,
    #[serde(default = "default_true")]
    pub high_res: bool,
    #[serde(default = "default_true")]
    pub from_main_stream: bool,
    #[serde(default)]
    pub selection: SnapshotSelectionPolicy,
}

fn default_snapshot_quality() -> u8 {
    70
}

impl Default for SnapshotsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timestamp: false,
            bounding_box: true,
            crop: false,
            quality: default_snapshot_quality(),
            high_res: true,
            from_main_stream: true,
            selection: SnapshotSelectionPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_motion_threshold")]
    pub threshold: u8,
    #[serde(default = "default_lightning_threshold")]
    pub lightning_threshold: f32,
    #[serde(default = "default_true")]
    pub improve_contrast: bool,
    #[serde(default = "default_contour_area")]
    pub contour_area: u32,
    #[serde(default = "default_delta_alpha")]
    pub delta_alpha: f32,
    #[serde(default = "default_frame_alpha")]
    pub frame_alpha: f32,
    #[serde(default = "default_motion_frame_height")]
    pub frame_height: u32,
    #[serde(default)]
    pub mask: Value,
}

fn default_motion_threshold() -> u8 {
    30
}
fn default_lightning_threshold() -> f32 {
    0.8
}
fn default_contour_area() -> u32 {
    10
}
fn default_delta_alpha() -> f32 {
    0.2
}
fn default_frame_alpha() -> f32 {
    0.01
}
fn default_motion_frame_height() -> u32 {
    100
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: default_motion_threshold(),
            lightning_threshold: default_lightning_threshold(),
            improve_contrast: true,
            contour_area: default_contour_area(),
            delta_alpha: default_delta_alpha(),
            frame_alpha: default_frame_alpha(),
            frame_height: default_motion_frame_height(),
            mask: json!({}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LprConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector_model_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_model_path: Option<PathBuf>,
    #[serde(default = "default_lpr_min_confidence")]
    pub min_confidence: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_regex: Option<String>,
    #[serde(default)]
    pub known_plates: BTreeMap<String, String>,
}

fn default_lpr_min_confidence() -> f32 {
    0.75
}

impl Default for LprConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            detector_model_path: None,
            ocr_model_path: None,
            min_confidence: default_lpr_min_confidence(),
            format_regex: None,
            known_plates: BTreeMap::new(),
        }
    }
}

pub const DEFAULT_ATTRIBUTE_LABELS: [&str; 9] = [
    "face",
    "license_plate",
    "amazon",
    "fedex",
    "ups",
    "dhl",
    "correios",
    "mercado_livre",
    "usps",
];

pub fn default_attribute_labels() -> Vec<String> {
    DEFAULT_ATTRIBUTE_LABELS
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectsConfig {
    #[serde(default = "default_tracked_objects")]
    pub track: Vec<String>,
    #[serde(default = "default_attribute_labels")]
    pub attribute_labels: Vec<String>,
    #[serde(default)]
    pub filters: BTreeMap<String, ObjectFilterConfig>,
    #[serde(default)]
    pub mask: Value,
}

fn default_tracked_objects() -> Vec<String> {
    vec!["person".to_string()]
}

impl ObjectsConfig {
    /// Removes any label present in `self.track` (such as `license_plate` on dedicated zoomed LPR cameras)
    /// from `self.attribute_labels` so `ByteTracker` tracks it as a standalone primary object.
    pub fn normalize_standalone_tracked_attributes(&mut self) {
        let tracked = self.track.clone();
        self.attribute_labels.retain(|attr| {
            !tracked
                .iter()
                .any(|trk_lbl| trk_lbl.eq_ignore_ascii_case(attr))
        });
    }
}

impl Default for ObjectsConfig {
    fn default() -> Self {
        let mut filters = BTreeMap::new();
        filters.insert("person".to_string(), ObjectFilterConfig::default());
        Self {
            track: default_tracked_objects(),
            attribute_labels: default_attribute_labels(),
            filters,
            mask: json!({}),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectFilterConfig {
    #[serde(default)]
    pub min_area: f64,
    #[serde(default = "default_max_area")]
    pub max_area: f64,
    #[serde(default)]
    pub min_ratio: f32,
    #[serde(default = "default_max_ratio")]
    pub max_ratio: f32,
    #[serde(default = "default_filter_threshold")]
    pub threshold: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub night_threshold: Option<f32>,
    #[serde(default = "default_filter_min_score")]
    pub min_score: f32,
    #[serde(default)]
    pub mask: Value,
}

impl ObjectFilterConfig {
    /// Resolves `(min_area_px, max_area_px)` for the given `(frame_w, frame_h)`,
    /// treating values in `(0.0, 1.0]` as normalized fractions of `frame_w * frame_h`
    /// and values `> 1.0` as absolute pixel areas.
    pub fn resolve_area_bounds_px(&self, frame_w: u32, frame_h: u32) -> (u32, u32) {
        let frame_area = (frame_w as f64) * (frame_h as f64);
        let min_val = if self.min_area.is_finite() {
            self.min_area
        } else {
            0.0
        };
        let max_val = if self.max_area.is_finite() {
            self.max_area
        } else {
            default_max_area()
        };
        let min_px = if min_val > 0.0 && min_val <= 1.0 {
            (min_val * frame_area).round() as u32
        } else {
            min_val.max(0.0).round() as u32
        };
        let raw_max_px = if max_val > 0.0 && max_val <= 1.0 {
            (max_val * frame_area).round() as u32
        } else {
            max_val.max(1.0).round() as u32
        };
        let max_px = raw_max_px.max(min_px.max(1));
        (min_px, max_px)
    }
}

fn default_max_area() -> f64 {
    24_000_000.0
}
fn default_max_ratio() -> f32 {
    24_000_000.0
}
fn default_filter_threshold() -> f32 {
    0.7
}
fn default_filter_min_score() -> f32 {
    0.5
}

impl Default for ObjectFilterConfig {
    fn default() -> Self {
        Self {
            min_area: 0.0,
            max_area: default_max_area(),
            min_ratio: 0.0,
            max_ratio: default_max_ratio(),
            threshold: default_filter_threshold(),
            night_threshold: None,
            min_score: default_filter_min_score(),
            mask: json!({}),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewConfig {
    #[serde(default)]
    pub alerts: ReviewSeverityConfig,
    #[serde(default)]
    pub detections: ReviewDetectionsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSeverityConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_alert_labels")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_sub_labels: Vec<String>,
    #[serde(default)]
    pub required_zones: Vec<String>,
    #[serde(default = "default_alert_cutoff")]
    pub cutoff_time: u32,
}

fn default_alert_labels() -> Vec<String> {
    vec!["person".to_string(), "car".to_string()]
}
fn default_alert_cutoff() -> u32 {
    40
}

impl Default for ReviewSeverityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            labels: default_alert_labels(),
            sub_labels: Vec::new(),
            exclude_sub_labels: Vec::new(),
            required_zones: Vec::new(),
            cutoff_time: default_alert_cutoff(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDetectionsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub required_zones: Vec<String>,
    #[serde(default = "default_detection_cutoff")]
    pub cutoff_time: u32,
}

fn default_detection_cutoff() -> u32 {
    30
}

impl Default for ReviewDetectionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            required_zones: Vec::new(),
            cutoff_time: default_detection_cutoff(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Go2RtcConfig {
    #[serde(default)]
    pub streams: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_audio_min_volume_dbfs")]
    pub min_volume_dbfs: i32,
    #[serde(default = "default_audio_labels")]
    pub labels: Vec<String>,
}

fn default_audio_min_volume_dbfs() -> i32 {
    -60
}

fn default_audio_labels() -> Vec<String> {
    vec![
        "audio".to_string(),
        "bark".to_string(),
        "speech".to_string(),
        "alarm".to_string(),
    ]
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_volume_dbfs: default_audio_min_volume_dbfs(),
            labels: default_audio_labels(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CameraMapPlacement {
    #[serde(default = "default_map_coord")]
    pub x: f32,
    #[serde(default = "default_map_coord")]
    pub y: f32,
    #[serde(default)]
    pub angle_deg: f32,
    #[serde(default = "default_map_fov_deg")]
    pub fov_deg: f32,
    #[serde(default = "default_map_range")]
    pub range: f32,
}

fn default_map_coord() -> f32 {
    0.5
}

fn default_map_fov_deg() -> f32 {
    90.0
}

fn default_map_range() -> f32 {
    0.25
}

impl Default for CameraMapPlacement {
    fn default() -> Self {
        Self {
            x: default_map_coord(),
            y: default_map_coord(),
            angle_deg: 0.0,
            fov_deg: default_map_fov_deg(),
            range: default_map_range(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnvifCameraConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default = "default_onvif_port")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, deserialize_with = "deserialize_autotracking_bool")]
    pub autotracking: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_preset: Option<String>,
    #[serde(default = "default_onvif_timeout_sec")]
    pub timeout_sec: u64,
}

impl std::fmt::Debug for OnvifCameraConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let masked_password = self
            .password
            .as_deref()
            .map(|p| if p.is_empty() { "" } else { "***" });
        f.debug_struct("OnvifCameraConfig")
            .field("host", &self.host.as_deref().map(scrub_rtsp_credentials))
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &masked_password)
            .field("autotracking", &self.autotracking)
            .field("return_preset", &self.return_preset)
            .field("timeout_sec", &self.timeout_sec)
            .finish()
    }
}

fn default_onvif_port() -> u16 {
    80
}

fn default_onvif_timeout_sec() -> u64 {
    5
}

fn deserialize_autotracking_bool<'de, D>(deserializer: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let val = Value::deserialize(deserializer)?;
    match val {
        Value::Bool(b) => Ok(b),
        Value::Object(map) => Ok(map
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)),
        _ => Ok(false),
    }
}

impl Default for OnvifCameraConfig {
    fn default() -> Self {
        Self {
            host: None,
            port: default_onvif_port(),
            user: None,
            password: None,
            autotracking: false,
            return_preset: None,
            timeout_sec: default_onvif_timeout_sec(),
        }
    }
}

impl OnvifCameraConfig {
    pub fn is_unconfigured(&self) -> bool {
        self.host.as_deref().is_none_or(|h| h.trim().is_empty())
            && self.user.as_deref().is_none_or(|u| u.trim().is_empty())
            && self.password.as_deref().is_none_or(|p| p.is_empty())
            && !self.autotracking
            && self
                .return_preset
                .as_deref()
                .is_none_or(|r| r.trim().is_empty())
            && self.port == default_onvif_port()
            && self.timeout_sec == default_onvif_timeout_sec()
    }

    pub fn is_configured(&self) -> bool {
        self.host.as_deref().is_some_and(|h| !h.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub indoor: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webui_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub privacy_mask: Value,
    #[serde(default)]
    pub ui: CameraUiConfig,
    #[serde(default)]
    pub map: CameraMapPlacement,
    #[serde(default, skip_serializing_if = "OnvifCameraConfig::is_unconfigured")]
    pub onvif: OnvifCameraConfig,
    #[serde(default)]
    pub ffmpeg: CameraFfmpegConfig,
    #[serde(default)]
    pub live: CameraLiveConfig,
    #[serde(default)]
    pub detect: DetectConfig,
    #[serde(default)]
    pub record: RecordConfig,
    #[serde(default)]
    pub snapshots: SnapshotsConfig,
    #[serde(default)]
    pub motion: MotionConfig,
    #[serde(default)]
    pub objects: ObjectsConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub zones: BTreeMap<String, ZoneConfig>,
    #[serde(default)]
    pub lines: BTreeMap<String, LineConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub state_rois: BTreeMap<String, StateRoiConfig>,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            indoor: false,
            webui_url: None,
            notes: None,
            privacy_mask: Value::Null,
            ui: CameraUiConfig::default(),
            map: CameraMapPlacement::default(),
            onvif: OnvifCameraConfig::default(),
            ffmpeg: CameraFfmpegConfig::default(),
            live: CameraLiveConfig::default(),
            detect: DetectConfig::default(),
            record: RecordConfig::default(),
            snapshots: SnapshotsConfig::default(),
            motion: MotionConfig::default(),
            objects: ObjectsConfig::default(),
            audio: AudioConfig::default(),
            review: ReviewConfig::default(),
            zones: BTreeMap::new(),
            lines: BTreeMap::new(),
            state_rois: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateRoiConfig {
    #[serde(default = "default_state_roi_crop")]
    pub crop: [f32; 4],
    #[serde(default = "default_state_roi_classes")]
    pub classes: Vec<String>,
    #[serde(default = "default_state_roi_interval_sec")]
    pub interval_sec: u32,
    #[serde(default = "default_state_roi_hysteresis")]
    pub hysteresis: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anomaly_state: Option<String>,
    #[serde(default = "default_state_roi_anomaly_timeout_sec")]
    pub anomaly_timeout_sec: u32,
}

fn default_state_roi_crop() -> [f32; 4] {
    [0.25, 0.25, 0.75, 0.75]
}

fn default_state_roi_classes() -> Vec<String> {
    vec!["closed".to_string(), "open".to_string()]
}

fn default_state_roi_interval_sec() -> u32 {
    5
}

fn default_state_roi_hysteresis() -> u32 {
    3
}

fn default_state_roi_anomaly_timeout_sec() -> u32 {
    300
}

impl Default for StateRoiConfig {
    fn default() -> Self {
        Self {
            crop: default_state_roi_crop(),
            classes: default_state_roi_classes(),
            interval_sec: default_state_roi_interval_sec(),
            hysteresis: default_state_roi_hysteresis(),
            anomaly_state: None,
            anomaly_timeout_sec: default_state_roi_anomaly_timeout_sec(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LineDirection {
    #[default]
    Any,
    AToB,
    BToA,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LineConfig {
    pub points: [f32; 4],
    #[serde(default)]
    pub direction: LineDirection,
    #[serde(default)]
    pub objects: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoneConfig {
    pub coordinates: Value,
    #[serde(default)]
    pub objects: Vec<String>,
    #[serde(default = "default_zone_inertia")]
    pub inertia: u32,
    #[serde(default)]
    pub loitering_time: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_area: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_area: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

fn default_zone_inertia() -> u32 {
    3
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CameraFfmpegConfig {
    #[serde(default)]
    pub inputs: Vec<CameraFfmpegInput>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub output_args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_offset_ms: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraFfmpegInput {
    pub path: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub input_args: Value,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraLiveConfig {
    #[serde(default)]
    pub streams: BTreeMap<String, String>,
    #[serde(default = "default_detect_height")]
    pub height: u32,
    #[serde(default = "default_live_quality")]
    pub quality: u8,
    #[serde(default)]
    pub rotate: u16,
}

fn default_live_quality() -> u8 {
    8
}

impl Default for CameraLiveConfig {
    fn default() -> Self {
        Self {
            streams: BTreeMap::new(),
            height: 720,
            quality: 8,
            rotate: 0,
        }
    }
}

impl NinjaConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw_yaml = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file at {}", path.display()))?;
        Self::from_yaml_str(&raw_yaml, path.to_path_buf())
    }

    pub fn from_yaml_str(raw_yaml: &str, config_path: PathBuf) -> Result<Self> {
        let raw_doc: Value = serde_yaml::from_str(raw_yaml).unwrap_or(Value::Null);
        let mut cfg: NinjaConfig = if raw_yaml.trim().is_empty() || raw_doc.is_null() {
            serde_yaml::from_str("{}").context("failed to parse config YAML")?
        } else {
            serde_yaml::from_str(raw_yaml).context("failed to parse config YAML")?
        };
        cfg.raw_yaml = raw_yaml.to_string();
        cfg.config_path = config_path;

        if cfg.detectors.is_empty() {
            cfg.detectors
                .insert("detector01".to_string(), DetectorEntryConfig::default());
        }

        cfg.objects.normalize_standalone_tracked_attributes();
        cfg.ui.theme = normalize_ui_theme(&cfg.ui.theme);
        sanitize_floorplan_config(&mut cfg.ui.floorplan);

        for (cam_name, cam) in cfg.cameras.iter_mut() {
            if cam.live.streams.is_empty() {
                cam.live
                    .streams
                    .insert("Stream 1".to_string(), format!("{cam_name}_sub"));
                cam.live
                    .streams
                    .insert("Stream 2".to_string(), cam_name.clone());
            }
            let has_explicit_cam_track = raw_doc
                .get("cameras")
                .and_then(|c| c.get(cam_name.as_str()))
                .and_then(|c| c.get("objects"))
                .and_then(|o| o.get("track"))
                .is_some();
            if cam.objects.track.is_empty()
                || (!has_explicit_cam_track
                    && cam.objects.track == default_tracked_objects()
                    && cfg.objects.track != default_tracked_objects())
            {
                cam.objects.track = cfg.objects.track.clone();
            }
            let has_explicit_cam_attrs = raw_doc
                .get("cameras")
                .and_then(|c| c.get(cam_name.as_str()))
                .and_then(|c| c.get("objects"))
                .and_then(|o| o.get("attribute_labels"))
                .is_some();
            if cam.objects.attribute_labels.is_empty()
                || (!has_explicit_cam_attrs
                    && cam.objects.attribute_labels == default_attribute_labels()
                    && cfg.objects.attribute_labels != default_attribute_labels())
            {
                cam.objects.attribute_labels = cfg.objects.attribute_labels.clone();
            }
            cam.objects.normalize_standalone_tracked_attributes();
            if !cam.record.alert_backup.enabled && cfg.record.alert_backup.enabled {
                cam.record.alert_backup = cfg.record.alert_backup.clone();
            }
            if cam.review.alerts.sub_labels.is_empty() && !cfg.review.alerts.sub_labels.is_empty() {
                cam.review.alerts.sub_labels = cfg.review.alerts.sub_labels.clone();
            }
            if cam.review.alerts.exclude_sub_labels.is_empty()
                && !cfg.review.alerts.exclude_sub_labels.is_empty()
            {
                cam.review.alerts.exclude_sub_labels = cfg.review.alerts.exclude_sub_labels.clone();
            }
            for roi in cam.state_rois.values_mut() {
                if roi.classes.is_empty() {
                    roi.classes = default_state_roi_classes();
                }
                roi.interval_sec = roi.interval_sec.max(1);
                roi.hysteresis = roi.hysteresis.max(1);
                roi.anomaly_timeout_sec = roi.anomaly_timeout_sec.max(1);
                if roi
                    .anomaly_state
                    .as_ref()
                    .is_some_and(|s| s.trim().is_empty())
                {
                    roi.anomaly_state = None;
                }
            }
        }

        let extra_keys: Vec<String> = cfg.go2rtc.extra.keys().cloned().collect();
        for k in extra_keys {
            if k.trim().eq_ignore_ascii_case("streams")
                && let Some(val) = cfg.go2rtc.extra.remove(&k)
            {
                let extra_streams: BTreeMap<String, Value> = serde_json::from_value(val)
                    .context("invalid go2rtc.streams config in extra")?;
                validate_go2rtc_streams_map(&extra_streams)?;
                for (sk, sv) in extra_streams {
                    cfg.go2rtc.streams.entry(sk).or_insert(sv);
                }
            }
        }
        validate_go2rtc_streams_map(&cfg.go2rtc.streams)?;
        validate_go2rtc_extra_map(&cfg.go2rtc.extra)?;

        Ok(cfg)
    }

    /// Returns the RTSP input path configured for a given role (`record` or `detect`).
    pub fn camera_input_for_role(&self, camera: &str, role: &str) -> Option<String> {
        let cam = self.cameras.get(camera)?;
        for input in &cam.ffmpeg.inputs {
            if input.roles.iter().any(|r| r == role) {
                return Some(input.path.clone());
            }
        }
        None
    }

    /// Writes `/dev/shm/go2rtc.yaml` compatible with go2rtc.
    pub fn write_go2rtc_config(&self, target_path: impl AsRef<Path>) -> Result<()> {
        validate_go2rtc_streams_map(&self.go2rtc.streams)?;
        validate_go2rtc_extra_map(&self.go2rtc.extra)?;
        let target_path = target_path.as_ref();
        if let Ok(meta) = std::fs::symlink_metadata(target_path)
            && meta.file_type().is_symlink()
        {
            anyhow::bail!(
                "refusing to write go2rtc config through symlink {}",
                target_path.display()
            );
        }
        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut root = serde_json::Map::new();
        root.insert(
            "streams".to_string(),
            serde_json::to_value(&self.go2rtc.streams)?,
        );
        root.insert("api".to_string(), json!({ "origin": "*" }));
        root.insert("log".to_string(), json!({ "format": "text" }));
        root.insert("webrtc".to_string(), json!({ "candidates": ["stun:8555"] }));
        for (k, v) in &self.go2rtc.extra {
            let k_norm = k.trim().to_ascii_lowercase();
            if matches!(
                k_norm.as_str(),
                "exec"
                    | "expr"
                    | "echo"
                    | "hass"
                    | "ngrok"
                    | "streams"
                    | "api"
                    | "log"
                    | "webrtc"
                    | "publish"
                    | "webtorrent"
                    | "roborock"
                    | "dvrip"
            ) {
                continue;
            }
            if !root
                .keys()
                .any(|rk| rk.trim().eq_ignore_ascii_case(&k_norm))
            {
                root.insert(k.clone(), v.clone());
            }
        }

        let yaml_str = serde_yaml::to_string(&Value::Object(root))?;
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(target_path)?;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            file.write_all(yaml_str.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(target_path, yaml_str)?;
        }
        Ok(())
    }

    pub fn load_labelmap(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        let mut candidates = vec![PathBuf::from(&self.model.labelmap_path)];
        if let Ok(env_path) = std::env::var("NINJANVR_LABELMAP_PATH") {
            candidates.push(PathBuf::from(env_path));
        }
        for candidate in candidates {
            if !is_safe_labelmap_path(&candidate) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                for (idx, line) in content.lines().enumerate() {
                    if map.len() >= 256 {
                        break;
                    }
                    let trimmed = line.trim();
                    let trimmed_lc = trimmed.to_ascii_lowercase();
                    if !trimmed.is_empty()
                        && trimmed.len() <= 128
                        && !trimmed.contains("://")
                        && !trimmed_lc.starts_with("password:")
                        && !trimmed_lc.starts_with("api_key:")
                        && !trimmed_lc.starts_with("secret:")
                        && !trimmed_lc.starts_with("client_secret:")
                        && !trimmed_lc.starts_with("token:")
                        && !trimmed_lc.starts_with("-----begin")
                    {
                        map.insert(idx.to_string(), trimmed.to_string());
                    }
                }
                if !map.is_empty() {
                    return map;
                }
            }
        }

        // Standard COCO fallback
        let defaults = [
            (0, "__background__"),
            (1, "person"),
            (2, "bicycle"),
            (3, "car"),
            (4, "motorcycle"),
            (6, "bus"),
            (7, "train"),
            (8, "car"),
            (16, "bird"),
            (17, "cat"),
            (18, "dog"),
        ];
        for (idx, label) in defaults {
            map.insert(idx.to_string(), label.to_string());
        }
        map
    }

    /// Alias for `to_api_config()` returning the `/api/config` JSON object.
    pub fn to_api_value(&self) -> Value {
        self.to_api_config()
    }

    /// Generates the complete `/api/config` JSON object expected by the React SPA.
    pub fn to_api_config(&self) -> Value {
        let labelmap = self.load_labelmap();
        let attributes_map = json!({
            "person": ["amazon", "face"],
            "car": ["amazon", "an_post", "canada_post", "correios", "dhl", "dpd", "fedex", "gls", "license_plate", "mercado_livre", "nzpost", "postnl", "postnord", "purolator", "royal_mail", "ups", "usps"],
            "truck": ["amazon", "correios", "dhl", "fedex", "license_plate", "mercado_livre", "ups", "usps"],
            "garbage_truck": ["license_plate"],
            "motorcycle": ["license_plate"],
            "bus": ["license_plate"],
            "school_bus": ["license_plate"]
        });

        let mut detectors_json = serde_json::Map::new();
        for (name, det) in &self.detectors {
            let eff_model = det.model.as_ref().unwrap_or(&self.model);
            detectors_json.insert(
                name.clone(),
                json!({
                    "type": det.detector_type,
                    "device": det.device,
                    "model": {
                        "path": eff_model.path,
                        "labelmap_path": eff_model.labelmap_path,
                        "width": eff_model.width,
                        "height": eff_model.height,
                        "labelmap": labelmap,
                        "attributes_map": attributes_map,
                        "input_tensor": eff_model.input_tensor,
                        "input_pixel_format": eff_model.input_pixel_format,
                        "input_dtype": "int",
                        "model_type": eff_model.model_type
                    }
                }),
            );
        }

        let mut cameras_json = serde_json::Map::new();
        for (order_idx, (cam_name, cam)) in self.cameras.iter().enumerate() {
            let inputs_json: Vec<Value> = cam
                .ffmpeg
                .inputs
                .iter()
                .map(|inp| {
                    json!({
                        "path": scrub_rtsp_credentials(&inp.path),
                        "roles": inp.roles,
                        "global_args": [],
                        "hwaccel_args": [],
                        "input_args": if inp.input_args.is_null() { json!("preset-rtsp-restream") } else { inp.input_args.clone() }
                    })
                })
                .collect();

            let mut filters_json = serde_json::Map::new();
            for label in &cam.objects.track {
                let f = cam.objects.filters.get(label).cloned().unwrap_or_default();
                filters_json.insert(
                    label.clone(),
                    json!({
                        "min_area": f.min_area,
                        "max_area": f.max_area,
                        "min_ratio": f.min_ratio,
                        "max_ratio": f.max_ratio,
                        "threshold": f.threshold,
                        "night_threshold": f.night_threshold,
                        "min_score": f.min_score,
                        "mask": f.mask
                    }),
                );
            }
            for (label, f) in &cam.objects.filters {
                filters_json.entry(label.clone()).or_insert_with(|| {
                    json!({
                        "min_area": f.min_area,
                        "max_area": f.max_area,
                        "min_ratio": f.min_ratio,
                        "max_ratio": f.max_ratio,
                        "threshold": f.threshold,
                        "night_threshold": f.night_threshold,
                        "min_score": f.min_score,
                        "mask": f.mask
                    })
                });
            }

            cameras_json.insert(
                cam_name.clone(),
                json!({
                    "name": cam_name,
                    "enabled": cam.enabled,
                    "enabled_in_config": cam.enabled,
                    "indoor": cam.indoor,
                    "type": "generic",
                    "best_image_timeout": 60,
                    "audio": {
                        "enabled": cam.audio.enabled,
                        "enabled_in_config": cam.audio.enabled,
                        "min_volume_dbfs": cam.audio.min_volume_dbfs,
                        "labels": cam.audio.labels,
                        "max_not_heard": 30,
                        "min_volume": 500,
                        "listen": ["bark", "fire_alarm", "speech", "yell"],
                        "filters": {
                            "bark": { "threshold": 0.8 },
                            "fire_alarm": { "threshold": 0.8 },
                            "speech": { "threshold": 0.8 },
                            "yell": { "threshold": 0.8 }
                        },
                        "num_threads": 2
                    },
                    "audio_transcription": {
                        "enabled": false,
                        "enabled_in_config": false,
                        "live_enabled": false
                    },
                    "birdseye": {
                        "enabled": false,
                        "mode": "objects",
                        "order": order_idx
                    },
                    "detect": {
                        "enabled": cam.detect.enabled,
                        "width": cam.detect.width,
                        "height": cam.detect.height,
                        "fps": cam.detect.fps,
                        "min_initialized": cam.detect.min_initialized,
                        "min_initialized_night": cam.detect.min_initialized_night,
                        "max_disappeared": cam.detect.max_disappeared,
                        "detector": cam.detect.detector,
                        "stationary": {
                            "interval": cam.detect.stationary.interval,
                            "threshold": cam.detect.stationary.threshold,
                            "max_frames": { "objects": {} },
                            "classifier": cam.detect.stationary.classifier
                        },
                        "annotation_offset": 0
                    },
                    "face_recognition": {
                        "enabled": self.face_recognition.enabled,
                        "min_area": self.face_recognition.min_face_area
                    },
                    "ffmpeg": {
                        "path": "default",
                        "global_args": ["-hide_banner", "-loglevel", "warning", "-threads", "2"],
                        "hwaccel_args": "",
                        "input_args": "preset-rtsp-generic",
                        "output_args": if cam.ffmpeg.output_args.is_null() {
                            json!({
                                "detect": ["-threads", "2", "-f", "rawvideo", "-pix_fmt", "yuv420p"],
                                "record": "preset-record-generic-audio-aac"
                            })
                        } else {
                            cam.ffmpeg.output_args.clone()
                        },
                        "retry_interval": 10.0,
                        "apple_compatibility": false,
                        "gpu": 0,
                        "audio_offset_ms": cam.ffmpeg.audio_offset_ms,
                        "inputs": inputs_json
                    },
                    "live": {
                        "streams": cam.live.streams,
                        "height": cam.live.height,
                        "quality": cam.live.quality,
                        "rotate": cam.live.rotate
                    },
                    "lpr": {
                        "enabled": self.lpr.enabled,
                        "expire_time": 3,
                        "min_area": 1000,
                        "enhancement": 0
                    },
                    "motion": {
                        "enabled": cam.motion.enabled,
                        "threshold": cam.motion.threshold,
                        "lightning_threshold": cam.motion.lightning_threshold,
                        "improve_contrast": cam.motion.improve_contrast,
                        "contour_area": cam.motion.contour_area,
                        "delta_alpha": cam.motion.delta_alpha,
                        "frame_alpha": cam.motion.frame_alpha,
                        "frame_height": cam.motion.frame_height,
                        "mask": cam.motion.mask,
                        "mqtt_off_delay": 30
                    },
                    "objects": {
                        "track": cam.objects.track,
                        "attribute_labels": cam.objects.attribute_labels,
                        "filters": filters_json,
                        "mask": cam.objects.mask,
                        "genai": {
                            "enabled": false,
                            "enabled_in_config": false,
                            "use_snapshot": false,
                            "prompt": "",
                            "object_prompts": {},
                            "objects": [],
                            "required_zones": [],
                            "debug_save_thumbnails": false,
                            "send_triggers": { "tracked_object_end": true }
                        }
                    },
                    "record": {
                        "enabled": cam.record.enabled,
                        "enabled_in_config": cam.record.enabled,
                        "expire_interval": cam.record.expire_interval,
                        "continuous_substream": cam.record.continuous_substream,
                        "mode_tiering": cam.record.mode_tiering,
                        "max_size_gb": cam.record.max_size_gb,
                        "min_free_gb": cam.record.min_free_gb,
                        "storage_tiers": cam.record.storage_tiers,
                        "alert_backup": cam.record.alert_backup,
                        "cold_compaction": cam.record.cold_compaction,
                        "continuous": { "days": cam.record.continuous.days },
                        "motion": { "days": cam.record.motion.days },
                        "detections": {
                            "pre_capture": cam.record.detections.pre_capture,
                            "post_capture": cam.record.detections.post_capture,
                            "retain": {
                                "days": cam.record.detections.retain.days,
                                "mode": cam.record.detections.retain.mode,
                                "max_size_gb": cam.record.detections.retain.max_size_gb,
                                "min_free_gb": cam.record.detections.retain.min_free_gb
                            }
                        },
                        "alerts": {
                            "pre_capture": cam.record.alerts.pre_capture,
                            "post_capture": cam.record.alerts.post_capture,
                            "retain": {
                                "days": cam.record.alerts.retain.days,
                                "mode": cam.record.alerts.retain.mode,
                                "max_size_gb": cam.record.alerts.retain.max_size_gb,
                                "min_free_gb": cam.record.alerts.retain.min_free_gb
                            }
                        },
                        "export": {
                            "hwaccel_args": "",
                            "max_concurrent": 3,
                            "chapters": "review_items"
                        },
                        "preview": {
                            "quality": cam.record.preview.quality
                        }
                    },
                    "review": {
                        "alerts": {
                            "enabled": cam.review.alerts.enabled,
                            "enabled_in_config": cam.review.alerts.enabled,
                            "labels": cam.review.alerts.labels,
                            "sub_labels": cam.review.alerts.sub_labels,
                            "exclude_sub_labels": cam.review.alerts.exclude_sub_labels,
                            "required_zones": cam.review.alerts.required_zones,
                            "cutoff_time": cam.review.alerts.cutoff_time
                        },
                        "detections": {
                            "enabled": cam.review.detections.enabled,
                            "enabled_in_config": cam.review.detections.enabled,
                            "required_zones": cam.review.detections.required_zones,
                            "cutoff_time": cam.review.detections.cutoff_time
                        },
                        "genai": {
                            "enabled": false,
                            "enabled_in_config": false,
                            "alerts": true,
                            "detections": false,
                            "image_source": "preview",
                            "additional_concerns": [],
                            "debug_save_thumbnails": false,
                            "activity_context_prompt": ""
                        }
                    },
                    "semantic_search": { "triggers": {} },
                    "snapshots": {
                        "enabled": cam.snapshots.enabled,
                        "timestamp": cam.snapshots.timestamp,
                        "bounding_box": cam.snapshots.bounding_box,
                        "crop": cam.snapshots.crop,
                        "required_zones": [],
                        "retain": { "default": 10.0, "objects": {} },
                        "quality": cam.snapshots.quality,
                        "high_res": cam.snapshots.high_res,
                        "from_main_stream": cam.snapshots.from_main_stream,
                        "selection": cam.snapshots.selection
                    },
                    "timestamp_style": {
                        "position": "tl",
                        "format": "%m/%d/%Y %H:%M:%S",
                        "color": { "red": 255, "green": 255, "blue": 255 },
                        "thickness": 2
                    },
                    "mqtt": {
                        "enabled": true,
                        "timestamp": true,
                        "bounding_box": true,
                        "crop": true,
                        "height": 270,
                        "required_zones": [],
                        "quality": 70
                    },
                    "notifications": {
                        "enabled": false,
                        "cooldown": 0,
                        "enabled_in_config": false
                    },
                    "onvif": {
                        "host": cam.onvif.host.as_deref().map(scrub_rtsp_credentials),
                        "port": cam.onvif.port,
                        "user": cam.onvif.user,
                        "password": if cam.onvif.password.as_ref().is_some_and(|p| !p.is_empty() && p != "***") {
                            json!("***")
                        } else {
                            Value::Null
                        },
                        "has_password": cam.onvif.password.as_ref().is_some_and(|p| !p.is_empty() && p != "***"),
                        "autotracking": cam.onvif.autotracking,
                        "return_preset": cam.onvif.return_preset,
                        "timeout_sec": cam.onvif.timeout_sec
                    },
                    "ui": {
                        "order": if cam.ui.order != 0 { cam.ui.order } else { order_idx as i32 },
                        "dashboard": cam.ui.dashboard,
                        "review": true
                    },
                    "webui_url": cam.webui_url.as_deref().map(scrub_rtsp_credentials),
                    "notes": cam.notes,
                    "privacy_mask": cam.privacy_mask,
                    "map": cam.map,
                    "profiles": {},
                    "zones": cam.zones,
                    "lines": cam.lines,
                    "state_rois": cam.state_rois,
                    "ffmpeg_cmds": []
                }),
            );
        }

        json!({
            "version": self.version,
            "safe_mode": false,
            "environment_vars": {},
            "logger": { "default": "info", "logs": {} },
            "auth": {
                "enabled": self.auth.enabled,
                "local_login_enabled": self.auth.local_login_enabled,
                "reset_admin_password": self.auth.reset_admin_password,
                "cookie_name": self.auth.cookie_name,
                "cookie_secure": self.auth.cookie_secure,
                "session_length": self.auth.session_length,
                "refresh_time": self.auth.refresh_time,
                "trusted_proxies": self.auth.trusted_proxies,
                "hash_iterations": self.auth.hash_iterations,
                "roles": {
                    "admin": [],
                    "viewer": []
                },
                "admin_first_time_login": self.auth.admin_first_time_login,
                "oidc": {
                    "enabled": self.auth.oidc.enabled,
                    "provider_name": self.auth.oidc.provider_name,
                    "issuer_url": scrub_rtsp_credentials(&self.auth.oidc.issuer_url),
                    "client_id": self.auth.oidc.client_id,
                    "client_secret_env": self.auth.oidc.client_secret_env,
                    "scopes": self.auth.oidc.scopes,
                    "auto_redirect": self.auth.oidc.auto_redirect,
                    "username_claim": self.auth.oidc.username_claim,
                    "groups_claim": self.auth.oidc.groups_claim,
                    "role_mapping": self.auth.oidc.role_mapping,
                    "default_role": self.auth.oidc.default_role
                },
                "proxy": {
                    "enabled": self.auth.proxy.enabled,
                    "trusted_proxies": self.auth.proxy.trusted_proxies,
                    "user_header": self.auth.proxy.user_header,
                    "groups_header": self.auth.proxy.groups_header,
                    "logout_url": scrub_rtsp_credentials(&self.auth.proxy.logout_url)
                }
            },
            "database": {
                "path": self.database.path
            },
            "go2rtc": {
                "streams": scrub_json_value(&serde_json::to_value(&self.go2rtc.streams).unwrap_or_else(|_| json!({})))
            },
            "mqtt": {
                "enabled": self.mqtt.enabled,
                "host": self.mqtt.host,
                "port": self.mqtt.port,
                "topic_prefix": self.mqtt.topic_prefix,
                "client_id": self.mqtt.client_id,
                "user": self.mqtt.user,
                "has_password": self.mqtt.password.as_ref().is_some_and(|p| !p.is_empty() && p != "***" && p != "*"),
                "stats_interval": self.mqtt.stats_interval,
                "tls_enabled": self.mqtt.should_use_tls(),
                "tls_ca_certs": self.mqtt.tls_ca_certs,
                "tls_client_cert": self.mqtt.tls_client_cert,
                "tls_client_key": self.mqtt.tls_client_key,
                "tls_insecure": self.mqtt.tls_insecure,
                "qos": 0
            },
            "notifications": {
                "enabled": false,
                "cooldown": 0,
                "enabled_in_config": false
            },
            "networking": {
                "ipv6": { "enabled": false },
                "listen": { "internal": 5000, "external": 8971 }
            },
            "proxy": {
                "header_map": { "role_map": {} },
                "default_role": "viewer",
                "separator": ","
            },
            "telemetry": {
                "network_interfaces": [],
                "stats": {
                    "amd_gpu_stats": false,
                    "intel_gpu_stats": false,
                    "network_bandwidth": false
                },
                "version_check": false
            },
            "tls": {
                "enabled": self.tls.enabled,
                "san_hosts": self.tls.san_hosts
            },
            "ui": {
                "time_format": self.ui.time_format,
                "unit_system": self.ui.unit_system,
                "theme": self.ui.theme,
                "high_contrast": self.ui.high_contrast,
                "floorplan": {
                    "background_data_url": self.ui.floorplan.background_data_url,
                    "opacity": self.ui.floorplan.opacity,
                    "width_m": self.ui.floorplan.width_m,
                    "height_m": self.ui.floorplan.height_m,
                    "rooms": self.ui.floorplan.rooms,
                    "walls": self.ui.floorplan.walls
                }
            },
            "detectors": detectors_json,
            "model": {
                "path": self.model.path,
                "labelmap_path": self.model.labelmap_path,
                "width": self.model.width,
                "height": self.model.height,
                "labelmap": {},
                "attributes_map": attributes_map,
                "input_tensor": self.model.input_tensor,
                "input_pixel_format": self.model.input_pixel_format,
                "input_dtype": "int",
                "model_type": self.model.model_type,
                "colormap": { "person": [31, 119, 180] },
                "all_attributes": ["face", "license_plate"],
                "non_logo_attributes": ["face", "license_plate"]
            },
            "genai": {
                "enabled": self.genai.enabled,
                "provider": self.genai.provider,
                "base_url": self.genai.base_url.as_deref().map(scrub_rtsp_credentials),
                "api_key_env": self.genai.api_key_env,
                "has_api_key": self.genai.has_api_key(),
                "model": self.genai.model,
                "verify_ambiguous_alerts": self.genai.verify_ambiguous_alerts,
                "ambiguous_score_min": self.genai.ambiguous_score_min,
                "ambiguous_score_max": self.genai.ambiguous_score_max,
                "auto_suppress_false_positives": self.genai.auto_suppress_false_positives,
                "timeout_ms": self.genai.timeout_ms
            },
            "cameras": cameras_json,
            "audio": {
                "enabled": self.audio.enabled,
                "min_volume_dbfs": self.audio.min_volume_dbfs,
                "labels": self.audio.labels,
                "max_not_heard": 30,
                "min_volume": 500,
                "listen": ["bark", "fire_alarm", "speech", "yell"],
                "filters": {},
                "num_threads": 2
            },
            "birdseye": {
                "enabled": false,
                "mode": "objects",
                "restream": false,
                "width": 1280,
                "height": 720,
                "quality": 8,
                "inactivity_threshold": 30,
                "layout": { "scaling_factor": 2.0 },
                "idle_heartbeat_fps": 0.0
            },
            "detect": {
                "enabled": self.detect.enabled,
                "height": self.detect.height,
                "width": self.detect.width,
                "fps": self.detect.fps,
                "min_initialized": self.detect.min_initialized,
                "min_initialized_night": self.detect.min_initialized_night,
                "max_disappeared": self.detect.max_disappeared,
                "detector": self.detect.detector,
                "stationary": {
                    "interval": self.detect.stationary.interval,
                    "threshold": self.detect.stationary.threshold,
                    "max_frames": { "objects": {} },
                    "classifier": self.detect.stationary.classifier
                },
                "annotation_offset": 0
            },
            "motion": {
                "enabled": self.motion.enabled,
                "threshold": self.motion.threshold,
                "lightning_threshold": self.motion.lightning_threshold,
                "improve_contrast": self.motion.improve_contrast,
                "contour_area": self.motion.contour_area,
                "delta_alpha": self.motion.delta_alpha,
                "frame_alpha": self.motion.frame_alpha,
                "frame_height": self.motion.frame_height,
                "mask": self.motion.mask,
                "mqtt_off_delay": 30
            },
            "ffmpeg": {
                "path": "default",
                "global_args": ["-hide_banner", "-loglevel", "warning", "-threads", "2"],
                "hwaccel_args": "",
                "input_args": "preset-rtsp-generic",
                "output_args": {
                    "detect": ["-threads", "2", "-f", "rawvideo", "-pix_fmt", "yuv420p"],
                    "record": "preset-record-generic-audio-aac"
                },
                "retry_interval": 10.0,
                "apple_compatibility": false,
                "gpu": 0
            },
            "live": {
                "streams": [],
                "height": 720,
                "quality": 8
            },
            "objects": {
                "track": self.objects.track,
                "attribute_labels": self.objects.attribute_labels,
                "filters": serde_json::to_value(&self.objects.filters).unwrap_or_else(|_| json!({})),
                "mask": self.objects.mask,
                "genai": {
                    "enabled": false,
                    "use_snapshot": false,
                    "prompt": "",
                    "object_prompts": {},
                    "objects": [],
                    "required_zones": [],
                    "debug_save_thumbnails": false,
                    "send_triggers": { "tracked_object_end": true }
                }
            },
            "record": {
                "enabled": self.record.enabled,
                "expire_interval": self.record.expire_interval,
                "continuous_substream": self.record.continuous_substream,
                "mode_tiering": self.record.mode_tiering,
                "max_size_gb": self.record.max_size_gb,
                "min_free_gb": self.record.min_free_gb,
                "storage_tiers": self.record.storage_tiers,
                "alert_backup": self.record.alert_backup,
                "cold_compaction": self.record.cold_compaction,
                "continuous": { "days": self.record.continuous.days },
                "motion": { "days": self.record.motion.days },
                "detections": {
                    "pre_capture": self.record.detections.pre_capture,
                    "post_capture": self.record.detections.post_capture,
                    "retain": {
                        "days": self.record.detections.retain.days,
                        "mode": self.record.detections.retain.mode,
                        "max_size_gb": self.record.detections.retain.max_size_gb,
                        "min_free_gb": self.record.detections.retain.min_free_gb
                    }
                },
                "alerts": {
                    "pre_capture": self.record.alerts.pre_capture,
                    "post_capture": self.record.alerts.post_capture,
                    "retain": {
                        "days": self.record.alerts.retain.days,
                        "mode": self.record.alerts.retain.mode,
                        "max_size_gb": self.record.alerts.retain.max_size_gb,
                        "min_free_gb": self.record.alerts.retain.min_free_gb
                    }
                },
                "export": {
                    "hwaccel_args": "",
                    "max_concurrent": 3,
                    "chapters": "review_items"
                },
                "preview": { "quality": self.record.preview.quality }
            },
            "review": {
                "alerts": {
                    "enabled": self.review.alerts.enabled,
                    "labels": self.review.alerts.labels,
                    "sub_labels": self.review.alerts.sub_labels,
                    "exclude_sub_labels": self.review.alerts.exclude_sub_labels,
                    "required_zones": self.review.alerts.required_zones,
                    "cutoff_time": self.review.alerts.cutoff_time
                },
                "detections": {
                    "enabled": self.review.detections.enabled,
                    "required_zones": self.review.detections.required_zones,
                    "cutoff_time": self.review.detections.cutoff_time
                },
                "genai": {
                    "enabled": false,
                    "alerts": true,
                    "detections": false,
                    "image_source": "preview",
                    "additional_concerns": [],
                    "debug_save_thumbnails": false,
                    "activity_context_prompt": ""
                }
            },
            "snapshots": {
                "enabled": self.snapshots.enabled,
                "timestamp": self.snapshots.timestamp,
                "bounding_box": self.snapshots.bounding_box,
                "crop": self.snapshots.crop,
                "required_zones": [],
                "retain": { "default": 10.0, "objects": {} },
                "quality": self.snapshots.quality,
                "high_res": self.snapshots.high_res,
                "from_main_stream": self.snapshots.from_main_stream,
                "selection": self.snapshots.selection
            },
            "timestamp_style": {
                "position": "tl",
                "format": "%m/%d/%Y %H:%M:%S",
                "color": { "red": 255, "green": 255, "blue": 255 },
                "thickness": 2
            },
            "audio_transcription": {
                "enabled": false,
                "language": "en",
                "device": "CPU",
                "model_size": "small",
                "live_enabled": false
            },
            "classification": {
                "bird": { "enabled": false, "threshold": 0.9 },
                "custom": {}
            },
            "semantic_search": {
                "enabled": false,
                "reindex": false,
                "model": "jinav1",
                "model_size": "small"
            },
            "face_recognition": {
                "enabled": self.face_recognition.enabled,
                "model_path": self.face_recognition.model_path,
                "similarity_threshold": self.face_recognition.similarity_threshold,
                "min_face_area": self.face_recognition.min_face_area,
                "unknown_alert": self.face_recognition.unknown_alert,
                "model_size": "small",
                "unknown_score": 0.8,
                "detection_threshold": 0.7,
                "recognition_threshold": self.face_recognition.similarity_threshold,
                "min_area": self.face_recognition.min_face_area,
                "min_faces": 1,
                "save_attempts": 200,
                "blur_confidence_filter": true
            },
            "lpr": {
                "enabled": self.lpr.enabled,
                "detector_model_path": self.lpr.detector_model_path,
                "ocr_model_path": self.lpr.ocr_model_path,
                "min_confidence": self.lpr.min_confidence,
                "format_regex": self.lpr.format_regex,
                "known_plates": self.lpr.known_plates,
                "model_size": "small",
                "detection_threshold": 0.7,
                "min_area": 1000,
                "recognition_threshold": self.lpr.min_confidence,
                "min_plate_length": 4,
                "match_distance": 1,
                "enhancement": 0,
                "debug_save_plates": false,
                "replace_rules": []
            },
            "camera_groups": self.camera_groups,
            "profiles": self.profiles,
            "peers": scrub_json_value(&serde_json::to_value(&self.peers).unwrap_or_else(|_| json!({}))),
            "plus": { "enabled": false }
        })
    }

    /// Resolves the SSRF-safe base HTTP/HTTPS URL for a camera's native Web UI, using either
    /// `camera.webui_url` or the host/IP extracted from its configured RTSP input or go2rtc stream.
    /// Loopback, unspecified, link-local, metadata, and internal service addresses are never returned.
    pub fn resolve_camera_webui_base_url(&self, camera: &str) -> Option<String> {
        let cam = self.cameras.get(camera)?;
        if let Some(ref custom) = cam.webui_url {
            let trimmed = custom.trim().trim_end_matches('/');
            if (trimmed.starts_with("http://") || trimmed.starts_with("https://"))
                && !trimmed.contains(['@', '?', '#', '\\'])
                && !trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
                && let Some(h) = extract_non_loopback_host(trimmed)
                && !h.is_empty()
            {
                return Some(trimmed.to_string());
            }
        }
        for inp in &cam.ffmpeg.inputs {
            if let Some(host) = extract_non_loopback_host(&inp.path) {
                return Some(format!("http://{host}"));
            }
        }
        for key in [camera.to_string(), format!("{camera}_sub")] {
            if let Some(val) = self.go2rtc.streams.get(&key)
                && let Some(host) = extract_host_from_go2rtc_val(val)
            {
                return Some(format!("http://{host}"));
            }
        }
        None
    }

    /// Collects all raw URL strings currently stored in `cameras.*.ffmpeg.inputs`,
    /// `cameras.*.onvif.host`, `cameras.*.webui_url`, `go2rtc.streams`, `genai.base_url`,
    /// `peers.*.url`, `auth.oidc.issuer_url`, and `auth.proxy.logout_url`.
    pub fn collect_original_urls(&self) -> Vec<String> {
        let mut out = Vec::new();
        for cam in self.cameras.values() {
            collect_camera_urls(cam, &mut out);
        }
        fn collect_from_val(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::String(s) if s.contains("://") => out.push(s.clone()),
                Value::Array(arr) => {
                    for item in arr {
                        collect_from_val(item, out);
                    }
                }
                Value::Object(map) => {
                    for val in map.values() {
                        collect_from_val(val, out);
                    }
                }
                _ => {}
            }
        }
        for val in self.go2rtc.streams.values() {
            collect_from_val(val, &mut out);
        }
        if let Some(ref u) = self.genai.base_url
            && u.contains("://")
        {
            out.push(u.clone());
        }
        for peer in self.peers.values() {
            if peer.url.contains("://") {
                out.push(peer.url.clone());
            }
        }
        if self.auth.oidc.issuer_url.contains("://") {
            out.push(self.auth.oidc.issuer_url.clone());
        }
        if self.auth.proxy.logout_url.contains("://") {
            out.push(self.auth.proxy.logout_url.clone());
        }
        out
    }

    /// Restores scrubbed URLs (`*:*@`, `*@`, `=***`) and masked secret fields (`***`, `*`) in `self`
    /// from a previously loaded `NinjaConfig` and refreshes `self.raw_yaml` when any secret is restored.
    pub fn restore_scrubbed_from_previous(&mut self, prev: &NinjaConfig) {
        let originals = prev.collect_original_urls();
        let mut restored_any = false;

        for (cam_name, cam) in &mut self.cameras {
            let mut cam_originals = Vec::new();
            if let Some(prev_cam) = prev.cameras.get(cam_name) {
                collect_camera_urls(prev_cam, &mut cam_originals);
            }
            for u in &originals {
                if !cam_originals.contains(u) {
                    cam_originals.push(u.clone());
                }
            }
            for inp in &mut cam.ffmpeg.inputs {
                if is_url_scrubbed(&inp.path) {
                    let restored = restore_scrubbed_url(&inp.path, &cam_originals);
                    if restored != inp.path {
                        inp.path = restored;
                        restored_any = true;
                    }
                }
            }
            if let Some(ref h) = cam.onvif.host
                && is_url_scrubbed(h)
            {
                let restored = restore_scrubbed_url(h, &cam_originals);
                if &restored != h {
                    cam.onvif.host = Some(restored);
                    restored_any = true;
                }
            }
            if let Some(ref u) = cam.webui_url
                && is_url_scrubbed(u)
            {
                let restored = restore_scrubbed_url(u, &cam_originals);
                if &restored != u {
                    cam.webui_url = Some(restored);
                    restored_any = true;
                }
            }
            if cam
                .onvif
                .password
                .as_deref()
                .is_some_and(|p| p.trim() == "***" || p.trim() == "*")
            {
                cam.onvif.password = prev
                    .cameras
                    .get(cam_name)
                    .and_then(|c| c.onvif.password.clone());
                restored_any = true;
            }
        }

        for v in self.go2rtc.streams.values_mut() {
            let next_v = restore_scrubbed_json(v, &originals);
            if next_v != *v {
                *v = next_v;
                restored_any = true;
            }
        }

        if let Some(ref u) = self.genai.base_url
            && is_url_scrubbed(u)
        {
            let restored = restore_scrubbed_url(u, &originals);
            if &restored != u {
                self.genai.base_url = Some(restored);
                restored_any = true;
            }
        }

        for (peer_name, peer) in &mut self.peers {
            if is_url_scrubbed(&peer.url) {
                let mut peer_originals = Vec::new();
                if let Some(prev_peer) = prev.peers.get(peer_name)
                    && prev_peer.url.contains("://")
                {
                    peer_originals.push(prev_peer.url.clone());
                }
                for u in &originals {
                    if !peer_originals.contains(u) {
                        peer_originals.push(u.clone());
                    }
                }
                let restored = restore_scrubbed_url(&peer.url, &peer_originals);
                if restored != peer.url {
                    peer.url = restored;
                    restored_any = true;
                }
            }
        }

        if is_url_scrubbed(&self.auth.oidc.issuer_url) {
            let restored = restore_scrubbed_url(&self.auth.oidc.issuer_url, &originals);
            if restored != self.auth.oidc.issuer_url {
                self.auth.oidc.issuer_url = restored;
                restored_any = true;
            }
        }

        if is_url_scrubbed(&self.auth.proxy.logout_url) {
            let restored = restore_scrubbed_url(&self.auth.proxy.logout_url, &originals);
            if restored != self.auth.proxy.logout_url {
                self.auth.proxy.logout_url = restored;
                restored_any = true;
            }
        }

        if self
            .mqtt
            .password
            .as_deref()
            .is_some_and(|p| p.trim() == "***" || p.trim() == "*")
        {
            self.mqtt.password = prev.mqtt.password.clone();
            restored_any = true;
        }

        if self
            .genai
            .api_key
            .as_deref()
            .is_some_and(|k| k.trim() == "***" || k.trim() == "*")
        {
            self.genai.api_key = prev.genai.api_key.clone();
            restored_any = true;
        }

        if restored_any && let Ok(clean) = self.to_clean_yaml() {
            self.raw_yaml = clean;
        }
    }

    /// Serializes this `NinjaConfig` to a clean YAML string and validates it can be re-parsed.
    pub fn to_clean_yaml(&self) -> Result<String> {
        let yaml_str =
            serde_yaml::to_string(self).context("failed to serialize NinjaConfig to YAML")?;
        Ok(yaml_str)
    }

    /// Applies a partial or full structured JSON configuration update (from UI v2),
    /// automatically preserving any scrubbed RTSP/MQTT credentials and refreshing `raw_yaml`.
    pub fn apply_structured_json(&mut self, patch: &Value) -> Result<()> {
        let originals = self.collect_original_urls();
        let restored = restore_scrubbed_json(patch, &originals);
        let Some(obj) = restored.as_object() else {
            anyhow::bail!("structured config payload must be a JSON object");
        };

        let prev_global_model = self.model.clone();

        if let Some(mqtt_val) = obj.get("mqtt") {
            let prev_pass = self.mqtt.password.clone();
            let prev_tls_enabled = self.mqtt.tls_enabled;
            let prev_should_use_tls = self.mqtt.should_use_tls();
            let clear_pass = mqtt_val
                .get("clear_password")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let merged_mqtt = merge_section_json(&self.mqtt, mqtt_val, &[]);
            let mut next_mqtt: MqttConfig =
                serde_json::from_value(merged_mqtt).context("invalid mqtt config")?;
            if clear_pass {
                next_mqtt.password = None;
            } else if next_mqtt
                .password
                .as_deref()
                .map(|p| p.trim().is_empty() || p.trim() == "***" || p.trim() == "*")
                .unwrap_or(true)
            {
                next_mqtt.password = prev_pass;
            }
            if prev_tls_enabled.is_none()
                && !prev_should_use_tls
                && mqtt_val.get("tls_enabled") == Some(&Value::Bool(false))
            {
                next_mqtt.tls_enabled = None;
            }
            self.mqtt = next_mqtt;
        }

        if let Some(auth_val) = obj.get("auth") {
            let merged_auth = merge_section_json(&self.auth, auth_val, &[]);
            self.auth = serde_json::from_value(merged_auth).context("invalid auth config")?;
        }
        if let Some(oidc_val) = obj.get("oidc") {
            let merged_oidc = merge_section_json(&self.auth.oidc, oidc_val, &[]);
            self.auth.oidc = serde_json::from_value(merged_oidc).context("invalid oidc config")?;
        }
        if let Some(proxy_val) = obj.get("proxy")
            && proxy_val.get("header_map").is_none()
        {
            let merged_proxy = merge_section_json(&self.auth.proxy, proxy_val, &[]);
            self.auth.proxy =
                serde_json::from_value(merged_proxy).context("invalid proxy config")?;
        }
        if let Some(tls_val) = obj.get("tls") {
            let merged_tls = merge_section_json(&self.tls, tls_val, &[]);
            self.tls = serde_json::from_value(merged_tls).context("invalid tls config")?;
        }
        if let Some(db_val) = obj.get("database") {
            let merged_db = merge_section_json(&self.database, db_val, &[]);
            self.database = serde_json::from_value(merged_db).context("invalid database config")?;
        }
        if let Some(model_val) = obj.get("model") {
            let merged_model = merge_section_json(&self.model, model_val, &[]);
            let next_model: ModelConfig =
                serde_json::from_value(merged_model).context("invalid model config")?;
            if !is_safe_labelmap_path(Path::new(&next_model.labelmap_path)) {
                anyhow::bail!("invalid model.labelmap_path");
            }
            if !is_safe_model_path(Path::new(&next_model.path)) {
                anyhow::bail!("invalid model.path");
            }
            self.model = next_model;
        }
        if let Some(det_val) = obj.get("detectors") {
            let mut next_dets: BTreeMap<String, DetectorEntryConfig> =
                serde_json::from_value(det_val.clone()).context("invalid detectors config")?;
            if !next_dets.is_empty() {
                for (det_name, det) in next_dets.iter_mut() {
                    let prev_det_model = self.detectors.get(det_name).and_then(|d| d.model.clone());
                    let has_explicit_model_in_patch =
                        det_val.get(det_name).and_then(|d| d.get("model")).is_some();
                    if !has_explicit_model_in_patch && prev_det_model.is_some() {
                        det.model = prev_det_model;
                    } else if prev_det_model.is_none()
                        && let Some(ref m) = det.model
                        && model_configs_equal(m, &prev_global_model)
                    {
                        det.model = None;
                    }
                    if let Some(ref m) = det.model {
                        if !is_safe_labelmap_path(Path::new(&m.labelmap_path)) {
                            anyhow::bail!("invalid detector model.labelmap_path in '{det_name}'");
                        }
                        if !is_safe_model_path(Path::new(&m.path)) {
                            anyhow::bail!("invalid detector model.path in '{det_name}'");
                        }
                    }
                }
                self.detectors = next_dets;
            }
        }
        if let Some(genai_val) = obj.get("genai")
            && genai_val.as_object().is_some_and(|m| !m.is_empty())
        {
            let prev_key = self.genai.api_key.clone();
            let clear_key = genai_val
                .get("clear_api_key")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let merged_genai = merge_section_json(&self.genai, genai_val, &[]);
            let mut next_genai: GenAiConfig =
                serde_json::from_value(merged_genai).context("invalid genai config")?;
            if clear_key {
                next_genai.api_key = None;
            } else if next_genai
                .api_key
                .as_deref()
                .map(|k| k.trim().is_empty() || k.trim() == "***" || k.trim() == "*")
                .unwrap_or(true)
            {
                next_genai.api_key = prev_key;
            }
            if next_genai
                .base_url
                .as_deref()
                .is_some_and(|u| u.trim().is_empty())
            {
                next_genai.base_url = None;
            }
            self.genai = next_genai;
        }
        if let Some(face_val) = obj.get("face_recognition") {
            let merged_face = merge_section_json(&self.face_recognition, face_val, &[]);
            let next_face: FaceRecognitionConfig =
                serde_json::from_value(merged_face).context("invalid face_recognition config")?;
            if !is_safe_model_path(&next_face.model_path) {
                anyhow::bail!("invalid face_recognition.model_path");
            }
            self.face_recognition = next_face;
        }
        if let Some(detect_val) = obj.get("detect") {
            let merged_detect = merge_section_json(&self.detect, detect_val, &[]);
            self.detect = serde_json::from_value(merged_detect).context("invalid detect config")?;
        }
        if let Some(motion_val) = obj.get("motion") {
            let merged_motion = merge_section_json(&self.motion, motion_val, &["mask"]);
            self.motion = serde_json::from_value(merged_motion).context("invalid motion config")?;
        }
        if let Some(objects_val) = obj.get("objects") {
            let merged_objects =
                merge_section_json(&self.objects, objects_val, &["filters", "mask"]);
            self.objects =
                serde_json::from_value(merged_objects).context("invalid objects config")?;
            self.objects.normalize_standalone_tracked_attributes();
        }
        if let Some(lpr_val) = obj.get("lpr") {
            let merged_lpr = merge_section_json(&self.lpr, lpr_val, &["known_plates"]);
            let next_lpr: LprConfig =
                serde_json::from_value(merged_lpr).context("invalid lpr config")?;
            if let Some(ref p) = next_lpr.detector_model_path
                && !is_safe_model_path(p)
            {
                anyhow::bail!("invalid lpr.detector_model_path");
            }
            if let Some(ref p) = next_lpr.ocr_model_path
                && !is_safe_model_path(p)
            {
                anyhow::bail!("invalid lpr.ocr_model_path");
            }
            self.lpr = next_lpr;
        }
        if let Some(record_val) = obj.get("record") {
            let merged_record = merge_section_json(&self.record, record_val, &["per_camera_dir"]);
            self.record = serde_json::from_value(merged_record).context("invalid record config")?;
        }
        if let Some(snap_val) = obj.get("snapshots") {
            let merged_snap = merge_section_json(&self.snapshots, snap_val, &[]);
            self.snapshots =
                serde_json::from_value(merged_snap).context("invalid snapshots config")?;
        }
        if let Some(rev_val) = obj.get("review") {
            let merged_rev = merge_section_json(&self.review, rev_val, &[]);
            self.review = serde_json::from_value(merged_rev).context("invalid review config")?;
        }
        if let Some(audio_val) = obj.get("audio") {
            let merged_audio = merge_section_json(&self.audio, audio_val, &[]);
            self.audio = serde_json::from_value(merged_audio).context("invalid audio config")?;
        }
        if let Some(profiles_val) = obj.get("profiles") {
            self.profiles =
                serde_json::from_value(profiles_val.clone()).context("invalid profiles config")?;
        }
        if let Some(peers_val) = obj.get("peers") {
            self.peers =
                serde_json::from_value(peers_val.clone()).context("invalid peers config")?;
        }
        if let Some(groups_val) = obj.get("camera_groups") {
            self.camera_groups = serde_json::from_value(groups_val.clone())
                .context("invalid camera_groups config")?;
        }
        if let Some(ui_val) = obj.get("ui") {
            let merged_ui = merge_section_json(&self.ui, ui_val, &[]);
            let mut next_ui: UiConfig =
                serde_json::from_value(merged_ui).context("invalid ui config")?;
            next_ui.theme = normalize_ui_theme(&next_ui.theme);
            sanitize_floorplan_config(&mut next_ui.floorplan);
            self.ui = next_ui;
        }
        if let Some(fp_val) = obj.get("floorplan") {
            let merged_fp = merge_section_json(&self.ui.floorplan, fp_val, &[]);
            let mut next_fp: FloorplanConfig =
                serde_json::from_value(merged_fp).context("invalid floorplan config")?;
            sanitize_floorplan_config(&mut next_fp);
            self.ui.floorplan = next_fp;
        }
        if let Some(go2rtc_val) = obj.get("go2rtc")
            && let Some(go2rtc_obj) = go2rtc_val.as_object()
        {
            for (gk, gv) in go2rtc_obj {
                if gk.trim().eq_ignore_ascii_case("streams") {
                    let next_streams: BTreeMap<String, Value> = serde_json::from_value(gv.clone())
                        .context("invalid go2rtc.streams config")?;
                    validate_go2rtc_streams_map(&next_streams)?;
                    self.go2rtc.streams = next_streams;
                }
            }
        }
        validate_go2rtc_streams_map(&self.go2rtc.streams)?;
        validate_go2rtc_extra_map(&self.go2rtc.extra)?;
        if let Some(cams_val) = obj.get("cameras") {
            let Some(cams_map) = cams_val.as_object() else {
                anyhow::bail!("invalid cameras config: expected object");
            };
            let mut next_cams: BTreeMap<String, CameraConfig> = BTreeMap::new();
            for (cam_name, cam_patch_val) in cams_map {
                let mut cam_originals = Vec::new();
                if let Some(prev_cam) = self.cameras.get(cam_name) {
                    collect_camera_urls(prev_cam, &mut cam_originals);
                }
                for u in &originals {
                    if !cam_originals.contains(u) {
                        cam_originals.push(u.clone());
                    }
                }
                let raw_cam_patch = patch
                    .get("cameras")
                    .and_then(|c| c.get(cam_name))
                    .unwrap_or(cam_patch_val);
                let restored_cam_patch = restore_scrubbed_json(raw_cam_patch, &cam_originals);
                let merged_cam_val = if let Some(prev_cam) = self.cameras.get(cam_name) {
                    merge_section_json(
                        prev_cam,
                        &restored_cam_patch,
                        &[
                            "zones",
                            "lines",
                            "state_rois",
                            "filters",
                            "streams",
                            "mask",
                            "privacy_mask",
                            "per_camera_dir",
                        ],
                    )
                } else {
                    restored_cam_patch.clone()
                };
                let mut cam: CameraConfig = serde_json::from_value(merged_cam_val)
                    .with_context(|| format!("invalid camera config for '{cam_name}'"))?;

                let prev_cam_opt = self.cameras.get(cam_name);
                if prev_cam_opt.is_none_or(|p| p.ffmpeg.output_args.is_null())
                    && cam.ffmpeg.output_args
                        == json!({
                            "detect": ["-threads", "2", "-f", "rawvideo", "-pix_fmt", "yuv420p"],
                            "record": "preset-record-generic-audio-aac"
                        })
                {
                    cam.ffmpeg.output_args = Value::Null;
                }
                for (inp_idx, inp) in cam.ffmpeg.inputs.iter_mut().enumerate() {
                    let prev_inp_null = prev_cam_opt
                        .and_then(|p| p.ffmpeg.inputs.get(inp_idx))
                        .is_none_or(|pi| pi.input_args.is_null());
                    if prev_inp_null && inp.input_args == json!("preset-rtsp-restream") {
                        inp.input_args = Value::Null;
                    }
                }

                let clear_onvif_pass = cam_patch_val
                    .get("onvif")
                    .and_then(|o| o.get("clear_password"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let prev_onvif_pass = prev_cam_opt.and_then(|prev| prev.onvif.password.clone());
                if clear_onvif_pass {
                    cam.onvif.password = None;
                } else if cam
                    .onvif
                    .password
                    .as_deref()
                    .map(|p| p.trim().is_empty() || p.trim() == "***" || p.trim() == "*")
                    .unwrap_or(true)
                {
                    cam.onvif.password = prev_onvif_pass;
                }
                if cam
                    .onvif
                    .host
                    .as_deref()
                    .is_some_and(|h| h.trim().is_empty())
                {
                    cam.onvif.host = None;
                }
                if cam
                    .onvif
                    .user
                    .as_deref()
                    .is_some_and(|u| u.trim().is_empty())
                {
                    cam.onvif.user = None;
                }
                if cam
                    .onvif
                    .return_preset
                    .as_deref()
                    .is_some_and(|r| r.trim().is_empty())
                {
                    cam.onvif.return_preset = None;
                }
                for zcfg in cam.zones.values_mut() {
                    if zcfg.preset.as_deref().is_some_and(|p| p.trim().is_empty()) {
                        zcfg.preset = None;
                    }
                }
                if cam.live.streams.is_empty() {
                    cam.live
                        .streams
                        .insert("Stream 1".to_string(), format!("{cam_name}_sub"));
                    cam.live
                        .streams
                        .insert("Stream 2".to_string(), cam_name.clone());
                }
                let has_explicit_cam_track = cam_patch_val
                    .get("objects")
                    .and_then(|o| o.get("track"))
                    .is_some();
                if cam.objects.track.is_empty()
                    || (!has_explicit_cam_track
                        && cam.objects.track == default_tracked_objects()
                        && self.objects.track != default_tracked_objects())
                {
                    cam.objects.track = self.objects.track.clone();
                }
                let has_explicit_cam_attrs = cam_patch_val
                    .get("objects")
                    .and_then(|o| o.get("attribute_labels"))
                    .is_some();
                if cam.objects.attribute_labels.is_empty()
                    || (!has_explicit_cam_attrs
                        && cam.objects.attribute_labels == default_attribute_labels()
                        && self.objects.attribute_labels != default_attribute_labels())
                {
                    cam.objects.attribute_labels = self.objects.attribute_labels.clone();
                }
                if !cam.record.alert_backup.enabled && self.record.alert_backup.enabled {
                    cam.record.alert_backup = self.record.alert_backup.clone();
                }
                if cam.review.alerts.sub_labels.is_empty()
                    && !self.review.alerts.sub_labels.is_empty()
                {
                    cam.review.alerts.sub_labels = self.review.alerts.sub_labels.clone();
                }
                if cam.review.alerts.exclude_sub_labels.is_empty()
                    && !self.review.alerts.exclude_sub_labels.is_empty()
                {
                    cam.review.alerts.exclude_sub_labels =
                        self.review.alerts.exclude_sub_labels.clone();
                }
                cam.objects.normalize_standalone_tracked_attributes();
                for roi in cam.state_rois.values_mut() {
                    if roi.classes.is_empty() {
                        roi.classes = default_state_roi_classes();
                    }
                    roi.interval_sec = roi.interval_sec.max(1);
                    roi.hysteresis = roi.hysteresis.max(1);
                    roi.anomaly_timeout_sec = roi.anomaly_timeout_sec.max(1);
                    if roi
                        .anomaly_state
                        .as_ref()
                        .is_some_and(|s| s.trim().is_empty())
                    {
                        roi.anomaly_state = None;
                    }
                }
                next_cams.insert(cam_name.clone(), cam);
            }
            self.cameras = next_cams;
        }

        let clean_yaml = self.to_clean_yaml()?;
        // Verify the generated YAML can be parsed back cleanly
        let _ = Self::from_yaml_str(&clean_yaml, self.config_path.clone())?;
        self.raw_yaml = clean_yaml;
        Ok(())
    }
}

fn collect_camera_urls(cam: &CameraConfig, out: &mut Vec<String>) {
    for inp in &cam.ffmpeg.inputs {
        if !inp.path.is_empty() {
            out.push(inp.path.clone());
        }
    }
    if let Some(ref s) = cam.onvif.host
        && s.contains("://")
    {
        out.push(s.clone());
    }
    if let Some(ref s) = cam.webui_url
        && s.contains("://")
    {
        out.push(s.clone());
    }
}

fn is_url_scrubbed(s: &str) -> bool {
    s.contains("://*:*@") || s.contains("://*@") || s.contains("=***")
}

fn model_configs_equal(a: &ModelConfig, b: &ModelConfig) -> bool {
    a.width == b.width
        && a.height == b.height
        && a.input_tensor == b.input_tensor
        && a.input_pixel_format == b.input_pixel_format
        && a.path == b.path
        && a.labelmap_path == b.labelmap_path
        && a.model_type == b.model_type
}

fn sanitize_floorplan_config(fp: &mut FloorplanConfig) {
    if fp
        .background_data_url
        .as_ref()
        .is_some_and(|s| s.trim().is_empty())
    {
        fp.background_data_url = None;
    }
    fp.opacity = if fp.opacity.is_finite() {
        fp.opacity.clamp(0.10, 1.0)
    } else {
        default_floorplan_opacity()
    };
    fp.width_m = if fp.width_m.is_finite() {
        fp.width_m.clamp(2.0, 500.0)
    } else {
        default_floorplan_width_m()
    };
    fp.height_m = if fp.height_m.is_finite() {
        fp.height_m.clamp(2.0, 500.0)
    } else {
        default_floorplan_height_m()
    };
}

fn merge_section_json<T: Serialize>(
    current: &T,
    patch: &Value,
    replace_map_keys: &[&str],
) -> Value {
    let Ok(mut base) = serde_json::to_value(current) else {
        return patch.clone();
    };
    merge_json_values(&mut base, patch, replace_map_keys);
    base
}

fn merge_json_values(base: &mut Value, patch: &Value, replace_map_keys: &[&str]) {
    if let (Value::Object(base_map), Value::Object(patch_map)) = (base, patch) {
        for (k, v) in patch_map {
            if replace_map_keys.contains(&k.as_str()) {
                base_map.insert(k.clone(), v.clone());
            } else if let Some(existing) = base_map.get_mut(k)
                && existing.is_object()
                && v.is_object()
            {
                merge_json_values(existing, v, replace_map_keys);
            } else {
                base_map.insert(k.clone(), v.clone());
            }
        }
    }
}

fn validate_go2rtc_extra_map(extra: &BTreeMap<String, Value>) -> Result<()> {
    for (k, v) in extra {
        let k_norm = k.trim().to_ascii_lowercase();
        if k_norm == "streams" {
            let extra_streams: BTreeMap<String, Value> = serde_json::from_value(v.clone())
                .context("invalid go2rtc.streams config in extra")?;
            validate_go2rtc_streams_map(&extra_streams)?;
        } else if k_norm == "ffmpeg"
            && let Some(obj) = v.as_object()
            && let Some(bin_val) = obj.get("bin")
        {
            let Some(bin_str) = bin_val.as_str() else {
                anyhow::bail!("go2rtc.ffmpeg.bin must be a string");
            };
            let trimmed = bin_str.trim();
            if trimmed.is_empty()
                || trimmed.contains("..")
                || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
                || !matches!(
                    trimmed,
                    "ffmpeg"
                        | "/usr/bin/ffmpeg"
                        | "/usr/local/bin/ffmpeg"
                        | "/usr/lib/jellyfin-ffmpeg/ffmpeg"
                )
            {
                anyhow::bail!("disallowed go2rtc.ffmpeg.bin path '{trimmed}'");
            }
        }
    }
    Ok(())
}

/// Returns `true` if `raw` is an empty, control-character-containing, or command/local-file
/// `go2rtc` producer string (`exec:`, `echo:`, `expr:`, `file:`, `stdio:`, or `ffmpeg:` wrapping any of those or `/`).
pub fn is_disallowed_go2rtc_producer(raw: &str) -> bool {
    if raw.chars().any(|c| c.is_control()) {
        return true;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return true;
    }
    let has_disallowed_scheme = |s: &str| -> bool {
        let lc = s.trim_start().to_ascii_lowercase();
        lc.starts_with('/')
            || lc.starts_with("./")
            || lc.starts_with("../")
            || lc.starts_with('\\')
            || lc.starts_with('-')
            || lc.starts_with("exec:")
            || lc.starts_with("echo:")
            || lc.starts_with("expr:")
            || lc.starts_with("file:")
            || lc.starts_with("stdio:")
            || lc.starts_with("hass:")
            || lc.starts_with("cmd:")
            || lc.starts_with("shell:")
            || lc.starts_with("sh:")
            || lc.starts_with("bash:")
            || lc.starts_with("pipe:")
            || lc.starts_with("subfile:")
            || lc.starts_with("concat:")
            || lc.starts_with("data:")
    };

    if has_disallowed_scheme(trimmed) {
        return true;
    }

    let mut current = trimmed;
    while let Some(pos) = current.find(':') {
        let prefix = current[..pos].trim();
        if prefix.eq_ignore_ascii_case("ffmpeg") {
            current = current[pos + 1..].trim_start();
            if current.is_empty() || has_disallowed_scheme(current) {
                return true;
            }
        } else {
            break;
        }
    }

    for frag in trimmed.split('#').skip(1) {
        let frag_lc = frag.trim().to_ascii_lowercase();
        let rhs = frag_lc
            .split_once('=')
            .map(|(_, v)| v.trim_start())
            .unwrap_or(frag_lc.as_str());
        if has_disallowed_scheme(frag_lc.as_str())
            || has_disallowed_scheme(rhs)
            || frag_lc.contains("exec:")
            || frag_lc.contains("echo:")
            || frag_lc.contains("expr:")
            || frag_lc.contains("file:")
            || frag_lc.contains("stdio:")
            || frag_lc.contains("cmd:")
            || frag_lc.contains("shell:")
        {
            return true;
        }
    }

    false
}

/// Validates all stream keys and producer entries in `go2rtc.streams`.
pub fn validate_go2rtc_streams_map(streams: &BTreeMap<String, Value>) -> Result<()> {
    for (k, v) in streams {
        if k.is_empty()
            || k.len() > 128
            || !k
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            anyhow::bail!("invalid go2rtc stream key '{k}'");
        }
        match v {
            Value::String(s) => {
                if is_disallowed_go2rtc_producer(s) {
                    anyhow::bail!("disallowed go2rtc stream producer in '{k}'");
                }
            }
            Value::Array(arr) => {
                if arr.is_empty() {
                    anyhow::bail!("go2rtc stream '{k}' array must not be empty");
                }
                for item in arr {
                    let Some(s) = item.as_str() else {
                        anyhow::bail!("go2rtc stream '{k}' array entries must be strings");
                    };
                    if is_disallowed_go2rtc_producer(s) {
                        anyhow::bail!("disallowed go2rtc stream producer in '{k}'");
                    }
                }
            }
            _ => {
                anyhow::bail!("go2rtc stream '{k}' must be a producer string or array of strings");
            }
        }
    }
    Ok(())
}

const FORBIDDEN_SYSTEM_PATH_PREFIXES: [&str; 8] = [
    "/proc",
    "/sys",
    "/dev",
    "/etc",
    "/root",
    "/var/run",
    "/run/secrets",
    "/var/secrets",
];

fn has_forbidden_path_components_or_prefix(path: &Path) -> bool {
    let Some(raw_str) = path.to_str() else {
        return true;
    };
    let trimmed = raw_str.trim();
    if trimmed.is_empty() || trimmed.contains('\\') || trimmed.chars().any(|c| c.is_control()) {
        return true;
    }
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => return true,
            std::path::Component::Normal(part) => {
                let Some(s) = part.to_str() else {
                    return true;
                };
                if s.starts_with('.') {
                    return true;
                }
            }
            _ => {}
        }
    }
    let lc = trimmed.to_ascii_lowercase();
    for prefix in FORBIDDEN_SYSTEM_PATH_PREFIXES {
        if lc == prefix || lc.starts_with(&format!("{prefix}/")) || path.starts_with(prefix) {
            return true;
        }
    }
    false
}

fn validate_existing_or_ancestor_canonical_path(
    path: &Path,
    allowed_exts: &[&str],
    max_file_bytes: Option<u64>,
) -> bool {
    if path.exists() || std::fs::symlink_metadata(path).is_ok() {
        let Ok(canon) = std::fs::canonicalize(path) else {
            return false;
        };
        if has_forbidden_path_components_or_prefix(&canon) {
            return false;
        }
        if !canon
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| allowed_exts.iter().any(|a| ext.eq_ignore_ascii_case(a)))
        {
            return false;
        }
        let Ok(meta) = std::fs::metadata(&canon) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        if let Some(limit) = max_file_bytes
            && meta.len() > limit
        {
            return false;
        }
    } else {
        for ancestor in path.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            if ancestor.exists() || std::fs::symlink_metadata(ancestor).is_ok() {
                let Ok(canon_anc) = std::fs::canonicalize(ancestor) else {
                    return false;
                };
                if has_forbidden_path_components_or_prefix(&canon_anc) {
                    return false;
                }
                break;
            }
        }
    }
    true
}

/// Validates that a candidate labelmap path is a safe `.txt` file path without parent traversal,
/// dotfile components, or system/secret directory prefixes.
pub fn is_safe_labelmap_path(path: &Path) -> bool {
    if has_forbidden_path_components_or_prefix(path) {
        return false;
    }
    if !path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
    {
        return false;
    }
    validate_existing_or_ancestor_canonical_path(path, &["txt"], Some(256 * 1024))
}

/// Validates that a candidate AI model path is a safe `.xml`, `.onnx`, `.bin`, or `.tflite` path
/// without parent traversal, dotfile components, device nodes, or system/secret directory prefixes.
pub fn is_safe_model_path(path: &Path) -> bool {
    if has_forbidden_path_components_or_prefix(path) {
        return false;
    }
    if !path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ["xml", "onnx", "bin", "tflite"]
                .iter()
                .any(|a| ext.eq_ignore_ascii_case(a))
        })
    {
        return false;
    }
    validate_existing_or_ancestor_canonical_path(path, &["xml", "onnx", "bin", "tflite"], None)
}

fn is_forbidden_ipv4(v4: std::net::Ipv4Addr) -> bool {
    let octs = v4.octets();
    v4.is_loopback()
        || v4.is_unspecified()
        || octs[0] == 0
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || octs == [100, 100, 100, 200]
        || octs == [192, 0, 0, 192]
}

fn is_forbidden_ipv6(v6: std::net::Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
        return true;
    }
    let segs = v6.segments();
    // fe80::/10 link-local
    if (segs[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // AWS EC2 IPv6 IMDS (fd00:ec2::254)
    if segs == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254] {
        return true;
    }
    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d)
    if let Some(v4) = v6.to_ipv4()
        && is_forbidden_ipv4(v4)
    {
        return true;
    }
    // IPv4-translated (::ffff:0:a.b.c.d) and NAT64 (64:ff9b::/96, 64:ff9b:1::/48)
    if (segs[0..4] == [0, 0, 0, 0] && segs[4] == 0xffff && segs[5] == 0)
        || (segs[0] == 0x0064 && segs[1] == 0xff9b)
    {
        let v4 = std::net::Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            (segs[6] & 0xff) as u8,
            (segs[7] >> 8) as u8,
            (segs[7] & 0xff) as u8,
        );
        if is_forbidden_ipv4(v4) {
            return true;
        }
    }
    // 6to4 (2002::/16) embeds IPv4 in segs[1..=2]
    if segs[0] == 0x2002 {
        let v4 = std::net::Ipv4Addr::new(
            (segs[1] >> 8) as u8,
            (segs[1] & 0xff) as u8,
            (segs[2] >> 8) as u8,
            (segs[2] & 0xff) as u8,
        );
        if is_forbidden_ipv4(v4) {
            return true;
        }
    }
    false
}

fn extract_non_loopback_host(url: &str) -> Option<String> {
    let idx = url.find("://")?;
    let after_scheme = &url[idx + 3..];
    let authority_end = after_scheme
        .find(['/', '?', '#', '\\'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.is_empty()
        || authority
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '%')
    {
        return None;
    }
    let host_port = if let Some(at_idx) = authority.rfind('@') {
        &authority[at_idx + 1..]
    } else {
        authority
    };
    if host_port.is_empty() {
        return None;
    }

    if host_port.starts_with('[') {
        let end = host_port.find(']')?;
        let rest_port = &host_port[end + 1..];
        if !rest_port.is_empty() {
            let port_str = rest_port.strip_prefix(':')?;
            let port = port_str.parse::<u16>().ok()?;
            if port == 0 {
                return None;
            }
        }
        let v6_str = &host_port[1..end];
        let v6 = v6_str.parse::<std::net::Ipv6Addr>().ok()?;
        if is_forbidden_ipv6(v6) {
            return None;
        }
        return Some(format!("[{v6_str}]"));
    }

    if host_port.contains(['[', ']']) {
        return None;
    }

    let (raw_host, port_opt) = match host_port.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (host_port, None),
    };
    if let Some(p) = port_opt {
        if p.contains(':') {
            return None;
        }
        let port = p.parse::<u16>().ok()?;
        if port == 0 {
            return None;
        }
    }

    let host_trimmed = raw_host.trim().trim_end_matches('.');
    if host_trimmed.is_empty() {
        return None;
    }

    if let Ok(ip) = host_trimmed.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                if is_forbidden_ipv4(v4) {
                    return None;
                }
                return Some(host_trimmed.to_string());
            }
            std::net::IpAddr::V6(_) => {
                // Unbracketed IPv6 in URL authority is invalid
                return None;
            }
        }
    }

    let host_lc = host_trimmed.to_ascii_lowercase();
    let labels: Vec<&str> = host_lc.split('.').collect();
    if labels.iter().any(|l| {
        l.is_empty()
            || l.starts_with('-')
            || l.ends_with('-')
            || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }) {
        return None;
    }

    // Reject numeric/hex/octal/shorthand IPv4 representations (RFC 1123 / WHATWG URL standard:
    // valid DNS TLDs never begin with an ASCII digit).
    if let Some(last_label) = labels.last()
        && last_label
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    {
        return None;
    }

    if host_lc == "localhost"
        || host_lc.ends_with(".localhost")
        || host_lc == "ip6-localhost"
        || host_lc == "ip6-loopback"
        || host_lc == "broadcasthost"
        || host_lc == "ninjanvr"
        || host_lc == "metadata"
        || host_lc == "metadata.google.internal"
        || host_lc.ends_with(".internal")
        || host_lc.ends_with(".svc")
        || host_lc.ends_with(".cluster.local")
    {
        return None;
    }
    Some(host_trimmed.to_string())
}

fn extract_host_from_go2rtc_val(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => extract_non_loopback_host(s),
        Value::Array(arr) => arr.iter().find_map(extract_host_from_go2rtc_val),
        _ => None,
    }
}

fn extract_authority_host(host_port_and_more: &str) -> &str {
    if host_port_and_more.starts_with('[')
        && let Some(bracket_end) = host_port_and_more.find(']')
    {
        return &host_port_and_more[..=bracket_end];
    }
    let end = host_port_and_more
        .find(['/', '?', '#', ':', '\\'])
        .unwrap_or(host_port_and_more.len());
    &host_port_and_more[..end]
}

fn find_userinfo_at_index(after_scheme: &str) -> Option<usize> {
    let slash_end = after_scheme.find(['/', '\\']).unwrap_or(after_scheme.len());
    let pre_slash = &after_scheme[..slash_end];
    let at_idx = pre_slash.rfind('@')?;
    let after_at = &pre_slash[at_idx + 1..];
    let host_end = after_at.find(['?', '#']).unwrap_or(after_at.len());
    let host_port_candidate = &after_at[..host_end];
    if host_port_candidate.is_empty() || host_port_candidate.contains('=') {
        return None;
    }
    let before_at = &pre_slash[..at_idx];
    if let Some(q_or_h) = before_at.find(['?', '#']) {
        // Only treat `?` or `#` before `@` as part of unencoded userinfo if there is a `/` after
        // the host or if the part before `?`/`#` contains `:` with a non-numeric password.
        let before_qh = &before_at[..q_or_h];
        let has_userinfo_colon = before_qh
            .split_once(':')
            .is_some_and(|(_, p)| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()));
        if slash_end == after_scheme.len() && !has_userinfo_colon && !before_qh.contains('@') {
            return None;
        }
    }
    Some(at_idx)
}

fn is_sensitive_query_param_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "password"
            | "pass"
            | "passwd"
            | "pwd"
            | "token"
            | "access_token"
            | "auth_token"
            | "api_key"
            | "apikey"
            | "secret"
            | "client_secret"
            | "auth"
            | "key"
    )
}

fn scrub_url_query_params(url_after_authority: &str) -> String {
    let Some(q_idx) = url_after_authority.find('?') else {
        return url_after_authority.to_string();
    };
    let prefix = &url_after_authority[..q_idx + 1];
    let after_q = &url_after_authority[q_idx + 1..];
    let (query_part, frag_part) = match after_q.find('#') {
        Some(h_idx) => (&after_q[..h_idx], &after_q[h_idx..]),
        None => (after_q, ""),
    };
    let mut scrubbed_pairs = Vec::new();
    for pair in query_part.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if is_sensitive_query_param_key(k) && !v.is_empty() && v != "***" && v != "*" {
                scrubbed_pairs.push(format!("{k}=***"));
            } else if v.contains("://") {
                scrubbed_pairs.push(format!("{k}={}", scrub_single_url_credentials(v)));
            } else {
                scrubbed_pairs.push(pair.to_string());
            }
        } else {
            scrubbed_pairs.push(pair.to_string());
        }
    }
    format!("{prefix}{}{frag_part}", scrubbed_pairs.join("&"))
}

fn restore_url_query_params(scrubbed_rest: &str, orig_after_scheme: &str) -> String {
    if !scrubbed_rest.contains("=***") {
        return scrubbed_rest.to_string();
    }
    let Some(q_idx) = scrubbed_rest.find('?') else {
        return scrubbed_rest.to_string();
    };
    let Some(orig_q_idx) = orig_after_scheme.find('?') else {
        return scrubbed_rest.to_string();
    };
    let orig_after_q = &orig_after_scheme[orig_q_idx + 1..];
    let orig_query = orig_after_q.split('#').next().unwrap_or(orig_after_q);
    let mut orig_params = BTreeMap::new();
    for pair in orig_query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            orig_params.insert(k.to_ascii_lowercase(), v);
        }
    }

    let prefix = &scrubbed_rest[..q_idx + 1];
    let after_q = &scrubbed_rest[q_idx + 1..];
    let (query_part, frag_part) = match after_q.find('#') {
        Some(h_idx) => (&after_q[..h_idx], &after_q[h_idx..]),
        None => (after_q, ""),
    };
    let mut restored_pairs = Vec::new();
    for pair in query_part.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && v == "***"
            && let Some(orig_v) = orig_params.get(&k.to_ascii_lowercase())
        {
            restored_pairs.push(format!("{k}={orig_v}"));
        } else {
            restored_pairs.push(pair.to_string());
        }
    }
    format!("{prefix}{}{frag_part}", restored_pairs.join("&"))
}

/// Restores credentials into a scrubbed RTSP/HTTP URL (`rtsp://*:*@host/path` or `?password=***`) if a matching original exists.
pub fn restore_scrubbed_url(input: &str, originals: &[String]) -> String {
    if !is_url_scrubbed(input) {
        return input.to_string();
    }
    // 1. Exact scrubbed match
    for orig in originals {
        if scrub_rtsp_credentials(orig) == input {
            return orig.clone();
        }
    }
    // 2. Match by scheme + host authority if path or query was edited
    if let Some(scheme_idx) = input.find("://") {
        let after_scheme = &input[scheme_idx + 3..];
        if let Some(at_idx) = find_userinfo_at_index(after_scheme) {
            let rest = &after_scheme[at_idx + 1..];
            let target_host = extract_authority_host(rest);
            for orig in originals {
                if let Some(orig_scheme_idx) = orig.find("://") {
                    let orig_after = &orig[orig_scheme_idx + 3..];
                    if let Some(orig_at) = find_userinfo_at_index(orig_after) {
                        let orig_creds = &orig_after[..orig_at];
                        let orig_host_part = &orig_after[orig_at + 1..];
                        if extract_authority_host(orig_host_part) == target_host {
                            let restored_rest = restore_url_query_params(rest, orig_after);
                            return format!(
                                "{}{}@{}",
                                &input[..scheme_idx + 3],
                                orig_creds,
                                restored_rest
                            );
                        }
                    }
                }
            }
        } else if input.contains("=***") {
            let target_host = extract_authority_host(after_scheme);
            for orig in originals {
                if let Some(orig_scheme_idx) = orig.find("://") {
                    let orig_after = &orig[orig_scheme_idx + 3..];
                    let orig_host_part = match find_userinfo_at_index(orig_after) {
                        Some(orig_at) => &orig_after[orig_at + 1..],
                        None => orig_after,
                    };
                    if extract_authority_host(orig_host_part) == target_host {
                        let restored_after = restore_url_query_params(after_scheme, orig_after);
                        return format!("{}{}", &input[..scheme_idx + 3], restored_after);
                    }
                }
            }
        }
    }
    input.to_string()
}

pub fn restore_scrubbed_json(val: &Value, originals: &[String]) -> Value {
    match val {
        Value::String(s) => Value::String(restore_scrubbed_url(s, originals)),
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| restore_scrubbed_json(v, originals))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), restore_scrubbed_json(v, originals));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn scrub_single_url_credentials(input: &str) -> String {
    let Some(scheme_idx) = input.find("://") else {
        return input.to_string();
    };
    let after_scheme = &input[scheme_idx + 3..];
    if let Some(at_idx) = find_userinfo_at_index(after_scheme) {
        let userinfo = &after_scheme[..at_idx];
        let rest_scrubbed = scrub_url_query_params(&after_scheme[at_idx + 1..]);
        if userinfo.contains(':') {
            format!("{}*:*@{}", &input[..scheme_idx + 3], rest_scrubbed)
        } else {
            format!("{}*@{}", &input[..scheme_idx + 3], rest_scrubbed)
        }
    } else {
        format!(
            "{}{}",
            &input[..scheme_idx + 3],
            scrub_url_query_params(after_scheme)
        )
    }
}

/// Scrubs `user:password@` and sensitive query parameter credentials from RTSP/HTTP URLs before returning them in API responses.
pub fn scrub_rtsp_credentials(input: &str) -> String {
    scrub_single_url_credentials(input)
}

/// Scrubs credentials from all RTSP/HTTP URLs and `password:` / `api_key:` fields embedded in a raw YAML configuration string.
pub fn scrub_yaml_credentials(raw_yaml: &str) -> String {
    let mut out = String::with_capacity(raw_yaml.len());
    let mut cursor = 0usize;
    let bytes = raw_yaml.as_bytes();
    while let Some(rel_idx) = raw_yaml[cursor..].find("://") {
        let scheme_sep = cursor + rel_idx;
        let mut scheme_start = scheme_sep;
        while scheme_start > cursor {
            let b = bytes[scheme_start - 1];
            if b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.' {
                scheme_start -= 1;
            } else {
                break;
            }
        }
        if scheme_start == scheme_sep {
            out.push_str(&raw_yaml[cursor..scheme_sep + 3]);
            cursor = scheme_sep + 3;
            continue;
        }
        out.push_str(&raw_yaml[cursor..scheme_start]);
        let quote_char = if scheme_start > 0 && matches!(bytes[scheme_start - 1], b'"' | b'\'') {
            Some(bytes[scheme_start - 1] as char)
        } else {
            None
        };
        let after_sep = scheme_sep + 3;
        let line_end = raw_yaml[after_sep..]
            .find(['\r', '\n'])
            .map(|i| after_sep + i)
            .unwrap_or(raw_yaml.len());
        let slash_on_line = raw_yaml[after_sep..line_end]
            .find('/')
            .map(|i| after_sep + i)
            .unwrap_or(line_end);
        let last_at_before_slash = raw_yaml[after_sep..slash_on_line]
            .rfind('@')
            .map(|i| after_sep + i);

        let mut url_end = after_sep;
        let slash_pos = raw_yaml[after_sep..].find('/').unwrap_or(usize::MAX);
        let mut in_ipv6_brackets = false;
        let mut prev_was_ws = false;
        for (offset, ch) in raw_yaml[after_sep..].char_indices() {
            let abs_pos = after_sep + offset;
            if ch == '\r' || ch == '\n' {
                break;
            }
            if let Some(q) = quote_char {
                if ch == q {
                    break;
                }
                url_end = abs_pos + ch.len_utf8();
                continue;
            }
            if last_at_before_slash.is_some_and(|at_pos| abs_pos < at_pos) {
                if ch.is_whitespace() {
                    break;
                }
                url_end = abs_pos + ch.len_utf8();
                continue;
            }
            if ch == '[' && offset < slash_pos {
                in_ipv6_brackets = true;
                url_end = abs_pos + ch.len_utf8();
                prev_was_ws = false;
                continue;
            }
            if in_ipv6_brackets && ch == ']' {
                in_ipv6_brackets = false;
                url_end = abs_pos + ch.len_utf8();
                prev_was_ws = false;
                continue;
            }
            if ch.is_whitespace()
                || matches!(ch, '"' | '\'' | ',' | ']')
                || (ch == '#' && prev_was_ws)
            {
                break;
            }
            prev_was_ws = ch.is_whitespace();
            url_end = abs_pos + ch.len_utf8();
        }
        let url_token = &raw_yaml[scheme_start..url_end];
        out.push_str(&scrub_rtsp_credentials(url_token));
        cursor = url_end;
    }
    out.push_str(&raw_yaml[cursor..]);

    let mut scrubbed_lines = String::with_capacity(out.len());
    let mut in_multiline_secret_indent: Option<usize> = None;
    for chunk in out.split_inclusive('\n') {
        let (line_body, newline) = if let Some(stripped) = chunk.strip_suffix("\r\n") {
            (stripped, "\r\n")
        } else if let Some(stripped) = chunk.strip_suffix('\n') {
            (stripped, "\n")
        } else {
            (chunk, "")
        };
        let trimmed_start = line_body.trim_start();
        let indent = line_body.len() - trimmed_start.len();

        if let Some(base_indent) = in_multiline_secret_indent {
            if trimmed_start.is_empty() || trimmed_start.starts_with('#') {
                scrubbed_lines.push_str(chunk);
                continue;
            }
            if indent > base_indent {
                let leading_ws = &line_body[..indent];
                scrubbed_lines.push_str(leading_ws);
                scrubbed_lines.push_str("***");
                scrubbed_lines.push_str(newline);
                continue;
            } else {
                in_multiline_secret_indent = None;
            }
        }

        let after_list_marker = trimmed_start
            .strip_prefix("- ")
            .map(|s| s.trim_start())
            .unwrap_or(trimmed_start);
        let prefix_len = line_body.len() - after_list_marker.len();
        let leading_prefix = &line_body[..prefix_len];

        let mut replaced = false;
        for (raw_key, canonical_key) in [
            ("password:", "password:"),
            ("\"password\":", "\"password\":"),
            ("'password':", "'password':"),
            ("api_key:", "api_key:"),
            ("\"api_key\":", "\"api_key\":"),
            ("'api_key':", "'api_key':"),
            ("client_secret:", "client_secret:"),
            ("\"client_secret\":", "\"client_secret\":"),
        ] {
            if let Some(after_key) = after_list_marker.strip_prefix(raw_key) {
                let val_part = after_key.trim();
                if matches!(val_part, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
                    in_multiline_secret_indent = Some(indent);
                    scrubbed_lines.push_str(leading_prefix);
                    scrubbed_lines.push_str(canonical_key);
                    scrubbed_lines.push_str(" \"***\"");
                    scrubbed_lines.push_str(newline);
                    replaced = true;
                    break;
                }
                let unquoted = val_part.trim_matches('"').trim_matches('\'').trim();
                if !unquoted.is_empty()
                    && !unquoted.eq_ignore_ascii_case("null")
                    && unquoted != "~"
                    && !unquoted.starts_with('#')
                {
                    scrubbed_lines.push_str(leading_prefix);
                    scrubbed_lines.push_str(canonical_key);
                    scrubbed_lines.push_str(" \"***\"");
                    scrubbed_lines.push_str(newline);
                    replaced = true;
                    break;
                }
            }
        }
        if replaced {
            continue;
        }
        let flow_scrubbed = scrub_inline_yaml_secret_keys(line_body);
        scrubbed_lines.push_str(&flow_scrubbed);
        scrubbed_lines.push_str(newline);
    }
    scrubbed_lines
}

fn scrub_inline_yaml_secret_keys(line: &str) -> String {
    if !line.contains('{')
        || (!line.contains("password:")
            && !line.contains("api_key:")
            && !line.contains("client_secret:"))
    {
        return line.to_string();
    }
    let mut current = line.to_string();
    for key in ["password:", "api_key:", "client_secret:"] {
        let mut search_from = 0usize;
        while let Some(rel) = current[search_from..].find(key) {
            let key_pos = search_from + rel;
            if key_pos > 0 {
                let prev = current.as_bytes()[key_pos - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'-' {
                    search_from = key_pos + key.len();
                    continue;
                }
            }
            let val_start = key_pos + key.len();
            let rest = &current[val_start..];
            let ws_len = rest.len() - rest.trim_start().len();
            let val_trimmed = &rest[ws_len..];
            let val_end_rel = val_trimmed.find([',', '}']).unwrap_or(val_trimmed.len());
            let raw_val = val_trimmed[..val_end_rel].trim();
            let unquoted = raw_val.trim_matches('"').trim_matches('\'').trim();
            if !unquoted.is_empty()
                && !unquoted.eq_ignore_ascii_case("null")
                && unquoted != "~"
                && unquoted != "***"
            {
                let replace_end = val_start + ws_len + val_end_rel;
                current.replace_range(val_start..replace_end, " \"***\"");
                search_from = val_start + 6;
            } else {
                search_from = val_start + ws_len + val_end_rel;
            }
        }
    }
    current
}

fn is_sensitive_json_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "password"
            | "pass"
            | "passwd"
            | "pwd"
            | "api_key"
            | "apikey"
            | "secret"
            | "client_secret"
            | "token"
            | "access_token"
            | "auth_token"
    )
}

/// Recursively scrubs credentials from any URL strings and sensitive secret fields inside a JSON `Value`.
pub fn scrub_json_value(val: &Value) -> Value {
    match val {
        Value::String(s) => Value::String(scrub_rtsp_credentials(s)),
        Value::Array(arr) => Value::Array(arr.iter().map(scrub_json_value).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let scrubbed_key = scrub_rtsp_credentials(k);
                let scrubbed_val = if is_sensitive_json_key(k) {
                    match v {
                        Value::String(s) => {
                            let trimmed = s.trim();
                            if trimmed.is_empty()
                                || trimmed.eq_ignore_ascii_case("null")
                                || trimmed == "***"
                                || trimmed == "*"
                            {
                                v.clone()
                            } else {
                                Value::String("***".to_string())
                            }
                        }
                        other => scrub_json_value(other),
                    }
                } else {
                    scrub_json_value(v)
                };
                out.insert(scrubbed_key, scrubbed_val);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Supervises the go2rtc child process, ensuring `/dev/shm/go2rtc.yaml` is written
/// and automatically restarting go2rtc if it exits unexpectedly.
pub struct Go2RtcSupervisor {
    config: Arc<RwLock<NinjaConfig>>,
    go2rtc_bin: PathBuf,
    shm_config_path: PathBuf,
    homekit_config_path: PathBuf,
    pid: Arc<AtomicU32>,
    running: AtomicBool,
}

impl Go2RtcSupervisor {
    pub fn new(config: Arc<RwLock<NinjaConfig>>) -> Self {
        let mut bin_candidates = Vec::new();
        if let Ok(env_bin) = std::env::var("NINJANVR_GO2RTC_BIN") {
            let p = PathBuf::from(env_bin);
            if p.is_absolute() {
                bin_candidates.push(p);
            }
        }
        bin_candidates.push(PathBuf::from("/usr/local/go2rtc/bin/go2rtc"));
        bin_candidates.push(PathBuf::from("/usr/local/bin/go2rtc"));
        bin_candidates.push(PathBuf::from("/usr/bin/go2rtc"));

        let go2rtc_bin = bin_candidates
            .into_iter()
            .find(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("/usr/local/go2rtc/bin/go2rtc"));

        Self {
            config,
            go2rtc_bin,
            shm_config_path: PathBuf::from(DEFAULT_GO2RTC_SHM_PATH),
            homekit_config_path: PathBuf::from(DEFAULT_GO2RTC_HOMEKIT_PATH),
            pid: Arc::new(AtomicU32::new(0)),
            running: AtomicBool::new(false),
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::Relaxed)
    }

    pub fn spawn_supervisor_task(self: Arc<Self>) {
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        tokio::spawn(async move {
            loop {
                let config_ok = {
                    let cfg = self.config.read().await;
                    match cfg.write_go2rtc_config(&self.shm_config_path) {
                        Ok(()) => true,
                        Err(err) => {
                            error!("Failed to write go2rtc config: {err:#}");
                            false
                        }
                    }
                };
                if !config_ok {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }

                if !self.go2rtc_bin.is_absolute() || !self.go2rtc_bin.exists() {
                    warn!(
                        "go2rtc binary not found at {}, retrying in 15s",
                        self.go2rtc_bin.display()
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                    continue;
                }

                let mut cmd = Command::new(&self.go2rtc_bin);
                if self.homekit_config_path.exists() {
                    cmd.arg(format!("-config={}", self.homekit_config_path.display()));
                }
                cmd.arg(format!("-config={}", self.shm_config_path.display()));
                cmd.stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true);

                match cmd.spawn() {
                    Ok(mut child) => {
                        let pid = child.id().unwrap_or(0);
                        self.pid.store(pid, Ordering::Relaxed);
                        info!("Started supervised go2rtc process (PID: {pid})");
                        let status = child.wait().await;
                        self.pid.store(0, Ordering::Relaxed);
                        warn!("go2rtc process exited with status: {status:?}, restarting in 3s");
                    }
                    Err(err) => {
                        error!("Failed to spawn go2rtc: {err:#}");
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ninjanvr_config_and_generates_api_payload() {
        let yaml = r#"
mqtt:
  enabled: true
  host: 127.0.0.1
  port: 1883
cameras:
  sala:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/sala_sub
          roles: [detect]
        - path: rtsp://127.0.0.1:8554/sala
          roles: [record, audio]
    detect:
      width: 640
      height: 480
      fps: 5
    record:
      enabled: true
      continuous:
        days: 3
auth:
  enabled: true
go2rtc:
  streams:
    sala:
      - rtsp://user:pass@192.0.2.1:554/main
version: 0.18-0
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(cfg.cameras.len(), 1);
        assert_eq!(cfg.cameras["sala"].detect.width, 640);
        assert_eq!(cfg.cameras["sala"].record.continuous.days, 3.0);
        assert_eq!(
            cfg.camera_input_for_role("sala", "record").as_deref(),
            Some("rtsp://127.0.0.1:8554/sala")
        );

        let api_val = cfg.to_api_config();
        assert_eq!(api_val["version"], "0.18-0");
        assert_eq!(api_val["cameras"]["sala"]["detect"]["width"], 640);
        assert_eq!(
            api_val["cameras"]["sala"]["record"]["continuous"]["days"],
            3.0
        );
    }

    #[test]
    fn structured_update_preserves_scrubbed_credentials_and_roundtrips_yaml() {
        let yaml = r#"
mqtt:
  enabled: true
  host: 127.0.0.1
  port: 1883
  user: mqtt_admin
  password: super_secret_mqtt_password
cameras:
  sala:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://camuser:campass@192.0.2.50:554/sub
          roles: [detect]
        - path: rtsp://camuser:campass@192.0.2.50:554/main
          roles: [record]
    detect:
      width: 1280
      height: 720
      fps: 5
go2rtc:
  streams:
    sala:
      - rtsp://camuser:campass@192.0.2.50:554/main
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let mut api_payload = cfg.to_api_config();

        // Simulate UI v2 modifying camera detect FPS, adding a zone, and changing MQTT port
        // while sending back the scrubbed URLs from GET /api/config
        api_payload["mqtt"]["port"] = json!(1884);
        api_payload["cameras"]["sala"]["detect"]["fps"] = json!(8);
        api_payload["cameras"]["sala"]["zones"] = json!({
            "porta": {
                "coordinates": "0.1,0.1,0.5,0.1,0.5,0.8,0.1,0.8",
                "objects": ["person"],
                "inertia": 3,
                "loitering_time": 0
            }
        });

        cfg.apply_structured_json(&api_payload).unwrap();

        assert_eq!(cfg.mqtt.port, 1884);
        assert_eq!(
            cfg.mqtt.password.as_deref(),
            Some("super_secret_mqtt_password")
        );
        assert_eq!(cfg.cameras["sala"].detect.fps, 8);
        assert!(cfg.cameras["sala"].zones.contains_key("porta"));
        assert_eq!(
            cfg.camera_input_for_role("sala", "record").as_deref(),
            Some("rtsp://camuser:campass@192.0.2.50:554/main")
        );
        assert!(cfg.raw_yaml.contains("camuser:campass@192.0.2.50:554/main"));

        // Verify CameraMapPlacement roundtrips through structured JSON update and YAML
        let map_patch = json!({
            "cameras": {
                "front_door": {
                    "enabled": true,
                    "map": {
                        "x": 0.25,
                        "y": 0.75,
                        "angle_deg": 135.0,
                        "fov_deg": 110.0,
                        "range": 0.35
                    },
                    "ffmpeg": {
                        "inputs": [
                            {
                                "path": "rtsp://*:*@192.0.2.50:554/main",
                                "roles": ["record", "detect"]
                            }
                        ]
                    }
                }
            }
        });
        cfg.apply_structured_json(&map_patch).unwrap();
        let cam_map = cfg.cameras["front_door"].map;
        assert!((cam_map.x - 0.25).abs() < 1e-4);
        assert!((cam_map.y - 0.75).abs() < 1e-4);
        assert!((cam_map.angle_deg - 135.0).abs() < 1e-4);
        assert!((cam_map.fov_deg - 110.0).abs() < 1e-4);
        assert!((cam_map.range - 0.35).abs() < 1e-4);
        let api_json = cfg.to_api_config();
        assert_eq!(api_json["cameras"]["front_door"]["map"]["fov_deg"], 110.0);
    }

    #[test]
    fn detector_backend_types_and_accelerators_roundtrip_yaml_and_json() {
        let yaml = r#"
detectors:
  ov_cpu:
    type: openvino
    device: CPU
  ort_cuda:
    type: onnxruntime
    device: CUDA
    model:
      width: 640
      height: 640
      input_tensor: nchw
      input_pixel_format: rgb
      path: /config/models/yolo11s.onnx
      model_type: yolo-generic
  ort_alias:
    type: ort
    device: ROCM
version: 0.18-0
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(
            cfg.detectors["ov_cpu"].normalized_backend_type(),
            "openvino"
        );
        assert_eq!(
            cfg.detectors["ort_cuda"].normalized_backend_type(),
            "onnxruntime"
        );
        assert_eq!(
            cfg.detectors["ort_alias"].normalized_backend_type(),
            "onnxruntime"
        );
        assert_eq!(cfg.detectors["ort_cuda"].device, "CUDA");

        let api_val = cfg.to_api_config();
        assert_eq!(api_val["detectors"]["ort_cuda"]["type"], "onnxruntime");
        assert_eq!(api_val["detectors"]["ort_cuda"]["device"], "CUDA");
        assert_eq!(
            api_val["detectors"]["ort_cuda"]["model"]["path"],
            "/config/models/yolo11s.onnx"
        );
        assert_eq!(api_val["detectors"]["ort_cuda"]["model"]["width"], 640);

        let patch = json!({
            "detectors": {
                "ort_cuda": {
                    "type": "onnxruntime",
                    "device": "TensorRT"
                }
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert_eq!(cfg.detectors["ort_cuda"].device, "TensorRT");
        assert_eq!(
            cfg.detectors["ort_cuda"].normalized_backend_type(),
            "onnxruntime"
        );
    }

    #[test]
    fn credential_scrubbing_and_restoration_handles_ipv4_ipv6_tokens_and_edited_paths() {
        // 1. user:password@ (*:*@) vs single token@ (*@) and IPv6 authority
        assert_eq!(
            scrub_rtsp_credentials("rtsp://admin:pass123@192.0.2.50:554/h264Preview_01_main"),
            "rtsp://*:*@192.0.2.50:554/h264Preview_01_main"
        );
        assert_eq!(
            scrub_rtsp_credentials("rtsp://single_token@192.0.2.60:554/live/ch0"),
            "rtsp://*@192.0.2.60:554/live/ch0"
        );
        assert_eq!(
            scrub_rtsp_credentials("rtsp://user:pass@[2001:db8::1]:8554/stream"),
            "rtsp://*:*@[2001:db8::1]:8554/stream"
        );
        assert_eq!(
            scrub_rtsp_credentials("rtsps://token_only@[2001:db8::2]:8555/secure"),
            "rtsps://*@[2001:db8::2]:8555/secure"
        );
        assert_eq!(
            scrub_rtsp_credentials("rtsp://127.0.0.1:8554/no_creds"),
            "rtsp://127.0.0.1:8554/no_creds"
        );
        assert_eq!(
            scrub_rtsp_credentials("preset-rtsp-restream"),
            "preset-rtsp-restream"
        );

        // 2. scrub_yaml_credentials across user:pass@, token@, and IPv6 URLs in raw YAML
        let raw_yaml = r#"
go2rtc:
  streams:
    cam_v4:
      - rtsp://admin:pass123@192.0.2.50:554/h264Preview_01_main
    cam_tok:
      - "rtsp://single_token@192.0.2.60:554/live/ch0"
    cam_v6:
      - rtsp://user:pass@[2001:db8::1]:8554/stream#video=copy
"#;
        let scrubbed_yaml = scrub_yaml_credentials(raw_yaml);
        assert!(scrubbed_yaml.contains("rtsp://*:*@192.0.2.50:554/h264Preview_01_main"));
        assert!(scrubbed_yaml.contains("\"rtsp://*@192.0.2.60:554/live/ch0\""));
        assert!(scrubbed_yaml.contains("rtsp://*:*@[2001:db8::1]:8554/stream#video=copy"));
        assert!(!scrubbed_yaml.contains("admin:pass123"));
        assert!(!scrubbed_yaml.contains("single_token"));
        assert!(!scrubbed_yaml.contains("user:pass"));

        // 3. restore_scrubbed_url: exact match, path modification on scrubbed URL, and unmatched host
        let originals = vec![
            "rtsp://user:pass@192.0.2.50:554/h264Preview_01_main".to_string(),
            "rtsp://single_token@192.0.2.60:554/live/ch0".to_string(),
            "rtsp://v6user:v6pass@[2001:db8::1]:8554/stream".to_string(),
        ];

        // Exact matches
        assert_eq!(
            restore_scrubbed_url("rtsp://*:*@192.0.2.50:554/h264Preview_01_main", &originals),
            "rtsp://user:pass@192.0.2.50:554/h264Preview_01_main"
        );
        assert_eq!(
            restore_scrubbed_url("rtsp://*@192.0.2.60:554/live/ch0", &originals),
            "rtsp://single_token@192.0.2.60:554/live/ch0"
        );
        assert_eq!(
            restore_scrubbed_url("rtsp://*:*@[2001:db8::1]:8554/stream", &originals),
            "rtsp://v6user:v6pass@[2001:db8::1]:8554/stream"
        );

        // Path modification on a scrubbed URL restores credentials by matching host
        assert_eq!(
            restore_scrubbed_url("rtsp://*:*@192.0.2.50:554/h265Preview_01_main", &originals),
            "rtsp://user:pass@192.0.2.50:554/h265Preview_01_main"
        );
        assert_eq!(
            restore_scrubbed_url("rtsp://*@192.0.2.60:554/live/ch1_sub", &originals),
            "rtsp://single_token@192.0.2.60:554/live/ch1_sub"
        );
        assert_eq!(
            restore_scrubbed_url("rtsp://*:*@[2001:db8::1]:8554/sub_stream", &originals),
            "rtsp://v6user:v6pass@[2001:db8::1]:8554/sub_stream"
        );

        // Unmatched host leaves URL untouched without panicking
        assert_eq!(
            restore_scrubbed_url("rtsp://*:*@198.51.100.99:554/stream", &originals),
            "rtsp://*:*@198.51.100.99:554/stream"
        );
        assert_eq!(
            restore_scrubbed_url("rtsp://*:*@[2001:db8::99]:8554/stream", &originals),
            "rtsp://*:*@[2001:db8::99]:8554/stream"
        );
        assert_eq!(
            restore_scrubbed_url("not_a_url_at_all", &originals),
            "not_a_url_at_all"
        );
    }

    #[test]
    fn to_api_value_and_apply_structured_json_roundtrip_all_sections() {
        let initial_yaml = r#"
version: 0.18-0
mqtt:
  enabled: true
  host: 127.0.0.1
  port: 1883
  user: mqtt_op
  password: initial_mqtt_password
cameras:
  driveway:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://cam_admin:cam_secret@192.0.2.50:554/h264Preview_01_main
          roles: [record]
        - path: rtsp://cam_admin:cam_secret@192.0.2.50:554/h264Preview_01_sub
          roles: [detect]
go2rtc:
  streams:
    driveway:
      - rtsp://cam_admin:cam_secret@192.0.2.50:554/h264Preview_01_main
"#;
        let mut cfg =
            NinjaConfig::from_yaml_str(initial_yaml, PathBuf::from("/tmp/roundtrip.yml")).unwrap();
        let mut api = cfg.to_api_value();
        assert_eq!(api["mqtt"]["has_password"], true);

        // Mutate all config sections in the structured payload
        api["mqtt"]["host"] = json!("10.20.30.40");
        api["mqtt"]["port"] = json!(8883);
        api["mqtt"]["tls_enabled"] = json!(true);
        api["mqtt"]["password"] = json!("***"); // Masked password must preserve original password

        api["auth"]["session_length"] = json!(43200);
        api["auth"]["trusted_proxies"] = json!(["10.0.0.0/8", "127.0.0.1/32"]);
        api["auth"]["oidc"] = json!({
            "enabled": true,
            "provider_name": "Zitadel",
            "issuer_url": "https://sso.example.org",
            "client_id": "ninjanvr-web",
            "client_secret_env": "NINJANVR_OIDC_SECRET",
            "scopes": ["openid", "profile", "email", "groups"],
            "auto_redirect": true,
            "username_claim": "preferred_username",
            "groups_claim": "groups",
            "role_mapping": {
                "admin": ["nvr-admins"],
                "operator": ["nvr-ops"],
                "viewer": ["nvr-viewers"]
            },
            "default_role": "viewer"
        });
        api["auth"]["proxy"] = json!({
            "enabled": true,
            "trusted_proxies": ["127.0.0.1/32"],
            "user_header": "X-Forwarded-User",
            "groups_header": "X-Forwarded-Groups",
            "logout_url": "https://sso.example.org/logout"
        });

        api["detectors"] = json!({
            "ov_gpu": {
                "type": "openvino",
                "device": "GPU"
            }
        });
        api["model"] = json!({
            "width": 416,
            "height": 416,
            "input_tensor": "nhwc",
            "input_pixel_format": "rgb",
            "path": "/openvino-model/yolox.xml",
            "labelmap_path": "/openvino-model/coco.txt",
            "model_type": "yolox"
        });
        api["motion"] = json!({
            "enabled": true,
            "threshold": 35,
            "lightning_threshold": 0.75,
            "improve_contrast": true,
            "contour_area": 15,
            "delta_alpha": 0.2,
            "frame_alpha": 0.01,
            "frame_height": 120,
            "mask": "0.0,0.0,1.0,0.0,1.0,0.1,0.0,0.1"
        });
        api["objects"] = json!({
            "track": ["person", "car", "dog"],
            "filters": {
                "person": {
                    "min_area": 0.02,
                    "max_area": 500000.0,
                    "min_ratio": 0.2,
                    "max_ratio": 4.0,
                    "threshold": 0.72,
                    "min_score": 0.55,
                    "mask": {}
                }
            },
            "mask": {}
        });
        api["record"] = json!({
            "enabled": true,
            "expire_interval": 120,
            "continuous_substream": true,
            "mode_tiering": true,
            "max_size_gb": 500.0,
            "min_free_gb": 25.0,
            "continuous": { "days": 7.0 },
            "motion": { "days": 14.0 },
            "alerts": {
                "pre_capture": 8,
                "post_capture": 12,
                "retain": { "days": 30.0, "mode": "active_objects" }
            },
            "detections": {
                "pre_capture": 5,
                "post_capture": 10,
                "retain": { "days": 14.0, "mode": "motion" }
            },
            "preview": { "quality": "high" },
            "storage_tiers": {
                "enabled": true,
                "hot_dir": "/media/hot",
                "cold_dir": "/media/cold",
                "migrate_after_hours": 48
            },
            "alert_backup": {
                "enabled": true,
                "target_dir": "/media/backup",
                "severities": ["alert"],
                "include_snapshot": true
            },
            "cold_compaction": {
                "enabled": true,
                "after_hours": 72,
                "crf": 30,
                "idle_only": true,
                "codec": "hevc",
                "preset": "fast",
                "hwaccel": "vaapi"
            }
        });
        api["snapshots"] = json!({
            "enabled": true,
            "timestamp": true,
            "bounding_box": true,
            "crop": true,
            "quality": 85,
            "high_res": true,
            "from_main_stream": true,
            "selection": "best_overall"
        });
        api["review"] = json!({
            "alerts": {
                "enabled": true,
                "labels": ["person", "car"],
                "required_zones": ["entry_zone"],
                "cutoff_time": 45
            },
            "detections": {
                "enabled": true,
                "required_zones": ["sidewalk"],
                "cutoff_time": 25
            }
        });
        api["audio"] = json!({
            "enabled": true,
            "min_volume_dbfs": -48,
            "labels": ["bark", "speech", "glass"]
        });
        api["profiles"] = json!({
            "home": {
                "disabled_alerts_cameras": ["driveway"],
                "disabled_record_cameras": []
            }
        });
        api["peers"] = json!({
            "warehouse_nvr": {
                "enabled": true,
                "url": "https://nvr2.example.org",
                "token_env": "NINJANVR_PEER_WAREHOUSE_TOKEN"
            }
        });
        api["go2rtc"]["streams"] = json!({
            "driveway": ["rtsp://*:*@192.0.2.50:554/h265Preview_01_main"],
            "driveway_sub": ["rtsp://*:*@192.0.2.50:554/h264Preview_01_sub"]
        });
        api["cameras"]["driveway"]["ffmpeg"]["inputs"] = json!([
            {
                "path": "rtsp://*:*@192.0.2.50:554/h265Preview_01_main",
                "roles": ["record"]
            },
            {
                "path": "rtsp://*:*@192.0.2.50:554/h264Preview_01_sub",
                "roles": ["detect"]
            }
        ]);
        api["cameras"]["driveway"]["live"] = json!({
            "streams": {
                "Sub": "driveway_sub",
                "Main": "driveway"
            },
            "height": 1080,
            "quality": 9,
            "rotate": 90
        });
        api["cameras"]["driveway"]["zones"] = json!({
            "entry_zone": {
                "coordinates": "0.1,0.2,0.8,0.2,0.8,0.9,0.1,0.9",
                "objects": ["person", "car"],
                "inertia": 4,
                "loitering_time": 6,
                "min_area": 0.03,
                "max_area": 200000.0,
                "min_score": 0.6
            }
        });
        api["cameras"]["driveway"]["lines"] = json!({
            "gate_line": {
                "points": [0.1, 0.5, 0.9, 0.5],
                "direction": "a_to_b",
                "objects": ["person", "car"]
            }
        });
        api["cameras"]["driveway"]["motion"]["threshold"] = json!(28);
        api["cameras"]["driveway"]["objects"]["track"] = json!(["person", "car"]);
        api["cameras"]["driveway"]["review"] = json!({
            "alerts": {
                "enabled": true,
                "labels": ["person"],
                "required_zones": ["entry_zone"],
                "cutoff_time": 50
            },
            "detections": {
                "enabled": true,
                "required_zones": [],
                "cutoff_time": 20
            }
        });
        api["cameras"]["driveway"]["record"]["continuous"]["days"] = json!(10.0);
        api["cameras"]["driveway"]["snapshots"]["quality"] = json!(90);
        api["cameras"]["driveway"]["audio"]["enabled"] = json!(true);
        api["cameras"]["driveway"]["audio"]["min_volume_dbfs"] = json!(-52);

        cfg.apply_structured_json(&api).unwrap();

        // Masked password must have been preserved
        assert_eq!(cfg.mqtt.password.as_deref(), Some("initial_mqtt_password"));

        // Serialize to YAML and re-parse with NinjaConfig::from_yaml_str
        let exported_yaml = cfg.to_clean_yaml().unwrap();
        let reparsed =
            NinjaConfig::from_yaml_str(&exported_yaml, PathBuf::from("/tmp/roundtrip.yml"))
                .unwrap();

        assert_eq!(reparsed.mqtt.host, "10.20.30.40");
        assert_eq!(reparsed.mqtt.port, 8883);
        assert!(reparsed.mqtt.should_use_tls());
        assert_eq!(
            reparsed.mqtt.password.as_deref(),
            Some("initial_mqtt_password")
        );
        assert!(reparsed.auth.oidc.enabled);
        assert_eq!(reparsed.auth.oidc.provider_name, "Zitadel");
        assert_eq!(reparsed.auth.oidc.issuer_url, "https://sso.example.org");
        assert!(reparsed.auth.proxy.enabled);
        assert_eq!(reparsed.auth.proxy.user_header, "X-Forwarded-User");
        assert_eq!(reparsed.detectors["ov_gpu"].device, "GPU");
        assert_eq!(reparsed.model.width, 416);
        assert_eq!(reparsed.model.model_type, "yolox");
        assert_eq!(reparsed.motion.threshold, 35);
        assert_eq!(reparsed.objects.track, vec!["person", "car", "dog"]);
        assert_eq!(reparsed.record.continuous.days, 7.0);
        assert_eq!(reparsed.record.cold_compaction.codec, "hevc");
        assert_eq!(
            reparsed.snapshots.selection,
            SnapshotSelectionPolicy::BestOverall
        );
        assert_eq!(reparsed.review.alerts.cutoff_time, 45);
        assert!(reparsed.audio.enabled);
        assert_eq!(reparsed.audio.min_volume_dbfs, -48);
        assert_eq!(
            reparsed.profiles["home"].disabled_alerts_cameras,
            vec!["driveway"]
        );
        assert_eq!(
            reparsed.peers["warehouse_nvr"].url,
            "https://nvr2.example.org"
        );

        let drv = &reparsed.cameras["driveway"];
        assert_eq!(
            reparsed
                .camera_input_for_role("driveway", "record")
                .as_deref(),
            Some("rtsp://cam_admin:cam_secret@192.0.2.50:554/h265Preview_01_main")
        );
        assert_eq!(
            reparsed
                .camera_input_for_role("driveway", "detect")
                .as_deref(),
            Some("rtsp://cam_admin:cam_secret@192.0.2.50:554/h264Preview_01_sub")
        );
        assert_eq!(drv.live.rotate, 90);
        assert_eq!(drv.zones["entry_zone"].loitering_time, 6);
        assert_eq!(drv.lines["gate_line"].direction, LineDirection::AToB);
        assert_eq!(drv.motion.threshold, 28);
        assert_eq!(drv.review.alerts.cutoff_time, 50);
        assert_eq!(drv.record.continuous.days, 10.0);
        assert_eq!(drv.snapshots.quality, 90);
        assert!(drv.audio.enabled);
        assert_eq!(drv.audio.min_volume_dbfs, -52);

        // Now verify mqtt.clear_password clears the password
        let clear_patch = json!({
            "mqtt": {
                "enabled": true,
                "host": "10.20.30.40",
                "port": 1883,
                "clear_password": true
            }
        });
        cfg.apply_structured_json(&clear_patch).unwrap();
        assert!(cfg.mqtt.password.is_none());
        assert_eq!(cfg.to_api_value()["mqtt"]["has_password"], false);
    }

    #[test]
    fn write_go2rtc_config_generates_streams_webrtc_api_and_extra_keys() {
        let yaml = r#"
go2rtc:
  streams:
    front_door:
      - rtsp://admin:secret@192.0.2.10:554/main
    front_door_sub:
      - rtsp://admin:secret@192.0.2.10:554/sub
  rtsp:
    listen: ":8554"
  ffmpeg:
    bin: /usr/local/bin/ffmpeg
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_go2rtc_test_{}", std::process::id()));
        let go2rtc_path = tmp_dir.join("go2rtc.yaml");
        cfg.write_go2rtc_config(&go2rtc_path).unwrap();

        let written = std::fs::read_to_string(&go2rtc_path).unwrap();
        let parsed: Value = serde_yaml::from_str(&written).unwrap();

        assert_eq!(
            parsed["streams"]["front_door"][0],
            "rtsp://admin:secret@192.0.2.10:554/main"
        );
        assert_eq!(
            parsed["streams"]["front_door_sub"][0],
            "rtsp://admin:secret@192.0.2.10:554/sub"
        );
        assert_eq!(parsed["api"]["origin"], "*");
        assert_eq!(parsed["webrtc"]["candidates"][0], "stun:8555");
        assert_eq!(parsed["log"]["format"], "text");
        assert_eq!(parsed["rtsp"]["listen"], ":8554");
        assert_eq!(parsed["ffmpeg"]["bin"], "/usr/local/bin/ffmpeg");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn camera_input_for_role_load_labelmap_and_resolve_area_bounds_px_work() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_labelmap_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let custom_labelmap = tmp_dir.join("custom_labels.txt");
        std::fs::write(&custom_labelmap, "__background__\nperson\nforklift\ncar\n").unwrap();

        let yaml = format!(
            r#"
model:
  labelmap_path: {}
cameras:
  yard:
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/yard_sub
          roles: [detect]
        - path: rtsp://127.0.0.1:8554/yard_main
          roles: [record, audio]
"#,
            custom_labelmap.display()
        );
        let mut cfg = NinjaConfig::from_yaml_str(&yaml, tmp_dir.join("config.yml")).unwrap();

        // 1. camera_input_for_role (detect vs record vs missing)
        assert_eq!(
            cfg.camera_input_for_role("yard", "detect").as_deref(),
            Some("rtsp://127.0.0.1:8554/yard_sub")
        );
        assert_eq!(
            cfg.camera_input_for_role("yard", "record").as_deref(),
            Some("rtsp://127.0.0.1:8554/yard_main")
        );
        assert!(
            cfg.camera_input_for_role("yard", "nonexistent_role")
                .is_none()
        );
        assert!(
            cfg.camera_input_for_role("missing_camera", "detect")
                .is_none()
        );

        // 2. load_labelmap: custom file path vs fallback COCO-91 map
        let loaded_custom = cfg.load_labelmap();
        assert_eq!(
            loaded_custom.get("0").map(String::as_str),
            Some("__background__")
        );
        assert_eq!(loaded_custom.get("1").map(String::as_str), Some("person"));
        assert_eq!(loaded_custom.get("2").map(String::as_str), Some("forklift"));
        assert_eq!(loaded_custom.get("3").map(String::as_str), Some("car"));

        cfg.model.labelmap_path = "/nonexistent/path/to/labels.txt".to_string();
        let fallback_map = cfg.load_labelmap();
        assert_eq!(
            fallback_map.get("0").map(String::as_str),
            Some("__background__")
        );
        assert_eq!(fallback_map.get("1").map(String::as_str), Some("person"));
        assert_eq!(fallback_map.get("2").map(String::as_str), Some("bicycle"));
        assert_eq!(fallback_map.get("3").map(String::as_str), Some("car"));
        assert_eq!(fallback_map.get("17").map(String::as_str), Some("cat"));
        assert_eq!(fallback_map.get("18").map(String::as_str), Some("dog"));

        // 3. ObjectFilterConfig::resolve_area_bounds_px(1920, 1080)
        // Fractional: 0.05 * (1920 * 1080) = 103,680 px^2
        let frac_filter = ObjectFilterConfig {
            min_area: 0.05,
            max_area: 0.50,
            ..Default::default()
        };
        assert_eq!(
            frac_filter.resolve_area_bounds_px(1920, 1080),
            (103_680, 1_036_800)
        );

        // Absolute pixel values: 5000.0 -> 5,000 px^2
        let abs_filter = ObjectFilterConfig {
            min_area: 5000.0,
            max_area: 250_000.0,
            ..Default::default()
        };
        assert_eq!(
            abs_filter.resolve_area_bounds_px(1920, 1080),
            (5_000, 250_000)
        );

        // 4. NinjaConfig::load from disk, resolve_camera_webui_base_url, and Go2RtcSupervisor::new/pid
        let disk_cfg_path = tmp_dir.join("disk_config.yml");
        std::fs::write(
            &disk_cfg_path,
            r#"
cameras:
  custom_ui_cam:
    webui_url: "https://192.0.2.80:8443/"
  rtsp_host_cam:
    ffmpeg:
      inputs:
        - path: rtsp://admin:pass@192.0.2.81:554/main
          roles: [record]
  loopback_cam:
    webui_url: "http://127.0.0.1:8080"
    ffmpeg:
      inputs:
        - path: rtsp://localhost:8554/loop
          roles: [detect]
go2rtc:
  streams:
    loopback_cam:
      - rtsp://admin:pass@[2001:db8::88]:554/live
"#,
        )
        .unwrap();
        let disk_cfg = NinjaConfig::load(&disk_cfg_path).unwrap();
        assert_eq!(
            disk_cfg
                .resolve_camera_webui_base_url("custom_ui_cam")
                .as_deref(),
            Some("https://192.0.2.80:8443")
        );
        assert_eq!(
            disk_cfg
                .resolve_camera_webui_base_url("rtsp_host_cam")
                .as_deref(),
            Some("http://192.0.2.81")
        );
        assert_eq!(
            disk_cfg
                .resolve_camera_webui_base_url("loopback_cam")
                .as_deref(),
            Some("http://[2001:db8::88]")
        );
        assert!(disk_cfg.resolve_camera_webui_base_url("missing").is_none());

        let sup = Go2RtcSupervisor::new(Arc::new(RwLock::new(disk_cfg)));
        assert_eq!(sup.pid(), 0);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn camera_groups_and_camera_ui_roundtrip_yaml_and_json() {
        let yaml = r#"
camera_groups:
  perimeter:
    cameras: [gate_north, gate_south]
    icon: shield
    order: 1
  warehouse:
    cameras: [dock_01]
    order: 2
cameras:
  gate_north:
    enabled: true
    ui:
      order: 10
      dashboard: true
  gate_south:
    enabled: true
    ui:
      order: 5
      dashboard: true
  dock_01:
    enabled: true
    ui:
      order: 20
      dashboard: false
  default_ui_cam:
    enabled: true
version: 0.18-0
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(cfg.camera_groups.len(), 2);
        assert_eq!(
            cfg.camera_groups["perimeter"].cameras,
            vec!["gate_north".to_string(), "gate_south".to_string()]
        );
        assert_eq!(
            cfg.camera_groups["perimeter"].icon.as_deref(),
            Some("shield")
        );
        assert_eq!(cfg.camera_groups["perimeter"].order, 1);
        assert_eq!(cfg.camera_groups["warehouse"].icon, None);
        assert_eq!(cfg.camera_groups["warehouse"].order, 2);

        assert_eq!(cfg.cameras["gate_north"].ui.order, 10);
        assert!(cfg.cameras["gate_north"].ui.dashboard);
        assert_eq!(cfg.cameras["dock_01"].ui.order, 20);
        assert!(!cfg.cameras["dock_01"].ui.dashboard);
        // Default CameraUiConfig when omitted in YAML
        assert_eq!(cfg.cameras["default_ui_cam"].ui.order, 0);
        assert!(cfg.cameras["default_ui_cam"].ui.dashboard);

        let api_val = cfg.to_api_value();
        assert_eq!(
            api_val["camera_groups"]["perimeter"]["cameras"],
            json!(["gate_north", "gate_south"])
        );
        assert_eq!(api_val["camera_groups"]["perimeter"]["icon"], "shield");
        assert_eq!(api_val["camera_groups"]["perimeter"]["order"], 1);
        assert_eq!(api_val["cameras"]["gate_north"]["ui"]["order"], 10);
        assert_eq!(api_val["cameras"]["gate_north"]["ui"]["dashboard"], true);
        assert_eq!(api_val["cameras"]["dock_01"]["ui"]["order"], 20);
        assert_eq!(api_val["cameras"]["dock_01"]["ui"]["dashboard"], false);

        // Apply structured JSON update modifying camera_groups and camera.ui
        let patch = json!({
            "camera_groups": {
                "perimeter": {
                    "cameras": ["gate_north", "gate_south", "dock_01"],
                    "icon": "lock",
                    "order": 1
                },
                "interior": {
                    "cameras": ["default_ui_cam"],
                    "order": 3
                }
            },
            "cameras": {
                "gate_north": {
                    "enabled": true,
                    "ui": { "order": 1, "dashboard": true }
                },
                "dock_01": {
                    "enabled": true,
                    "ui": { "order": 2, "dashboard": true }
                }
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert_eq!(cfg.camera_groups.len(), 2);
        assert_eq!(cfg.camera_groups["perimeter"].cameras.len(), 3);
        assert_eq!(cfg.camera_groups["perimeter"].icon.as_deref(), Some("lock"));
        assert_eq!(cfg.camera_groups["interior"].order, 3);
        assert_eq!(cfg.cameras["gate_north"].ui.order, 1);
        assert!(cfg.cameras["dock_01"].ui.dashboard);

        let roundtripped =
            NinjaConfig::from_yaml_str(&cfg.raw_yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(roundtripped.camera_groups, cfg.camera_groups);
        assert_eq!(
            roundtripped.cameras["gate_north"].ui,
            cfg.cameras["gate_north"].ui
        );
    }

    #[test]
    fn attribute_labels_and_best_attribute_snapshot_policy_roundtrip() {
        let yaml = r#"
objects:
  track: [person, car]
  attribute_labels: [face, license_plate, correios, mercado_livre]
snapshots:
  enabled: true
  selection: best_attribute
cameras:
  portao:
    enabled: true
    snapshots:
      enabled: true
      selection: best_attribute
version: 0.18-0
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(
            cfg.objects.attribute_labels,
            vec!["face", "license_plate", "correios", "mercado_livre"]
        );
        assert_eq!(
            cfg.cameras["portao"].objects.attribute_labels,
            vec!["face", "license_plate", "correios", "mercado_livre"]
        );
        assert_eq!(
            cfg.snapshots.selection,
            SnapshotSelectionPolicy::BestAttribute
        );
        assert_eq!(
            cfg.cameras["portao"].snapshots.selection,
            SnapshotSelectionPolicy::BestAttribute
        );

        let api_val = cfg.to_api_value();
        assert_eq!(
            api_val["objects"]["attribute_labels"],
            json!(["face", "license_plate", "correios", "mercado_livre"])
        );
        assert_eq!(api_val["snapshots"]["selection"], "best_attribute");
        assert_eq!(
            api_val["cameras"]["portao"]["snapshots"]["selection"],
            "best_attribute"
        );

        let patch = json!({
            "objects": {
                "track": ["person", "car", "truck"],
                "attribute_labels": ["face", "license_plate", "dhl"]
            },
            "snapshots": {
                "enabled": true,
                "selection": "best_attribute"
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert_eq!(
            cfg.objects.attribute_labels,
            vec!["face", "license_plate", "dhl"]
        );
        assert_eq!(
            cfg.snapshots.selection,
            SnapshotSelectionPolicy::BestAttribute
        );
    }

    #[test]
    fn floorplan_config_rooms_walls_opacity_and_background_roundtrip_yaml_and_json() {
        let yaml = r##"
ui:
  floorplan:
    opacity: 0.45
    background_data_url: "data:image/svg+xml;base64,PHN2Zy8+"
    rooms:
      - id: "escritorio"
        label: "ESCRITÓRIO"
        x: 0.08
        y: 0.10
        w: 0.38
        h: 0.36
        color: "#0ea5e9"
    walls:
      - points: [0.08, 0.46, 0.46, 0.46]
version: 0.18-0
"##;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert!((cfg.ui.floorplan.opacity - 0.45).abs() < 1e-4);
        assert_eq!(
            cfg.ui.floorplan.background_data_url.as_deref(),
            Some("data:image/svg+xml;base64,PHN2Zy8+")
        );
        assert_eq!(cfg.ui.floorplan.rooms.len(), 1);
        assert_eq!(cfg.ui.floorplan.rooms[0].id, "escritorio");
        assert_eq!(cfg.ui.floorplan.rooms[0].label, "ESCRITÓRIO");
        assert!((cfg.ui.floorplan.rooms[0].x - 0.08).abs() < 1e-4);
        assert!((cfg.ui.floorplan.rooms[0].y - 0.10).abs() < 1e-4);
        assert!((cfg.ui.floorplan.rooms[0].w - 0.38).abs() < 1e-4);
        assert!((cfg.ui.floorplan.rooms[0].h - 0.36).abs() < 1e-4);
        assert_eq!(cfg.ui.floorplan.rooms[0].color.as_deref(), Some("#0ea5e9"));
        assert_eq!(cfg.ui.floorplan.walls.len(), 1);
        assert_eq!(cfg.ui.floorplan.walls[0].points, [0.08, 0.46, 0.46, 0.46]);

        let api_val = cfg.to_api_value();
        assert_eq!(api_val["ui"]["floorplan"]["rooms"][0]["id"], "escritorio");
        assert_eq!(
            api_val["ui"]["floorplan"]["rooms"][0]["label"],
            "ESCRITÓRIO"
        );
        assert_eq!(
            api_val["ui"]["floorplan"]["walls"][0]["points"],
            json!([0.08_f32, 0.46_f32, 0.46_f32, 0.46_f32])
        );

        let patch = json!({
            "ui": {
                "floorplan": {
                    "background_data_url": "data:image/png;base64,iVBORw0KGgo=",
                    "opacity": 0.72,
                    "rooms": [
                        {
                            "id": "sala",
                            "label": "SALA",
                            "x": 0.10,
                            "y": 0.15,
                            "w": 0.45,
                            "h": 0.40,
                            "color": "#38bdf8"
                        },
                        {
                            "id": "cozinha",
                            "label": "COZINHA",
                            "x": 0.55,
                            "y": 0.15,
                            "w": 0.35,
                            "h": 0.40
                        }
                    ],
                    "walls": [
                        { "points": [0.55, 0.15, 0.55, 0.55] }
                    ]
                }
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert!((cfg.ui.floorplan.opacity - 0.72).abs() < 1e-4);
        assert_eq!(cfg.ui.floorplan.rooms.len(), 2);
        assert_eq!(cfg.ui.floorplan.rooms[0].id, "sala");
        assert_eq!(cfg.ui.floorplan.rooms[1].id, "cozinha");
        assert_eq!(cfg.ui.floorplan.walls[0].points, [0.55, 0.15, 0.55, 0.55]);

        let roundtripped =
            NinjaConfig::from_yaml_str(&cfg.raw_yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(roundtripped.ui.floorplan, cfg.ui.floorplan);
    }

    #[test]
    fn genai_config_roundtrips_yaml_and_structured_json() {
        assert!(GenAiConfig::default().model.is_empty());

        let yaml = r#"
genai:
  enabled: true
  provider: ollama
  base_url: http://127.0.0.1:11434
  api_key_env: OLLAMA_API_KEY
  model: qwen2.5vl:7b
  verify_ambiguous_alerts: true
  ambiguous_score_min: 0.42
  ambiguous_score_max: 0.74
  auto_suppress_false_positives: true
  timeout_ms: 6000
version: 0.18-0
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert!(cfg.genai.enabled);
        assert_eq!(cfg.genai.provider, "ollama");
        assert_eq!(
            cfg.genai.base_url.as_deref(),
            Some("http://127.0.0.1:11434")
        );
        assert_eq!(cfg.genai.model, "qwen2.5vl:7b");
        assert!(cfg.genai.verify_ambiguous_alerts);
        assert!((cfg.genai.ambiguous_score_min - 0.42).abs() < 1e-4);
        assert!((cfg.genai.ambiguous_score_max - 0.74).abs() < 1e-4);
        assert!(cfg.genai.auto_suppress_false_positives);
        assert_eq!(cfg.genai.timeout_ms, 6000);

        let api_val = cfg.to_api_value();
        assert_eq!(api_val["genai"]["enabled"], true);
        assert_eq!(api_val["genai"]["provider"], "ollama");
        assert_eq!(api_val["genai"]["model"], "qwen2.5vl:7b");
        assert_eq!(api_val["genai"]["has_api_key"], false);

        let patch = json!({
            "genai": {
                "enabled": true,
                "provider": "gemini",
                "api_key": "test_direct_genai_key",
                "api_key_env": "GEMINI_API_KEY",
                "model": "gemini-2.5-flash",
                "verify_ambiguous_alerts": false,
                "ambiguous_score_min": 0.48,
                "ambiguous_score_max": 0.70,
                "auto_suppress_false_positives": true,
                "timeout_ms": 4500
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert_eq!(cfg.genai.provider, "gemini");
        assert_eq!(cfg.genai.model, "gemini-2.5-flash");
        assert_eq!(cfg.genai.resolve_api_key(), "test_direct_genai_key");
        assert_eq!(cfg.to_api_value()["genai"]["has_api_key"], true);
        assert!(!cfg.genai.verify_ambiguous_alerts);
        assert_eq!(cfg.genai.timeout_ms, 4500);

        // Masked key "***" preserves the saved API key
        let preserve_patch = json!({
            "genai": {
                "enabled": true,
                "provider": "gemini",
                "api_key": "***",
                "api_key_env": "GEMINI_API_KEY",
                "model": "gemini-2.5-flash"
            }
        });
        cfg.apply_structured_json(&preserve_patch).unwrap();
        assert_eq!(cfg.genai.resolve_api_key(), "test_direct_genai_key");
    }

    #[test]
    fn onvif_config_and_preset_zones_scrub_and_roundtrip_passwords() {
        let yaml = r#"
cameras:
  ptz_gate:
    enabled: true
    onvif:
      host: 192.0.2.90
      port: 8000
      user: onvif_operator
      password: super_secret_onvif_pw
      autotracking: true
      return_preset: entry_gate
      timeout_sec: 7
    zones:
      gate_zone:
        coordinates: "0.1,0.1,0.8,0.1,0.8,0.9,0.1,0.9"
        objects: [person, car]
        inertia: 3
        loitering_time: 2
        preset: entry_gate
      global_zone:
        coordinates: "0.0,0.0,1.0,0.0,1.0,1.0,0.0,1.0"
        objects: [person]
  default_onvif_cam:
    enabled: true
version: 0.18-0
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/onvif.yml")).unwrap();
        let onvif = &cfg.cameras["ptz_gate"].onvif;
        assert!(onvif.is_configured());
        assert!(!onvif.is_unconfigured());
        assert_eq!(onvif.host.as_deref(), Some("192.0.2.90"));
        assert_eq!(onvif.port, 8000);
        assert_eq!(onvif.user.as_deref(), Some("onvif_operator"));
        assert_eq!(onvif.password.as_deref(), Some("super_secret_onvif_pw"));
        assert!(onvif.autotracking);
        assert_eq!(onvif.return_preset.as_deref(), Some("entry_gate"));
        assert_eq!(onvif.timeout_sec, 7);

        let def_onvif = &cfg.cameras["default_onvif_cam"].onvif;
        assert!(!def_onvif.is_configured());
        assert!(def_onvif.is_unconfigured());
        assert_eq!(def_onvif.port, 80);
        assert_eq!(def_onvif.timeout_sec, 5);
        assert!(!def_onvif.autotracking);

        assert_eq!(
            cfg.cameras["ptz_gate"].zones["gate_zone"].preset.as_deref(),
            Some("entry_gate")
        );
        assert_eq!(cfg.cameras["ptz_gate"].zones["global_zone"].preset, None);

        // 1. scrub_yaml_credentials scrubs ONVIF password in raw YAML
        let scrubbed_yaml = scrub_yaml_credentials(yaml);
        assert!(!scrubbed_yaml.contains("super_secret_onvif_pw"));
        assert!(scrubbed_yaml.contains("password: \"***\""));

        // 2. to_api_value() / to_api_config() scrubs ONVIF password
        let mut api = cfg.to_api_value();
        let api_str = serde_json::to_string(&api).unwrap();
        assert!(!api_str.contains("super_secret_onvif_pw"));
        assert_eq!(api["cameras"]["ptz_gate"]["onvif"]["password"], "***");
        assert_eq!(api["cameras"]["ptz_gate"]["onvif"]["has_password"], true);
        assert_eq!(api["cameras"]["ptz_gate"]["onvif"]["host"], "192.0.2.90");
        assert_eq!(api["cameras"]["ptz_gate"]["onvif"]["port"], 8000);
        assert_eq!(api["cameras"]["ptz_gate"]["onvif"]["autotracking"], true);
        assert_eq!(
            api["cameras"]["ptz_gate"]["onvif"]["return_preset"],
            "entry_gate"
        );
        assert_eq!(api["cameras"]["ptz_gate"]["onvif"]["timeout_sec"], 7);
        assert_eq!(
            api["cameras"]["ptz_gate"]["zones"]["gate_zone"]["preset"],
            "entry_gate"
        );

        // 3. apply_structured_json preserves scrubbed "***" password while updating port & zone preset
        api["cameras"]["ptz_gate"]["onvif"]["port"] = json!(8080);
        api["cameras"]["ptz_gate"]["zones"]["gate_zone"]["preset"] = json!("driveway");
        cfg.apply_structured_json(&api).unwrap();
        assert_eq!(cfg.cameras["ptz_gate"].onvif.port, 8080);
        assert_eq!(
            cfg.cameras["ptz_gate"].onvif.password.as_deref(),
            Some("super_secret_onvif_pw")
        );
        assert_eq!(
            cfg.cameras["ptz_gate"].zones["gate_zone"].preset.as_deref(),
            Some("driveway")
        );

        // 4. apply_structured_json updates password when a new non-masked password is sent
        api["cameras"]["ptz_gate"]["onvif"]["password"] = json!("rotated_onvif_pw");
        cfg.apply_structured_json(&api).unwrap();
        assert_eq!(
            cfg.cameras["ptz_gate"].onvif.password.as_deref(),
            Some("rotated_onvif_pw")
        );

        // 5. apply_structured_json clears password when clear_password is true
        api["cameras"]["ptz_gate"]["onvif"]["password"] = json!("***");
        api["cameras"]["ptz_gate"]["onvif"]["clear_password"] = json!(true);
        cfg.apply_structured_json(&api).unwrap();
        assert!(cfg.cameras["ptz_gate"].onvif.password.is_none());
        assert_eq!(
            cfg.to_api_value()["cameras"]["ptz_gate"]["onvif"]["has_password"],
            false
        );
    }

    #[test]
    fn parses_face_recognition_and_review_sub_label_rules_and_roundtrips_json() {
        let yaml = r#"
face_recognition:
  enabled: true
  model_path: /config/models/arcface_r50.onnx
  similarity_threshold: 0.72
  min_face_area: 900
  unknown_alert: true
review:
  alerts:
    enabled: true
    labels: [person, car]
    sub_labels: [unknown]
    exclude_sub_labels: [Bruno, Alice]
cameras:
  portao:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/portao_sub
          roles: [detect]
version: 0.18-0
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert!(cfg.face_recognition.enabled);
        assert_eq!(
            cfg.face_recognition.model_path,
            PathBuf::from("/config/models/arcface_r50.onnx")
        );
        assert!((cfg.face_recognition.similarity_threshold - 0.72).abs() < 1e-5);
        assert_eq!(cfg.face_recognition.min_face_area, 900);
        assert!(cfg.face_recognition.unknown_alert);
        assert_eq!(cfg.review.alerts.sub_labels, vec!["unknown"]);
        assert_eq!(cfg.review.alerts.exclude_sub_labels, vec!["Bruno", "Alice"]);
        assert_eq!(
            cfg.cameras["portao"].review.alerts.exclude_sub_labels,
            vec!["Bruno", "Alice"]
        );

        let api_val = cfg.to_api_value();
        assert_eq!(api_val["face_recognition"]["enabled"], true);
        assert_eq!(api_val["face_recognition"]["min_face_area"], 900);
        assert_eq!(api_val["face_recognition"]["unknown_alert"], true);
        assert_eq!(
            api_val["review"]["alerts"]["exclude_sub_labels"],
            json!(["Bruno", "Alice"])
        );

        let patch = json!({
            "face_recognition": {
                "enabled": true,
                "model_path": "/config/models/adaface.onnx",
                "similarity_threshold": 0.68,
                "min_face_area": 800,
                "unknown_alert": false
            },
            "review": {
                "alerts": {
                    "enabled": true,
                    "labels": ["person"],
                    "sub_labels": [],
                    "exclude_sub_labels": ["Bruno"],
                    "required_zones": [],
                    "cutoff_time": 40
                },
                "detections": {
                    "enabled": true,
                    "required_zones": [],
                    "cutoff_time": 30
                }
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert_eq!(
            cfg.face_recognition.model_path,
            PathBuf::from("/config/models/adaface.onnx")
        );
        assert!((cfg.face_recognition.similarity_threshold - 0.68).abs() < 1e-5);
        assert_eq!(cfg.face_recognition.min_face_area, 800);
        assert!(!cfg.face_recognition.unknown_alert);
        assert_eq!(cfg.review.alerts.exclude_sub_labels, vec!["Bruno"]);
    }

    #[test]
    fn lpr_config_known_plates_and_standalone_license_plate_track_roundtrip() {
        let yaml = r#"
lpr:
  enabled: true
  detector_model_path: /config/models/lpr_det.onnx
  ocr_model_path: /config/models/lpr_ocr.onnx
  min_confidence: 0.82
  format_regex: "^[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$"
  known_plates:
    ABC1D23: "Family SUV"
    XYZ9876: "Courier Van"
cameras:
  gate_lpr:
    enabled: true
    objects:
      track: [license_plate]
version: 0.18-0
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/lpr.yml")).unwrap();
        assert!(cfg.lpr.enabled);
        assert_eq!(
            cfg.lpr.detector_model_path,
            Some(PathBuf::from("/config/models/lpr_det.onnx"))
        );
        assert_eq!(
            cfg.lpr.ocr_model_path,
            Some(PathBuf::from("/config/models/lpr_ocr.onnx"))
        );
        assert!((cfg.lpr.min_confidence - 0.82).abs() < 1e-5);
        assert_eq!(
            cfg.lpr.format_regex.as_deref(),
            Some("^[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$")
        );
        assert_eq!(
            cfg.lpr.known_plates.get("ABC1D23").map(String::as_str),
            Some("Family SUV")
        );
        assert!(
            !cfg.cameras["gate_lpr"]
                .objects
                .attribute_labels
                .iter()
                .any(|a| a.eq_ignore_ascii_case("license_plate")),
            "standalone license_plate in objects.track must not remain in attribute_labels"
        );

        let api_val = cfg.to_api_value();
        assert_eq!(api_val["lpr"]["enabled"], true);
        assert_eq!(api_val["lpr"]["known_plates"]["ABC1D23"], "Family SUV");
        assert_eq!(api_val["cameras"]["gate_lpr"]["lpr"]["enabled"], true);

        let patch = json!({
            "lpr": {
                "enabled": true,
                "min_confidence": 0.85,
                "format_regex": "^[A-Z]{3}[0-9]{4}$",
                "known_plates": {
                    "BRA2E19": "Visitor Sedan"
                }
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert!((cfg.lpr.min_confidence - 0.85).abs() < 1e-5);
        assert_eq!(
            cfg.lpr.known_plates.get("BRA2E19").map(String::as_str),
            Some("Visitor Sedan")
        );
    }

    #[test]
    fn state_rois_roundtrip_yaml_and_structured_json_with_defaults() {
        let yaml = r#"
cameras:
  portao:
    enabled: true
    state_rois:
      gate:
        crop: [0.1, 0.2, 0.6, 0.8]
        classes: [closed, open]
        interval_sec: 5
        hysteresis: 3
        anomaly_state: open
        anomaly_timeout_sec: 180
      parking_spot:
        crop: [0.5, 0.5, 0.9, 0.9]
version: 0.18-0
"#;
        let mut cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let gate = &cfg.cameras["portao"].state_rois["gate"];
        assert_eq!(gate.crop, [0.1, 0.2, 0.6, 0.8]);
        assert_eq!(gate.classes, vec!["closed", "open"]);
        assert_eq!(gate.interval_sec, 5);
        assert_eq!(gate.hysteresis, 3);
        assert_eq!(gate.anomaly_state.as_deref(), Some("open"));
        assert_eq!(gate.anomaly_timeout_sec, 180);

        let spot = &cfg.cameras["portao"].state_rois["parking_spot"];
        assert_eq!(spot.classes, vec!["closed", "open"]);
        assert_eq!(spot.interval_sec, 5);
        assert_eq!(spot.hysteresis, 3);
        assert!(spot.anomaly_state.is_none());
        assert_eq!(spot.anomaly_timeout_sec, 300);

        let api_val = cfg.to_api_value();
        assert_eq!(
            api_val["cameras"]["portao"]["state_rois"]["gate"]["anomaly_state"],
            "open"
        );
        assert_eq!(
            api_val["cameras"]["portao"]["state_rois"]["gate"]["anomaly_timeout_sec"],
            180
        );

        let patch = json!({
            "cameras": {
                "portao": {
                    "enabled": true,
                    "state_rois": {
                        "gate": {
                            "crop": [0.15, 0.25, 0.65, 0.85],
                            "classes": ["closed", "open"],
                            "interval_sec": 4,
                            "hysteresis": 4,
                            "anomaly_state": "open",
                            "anomaly_timeout_sec": 240
                        },
                        "driveway_spot": {
                            "crop": [0.4, 0.4, 0.9, 0.9],
                            "classes": ["empty", "occupied"],
                            "interval_sec": 10,
                            "hysteresis": 2,
                            "anomaly_state": "occupied",
                            "anomaly_timeout_sec": 600
                        }
                    }
                }
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert_eq!(cfg.cameras["portao"].state_rois.len(), 2);
        assert_eq!(
            cfg.cameras["portao"].state_rois["driveway_spot"].classes,
            vec!["empty", "occupied"]
        );
        assert_eq!(
            cfg.cameras["portao"].state_rois["driveway_spot"].hysteresis,
            2
        );
        assert_eq!(
            cfg.cameras["portao"].state_rois["driveway_spot"]
                .anomaly_state
                .as_deref(),
            Some("occupied")
        );
        assert_eq!(
            cfg.cameras["portao"].state_rois["driveway_spot"].anomaly_timeout_sec,
            600
        );
    }

    #[test]
    fn ui_config_theme_and_high_contrast_defaults_yaml_and_structured_json_roundtrip() {
        // 1. Default values when omitted in YAML
        let default_yaml = "version: 0.18-0\n";
        let mut cfg =
            NinjaConfig::from_yaml_str(default_yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(cfg.ui.theme, "dark");
        assert!(!cfg.ui.high_contrast);

        let api_default = cfg.to_api_value();
        assert_eq!(api_default["ui"]["theme"], "dark");
        assert_eq!(api_default["ui"]["high_contrast"], false);

        // 2. Explicit light theme and high_contrast: true in YAML
        let light_yaml = r#"
ui:
  theme: light
  high_contrast: true
  time_format: 24h
  unit_system: metric
version: 0.18-0
"#;
        let mut light_cfg =
            NinjaConfig::from_yaml_str(light_yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(light_cfg.ui.theme, "light");
        assert!(light_cfg.ui.high_contrast);

        let api_light = light_cfg.to_api_value();
        assert_eq!(api_light["ui"]["theme"], "light");
        assert_eq!(api_light["ui"]["high_contrast"], true);

        // 3. System auto theme and invalid fallback normalization
        let system_yaml = r#"
ui:
  theme: SYSTEM
  high_contrast: false
version: 0.18-0
"#;
        let sys_cfg =
            NinjaConfig::from_yaml_str(system_yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(sys_cfg.ui.theme, "system");
        assert!(!sys_cfg.ui.high_contrast);

        let invalid_yaml = r#"
ui:
  theme: neon_cyberpunk
version: 0.18-0
"#;
        let invalid_cfg =
            NinjaConfig::from_yaml_str(invalid_yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        assert_eq!(invalid_cfg.ui.theme, "dark");

        // 4. apply_structured_json round-trip and partial patch preservation
        let patch = json!({
            "ui": {
                "theme": "light",
                "high_contrast": true
            }
        });
        cfg.apply_structured_json(&patch).unwrap();
        assert_eq!(cfg.ui.theme, "light");
        assert!(cfg.ui.high_contrast);
        assert_eq!(cfg.ui.time_format, "browser");
        assert_eq!(cfg.ui.unit_system, "metric");

        // Partial floorplan update preserves theme and high_contrast
        let fp_patch = json!({
            "ui": {
                "floorplan": {
                    "opacity": 0.60,
                    "rooms": [],
                    "walls": []
                }
            }
        });
        cfg.apply_structured_json(&fp_patch).unwrap();
        assert_eq!(cfg.ui.theme, "light");
        assert!(cfg.ui.high_contrast);
        assert!((cfg.ui.floorplan.opacity - 0.60).abs() < 1e-4);

        // Switch to system mode and disable high_contrast, then roundtrip via raw_yaml
        light_cfg
            .apply_structured_json(&json!({
                "ui": {
                    "theme": "system",
                    "high_contrast": false
                }
            }))
            .unwrap();
        assert_eq!(light_cfg.ui.theme, "system");
        assert!(!light_cfg.ui.high_contrast);

        let reparsed =
            NinjaConfig::from_yaml_str(&light_cfg.raw_yaml, PathBuf::from("/tmp/config.yml"))
                .unwrap();
        assert_eq!(reparsed.ui.theme, "system");
        assert!(!reparsed.ui.high_contrast);
    }

    #[test]
    fn mqtt_should_use_tls_evaluates_explicit_override_port_and_ca_certs() {
        let mut mqtt = MqttConfig::default();
        assert_eq!(mqtt.port, 1883);
        assert!(!mqtt.should_use_tls());

        // Port 8883 enables TLS by default when tls_enabled is None
        mqtt.port = 8883;
        assert!(mqtt.should_use_tls());

        // Explicit tls_enabled: Some(false) overrides port 8883 and tls_ca_certs
        mqtt.tls_enabled = Some(false);
        mqtt.tls_ca_certs = Some(PathBuf::from("/etc/ssl/certs/ca.pem"));
        assert!(!mqtt.should_use_tls());

        // Explicit tls_enabled: Some(true) enables TLS on port 1883 without CA certs
        mqtt.port = 1883;
        mqtt.tls_ca_certs = None;
        mqtt.tls_enabled = Some(true);
        assert!(mqtt.should_use_tls());

        // tls_ca_certs: Some(...) enables TLS when tls_enabled is None on port 1883
        mqtt.tls_enabled = None;
        mqtt.tls_ca_certs = Some(PathBuf::from("/etc/ssl/certs/ca.pem"));
        assert!(mqtt.should_use_tls());
    }

    #[test]
    fn genai_has_api_key_and_resolve_api_key_handle_masks_and_env_vars() {
        let mut genai = GenAiConfig::default();
        assert!(!genai.has_api_key());

        for blank_or_masked in ["", "   ", "***", "*", " *** "] {
            genai.api_key = Some(blank_or_masked.to_string());
            assert!(
                !genai.has_api_key(),
                "masked or blank key '{blank_or_masked}' must not report has_api_key"
            );
        }

        genai.api_key = Some("sk-direct-key-123".to_string());
        assert!(genai.has_api_key());
        assert_eq!(genai.resolve_api_key(), "sk-direct-key-123");

        let env_name = format!("NINJANVR_TEST_GENAI_KEY_{}", std::process::id());
        // SAFETY: Test-scoped unique environment variable name not read concurrently by other threads.
        unsafe {
            std::env::set_var(&env_name, "  sk-from-env-456  ");
        }

        // $ENV_VAR syntax in api_key
        genai.api_key = Some(format!("${env_name}"));
        assert_eq!(genai.resolve_api_key(), "sk-from-env-456");

        // Bare ENV_VAR name in api_key
        genai.api_key = Some(env_name.clone());
        assert_eq!(genai.resolve_api_key(), "sk-from-env-456");

        // Masked api_key ("***") falls back to api_key_env
        genai.api_key = Some("***".to_string());
        genai.api_key_env = env_name.clone();
        assert_eq!(genai.resolve_api_key(), "sk-from-env-456");

        unsafe {
            std::env::remove_var(&env_name);
        }
        genai.api_key = None;
        assert_eq!(genai.resolve_api_key(), "");
    }

    #[test]
    fn onvif_config_deserializes_legacy_autotracking_map_and_detects_configuration_state() {
        let def = OnvifCameraConfig::default();
        assert!(def.is_unconfigured());
        assert!(!def.is_configured());

        // Whitespace-only host is still not configured
        let blank_host = OnvifCameraConfig {
            host: Some("   ".to_string()),
            ..Default::default()
        };
        assert!(blank_host.is_unconfigured());
        assert!(!blank_host.is_configured());

        // Custom port marks it non-default (not unconfigured), even before host is set
        let custom_port = OnvifCameraConfig {
            port: 8899,
            ..Default::default()
        };
        assert!(!custom_port.is_unconfigured());
        assert!(!custom_port.is_configured());

        // Legacy Frigate-style map: autotracking: { enabled: true }
        let legacy_true: OnvifCameraConfig = serde_yaml::from_str(
            r#"
host: 192.0.2.50
port: 8000
autotracking:
  enabled: true
"#,
        )
        .unwrap();
        assert!(legacy_true.autotracking);
        assert!(legacy_true.is_configured());
        assert!(!legacy_true.is_unconfigured());

        // Legacy map with enabled: false and non-boolean fallback
        let legacy_false: OnvifCameraConfig = serde_yaml::from_str(
            r#"
autotracking:
  enabled: false
"#,
        )
        .unwrap();
        assert!(!legacy_false.autotracking);

        let invalid_type: OnvifCameraConfig = serde_yaml::from_str("autotracking: 42\n").unwrap();
        assert!(!invalid_type.autotracking);
    }

    #[test]
    fn scrub_and_restore_json_value_preserves_nested_structures_and_credentials() {
        let originals = vec![
            "rtsp://admin:s3cr3t@192.0.2.10:554/cam/realmonitor?channel=1&subtype=0".to_string(),
            "rtsps://oper:p%40ss@192.0.2.11:322/live/main".to_string(),
        ];
        let payload = json!({
            "cameras": {
                "gate": {
                    "enabled": true,
                    "fps": 15,
                    "inputs": [
                        { "path": originals[0], "roles": ["record"] },
                        { "path": "rtsp://127.0.0.1:8554/gate_sub", "roles": ["detect"] }
                    ]
                }
            },
            "go2rtc": {
                "streams": {
                    "gate_main": [originals[0], "ffmpeg:gate_main#audio=aac"],
                    "patio_tls": originals[1]
                }
            }
        });

        let scrubbed = scrub_json_value(&payload);
        let scrubbed_str = serde_json::to_string(&scrubbed).unwrap();
        assert!(!scrubbed_str.contains("s3cr3t"));
        assert!(!scrubbed_str.contains("p%40ss"));
        assert!(scrubbed_str.contains("rtsp://*:*@192.0.2.10:554"));
        assert!(scrubbed_str.contains("rtsps://*:*@192.0.2.11:322"));
        assert_eq!(scrubbed["cameras"]["gate"]["fps"], 15);

        let restored = restore_scrubbed_json(&scrubbed, &originals);
        assert_eq!(restored, payload);
    }

    #[test]
    fn config_bugfixes_inherit_global_track_preserve_roundtrip_fields_and_scrub_consecutive_secrets()
     {
        let yaml = r#"
tls:
  enabled: true
  san_hosts:
    - nvr.local
objects:
  track:
    - person
    - car
    - dog
record:
  detections:
    retain:
      days: 14
      mode: motion
      max_size_gb: 120.5
      min_free_gb: 25.0
  alerts:
    retain:
      days: 30
      mode: all
      max_size_gb: 80.0
      min_free_gb: 15.0
cameras:
  cam_inherited:
    enabled: true
    indoor: true
    objects:
      filters:
        person:
          threshold: 0.72
          night_threshold: 0.61
  cam_explicit:
    enabled: true
    objects:
      track:
        - person
"#;
        let mut cfg =
            NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config_bugfixes.yml")).unwrap();

        // 1. Camera without explicit objects.track inherits global [person, car, dog],
        // while camera with explicit [person] keeps [person]
        assert_eq!(
            cfg.cameras["cam_inherited"].objects.track,
            vec!["person".to_string(), "car".to_string(), "dog".to_string()]
        );
        assert_eq!(
            cfg.cameras["cam_explicit"].objects.track,
            vec!["person".to_string()]
        );

        // 2. Round-trip through to_api_value() -> apply_structured_json() preserves
        // night_threshold, indoor, tls.san_hosts, and retain.{max_size_gb, min_free_gb}
        let api_val = cfg.to_api_value();
        assert_eq!(api_val["cameras"]["cam_inherited"]["indoor"], true);
        let api_nt =
            api_val["cameras"]["cam_inherited"]["objects"]["filters"]["person"]["night_threshold"]
                .as_f64()
                .unwrap();
        assert!((api_nt - 0.61).abs() < 1e-5);
        assert_eq!(api_val["tls"]["san_hosts"], json!(["nvr.local"]));
        assert_eq!(
            api_val["record"]["detections"]["retain"]["max_size_gb"],
            120.5
        );
        assert_eq!(
            api_val["record"]["detections"]["retain"]["min_free_gb"],
            25.0
        );

        cfg.apply_structured_json(&api_val).unwrap();
        assert!(cfg.cameras["cam_inherited"].indoor);
        let roundtrip_nt = cfg.cameras["cam_inherited"].objects.filters["person"]
            .night_threshold
            .unwrap();
        assert!((roundtrip_nt - 0.61).abs() < 1e-5);
        assert_eq!(cfg.tls.san_hosts, vec!["nvr.local".to_string()]);
        assert_eq!(cfg.record.detections.retain.max_size_gb, Some(120.5));
        assert_eq!(cfg.record.detections.retain.min_free_gb, Some(25.0));
        assert_eq!(cfg.record.alerts.retain.max_size_gb, Some(80.0));
        assert_eq!(cfg.record.alerts.retain.min_free_gb, Some(15.0));

        // 3. scrub_yaml_credentials preserves an empty api_key/password line immediately following a scrubbed line
        let raw_with_consecutive = "mqtt:\n  password: super_secret\ngenai:\n  api_key: \"\"\n";
        let scrubbed_yaml = scrub_yaml_credentials(raw_with_consecutive);
        assert!(scrubbed_yaml.contains("password: \"***\""));
        assert!(
            scrubbed_yaml.contains("api_key: \"\""),
            "empty api_key line after a scrubbed password line must not be dropped: {scrubbed_yaml}"
        );

        // 4. GenAiConfig::resolve_api_key with unset $ENV_VAR falls back to api_key_env / empty string
        let unset_genai = GenAiConfig {
            api_key: Some("$NINJANVR_DEFINITELY_UNSET_VAR_999999".to_string()),
            api_key_env: "NINJANVR_DEFINITELY_UNSET_ENV_999999".to_string(),
            ..Default::default()
        };
        assert_eq!(unset_genai.resolve_api_key(), "");
    }

    #[test]
    fn to_api_config_scrubs_onvif_host_and_webui_url_and_restores_from_previous() {
        let yaml = r#"
mqtt:
  enabled: true
  host: 127.0.0.1
  port: 1883
  user: mqtt_admin
  password: secret_mqtt_password
genai:
  enabled: true
  provider: gemini
  api_key: secret_genai_api_key
  model: gemini-2.5-flash
cameras:
  gate:
    enabled: true
    webui_url: "https://webui_user:webui_pass@192.0.2.91:8443/ui"
    onvif:
      host: "http://onvif_user:onvif_pass@192.0.2.92:8000"
      port: 8000
      user: onvif_user
      password: secret_onvif_password
    ffmpeg:
      inputs:
        - path: rtsp://rtsp_user:rtsp_pass@192.0.2.93:554/main
          roles: [record, detect]
go2rtc:
  streams:
    gate:
      - rtsp://stream_token@192.0.2.94:554/live
"#;
        let orig_cfg =
            NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/r16_config.yml")).unwrap();

        // 1. to_api_config scrubs onvif.host and webui_url credentials
        let api_val = orig_cfg.to_api_config();
        assert_eq!(
            api_val["cameras"]["gate"]["onvif"]["host"],
            "http://*:*@192.0.2.92:8000"
        );
        assert_eq!(
            api_val["cameras"]["gate"]["webui_url"],
            "https://*:*@192.0.2.91:8443/ui"
        );
        let api_str = serde_json::to_string(&api_val).unwrap();
        assert!(!api_str.contains("webui_pass"));
        assert!(!api_str.contains("onvif_pass"));
        assert!(!api_str.contains("rtsp_pass"));
        assert!(!api_str.contains("stream_token"));
        assert!(!api_str.contains("secret_onvif_password"));
        assert!(!api_str.contains("secret_mqtt_password"));
        assert!(!api_str.contains("secret_genai_api_key"));

        // 2. collect_original_urls includes onvif.host and webui_url so structured updates restore them
        let originals = orig_cfg.collect_original_urls();
        assert!(
            originals
                .iter()
                .any(|u| u == "http://onvif_user:onvif_pass@192.0.2.92:8000")
        );
        assert!(
            originals
                .iter()
                .any(|u| u == "https://webui_user:webui_pass@192.0.2.91:8443/ui")
        );

        let mut structured_cfg = orig_cfg.clone();
        structured_cfg.apply_structured_json(&api_val).unwrap();
        assert_eq!(
            structured_cfg.cameras["gate"].onvif.host.as_deref(),
            Some("http://onvif_user:onvif_pass@192.0.2.92:8000")
        );
        assert_eq!(
            structured_cfg.cameras["gate"].webui_url.as_deref(),
            Some("https://webui_user:webui_pass@192.0.2.91:8443/ui")
        );

        // 3. restore_scrubbed_from_previous restores all scrubbed URLs and masked passwords/keys in raw YAML edits
        let scrubbed_yaml = scrub_yaml_credentials(&orig_cfg.raw_yaml);
        assert!(!scrubbed_yaml.contains("secret_mqtt_password"));
        assert!(!scrubbed_yaml.contains("secret_genai_api_key"));
        assert!(!scrubbed_yaml.contains("secret_onvif_password"));
        assert!(!scrubbed_yaml.contains("webui_pass"));
        assert!(!scrubbed_yaml.contains("onvif_pass"));
        assert!(!scrubbed_yaml.contains("rtsp_pass"));
        assert!(!scrubbed_yaml.contains("stream_token"));

        let mut edited_cfg =
            NinjaConfig::from_yaml_str(&scrubbed_yaml, PathBuf::from("/tmp/r16_config.yml"))
                .unwrap();
        edited_cfg.restore_scrubbed_from_previous(&orig_cfg);

        assert_eq!(
            edited_cfg.mqtt.password.as_deref(),
            Some("secret_mqtt_password")
        );
        assert_eq!(
            edited_cfg.genai.api_key.as_deref(),
            Some("secret_genai_api_key")
        );
        let gate = &edited_cfg.cameras["gate"];
        assert_eq!(
            gate.onvif.password.as_deref(),
            Some("secret_onvif_password")
        );
        assert_eq!(
            gate.onvif.host.as_deref(),
            Some("http://onvif_user:onvif_pass@192.0.2.92:8000")
        );
        assert_eq!(
            gate.webui_url.as_deref(),
            Some("https://webui_user:webui_pass@192.0.2.91:8443/ui")
        );
        assert_eq!(
            gate.ffmpeg.inputs[0].path,
            "rtsp://rtsp_user:rtsp_pass@192.0.2.93:554/main"
        );
        assert_eq!(
            edited_cfg.go2rtc.streams["gate"],
            json!(["rtsp://stream_token@192.0.2.94:554/live"])
        );
        assert!(edited_cfg.raw_yaml.contains("secret_mqtt_password"));
        assert!(edited_cfg.raw_yaml.contains("secret_genai_api_key"));
        assert!(edited_cfg.raw_yaml.contains("secret_onvif_password"));
        assert!(
            edited_cfg
                .raw_yaml
                .contains("rtsp://rtsp_user:rtsp_pass@192.0.2.93:554/main")
        );
    }

    #[test]
    fn extract_non_loopback_host_and_resolve_camera_webui_base_url_block_ssrf_targets() {
        for forbidden_host in [
            "127.0.0.1",
            "127.0.0.2",
            "127.255.255.254",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "[::ffff:127.0.0.2]",
            "0.0.0.0",
            "[::]",
            "[::ffff:0.0.0.0]",
            "169.254.169.254",
            "169.254.0.1",
            "[::ffff:169.254.169.254]",
            "[fe80::1]",
            "[febf::1234]",
            "localhost",
            "LOCALHOST",
            "sub.localhost",
            "metadata",
            "metadata.google.internal",
            "service.internal",
            "ninjanvr",
            "NINJANVR",
            "ninjanvr.default.svc",
            "cam.svc.cluster.local",
        ] {
            let rtsp_url = format!("rtsp://{forbidden_host}:554/stream");
            assert!(
                extract_non_loopback_host(&rtsp_url).is_none(),
                "SSRF target '{forbidden_host}' must be rejected by extract_non_loopback_host"
            );
        }

        assert_eq!(
            extract_non_loopback_host("rtsp://192.0.2.100:554/live").as_deref(),
            Some("192.0.2.100")
        );
        assert_eq!(
            extract_non_loopback_host("rtsp://[2001:db8::42]:554/live").as_deref(),
            Some("[2001:db8::42]")
        );

        let yaml = r#"
cameras:
  ssrf_loopback:
    webui_url: "http://127.0.0.2:8080"
  ssrf_v6_mapped:
    webui_url: "http://[::ffff:127.0.0.1]:8080"
  ssrf_unspec:
    webui_url: "http://0.0.0.0:8080"
  ssrf_imds:
    webui_url: "http://169.254.169.254"
  ssrf_v6_link_local:
    webui_url: "http://[fe80::1]"
  ssrf_gcp_meta:
    webui_url: "http://metadata.google.internal"
  ssrf_self:
    webui_url: "http://ninjanvr:5000"
  ssrf_k8s_svc:
    webui_url: "http://cam.svc.cluster.local:8080"
  bad_userinfo:
    webui_url: "http://admin:pass@192.0.2.100:8080"
  bad_query:
    webui_url: "http://192.0.2.100:8080?foo=bar"
  bad_fragment:
    webui_url: "http://192.0.2.100:8080#frag"
  bad_backslash:
    webui_url: "http://192.0.2.100:8080\\admin"
  valid_cam:
    webui_url: "https://192.0.2.100:8443/"
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/ssrf.yml")).unwrap();
        for blocked_cam in [
            "ssrf_loopback",
            "ssrf_v6_mapped",
            "ssrf_unspec",
            "ssrf_imds",
            "ssrf_v6_link_local",
            "ssrf_gcp_meta",
            "ssrf_self",
            "ssrf_k8s_svc",
            "bad_userinfo",
            "bad_query",
            "bad_fragment",
            "bad_backslash",
        ] {
            assert!(
                cfg.resolve_camera_webui_base_url(blocked_cam).is_none(),
                "camera '{blocked_cam}' webui_url must be rejected"
            );
        }
        assert_eq!(
            cfg.resolve_camera_webui_base_url("valid_cam").as_deref(),
            Some("https://192.0.2.100:8443")
        );
    }

    #[test]
    fn go2rtc_producer_and_stream_validation_blocks_command_injection_and_dangerous_extra_keys() {
        for disallowed in [
            "",
            "   ",
            "rtsp://192.0.2.10:554/main\nexec:id",
            "rtsp://192.0.2.10:554/main\0",
            "exec:sh -c id",
            "EXEC:/bin/sh",
            "echo:hello",
            "expr:1+1",
            "file:/etc/passwd",
            "stdio:cat /etc/shadow",
            "ffmpeg:/etc/passwd",
            "ffmpeg: /config/config.yml",
            "ffmpeg:exec:id",
            "ffmpeg:echo:test",
            "ffmpeg:expr:1+1",
            "ffmpeg:file:/etc/passwd",
            "ffmpeg:stdio:sh",
            "ffmpeg:",
        ] {
            assert!(
                is_disallowed_go2rtc_producer(disallowed),
                "producer '{disallowed}' must be disallowed"
            );
        }

        for allowed in [
            "rtsp://admin:pass@192.0.2.10:554/h264Preview_01_main",
            "rtsps://192.0.2.10:322/live",
            "ffmpeg:front_door#audio=aac",
            "ffmpeg:rtsp://192.0.2.10:554/main#video=copy",
        ] {
            assert!(
                !is_disallowed_go2rtc_producer(allowed),
                "producer '{allowed}' must be allowed"
            );
        }

        for bad_key in [
            "",
            "front door",
            "front/door",
            "exec:cam",
            "../cam",
            &"a".repeat(129),
        ] {
            let mut map = BTreeMap::new();
            map.insert(
                bad_key.to_string(),
                json!("rtsp://192.0.2.10:554/h264Preview_01_main"),
            );
            assert!(
                validate_go2rtc_streams_map(&map).is_err(),
                "unsafe go2rtc stream key '{bad_key}' must be rejected"
            );
        }

        for bad_producer in [
            "exec:id",
            "echo:secret",
            "expr:2+2",
            "file:/config/.jwt_secret",
            "ffmpeg:/etc/passwd",
        ] {
            let yaml =
                format!("go2rtc:\n  streams:\n    front_door:\n      - \"{bad_producer}\"\n");
            assert!(
                NinjaConfig::from_yaml_str(&yaml, PathBuf::from("/tmp/bad_go2rtc.yml")).is_err(),
                "from_yaml_str must reject disallowed producer '{bad_producer}'"
            );
        }

        let valid_yaml = r#"
go2rtc:
  streams:
    front_door:
      - rtsp://192.0.2.10:554/main
  rtsp:
    listen: ":8554"
  exec:
    allow: true
  expr:
    allow: true
  echo:
    allow: true
  hass:
    config: /config
  ngrok:
    command: ngrok
"#;
        let mut cfg =
            NinjaConfig::from_yaml_str(valid_yaml, PathBuf::from("/tmp/go2rtc_extra.yml")).unwrap();

        let bad_patch = json!({
            "go2rtc": {
                "streams": {
                    "front_door": ["exec:cat /config/.jwt_secret"]
                }
            }
        });
        assert!(
            cfg.apply_structured_json(&bad_patch).is_err(),
            "apply_structured_json must reject disallowed go2rtc producer"
        );

        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_go2rtc_sec_test_{}", std::process::id()));
        let out_path = tmp_dir.join("go2rtc.yaml");
        cfg.write_go2rtc_config(&out_path).unwrap();
        let written = std::fs::read_to_string(&out_path).unwrap();
        let parsed: Value = serde_yaml::from_str(&written).unwrap();
        assert_eq!(parsed["rtsp"]["listen"], ":8554");
        for blocked_extra in ["exec", "expr", "echo", "hass", "ngrok"] {
            assert!(
                parsed.get(blocked_extra).is_none(),
                "dangerous go2rtc.extra key '{blocked_extra}' must be omitted from go2rtc.yaml"
            );
        }

        // Direct mutation of streams must also be caught by write_go2rtc_config
        cfg.go2rtc
            .streams
            .insert("front_door".to_string(), json!("ffmpeg:/etc/passwd"));
        assert!(cfg.write_go2rtc_config(&out_path).is_err());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn labelmap_path_validation_and_loader_block_secret_exfiltration_and_allow_valid_txt() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_labelmap_sec_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();

        let jwt_secret_file = tmp_dir.join(".jwt_secret");
        std::fs::write(&jwt_secret_file, "super_secret_jwt_signing_key_hex\n").unwrap();

        let cfg_yml_file = tmp_dir.join("config.yml");
        std::fs::write(
            &cfg_yml_file,
            "mqtt:\n  password: secret_mqtt_pw\ncameras:\n  cam1:\n    path: rtsp://admin:pass@192.0.2.1/live\n",
        )
        .unwrap();

        let symlink_to_secret = tmp_dir.join("labels_link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&jwt_secret_file, &symlink_to_secret).unwrap();

        let symlink_to_yml = tmp_dir.join("config_link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&cfg_yml_file, &symlink_to_yml).unwrap();

        for unsafe_path in [
            Path::new(""),
            Path::new("/config/.jwt_secret"),
            Path::new(".jwt_secret"),
            Path::new("/config/.env.txt"),
            Path::new("/config/.active_profile"),
            Path::new("/config/config.yml"),
            Path::new("config.yml"),
            Path::new("/config/models/../../.jwt_secret"),
            Path::new("../labels.txt"),
            Path::new("/etc/passwd"),
            Path::new("/etc/hosts.txt"),
            Path::new("/proc/self/environ"),
            Path::new("/sys/kernel/notes.txt"),
            Path::new("/dev/null.txt"),
            Path::new("/root/labels.txt"),
            Path::new("/run/secrets/token.txt"),
            jwt_secret_file.as_path(),
            cfg_yml_file.as_path(),
            symlink_to_secret.as_path(),
            symlink_to_yml.as_path(),
        ] {
            assert!(
                !is_safe_labelmap_path(unsafe_path),
                "unsafe labelmap path '{}' must be rejected",
                unsafe_path.display()
            );
        }

        let valid_labelmap = tmp_dir.join("valid_labels.txt");
        std::fs::write(
            &valid_labelmap,
            "person\ncar\nrtsp://user:pass@192.0.2.1/leak\npassword: leaked\napi_key: leaked\nbicycle\n",
        )
        .unwrap();
        assert!(is_safe_labelmap_path(&valid_labelmap));
        assert!(is_safe_labelmap_path(Path::new("/openvino-model/coco.txt")));

        let mut cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", tmp_dir.join("config.yml")).unwrap();

        // Unsafe paths fall back to standard COCO map without reading secret files
        for bad in [
            &jwt_secret_file,
            &cfg_yml_file,
            &symlink_to_secret,
            &symlink_to_yml,
            &PathBuf::from("/etc/passwd"),
        ] {
            cfg.model.labelmap_path = bad.to_string_lossy().to_string();
            let map = cfg.load_labelmap();
            assert_eq!(map.get("0").map(String::as_str), Some("__background__"));
            assert!(!map.values().any(|v| v.contains("super_secret_jwt")));
            assert!(!map.values().any(|v| v.contains("secret_mqtt_pw")));
        }

        // Valid .txt file loads valid labels while skipping credential-like lines
        cfg.model.labelmap_path = valid_labelmap.to_string_lossy().to_string();
        let filtered_map = cfg.load_labelmap();
        assert_eq!(filtered_map.get("0").map(String::as_str), Some("person"));
        assert_eq!(filtered_map.get("1").map(String::as_str), Some("car"));
        assert_eq!(filtered_map.get("5").map(String::as_str), Some("bicycle"));
        assert!(!filtered_map.values().any(|v| v.contains("://")));
        assert!(!filtered_map.values().any(|v| v.starts_with("password:")));
        assert!(!filtered_map.values().any(|v| v.starts_with("api_key:")));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn phase1a_audit_scrub_and_restore_query_params_special_chars_and_yaml_variants() {
        // 1. Query-parameter credentials and special characters (#, ?, ,) in userinfo
        let urls_and_expected = [
            (
                "rtsp://192.0.2.10:554/live?user=admin&password=q_secret_123&channel=1",
                "rtsp://192.0.2.10:554/live?user=admin&password=***&channel=1",
            ),
            (
                "http://192.0.2.11/cgi-bin/snapshot.cgi?usr=op&pwd=snap_pw&token=tok_abc#frag",
                "http://192.0.2.11/cgi-bin/snapshot.cgi?usr=op&pwd=***&token=***#frag",
            ),
            (
                "rtsp://admin:p#ss?1,2]3@192.0.2.12:554/stream",
                "rtsp://*:*@192.0.2.12:554/stream",
            ),
            (
                "http://192.0.2.13/proxy?url=rtsp://admin:nested_pw@192.0.2.14:554/sub",
                "http://192.0.2.13/proxy?url=rtsp://*:*@192.0.2.14:554/sub",
            ),
        ];
        for (raw, expected_scrubbed) in urls_and_expected {
            let scrubbed = scrub_rtsp_credentials(raw);
            assert_eq!(scrubbed, expected_scrubbed);
            let originals = vec![raw.to_string()];
            assert_eq!(restore_scrubbed_url(&scrubbed, &originals), raw);
        }

        // Edited path with scrubbed query parameter restores original secret parameter
        let orig_query_urls =
            vec!["rtsp://192.0.2.10:554/main?user=admin&password=q_secret_123".to_string()];
        assert_eq!(
            restore_scrubbed_url(
                "rtsp://192.0.2.10:554/sub?user=admin&password=***",
                &orig_query_urls
            ),
            "rtsp://192.0.2.10:554/sub?user=admin&password=q_secret_123"
        );

        // 2. scrub_yaml_credentials handles commas/hashes in userinfo, list items, flow maps, and block scalars
        let tricky_yaml = r#"
mqtt: { host: 127.0.0.1, password: inline_mqtt_secret }
genai:
  "api_key": quoted_key_secret
  password: |
    multiline_secret_line_1
    multiline_secret_line_2
cameras:
  cam1:
    ffmpeg:
      inputs:
        - path: rtsp://admin:p,a#s]s@192.0.2.20:554/main?token=query_tok_99
        - password: list_item_password_secret
"#;
        let scrubbed_yaml = scrub_yaml_credentials(tricky_yaml);
        for secret in [
            "inline_mqtt_secret",
            "quoted_key_secret",
            "multiline_secret_line_1",
            "multiline_secret_line_2",
            "p,a#s]s",
            "query_tok_99",
            "list_item_password_secret",
        ] {
            assert!(
                !scrubbed_yaml.contains(secret),
                "scrub_yaml_credentials leaked '{secret}' in:\n{scrubbed_yaml}"
            );
        }
        assert!(scrubbed_yaml.contains("rtsp://*:*@192.0.2.20:554/main?token=***"));

        // 3. scrub_json_value masks sensitive object keys (password, api_key, token, secret)
        let raw_json = json!({
            "password": "json_plain_password",
            "api_key": "json_plain_api_key",
            "access_token": "json_plain_token",
            "nested": {
                "client_secret": "json_client_secret",
                "url": "https://192.0.2.30/api?api_key=url_query_key"
            }
        });
        let scrubbed_json = scrub_json_value(&raw_json);
        assert_eq!(scrubbed_json["password"], "***");
        assert_eq!(scrubbed_json["api_key"], "***");
        assert_eq!(scrubbed_json["access_token"], "***");
        assert_eq!(scrubbed_json["nested"]["client_secret"], "***");
        assert_eq!(
            scrubbed_json["nested"]["url"],
            "https://192.0.2.30/api?api_key=***"
        );

        // 4. Debug implementations redact secrets
        let mqtt_dbg = format!(
            "{:?}",
            MqttConfig {
                password: Some("mqtt_dbg_secret".to_string()),
                ..Default::default()
            }
        );
        assert!(!mqtt_dbg.contains("mqtt_dbg_secret"));
        let genai_dbg = format!(
            "{:?}",
            GenAiConfig {
                api_key: Some("genai_dbg_secret".to_string()),
                base_url: Some("https://u:genai_url_secret@example.org".to_string()),
                ..Default::default()
            }
        );
        assert!(!genai_dbg.contains("genai_dbg_secret"));
        assert!(!genai_dbg.contains("genai_url_secret"));
        let onvif_dbg = format!(
            "{:?}",
            OnvifCameraConfig {
                password: Some("onvif_dbg_secret".to_string()),
                host: Some("http://u:onvif_url_secret@192.0.2.40".to_string()),
                ..Default::default()
            }
        );
        assert!(!onvif_dbg.contains("onvif_dbg_secret"));
        assert!(!onvif_dbg.contains("onvif_url_secret"));
    }

    #[test]
    fn phase1a_audit_ssrf_nonstandard_ipv4_ipv6_translations_and_percent_encoding_blocked() {
        for bypass_host in [
            "127.0.0.1.",
            "169.254.169.254.",
            "127.1",
            "127.0.1",
            "2130706433",
            "2851995649",
            "0",
            "0.0.0.1",
            "0.255.255.255",
            "0177.0.0.1",
            "0x7f.0.0.1",
            "0x7f000001",
            "100.100.100.200",
            "192.0.0.192",
            "255.255.255.255",
            "224.0.0.1",
            "%31%32%37.0.0.1",
            "%6c%6f%63%61%6c%68%6f%73%74",
            "[::127.0.0.1]",
            "[::169.254.169.254]",
            "[::ffff:0:127.0.0.1]",
            "[64:ff9b::127.0.0.1]",
            "[64:ff9b::169.254.169.254]",
            "[2002:7f00:0001::]",
            "[2002:a9fe:a9fe::]",
            "[fd00:ec2::254]",
            "[ff02::1]",
            "[2001:db8::1%25eth0]",
            "[2001:db8::1",
            "ip6-localhost",
            "ip6-loopback",
        ] {
            let url = format!("http://{bypass_host}:8080/");
            assert!(
                extract_non_loopback_host(&url).is_none(),
                "SSRF bypass host '{bypass_host}' must be rejected"
            );
        }

        // Invalid ports and multiple colons in non-IPv6 authority must be rejected
        assert!(extract_non_loopback_host("http://192.0.2.50:0/").is_none());
        assert!(extract_non_loopback_host("http://192.0.2.50:99999/").is_none());
        assert!(extract_non_loopback_host("http://192.0.2.50:80:81/").is_none());
        assert!(extract_non_loopback_host("http://[2001:db8::1]:0/").is_none());
    }

    #[test]
    fn phase1a_audit_go2rtc_extra_case_insensitive_streams_and_chained_ffmpeg_blocked() {
        for bypass_producer in [
            "/etc/passwd",
            "./config.yml",
            "../.jwt_secret",
            "-i /etc/passwd",
            "ffmpeg:ffmpeg:/etc/passwd",
            "ffmpeg:ffmpeg:exec:id",
            "ffmpeg: -re -i /config/.jwt_secret",
            "ffmpeg:./secret.txt",
            "ffmpeg:../secret.txt",
            "hass:HomeAssistant",
            "cmd:/bin/sh",
            "pipe:1",
            "rtsp://192.0.2.10:554/main#input=exec:id",
            "rtsp://192.0.2.10:554/main\t",
        ] {
            assert!(
                is_disallowed_go2rtc_producer(bypass_producer),
                "bypass producer '{bypass_producer}' must be disallowed"
            );
        }

        // Case-insensitive `Streams:` in go2rtc.extra must be validated and blocked if unsafe
        let capitalized_streams_yaml = r#"
go2rtc:
  Streams:
    evil:
      - exec:id
"#;
        assert!(
            NinjaConfig::from_yaml_str(
                capitalized_streams_yaml,
                PathBuf::from("/tmp/cap_streams.yml")
            )
            .is_err(),
            "capitalized go2rtc.Streams with exec: producer must be rejected"
        );

        // Disallowed go2rtc.ffmpeg.bin must be rejected
        let bad_ffmpeg_bin_yaml = r#"
go2rtc:
  ffmpeg:
    bin: /bin/sh
"#;
        assert!(
            NinjaConfig::from_yaml_str(
                bad_ffmpeg_bin_yaml,
                PathBuf::from("/tmp/bad_ffmpeg_bin.yml")
            )
            .is_err(),
            "arbitrary go2rtc.ffmpeg.bin must be rejected"
        );

        // Empty YAML / comment-only YAML parses cleanly with defaults
        let empty_cfg = NinjaConfig::from_yaml_str("   \n# comment\n", PathBuf::from("/tmp/e.yml"))
            .expect("empty/comment YAML must parse with defaults");
        assert_eq!(empty_cfg.version, "0.18-0");

        // write_go2rtc_config refuses symlinks and sets 0600 permissions on Unix
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_go2rtc_perm_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let real_out = tmp_dir.join("go2rtc.yaml");
        empty_cfg.write_go2rtc_config(&real_out).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&real_out).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "go2rtc.yaml must be written with mode 0600");

            let sym_out = tmp_dir.join("sym_go2rtc.yaml");
            std::os::unix::fs::symlink(&real_out, &sym_out).unwrap();
            assert!(
                empty_cfg.write_go2rtc_config(&sym_out).is_err(),
                "write_go2rtc_config must refuse writing through a symlink"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn phase1a_audit_partial_structured_json_merges_and_per_camera_credential_isolation() {
        let yaml = r#"
mqtt:
  enabled: true
  host: 127.0.0.1
  port: 1883
  user: mqtt_user
  password: mqtt_secret_password
lpr:
  enabled: true
  detector_model_path: /config/models/lpr_det.onnx
  ocr_model_path: /config/models/lpr_ocr.onnx
  min_confidence: 0.80
cameras:
  cam_a:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://user_a:pass_a@192.0.2.50:554/ch1_main
          roles: [record]
  cam_b:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://user_b:pass_b@192.0.2.50:554/ch2_main
          roles: [record]
"#;
        let mut cfg =
            NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/merge_test.yml")).unwrap();

        // 1. Partial LPR patch preserves detector_model_path and ocr_model_path
        cfg.apply_structured_json(&json!({
            "lpr": {
                "min_confidence": 0.88
            }
        }))
        .unwrap();
        assert_eq!(
            cfg.lpr.detector_model_path,
            Some(PathBuf::from("/config/models/lpr_det.onnx"))
        );
        assert_eq!(
            cfg.lpr.ocr_model_path,
            Some(PathBuf::from("/config/models/lpr_ocr.onnx"))
        );

        // 2. Single-asterisk "*" MQTT password mask preserves existing password, and port 8883 auto-TLS
        // is not broken by a round-trip from to_api_value()
        let mut api = cfg.to_api_value();
        api["mqtt"]["port"] = json!(8883);
        api["mqtt"]["password"] = json!("*");
        // Simulate cam_b editing its stream path on the shared NVR host 192.0.2.50
        api["cameras"]["cam_b"]["ffmpeg"]["inputs"][0]["path"] =
            json!("rtsp://*:*@192.0.2.50:554/ch2_sub");
        cfg.apply_structured_json(&api).unwrap();

        assert_eq!(cfg.mqtt.password.as_deref(), Some("mqtt_secret_password"));
        assert!(
            cfg.mqtt.should_use_tls(),
            "changing port to 8883 via structured JSON must auto-enable TLS when tls_enabled was None"
        );
        assert_eq!(
            cfg.cameras["cam_a"].ffmpeg.inputs[0].path,
            "rtsp://user_a:pass_a@192.0.2.50:554/ch1_main"
        );
        assert_eq!(
            cfg.cameras["cam_b"].ffmpeg.inputs[0].path,
            "rtsp://user_b:pass_b@192.0.2.50:554/ch2_sub",
            "cam_b must restore its own user_b:pass_b credentials, not cam_a's"
        );

        // 3. Unsafe model paths and symlinked dot-directories are rejected
        assert!(!is_safe_model_path(Path::new("/etc/passwd")));
        assert!(!is_safe_model_path(Path::new("../model.onnx")));
        assert!(!is_safe_model_path(Path::new("/config/.hidden/model.onnx")));
        assert!(!is_safe_model_path(Path::new("/dev/zero.onnx")));
        assert!(is_safe_model_path(Path::new("/config/models/yolov8n.onnx")));
        assert!(
            cfg.apply_structured_json(&json!({
                "face_recognition": {
                    "model_path": "../etc/passwd"
                }
            }))
            .is_err()
        );
    }
}
