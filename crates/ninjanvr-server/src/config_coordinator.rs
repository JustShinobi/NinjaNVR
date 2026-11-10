//! Transactional configuration persistence, instance-scoped commit serialization,
//! and camera hot-reload coordination.
//!
//! `ConfigCoordinator` replaces the process-global `CONFIG_COMMIT_LOCKS` singleton and
//! the scattered `state.config.write().await` + `refresh_raw_yaml()` +
//! `persist_config_yaml_atomically()` sequences across HTTP handlers with an
//! `AppState`-owned deep module. Every mutation acquires the coordinator's instance-scoped
//! commit lock, clones and transforms `NinjaConfig` without holding the `RwLock` write
//! guard across blocking disk I/O, persists `config.yml` (mode `0o600`, `O_NOFOLLOW`,
//! `sync_all`, atomic rename) and `go2rtc.yaml`, swaps the in-memory config, and
//! optionally invalidates cached per-camera driver state in `CameraDriverRuntime`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use ninjanvr_config::{DEFAULT_GO2RTC_SHM_PATH, NinjaConfig};
use ninjanvr_infer::AiDetectorEngine;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};
use tracing::warn;

use crate::camera_protocols::CameraDriverRuntime;

/// Typed error returned by transactional `ConfigCoordinator` commit operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigCommitError {
    Validation(String),
    Persistence(String),
}

impl std::fmt::Display for ConfigCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(msg) | Self::Persistence(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ConfigCommitError {}

/// Outcome of a structured configuration patch commit.
#[derive(Debug, Clone)]
pub struct StructuredConfigCommitOutcome {
    pub config: NinjaConfig,
    pub api_config: Value,
    pub scrubbed_raw_yaml: String,
    pub invalidated_cameras: Vec<String>,
}

/// Outcome of a detector model deployment commit.
#[derive(Debug, Clone)]
pub enum ModelDeployOutcome {
    Deployed { loaded: bool, persisted: bool },
    RolledBack { previous_model_path: String },
}

/// Validates that `raw` is a safe `.txt` labelmap path without `..`, dotfiles, symlinks,
/// or sensitive system prefixes.
pub fn is_safe_labelmap_path_str(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.contains("..")
        || trimmed.contains('\\')
        || trimmed.chars().any(|c| c.is_control())
    {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    for forbidden_prefix in [
        "/proc",
        "/sys",
        "/etc",
        "/dev",
        "/root",
        "/var/run",
        "/run/secrets",
    ] {
        if lower == forbidden_prefix || lower.starts_with(&format!("{forbidden_prefix}/")) {
            return false;
        }
    }
    let path = Path::new(trimmed);
    if !path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
    {
        return false;
    }
    let mut normal_segments: Vec<String> = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir | std::path::Component::CurDir => return false,
            std::path::Component::Normal(os) => {
                let Some(s) = os.to_str() else {
                    return false;
                };
                if s.starts_with('.') {
                    return false;
                }
                normal_segments.push(s.to_ascii_lowercase());
            }
            _ => {}
        }
    }
    if path.is_absolute()
        && let Some(first) = normal_segments.first().map(String::as_str)
    {
        if matches!(first, "proc" | "sys" | "etc" | "dev" | "root") {
            return false;
        }
        let second = normal_segments.get(1).map(String::as_str);
        if (first == "var" && second == Some("run"))
            || (first == "run" && second == Some("secrets"))
        {
            return false;
        }
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return false;
    }
    if let Ok(canon) = std::fs::canonicalize(path) {
        let canon_str = canon.to_string_lossy().to_ascii_lowercase();
        for forbidden_prefix in [
            "/proc",
            "/sys",
            "/etc",
            "/dev",
            "/root",
            "/var/run",
            "/run/secrets",
        ] {
            if canon_str == forbidden_prefix
                || canon_str.starts_with(&format!("{forbidden_prefix}/"))
            {
                return false;
            }
        }
        if !canon
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
        {
            return false;
        }
        if canon
            .file_name()
            .and_then(|f| f.to_str())
            .is_none_or(|f| f.starts_with('.'))
        {
            return false;
        }
    }
    true
}

/// Validates both the primary `model.labelmap_path` and every detector's `model.labelmap_path`.
pub fn validate_config_labelmap_paths(cfg: &NinjaConfig) -> std::result::Result<(), &'static str> {
    if !is_safe_labelmap_path_str(&cfg.model.labelmap_path) {
        return Err(
            "Invalid model.labelmap_path: must be a safe .txt path without '..', dotfiles, or system paths",
        );
    }
    for det in cfg.detectors.values() {
        if let Some(ref m) = det.model
            && !is_safe_labelmap_path_str(&m.labelmap_path)
        {
            return Err(
                "Invalid detector model.labelmap_path: must be a safe .txt path without '..', dotfiles, or system paths",
            );
        }
    }
    Ok(())
}

/// Returns `true` when a YAML mapping key holds a secret or credential that must be scrubbed.
pub fn is_sensitive_yaml_secret_key_name(key: &str) -> bool {
    matches!(
        key.trim()
            .trim_matches('"')
            .trim_matches('\'')
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "password"
            | "cloud_password"
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
            | "webhook_secret"
            | "jwt_secret"
    )
}

