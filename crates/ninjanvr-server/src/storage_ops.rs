//! Granular storage breakdown telemetry (`GET /api/storage/breakdown`) and
//! scoped media/catalog cleanup actions (`POST /api/storage/cleanup`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Json, Response};
use axum_extra::extract::cookie::CookieJar;
use ninjanvr_config::NinjaConfig;
use ninjanvr_db::CameraScope;
use ninjanvr_media::{sanitize_quota_gb, sanitize_retention_days};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::auth_gate::{AccessTarget, authenticate_read};
use crate::media_lifecycle;
use crate::share_engine::purge_revoked_share_credits;
use crate::vod_routes::resolve_configured_cold_dir;
use crate::{AppState, is_safe_id, stat_mount_mb};

const BREAKDOWN_CACHE_TTL: Duration = Duration::from_secs(10);
const MIN_FORCE_REFRESH_TTL: Duration = Duration::from_secs(2);
const MAX_SCAN_ENTRIES_PER_DIR: usize = 200_000;

fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn mb_to_bytes(mb: f64) -> u64 {
    if mb.is_finite() && mb > 0.0 {
        (mb * 1024.0 * 1024.0).round() as u64
    } else {
        0
    }
}

/// Isolated `AppState`-owned singleflight cache for `GET /api/storage/breakdown`.
#[derive(Clone, Default)]
pub struct StorageBreakdownCache {
    manual_seq: Arc<AtomicU64>,
    compute_lock: Arc<tokio::sync::Mutex<()>>,
    inner: Arc<Mutex<Option<(Instant, u64, StorageBreakdownResponse)>>>,
}

impl StorageBreakdownCache {
    pub fn new() -> Self {
        Self {
            manual_seq: Arc::new(AtomicU64::new(1)),
            compute_lock: Arc::new(tokio::sync::Mutex::new(())),
            inner: Arc::new(Mutex::new(None)),
        }
    }

