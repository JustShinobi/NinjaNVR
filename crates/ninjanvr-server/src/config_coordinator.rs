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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use ninjanvr_config::{DEFAULT_GO2RTC_SHM_PATH, NinjaConfig};
use tokio::sync::{Mutex, MutexGuard, RwLock};
use tracing::warn;

use crate::camera_protocols::CameraDriverRuntime;

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

    /// Acquires the instance-scoped configuration commit lock so full config saves,
    /// structured patches, model deployments, and camera mutations never interleave.
    pub async fn lock_commit(&self) -> MutexGuard<'_, ()> {
        self.commit_lock.lock().await
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
}