fn collect_yaml_sensitive_secrets(val: &serde_yaml::Value, out: &mut Vec<String>) {
    match val {
        serde_yaml::Value::Mapping(map) => {
            for (k, v) in map {
                if let Some(k_str) = k.as_str()
                    && is_sensitive_yaml_secret_key_name(k_str)
                {
                    let scalar_opt = match v {
                        serde_yaml::Value::String(s) => Some(s.clone()),
                        serde_yaml::Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    };
                    if let Some(s) = scalar_opt {
                        let t = s.trim();
                        if !t.is_empty()
                            && !t.eq_ignore_ascii_case("null")
                            && t != "~"
                            && t != "***"
                            && t != "*"
                        {
                            out.push(t.to_string());
                            if t.contains('"') {
                                out.push(t.replace('"', "\\\""));
                            }
                            if t.contains('\'') {
                                out.push(t.replace('\'', "''"));
                            }
                            for line in s.lines() {
                                let lt = line.trim();
                                if lt.len() >= 3 && lt != "***" {
                                    out.push(lt.to_string());
                                }
                            }
                        }
                    }
                }
                collect_yaml_sensitive_secrets(v, out);
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            for item in seq {
                collect_yaml_sensitive_secrets(item, out);
            }
        }
        _ => {}
    }
}

/// Scrubs RTSP/HTTP URL credentials, sensitive YAML keys (including multiline block scalars),
/// and known secret values from `raw_yaml`.
pub fn scrub_raw_yaml_secrets_strict(raw_yaml: &str, cfg: &NinjaConfig) -> String {
    let base_scrubbed = ninjanvr_config::scrub_yaml_credentials(raw_yaml);
    let mut out = String::with_capacity(base_scrubbed.len());
    let mut skip_block_scalar_indent: Option<usize> = None;
    for chunk in base_scrubbed.split_inclusive('\n') {
        let (line_body, newline) = if let Some(stripped) = chunk.strip_suffix("\r\n") {
            (stripped, "\r\n")
        } else if let Some(stripped) = chunk.strip_suffix('\n') {
            (stripped, "\n")
        } else {
            (chunk, "")
        };
        let trimmed_start = line_body.trim_start();
        let line_indent = line_body.len() - trimmed_start.len();
        if let Some(base_indent) = skip_block_scalar_indent {
            if trimmed_start.is_empty() || line_indent > base_indent {
                continue;
            }
            skip_block_scalar_indent = None;
        }
        if trimmed_start.starts_with('#') || trimmed_start.is_empty() {
            out.push_str(chunk);
            continue;
        }
        let after_list = trimmed_start
            .strip_prefix("- ")
            .map_or(trimmed_start, |s| s.trim_start());
        let prefix_len = line_body.len() - after_list.len();
        let leading_prefix = &line_body[..prefix_len];
        if let Some((raw_k, raw_v)) = after_list.split_once(':')
            && is_sensitive_yaml_secret_key_name(raw_k)
        {
            let val_trimmed = raw_v.trim();
            let unquoted = val_trimmed.trim_matches('"').trim_matches('\'').trim();
            if unquoted.is_empty()
                || unquoted.starts_with('#')
                || unquoted.starts_with('|')
                || unquoted.starts_with('>')
            {
                skip_block_scalar_indent = Some(line_indent);
                out.push_str(leading_prefix);
                out.push_str(raw_k.trim_end());
                out.push_str(": \"***\"");
                out.push_str(newline);
                continue;
            }
            if !unquoted.eq_ignore_ascii_case("null") && unquoted != "~" && unquoted != "***" {
                out.push_str(leading_prefix);
                out.push_str(raw_k.trim_end());
                out.push_str(": \"***\"");
                out.push_str(newline);
                continue;
            }
        }
        out.push_str(line_body);
        out.push_str(newline);
    }

    let mut known_secrets: Vec<String> = Vec::new();
    if let Ok(parsed_yaml) = serde_yaml::from_str::<serde_yaml::Value>(raw_yaml) {
        collect_yaml_sensitive_secrets(&parsed_yaml, &mut known_secrets);
    }
    if let Some(ref p) = cfg.mqtt.password {
        let t = p.trim();
        if !t.is_empty() && t != "***" && t != "*" {
            known_secrets.push(t.to_string());
        }
    }
    if let Some(ref k) = cfg.genai.api_key {
        let t = k.trim();
        if !t.is_empty() && t != "***" && t != "*" {
            known_secrets.push(t.to_string());
        }
    }
    for cam in cfg.cameras.values() {
        if let Some(ref p) = cam.onvif.password {
            let t = p.trim();
            if !t.is_empty() && t != "***" && t != "*" {
                known_secrets.push(t.to_string());
            }
        }
        if let Some(ref cp) = cam.hardware.cloud_password {
            let t = cp.trim();
            if !t.is_empty() && t != "***" && t != "*" {
                known_secrets.push(t.to_string());
            }
        }
    }
    for secret in &known_secrets {
        if secret.len() >= 3 && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "***");
        }
    }
    out
}