    pub fn invalidate(&self) {
        self.manual_seq.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn composite_seq(&self, rec_seq: u64, ev_seq: u64) -> u64 {
        let m = self.manual_seq.load(Ordering::Acquire);
        rec_seq
            .wrapping_mul(1_000_003)
            .wrapping_add(ev_seq.wrapping_mul(1_009))
            .wrapping_add(m)
    }

    fn get_if_within_ttl(&self, seq: u64, ttl: Duration) -> Option<StorageBreakdownResponse> {
        let guard = self.inner.lock().ok()?;
        let (ts, cached_seq, ref val) = *guard.as_ref()?;
        if cached_seq == seq && ts.elapsed() < ttl {
            Some(val.clone())
        } else {
            None
        }
    }

    pub(crate) fn store(&self, seq: u64, val: StorageBreakdownResponse) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some((Instant::now(), seq, val));
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageCategoryItem {
    pub bytes: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageCategoriesBreakdown {
    pub recordings_continuous: StorageCategoryItem,
    pub recordings_motion: StorageCategoryItem,
    pub recordings_events: StorageCategoryItem,
    pub recordings_retained_excess: StorageCategoryItem,
    pub recordings_compacted: StorageCategoryItem,
    pub events_media: StorageCategoryItem,
    pub pinned_events_count: u64,
    pub reviews_media: StorageCategoryItem,
    pub previews_media: StorageCategoryItem,
    pub exports_media: StorageCategoryItem,
    pub share_artifacts_count: u64,
    pub dataset_and_models: StorageCategoryItem,
    pub dataset_samples_count: u64,
    pub models_files_count: u64,
    pub database: StorageCategoryItem,
    pub database_main_bytes: u64,
    pub database_wal_bytes: u64,
    pub timeline_rows_count: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CameraGranularStorageDto {
    pub camera: String,
    pub configured: bool,
    pub total_bytes: u64,
    pub recordings_bytes: u64,
    pub continuous_bytes: u64,
    pub motion_bytes: u64,
    pub event_segments_bytes: u64,
    pub compacted_bytes: u64,
    pub retained_past_continuous_bytes: u64,
    pub retained_past_continuous_count: u64,
    pub segments_count: u64,
    pub oldest_segment_ts: Option<f64>,
    pub newest_segment_ts: Option<f64>,
    pub events_count: u64,
    pub pinned_events_count: u64,
    pub events_media_bytes: u64,
    pub reviews_count: u64,
    pub reviews_media_bytes: u64,
    pub previews_bytes: u64,
    pub previews_files_count: u64,
    pub exports_bytes: u64,
    pub exports_count: u64,
    pub bandwidth_mb_hr: f64,
    pub estimated_retention_days: f64,
    pub continuous_days: f64,
    pub event_retention_days: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageBreakdownResponse {
    pub generated_at: f64,
    pub total_tracked_bytes: u64,
    pub volume_total_bytes: u64,
    pub volume_used_bytes: u64,
    pub volume_free_bytes: u64,
    pub scoped: bool,
    pub categories: StorageCategoriesBreakdown,
    pub cameras: BTreeMap<String, CameraGranularStorageDto>,
}

#[derive(Debug, Deserialize, Default)]
pub struct BreakdownQueryParams {
    #[serde(default)]
    pub refresh: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupTarget {
    Events,
    Reviews,
    ExcessRecordings,
    Recordings,
    Previews,
    Exports,
    RetentionPass,
    WalCheckpoint,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageCleanupRequest {
    pub target: CleanupTarget,
    #[serde(default)]
    pub camera: Option<String>,
    #[serde(default)]
    pub older_than_days: Option<f64>,
    #[serde(default)]
    pub include_pinned: bool,
    #[serde(default = "default_true")]
    pub prune_retained_segments: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageCleanupResponse {
    pub success: bool,
    pub target: CleanupTarget,
    pub camera: Option<String>,
    pub deleted_items: u64,
    pub deleted_segments: u64,
    pub freed_bytes: u64,
    pub duration_ms: u64,
    pub message: String,
}

#[derive(Debug, Clone)]
struct CameraPolicyMeta {
    continuous_days: f64,
    motion_days: f64,
    event_days: f64,
    pre_capture_sec: f64,
    post_capture_sec: f64,
    quota_mb: Option<f64>,
}

fn extract_camera_policies(cfg: &NinjaConfig) -> BTreeMap<String, CameraPolicyMeta> {
    let mut map = BTreeMap::new();
    for (name, c) in &cfg.cameras {
        let pre_cap = c
            .record
            .alerts
            .pre_capture
            .max(c.record.detections.pre_capture)
            .max(cfg.record.alerts.pre_capture) as f64;
        let post_cap = c
            .record
            .alerts
            .post_capture
            .max(c.record.detections.post_capture)
            .max(cfg.record.alerts.post_capture) as f64;
        let min_buf_days = ((pre_cap + post_cap + 60.0) / 86_400.0).max(0.002);
        let continuous_days = sanitize_retention_days(c.record.continuous.days, 7.0, min_buf_days);
        let alerts_days = sanitize_retention_days(c.record.alerts.retain.days, 7.0, 0.1);
        let detections_days = sanitize_retention_days(c.record.detections.retain.days, 7.0, 0.1);
        let event_days = alerts_days.max(detections_days);
        let motion_days = if c.record.motion.days.is_finite() && c.record.motion.days > 0.0 {
            sanitize_retention_days(c.record.motion.days, continuous_days, continuous_days)
        } else {
            continuous_days
        };
        let quota_mb = sanitize_quota_gb(c.record.max_size_gb)
            .or_else(|| sanitize_quota_gb(c.record.alerts.retain.max_size_gb))
            .or_else(|| sanitize_quota_gb(c.record.detections.retain.max_size_gb))
            .map(|g| g * 1024.0);
        map.insert(
            name.clone(),
            CameraPolicyMeta {
                continuous_days,
                motion_days,
                event_days,
                pre_capture_sec: pre_cap,
                post_capture_sec: post_cap,
                quota_mb,
            },
        );
    }
    map
}

fn sum_regular_files_non_symlink(dir: &Path, max_depth: usize) -> (u64, u64) {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return (0, 0);
    };
    if !meta.file_type().is_dir() {
        return (0, 0);
    }
    let mut files_count = 0u64;
    let mut total_bytes = 0u64;
    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    let mut visited = 0usize;
    while let Some((cur, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_SCAN_ENTRIES_PER_DIR {
                return (files_count, total_bytes);
            }
            let p = entry.path();
            let Ok(emeta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if emeta.file_type().is_symlink() {
                continue;
            }
            if emeta.file_type().is_file() {
                files_count += 1;
                total_bytes += emeta.len();
            } else if emeta.file_type().is_dir() && depth < max_depth {
                stack.push((p, depth + 1));
            }
        }
    }
    (files_count, total_bytes)
}

fn remove_regular_files_under_media_subdir(
    media_dir: &Path,
    rel_root: &Path,
    max_depth: usize,
) -> (u64, u64) {
    let target_dir = media_dir.join(rel_root);
    let Ok(meta) = std::fs::symlink_metadata(&target_dir) else {
        return (0, 0);
    };
    if !meta.file_type().is_dir() {
        return (0, 0);
    }
    let mut removed_files = 0u64;
    let mut freed_bytes = 0u64;
    let mut stack: Vec<(PathBuf, usize)> = vec![(rel_root.to_path_buf(), 0)];
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut visited = 0usize;

    while let Some((cur_rel, depth)) = stack.pop() {
        let cur_abs = media_dir.join(&cur_rel);
        let Ok(entries) = std::fs::read_dir(&cur_abs) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_SCAN_ENTRIES_PER_DIR {
                break;
            }
            let Some(name_str) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name_str.is_empty() || name_str == "." || name_str == ".." {
                continue;
            }
            let child_rel = cur_rel.join(&name_str);
            let child_abs = media_dir.join(&child_rel);
            let Ok(emeta) = std::fs::symlink_metadata(&child_abs) else {
                continue;
            };
            if emeta.file_type().is_symlink() {
                continue;
            }
            if emeta.file_type().is_file() {
                let b = media_lifecycle::remove_non_symlink_under_media_dir_bytes(
                    media_dir, &child_rel,
                );
                if b > 0 {
                    removed_files += 1;
                    freed_bytes += b;
                }
            } else if emeta.file_type().is_dir() && depth < max_depth {
                subdirs.push(child_abs);
                stack.push((child_rel, depth + 1));
            }
        }
    }

    for dir in subdirs.into_iter().rev() {
        let _ = std::fs::remove_dir(dir);
    }
    (removed_files, freed_bytes)
}

fn match_camera_prefix<'a>(stem: &str, sorted_cameras_desc: &'a [String]) -> Option<&'a str> {
    for cam in sorted_cameras_desc {
        if let Some(rest) = stem.strip_prefix(cam.as_str())
            && let Some(id_part) = rest.strip_prefix('-')
            && !id_part.is_empty()
        {
            return Some(cam.as_str());
        }
    }
    None
}

/// Returns all `(camera_prefix, candidate_id)` decompositions of `stem` against `sorted_cameras_desc`
/// so prefixes like `cam` and `cam-north` are both inspected before unlinking an orphan clip.
fn all_camera_prefix_decompositions<'a>(
    stem: &'a str,
    sorted_cameras_desc: &'a [String],
) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    for cam in sorted_cameras_desc {
        if let Some(rest) = stem.strip_prefix(cam.as_str())
            && let Some(id_part) = rest.strip_prefix('-')
            && !id_part.is_empty()
        {
            out.push((cam.as_str(), id_part));
        }
    }
    out
}

fn ensure_orphan_camera_dto<'a>(
    cam_dtos: &'a mut BTreeMap<String, CameraGranularStorageDto>,
    cam_name: &str,
    default_cont_days: f64,
    default_event_days: f64,
    global_est_days: f64,
) -> &'a mut CameraGranularStorageDto {
    cam_dtos
        .entry(cam_name.to_string())
        .or_insert_with(|| CameraGranularStorageDto {
            camera: cam_name.to_string(),
            configured: false,
            estimated_retention_days: global_est_days,
            continuous_days: default_cont_days,
            event_retention_days: default_event_days,
            ..CameraGranularStorageDto::default()
        })
}

fn compute_unscoped_breakdown_sync(
    state: &AppState,
    cfg: &NinjaConfig,
) -> StorageBreakdownResponse {
    let now = now_f64();
    let policies = extract_camera_policies(cfg);
    let default_cont_days = sanitize_retention_days(cfg.record.continuous.days, 7.0, 0.002);
    let default_event_days = sanitize_retention_days(
        cfg.record
            .alerts
            .retain
            .days
            .max(cfg.record.detections.retain.days),
        7.0,
        0.1,
    );
    let default_cont_cutoff = now - default_cont_days * 86_400.0;
    let cont_cutoffs: HashMap<String, f64> = policies
        .iter()
        .map(|(cam, p)| (cam.clone(), now - p.continuous_days * 86_400.0))
        .collect();

    let db_snap = state
        .db
        .query_storage_db_snapshot(&cont_cutoffs, default_cont_cutoff, now)
        .unwrap_or_default();

    let rec_dir = state.media_dir.join("recordings");
    let rec_mount = stat_mount_mb(&rec_dir, "ext4");
    let volume_total_mb = rec_mount["total"].as_f64().unwrap_or(0.0).max(0.0);
    let volume_used_mb = rec_mount["used"].as_f64().unwrap_or(0.0).max(0.0);
    let volume_free_mb = rec_mount["free"].as_f64().unwrap_or(20_000.0).max(1.0);

    let mut all_cameras: BTreeSet<String> = policies.keys().cloned().collect();
    for cam in db_snap.cameras.keys() {
        all_cameras.insert(cam.clone());
    }
    let mut sorted_cams_desc: Vec<String> = all_cameras.iter().cloned().collect();
    sorted_cams_desc.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));

    let total_bw_mb_hr: f64 = db_snap
        .cameras
        .values()
        .map(|c| c.bandwidth_mb_hr)
        .filter(|v| v.is_finite() && *v > 0.0)
        .sum();
    let global_quota_mb = sanitize_quota_gb(cfg.record.max_size_gb)
        .or_else(|| sanitize_quota_gb(cfg.record.alerts.retain.max_size_gb))
        .or_else(|| sanitize_quota_gb(cfg.record.detections.retain.max_size_gb))
        .map(|g| g * 1024.0);
    let effective_pool_mb = global_quota_mb.map_or(volume_free_mb, |q| q.min(volume_free_mb));
    let global_est_days = if total_bw_mb_hr > 0.1 {
        ((effective_pool_mb / (total_bw_mb_hr * 24.0)) * 10.0).round() / 10.0
    } else {
        14.0
    };

    let mut cam_dtos: BTreeMap<String, CameraGranularStorageDto> = BTreeMap::new();
    for cam in &all_cameras {
        let db_cam = db_snap.cameras.get(cam).cloned().unwrap_or_default();
        let pol = policies.get(cam);
        let cont_days = pol.map_or(default_cont_days, |p| p.continuous_days);
        let evt_days = pol.map_or(default_event_days, |p| p.event_days);
        let est_days = if let Some(cq_mb) = pol.and_then(|p| p.quota_mb) {
            if db_cam.bandwidth_mb_hr > 0.1 {
                ((cq_mb.min(volume_free_mb) / (db_cam.bandwidth_mb_hr * 24.0)) * 10.0).round()
                    / 10.0
            } else {
                14.0
            }
        } else {
            global_est_days
        };
        let rec_bytes = mb_to_bytes(db_cam.recordings_mb);
        cam_dtos.insert(
            cam.clone(),
            CameraGranularStorageDto {
                camera: cam.clone(),
                configured: pol.is_some(),
                total_bytes: rec_bytes,
                recordings_bytes: rec_bytes,
                continuous_bytes: mb_to_bytes(db_cam.continuous_mb),
                motion_bytes: mb_to_bytes(db_cam.motion_mb),
                event_segments_bytes: mb_to_bytes(db_cam.event_segments_mb),
                compacted_bytes: mb_to_bytes(db_cam.compacted_mb),
                retained_past_continuous_bytes: mb_to_bytes(db_cam.retained_past_continuous_mb),
                retained_past_continuous_count: db_cam.retained_past_continuous_count,
                segments_count: db_cam.segments_count,
                oldest_segment_ts: db_cam.oldest_segment_ts,
                newest_segment_ts: db_cam.newest_segment_ts,
                events_count: db_cam.events_count,
                pinned_events_count: db_cam.pinned_events_count,
                events_media_bytes: 0,
                reviews_count: db_cam.reviews_count,
                reviews_media_bytes: 0,
                previews_bytes: 0,
                previews_files_count: 0,
                exports_bytes: 0,
                exports_count: 0,
                bandwidth_mb_hr: db_cam.bandwidth_mb_hr,
                estimated_retention_days: est_days,
                continuous_days: cont_days,
                event_retention_days: evt_days,
            },
        );
    }

    // Track export filenames (including `{id}.webp` for in-progress exports)
    let mut export_thumb_names: BTreeSet<String> = BTreeSet::new();
    let mut export_video_names_to_cam: HashMap<String, String> = HashMap::new();
    let mut export_thumb_names_to_cam: HashMap<String, String> = HashMap::new();
    for (exp_id, cam, vpath, tpath, _in_prog) in &db_snap.export_entries {
        if is_safe_id(cam) {
            ensure_orphan_camera_dto(
                &mut cam_dtos,
                cam,
                default_cont_days,
                default_event_days,
                global_est_days,
            )
            .exports_count += 1;
        }
        let default_thumb = format!("{exp_id}.webp");
        export_thumb_names.insert(default_thumb.clone());
        export_thumb_names_to_cam.insert(default_thumb, cam.clone());
        if let Some(vn) = Path::new(vpath).file_name().and_then(|n| n.to_str()) {
            export_video_names_to_cam.insert(vn.to_string(), cam.clone());
        }
        if let Some(tn) = Path::new(tpath).file_name().and_then(|n| n.to_str())
            && !tn.is_empty()
        {
            export_thumb_names.insert(tn.to_string());
            export_thumb_names_to_cam.insert(tn.to_string(), cam.clone());
        }
    }

    // Scan `clips/` top-level files (without recursing into subdirs here)
    let clips_dir = state.media_dir.join("clips");
    let mut events_media_total_bytes = 0u64;
    let mut exports_media_total_bytes = 0u64;
    let mut share_artifacts_count = 0u64;
    let mut previews_media_total_bytes = 0u64;
    let mut previews_media_total_files = 0u64;

    if std::fs::symlink_metadata(&clips_dir).is_ok_and(|m| m.file_type().is_dir())
        && let Ok(entries) = std::fs::read_dir(&clips_dir)
    {
        for entry in entries.flatten().take(MAX_SCAN_ENTRIES_PER_DIR) {
            let p = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let len = meta.len();
            let Some(fname) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if fname.starts_with("latest_") {
                // Live fallback frame (~15-30 KB per camera); excluded from purgeable previews count
                continue;
            }
            if fname.starts_with("share_clip_") || fname.starts_with("share_redacted_") {
                exports_media_total_bytes += len;
                share_artifacts_count += 1;
                continue;
            }
            if export_thumb_names.contains(fname) {
                exports_media_total_bytes += len;
                if let Some(cam) = export_thumb_names_to_cam.get(fname)
                    && let Some(c_dto) = cam_dtos.get_mut(cam)
                {
                    c_dto.exports_bytes += len;
                }
                continue;
            }

            events_media_total_bytes += len;
            let stem_raw = Path::new(fname)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let stem_clean = stem_raw.strip_suffix("-clean").unwrap_or(stem_raw);
            let resolved_cam = db_snap
                .event_cameras_by_id
                .get(stem_clean)
                .cloned()
                .or_else(|| {
                    all_camera_prefix_decompositions(stem_clean, &sorted_cams_desc)
                        .into_iter()
                        .find_map(|(cp, id_part)| {
                            db_snap
                                .event_cameras_by_id
                                .get(id_part)
                                .filter(|db_cam| db_cam.as_str() == cp)
                                .cloned()
                        })
                })
                .or_else(|| match_camera_prefix(stem_clean, &sorted_cams_desc).map(str::to_string));
            if let Some(cam) = resolved_cam
                && let Some(c_dto) = cam_dtos.get_mut(&cam)
            {
                c_dto.events_media_bytes += len;
            }
        }
    }

    // Scan `clips/review/`
    let review_dir = clips_dir.join("review");
    let mut reviews_media_total_bytes = 0u64;
    if std::fs::symlink_metadata(&review_dir).is_ok_and(|m| m.file_type().is_dir())
        && let Ok(entries) = std::fs::read_dir(&review_dir)
    {
        for entry in entries.flatten().take(MAX_SCAN_ENTRIES_PER_DIR) {
            let p = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let len = meta.len();
            reviews_media_total_bytes += len;
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let after_thumb = stem.strip_prefix("thumb-").unwrap_or(stem);
            let resolved_cam = db_snap
                .review_cameras_by_id
                .get(after_thumb)
                .cloned()
                .or_else(|| {
                    all_camera_prefix_decompositions(after_thumb, &sorted_cams_desc)
                        .into_iter()
                        .find_map(|(cp, id_part)| {
                            db_snap
                                .review_cameras_by_id
                                .get(id_part)
                                .filter(|db_cam| db_cam.as_str() == cp)
                                .cloned()
                        })
                })
                .or_else(|| {
                    match_camera_prefix(after_thumb, &sorted_cams_desc).map(str::to_string)
                });
            if let Some(cam) = resolved_cam
                && let Some(c_dto) = cam_dtos.get_mut(&cam)
            {
                c_dto.reviews_media_bytes += len;
            }
        }
    }

    // Scan `clips/previews/<camera>/` + `clips/preview_frames/<camera>/` + legacy `clips/review_frames/`
    let previews_root = clips_dir.join("previews");
    if std::fs::symlink_metadata(&previews_root).is_ok_and(|m| m.file_type().is_dir())
        && let Ok(cam_dirs) = std::fs::read_dir(&previews_root)
    {
        for cam_entry in cam_dirs.flatten().take(1024) {
            let cp = cam_entry.path();
            let Some(cam_name) = cp.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_safe_id(cam_name) {
                continue;
            }
            let (f_cnt, f_bytes) = sum_regular_files_non_symlink(&cp, 1);
            if f_cnt > 0 || f_bytes > 0 {
                previews_media_total_files += f_cnt;
                previews_media_total_bytes += f_bytes;
                let c_dto = ensure_orphan_camera_dto(
                    &mut cam_dtos,
                    cam_name,
                    default_cont_days,
                    default_event_days,
                    global_est_days,
                );
                c_dto.previews_files_count += f_cnt;
                c_dto.previews_bytes += f_bytes;
            }
        }
    }

    for (cam_name, (pf_cnt, pf_bytes)) in state.preview_frames.disk_usage_by_camera() {
        previews_media_total_files += pf_cnt;
        previews_media_total_bytes += pf_bytes;
        if is_safe_id(&cam_name) {
            let c_dto = ensure_orphan_camera_dto(
                &mut cam_dtos,
                &cam_name,
                default_cont_days,
                default_event_days,
                global_est_days,
            );
            c_dto.previews_files_count += pf_cnt;
            c_dto.previews_bytes += pf_bytes;
        }
    }

    let (leg_rev_cnt, leg_rev_bytes) =
        sum_regular_files_non_symlink(&clips_dir.join("review_frames"), 2);
    previews_media_total_files += leg_rev_cnt;
    previews_media_total_bytes += leg_rev_bytes;

    // Scan `exports/`
    let exports_dir = state.media_dir.join("exports");
    if std::fs::symlink_metadata(&exports_dir).is_ok_and(|m| m.file_type().is_dir())
        && let Ok(entries) = std::fs::read_dir(&exports_dir)
    {
        for entry in entries.flatten().take(MAX_SCAN_ENTRIES_PER_DIR) {
            let p = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let len = meta.len();
            exports_media_total_bytes += len;
            if let Some(fname) = p.file_name().and_then(|n| n.to_str()) {
                if fname.starts_with("share_clip_")
                    || fname.starts_with("share_redacted_")
                    || fname.starts_with(".share_redacted_")
                {
                    share_artifacts_count += 1;
                } else if let Some(cam) = export_video_names_to_cam.get(fname)
                    && let Some(c_dto) = cam_dtos.get_mut(cam)
                {
                    c_dto.exports_bytes += len;
                }
            }
        }
    }

    // Dataset, faces, and models
    let (ds1_cnt, ds1_bytes) = sum_regular_files_non_symlink(&state.media_dir.join("dataset"), 2);
    let (ds2_cnt, ds2_bytes) = sum_regular_files_non_symlink(&clips_dir.join("dataset"), 2);
    let (fc1_cnt, fc1_bytes) = sum_regular_files_non_symlink(&state.media_dir.join("faces"), 2);
    let (fc2_cnt, fc2_bytes) = sum_regular_files_non_symlink(&clips_dir.join("faces"), 2);
    let (mod_cnt, mod_bytes) = sum_regular_files_non_symlink(&state.media_dir.join("models"), 2);
    let dataset_and_models_files = ds1_cnt + ds2_cnt + fc1_cnt + fc2_cnt + mod_cnt;
    let dataset_and_models_bytes = ds1_bytes + ds2_bytes + fc1_bytes + fc2_bytes + mod_bytes;

    // SQLite DB + WAL + SHM
    let db_path = state.db.path();
    let mut db_main_bytes = 0u64;
    let mut db_wal_bytes = 0u64;
    let mut db_files_count = 0u64;
    if db_path.as_os_str() != ":memory:" {
        if let Ok(m) = std::fs::symlink_metadata(db_path)
            && m.file_type().is_file()
        {
            db_main_bytes = m.len();
            db_files_count += 1;
        }
        let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
        if let Ok(m) = std::fs::symlink_metadata(&wal_path)
            && m.file_type().is_file()
        {
            db_wal_bytes += m.len();
            db_files_count += 1;
        }
        let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
        if let Ok(m) = std::fs::symlink_metadata(&shm_path)
            && m.file_type().is_file()
        {
            db_wal_bytes += m.len();
            db_files_count += 1;
        }
    }
    let database_total_bytes = db_main_bytes + db_wal_bytes;

    // Finalize per-camera total_bytes & aggregate recordings categories
    let mut total_cont_bytes = 0u64;
    let mut total_mot_bytes = 0u64;
    let mut total_evt_seg_bytes = 0u64;
    let mut total_excess_bytes = 0u64;
    let mut total_excess_count = 0u64;
    let mut total_compacted_bytes = 0u64;
    let mut total_segments_count = 0u64;
    let mut total_events_count = 0u64;
    let mut total_pinned_events_count = 0u64;
    let mut total_reviews_count = 0u64;

    for c_dto in cam_dtos.values_mut() {
        c_dto.total_bytes = c_dto.recordings_bytes
            + c_dto.events_media_bytes
            + c_dto.reviews_media_bytes
            + c_dto.previews_bytes
            + c_dto.exports_bytes;
        total_cont_bytes += c_dto.continuous_bytes;
        total_mot_bytes += c_dto.motion_bytes;
        total_evt_seg_bytes += c_dto.event_segments_bytes;
        total_excess_bytes += c_dto.retained_past_continuous_bytes;
        total_excess_count += c_dto.retained_past_continuous_count;
        total_compacted_bytes += c_dto.compacted_bytes;
        total_segments_count += c_dto.segments_count;
        total_events_count += c_dto.events_count;
        total_pinned_events_count += c_dto.pinned_events_count;
        total_reviews_count += c_dto.reviews_count;
    }

    let total_rec_bytes = total_cont_bytes + total_mot_bytes + total_evt_seg_bytes;
    let total_tracked_bytes = total_rec_bytes
        + events_media_total_bytes
        + reviews_media_total_bytes
        + previews_media_total_bytes
        + exports_media_total_bytes
        + dataset_and_models_bytes
        + database_total_bytes;

    StorageBreakdownResponse {
        generated_at: now,
        total_tracked_bytes,
        volume_total_bytes: mb_to_bytes(volume_total_mb),
        volume_used_bytes: mb_to_bytes(volume_used_mb),
        volume_free_bytes: mb_to_bytes(volume_free_mb),
        scoped: false,
        categories: StorageCategoriesBreakdown {
            recordings_continuous: StorageCategoryItem {
                bytes: total_cont_bytes,
                count: total_segments_count,
            },
            recordings_motion: StorageCategoryItem {
                bytes: total_mot_bytes,
                count: 0,
            },
            recordings_events: StorageCategoryItem {
                bytes: total_evt_seg_bytes,
                count: 0,
            },
            recordings_retained_excess: StorageCategoryItem {
                bytes: total_excess_bytes,
                count: total_excess_count,
            },
            recordings_compacted: StorageCategoryItem {
                bytes: total_compacted_bytes,
                count: 0,
            },
            events_media: StorageCategoryItem {
                bytes: events_media_total_bytes,
                count: total_events_count,
            },
            pinned_events_count: total_pinned_events_count,
            reviews_media: StorageCategoryItem {
                bytes: reviews_media_total_bytes,
                count: total_reviews_count,
            },
            previews_media: StorageCategoryItem {
                bytes: previews_media_total_bytes,
                count: previews_media_total_files,
            },
            exports_media: StorageCategoryItem {
                bytes: exports_media_total_bytes,
                count: db_snap.export_entries.len() as u64,
            },
            share_artifacts_count,
            dataset_and_models: StorageCategoryItem {
                bytes: dataset_and_models_bytes,
                count: dataset_and_models_files,
            },
            dataset_samples_count: db_snap.dataset_samples_count,
            models_files_count: mod_cnt,
            database: StorageCategoryItem {
                bytes: database_total_bytes,
                count: db_files_count,
            },
            database_main_bytes: db_main_bytes,
            database_wal_bytes: db_wal_bytes,
            timeline_rows_count: db_snap.timeline_rows_count,
        },
        cameras: cam_dtos,
    }
}

