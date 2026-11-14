#![forbid(unsafe_code)]
//! Staged atomic detector model deployment, credential-safe labelmap parsing, and
//! runtime model catalog discovery (`DetectorModelManager`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ninjanvr_infer::{AiDetectorEngine, DiscoveredModelEntry, parse_safe_labelmap_lines};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::config_coordinator::{
    ConfigCommitError, ConfigCoordinator, ModelDeployOutcome, should_rollback_model_deploy,
};

/// Bundle of validated detector model artifacts ready for staged compilation and atomic swap.
#[derive(Debug, Clone)]
pub struct StagedModelUploadBundle {
    pub safe_filename: String,
    pub model_bytes: Vec<u8>,
    pub weights_filename: Option<String>,
    pub weights_bytes: Option<Vec<u8>>,
    pub safe_labelmap_name: String,
    pub labelmap_text: String,
    pub model_type: String,
    pub width: u32,
    pub height: u32,
    pub input_tensor: String,
    pub input_pixel_format: String,
}

/// RAII guard that unlinks all staged detector model artifacts (`staged_<nonce>_*`) on drop,
/// ensuring task cancellation, early errors, or panics at any `.await` never leak staging files.
pub(crate) struct StagedFilesGuard {
    paths: Vec<PathBuf>,
}

impl StagedFilesGuard {
    pub(crate) fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }
}

impl Drop for StagedFilesGuard {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Deep manager owning staged candidate compilation, rollback-safe file publication,
/// `ConfigCoordinator` persistence, and runtime model catalog discovery.
#[derive(Clone, Default)]
pub struct DetectorModelManager;

impl DetectorModelManager {
    pub fn new() -> Self {
        Self
    }