/// Builds a sanitized, camera-scoped JSON representation of `cfg` for `/api/config`.
pub fn to_scoped_api_config(cfg: &NinjaConfig, allowed_cameras: Option<&[String]>) -> Value {
    let mut api_cfg = if validate_config_labelmap_paths(cfg).is_ok() {
        cfg.to_api_config()
    } else {
        let mut safe_clone = cfg.clone();
        if !is_safe_labelmap_path_str(&safe_clone.model.labelmap_path) {
            safe_clone.model.labelmap_path = "/openvino-model/coco_91cl_bkgr.txt".to_string();
        }
        for det in safe_clone.detectors.values_mut() {
            if let Some(ref mut m) = det.model
                && !is_safe_labelmap_path_str(&m.labelmap_path)
            {
                m.labelmap_path = "/openvino-model/coco_91cl_bkgr.txt".to_string();
            }
        }
        safe_clone.to_api_config()
    };

    if let Some(allowed) = allowed_cameras {
        let is_cam_permitted = |cam_name: &str| allowed.iter().any(|a| a == cam_name || a == "*");
        if let Some(cams_obj) = api_cfg.get_mut("cameras").and_then(|v| v.as_object_mut()) {
            cams_obj.retain(|k, _| is_cam_permitted(k));
        }
        if let Some(streams_obj) = api_cfg
            .get_mut("go2rtc")
            .and_then(|g| g.get_mut("streams"))
            .and_then(|v| v.as_object_mut())
        {
            streams_obj.retain(|k, _| {
                let base_cam = k.strip_suffix("_sub").unwrap_or(k);
                is_cam_permitted(base_cam)
            });
        }
        if let Some(groups_obj) = api_cfg
            .get_mut("camera_groups")
            .and_then(|v| v.as_object_mut())
        {
            groups_obj.retain(|_, grp_val| {
                if let Some(cams_arr) = grp_val.get_mut("cameras").and_then(|v| v.as_array_mut()) {
                    cams_arr.retain(|c| c.as_str().is_some_and(is_cam_permitted));
                    !cams_arr.is_empty()
                } else {
                    false
                }
            });
        }
        if let Some(profiles_obj) = api_cfg.get_mut("profiles").and_then(|v| v.as_object_mut()) {
            for prof_val in profiles_obj.values_mut() {
                if let Some(prof_cams) = prof_val.get_mut("cameras").and_then(|v| v.as_object_mut())
                {
                    prof_cams.retain(|k, _| is_cam_permitted(k));
                }
                for arr_key in ["disabled_alerts_cameras", "disabled_record_cameras"] {
                    if let Some(arr) = prof_val.get_mut(arr_key).and_then(|v| v.as_array_mut()) {
                        arr.retain(|c| c.as_str().is_some_and(is_cam_permitted));
                    }
                }
            }
        }
        if let Some(rooms_arr) = api_cfg
            .get_mut("ui")
            .and_then(|u| u.get_mut("floorplan"))
            .and_then(|f| f.get_mut("rooms"))
            .and_then(|v| v.as_array_mut())
        {
            for room in rooms_arr.iter_mut() {
                if let Some(room_obj) = room.as_object_mut()
                    && let Some(cam_str) = room_obj.get("camera").and_then(|v| v.as_str())
                    && !cam_str.is_empty()
                    && !is_cam_permitted(cam_str)
                {
                    room_obj.insert("camera".to_string(), Value::Null);
                }
            }
        }
    }
    api_cfg
}

/// A deploy is rolled back only when it would take a working detector offline: the new model
/// failed to load while the previous one was running. When no model was loaded before (no
/// OpenVINO runtime, first deploy) the new config is kept so a later runtime fix picks it up.
pub fn should_rollback_model_deploy(was_loaded: bool, now_loaded: bool) -> bool {
    was_loaded && !now_loaded
}

/// Computes which cameras require `CameraDriverRuntime` cache invalidation when transitioning
/// from `prev_cfg` to `next_cfg`.
pub fn diff_driver_invalidated_cameras(
    prev_cfg: &NinjaConfig,
    next_cfg: &NinjaConfig,
) -> Vec<String> {
    let mut changed = Vec::new();
    for (cam, prev_cam) in &prev_cfg.cameras {
        match next_cfg.cameras.get(cam) {
            None => changed.push(cam.clone()),
            Some(next_cam) => {
                let onvif_diff = serde_json::to_value(&prev_cam.onvif).ok()
                    != serde_json::to_value(&next_cam.onvif).ok();
                let hw_diff = serde_json::to_value(&prev_cam.hardware).ok()
                    != serde_json::to_value(&next_cam.hardware).ok();
                let ffmpeg_diff = serde_json::to_value(&prev_cam.ffmpeg.inputs).ok()
                    != serde_json::to_value(&next_cam.ffmpeg.inputs).ok();
                let webui_diff = prev_cam.webui_url != next_cam.webui_url;
                let upstream_diff =
                    prev_cfg.infer_camera_upstream(cam) != next_cfg.infer_camera_upstream(cam);
                if onvif_diff || hw_diff || ffmpeg_diff || webui_diff || upstream_diff {
                    changed.push(cam.clone());
                }
            }
        }
    }
    for cam in next_cfg.cameras.keys() {
        if !prev_cfg.cameras.contains_key(cam) && !changed.contains(cam) {
            changed.push(cam.clone());
        }
    }
    changed
}

