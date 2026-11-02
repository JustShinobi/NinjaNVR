#![recursion_limit = "512"]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub const DEFAULT_CONFIG_PATH: &str = "/config/config.yml";
pub const DEFAULT_DB_PATH: &str = "/config/frigate.db";
pub const DEFAULT_GO2RTC_SHM_PATH: &str = "/dev/shm/go2rtc.yaml";
pub const DEFAULT_GO2RTC_HOMEKIT_PATH: &str = "/config/go2rtc_homekit.yml";
pub const NINJANVR_VERSION: &str = match option_env!("NINJANVR_RELEASE_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileOverrideConfig {
    #[serde(default)]
    pub disabled_alerts_cameras: Vec<String>,
    #[serde(default)]
    pub disabled_record_cameras: Vec<String>,
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
    pub profiles: BTreeMap<String, ProfileOverrideConfig>,
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
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

fn default_mqtt_host() -> String {
    "127.0.0.1".to_string()
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_mqtt_prefix() -> String {
    "frigate".to_string()
}
fn default_mqtt_client_id() -> String {
    "frigate".to_string()
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
    "frigate_token".to_string()
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
    pub cold_dir: Option<PathBuf>,
    #[serde(default = "default_migrate_after_hours")]
    pub migrate_after_hours: u32,
}

fn default_migrate_after_hours() -> u32 {
    24
}

impl Default for StorageTiersConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cold_dir: None,
            migrate_after_hours: default_migrate_after_hours(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectsConfig {
    #[serde(default = "default_tracked_objects")]
    pub track: Vec<String>,
    #[serde(default)]
    pub filters: BTreeMap<String, ObjectFilterConfig>,
    #[serde(default)]
    pub mask: Value,
}

fn default_tracked_objects() -> Vec<String> {
    vec!["person".to_string()]
}

impl Default for ObjectsConfig {
    fn default() -> Self {
        let mut filters = BTreeMap::new();
        filters.insert("person".to_string(), ObjectFilterConfig::default());
        Self {
            track: default_tracked_objects(),
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
        let min_px = if self.min_area > 0.0 && self.min_area <= 1.0 {
            (self.min_area * frame_area).round() as u32
        } else {
            self.min_area.max(0.0).round() as u32
        };
        let max_px = if self.max_area > 0.0 && self.max_area <= 1.0 {
            (self.max_area * frame_area).round() as u32
        } else {
            self.max_area.max(1.0).round() as u32
        };
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
    -24
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub indoor: bool,
    #[serde(default)]
    pub map: CameraMapPlacement,
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
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            indoor: false,
            map: CameraMapPlacement::default(),
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
        let mut cfg: NinjaConfig =
            serde_yaml::from_str(raw_yaml).context("failed to parse config YAML")?;
        cfg.raw_yaml = raw_yaml.to_string();
        cfg.config_path = config_path;

        if cfg.detectors.is_empty() {
            cfg.detectors
                .insert("detector01".to_string(), DetectorEntryConfig::default());
        }

        for (cam_name, cam) in cfg.cameras.iter_mut() {
            if cam.live.streams.is_empty() {
                cam.live
                    .streams
                    .insert("Stream 1".to_string(), format!("{cam_name}_sub"));
                cam.live
                    .streams
                    .insert("Stream 2".to_string(), cam_name.clone());
            }
            if cam.objects.track.is_empty() {
                cam.objects.track = cfg.objects.track.clone();
            }
            if !cam.record.alert_backup.enabled && cfg.record.alert_backup.enabled {
                cam.record.alert_backup = cfg.record.alert_backup.clone();
            }
        }

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
        let target_path = target_path.as_ref();
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
            root.entry(k.clone()).or_insert_with(|| v.clone());
        }

        let yaml_str = serde_yaml::to_string(&Value::Object(root))?;
        std::fs::write(target_path, yaml_str)?;
        Ok(())
    }

    pub fn load_labelmap(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        let mut candidates = vec![PathBuf::from(&self.model.labelmap_path)];
        if let Ok(env_path) = std::env::var("NINJANVR_LABELMAP_PATH") {
            candidates.push(PathBuf::from(env_path));
        }
        for candidate in candidates {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                for (idx, line) in content.lines().enumerate() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
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

    /// Generates the complete Frigate 0.18 compatible `/api/config` JSON object expected by the React SPA.
    pub fn to_frigate_api_config(&self) -> Value {
        let labelmap = self.load_labelmap();
        let attributes_map = json!({
            "person": ["amazon", "face"],
            "car": ["amazon", "an_post", "canada_post", "dhl", "dpd", "fedex", "gls", "license_plate", "nzpost", "postnl", "postnord", "purolator", "royal_mail", "ups", "usps"],
            "truck": ["license_plate"],
            "garbage_truck": ["license_plate"],
            "motorcycle": ["license_plate"],
            "bus": ["license_plate"],
            "school_bus": ["license_plate"]
        });

        let mut detectors_json = serde_json::Map::new();
        for (name, det) in &self.detectors {
            detectors_json.insert(
                name.clone(),
                json!({
                    "type": det.detector_type,
                    "device": det.device,
                    "model": {
                        "path": self.model.path,
                        "labelmap_path": self.model.labelmap_path,
                        "width": self.model.width,
                        "height": self.model.height,
                        "labelmap": labelmap,
                        "attributes_map": attributes_map,
                        "input_tensor": self.model.input_tensor,
                        "input_pixel_format": self.model.input_pixel_format,
                        "input_dtype": "int",
                        "model_type": self.model.model_type
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
                    "type": "generic",
                    "best_image_timeout": 60,
                    "audio": {
                        "enabled": false,
                        "enabled_in_config": false,
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
                        "max_disappeared": cam.detect.max_disappeared,
                        "stationary": {
                            "interval": cam.detect.stationary.interval,
                            "threshold": cam.detect.stationary.threshold,
                            "max_frames": { "objects": {} },
                            "classifier": cam.detect.stationary.classifier
                        },
                        "annotation_offset": 0
                    },
                    "face_recognition": {
                        "enabled": false,
                        "min_area": 750
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
                        "gpu": 0,
                        "inputs": inputs_json
                    },
                    "live": {
                        "streams": cam.live.streams,
                        "height": cam.live.height,
                        "quality": cam.live.quality
                    },
                    "lpr": {
                        "enabled": false,
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
                        "continuous": { "days": cam.record.continuous.days },
                        "motion": { "days": cam.record.motion.days },
                        "detections": {
                            "pre_capture": cam.record.detections.pre_capture,
                            "post_capture": cam.record.detections.post_capture,
                            "retain": {
                                "days": cam.record.detections.retain.days,
                                "mode": cam.record.detections.retain.mode
                            }
                        },
                        "alerts": {
                            "pre_capture": cam.record.alerts.pre_capture,
                            "post_capture": cam.record.alerts.post_capture,
                            "retain": {
                                "days": cam.record.alerts.retain.days,
                                "mode": cam.record.alerts.retain.mode
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
                        "quality": cam.snapshots.quality
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
                        "host": "",
                        "port": 8000,
                        "tls_insecure": false,
                        "ignore_time_mismatch": false,
                        "autotracking": {
                            "enabled": false,
                            "enabled_in_config": false,
                            "calibrate_on_startup": false,
                            "zooming": "disabled",
                            "zoom_factor": 0.3,
                            "track": ["person"],
                            "required_zones": [],
                            "return_preset": "home",
                            "timeout": 10,
                            "movement_weights": []
                        }
                    },
                    "ui": {
                        "order": order_idx,
                        "dashboard": true,
                        "review": true
                    },
                    "map": cam.map,
                    "profiles": {},
                    "zones": cam.zones,
                    "lines": cam.lines,
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
                    "issuer_url": self.auth.oidc.issuer_url,
                    "client_id": self.auth.oidc.client_id,
                    "client_secret_env": self.auth.oidc.client_secret_env,
                    "scopes": self.auth.oidc.scopes,
                    "auto_redirect": self.auth.oidc.auto_redirect,
                    "username_claim": self.auth.oidc.username_claim,
                    "groups_claim": self.auth.oidc.groups_claim,
                    "role_mapping": self.auth.oidc.role_mapping,
                    "default_role": self.auth.oidc.default_role
                },
                "proxy": self.auth.proxy
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
                "has_password": self.mqtt.password.as_ref().is_some_and(|p| !p.is_empty()),
                "stats_interval": self.mqtt.stats_interval,
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
            "tls": { "enabled": self.tls.enabled },
            "ui": {
                "time_format": "browser",
                "unit_system": "metric"
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
            "genai": {},
            "cameras": cameras_json,
            "audio": {
                "enabled": false,
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
                "max_disappeared": self.detect.max_disappeared,
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
                "continuous": { "days": self.record.continuous.days },
                "motion": { "days": self.record.motion.days },
                "detections": {
                    "pre_capture": self.record.detections.pre_capture,
                    "post_capture": self.record.detections.post_capture,
                    "retain": {
                        "days": self.record.detections.retain.days,
                        "mode": self.record.detections.retain.mode
                    }
                },
                "alerts": {
                    "pre_capture": self.record.alerts.pre_capture,
                    "post_capture": self.record.alerts.post_capture,
                    "retain": {
                        "days": self.record.alerts.retain.days,
                        "mode": self.record.alerts.retain.mode
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
                "quality": self.snapshots.quality
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
                "enabled": false,
                "model_size": "small",
                "unknown_score": 0.8,
                "detection_threshold": 0.7,
                "recognition_threshold": 0.9,
                "min_area": 750,
                "min_faces": 1,
                "save_attempts": 200,
                "blur_confidence_filter": true
            },
            "lpr": {
                "enabled": false,
                "model_size": "small",
                "detection_threshold": 0.7,
                "min_area": 1000,
                "recognition_threshold": 0.9,
                "min_plate_length": 4,
                "match_distance": 1,
                "known_plates": {},
                "enhancement": 0,
                "debug_save_plates": false,
                "replace_rules": []
            },
            "camera_groups": {},
            "profiles": {},
            "plus": { "enabled": false }
        })
    }

    /// Collects all raw URL strings currently stored in `cameras.*.ffmpeg.inputs` and `go2rtc.streams`.
    pub fn collect_original_urls(&self) -> Vec<String> {
        let mut out = Vec::new();
        for cam in self.cameras.values() {
            for inp in &cam.ffmpeg.inputs {
                if !inp.path.is_empty() {
                    out.push(inp.path.clone());
                }
            }
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
        out
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

        if let Some(mqtt_val) = obj.get("mqtt") {
            let prev_pass = self.mqtt.password.clone();
            let clear_pass = mqtt_val
                .get("clear_password")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut next_mqtt: MqttConfig =
                serde_json::from_value(mqtt_val.clone()).context("invalid mqtt config")?;
            if clear_pass {
                next_mqtt.password = None;
            } else if next_mqtt
                .password
                .as_deref()
                .map(|p| p.is_empty() || p == "***")
                .unwrap_or(true)
            {
                next_mqtt.password = prev_pass;
            }
            self.mqtt = next_mqtt;
        }

        if let Some(auth_val) = obj.get("auth") {
            self.auth = serde_json::from_value(auth_val.clone()).context("invalid auth config")?;
        }
        if let Some(tls_val) = obj.get("tls") {
            self.tls = serde_json::from_value(tls_val.clone()).context("invalid tls config")?;
        }
        if let Some(db_val) = obj.get("database") {
            self.database =
                serde_json::from_value(db_val.clone()).context("invalid database config")?;
        }
        if let Some(det_val) = obj.get("detectors") {
            let next_dets: BTreeMap<String, DetectorEntryConfig> =
                serde_json::from_value(det_val.clone()).context("invalid detectors config")?;
            if !next_dets.is_empty() {
                self.detectors = next_dets;
            }
        }
        if let Some(model_val) = obj.get("model") {
            self.model =
                serde_json::from_value(model_val.clone()).context("invalid model config")?;
        }
        if let Some(detect_val) = obj.get("detect") {
            self.detect =
                serde_json::from_value(detect_val.clone()).context("invalid detect config")?;
        }
        if let Some(motion_val) = obj.get("motion") {
            self.motion =
                serde_json::from_value(motion_val.clone()).context("invalid motion config")?;
        }
        if let Some(objects_val) = obj.get("objects") {
            self.objects =
                serde_json::from_value(objects_val.clone()).context("invalid objects config")?;
        }
        if let Some(record_val) = obj.get("record") {
            self.record =
                serde_json::from_value(record_val.clone()).context("invalid record config")?;
        }
        if let Some(snap_val) = obj.get("snapshots") {
            self.snapshots =
                serde_json::from_value(snap_val.clone()).context("invalid snapshots config")?;
        }
        if let Some(rev_val) = obj.get("review") {
            self.review =
                serde_json::from_value(rev_val.clone()).context("invalid review config")?;
        }
        if let Some(go2rtc_val) = obj.get("go2rtc")
            && let Some(streams_val) = go2rtc_val.get("streams")
        {
            self.go2rtc.streams = serde_json::from_value(streams_val.clone())
                .context("invalid go2rtc.streams config")?;
        }
        if let Some(cams_val) = obj.get("cameras") {
            let mut next_cams: BTreeMap<String, CameraConfig> =
                serde_json::from_value(cams_val.clone()).context("invalid cameras config")?;
            for (cam_name, cam) in next_cams.iter_mut() {
                if cam.live.streams.is_empty() {
                    cam.live
                        .streams
                        .insert("Stream 1".to_string(), format!("{cam_name}_sub"));
                    cam.live
                        .streams
                        .insert("Stream 2".to_string(), cam_name.clone());
                }
                if cam.objects.track.is_empty() {
                    cam.objects.track = self.objects.track.clone();
                }
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

/// Restores credentials into a scrubbed RTSP/HTTP URL (`rtsp://*:*@host/path`) if a matching original exists.
pub fn restore_scrubbed_url(input: &str, originals: &[String]) -> String {
    if !input.contains("://*:*@") && !input.contains("://*@") {
        return input.to_string();
    }
    // 1. Exact scrubbed match
    for orig in originals {
        if scrub_rtsp_credentials(orig) == input {
            return orig.clone();
        }
    }
    // 2. Match by scheme + host authority if path was edited
    if let Some(scheme_idx) = input.find("://") {
        let after_scheme = &input[scheme_idx + 3..];
        if let Some(at_idx) = after_scheme.find('@') {
            let rest = &after_scheme[at_idx + 1..];
            let host_end = rest.find(['/', '?', '#', ':']).unwrap_or(rest.len());
            let target_host = &rest[..host_end];
            for orig in originals {
                if let Some(orig_scheme_idx) = orig.find("://") {
                    let orig_after = &orig[orig_scheme_idx + 3..];
                    let orig_auth_end =
                        orig_after.find(['/', '?', '#']).unwrap_or(orig_after.len());
                    let orig_auth = &orig_after[..orig_auth_end];
                    if let Some(orig_at) = orig_auth.rfind('@') {
                        let orig_creds = &orig_auth[..orig_at];
                        let orig_host_part = &orig_auth[orig_at + 1..];
                        let orig_host_end =
                            orig_host_part.find(':').unwrap_or(orig_host_part.len());
                        if &orig_host_part[..orig_host_end] == target_host {
                            return format!("{}{}@{}", &input[..scheme_idx + 3], orig_creds, rest);
                        }
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

/// Scrubs `user:password@` credentials from RTSP/HTTP URLs before returning them in API responses.
pub fn scrub_rtsp_credentials(input: &str) -> String {
    let Some(scheme_idx) = input.find("://") else {
        return input.to_string();
    };
    let after_scheme = &input[scheme_idx + 3..];
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let Some(at_idx) = authority.rfind('@') else {
        return input.to_string();
    };
    let userinfo = &authority[..at_idx];
    if userinfo.contains(':') {
        format!(
            "{}*:*@{}",
            &input[..scheme_idx + 3],
            &after_scheme[at_idx + 1..]
        )
    } else {
        format!(
            "{}*@{}",
            &input[..scheme_idx + 3],
            &after_scheme[at_idx + 1..]
        )
    }
}

/// Recursively scrubs credentials from any URL strings inside a JSON `Value`.
pub fn scrub_json_value(val: &Value) -> Value {
    match val {
        Value::String(s) => Value::String(scrub_rtsp_credentials(s)),
        Value::Array(arr) => Value::Array(arr.iter().map(scrub_json_value).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), scrub_json_value(v));
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
}

impl Go2RtcSupervisor {
    pub fn new(config: Arc<RwLock<NinjaConfig>>) -> Self {
        let mut bin_candidates = Vec::new();
        if let Ok(env_bin) = std::env::var("NINJANVR_GO2RTC_BIN") {
            bin_candidates.push(PathBuf::from(env_bin));
        }
        bin_candidates.push(PathBuf::from("/usr/local/go2rtc/bin/go2rtc"));
        bin_candidates.push(PathBuf::from("/usr/local/bin/go2rtc"));

        let go2rtc_bin = bin_candidates
            .into_iter()
            .find(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("go2rtc"));

        Self {
            config,
            go2rtc_bin,
            shm_config_path: PathBuf::from(DEFAULT_GO2RTC_SHM_PATH),
            homekit_config_path: PathBuf::from(DEFAULT_GO2RTC_HOMEKIT_PATH),
            pid: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::Relaxed)
    }

    pub fn spawn_supervisor_task(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                {
                    let cfg = self.config.read().await;
                    if let Err(err) = cfg.write_go2rtc_config(&self.shm_config_path) {
                        error!("Failed to write go2rtc config: {err:#}");
                    }
                }

                if !self.go2rtc_bin.exists() {
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
    fn parses_frigate_config_and_generates_api_payload() {
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

        let api_val = cfg.to_frigate_api_config();
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
        let mut api_payload = cfg.to_frigate_api_config();

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
        let api_json = cfg.to_frigate_api_config();
        assert_eq!(api_json["cameras"]["front_door"]["map"]["fov_deg"], 110.0);
    }
}