fn filter_breakdown_for_scope(
    mut resp: StorageBreakdownResponse,
    scope: &CameraScope,
) -> StorageBreakdownResponse {
    if !scope.is_restricted() {
        return resp;
    }
    resp.scoped = true;
    resp.cameras.retain(|cam, _| scope.allows(cam));

    let mut cont_b = 0u64;
    let mut mot_b = 0u64;
    let mut evt_seg_b = 0u64;
    let mut excess_b = 0u64;
    let mut excess_c = 0u64;
    let mut comp_b = 0u64;
    let mut seg_c = 0u64;
    let mut ev_b = 0u64;
    let mut ev_c = 0u64;
    let mut pin_c = 0u64;
    let mut rev_b = 0u64;
    let mut rev_c = 0u64;
    let mut prev_b = 0u64;
    let mut prev_c = 0u64;
    let mut exp_b = 0u64;
    let mut exp_c = 0u64;

    for c in resp.cameras.values() {
        cont_b += c.continuous_bytes;
        mot_b += c.motion_bytes;
        evt_seg_b += c.event_segments_bytes;
        excess_b += c.retained_past_continuous_bytes;
        excess_c += c.retained_past_continuous_count;
        comp_b += c.compacted_bytes;
        seg_c += c.segments_count;
        ev_b += c.events_media_bytes;
        ev_c += c.events_count;
        pin_c += c.pinned_events_count;
        rev_b += c.reviews_media_bytes;
        rev_c += c.reviews_count;
        prev_b += c.previews_bytes;
        prev_c += c.previews_files_count;
        exp_b += c.exports_bytes;
        exp_c += c.exports_count;
    }

    resp.total_tracked_bytes = cont_b + mot_b + evt_seg_b + ev_b + rev_b + prev_b + exp_b;
    resp.categories = StorageCategoriesBreakdown {
        recordings_continuous: StorageCategoryItem {
            bytes: cont_b,
            count: seg_c,
        },
        recordings_motion: StorageCategoryItem {
            bytes: mot_b,
            count: 0,
        },
        recordings_events: StorageCategoryItem {
            bytes: evt_seg_b,
            count: 0,
        },
        recordings_retained_excess: StorageCategoryItem {
            bytes: excess_b,
            count: excess_c,
        },
        recordings_compacted: StorageCategoryItem {
            bytes: comp_b,
            count: 0,
        },
        events_media: StorageCategoryItem {
            bytes: ev_b,
            count: ev_c,
        },
        pinned_events_count: pin_c,
        reviews_media: StorageCategoryItem {
            bytes: rev_b,
            count: rev_c,
        },
        previews_media: StorageCategoryItem {
            bytes: prev_b,
            count: prev_c,
        },
        exports_media: StorageCategoryItem {
            bytes: exp_b,
            count: exp_c,
        },
        share_artifacts_count: 0,
        dataset_and_models: StorageCategoryItem::default(),
        dataset_samples_count: 0,
        models_files_count: 0,
        database: StorageCategoryItem::default(),
        database_main_bytes: 0,
        database_wal_bytes: 0,
        timeline_rows_count: 0,
    };
    resp
}