/// Atomically persists `raw_yaml` to `path` using a randomized `0o600` `O_NOFOLLOW`
/// temporary file in the same directory followed by `sync_all()` and `rename()`.
///
/// Rejects empty paths, parent-directory (`..`) traversals, symlinked target files,
/// target directories, and symlinked parent directories.
pub fn persist_config_yaml_atomically(path: &Path, raw_yaml: &str) -> Result<()> {
    use std::io::Write;
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "Refusing to persist configuration to unsafe path {}",
            path.display()
        );
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "Refusing to overwrite symlinked configuration file at {}",
                path.display()
            );
        }
        if meta.is_dir() {
            anyhow::bail!(
                "Refusing to overwrite directory at configuration path {}",
                path.display()
            );
        }
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if let Ok(pmeta) = std::fs::symlink_metadata(parent)
        && pmeta.file_type().is_symlink()
    {
        anyhow::bail!(
            "Refusing to persist configuration inside symlinked directory {}",
            parent.display()
        );
    }
    std::fs::create_dir_all(parent)?;
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("config.yml");
    let nonce: u64 = rand::random();
    let tmp_path = parent.join(format!(".{fname}.{nonce:016x}.tmp"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut tmp_file = opts.open(&tmp_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = tmp_file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
            drop(tmp_file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err.into());
        }
    }
    if let Err(err) = tmp_file
        .write_all(raw_yaml.as_bytes())
        .and_then(|()| tmp_file.sync_all())
    {
        drop(tmp_file);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    drop(tmp_file);
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (meta.file_type().is_symlink() || meta.is_dir())
    {
        let _ = std::fs::remove_file(&tmp_path);
        anyhow::bail!(
            "Refusing to rename configuration over symlink or directory at {}",
            path.display()
        );
    }
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err.into());
    }
    Ok(())
}

/// Instance-scoped coordinator that serializes all configuration commits and
/// synchronizes `config.yml`, `go2rtc.yaml`, and per-camera driver caches.
#[derive(Clone)]
pub struct ConfigCoordinator {
    config: Arc<RwLock<NinjaConfig>>,
    commit_lock: Arc<Mutex<()>>,
    go2rtc_config_path: PathBuf,
}

impl ConfigCoordinator {
    /// Creates a new `ConfigCoordinator` owning `config` and a fresh instance-scoped
    /// commit lock.
    pub fn new(config: Arc<RwLock<NinjaConfig>>) -> Self {
        Self {
            config,
            commit_lock: Arc::new(Mutex::new(())),
            go2rtc_config_path: PathBuf::from(DEFAULT_GO2RTC_SHM_PATH),
        }
    }