    /// Discovers all `.onnx` and `.xml` models across the runtime catalog, `media_dir/models`,
    /// and `db_path`'s parent directory without exposing staging files (`staged_*`).
    pub fn build_catalog_value(
        detector: &AiDetectorEngine,
        media_dir: &Path,
        db_path: &Path,
    ) -> Value {
        let mut cat = detector.runtime_catalog();
        let mut extra_dirs = vec![media_dir.join("models")];
        if let Some(db_parent) = db_path.parent() {
            extra_dirs.push(db_parent.join("models"));
            extra_dirs.push(db_parent.to_path_buf());
        }
        for dir in extra_dirs {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    let Some(fname) = p.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if fname.starts_with("staged_") || fname.starts_with('.') {
                        continue;
                    }
                    if let Some(ext) = p.extension().and_then(|e| e.to_str())
                        && (ext.eq_ignore_ascii_case("xml") || ext.eq_ignore_ascii_case("onnx"))
                    {
                        let p_str = p.to_string_lossy().to_string();
                        if !cat.discovered_models.iter().any(|m| m.path == p_str) {
                            let arch = if p_str.to_ascii_lowercase().contains("yolox") {
                                "yolox"
                            } else if p_str.to_ascii_lowercase().contains("yolo")
                                || ext.eq_ignore_ascii_case("onnx")
                            {
                                "yolo-generic"
                            } else {
                                "ssd"
                            };
                            cat.discovered_models.push(DiscoveredModelEntry {
                                path: p_str,
                                arch: arch.to_string(),
                            });
                        }
                    }
                }
            }
        }
        serde_json::to_value(cat).unwrap_or_else(|_| json!({}))
    }

    /// Stages `bundle` under unique `staged_<nonce>_*` filenames, compiles a candidate primary
    /// detector **before** overwriting the active model file or mutating `detector`, unlinks
    /// staged files cleanly if compilation fails when a detector was previously loaded, and
    /// otherwise publishes the files atomically and commits `config_coordinator`.
    pub async fn deploy_uploaded_bundle(
        &self,
        bundle: StagedModelUploadBundle,
        media_dir: &Path,
        db_path: &Path,
        detector: &Arc<AiDetectorEngine>,
        config_coordinator: &ConfigCoordinator,
    ) -> Result<(ModelDeployOutcome, PathBuf, PathBuf), ConfigCommitError> {
        let models_dir = media_dir.join("models");
        let final_model_path = models_dir.join(&bundle.safe_filename);
        let final_labelmap_path = models_dir.join(&bundle.safe_labelmap_name);

        // Fail fast with 400 Validation before staging if any target path or `models/` dir is a symlink or directory
        if std::fs::symlink_metadata(&models_dir)
            .is_ok_and(|m| m.file_type().is_symlink() || !m.file_type().is_dir())
        {
            return Err(ConfigCommitError::Validation(
                "Target models directory is a symlink or not a directory".to_string(),
            ));
        }
        for target_candidate in [
            Some(final_model_path.as_path()),
            Some(final_labelmap_path.as_path()),
            bundle
                .weights_filename
                .as_deref()
                .map(|w| models_dir.join(w))
                .as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if std::fs::symlink_metadata(target_candidate)
                .is_ok_and(|m| m.file_type().is_symlink() || m.file_type().is_dir())
            {
                return Err(ConfigCommitError::Validation(format!(
                    "Refusing to overwrite symlink or directory target {}",
                    target_candidate.display()
                )));
            }
        }

        let nonce = format!("{:016x}", rand::random::<u64>());
        let staged_prefix = format!("staged_{nonce}_");
        let staged_model_name = format!("{staged_prefix}{}", bundle.safe_filename);
        let staged_labelmap_name = format!("{staged_prefix}{}", bundle.safe_labelmap_name);

        // For OpenVINO `.xml` models, the C runtime resolves companion weights from `{xml_stem}.bin`.
        let staged_weights_name = if bundle.weights_bytes.is_some() {
            if bundle.safe_filename.to_ascii_lowercase().ends_with(".xml") {
                let staged_stem = staged_model_name
                    .strip_suffix(".xml")
                    .or_else(|| staged_model_name.strip_suffix(".XML"))
                    .unwrap_or(&staged_model_name);
                Some(format!("{staged_stem}.bin"))
            } else {
                bundle
                    .weights_filename
                    .as_ref()
                    .map(|w| format!("{staged_prefix}{w}"))
            }
        } else {
            None
        };

        let staged_model_path = models_dir.join(&staged_model_name);
        let staged_labelmap_path = models_dir.join(&staged_labelmap_name);
        let mut staged_guard_paths = vec![staged_model_path.clone(), staged_labelmap_path.clone()];
        if let Some(ref wn) = staged_weights_name {
            staged_guard_paths.push(models_dir.join(wn));
        }
        let staged_guard = StagedFilesGuard::new(staged_guard_paths);

        crate::atomic_write_within_media_subdir(
            media_dir,
            "models",
            &staged_model_name,
            &bundle.model_bytes,
        )
        .await
        .map_err(|e| ConfigCommitError::Validation(e.to_string()))?;

        if let (Some(w_bytes), Some(w_staged_name)) =
            (&bundle.weights_bytes, staged_weights_name.as_deref())
        {
            crate::atomic_write_within_media_subdir(media_dir, "models", w_staged_name, w_bytes)
                .await
                .map_err(|err| ConfigCommitError::Validation(err.to_string()))?;
        }

        crate::atomic_write_within_media_subdir(
            media_dir,
            "models",
            &staged_labelmap_name,
            bundle.labelmap_text.as_bytes(),
        )
        .await
        .map_err(|err| ConfigCommitError::Validation(err.to_string()))?;

        let labelmap_btree = parse_safe_labelmap_lines(&bundle.labelmap_text);

        let staged_patch = json!({
            "model": {
                "path": staged_model_path.to_string_lossy().to_string(),
                "labelmap_path": staged_labelmap_path.to_string_lossy().to_string(),
                "width": bundle.width,
                "height": bundle.height,
                "model_type": bundle.model_type,
                "input_tensor": bundle.input_tensor,
                "input_pixel_format": bundle.input_pixel_format,
            }
        });
        let final_patch = json!({
            "model": {
                "path": final_model_path.to_string_lossy().to_string(),
                "labelmap_path": final_labelmap_path.to_string_lossy().to_string(),
                "width": bundle.width,
                "height": bundle.height,
                "model_type": bundle.model_type,
                "input_tensor": bundle.input_tensor,
                "input_pixel_format": bundle.input_pixel_format,
            }
        });

        let prev_cfg_snap = config_coordinator.config().read().await.clone();
        let mut candidate_cfg = prev_cfg_snap.clone();
        candidate_cfg
            .apply_structured_json(&staged_patch)
            .map_err(|err| ConfigCommitError::Validation(format!("{err:#}")))?;

        let det_for_compile = detector.clone();
        let lm_for_compile = labelmap_btree.clone();
        let compile_res = tokio::task::spawn_blocking(move || {
            let was_loaded = det_for_compile.is_model_loaded();
            let compiled =
                AiDetectorEngine::compile_candidate_primary(&candidate_cfg, Some(lm_for_compile));
            (was_loaded, compiled)
        })
        .await
        .map_err(|e| {
            ConfigCommitError::Persistence(format!("Detector compile task failed: {e}"))
        })?;

        drop(staged_guard);

        let (was_loaded, compiled) = compile_res;
        if should_rollback_model_deploy(was_loaded, compiled.loaded) {
            return Ok((
                ModelDeployOutcome::RolledBack {
                    previous_model_path: prev_cfg_snap.model.path,
                },
                final_model_path,
                final_labelmap_path,
            ));
        }

        let outcome = config_coordinator
            .commit_precompiled_detector_model(
                &bundle,
                media_dir,
                db_path,
                &final_patch,
                compiled,
                final_model_path.to_string_lossy().to_string(),
                detector,
            )
            .await?;

        info!(
            "Staged detector deployment completed for {} (outcome={outcome:?})",
            final_model_path.display()
        );
        if let ModelDeployOutcome::RolledBack {
            ref previous_model_path,
        } = outcome
        {
            warn!("Detector model deployment rolled back to {previous_model_path}");
        }

        Ok((outcome, final_model_path, final_labelmap_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninjanvr_config::NinjaConfig;
    use tokio::sync::RwLock;

    #[test]
    fn catalog_hides_staged_temporary_files_and_discovers_published_models() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_det_mgr_{}", rand::random::<u64>()));
        let models_dir = tmp_dir.join("models");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::write(models_dir.join("custom_yolo11s.onnx"), b"VALID_BYTES").unwrap();
        std::fs::write(
            models_dir.join("staged_0123456789abcdef_custom_yolo11s.onnx"),
            b"STAGED_BYTES",
        )
        .unwrap();

        let cfg = NinjaConfig::from_yaml_str("cameras: {}\n", PathBuf::new()).unwrap();
        let det = AiDetectorEngine::new(&cfg);
        let cat =
            DetectorModelManager::build_catalog_value(&det, &tmp_dir, &tmp_dir.join("ninjanvr.db"));
        let discovered = cat["discovered_models"].as_array().unwrap();
        assert!(discovered.iter().any(|m| {
            m["path"]
                .as_str()
                .unwrap_or("")
                .ends_with("custom_yolo11s.onnx")
        }));
        assert!(
            !discovered
                .iter()
                .any(|m| m["path"].as_str().unwrap_or("").contains("staged_"))
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn reload_if_changed_preserves_primary_detector_when_only_camera_zones_change() {
        let yaml_a = r#"
cameras:
  front:
    enabled: true
    zones:
      porch:
        coordinates: "0.1,0.1,0.9,0.1,0.9,0.9,0.1,0.9"
"#;
        let yaml_b = r#"
cameras:
  front:
    enabled: true
    zones:
      porch:
        coordinates: "0.2,0.2,0.8,0.2,0.8,0.8,0.2,0.8"
"#;
        let cfg_a = NinjaConfig::from_yaml_str(yaml_a, PathBuf::new()).unwrap();
        let cfg_b = NinjaConfig::from_yaml_str(yaml_b, PathBuf::new()).unwrap();
        let shared = Arc::new(RwLock::new(cfg_a));
        let coord = ConfigCoordinator::new(shared).with_go2rtc_config_path(PathBuf::new());
        let det = Arc::new(AiDetectorEngine::new(&cfg_b));
        // Seed a synthetic inference sample via a dummy RGB region pass
        let _ = det.detect_region(
            &vec![128u8; 64 * 64 * 3],
            ninjanvr_vision::BoundingBox::new(0, 0, 64, 64),
            64,
            64,
            0.5,
        );
        let samples_before = det.inference_samples();

        let _ = coord
            .commit_raw_yaml(yaml_b, Some(&det), None)
            .await
            .unwrap();
        assert_eq!(
            det.inference_samples(),
            samples_before,
            "Zone-only config commit must not reset primary detector inference_samples"
        );
    }

    #[tokio::test]
    async fn staged_files_guard_unlinks_on_abort_and_deploy_mirrors_weights_and_updates_pool() {
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_staged_guard_{}", rand::random::<u64>()));
        let media_dir = tmp_dir.join("media");
        let db_dir = tmp_dir.join("db_root");
        std::fs::create_dir_all(media_dir.join("models")).unwrap();
        std::fs::create_dir_all(&db_dir).unwrap();

        // 1. StagedFilesGuard unlinks all staged files when a future is cancelled at .await
        let s1 = media_dir.join("models/staged_test_a.xml");
        let s2 = media_dir.join("models/staged_test_a.bin");
        std::fs::write(&s1, b"xml").unwrap();
        std::fs::write(&s2, b"bin").unwrap();
        let s1_c = s1.clone();
        let s2_c = s2.clone();
        let handle = tokio::spawn(async move {
            let _guard = StagedFilesGuard::new(vec![s1_c, s2_c]);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        handle.abort();
        let _ = handle.await;
        assert!(
            !s1.exists(),
            "StagedFilesGuard must unlink staged .xml on cancellation"
        );
        assert!(
            !s2.exists(),
            "StagedFilesGuard must unlink staged .bin on cancellation"
        );

        // 2. deploy_uploaded_bundle mirrors .bin weights into db_parent/models and updates default-model pool
        let yaml = r#"
model:
  path: /openvino-model/ssdlite_mobilenet_v2.xml
  width: 300
  height: 300
detectors:
  default_pool_cpu:
    type: openvino
    device: CPU
cameras: {}
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::new()).unwrap();
        let det = Arc::new(AiDetectorEngine::new(&cfg));
        let init_dims = det
            .named_detector(Some("default_pool_cpu"))
            .unwrap()
            .model_dimensions();
        assert_eq!((init_dims.0, init_dims.1), (300, 300));
        let coord = ConfigCoordinator::new(Arc::new(RwLock::new(cfg)))
            .with_go2rtc_config_path(PathBuf::new());

        let bundle = StagedModelUploadBundle {
            safe_filename: "custom_net.xml".to_string(),
            model_bytes: b"<net/>".to_vec(),
            weights_filename: Some("custom_net.bin".to_string()),
            weights_bytes: Some(b"WEIGHTS_PAYLOAD".to_vec()),
            safe_labelmap_name: "custom_labels.txt".to_string(),
            labelmap_text: "person\ncar\n".to_string(),
            model_type: "yolox".to_string(),
            width: 640,
            height: 640,
            input_tensor: "nchw".to_string(),
            input_pixel_format: "rgb".to_string(),
        };
        let (outcome, final_m, _) = DetectorModelManager::new()
            .deploy_uploaded_bundle(
                bundle,
                &media_dir,
                &db_dir.join("ninjanvr.db"),
                &det,
                &coord,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ModelDeployOutcome::Deployed { .. }));
        assert_eq!(
            std::fs::read(media_dir.join("models/custom_net.bin")).unwrap(),
            b"WEIGHTS_PAYLOAD"
        );
        assert_eq!(
            std::fs::read(db_dir.join("models/custom_net.bin")).unwrap(),
            b"WEIGHTS_PAYLOAD",
            "companion .bin weights must be mirrored into db_parent/models"
        );
        let pool_det = det.named_detector(Some("default_pool_cpu")).unwrap();
        let next_dims = pool_det.model_dimensions();
        assert_eq!((next_dims.0, next_dims.1), (640, 640));
        assert_eq!(pool_det.active_model_path(), final_m.to_string_lossy());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