pub async fn api_storage_breakdown(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(q): Query<BreakdownQueryParams>,
) -> Response {
    let principal = match authenticate_read(&state, &jar, &headers).await {
        Ok(p) => p,
        Err(rej) => return rej.into_response(),
    };
    if !principal.can_mutate() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Operator or Admin role required" })),
        )
            .into_response();
    }
    let scope = principal.camera_scope();
    let requested_force = q
        .refresh
        .as_deref()
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "yes"));
    // Only unrestricted admins can bypass the normal 10s TTL, and even admin refreshes coalesce with a 2s floor
    let effective_ttl = if requested_force && principal.is_admin() {
        MIN_FORCE_REFRESH_TTL
    } else {
        BREAKDOWN_CACHE_TTL
    };

    let seq_fast = state.storage_breakdown_cache.composite_seq(
        state.db.recordings_mutation_seq(),
        state.db.events_mutation_seq(),
    );
    if let Some(cached) = state
        .storage_breakdown_cache
        .get_if_within_ttl(seq_fast, effective_ttl)
    {
        return Json(filter_breakdown_for_scope(cached, &scope)).into_response();
    }

    // Singleflight lock so concurrent callers share one filesystem/DB scan
    let _singleflight = state.storage_breakdown_cache.compute_lock.lock().await;
    let seq = state.storage_breakdown_cache.composite_seq(
        state.db.recordings_mutation_seq(),
        state.db.events_mutation_seq(),
    );
    if let Some(cached) = state
        .storage_breakdown_cache
        .get_if_within_ttl(seq, effective_ttl)
    {
        return Json(filter_breakdown_for_scope(cached, &scope)).into_response();
    }

    let cfg = state.config.read().await.clone();
    let state_for_scan = state.clone();
    let unscoped = match tokio::task::spawn_blocking(move || {
        compute_unscoped_breakdown_sync(&state_for_scan, &cfg)
    })
    .await
    {
        Ok(val) => val,
        Err(err) => {
            warn!("Storage breakdown task failed: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    state.storage_breakdown_cache.store(seq, unscoped.clone());
    Json(filter_breakdown_for_scope(unscoped, &scope)).into_response()
}

fn measure_wal_bytes(db_path: &Path) -> u64 {
    if db_path.as_os_str() == ":memory:" {
        return 0;
    }
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    std::fs::symlink_metadata(&wal_path)
        .ok()
        .filter(|m| m.file_type().is_file())
        .map_or(0, |m| m.len())
}

fn resolve_safe_preview_rel_path(raw_db_path: &str, expected_cam: &str) -> Option<PathBuf> {
    if !is_safe_id(expected_cam) {
        return None;
    }
    let rel_str = ninjanvr_db::strip_media_root(raw_db_path)
        .unwrap_or_else(|| raw_db_path.trim().trim_start_matches('/'));
    if rel_str.is_empty() || rel_str.contains("..") || rel_str.contains('\0') {
        return None;
    }
    let expected_a = format!("clips/previews/{expected_cam}/");
    let expected_b = format!("previews/{expected_cam}/");
    if rel_str.starts_with(&expected_a) || rel_str.starts_with(&expected_b) {
        Some(PathBuf::from(rel_str))
    } else {
        None
    }
}

pub async fn api_storage_cleanup(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(req): Json<StorageCleanupRequest>,
) -> Response {
    let camera_clean: Option<String> = req
        .camera
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
        .map(String::from);

    if let Some(ref cam) = camera_clean
        && !is_safe_id(cam)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "Invalid camera identifier"
            })),
        )
            .into_response();
    }

    if let Some(days) = req.older_than_days
        && (!days.is_finite() || days < 0.0 || days > 36500.0)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "message": "older_than_days must be a non-negative finite number"
            })),
        )
            .into_response();
    }

    let access_target = match camera_clean.as_deref() {
        Some(cam) => AccessTarget::Camera(cam),
        None => AccessTarget::Global,
    };
    let principal = match state
        .auth_gate
        .authorize_mutation(&state, &jar, &uri, &headers, true, access_target)
        .await
    {
        Ok(p) => p,
        Err(rej) => return rej.into_response(),
    };
    let scope = principal.camera_scope();

    if scope.is_restricted()
        && matches!(
            req.target,
            CleanupTarget::RetentionPass | CleanupTarget::WalCheckpoint
        )
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Camera-restricted users cannot run global maintenance tasks"
            })),
        )
            .into_response();
    }

    let started = Instant::now();
    let now = now_f64();
    let before_ts: Option<f64> = req
        .older_than_days
        .filter(|d| *d > 0.0)
        .map(|d| now - d * 86_400.0);

    let cfg = state.config.read().await.clone();
    let policies = extract_camera_policies(&cfg);
    let cold_dir_opt = resolve_configured_cold_dir(&cfg);
    let default_cont_days = sanitize_retention_days(cfg.record.continuous.days, 7.0, 0.002);

    if req.target == CleanupTarget::RetentionPass {
        let before_snap = {
            let st = state.clone();
            let c = cfg.clone();
            tokio::task::spawn_blocking(move || compute_unscoped_breakdown_sync(&st, &c))
                .await
                .unwrap_or_default()
        };
        let _ = state.recorder.run_retention_pass_once(now).await;
        let state_bg = state.clone();
        let cold_bg = cold_dir_opt.clone();
        let removed_orphans = tokio::task::spawn_blocking(move || {
            let orphans = state_bg
                .db
                .prune_missing_recording_files(|db_path| {
                    media_lifecycle::resolve_contained_recording_segment(
                        &state_bg.media_dir,
                        cold_bg.as_deref(),
                        None,
                        db_path,
                    )
                    .is_some()
                })
                .unwrap_or(0);
            let now_ms = (now * 1000.0) as u64;
            let _ = state_bg.preview_frames.prune(
                now_ms,
                crate::preview_frames::PREVIEW_FRAME_RETENTION_SECS * 1000,
            );
            purge_revoked_share_credits(&state_bg);
            let _ = state_bg.db.checkpoint_wal_truncate();
            orphans
        })
        .await
        .unwrap_or(0);

        state.vod_cache.invalidate_all();
        state.storage_breakdown_cache.invalidate();
        let after_snap = {
            let st = state.clone();
            let c = cfg.clone();
            tokio::task::spawn_blocking(move || compute_unscoped_breakdown_sync(&st, &c))
                .await
                .unwrap_or_default()
        };
        let post_seq = state.storage_breakdown_cache.composite_seq(
            state.db.recordings_mutation_seq(),
            state.db.events_mutation_seq(),
        );
        state
            .storage_breakdown_cache
            .store(post_seq, after_snap.clone());

        let deleted_segments = before_snap
            .categories
            .recordings_continuous
            .count
            .saturating_sub(after_snap.categories.recordings_continuous.count);
        let deleted_items = (removed_orphans as u64)
            + before_snap
                .categories
                .events_media
                .count
                .saturating_sub(after_snap.categories.events_media.count)
            + before_snap
                .categories
                .reviews_media
                .count
                .saturating_sub(after_snap.categories.reviews_media.count)
            + before_snap
                .categories
                .previews_media
                .count
                .saturating_sub(after_snap.categories.previews_media.count);
        let freed_bytes = before_snap
            .total_tracked_bytes
            .saturating_sub(after_snap.total_tracked_bytes);
        let duration_ms = started.elapsed().as_millis() as u64;
        return Json(StorageCleanupResponse {
            success: true,
            target: req.target,
            camera: None,
            deleted_items,
            deleted_segments,
            freed_bytes,
            duration_ms,
            message: format!("Retention pass completed in {duration_ms}ms"),
        })
        .into_response();
    }

    let state_work = state.clone();
    let camera_for_work = camera_clean.clone();
    let target = req.target;
    let include_pinned = req.include_pinned;
    let prune_retained_segments = req.prune_retained_segments;

    let work_res =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, u64, u64, Vec<String>)> {
            let mut deleted_items = 0u64;
            let mut deleted_segments = 0u64;
            let mut freed_bytes = 0u64;
            let mut invalidated_cameras: Vec<String> = Vec::new();

            let target_cameras = |db_cams: BTreeSet<String>| -> Vec<String> {
                let mut set: BTreeSet<String> = policies.keys().cloned().collect();
                for c in db_cams {
                    set.insert(c);
                }
                set.into_iter()
                    .filter(|c| {
                        if let Some(ref only_cam) = camera_for_work {
                            c == only_cam
                        } else {
                            scope.allows(c)
                        }
                    })
                    .collect()
            };

            match target {
                CleanupTarget::Events => {
                    let wal_before = measure_wal_bytes(state_work.db.path());
                    let purged = state_work.db.purge_events_filtered(
                        &scope,
                        camera_for_work.as_deref(),
                        before_ts,
                        now,
                        include_pinned,
                    )?;
                    deleted_items = purged.len() as u64;
                    let mut affected_cams: BTreeSet<String> = BTreeSet::new();
                    for (ev_id, cam, thumb_opt) in &purged {
                        affected_cams.insert(cam.clone());
                        freed_bytes += media_lifecycle::cleanup_event_media_artifacts_bytes(
                            &state_work.media_dir,
                            ev_id,
                            Some(cam.as_str()),
                            thumb_opt.as_deref(),
                        );
                    }
                    purge_revoked_share_credits(&state_work);

                    // Sweep genuinely orphaned event files in top-level `clips/` using exact DB primary-key
                    // and `thumb_path` verification (zero truncation, multi-prefix camera disambiguation).
                    let clips_dir = state_work.media_dir.join("clips");
                    let grace_cutoff = before_ts.unwrap_or(now - 60.0).min(now - 60.0);
                    if std::fs::symlink_metadata(&clips_dir).is_ok_and(|m| m.file_type().is_dir())
                        && let Ok(entries) = std::fs::read_dir(&clips_dir)
                    {
                        let db_cams: BTreeSet<String> = state_work
                            .db
                            .query_storage_db_snapshot(&HashMap::new(), now, now)
                            .map(|s| s.cameras.keys().cloned().collect())
                            .unwrap_or_default();
                        let mut all_cams_desc: Vec<String> = policies
                            .keys()
                            .chain(db_cams.iter())
                            .cloned()
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        all_cams_desc.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));

                        let mut candidates: Vec<(String, Vec<String>)> = Vec::new();
                        for entry in entries.flatten().take(MAX_SCAN_ENTRIES_PER_DIR) {
                            let p = entry.path();
                            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                                continue;
                            };
                            if !meta.file_type().is_file() {
                                continue;
                            }
                            let Some(fname) = p.file_name().and_then(|n| n.to_str()) else {
                                continue;
                            };
                            if fname.starts_with('.')
                                || fname.starts_with("latest_")
                                || fname.starts_with("share_clip_")
                                || fname.starts_with("share_redacted_")
                                || fname.starts_with("review-")
                                || !is_safe_id(fname)
                            {
                                continue;
                            }
                            let ext_ok = Path::new(fname)
                                .extension()
                                .and_then(|e| e.to_str())
                                .is_some_and(|e| {
                                    matches!(
                                        e.to_ascii_lowercase().as_str(),
                                        "jpg" | "jpeg" | "webp" | "mp4"
                                    )
                                });
                            if !ext_ok {
                                continue;
                            }
                            let mtime_ok = meta
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                                .is_some_and(|d| d.as_secs_f64() < grace_cutoff);
                            if !mtime_ok {
                                continue;
                            }

                            let stem_raw = Path::new(fname)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("");
                            let stem_clean = stem_raw.strip_suffix("-clean").unwrap_or(stem_raw);
                            let decomps =
                                all_camera_prefix_decompositions(stem_clean, &all_cams_desc);

                            // Require every matching camera prefix to be permitted by `camera_for_work` / `scope`
                            // so `cam` vs `cam-north` ambiguity can never delete across camera boundaries.
                            let permitted = if decomps.is_empty() {
                                camera_for_work.is_none() && !scope.is_restricted()
                            } else if let Some(ref only_cam) = camera_for_work {
                                decomps.len() == 1 && decomps[0].0 == only_cam.as_str()
                            } else {
                                decomps.iter().all(|(cp, _)| scope.allows(cp))
                            };
                            if !permitted {
                                continue;
                            }

                            let mut cand_ids = vec![stem_clean.to_string()];
                            for (_cp, id_part) in decomps {
                                cand_ids.push(id_part.to_string());
                            }
                            candidates.push((fname.to_string(), cand_ids));
                        }

                        if !candidates.is_empty()
                            && let Ok(unreferenced_fnames) =
                                state_work.db.filter_unreferenced_clip_candidates(&candidates)
                        {
                            for fname in unreferenced_fnames {
                                let rel = Path::new("clips").join(&fname);
                                freed_bytes +=
                                    media_lifecycle::remove_non_symlink_under_media_dir_bytes(
                                        &state_work.media_dir,
                                        &rel,
                                    );
                            }
                        }
                    }

                    if prune_retained_segments {
                        let cams_to_check: Vec<String> = if let Some(ref c) = camera_for_work {
                            vec![c.clone()]
                        } else {
                            affected_cams.into_iter().collect()
                        };
                        for cam in cams_to_check {
                            if !scope.allows(&cam) {
                                continue;
                            }
                            let pol = policies.get(&cam);
                            let cont_days = pol.map_or(default_cont_days, |p| p.continuous_days);
                            let motion_days = pol.map_or(cont_days, |p| p.motion_days);
                            let pre_sec = pol.map_or(5.0, |p| p.pre_capture_sec);
                            let post_sec = pol.map_or(5.0, |p| p.post_capture_sec);
                            let cont_cutoff = now - cont_days * 86_400.0;
                            let motion_cutoff_opt =
                                (motion_days > cont_days).then_some(now - motion_days * 86_400.0);
                            let effective_cutoff = match before_ts {
                                Some(bt) => cont_cutoff.min(bt),
                                None => cont_cutoff,
                            };
                            let deleted_rows = state_work.db.delete_recordings_before_cutoff(
                                &cam,
                                effective_cutoff,
                                pre_sec,
                                post_sec,
                                !include_pinned,
                                true,
                                true,
                                motion_cutoff_opt,
                            )?;
                            if !deleted_rows.is_empty() {
                                let (seg_cnt, seg_bytes) =
                                    media_lifecycle::unlink_deleted_recording_rows(
                                        &state_work.media_dir,
                                        cold_dir_opt.as_deref(),
                                        &cam,
                                        &deleted_rows,
                                    );
                                deleted_segments += seg_cnt as u64;
                                freed_bytes += seg_bytes;
                                state_work.share_engine.on_camera_recordings_purged(
                                    &state_work,
                                    &cam,
                                    0.0,
                                    effective_cutoff,
                                );
                                invalidated_cameras.push(cam);
                            }
                        }
                    }
                    let _ = state_work.db.checkpoint_wal_truncate();
                    let wal_after = measure_wal_bytes(state_work.db.path());
                    freed_bytes += wal_before.saturating_sub(wal_after);
                }
                CleanupTarget::Reviews => {
                    let wal_before = measure_wal_bytes(state_work.db.path());
                    let purged = state_work.db.purge_reviews_filtered(
                        &scope,
                        camera_for_work.as_deref(),
                        before_ts,
                        now,
                        include_pinned,
                    )?;
                    deleted_items = purged.len() as u64;
                    for (rev_id, cam, thumb_opt) in &purged {
                        freed_bytes += media_lifecycle::cleanup_review_media_artifacts_bytes(
                            &state_work.media_dir,
                            rev_id,
                            Some(cam.as_str()),
                            thumb_opt.as_deref(),
                        );
                    }
                    purge_revoked_share_credits(&state_work);
                    let _ = state_work.db.checkpoint_wal_truncate();
                    let wal_after = measure_wal_bytes(state_work.db.path());
                    freed_bytes += wal_before.saturating_sub(wal_after);
                }
                CleanupTarget::ExcessRecordings | CleanupTarget::Recordings => {
                    let db_snap = state_work
                        .db
                        .query_storage_db_snapshot(&HashMap::new(), now, now)
                        .unwrap_or_default();
                    let cams = target_cameras(db_snap.cameras.keys().cloned().collect());
                    for cam in cams {
                        let pol = policies.get(&cam);
                        let cont_days = pol.map_or(default_cont_days, |p| p.continuous_days);
                        let pre_sec = pol.map_or(5.0, |p| p.pre_capture_sec);
                        let post_sec = pol.map_or(5.0, |p| p.post_capture_sec);
                        let cutoff = if target == CleanupTarget::ExcessRecordings {
                            let cont_cutoff = now - cont_days * 86_400.0;
                            match before_ts {
                                Some(bt) => cont_cutoff.min(bt),
                                None => cont_cutoff,
                            }
                        } else {
                            before_ts.unwrap_or(now)
                        };
                        let deleted_rows = state_work.db.delete_recordings_before_cutoff(
                            &cam,
                            cutoff,
                            pre_sec,
                            post_sec,
                            !include_pinned,
                            false,
                            false,
                            None,
                        )?;
                        if !deleted_rows.is_empty() {
                            let (seg_cnt, seg_bytes) =
                                media_lifecycle::unlink_deleted_recording_rows(
                                    &state_work.media_dir,
                                    cold_dir_opt.as_deref(),
                                    &cam,
                                    &deleted_rows,
                                );
                            deleted_items += seg_cnt as u64;
                            deleted_segments += seg_cnt as u64;
                            freed_bytes += seg_bytes;
                            state_work
                                .share_engine
                                .on_camera_recordings_purged(&state_work, &cam, 0.0, cutoff);
                            invalidated_cameras.push(cam);
                        }
                    }
                    let _ = state_work.db.checkpoint_wal_truncate();
                }
                CleanupTarget::Previews => {
                    let purged_previews = state_work.db.purge_previews_filtered(
                        &scope,
                        camera_for_work.as_deref(),
                        before_ts,
                    )?;
                    for (_id, cam, db_path) in &purged_previews {
                        deleted_items += 1;
                        if let Some(safe_rel) = resolve_safe_preview_rel_path(db_path, cam) {
                            freed_bytes += media_lifecycle::remove_non_symlink_under_media_dir_bytes(
                                &state_work.media_dir,
                                &safe_rel,
                            );
                        }
                        if is_safe_id(cam)
                            && let Some(fname) = Path::new(db_path).file_name().and_then(|n| n.to_str())
                            && is_safe_id(fname)
                        {
                            let fallback_rel =
                                Path::new("clips").join("previews").join(cam).join(fname);
                            freed_bytes += media_lifecycle::remove_non_symlink_under_media_dir_bytes(
                                &state_work.media_dir,
                                &fallback_rel,
                            );
                        }
                    }
                    // Also sweep unindexed hourly preview MP4s in `clips/previews/<cam>/` when no age filter is set
                    if before_ts.is_none() {
                        let previews_root = state_work.media_dir.join("clips").join("previews");
                        if std::fs::symlink_metadata(&previews_root).is_ok_and(|m| m.file_type().is_dir())
                            && let Ok(cam_dirs) = std::fs::read_dir(&previews_root)
                        {
                            for cam_entry in cam_dirs.flatten().take(1024) {
                                let cp = cam_entry.path();
                                let Some(cam_name) = cp.file_name().and_then(|n| n.to_str()) else {
                                    continue;
                                };
                                if !is_safe_id(cam_name) {
                                    continue;
                                }
                                if let Some(ref only_cam) = camera_for_work {
                                    if cam_name != only_cam {
                                        continue;
                                    }
                                } else if !scope.allows(cam_name) {
                                    continue;
                                }
                                if std::fs::symlink_metadata(&cp).is_ok_and(|m| m.file_type().is_dir())
                                    && let Ok(files) = std::fs::read_dir(&cp)
                                {
                                    for f in files.flatten().take(MAX_SCAN_ENTRIES_PER_DIR) {
                                        if let Some(fname) = f.file_name().to_str()
                                            && is_safe_id(fname)
                                        {
                                            let rel = Path::new("clips")
                                                .join("previews")
                                                .join(cam_name)
                                                .join(fname);
                                            let b =
                                                media_lifecycle::remove_non_symlink_under_media_dir_bytes(
                                                    &state_work.media_dir,
                                                    &rel,
                                                );
                                            if b > 0 {
                                                deleted_items += 1;
                                                freed_bytes += b;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let before_ms = before_ts.map(|t| (t.max(0.0) * 1000.0) as u64);
                    let (pf_cnt, pf_bytes) = state_work.preview_frames.purge_filtered(
                        &scope,
                        camera_for_work.as_deref(),
                        before_ms,
                    );
                    deleted_items += pf_cnt as u64;
                    freed_bytes += pf_bytes;

                    if !scope.is_restricted() && camera_for_work.is_none() && before_ts.is_none() {
                        let (rf_cnt, rf_bytes) = remove_regular_files_under_media_subdir(
                            &state_work.media_dir,
                            &Path::new("clips").join("review_frames"),
                            2,
                        );
                        deleted_items += rf_cnt;
                        freed_bytes += rf_bytes;
                    }

                    let _ = state_work.db.checkpoint_wal_truncate();
                }
                CleanupTarget::Exports => {
                    let export_ids = state_work.db.list_completed_exports_filtered(
                        &scope,
                        camera_for_work.as_deref(),
                        before_ts,
                    )?;
                    for exp_id in &export_ids {
                        state_work.export_jobs.remove(exp_id);
                        if let Ok((deleted, bytes)) =
                            media_lifecycle::delete_export_with_artifacts_bytes(
                                &state_work.db,
                                &state_work.media_dir,
                                exp_id,
                            )
                            && deleted
                        {
                            deleted_items += 1;
                            freed_bytes += bytes;
                        }
                    }
                    if !scope.is_restricted()
                        && camera_for_work.is_none()
                        && let Ok(expired_targets) =
                            state_work.db.list_expired_or_revoked_share_link_targets(now)
                    {
                        let _ = state_work.db.prune_expired_share_links(now);
                        for tid in expired_targets {
                            if !state_work
                                .db
                                .has_active_share_links_for_target(&tid, now)
                                .unwrap_or(true)
                                && !state_work
                                    .db
                                    .has_recently_accessed_share_links_for_target(&tid, now, 900.0)
                                    .unwrap_or(true)
                            {
                                let b = media_lifecycle::cleanup_share_clip_artifacts_in_dir_bytes(
                                    &state_work.media_dir,
                                    "",
                                    &tid,
                                );
                                if b > 0 {
                                    deleted_items += 1;
                                    freed_bytes += b;
                                }
                            }
                        }
                    }
                    purge_revoked_share_credits(&state_work);
                    let _ = state_work.db.checkpoint_wal_truncate();
                }
                CleanupTarget::WalCheckpoint => {
                    let wal_before = measure_wal_bytes(state_work.db.path());
                    let (_log_frames, ckpt_frames) = state_work.db.checkpoint_wal_truncate()?;
                    let wal_after = measure_wal_bytes(state_work.db.path());
                    deleted_items = ckpt_frames.max(0) as u64;
                    freed_bytes = wal_before.saturating_sub(wal_after);
                }
                CleanupTarget::RetentionPass => unreachable!(),
            }

            Ok((deleted_items, deleted_segments, freed_bytes, invalidated_cameras))
        })
        .await;

    let (deleted_items, deleted_segments, freed_bytes, invalidated_cameras) = match work_res {
        Ok(Ok(tuple)) => tuple,
        Ok(Err(err)) => {
            warn!("Storage cleanup ({:?}) failed: {err:#}", req.target);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "message": "Storage cleanup operation failed"
                })),
            )
                .into_response();
        }
        Err(err) => {
            warn!("Storage cleanup worker panicked: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    for cam in &invalidated_cameras {
        state.vod_cache.invalidate_camera(cam);
    }
    state.storage_breakdown_cache.invalidate();

    let duration_ms = started.elapsed().as_millis() as u64;
    let freed_mb = (freed_bytes as f64) / (1024.0 * 1024.0);
    info!(
        "Storage cleanup {:?} (camera={:?}): deleted_items={}, deleted_segments={}, freed={:.2} MB in {}ms",
        req.target, camera_clean, deleted_items, deleted_segments, freed_mb, duration_ms
    );

    Json(StorageCleanupResponse {
        success: true,
        target: req.target,
        camera: camera_clean,
        deleted_items,
        deleted_segments,
        freed_bytes,
        duration_ms,
        message: format!(
            "Cleaned {deleted_items} item(s) and {deleted_segments} segment(s), freeing {freed_mb:.2} MB"
        ),
    })
    .into_response()
}