    /// Overrides the target path used when synchronizing `go2rtc.yaml` (useful for
    /// isolated unit tests with a temporary directory).
    pub fn with_go2rtc_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.go2rtc_config_path = path.into();
        self
    }

    /// Returns a reference to the underlying shared `NinjaConfig`.
    pub fn config(&self) -> &Arc<RwLock<NinjaConfig>> {
        &self.config
    }

    /// Persists `cfg.raw_yaml` atomically to `cfg.config_path` (when non-empty) and
    /// synchronizes the `go2rtc` configuration to `DEFAULT_GO2RTC_SHM_PATH`.
    pub fn persist_and_sync_go2rtc(cfg: &NinjaConfig) -> Result<()> {
        Self::persist_and_sync_go2rtc_with_path(cfg, Path::new(DEFAULT_GO2RTC_SHM_PATH))
    }

    /// Persists `cfg.raw_yaml` atomically to `cfg.config_path` (when non-empty) and
    /// writes `go2rtc_path` via `cfg.write_go2rtc_config` (when non-empty).
    pub fn persist_and_sync_go2rtc_with_path(cfg: &NinjaConfig, go2rtc_path: &Path) -> Result<()> {
        if !cfg.config_path.as_os_str().is_empty() {
            persist_config_yaml_atomically(&cfg.config_path, &cfg.raw_yaml)?;
        }
        if !go2rtc_path.as_os_str().is_empty() {
            if go2rtc_path == Path::new(DEFAULT_GO2RTC_SHM_PATH) {
                if !cfg.config_path.as_os_str().is_empty()
                    && let Err(err) = cfg.write_go2rtc_config(go2rtc_path)
                {
                    warn!(
                        "Failed to write go2rtc config to {}: {err:#}",
                        go2rtc_path.display()
                    );
                }
            } else {
                cfg.write_go2rtc_config(go2rtc_path)?;
            }
        }
        Ok(())
    }

    /// Atomically validates, restores scrubbed secrets from the previous config, persists
    /// `config.yml` and `go2rtc.yaml`, reloads `detector`, swaps the active `NinjaConfig`,
    /// and invalidates any modified cameras in `camera_runtime`.
    pub async fn commit_raw_yaml(
        &self,
        raw_yaml: &str,
        detector: Option<&Arc<AiDetectorEngine>>,
        camera_runtime: Option<&CameraDriverRuntime>,
    ) -> std::result::Result<NinjaConfig, ConfigCommitError> {
        let _commit_guard = self.commit_lock.lock().await;
        let prev_cfg = self.config.read().await.clone();
        let path = prev_cfg.config_path.clone();
        let mut next_cfg = NinjaConfig::from_yaml_str(raw_yaml, path)
            .map_err(|err| ConfigCommitError::Validation(err.to_string()))?;
        validate_config_labelmap_paths(&next_cfg)
            .map_err(|msg| ConfigCommitError::Validation(msg.to_string()))?;
        next_cfg.restore_scrubbed_from_previous(&prev_cfg);
        validate_config_labelmap_paths(&next_cfg)
            .map_err(|msg| ConfigCommitError::Validation(msg.to_string()))?;

        let invalidated_cameras = diff_driver_invalidated_cameras(&prev_cfg, &next_cfg);
        let go2rtc_target = self.go2rtc_config_path.clone();
        let det_opt = detector.cloned();
        let next_cfg =
            tokio::task::spawn_blocking(move || -> std::result::Result<NinjaConfig, String> {
                Self::persist_and_sync_go2rtc_with_path(&next_cfg, &go2rtc_target)
                    .map_err(|err| err.to_string())?;
                if let Some(det) = det_opt {
                    det.reload(&next_cfg);
                }
                Ok(next_cfg)
            })
            .await
            .map_err(|e| ConfigCommitError::Persistence(e.to_string()))?
            .map_err(ConfigCommitError::Persistence)?;

        *self.config.write().await = next_cfg.clone();
        if let Some(rt) = camera_runtime {
            for cam in &invalidated_cameras {
                rt.invalidate_camera(cam);
            }
        }
        Ok(next_cfg)
    }

    /// Atomically applies a structured JSON configuration patch, validates labelmap paths,
    /// persists `config.yml` and `go2rtc.yaml`, reloads `detector`, swaps the active `NinjaConfig`,
    /// and invalidates any modified cameras in `camera_runtime`.
    pub async fn commit_structured_patch(
        &self,
        patch: &Value,
        detector: Option<&Arc<AiDetectorEngine>>,
        camera_runtime: Option<&CameraDriverRuntime>,
    ) -> std::result::Result<StructuredConfigCommitOutcome, ConfigCommitError> {
        let _commit_guard = self.commit_lock.lock().await;
        let prev_cfg = self.config.read().await.clone();
        let mut next_cfg = prev_cfg.clone();
        next_cfg
            .apply_structured_json(patch)
            .map_err(|err| ConfigCommitError::Validation(format!("{err:#}")))?;
        validate_config_labelmap_paths(&next_cfg)
            .map_err(|msg| ConfigCommitError::Validation(msg.to_string()))?;

        let invalidated_cameras = diff_driver_invalidated_cameras(&prev_cfg, &next_cfg);
        let go2rtc_target = self.go2rtc_config_path.clone();
        let det_opt = detector.cloned();
        let (next_cfg, api_config, scrubbed_raw_yaml) = tokio::task::spawn_blocking(
            move || -> std::result::Result<(NinjaConfig, Value, String), String> {
                Self::persist_and_sync_go2rtc_with_path(&next_cfg, &go2rtc_target)
                    .map_err(|err| err.to_string())?;
                if let Some(det) = det_opt {
                    det.reload(&next_cfg);
                }
                let api_cfg = next_cfg.to_api_config();
                let raw_yaml = scrub_raw_yaml_secrets_strict(&next_cfg.raw_yaml, &next_cfg);
                Ok((next_cfg, api_cfg, raw_yaml))
            },
        )
        .await
        .map_err(|e| ConfigCommitError::Persistence(e.to_string()))?
        .map_err(ConfigCommitError::Persistence)?;

        *self.config.write().await = next_cfg.clone();
        if let Some(rt) = camera_runtime {
            for cam in &invalidated_cameras {
                rt.invalidate_camera(cam);
            }
        }
        Ok(StructuredConfigCommitOutcome {
            config: next_cfg,
            api_config,
            scrubbed_raw_yaml,
            invalidated_cameras,
        })
    }

    /// Atomically applies a detector `model_patch`, reloads `detector` with `labelmap`,
    /// rolls back `detector` if the new model would take a previously working detector offline,
    /// and otherwise persists `config.yml` and swaps the active `NinjaConfig`.
    pub async fn deploy_detector_model(
        &self,
        model_patch: &Value,
        labelmap: BTreeMap<i32, String>,
        detector: &Arc<AiDetectorEngine>,
    ) -> std::result::Result<ModelDeployOutcome, ConfigCommitError> {
        let _commit_guard = self.commit_lock.lock().await;
        let prev_cfg = self.config.read().await.clone();
        let mut next_cfg = prev_cfg.clone();
        next_cfg
            .apply_structured_json(model_patch)
            .map_err(|err| ConfigCommitError::Validation(format!("{err:#}")))?;
        validate_config_labelmap_paths(&next_cfg)
            .map_err(|msg| ConfigCommitError::Validation(msg.to_string()))?;

        let det_for_reload = detector.clone();
        let cfg_for_reload = next_cfg.clone();
        let (was_loaded, now_loaded) = tokio::task::spawn_blocking(move || {
            let was_loaded = det_for_reload.runtime_catalog().loaded;
            det_for_reload.reload(&cfg_for_reload);
            let now_loaded = det_for_reload.runtime_catalog().loaded;
            if !should_rollback_model_deploy(was_loaded, now_loaded) {
                det_for_reload.set_labelmap(labelmap);
            }
            (was_loaded, now_loaded)
        })
        .await
        .map_err(|e| ConfigCommitError::Persistence(format!("Detector reload task failed: {e}")))?;

        if should_rollback_model_deploy(was_loaded, now_loaded) {
            let det_for_rollback = detector.clone();
            let prev_for_rollback = prev_cfg.clone();
            let _ = tokio::task::spawn_blocking(move || {
                det_for_rollback.reload(&prev_for_rollback);
            })
            .await;
            return Ok(ModelDeployOutcome::RolledBack {
                previous_model_path: prev_cfg.model.path,
            });
        }

        let go2rtc_target = self.go2rtc_config_path.clone();
        let cfg_for_persist = next_cfg.clone();
        let persisted_res = tokio::task::spawn_blocking(move || {
            Self::persist_and_sync_go2rtc_with_path(&cfg_for_persist, &go2rtc_target)
        })
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))
        .and_then(|r| r);

        let persisted = match persisted_res {
            Ok(()) => true,
            Err(err) => {
                warn!("Uploaded model is active but config.yml could not be updated: {err:#}");
                false
            }
        };
        if persisted || next_cfg.config_path.as_os_str().is_empty() {
            *self.config.write().await = next_cfg;
        }
        Ok(ModelDeployOutcome::Deployed {
            loaded: now_loaded,
            persisted,
        })
    }

    /// Serializes a camera-scoped configuration mutation under the instance commit lock,
    /// refreshes `raw_yaml`, persists `config.yml` and `go2rtc.yaml` off the async
    /// executor thread before swapping `self.config`, and optionally invalidates the
    /// camera's cached runtime state in `CameraDriverRuntime`.
    pub async fn mutate_camera_and_persist<F, R>(
        &self,
        camera: &str,
        camera_runtime: Option<&CameraDriverRuntime>,
        invalidate_driver: bool,
        mutator: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut NinjaConfig) -> Result<R>,
    {
        let _commit_guard = self.commit_lock.lock().await;
        let mut next_cfg = self.config.read().await.clone();
        let result = mutator(&mut next_cfg)?;
        next_cfg.refresh_raw_yaml()?;

        let has_custom_go2rtc = self.go2rtc_config_path != Path::new(DEFAULT_GO2RTC_SHM_PATH)
            && !self.go2rtc_config_path.as_os_str().is_empty();
        let next_cfg = if next_cfg.config_path.as_os_str().is_empty() && !has_custom_go2rtc {
            Self::persist_and_sync_go2rtc(&next_cfg)?;
            next_cfg
        } else {
            let go2rtc_target = self.go2rtc_config_path.clone();
            tokio::task::spawn_blocking(move || -> Result<NinjaConfig> {
                Self::persist_and_sync_go2rtc_with_path(&next_cfg, &go2rtc_target)?;
                Ok(next_cfg)
            })
            .await
            .context("Configuration persistence blocking task failed")??
        };

        *self.config.write().await = next_cfg;
        if invalidate_driver && let Some(rt) = camera_runtime {
            rt.invalidate_camera(camera);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninjanvr_config::CameraSoftPreset;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempTestDir {
        path: PathBuf,
    }

    impl TempTestDir {
        fn new(prefix: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let nonce: u64 = rand::random();
            let path = std::env::temp_dir().join(format!(
                "ninjanvr_cfg_coord_{prefix}_{}_{nanos}_{nonce:016x}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp test dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn sample_config(config_path: PathBuf) -> NinjaConfig {
        let yaml = r#"
mqtt:
  enabled: false
cameras:
  front_door:
    enabled: true
    ffmpeg:
      inputs:
        - path: rtsp://admin:secret@192.0.2.10:554/stream1
          roles:
            - detect
            - record
"#;
        NinjaConfig::from_yaml_str(yaml, config_path).expect("valid sample config")
    }

    #[test]
    fn persist_config_yaml_atomically_enforces_permissions_and_rejects_unsafe_paths() {
        let dir = TempTestDir::new("atomic_persist");
        let cfg_path = dir.path().join("config.yml");
        let content_v1 = "mqtt:\n  enabled: false\ncameras: {}\n";
        let content_v2 = "mqtt:\n  enabled: true\ncameras: {}\n";

        persist_config_yaml_atomically(&cfg_path, content_v1).expect("initial atomic write");
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), content_v1);

        persist_config_yaml_atomically(&cfg_path, content_v2).expect("atomic overwrite");
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), content_v2);

        // Reject empty paths and parent-directory traversal (`..`)
        assert!(
            persist_config_yaml_atomically(Path::new(""), content_v1).is_err(),
            "empty path must be rejected"
        );
        let traversal_path = dir.path().join("nested").join("..").join("config.yml");
        assert!(
            persist_config_yaml_atomically(&traversal_path, content_v1).is_err(),
            "parent traversal path must be rejected"
        );

        // Reject overwriting a directory
        let sub_dir = dir.path().join("as_dir");
        std::fs::create_dir_all(&sub_dir).unwrap();
        assert!(
            persist_config_yaml_atomically(&sub_dir, content_v1).is_err(),
            "directory target must be rejected"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "persisted config.yml must have mode 0o600");

            // Reject overwriting a symlinked configuration file
            let secret_target = dir.path().join("protected_secret.txt");
            std::fs::write(&secret_target, b"DO_NOT_CLOBBER").unwrap();
            let symlink_cfg = dir.path().join("symlink_config.yml");
            std::os::unix::fs::symlink(&secret_target, &symlink_cfg).unwrap();
            assert!(
                persist_config_yaml_atomically(&symlink_cfg, content_v1).is_err(),
                "symlinked config target must be rejected"
            );
            assert_eq!(
                std::fs::read(&secret_target).unwrap(),
                b"DO_NOT_CLOBBER",
                "symlink target must remain untouched"
            );

            // Reject persisting inside a symlinked parent directory
            let real_parent = dir.path().join("real_parent");
            std::fs::create_dir_all(&real_parent).unwrap();
            let symlink_parent = dir.path().join("symlink_parent");
            std::os::unix::fs::symlink(&real_parent, &symlink_parent).unwrap();
            let inside_sym_parent = symlink_parent.join("config.yml");
            assert!(
                persist_config_yaml_atomically(&inside_sym_parent, content_v1).is_err(),
                "symlinked parent directory must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn concurrent_mutate_camera_and_persist_serializes_without_lost_updates() {
        let dir = TempTestDir::new("concurrent_mutations");
        let cfg_path = dir.path().join("config.yml");
        let go2rtc_path = dir.path().join("go2rtc.yaml");
        let initial_cfg = sample_config(cfg_path.clone());
        persist_config_yaml_atomically(&cfg_path, &initial_cfg.raw_yaml).unwrap();

        let shared_cfg = Arc::new(RwLock::new(initial_cfg));
        let coordinator =
            ConfigCoordinator::new(shared_cfg.clone()).with_go2rtc_config_path(&go2rtc_path);

        let total_presets = 16usize;
        let mut handles = Vec::with_capacity(total_presets);
        for idx in 0..total_presets {
            let coord = coordinator.clone();
            handles.push(tokio::spawn(async move {
                let token = format!("preset_{idx:02}");
                let name = format!("Preset {idx:02}");
                coord
                    .mutate_camera_and_persist("front_door", None, false, move |cfg| {
                        let cam = cfg
                            .cameras
                            .get_mut("front_door")
                            .context("front_door camera missing")?;
                        cam.hardware.soft_presets.insert(
                            token.clone(),
                            CameraSoftPreset {
                                name,
                                pan: (idx as f32) * 0.05,
                                tilt: -0.1,
                                zoom: 0.25,
                            },
                        );
                        Ok(token)
                    })
                    .await
            }));
        }

        for handle in handles {
            let token = handle
                .await
                .expect("task join")
                .expect("mutation succeeded");
            assert!(token.starts_with("preset_"));
        }

        let final_mem_cfg = coordinator.config().read().await.clone();
        let mem_presets = &final_mem_cfg.cameras["front_door"].hardware.soft_presets;
        assert_eq!(
            mem_presets.len(),
            total_presets,
            "all concurrent preset insertions must be preserved in memory without lost updates"
        );

        let reloaded_disk_cfg =
            NinjaConfig::load(&cfg_path).expect("reload persisted config.yml from disk");
        let disk_presets = &reloaded_disk_cfg.cameras["front_door"]
            .hardware
            .soft_presets;
        assert_eq!(
            disk_presets.len(),
            total_presets,
            "all concurrent preset insertions must be persisted on disk"
        );
        for idx in 0..total_presets {
            let key = format!("preset_{idx:02}");
            assert!(
                disk_presets.contains_key(&key),
                "missing persisted preset {key}"
            );
        }
        assert!(
            go2rtc_path.exists(),
            "go2rtc configuration file must be synchronized alongside config.yml"
        );
    }

    #[tokio::test]
    async fn mutate_camera_and_persist_invalidates_driver_runtime_only_when_requested() {
        let dir = TempTestDir::new("driver_invalidation");
        let cfg_path = dir.path().join("config.yml");
        let go2rtc_path = dir.path().join("go2rtc.yaml");
        let initial_cfg = sample_config(cfg_path.clone());
        let coordinator = ConfigCoordinator::new(Arc::new(RwLock::new(initial_cfg)))
            .with_go2rtc_config_path(&go2rtc_path);

        let runtime = CameraDriverRuntime::new();
        let endpoint = "http://192.0.2.10:2020/onvif/service";
        runtime.set_profile("front_door", endpoint, "profile_1");
        runtime.set_video_source("front_door", "raw_vs1");
        runtime.set_protocol("front_door", "tapo");
        runtime.set_profile("side_gate", endpoint, "Profile000");

        // 1. Mutation with invalidate_driver = false keeps cached runtime state intact
        coordinator
            .mutate_camera_and_persist("front_door", Some(&runtime), false, |cfg| {
                let cam = cfg.cameras.get_mut("front_door").unwrap();
                cam.hardware.soft_presets.insert(
                    "home".to_string(),
                    CameraSoftPreset {
                        name: "Home".to_string(),
                        pan: 0.0,
                        tilt: 0.0,
                        zoom: 0.0,
                    },
                );
                Ok(())
            })
            .await
            .expect("soft preset mutation");

        assert_eq!(
            runtime.get_profile("front_door", endpoint).as_deref(),
            Some("profile_1"),
            "cached profile must remain when invalidate_driver is false"
        );

        // 2. Failed mutation does not modify config or invalidate driver runtime
        let err_res: Result<()> = coordinator
            .mutate_camera_and_persist("front_door", Some(&runtime), true, |cfg| {
                let cam = cfg.cameras.get_mut("front_door").unwrap();
                cam.hardware.protocol = Some("broken_proto".to_string());
                anyhow::bail!("simulated validation error")
            })
            .await;
        assert!(err_res.is_err());
        assert_eq!(
            coordinator.config().read().await.cameras["front_door"]
                .hardware
                .protocol,
            None,
            "failed mutation must not leak into shared NinjaConfig"
        );
        assert_eq!(
            runtime.get_profile("front_door", endpoint).as_deref(),
            Some("profile_1"),
            "failed mutation must not invalidate CameraDriverRuntime"
        );

        // 3. Mutation with invalidate_driver = true clears cached state for target camera only
        coordinator
            .mutate_camera_and_persist("front_door", Some(&runtime), true, |cfg| {
                let cam = cfg.cameras.get_mut("front_door").unwrap();
                cam.hardware.protocol = Some("onvif".to_string());
                cam.hardware.profile_token = Some("Profile001".to_string());
                Ok(())
            })
            .await
            .expect("hardware protocol mutation");

        assert_eq!(
            runtime.get_profile("front_door", endpoint),
            None,
            "target camera profile cache must be invalidated"
        );
        assert_eq!(
            runtime.get_video_source("front_door"),
            None,
            "target camera video source cache must be invalidated"
        );
        assert_eq!(
            runtime.get_protocol("front_door"),
            None,
            "target camera protocol cache must be invalidated"
        );
        assert_eq!(
            runtime.get_profile("side_gate", endpoint).as_deref(),
            Some("Profile000"),
            "other cameras in CameraDriverRuntime must remain untouched"
        );
    }

    #[tokio::test]
    async fn commit_raw_yaml_and_structured_patch_restore_secrets_and_invalidate_modified_cameras()
    {
        use serde_json::json;

        let dir = TempTestDir::new("transactional_commits");
        let cfg_path = dir.path().join("config.yml");
        let go2rtc_path = dir.path().join("go2rtc.yaml");
        let initial_yaml = r#"
mqtt:
  enabled: true
  password: "mqtt_super_secret_pw"
cameras:
  front_door:
    enabled: true
    onvif:
      host: "192.0.2.10"
      port: 2020
      user: "admin"
      password: "onvif_secret_password"
    ffmpeg:
      inputs:
        - path: rtsp://admin:rtsp_secret@192.0.2.10:554/stream1
          roles: [detect, record]
  side_gate:
    enabled: true
    onvif:
      host: "192.0.2.11"
      port: 80
    ffmpeg:
      inputs:
        - path: rtsp://admin:gate_secret@192.0.2.11:554/stream1
          roles: [detect, record]
"#;
        let initial_cfg = NinjaConfig::from_yaml_str(initial_yaml, cfg_path.clone()).unwrap();
        persist_config_yaml_atomically(&cfg_path, &initial_cfg.raw_yaml).unwrap();

        let coordinator = ConfigCoordinator::new(Arc::new(RwLock::new(initial_cfg.clone())))
            .with_go2rtc_config_path(&go2rtc_path);
        let runtime = CameraDriverRuntime::new();
        runtime.set_profile(
            "front_door",
            "http://192.0.2.10:2020/onvif/service",
            "profile_1",
        );
        runtime.set_profile(
            "side_gate",
            "http://192.0.2.11:80/onvif/ptz_service",
            "Profile000",
        );

        // 1. Raw YAML save with scrubbed secrets restores secrets and invalidates only changed camera
        let scrubbed_base = scrub_raw_yaml_secrets_strict(&initial_cfg.raw_yaml, &initial_cfg);
        let scrubbed_update = scrubbed_base.replace("192.0.2.10", "192.0.2.99");
        let committed = coordinator
            .commit_raw_yaml(&scrubbed_update, None, Some(&runtime))
            .await
            .expect("commit_raw_yaml should succeed");
        assert_eq!(
            committed.mqtt.password.as_deref(),
            Some("mqtt_super_secret_pw")
        );
        assert_eq!(
            committed.cameras["front_door"].onvif.password.as_deref(),
            Some("onvif_secret_password")
        );
        assert_eq!(
            runtime.get_profile("front_door", "http://192.0.2.10:2020/onvif/service"),
            None,
            "modified camera must be invalidated on raw YAML save"
        );
        assert_eq!(
            runtime
                .get_profile("side_gate", "http://192.0.2.11:80/onvif/ptz_service")
                .as_deref(),
            Some("Profile000"),
            "unmodified camera must keep its cached profile"
        );

        // 2. Unsafe labelmap path in structured patch is rejected without modifying config
        let bad_patch = json!({
            "model": {
                "labelmap_path": "/etc/passwd"
            }
        });
        assert!(matches!(
            coordinator
                .commit_structured_patch(&bad_patch, None, Some(&runtime))
                .await,
            Err(ConfigCommitError::Validation(_))
        ));

        // 3. Valid structured patch updates side_gate while keeping front_door intact, invalidating only side_gate
        let good_patch = json!({
            "cameras": {
                "front_door": {},
                "side_gate": {
                    "onvif": {
                        "port": 8080
                    }
                }
            }
        });
        let outcome = coordinator
            .commit_structured_patch(&good_patch, None, Some(&runtime))
            .await
            .expect("structured patch should succeed");
        assert_eq!(outcome.invalidated_cameras, vec!["side_gate".to_string()]);
        assert_eq!(
            runtime.get_profile("side_gate", "http://192.0.2.11:80/onvif/ptz_service"),
            None
        );
        assert!(!outcome.scrubbed_raw_yaml.contains("mqtt_super_secret_pw"));
        assert!(!outcome.scrubbed_raw_yaml.contains("onvif_secret_password"));
    }
}
