#![forbid(unsafe_code)]
//! HLS fMP4 VOD route handlers (`master.m3u8`, `index.m3u8`, `init-v1-a1.mp4`, `seg-*.m4s`)
//! backed by an `O(1)` bounded TTL `VodWindowCache` so `.m4s` segment requests resolve
//! without repeating SQLite range scans or per-segment base directory canonicalizations.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::CookieJar;
use ninjanvr_config::NinjaConfig;
use ninjanvr_db::RecordingRow;
use serde::Deserialize;
use tracing::warn;

use crate::auth_gate::caller_allowed_cameras;
use crate::media_lifecycle::resolve_recording_disk_path;
use crate::media_redaction::MediaDeliveryGate;
use crate::share_engine::{ensure_path_within_canon_media_dir, ensure_path_within_media_dir};
use crate::{AppState, is_safe_id};

pub const MAX_VOD_SEGMENTS: usize = 8640;
pub const MAX_VOD_WINDOW_SEC: f64 = 86_400.0;
pub const MAX_VOD_CACHE_ENTRIES: usize = 64;
pub const VOD_CACHE_TTL: Duration = Duration::from_secs(30);

fn normalize_f64_bits(val: f64) -> u64 {
    if val == 0.0 {
        0.0f64.to_bits()
    } else {
        val.to_bits()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VodWindowCacheKey {
    pub camera: String,
    pub start_bits: u64,
    pub end_bits: u64,
    pub cold_dir: Option<PathBuf>,
}

impl VodWindowCacheKey {
    pub fn new(camera: &str, start: f64, end: f64, cold_dir: Option<&Path>) -> Self {
        Self {
            camera: camera.to_string(),
            start_bits: normalize_f64_bits(start),
            end_bits: normalize_f64_bits(end),
            cold_dir: cold_dir.map(Path::to_path_buf),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedVodSegment {
    pub recording: RecordingRow,
    pub disk_path: PathBuf,
    pub cumulative_base_time: f64,
}

type VodCacheMap = HashMap<VodWindowCacheKey, (Instant, u64, Arc<Vec<ResolvedVodSegment>>)>;

/// Bounded `O(1)` TTL cache for resolved HLS VOD windows (`(camera, start, end)` -> segments),
/// automatically invalidated whenever `NinjaDatabase::recordings_mutation_seq()` advances.
#[derive(Clone, Debug)]
pub struct VodWindowCache {
    entries: Arc<std::sync::RwLock<VodCacheMap>>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl Default for VodWindowCache {
    fn default() -> Self {
        Self::new()
    }
}

impl VodWindowCache {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(std::sync::RwLock::new(HashMap::new())),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn get(&self, key: &VodWindowCacheKey) -> Option<Arc<Vec<ResolvedVodSegment>>> {
        self.get_for_mutation_seq(key, None)
    }

    pub fn get_for_mutation_seq(
        &self,
        key: &VodWindowCacheKey,
        expected_mutation_seq: Option<u64>,
    ) -> Option<Arc<Vec<ResolvedVodSegment>>> {
        if let Ok(guard) = self.entries.read()
            && let Some((cached_at, seq, segs)) = guard.get(key)
            && cached_at.elapsed() <= VOD_CACHE_TTL
            && expected_mutation_seq.is_none_or(|exp| *seq == exp)
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(Arc::clone(segs));
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub fn insert(&self, key: VodWindowCacheKey, segments: Arc<Vec<ResolvedVodSegment>>) {
        self.insert_with_mutation_seq(key, 0, segments);
    }

    pub fn insert_with_mutation_seq(
        &self,
        key: VodWindowCacheKey,
        mutation_seq: u64,
        segments: Arc<Vec<ResolvedVodSegment>>,
    ) {
        let now = Instant::now();
        let Ok(mut guard) = self.entries.write() else {
            return;
        };
        if !guard.contains_key(&key) && guard.len() >= MAX_VOD_CACHE_ENTRIES {
            guard.retain(|_, (at, _, _)| now.duration_since(*at) <= VOD_CACHE_TTL);
            while guard.len() >= MAX_VOD_CACHE_ENTRIES {
                if let Some(oldest_key) = guard
                    .iter()
                    .min_by_key(|(_, (at, _, _))| *at)
                    .map(|(k, _)| k.clone())
                {
                    guard.remove(&oldest_key);
                } else {
                    break;
                }
            }
        }
        guard.insert(key, (now, mutation_seq, segments));
    }

    pub fn invalidate_camera(&self, camera: &str) {
        if let Ok(mut guard) = self.entries.write() {
            guard.retain(|k, _| k.camera != camera);
        }
    }

    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.entries.write() {
            guard.clear();
        }
    }

    pub fn hit_count(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn miss_count(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

pub(crate) fn resolve_configured_cold_dir(cfg: &NinjaConfig) -> Option<PathBuf> {
    if cfg.record.storage_tiers.enabled
        && let Some(ref cold_dir) = cfg.record.storage_tiers.cold_dir
        && !cold_dir.as_os_str().is_empty()
    {
        return Some(cold_dir.clone());
    }
    None
}

fn clamped_segment_duration(dur: f64) -> f64 {
    if dur.is_finite() && dur > 0.0 {
        dur.clamp(0.1, 3600.0)
    } else {
        10.0
    }
}

fn resolve_segment_with_precanon_roots(
    media_dir: &Path,
    canon_rec_dir: Option<&Path>,
    cold_dir_and_canon: Option<(&Path, &Path)>,
    camera: Option<&str>,
    db_path: &str,
) -> Option<PathBuf> {
    if db_path.is_empty()
        || Path::new(db_path)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    let disk_path = resolve_recording_disk_path(media_dir, db_path);
    if let Some(canon_rec) = canon_rec_dir
        && let Some(safe_hot) = ensure_path_within_canon_media_dir(canon_rec, &disk_path)
        && safe_hot.is_file()
        && safe_hot
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
    {
        let cam_ok = match camera {
            Some(cam) => safe_hot
                .strip_prefix(canon_rec)
                .ok()
                .and_then(|rel| rel.parent())
                .is_none_or(|parent| {
                    parent.as_os_str().is_empty()
                        || parent.components().any(|c| c.as_os_str() == cam)
                }),
            None => true,
        };
        if cam_ok {
            return Some(safe_hot);
        }
    }

    let (cold_root, canon_cold) = cold_dir_and_canon?;
    let mut cold_candidates = vec![disk_path];
    if let Some(rel) = ninjanvr_db::strip_media_root(db_path) {
        let rel_stripped = rel.strip_prefix("recordings/").unwrap_or(rel);
        cold_candidates.push(cold_root.join(rel_stripped));
        cold_candidates.push(cold_root.join(rel));
    }
    for cand in cold_candidates {
        let Some(safe_path) = ensure_path_within_canon_media_dir(canon_cold, &cand) else {
            continue;
        };
        if !safe_path.is_file()
            || !safe_path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
        {
            continue;
        }
        if let Some(cam) = camera
            && let Ok(rel) = safe_path.strip_prefix(canon_cold)
            && let Some(parent) = rel.parent()
            && !parent.as_os_str().is_empty()
            && !parent.components().any(|c| c.as_os_str() == cam)
        {
            continue;
        }
        return Some(safe_path);
    }
    None
}

pub(crate) fn resolve_vod_segments_uncached(
    state: &AppState,
    cold_dir: Option<&Path>,
    camera: &str,
    start: f64,
    end: f64,
) -> Vec<ResolvedVodSegment> {
    if camera.is_empty()
        || !is_safe_id(camera)
        || !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end < start
        || (end - start) > MAX_VOD_WINDOW_SEC
    {
        return Vec::new();
    }

    let recordings_dir = state.media_dir.join("recordings");
    let canon_rec_dir = ensure_path_within_media_dir(&state.media_dir, &recordings_dir);
    let canon_cold_dir = cold_dir.and_then(|cold_root| {
        if std::fs::symlink_metadata(cold_root).is_ok_and(|m| m.file_type().is_symlink()) {
            None
        } else {
            std::fs::canonicalize(cold_root).ok()
        }
    });
    let cold_pair = cold_dir.zip(canon_cold_dir.as_deref());

    let rows = state
        .db
        .query_recordings(camera, start, end)
        .unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len().min(MAX_VOD_SEGMENTS));
    let mut cumulative = 0.0f64;
    for r in rows {
        if out.len() >= MAX_VOD_SEGMENTS {
            break;
        }
        let Some(safe_path) = resolve_segment_with_precanon_roots(
            &state.media_dir,
            canon_rec_dir.as_deref(),
            cold_pair,
            Some(camera),
            &r.path,
        ) else {
            continue;
        };
        let dur = clamped_segment_duration(r.duration);
        out.push(ResolvedVodSegment {
            recording: r,
            disk_path: safe_path,
            cumulative_base_time: cumulative,
        });
        cumulative += dur;
    }
    out
}

#[cfg(test)]
pub(crate) fn existing_recordings_for_vod_with_cold(
    state: &AppState,
    cold_dir: Option<&Path>,
    camera: &str,
    start: f64,
    end: f64,
) -> Vec<(RecordingRow, PathBuf)> {
    resolve_vod_segments_uncached(state, cold_dir, camera, start, end)
        .into_iter()
        .map(|seg| (seg.recording, seg.disk_path))
        .collect()
}

#[cfg(test)]
pub(crate) fn existing_recordings_for_vod(
    state: &AppState,
    camera: &str,
    start: f64,
    end: f64,
) -> Vec<RecordingRow> {
    existing_recordings_for_vod_with_cold(state, None, camera, start, end)
        .into_iter()
        .map(|(r, _)| r)
        .collect()
}

async fn get_or_resolve_vod_window(
    state: &AppState,
    cold_dir_opt: Option<PathBuf>,
    camera: &str,
    start: f64,
    end: f64,
) -> Option<Arc<Vec<ResolvedVodSegment>>> {
    let key = VodWindowCacheKey::new(camera, start, end, cold_dir_opt.as_deref());
    let cur_seq = state.db.recordings_vod_invalidation_seq();
    if let Some(cached) = state.vod_cache.get_for_mutation_seq(&key, Some(cur_seq)) {
        return Some(cached);
    }
    let state_for_vod = state.clone();
    let cam_owned = camera.to_string();
    let resolved = tokio::task::spawn_blocking(move || {
        resolve_vod_segments_uncached(
            &state_for_vod,
            cold_dir_opt.as_deref(),
            &cam_owned,
            start,
            end,
        )
    })
    .await
    .ok()?;
    let arc_segs = Arc::new(resolved);
    if !arc_segs.is_empty() {
        let post_seq = state.db.recordings_vod_invalidation_seq();
        state
            .vod_cache
            .insert_with_mutation_seq(key, post_seq, Arc::clone(&arc_segs));
    }
    Some(arc_segs)
}

pub async fn vod_master_m3u8(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
) -> Response {
    if camera.is_empty() || !is_safe_id(&camera) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end < start
        || (end - start) > MAX_VOD_WINDOW_SEC
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let cold_dir_opt = {
        let cfg = state.config.read().await;
        if MediaDeliveryGate::blocks_untranscoded_video_in_config(&cfg, &camera) {
            return StatusCode::NOT_FOUND.into_response();
        }
        resolve_configured_cold_dir(&cfg)
    };
    let profile_opt = if let Some(segs) =
        get_or_resolve_vod_window(&state, cold_dir_opt, &camera, start, end).await
    {
        let candidate_paths: Vec<_> = segs.iter().take(8).map(|s| s.disk_path.clone()).collect();
        if candidate_paths.is_empty() {
            None
        } else {
            tokio::task::spawn_blocking(move || {
                candidate_paths
                    .iter()
                    .find_map(|p| ninjanvr_vod::probe_mp4_stream_profile(p))
            })
            .await
            .ok()
            .flatten()
        }
    } else {
        None
    };
    (
        [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
        ninjanvr_vod::build_master_playlist_for_profile(profile_opt.as_ref()),
    )
        .into_response()
}

pub async fn vod_index_m3u8(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
) -> Response {
    if camera.is_empty()
        || !is_safe_id(&camera)
        || !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end < start
        || (end - start) > MAX_VOD_WINDOW_SEC
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (privacy_mask_active, cold_dir_opt) = {
        let cfg = state.config.read().await;
        let masked = MediaDeliveryGate::blocks_untranscoded_video_in_config(&cfg, &camera);
        (masked, resolve_configured_cold_dir(&cfg))
    };
    if privacy_mask_active {
        return StatusCode::NOT_FOUND.into_response();
    }

    let state_for_vod = state.clone();
    let cam_for_vod = camera.clone();
    let cache_key = VodWindowCacheKey::new(&camera, start, end, cold_dir_opt.as_deref());
    let Ok((segs_arc, resolutions)) = tokio::task::spawn_blocking(move || {
        let resolved = resolve_vod_segments_uncached(
            &state_for_vod,
            cold_dir_opt.as_deref(),
            &cam_for_vod,
            start,
            end,
        );
        let mut cached_res: Option<(u32, u32)> = None;
        let mut prev_end: Option<f64> = None;
        let mut prev_rate: Option<f64> = None;
        let mut prev_had_objects: Option<bool> = None;
        let mut probes_done = 0usize;
        let mut resolutions: Vec<Option<(u32, u32)>> = Vec::with_capacity(resolved.len());
        for seg in &resolved {
            let dur = if seg.recording.duration.is_finite() && seg.recording.duration > 0.0 {
                seg.recording.duration.clamp(0.1, 3600.0)
            } else {
                10.0
            };
            let safe_size =
                if seg.recording.segment_size.is_finite() && seg.recording.segment_size >= 0.0 {
                    seg.recording.segment_size
                } else {
                    0.0
                };
            let cur_rate = safe_size / dur;
            let cur_has_objects = seg.recording.objects > 0;
            let time_gap = prev_end.is_some_and(|pe| (seg.recording.start_time - pe).abs() > 1.5);
            let objects_flipped = prev_had_objects.is_some_and(|po| po != cur_has_objects);
            let rate_shifted = prev_rate.is_some_and(|pr| {
                pr > 0.001 && cur_rate > 0.001 && ((cur_rate / pr) > 1.75 || (pr / cur_rate) > 1.75)
            });
            let should_probe = probes_done < 16
                || time_gap
                || objects_flipped
                || rate_shifted
                || cached_res.is_none();

            let res_opt = if should_probe && probes_done < 256 {
                probes_done += 1;
                if let Some(probed) = ninjanvr_vod::probe_mp4_resolution(&seg.disk_path) {
                    cached_res = Some(probed);
                    Some(probed)
                } else if rate_shifted || objects_flipped {
                    cached_res = None;
                    None
                } else {
                    cached_res
                }
            } else if rate_shifted || objects_flipped {
                cached_res = None;
                None
            } else {
                cached_res
            };
            resolutions.push(res_opt);
            prev_end = Some(seg.recording.end_time);
            prev_rate = Some(cur_rate);
            prev_had_objects = Some(cur_has_objects);
        }
        (Arc::new(resolved), resolutions)
    })
    .await
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if segs_arc.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let inv_seq = state.db.recordings_vod_invalidation_seq();
    state
        .vod_cache
        .insert_with_mutation_seq(cache_key, inv_seq, Arc::clone(&segs_arc));
    let recs: Vec<RecordingRow> = segs_arc.iter().map(|s| s.recording.clone()).collect();
    match ninjanvr_vod::build_media_playlist_with_optional_resolutions(&recs, &resolutions) {
        Ok(m3u8) => (
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            m3u8,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct VodInitQuery {
    pub seg: Option<usize>,
}

pub async fn vod_init_mp4(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, start, end)): AxumPath<(String, f64, f64)>,
    Query(q): Query<VodInitQuery>,
) -> Response {
    if camera.is_empty()
        || !is_safe_id(&camera)
        || !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end < start
        || (end - start) > MAX_VOD_WINDOW_SEC
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (privacy_mask_active, cold_dir_opt) = {
        let cfg = state.config.read().await;
        let masked = MediaDeliveryGate::blocks_untranscoded_video_in_config(&cfg, &camera);
        (masked, resolve_configured_cold_dir(&cfg))
    };
    if privacy_mask_active {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(seg) = q.seg
        && (seg == 0 || seg > MAX_VOD_SEGMENTS)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let start_idx = q.seg.unwrap_or(1).saturating_sub(1);
    let Some(segs_arc) = get_or_resolve_vod_window(&state, cold_dir_opt, &camera, start, end).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if start_idx >= segs_arc.len() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let Ok(Ok(_vod_permit)) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.vod_remux_semaphore.acquire(),
    )
    .await
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let Ok(Some(init_bytes)) = tokio::task::spawn_blocking(move || {
        for seg in segs_arc.iter().skip(start_idx).take(8) {
            if let Ok(init_bytes) = ninjanvr_vod::build_init_mp4(&seg.disk_path) {
                return Some(init_bytes);
            }
        }
        None
    })
    .await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    ([(header::CONTENT_TYPE, "video/mp4")], init_bytes).into_response()
}

pub async fn vod_segment_m4s(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath((camera, start, end, segment)): AxumPath<(String, f64, f64, String)>,
) -> Response {
    if camera.is_empty()
        || !is_safe_id(&camera)
        || !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end < start
        || (end - start) > MAX_VOD_WINDOW_SEC
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == &camera || a == "*")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (privacy_mask_active, cold_dir_opt) = {
        let cfg = state.config.read().await;
        let masked = MediaDeliveryGate::blocks_untranscoded_video_in_config(&cfg, &camera);
        (masked, resolve_configured_cold_dir(&cfg))
    };
    if privacy_mask_active {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !segment.ends_with(".m4s") || !is_safe_id(&segment) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(num_str) = segment
        .strip_prefix("seg-")
        .and_then(|s| s.split('-').next())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(seq_num) = num_str.parse::<usize>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if seq_num == 0 || seq_num > MAX_VOD_SEGMENTS {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(seq_u32) = u32::try_from(seq_num) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Some(segs_arc) = get_or_resolve_vod_window(&state, cold_dir_opt, &camera, start, end).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(target_seg) = segs_arc.get(seq_num - 1).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Ok(Ok(_vod_permit)) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.vod_remux_semaphore.acquire(),
    )
    .await
    else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    match tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        ninjanvr_vod::build_media_segment_m4s(
            &target_seg.disk_path,
            seq_u32,
            target_seg.cumulative_base_time,
        )
    })
    .await
    {
        Ok(Ok(bytes)) => ([(header::CONTENT_TYPE, "video/iso.segment")], bytes).into_response(),
        Ok(Err(err)) => {
            warn!("Failed to build fMP4 segment {segment} for {camera}: {err:#}");
            StatusCode::NOT_FOUND.into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
