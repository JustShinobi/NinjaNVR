#![forbid(unsafe_code)]
//! Deepened Guest Share & Redaction Engine (`ShareLinkEngine`).
//!
//! ## Design-It-Twice Comparison (`/codebase-design`)
//!
//! - **Design A — Unified `ShareLinkEngine` + `ShareClipTranscoder` Port (Selected):**
//!   Consolidates HMAC-SHA256 share token signing/verification, `SharePlaybackCredit`
//!   byte-range continuation and Safari `bytes=0-1` probe tracking, JTI and creator RBAC
//!   auto-revocation/re-grant lifecycle hooks, and cached privacy-redacted/audio-stripped
//!   MP4 preparation behind an `AppState`-owned `ShareLinkEngine`. External FFmpeg/FFprobe
//!   execution is isolated behind a two-adapter `ShareClipTranscoder` trait (`FfmpegShareClipTranscoder`
//!   for production and `FakeShareClipTranscoder` / `InMemoryShareClipTranscoder` for fast,
//!   deterministic in-memory tests).
//! - **Design B — Scattered Maps & Inline FFmpeg Spawning in `lib.rs` (Rejected):**
//!   Leaves four separate mutex/semaphore fields on `AppState` and requires every user,
//!   token, review, event, export, and recording deletion handler in `lib.rs` to manually
//!   sequence DB revocation, auto-revocation map pruning, credit purging, and file unlinking.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use ninjanvr_db::{EventFilter, ReviewFilter};
use ninjanvr_media::build_mp4_export_with_privacy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::{
    AppState, caller_allowed_cameras, extract_authenticated_user_with_cfg,
    infer_camera_prefix_from_id, open_regular_non_symlink_readonly,
    read_bounded_non_symlink_bytes_sync, resolve_recording_disk_path, unix_now,
    verify_caller_can_mutate, verify_cookie_csrf_origin, xml_escape_simple,
};

// ============================================================================
// Part 1 — ShareClipTranscoder Seam (Ports & Adapters)
// ============================================================================

/// Port for redacting MP4 clips with privacy masks and stripping audio tracks.
///
/// Production code uses [`FfmpegShareClipTranscoder`], while unit tests use
/// [`FakeShareClipTranscoder`] (`InMemoryShareClipTranscoder`) to verify `.mask_hash`
/// cache invalidation and audio-strip invariants in-memory without spawning `ffmpeg`.
pub trait ShareClipTranscoder: Send + Sync {
    /// Redacts `source_path` using `privacy_mask` drawbox filters and strips audio,
    /// writing the result atomically to `out_file`.
    fn redact_mp4_with_privacy<'a>(
        &'a self,
        source_path: &'a Path,
        exports_dir: &'a Path,
        redacted_id: &'a str,
        out_file: &'a Path,
        privacy_mask: Option<&'a Value>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

    /// Strips any audio track from `source_path`, writing the video-only MP4 to `out_file`.
    fn strip_mp4_audio<'a>(
        &'a self,
        source_path: &'a Path,
        exports_dir: &'a Path,
        clip_id: &'a str,
        out_file: &'a Path,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;

    /// Returns `true` if `path` contains an audio (`soun`) track or cannot be verified as video-only.
    fn has_audio_track(&self, path: &Path) -> bool {
        share_mp4_file_has_audio_track(path)
    }
}

/// Production `ShareClipTranscoder` adapter backed by `ffmpeg`.
#[derive(Debug, Default, Clone, Copy)]
pub struct FfmpegShareClipTranscoder;

impl FfmpegShareClipTranscoder {
    pub const fn new() -> Self {
        Self
    }
}

impl ShareClipTranscoder for FfmpegShareClipTranscoder {
    fn redact_mp4_with_privacy<'a>(
        &'a self,
        source_path: &'a Path,
        exports_dir: &'a Path,
        redacted_id: &'a str,
        out_file: &'a Path,
        privacy_mask: Option<&'a Value>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(redact_mp4_file_with_privacy(
            source_path,
            exports_dir,
            redacted_id,
            out_file,
            privacy_mask,
        ))
    }

    fn strip_mp4_audio<'a>(
        &'a self,
        source_path: &'a Path,
        exports_dir: &'a Path,
        clip_id: &'a str,
        out_file: &'a Path,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(strip_mp4_audio_to_file(
            source_path,
            exports_dir,
            clip_id,
            out_file,
        ))
    }
}

/// Recorded redaction invocation captured by [`FakeShareClipTranscoder`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRedactionCall {
    pub source_path: PathBuf,
    pub redacted_id: String,
    pub out_file: PathBuf,
    pub drawbox_filter: String,
}

/// Recorded audio-strip invocation captured by [`FakeShareClipTranscoder`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedAudioStripCall {
    pub source_path: PathBuf,
    pub clip_id: String,
    pub out_file: PathBuf,
}

/// Deterministic in-memory `ShareClipTranscoder` test adapter that synthesizes valid
/// ISO-BMFF video-only MP4 files without spawning `ffmpeg`.
#[derive(Debug, Default)]
pub struct FakeShareClipTranscoder {
    redactions: std::sync::Mutex<Vec<RecordedRedactionCall>>,
    audio_strips: std::sync::Mutex<Vec<RecordedAudioStripCall>>,
    fail_next: std::sync::atomic::AtomicBool,
}

/// Type alias for [`FakeShareClipTranscoder`].
pub type InMemoryShareClipTranscoder = FakeShareClipTranscoder;

impl FakeShareClipTranscoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_fail_next(&self, fail: bool) {
        self.fail_next
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn redaction_count(&self) -> usize {
        self.redactions.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn audio_strip_count(&self) -> usize {
        self.audio_strips.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn recorded_redactions(&self) -> Vec<RecordedRedactionCall> {
        self.redactions
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn recorded_audio_strips(&self) -> Vec<RecordedAudioStripCall> {
        self.audio_strips
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

/// Synthesizes a valid ISO-BMFF MP4 byte buffer containing only a `vide` track (`ftyp` + `mdat` + `moov`)
/// and embedding `payload_tag` inside `mdat` so tests can verify exact redaction provenance.
pub fn synthesize_video_only_mp4_bytes(payload_tag: &[u8]) -> Vec<u8> {
    fn mp4_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let total = (8 + payload.len()) as u32;
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        out.extend_from_slice(payload);
        out
    }

    fn mp4_full_box(box_type: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let total = (12 + body.len()) as u32;
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(box_type);
        let vf = ((version as u32) << 24) | (flags & 0x00FF_FFFF);
        out.extend_from_slice(&vf.to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    let ftyp = mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isom");
    let mut video_sample = vec![0x65u8; 64];
    video_sample.extend_from_slice(payload_tag);
    let mdat_offset = (ftyp.len() + 8) as u32;
    let mdat = mp4_box(b"mdat", &video_sample);

    let mut tkhd_body = vec![0u8; 80];
    tkhd_body[8..12].copy_from_slice(&1u32.to_be_bytes());
    tkhd_body[72..76].copy_from_slice(&(640u32 << 16).to_be_bytes());
    tkhd_body[76..80].copy_from_slice(&(480u32 << 16).to_be_bytes());
    let tkhd = mp4_full_box(b"tkhd", 0, 3, &tkhd_body);

    let mut mdhd_body = vec![0u8; 20];
    mdhd_body[8..12].copy_from_slice(&90000u32.to_be_bytes());
    mdhd_body[12..16].copy_from_slice(&900000u32.to_be_bytes());
    let mdhd = mp4_full_box(b"mdhd", 0, 0, &mdhd_body);

    let mut hdlr_body = vec![0u8; 20];
    hdlr_body[4..8].copy_from_slice(b"vide");
    let hdlr = mp4_full_box(b"hdlr", 0, 0, &hdlr_body);

    let stsd = mp4_full_box(b"stsd", 0, 0, &[0, 0, 0, 1]);
    let mut stsz_body = Vec::new();
    stsz_body.extend_from_slice(&(video_sample.len() as u32).to_be_bytes());
    stsz_body.extend_from_slice(&1u32.to_be_bytes());
    let stsz = mp4_full_box(b"stsz", 0, 0, &stsz_body);

    let mut stts_body = Vec::new();
    stts_body.extend_from_slice(&1u32.to_be_bytes());
    stts_body.extend_from_slice(&1u32.to_be_bytes());
    stts_body.extend_from_slice(&900000u32.to_be_bytes());
    let stts = mp4_full_box(b"stts", 0, 0, &stts_body);

    let mut stsc_body = Vec::new();
    stsc_body.extend_from_slice(&1u32.to_be_bytes());
    stsc_body.extend_from_slice(&1u32.to_be_bytes());
    stsc_body.extend_from_slice(&1u32.to_be_bytes());
    stsc_body.extend_from_slice(&1u32.to_be_bytes());
    let stsc = mp4_full_box(b"stsc", 0, 0, &stsc_body);

    let mut stco_body = Vec::new();
    stco_body.extend_from_slice(&1u32.to_be_bytes());
    stco_body.extend_from_slice(&mdat_offset.to_be_bytes());
    let stco = mp4_full_box(b"stco", 0, 0, &stco_body);

    let mut stbl_children = Vec::new();
    stbl_children.extend_from_slice(&stsd);
    stbl_children.extend_from_slice(&stsz);
    stbl_children.extend_from_slice(&stts);
    stbl_children.extend_from_slice(&stsc);
    stbl_children.extend_from_slice(&stco);
    let stbl = mp4_box(b"stbl", &stbl_children);
    let minf = mp4_box(b"minf", &stbl);

    let mut mdia_children = Vec::new();
    mdia_children.extend_from_slice(&mdhd);
    mdia_children.extend_from_slice(&hdlr);
    mdia_children.extend_from_slice(&minf);
    let mdia = mp4_box(b"mdia", &mdia_children);

    let mut trak_children = Vec::new();
    trak_children.extend_from_slice(&tkhd);
    trak_children.extend_from_slice(&mdia);
    let vide_trak = mp4_box(b"trak", &trak_children);

    let mvhd = mp4_full_box(b"mvhd", 0, 0, &[0u8; 96]);
    let mut moov_children = Vec::new();
    moov_children.extend_from_slice(&mvhd);
    moov_children.extend_from_slice(&vide_trak);
    let moov = mp4_box(b"moov", &moov_children);

    let mut out = Vec::new();
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&mdat);
    out.extend_from_slice(&moov);
    out
}

impl ShareClipTranscoder for FakeShareClipTranscoder {
    fn redact_mp4_with_privacy<'a>(
        &'a self,
        source_path: &'a Path,
        exports_dir: &'a Path,
        redacted_id: &'a str,
        out_file: &'a Path,
        privacy_mask: Option<&'a Value>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return false;
            }
            let Some(vf) = ninjanvr_media::build_privacy_drawbox_filter(privacy_mask) else {
                return false;
            };
            if !is_regular_non_symlink_file(source_path) {
                return false;
            }
            if std::fs::create_dir_all(exports_dir).is_err() {
                return false;
            }
            let Some(fname) = out_file.file_name().and_then(|s| s.to_str()) else {
                return false;
            };
            let bytes = synthesize_video_only_mp4_bytes(vf.as_bytes());
            if !write_atomic_non_symlink_file(exports_dir, fname, &bytes) {
                return false;
            }
            if let Ok(mut guard) = self.redactions.lock() {
                guard.push(RecordedRedactionCall {
                    source_path: source_path.to_path_buf(),
                    redacted_id: redacted_id.to_string(),
                    out_file: out_file.to_path_buf(),
                    drawbox_filter: vf,
                });
            }
            true
        })
    }

    fn strip_mp4_audio<'a>(
        &'a self,
        source_path: &'a Path,
        exports_dir: &'a Path,
        clip_id: &'a str,
        out_file: &'a Path,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return false;
            }
            if !is_regular_non_symlink_file(source_path) {
                return false;
            }
            if std::fs::create_dir_all(exports_dir).is_err() {
                return false;
            }
            let Some(fname) = out_file.file_name().and_then(|s| s.to_str()) else {
                return false;
            };
            let bytes = synthesize_video_only_mp4_bytes(clip_id.as_bytes());
            if !write_atomic_non_symlink_file(exports_dir, fname, &bytes) {
                return false;
            }
            if let Ok(mut guard) = self.audio_strips.lock() {
                guard.push(RecordedAudioStripCall {
                    source_path: source_path.to_path_buf(),
                    clip_id: clip_id.to_string(),
                    out_file: out_file.to_path_buf(),
                });
            }
            true
        })
    }
}

// ============================================================================
// Part 2 — ShareLinkEngine & SharePlaybackCredit State Machine
// ============================================================================

/// Deep `AppState`-owned engine encapsulating guest share link signing, playback credits,
/// byte-range continuation, creator RBAC auto-revocation, and redacted clip caching.
#[derive(Clone)]
pub struct ShareLinkEngine {
    pub clip_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    pub page_video_credits: Arc<std::sync::Mutex<HashMap<String, SharePlaybackCredit>>>,
    pub creator_auto_revoked: Arc<std::sync::Mutex<HashMap<String, &'static str>>>,
    pub ffmpeg_semaphore: Arc<tokio::sync::Semaphore>,
    pub transcoder: Arc<dyn ShareClipTranscoder>,
}

impl Default for ShareLinkEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ShareLinkEngine {
    /// Creates a new `ShareLinkEngine` backed by the production [`FfmpegShareClipTranscoder`].
    pub fn new() -> Self {
        Self::with_transcoder(Arc::new(FfmpegShareClipTranscoder::new()))
    }

    /// Creates a new `ShareLinkEngine` with a custom [`ShareClipTranscoder`] (such as [`FakeShareClipTranscoder`]).
    pub fn with_transcoder(transcoder: Arc<dyn ShareClipTranscoder>) -> Self {
        Self {
            clip_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            page_video_credits: Arc::new(std::sync::Mutex::new(HashMap::new())),
            creator_auto_revoked: Arc::new(std::sync::Mutex::new(HashMap::new())),
            ffmpeg_semaphore: Arc::new(tokio::sync::Semaphore::new(2)),
            transcoder,
        }
    }

    pub fn derive_signing_key(jwt_secret: &str) -> [u8; 32] {
        derive_share_signing_key(jwt_secret)
    }

    pub fn sign_token(secret: &str, payload: &ShareTokenPayload) -> String {
        sign_share_token(secret, payload)
    }

    pub fn verify_token(
        secret: &str,
        token: &str,
        now_ts: u64,
    ) -> std::result::Result<ShareTokenPayload, StatusCode> {
        verify_share_token(secret, token, now_ts)
    }

    /// Collapses share revocation and credit cleanup when a user's role or camera scope changes.
    pub fn on_creator_permissions_changed(
        &self,
        state: &AppState,
        username: &str,
        new_role: Option<&str>,
    ) -> Result<()> {
        if let Some(role) = new_role
            && role.trim().eq_ignore_ascii_case("viewer")
        {
            state.db.revoke_share_links_by_creator(username)?;
            if let Ok(mut auto_map) = self.creator_auto_revoked.lock() {
                remove_creator_from_auto_revoked(state, &mut auto_map, Some(username), None);
            }
        }
        purge_revoked_share_credits(state);
        Ok(())
    }

    /// Collapses share revocation, auto-revocation cleanup, and credit purging when a user is deleted.
    pub fn on_creator_deleted(&self, state: &AppState, username: &str) -> Result<()> {
        state.db.revoke_share_links_by_creator(username)?;
        if let Ok(mut auto_map) = self.creator_auto_revoked.lock() {
            remove_creator_from_auto_revoked(state, &mut auto_map, Some(username), None);
        }
        purge_revoked_share_credits(state);
        Ok(())
    }

    /// Collapses share revocation, auto-revocation cleanup, and credit purging when an API token is deleted.
    pub fn on_creator_token_revoked(
        &self,
        state: &AppState,
        token_name: Option<&str>,
    ) -> Result<()> {
        if let Some(name) = token_name {
            state
                .db
                .revoke_share_links_by_creator(&format!("token:{name}"))?;
        }
        if let Ok(mut auto_map) = self.creator_auto_revoked.lock() {
            remove_creator_from_auto_revoked(state, &mut auto_map, None, token_name);
        }
        purge_revoked_share_credits(state);
        Ok(())
    }

    /// Collapses share link revocation, credit purging, and cached share artifact unlinking when a
    /// target review, event, or export is deleted.
    pub fn on_target_deleted(
        &self,
        state: &AppState,
        kind: Option<&str>,
        target_id: &str,
    ) -> Result<()> {
        state.db.revoke_share_links_for_target(kind, target_id)?;
        purge_revoked_share_credits(state);
        crate::media_lifecycle::cleanup_share_clip_artifacts_in_dir(
            &state.media_dir,
            kind.unwrap_or(""),
            target_id,
        );
        cleanup_share_clip_artifacts(&state.media_dir, target_id);
        Ok(())
    }

    /// Collapses range-based share link revocation and artifact cleanup when camera recordings are purged.
    pub fn on_camera_recordings_purged(
        &self,
        state: &AppState,
        camera: &str,
        after: f64,
        before: f64,
    ) {
        cleanup_camera_share_clip_artifacts_in_range(state, camera, after, before);
    }

    /// Acquires the per-clip build mutex for `clip_key` so concurrent viewers share a single transcode.
    pub async fn acquire_clip_lock(&self, clip_key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.clip_locks.lock().await;
        if map.len() >= 64 {
            map.retain(|_, v| Arc::strong_count(v) > 1);
        }
        map.entry(clip_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Prepares and caches a privacy-redacted, audio-free MP4 clip for `safe_id` using `self.transcoder`,
    /// validating `.share_redacted_{safe_id}.mask_hash` and source mtime freshness.
    pub async fn prepare_redacted_export_clip(
        &self,
        media_dir: &Path,
        source_path: &Path,
        safe_id: &str,
        raw_privacy_mask: Option<&Value>,
        normalized_privacy_mask: Option<&Value>,
    ) -> std::result::Result<PathBuf, StatusCode> {
        self.prepare_redacted_export_clip_with_head(
            media_dir,
            source_path,
            safe_id,
            raw_privacy_mask,
            normalized_privacy_mask,
            false,
        )
        .await?
        .ok_or(StatusCode::NOT_FOUND)
    }

    /// Prepares and caches a privacy-redacted, audio-free MP4 clip for `safe_id`, returning
    /// `Ok(None)` when `is_head` is `true` and the redacted artifact is not yet cached.
    pub async fn prepare_redacted_export_clip_with_head(
        &self,
        media_dir: &Path,
        source_path: &Path,
        safe_id: &str,
        raw_privacy_mask: Option<&Value>,
        normalized_privacy_mask: Option<&Value>,
        is_head: bool,
    ) -> std::result::Result<Option<PathBuf>, StatusCode> {
        if safe_id.is_empty() || !is_safe_id(safe_id) {
            return Err(StatusCode::BAD_REQUEST);
        }
        if ninjanvr_media::build_privacy_drawbox_filter(normalized_privacy_mask).is_none() {
            return Err(StatusCode::NOT_FOUND);
        }
        let Some(exports_dir) = ensure_safe_media_subdir(media_dir, "exports") else {
            return Err(StatusCode::NOT_FOUND);
        };
        let redacted_id = format!("share_redacted_{safe_id}");
        let redacted_fname = format!("{redacted_id}.mp4");
        let mask_hash_fname = format!(".{redacted_id}.mask_hash");
        let redacted_path = exports_dir.join(&redacted_fname);
        let mask_hash_path = exports_dir.join(&mask_hash_fname);
        let current_mask_hash = compute_privacy_mask_hash(raw_privacy_mask);

        let clip_lock = self.acquire_clip_lock(&redacted_id).await;
        let _clip_guard = clip_lock.lock().await;

        let cache_invalid = {
            let red_p = redacted_path.clone();
            let mh_p = mask_hash_path.clone();
            let src_p = source_path.to_path_buf();
            let exp_hash = current_mask_hash.clone();
            let transcoder = self.transcoder.clone();
            tokio::task::spawn_blocking(move || {
                if is_regular_non_symlink_file(&red_p) {
                    let hash_matches = read_non_symlink_string(&mh_p)
                        .map(|existing_hash| existing_hash.trim() == exp_hash)
                        .unwrap_or(false);
                    !hash_matches
                        || is_cached_share_artifact_stale(&red_p, std::slice::from_ref(&src_p))
                        || transcoder.has_audio_track(&red_p)
                } else {
                    std::fs::symlink_metadata(&red_p).is_ok()
                }
            })
            .await
            .unwrap_or(true)
        };
        if cache_invalid {
            remove_non_symlink_dir_child(&exports_dir, &redacted_fname);
            remove_non_symlink_dir_child(&exports_dir, &mask_hash_fname);
        }

        if !is_regular_non_symlink_file(&redacted_path) {
            if is_head {
                return Ok(None);
            }
            let Ok(_sem_permit) = self.ffmpeg_semaphore.acquire().await else {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
            let build_ok = self
                .transcoder
                .redact_mp4_with_privacy(
                    source_path,
                    &exports_dir,
                    &redacted_id,
                    &redacted_path,
                    normalized_privacy_mask,
                )
                .await;
            let valid_no_audio = if build_ok && is_regular_non_symlink_file(&redacted_path) {
                let red_check = redacted_path.clone();
                let transcoder = self.transcoder.clone();
                tokio::task::spawn_blocking(move || !transcoder.has_audio_track(&red_check))
                    .await
                    .unwrap_or(false)
            } else {
                false
            };
            if valid_no_audio {
                let exp_dir_clone = exports_dir.clone();
                let mh_fname_clone = mask_hash_fname.clone();
                let mh_bytes = current_mask_hash.into_bytes();
                let _ = tokio::task::spawn_blocking(move || {
                    write_atomic_non_symlink_file(&exp_dir_clone, &mh_fname_clone, &mh_bytes)
                })
                .await;
            } else {
                remove_non_symlink_dir_child(&exports_dir, &redacted_fname);
                remove_non_symlink_dir_child(&exports_dir, &mask_hash_fname);
                return Err(StatusCode::NOT_FOUND);
            }
        }

        ensure_path_within_media_dir(media_dir, &redacted_path)
            .map(Some)
            .ok_or(StatusCode::NOT_FOUND)
    }

    /// Prepares and caches an audio-stripped MP4 clip for `safe_id` using `self.transcoder`.
    pub async fn prepare_audio_stripped_export_clip(
        &self,
        media_dir: &Path,
        source_path: &Path,
        safe_id: &str,
    ) -> std::result::Result<PathBuf, StatusCode> {
        self.prepare_audio_stripped_export_clip_with_head(media_dir, source_path, safe_id, false)
            .await?
            .ok_or(StatusCode::NOT_FOUND)
    }

    /// Prepares and caches an audio-stripped MP4 clip for `safe_id`, returning `Ok(None)` when
    /// `is_head` is `true` and the stripped artifact is not yet cached.
    pub async fn prepare_audio_stripped_export_clip_with_head(
        &self,
        media_dir: &Path,
        source_path: &Path,
        safe_id: &str,
        is_head: bool,
    ) -> std::result::Result<Option<PathBuf>, StatusCode> {
        if safe_id.is_empty() || !is_safe_id(safe_id) {
            return Err(StatusCode::BAD_REQUEST);
        }
        let Some(exports_dir) = ensure_safe_media_subdir(media_dir, "exports") else {
            return Err(StatusCode::NOT_FOUND);
        };
        let clip_id = format!("share_clip_{safe_id}");
        let clip_fname = format!("{clip_id}.mp4");
        let clip_path = exports_dir.join(&clip_fname);

        let clip_lock = self.acquire_clip_lock(&clip_id).await;
        let _clip_guard = clip_lock.lock().await;

        let clip_invalid = {
            let cp = clip_path.clone();
            let sp = source_path.to_path_buf();
            let transcoder = self.transcoder.clone();
            tokio::task::spawn_blocking(move || {
                (is_regular_non_symlink_file(&cp)
                    && (transcoder.has_audio_track(&cp)
                        || is_cached_share_artifact_stale(&cp, std::slice::from_ref(&sp))))
                    || (std::fs::symlink_metadata(&cp).is_ok() && !is_regular_non_symlink_file(&cp))
            })
            .await
            .unwrap_or(true)
        };
        if clip_invalid {
            remove_non_symlink_dir_child(&exports_dir, &clip_fname);
        }

        if !is_regular_non_symlink_file(&clip_path) {
            if is_head {
                return Ok(None);
            }
            let Ok(_sem_permit) = self.ffmpeg_semaphore.acquire().await else {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            };
            let build_ok = self
                .transcoder
                .strip_mp4_audio(source_path, &exports_dir, &clip_id, &clip_path)
                .await;
            let valid_no_audio = if build_ok && is_regular_non_symlink_file(&clip_path) {
                let cp_check = clip_path.clone();
                let transcoder = self.transcoder.clone();
                tokio::task::spawn_blocking(move || !transcoder.has_audio_track(&cp_check))
                    .await
                    .unwrap_or(false)
            } else {
                false
            };
            if !valid_no_audio {
                remove_non_symlink_dir_child(&exports_dir, &clip_fname);
                return Err(StatusCode::NOT_FOUND);
            }
        }

        ensure_path_within_media_dir(media_dir, &clip_path)
            .map(Some)
            .ok_or(StatusCode::NOT_FOUND)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharePlaybackCredit {
    pub page_credits: usize,
    pub active_stream_started: bool,
    pub min_next_range_start: u64,
    pub active_streams: Vec<u64>,
    pub active_stream_expires: Vec<u64>,
    pub sessions: HashMap<String, (u64, bool)>,
}

impl SharePlaybackCredit {
    pub fn has_active_session(&self, now_ts: u64) -> bool {
        self.sessions.values().any(|&(exp, _)| exp > now_ts)
    }

    pub fn is_valid_sid(&self, sid: &str, now_ts: u64) -> bool {
        !sid.is_empty()
            && self
                .sessions
                .get(sid)
                .is_some_and(|&(exp, claimed)| exp > now_ts && (claimed || self.page_credits > 0))
    }

    pub fn prune_expired_sessions(&mut self, now_ts: u64) {
        let expired_unclaimed = self
            .sessions
            .values()
            .filter(|&&(exp, claimed)| exp <= now_ts && !claimed)
            .count();
        self.sessions.retain(|_, (exp, _)| *exp > now_ts);
        self.page_credits = self.page_credits.saturating_sub(expired_unclaimed);

        if self.active_streams.is_empty()
            && self.active_stream_started
            && self.active_stream_expires.is_empty()
        {
            self.active_streams.push(self.min_next_range_start);
            self.active_stream_expires.push(u64::MAX);
        }
        let mut i = 0;
        while i < self.active_streams.len() {
            let exp = self
                .active_stream_expires
                .get(i)
                .copied()
                .unwrap_or(u64::MAX);
            if exp <= now_ts {
                self.active_streams.remove(i);
                if i < self.active_stream_expires.len() {
                    self.active_stream_expires.remove(i);
                }
            } else {
                i += 1;
            }
        }
        self.active_stream_expires
            .truncate(self.active_streams.len());
        self.sync_stream_summary();
    }

    pub fn insert_session(&mut self, sid: String, expires_at: u64, now_ts: u64) {
        self.prune_expired_sessions(now_ts);
        if let Some(&(_, claimed)) = self.sessions.get(&sid)
            && !claimed
        {
            self.page_credits = self.page_credits.saturating_sub(1);
        }
        while self.sessions.len() >= 64 && !self.sessions.contains_key(&sid) {
            if let Some(oldest_key) = self
                .sessions
                .iter()
                .min_by_key(|(_, (exp, _))| *exp)
                .map(|(k, _)| k.clone())
            {
                if let Some((_, claimed)) = self.sessions.remove(&oldest_key)
                    && !claimed
                {
                    self.page_credits = self.page_credits.saturating_sub(1);
                }
            } else {
                break;
            }
        }
        self.sessions.insert(sid, (expires_at, false));
    }

    pub fn has_any_active_stream(&self) -> bool {
        self.active_stream_started || !self.active_streams.is_empty()
    }

    pub fn record_initial_stream(&mut self, min_next_start: u64) {
        self.record_initial_stream_at(min_next_start, unix_now().saturating_add(900));
    }

    pub fn record_initial_stream_at(&mut self, min_next_start: u64, expires_at: u64) {
        while self.active_streams.len() >= 16 {
            self.active_streams.remove(0);
            if !self.active_stream_expires.is_empty() {
                self.active_stream_expires.remove(0);
            }
        }
        while self.active_stream_expires.len() < self.active_streams.len() {
            self.active_stream_expires.push(expires_at);
        }
        self.active_stream_expires
            .truncate(self.active_streams.len());
        self.active_streams.push(min_next_start);
        self.active_stream_expires.push(expires_at);
        self.sync_stream_summary();
    }

    pub fn sync_stream_summary(&mut self) {
        if self.active_streams.is_empty() {
            self.active_stream_started = false;
            self.min_next_range_start = u64::MAX;
        } else {
            self.active_stream_started = true;
            self.min_next_range_start = self
                .active_streams
                .iter()
                .copied()
                .min()
                .unwrap_or(u64::MAX);
        }
    }
}

/// Strips `share_redacted_` or `share_clip_` prefix from `raw_id`, returning `(is_share_artifact, bare_id)`.
pub fn strip_share_artifact_prefix(raw_id: &str) -> (bool, &str) {
    if let Some(rest) = raw_id
        .strip_prefix("share_redacted_")
        .or_else(|| raw_id.strip_prefix("share_clip_"))
    {
        (true, rest)
    } else {
        (false, raw_id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShareTokenPayload {
    pub k: String,
    pub id: String,
    pub exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cam: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateShareBody {
    #[serde(default = "default_share_kind")]
    pub kind: String,
    pub target_id: String,
    #[serde(default = "default_share_ttl")]
    pub expires_in_sec: u64,
    #[serde(default)]
    pub max_views: Option<u64>,
}

fn default_share_kind() -> String {
    "export".to_string()
}

fn default_share_ttl() -> u64 {
    86400
}

pub fn derive_share_signing_key(jwt_secret: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"ninjanvr-share-link-subkey-v1")
        .expect("HMAC accepts any key");
    mac.update(jwt_secret.as_bytes());
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

pub fn sign_share_token(secret: &str, payload: &ShareTokenPayload) -> String {
    let json_bytes = serde_json::to_vec(payload).unwrap_or_default();
    let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&json_bytes);
    let subkey = derive_share_signing_key(secret);
    let mut mac = Hmac::<Sha256>::new_from_slice(&subkey).expect("HMAC accepts any key");
    mac.update(b"ninjanvr-share-v1.");
    mac.update(payload_b64.as_bytes());
    let sig_bytes = mac.finalize().into_bytes();
    let sig_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig_bytes);
    format!("{payload_b64}.{sig_b64}")
}

pub fn verify_share_token(
    secret: &str,
    token: &str,
    now_ts: u64,
) -> std::result::Result<ShareTokenPayload, StatusCode> {
    let Some((payload_b64, sig_b64)) = token.split_once('.') else {
        return Err(StatusCode::FORBIDDEN);
    };
    let Ok(provided_sig) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig_b64) else {
        return Err(StatusCode::FORBIDDEN);
    };
    let subkey = derive_share_signing_key(secret);
    let mut mac = Hmac::<Sha256>::new_from_slice(&subkey).expect("HMAC accepts any key");
    mac.update(b"ninjanvr-share-v1.");
    mac.update(payload_b64.as_bytes());
    if mac.verify_slice(&provided_sig).is_err() {
        return Err(StatusCode::FORBIDDEN);
    }
    let Ok(payload_bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64)
    else {
        return Err(StatusCode::FORBIDDEN);
    };
    let Ok(payload) = serde_json::from_slice::<ShareTokenPayload>(&payload_bytes) else {
        return Err(StatusCode::FORBIDDEN);
    };
    if !matches!(payload.k.as_str(), "export" | "review" | "event") {
        return Err(StatusCode::FORBIDDEN);
    }
    if payload.exp <= now_ts {
        return Err(StatusCode::GONE);
    }
    Ok(payload)
}

pub(crate) fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

pub(crate) fn is_disallowed_export_fallback_id(id: &str) -> bool {
    id.is_empty()
        || id.starts_with('.')
        || id.starts_with("share_redacted_")
        || id.starts_with("share_clip_")
        || id.contains("..")
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

pub fn has_valid_recording_segments_for_window(
    state: &AppState,
    camera: &str,
    start: f64,
    raw_end: f64,
) -> bool {
    let end = raw_end.max(start).min(start + 300.0);
    let has_valid_in_rows = |rows: Vec<ninjanvr_db::RecordingRow>| -> bool {
        rows.into_iter().any(|rec| {
            let disk_path = resolve_recording_disk_path(&state.media_dir, &rec.path);
            ensure_path_within_media_dir(&state.media_dir, &disk_path).is_some_and(|p| p.is_file())
        })
    };
    state
        .db
        .query_recordings(camera, start - 5.0, end + 5.0)
        .is_ok_and(has_valid_in_rows)
        || state
            .db
            .query_recordings(camera, start - 10.0, end + 10.0)
            .is_ok_and(has_valid_in_rows)
}

pub fn resolve_share_target_metadata(
    state: &AppState,
    kind: &str,
    target_id: &str,
) -> (Option<String>, String, Option<u64>, bool) {
    if kind == "export" {
        if let Ok(Some(exp)) = state.db.get_export_by_id(target_id) {
            if exp["in_progress"].as_bool().unwrap_or(false) {
                return (None, target_id.to_string(), None, false);
            }
            let valid_file = exp["video_path"]
                .as_str()
                .and_then(|vp| {
                    ensure_path_within_media_dir(
                        &state.media_dir,
                        &resolve_recording_disk_path(&state.media_dir, vp),
                    )
                })
                .is_some_and(|p| p.is_file());
            if !valid_file {
                return (None, target_id.to_string(), None, false);
            }
            let cam = exp["camera"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| infer_camera_prefix_from_id(state, target_id))
                .or_else(|| {
                    exp["video_path"]
                        .as_str()
                        .and_then(|vp| Path::new(vp).file_stem())
                        .and_then(|s| s.to_str())
                        .and_then(|stem| infer_camera_prefix_from_id(state, stem))
                });
            let name = exp["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(target_id)
                .to_string();
            let date_ts = exp["date"].as_f64().map(|t| t as u64);
            return (cam, name, date_ts, true);
        }
        if !is_disallowed_export_fallback_id(target_id)
            && ensure_path_within_media_dir(
                &state.media_dir,
                &state
                    .media_dir
                    .join("exports")
                    .join(format!("{target_id}.mp4")),
            )
            .is_some_and(|p| p.is_file())
        {
            let cam_prefix = infer_camera_prefix_from_id(state, target_id).or_else(|| {
                target_id
                    .rsplitn(3, '_')
                    .nth(2)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            });
            return (cam_prefix, target_id.to_string(), None, true);
        }
        return (None, target_id.to_string(), None, false);
    }

    if (kind == "review" || kind == "event")
        && let (Ok(Some(rev)), Ok(Some(ev))) = (
            state.db.get_review_by_id(target_id),
            state.db.get_event(target_id),
        )
        && ev["camera"]
            .as_str()
            .is_some_and(|ec| ec.trim() != rev.camera.trim())
    {
        return (None, target_id.to_string(), None, false);
    }

    if kind == "review" {
        if let Ok(Some(rev)) = state.db.get_review_by_id(target_id) {
            let cam_name = rev.camera.trim().to_string();
            if cam_name.is_empty() {
                return (None, target_id.to_string(), None, false);
            }
            let end_f64 = rev.end_time.unwrap_or(rev.start_time + 15.0);
            let has_recs =
                has_valid_recording_segments_for_window(state, &cam_name, rev.start_time, end_f64);
            let title = format!("{cam_name} // {}", rev.severity.to_ascii_uppercase());
            return (Some(cam_name), title, Some(rev.start_time as u64), has_recs);
        }
        if let Ok(Some(ev)) = state.db.get_event(target_id) {
            let Some(cam_str) = ev["camera"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return (None, target_id.to_string(), None, false);
            };
            let start_f64 = ev["start_time"].as_f64().unwrap_or(0.0);
            let end_f64 = ev["end_time"].as_f64().unwrap_or(start_f64 + 15.0);
            let has_recs =
                has_valid_recording_segments_for_window(state, cam_str, start_f64, end_f64);
            let cam = Some(cam_str.to_string());
            let label = ev["label"].as_str().unwrap_or("event");
            let start = ev["start_time"].as_f64().map(|t| t as u64);
            let title = format!("{cam_str} // {label}");
            return (cam, title, start, has_recs);
        }
        return (None, target_id.to_string(), None, false);
    }

    if kind == "event" {
        if let Ok(Some(ev)) = state.db.get_event(target_id) {
            let Some(cam_str) = ev["camera"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return (None, target_id.to_string(), None, false);
            };
            let start_f64 = ev["start_time"].as_f64().unwrap_or(0.0);
            let end_f64 = ev["end_time"].as_f64().unwrap_or(start_f64 + 15.0);
            let has_recs =
                has_valid_recording_segments_for_window(state, cam_str, start_f64, end_f64);
            let cam = Some(cam_str.to_string());
            let label = ev["label"].as_str().unwrap_or("event");
            let start = ev["start_time"].as_f64().map(|t| t as u64);
            let title = format!("{cam_str} // {label}");
            return (cam, title, start, has_recs);
        }

        if let Ok(Some(rev)) = state.db.get_review_by_id(target_id) {
            let cam_name = rev.camera.trim().to_string();
            if cam_name.is_empty() {
                return (None, target_id.to_string(), None, false);
            }
            let end_f64 = rev.end_time.unwrap_or(rev.start_time + 15.0);
            let has_recs =
                has_valid_recording_segments_for_window(state, &cam_name, rev.start_time, end_f64);
            let title = format!("{cam_name} // {}", rev.severity.to_ascii_uppercase());
            return (Some(cam_name), title, Some(rev.start_time as u64), has_recs);
        }
    }

    (None, target_id.to_string(), None, false)
}

pub async fn api_share_list(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let mut links = state.db.list_share_links().unwrap_or_default();
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        links.retain(|item| {
            item["camera"]
                .as_str()
                .map(str::trim)
                .is_some_and(|c| allowed.iter().any(|a| a == c))
        });
    }
    Json(Value::Array(links))
}

pub fn has_active_or_in_flight_share_for_target(
    state: &AppState,
    target_id: &str,
    now_ts: u64,
) -> bool {
    if state
        .db
        .has_active_share_links_for_target(target_id, now_ts as f64)
        .unwrap_or(false)
    {
        return true;
    }
    let credited_jtis: Vec<String> = state
        .share_engine
        .page_video_credits
        .lock()
        .ok()
        .map(|mut map| {
            map.retain(|_, entry| {
                entry.prune_expired_sessions(now_ts);
                entry.page_credits > 0
                    || entry.has_any_active_stream()
                    || entry.has_active_session(now_ts)
            });
            map.keys().cloned().collect()
        })
        .unwrap_or_default();
    for jti in credited_jtis {
        if let Ok(Some(link)) = state.db.get_share_link(&jti)
            && !link["revoked"].as_bool().unwrap_or(false)
            && link["expires_at"]
                .as_f64()
                .is_some_and(|exp| exp > now_ts as f64)
            && link["target_id"].as_str() == Some(target_id)
        {
            return true;
        }
    }
    false
}

pub async fn api_share_revoke(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if let Some(err_resp) = verify_caller_can_mutate(&state, &jar, &headers).await {
        return err_resp;
    }
    let Some(link) = state.db.get_share_link(&id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Share link not found" })),
        )
            .into_response();
    };
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        let cam_ok = link["camera"]
            .as_str()
            .map(str::trim)
            .is_some_and(|c| allowed.iter().any(|a| a == c));
        if !cam_ok {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "success": false,
                    "message": "Camera is not permitted for this user"
                })),
            )
                .into_response();
        }
    }
    if let Err(err) = state.db.revoke_share_link(&id) {
        warn!("Failed to revoke share link '{id}' in database: {err}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "message": "Failed to revoke share link"
            })),
        )
            .into_response();
    }
    if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
        map.remove(&id);
    }
    if let Ok(mut auto_map) = state.share_engine.creator_auto_revoked.lock() {
        auto_map.remove(&id);
    }
    if let Some(target_id) = link["target_id"].as_str()
        && !has_active_or_in_flight_share_for_target(&state, target_id, unix_now())
    {
        cleanup_share_clip_artifacts(&state.media_dir, target_id);
    }
    Json(json!({ "success": true, "id": id })).into_response()
}

pub fn prune_share_creator_auto_revoked(
    state: &AppState,
    auto_map: &mut HashMap<String, &'static str>,
    now_ts: u64,
) {
    let now_f64 = now_ts as f64;
    auto_map.retain(|k, _| {
        state.db.get_share_link(k).ok().flatten().is_some_and(|l| {
            l["expires_at"]
                .as_f64()
                .is_some_and(|exp| exp > now_f64 && (exp as u64) > now_ts)
        })
    });
}

pub fn remove_creator_from_auto_revoked(
    state: &AppState,
    auto_map: &mut HashMap<String, &'static str>,
    creator_username: Option<&str>,
    creator_token_name: Option<&str>,
) {
    let target_user = creator_username.map(str::trim).filter(|s| !s.is_empty());
    let target_token = creator_token_name.map(str::trim).filter(|s| !s.is_empty());
    auto_map.retain(|k, _| {
        let Some(link) = state.db.get_share_link(k).ok().flatten() else {
            return false;
        };
        let Some(cb) = link["created_by"].as_str().map(str::trim) else {
            return true;
        };
        if let Some(u) = target_user {
            let link_u = cb.strip_prefix("ext:").unwrap_or(cb).trim();
            if !cb.starts_with("token:") && link_u.eq_ignore_ascii_case(u) {
                return false;
            }
        }
        if let Some(t) = target_token
            && let Some(link_t) = cb.strip_prefix("token:")
            && link_t.trim() == t
        {
            return false;
        }
        true
    });
}

pub fn purge_revoked_share_credits(state: &AppState) {
    let now_u64 = unix_now();
    let now_ts = now_u64 as f64;
    if let Ok(mut auto_map) = state.share_engine.creator_auto_revoked.lock()
        && !auto_map.is_empty()
    {
        prune_share_creator_auto_revoked(state, &mut auto_map, now_u64);
    }
    let keys: Vec<String> = state
        .share_engine
        .page_video_credits
        .lock()
        .ok()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    if !keys.is_empty() {
        let inactive_keys: Vec<String> = keys
            .into_iter()
            .filter(|jti| !state.db.is_share_link_active(jti, now_ts).unwrap_or(false))
            .collect();
        if !inactive_keys.is_empty()
            && let Ok(mut map) = state.share_engine.page_video_credits.lock()
        {
            for jti in inactive_keys {
                map.remove(&jti);
            }
        }
    }
    if let Ok(targets) = state.db.list_expired_or_revoked_share_link_targets(now_ts) {
        for target_id in targets {
            if !has_active_or_in_flight_share_for_target(state, &target_id, now_u64) {
                cleanup_share_clip_artifacts(&state.media_dir, &target_id);
            }
        }
    }
}

pub async fn api_share_create(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<CreateShareBody>,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if let Some(err_resp) = verify_caller_can_mutate(&state, &jar, &headers).await {
        return err_resp;
    }
    let kind = if body.kind.eq_ignore_ascii_case("review") {
        "review".to_string()
    } else if body.kind.eq_ignore_ascii_case("event") {
        "event".to_string()
    } else {
        "export".to_string()
    };
    let (target_cam, _, _, exists) = resolve_share_target_metadata(&state, &kind, &body.target_id);
    if let Some(allowed) = caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        match target_cam {
            Some(ref cam) if allowed.iter().any(|a| a == cam.trim()) => {}
            _ => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "success": false,
                        "message": "Camera is not permitted for this user"
                    })),
                )
                    .into_response();
            }
        }
    }
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "success": false,
                "message": "Share target not found"
            })),
        )
            .into_response();
    }

    let now = unix_now();
    let exp = now + body.expires_in_sec.clamp(1, 86400 * 30);
    let clamped_max_views = body.max_views.map(|v| v.max(1));
    let jti = format!("shr_{}_{:032x}", now, rand::random::<u128>());
    let created_by = {
        let cfg = state.config.read().await;
        let auth_enabled = cfg.auth.enabled;
        extract_authenticated_user_with_cfg(&state, &jar, &headers, &cfg.auth)
            .map(|(u, _)| {
                if u.starts_with("token:") || u.starts_with("ext:") {
                    u
                } else if let Ok(Some(db_u)) = state.db.get_user(&u) {
                    db_u.username
                } else if !auth_enabled && u.eq_ignore_ascii_case("admin") {
                    "admin".to_string()
                } else {
                    format!("ext:{}", u.to_ascii_lowercase())
                }
            })
            .unwrap_or_else(|| "admin".to_string())
    };
    if let Err(err) = state.db.insert_share_link_with_options(
        &jti,
        &kind,
        &body.target_id,
        target_cam.as_deref(),
        &created_by,
        now as f64,
        exp as f64,
        clamped_max_views,
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "error": "Failed to persist share link",
                "message": format!("Failed to persist share link: {err}")
            })),
        )
            .into_response();
    }
    let payload = ShareTokenPayload {
        k: kind,
        id: body.target_id.clone(),
        exp,
        cam: target_cam,
        jti: Some(jti.clone()),
    };
    let token = sign_share_token(&state.jwt_secret, &payload);
    let mut resp_json = json!({
        "success": true,
        "id": jti,
        "token": token,
        "url": format!("/share/{token}"),
        "video_url": format!("/share/{token}/video.mp4"),
        "expires_at": exp
    });
    if let Some(mv) = clamped_max_views {
        resp_json["max_views"] = json!(mv);
    }
    Json(resp_json).into_response()
}

pub fn parse_video_range_bounds(headers: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let range_val = headers.get(header::RANGE).and_then(|v| v.to_str().ok())?;
    let spec = range_val.trim().strip_prefix("bytes=")?;
    let (start_s, end_s) = spec.split_once('-')?;
    let start = start_s.trim().parse::<u64>().ok()?;
    let end_trimmed = end_s.trim();
    if end_trimmed.is_empty() {
        Some((start, None))
    } else {
        let end = end_trimmed.parse::<u64>().ok()?;
        if start <= end {
            Some((start, Some(end)))
        } else {
            None
        }
    }
}

pub fn is_initial_or_unranged_video_request(headers: &HeaderMap) -> bool {
    if !headers.contains_key(header::RANGE) {
        return true;
    }
    matches!(parse_video_range_bounds(headers), Some((0, _)))
}

pub fn verify_share_jti(
    state: &AppState,
    payload: &ShareTokenPayload,
    now_ts: u64,
) -> std::result::Result<(), (StatusCode, &'static str)> {
    verify_share_jti_with_view_mode(state, payload, now_ts, true, true, None, false).map(|_| ())
}

pub fn verify_share_jti_with_view_mode(
    state: &AppState,
    payload: &ShareTokenPayload,
    now_ts: u64,
    is_initial_request: bool,
    is_player_page: bool,
    provided_sid: Option<&str>,
    valid_continuation: bool,
) -> std::result::Result<bool, (StatusCode, &'static str)> {
    let Some(jti) = payload
        .jti
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err((
            StatusCode::FORBIDDEN,
            "Share link token is missing required JTI",
        ));
    };
    let link = match state.db.get_share_link(jti) {
        Ok(Some(link)) => link,
        Ok(None) => {
            return Err((StatusCode::GONE, "Share link has been revoked"));
        }
        Err(_) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to verify share link state",
            ));
        }
    };
    if link["revoked"].as_bool().unwrap_or(false) {
        if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
            map.remove(jti);
        }
        let had_stale_event_artifact = !is_player_page
            && (payload.k == "event" || payload.k == "review")
            && (state
                .media_dir
                .join("exports")
                .join(format!("share_clip_{}.mp4", payload.id))
                .is_file()
                || state
                    .media_dir
                    .join("exports")
                    .join(format!("share_redacted_{}.mp4", payload.id))
                    .is_file());
        if !has_active_or_in_flight_share_for_target(state, &payload.id, now_ts) {
            cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
        }
        if had_stale_event_artifact {
            let (resolved_cam, _, _, has_recs) =
                resolve_share_target_metadata(state, &payload.k, &payload.id);
            if resolved_cam.is_some() && !has_recs {
                return Err((StatusCode::NOT_FOUND, "Shared clip not found"));
            }
        }
        if let Some(auto_reason) = state
            .share_engine
            .creator_auto_revoked
            .lock()
            .ok()
            .and_then(|m| m.get(jti).copied())
        {
            return Err((StatusCode::FORBIDDEN, auto_reason));
        }
        if let Some(created_by) = link["created_by"].as_str()
            && !created_by.is_empty()
        {
            if let Some(token_name) = created_by.strip_prefix("token:") {
                let token_exists = state.db.list_api_tokens().ok().is_some_and(|tokens| {
                    tokens
                        .iter()
                        .any(|item| item["name"].as_str() == Some(token_name.trim()))
                });
                if token_exists
                    && !state
                        .db
                        .is_api_token_active_by_name(token_name, now_ts as f64)
                        .unwrap_or(false)
                {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "Share link creator API token revoked or expired",
                    ));
                }
            } else {
                let lookup_user = created_by.strip_prefix("ext:").unwrap_or(created_by).trim();
                if let Ok(Some(db_u)) = state.db.get_user(lookup_user)
                    && !db_u.role.trim().eq_ignore_ascii_case("viewer")
                    && !db_u.role.trim().eq_ignore_ascii_case("admin")
                {
                    let raw_cams = db_u.allowed_cameras.trim();
                    if !raw_cams.is_empty() && raw_cams != "*" {
                        let allowed: Vec<&str> = raw_cams
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .collect();
                        let cam_ok = allowed.contains(&"*")
                            || link["camera"]
                                .as_str()
                                .map(str::trim)
                                .is_some_and(|c| allowed.contains(&c));
                        if !cam_ok {
                            return Err((
                                StatusCode::FORBIDDEN,
                                "Share link creator camera access revoked",
                            ));
                        }
                    }
                }
            }
        }
        return Err((StatusCode::GONE, "Share link has been revoked"));
    }
    if !link["expires_at"]
        .as_f64()
        .is_some_and(|exp| exp.is_finite() && exp > 0.0 && (exp as u64) > now_ts)
    {
        if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
            map.remove(jti);
        }
        if !has_active_or_in_flight_share_for_target(state, &payload.id, now_ts) {
            cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
        }
        return Err((StatusCode::GONE, "Share link has expired"));
    }
    let access_count = link["access_count"].as_i64().unwrap_or(0);
    let credit_state = state
        .share_engine
        .page_video_credits
        .lock()
        .ok()
        .and_then(|mut map| {
            if let Some(entry) = map.get_mut(jti) {
                entry.prune_expired_sessions(now_ts);
                let cloned = entry.clone();
                if entry.page_credits == 0
                    && !entry.has_any_active_stream()
                    && !entry.has_active_session(now_ts)
                {
                    map.remove(jti);
                }
                Some(cloned)
            } else {
                None
            }
        })
        .unwrap_or_default();
    let has_valid_sid = !is_player_page
        && access_count > 0
        && provided_sid.is_some_and(|sid| credit_state.is_valid_sid(sid, now_ts));
    let has_page_credit = !is_player_page
        && provided_sid.is_none()
        && is_initial_request
        && !valid_continuation
        && access_count > 0
        && credit_state.page_credits > 0;
    let has_active_stream = !is_player_page
        && provided_sid.is_none()
        && valid_continuation
        && ((access_count > 0 && credit_state.has_any_active_stream()) || !is_initial_request);
    let count_view = is_player_page || (!has_valid_sid && !has_page_credit && !has_active_stream);
    if let Some(max_v) = link["max_views"].as_i64()
        && ((count_view && access_count >= max_v) || (!count_view && access_count > max_v))
    {
        if let Ok(mut map) = state.share_engine.page_video_credits.lock()
            && let Some(entry) = map.get_mut(jti)
        {
            entry.prune_expired_sessions(now_ts);
            if entry.page_credits == 0
                && !entry.has_any_active_stream()
                && !entry.has_active_session(now_ts)
            {
                map.remove(jti);
            }
        }
        if !has_active_or_in_flight_share_for_target(state, &payload.id, now_ts) {
            cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
        }
        return Err((StatusCode::GONE, "Share link view limit reached"));
    }
    if link["kind"].as_str() != Some(payload.k.as_str())
        || link["target_id"].as_str() != Some(payload.id.as_str())
        || link["camera"].as_str().map(str::trim) != payload.cam.as_deref().map(str::trim)
    {
        return Err((StatusCode::FORBIDDEN, "Share link payload mismatch"));
    }
    if let Some(created_by) = link["created_by"].as_str()
        && !created_by.is_empty()
    {
        let auth_enabled = state
            .config
            .try_read()
            .map(|cfg| cfg.auth.enabled)
            .unwrap_or(true);
        let revoke_invalid_creator_jti = |reason: &'static str, force_db_revoke: bool| {
            let mut db_revoke_failed = false;
            if (force_db_revoke || created_by != "admin")
                && let Err(err) = state.db.revoke_share_link(jti)
            {
                warn!("Failed to revoke share link '{jti}' after creator permission loss: {err:#}");
                db_revoke_failed = true;
            }
            if let Ok(mut auto_map) = state.share_engine.creator_auto_revoked.lock() {
                if !auto_map.contains_key(jti) && auto_map.len() >= 512 {
                    prune_share_creator_auto_revoked(state, &mut auto_map, now_ts);
                    while auto_map.len() >= 512 {
                        if let Some(evict_key) = auto_map.keys().next().cloned() {
                            auto_map.remove(&evict_key);
                        } else {
                            break;
                        }
                    }
                }
                auto_map.insert(jti.to_string(), reason);
            }
            if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
                map.remove(jti);
            }
            let other_active_link = db_revoke_failed
                && state
                    .db
                    .list_share_links()
                    .unwrap_or_default()
                    .iter()
                    .any(|item| {
                        item["id"].as_str() != Some(jti)
                            && !item["revoked"].as_bool().unwrap_or(false)
                            && item["expires_at"]
                                .as_f64()
                                .is_some_and(|exp| exp > now_ts as f64)
                            && item["target_id"].as_str() == Some(payload.id.as_str())
                    });
            if (!db_revoke_failed
                && !has_active_or_in_flight_share_for_target(state, &payload.id, now_ts))
                || (db_revoke_failed && !other_active_link)
            {
                cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
            }
        };
        if let Some(token_name) = created_by.strip_prefix("token:") {
            let token_valid = state
                .db
                .is_api_token_active_by_name(token_name, now_ts as f64)
                .unwrap_or(false);
            if !token_valid {
                let reason = "Share link creator API token revoked or expired";
                revoke_invalid_creator_jti(reason, false);
                return Err((StatusCode::FORBIDDEN, reason));
            }
        } else if let Some(ext_user) = created_by.strip_prefix("ext:") {
            let trimmed_ext = ext_user.trim();
            if trimmed_ext.is_empty() {
                let reason = "Share link creator user no longer exists";
                revoke_invalid_creator_jti(reason, true);
                return Err((StatusCode::FORBIDDEN, reason));
            }
            if let Ok(Some(db_u)) = state.db.get_user(trimmed_ext) {
                let role_trimmed = db_u.role.trim();
                if role_trimmed.eq_ignore_ascii_case("viewer")
                    || role_trimmed.eq_ignore_ascii_case("read_only")
                    || role_trimmed.eq_ignore_ascii_case("metrics")
                {
                    let reason = "Share link creator role is not permitted to share";
                    revoke_invalid_creator_jti(reason, false);
                    return Err((StatusCode::FORBIDDEN, reason));
                }
                if !role_trimmed.eq_ignore_ascii_case("admin") {
                    let raw_cams = db_u.allowed_cameras.trim();
                    if !raw_cams.is_empty() && raw_cams != "*" {
                        let allowed: Vec<&str> = raw_cams
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .collect();
                        let cam_ok = allowed.contains(&"*")
                            || link["camera"]
                                .as_str()
                                .map(str::trim)
                                .is_some_and(|c| allowed.contains(&c));
                        if !cam_ok {
                            let reason = "Share link creator camera access revoked";
                            revoke_invalid_creator_jti(reason, false);
                            return Err((StatusCode::FORBIDDEN, reason));
                        }
                    }
                }
            }
        } else {
            match state.db.get_user(created_by) {
                Ok(Some(db_u)) => {
                    let role_trimmed = db_u.role.trim();
                    if role_trimmed.eq_ignore_ascii_case("viewer")
                        || role_trimmed.eq_ignore_ascii_case("read_only")
                        || role_trimmed.eq_ignore_ascii_case("metrics")
                    {
                        let reason = "Share link creator role is not permitted to share";
                        revoke_invalid_creator_jti(reason, false);
                        return Err((StatusCode::FORBIDDEN, reason));
                    }
                    if !role_trimmed.eq_ignore_ascii_case("admin") {
                        let raw_cams = db_u.allowed_cameras.trim();
                        if !raw_cams.is_empty() && raw_cams != "*" {
                            let allowed: Vec<&str> = raw_cams
                                .split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .collect();
                            let cam_ok = allowed.contains(&"*")
                                || link["camera"]
                                    .as_str()
                                    .map(str::trim)
                                    .is_some_and(|c| allowed.contains(&c));
                            if !cam_ok {
                                let reason = "Share link creator camera access revoked";
                                revoke_invalid_creator_jti(reason, false);
                                return Err((StatusCode::FORBIDDEN, reason));
                            }
                        }
                    }
                }
                Ok(None) if !auth_enabled && created_by.eq_ignore_ascii_case("admin") => {}
                Ok(None) | Err(_) => {
                    let reason = "Share link creator user no longer exists";
                    revoke_invalid_creator_jti(reason, true);
                    return Err((StatusCode::FORBIDDEN, reason));
                }
            }
        }
    }
    Ok(count_view)
}

pub fn next_stream_state_from_video_response(
    resp: &Response,
    request_range: Option<(u64, Option<u64>)>,
) -> (bool, u64) {
    if resp.status() == StatusCode::PARTIAL_CONTENT
        && let Some(cr) = resp
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
        && let Some(spec) = cr.trim().strip_prefix("bytes ")
        && let Some((range_part, total_s)) = spec.split_once('/')
        && let Some((start_s, end_s)) = range_part.split_once('-')
        && let (Ok(start), Ok(end), Ok(total)) = (
            start_s.trim().parse::<u64>(),
            end_s.trim().parse::<u64>(),
            total_s.trim().parse::<u64>(),
        )
    {
        if matches!(request_range, Some((0, Some(1)))) && total > 2 {
            return (true, 0);
        }
        if start == 0 && end.saturating_add(1) < total {
            return (true, end.saturating_add(1).max(2));
        }
        if start == 0 && total > 2 {
            return (true, 2);
        }
        if start > 0 && end.saturating_add(1) < total {
            return (true, end.saturating_add(1));
        }
        if let Some((req_start, None)) = request_range
            && req_start > 0
            && req_start.saturating_add(1) < total
        {
            return (true, req_start.saturating_add(1));
        }
    }
    (false, u64::MAX)
}

pub fn prune_share_page_video_credits(map: &mut HashMap<String, SharePlaybackCredit>, now_ts: u64) {
    if map.len() < 512 {
        return;
    }
    for v in map.values_mut() {
        v.prune_expired_sessions(now_ts);
    }
    map.retain(|_, v| {
        v.page_credits > 0 || v.has_any_active_stream() || v.has_active_session(now_ts)
    });
    if map.len() >= 512 {
        map.retain(|_, v| v.page_credits > 0 || v.has_active_session(now_ts));
    }
    if map.len() >= 512 {
        map.retain(|_, v| v.page_credits > 0);
    }
    while map.len() >= 512 {
        if let Some(victim_key) = map
            .iter()
            .find(|(_, v)| !v.has_active_session(now_ts) || v.page_credits == 0)
            .map(|(k, _)| k.clone())
            .or_else(|| map.keys().next().cloned())
        {
            map.remove(&victim_key);
        } else {
            break;
        }
    }
}

pub fn rollback_share_stream_cursor(
    state: &AppState,
    payload: &ShareTokenPayload,
    prev_stream_cursor: Option<(usize, u64, u64)>,
) {
    if let Some((idx, prev_cursor, prev_exp)) = prev_stream_cursor
        && let Some(jti) = payload
            .jti
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        && let Ok(mut map) = state.share_engine.page_video_credits.lock()
        && let Some(entry) = map.get_mut(jti)
        && idx < entry.active_streams.len()
    {
        entry.active_streams[idx] = prev_cursor;
        if idx < entry.active_stream_expires.len() {
            entry.active_stream_expires[idx] = prev_exp;
        }
        entry.sync_stream_summary();
    }
}

pub struct ShareStreamCursorGuard {
    state: AppState,
    payload: ShareTokenPayload,
    cursor: Option<(usize, u64, u64)>,
}

impl ShareStreamCursorGuard {
    pub fn new(
        state: AppState,
        payload: ShareTokenPayload,
        cursor: Option<(usize, u64, u64)>,
    ) -> Self {
        Self {
            state,
            payload,
            cursor,
        }
    }

    pub fn disarm(&mut self) {
        self.cursor = None;
    }
}

impl Drop for ShareStreamCursorGuard {
    fn drop(&mut self) {
        if let Some(c) = self.cursor.take() {
            rollback_share_stream_cursor(&self.state, &self.payload, Some(c));
        }
    }
}

pub(crate) fn build_share_video_head_placeholder_response(headers: &HeaderMap) -> Response {
    let status = if parse_video_range_bounds(headers).is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    (
        status,
        [
            (header::CONTENT_TYPE, "video/mp4"),
            (header::ACCEPT_RANGES, "bytes"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CACHE_CONTROL, "private, no-store"),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
        ],
        Body::empty(),
    )
        .into_response()
}

#[allow(clippy::too_many_arguments)]
pub fn record_share_access_or_gone(
    state: &AppState,
    payload: &ShareTokenPayload,
    count_view: bool,
    is_player_page: bool,
    request_range: Option<(u64, Option<u64>)>,
    now_ts: u64,
    new_session_id: Option<String>,
    has_valid_sid: bool,
    valid_continuation: bool,
    prev_stream_cursor: Option<(usize, u64, u64)>,
    resp: Response,
) -> Response {
    if !resp.status().is_success() {
        rollback_share_stream_cursor(state, payload, prev_stream_cursor);
        return resp;
    }
    let is_initial_video = !is_player_page && matches!(request_range, None | Some((0, _)));
    let Some(jti) = payload
        .jti
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return resp;
    };
    if !state
        .db
        .is_share_link_active(jti, now_ts as f64)
        .unwrap_or(false)
    {
        if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
            map.remove(jti);
        }
        return (
            StatusCode::GONE,
            Json(json!({ "error": "Share link has been revoked" })),
        )
            .into_response();
    }
    if !is_player_page && has_valid_sid {
        if let Ok(mut map) = state.share_engine.page_video_credits.lock()
            && let Some(entry) = map.get_mut(jti)
            && let Some(sid_str) = new_session_id.as_deref()
            && let Some((exp, credit_claimed)) = entry.sessions.get_mut(sid_str)
            && *exp > now_ts
        {
            *exp = now_ts + 900;
            if !*credit_claimed {
                *credit_claimed = true;
                entry.page_credits = entry.page_credits.saturating_sub(1);
            }
        }
        let _ = state.db.touch_share_link_last_accessed(jti, now_ts as f64);
        return resp;
    }
    let (stream_active, min_next_start) = if is_player_page {
        (false, u64::MAX)
    } else {
        next_stream_state_from_video_response(&resp, request_range)
    };
    if !is_player_page && is_initial_video && !valid_continuation && !count_view {
        let consumed_credit = state
            .share_engine
            .page_video_credits
            .lock()
            .ok()
            .and_then(|mut map| {
                if let Some(entry) = map.get_mut(jti)
                    && entry.page_credits > 0
                {
                    entry.page_credits -= 1;
                    if let Some(unclaimed_sid) = entry
                        .sessions
                        .iter()
                        .filter(|(_, (_, claimed))| !*claimed)
                        .min_by_key(|(_, (exp, _))| *exp)
                        .map(|(k, _)| k.clone())
                    {
                        entry.sessions.remove(&unclaimed_sid);
                    }
                    if stream_active {
                        entry.record_initial_stream_at(min_next_start, now_ts + 900);
                    }
                    entry.prune_expired_sessions(now_ts);
                    if entry.page_credits == 0
                        && !entry.has_any_active_stream()
                        && !entry.has_active_session(now_ts)
                    {
                        map.remove(jti);
                    }
                    Some(true)
                } else {
                    None
                }
            })
            .unwrap_or(false);
        if consumed_credit {
            let _ = state.db.touch_share_link_last_accessed(jti, now_ts as f64);
            return resp;
        }
    }
    if count_view || (!is_player_page && is_initial_video && !valid_continuation) {
        match state.db.record_share_link_access(jti, now_ts as f64) {
            Ok(true) => {
                if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
                    if is_player_page {
                        if !map.contains_key(jti) && map.len() >= 512 {
                            prune_share_page_video_credits(&mut map, now_ts);
                        }
                        let entry = map.entry(jti.to_string()).or_default();
                        entry.page_credits += 1;
                        if let Some(sid) = new_session_id {
                            entry.insert_session(sid, now_ts + 900, now_ts);
                        }
                    } else if is_initial_video && stream_active {
                        if !map.contains_key(jti) && map.len() >= 512 {
                            prune_share_page_video_credits(&mut map, now_ts);
                        }
                        let entry = map.entry(jti.to_string()).or_default();
                        entry.record_initial_stream_at(min_next_start, now_ts + 900);
                    } else if let Some(entry) = map.get_mut(jti) {
                        entry.prune_expired_sessions(now_ts);
                        if entry.page_credits == 0
                            && !entry.has_any_active_stream()
                            && !entry.has_active_session(now_ts)
                        {
                            map.remove(jti);
                        }
                    }
                }
                resp
            }
            Ok(false) => {
                if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
                    map.remove(jti);
                }
                if !has_active_or_in_flight_share_for_target(state, &payload.id, now_ts) {
                    cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
                }
                (
                    StatusCode::GONE,
                    Json(json!({ "error": "Share link view limit reached or revoked" })),
                )
                    .into_response()
            }
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to record share link access" })),
            )
                .into_response(),
        }
    } else {
        if let Ok(mut map) = state.share_engine.page_video_credits.lock()
            && let Some(entry) = map.get_mut(jti)
        {
            let tentative_next = request_range.map(|(start, end_opt)| match end_opt {
                Some(end) => end.saturating_add(1).max(2),
                None => start.saturating_add(1).max(2),
            });
            let matched_slot = tentative_next
                .and_then(|t_next| {
                    prev_stream_cursor
                        .and_then(|(idx, _, _)| {
                            (idx < entry.active_streams.len()
                                && entry.active_streams[idx] == t_next)
                                .then_some(idx)
                        })
                        .or_else(|| entry.active_streams.iter().position(|&c| c == t_next))
                })
                .or_else(|| {
                    prev_stream_cursor.and_then(|(idx, prev_cursor, _)| {
                        (idx < entry.active_streams.len()
                            && entry.active_streams[idx] == prev_cursor)
                            .then_some(idx)
                    })
                });
            if stream_active {
                if let Some(pos) = matched_slot {
                    entry.active_streams[pos] = min_next_start;
                    while entry.active_stream_expires.len() <= pos {
                        entry.active_stream_expires.push(now_ts + 900);
                    }
                    entry.active_stream_expires[pos] = now_ts + 900;
                } else if entry.active_streams.is_empty() {
                    entry.record_initial_stream_at(min_next_start, now_ts + 900);
                }
            } else if let Some(pos) = matched_slot {
                entry.active_streams.remove(pos);
                if pos < entry.active_stream_expires.len() {
                    entry.active_stream_expires.remove(pos);
                }
            }
            entry.sync_stream_summary();
            entry.prune_expired_sessions(now_ts);
            if entry.page_credits == 0
                && !entry.has_any_active_stream()
                && !entry.has_active_session(now_ts)
            {
                map.remove(jti);
            }
        }
        let _ = state.db.touch_share_link_last_accessed(jti, now_ts as f64);
        resp
    }
}

fn is_link_preview_bot_user_agent(headers: &HeaderMap) -> bool {
    let Some(ua) = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let ua_lower = ua.to_ascii_lowercase();
    [
        "whatsapp",
        "slackbot",
        "telegrambot",
        "discordbot",
        "twitterbot",
        "facebookexternalhit",
        "linkedinbot",
        "applebot",
    ]
    .iter()
    .any(|bot| ua_lower.contains(bot))
}

pub async fn share_player_page(
    State(state): State<AppState>,
    method: axum::http::Method,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> Response {
    let now_ts = unix_now();
    let payload = match verify_share_token(&state.jwt_secret, &token, now_ts) {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(json!({ "error": "Share link invalid or expired" })),
            )
                .into_response();
        }
    };
    if let Err((status, msg)) = verify_share_jti(&state, &payload, now_ts) {
        return (status, Json(json!({ "error": msg }))).into_response();
    }
    if payload.k != "export"
        && let (Ok(Some(rev)), Ok(Some(ev))) = (
            state.db.get_review_by_id(&payload.id),
            state.db.get_event(&payload.id),
        )
        && ev["camera"]
            .as_str()
            .is_some_and(|ec| ec.trim() != rev.camera.trim())
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Share link camera mismatch" })),
        )
            .into_response();
    }
    let (resolved_cam, clip_title, clip_ts, exists) =
        resolve_share_target_metadata(&state, &payload.k, &payload.id);
    if let Some(ref token_cam) = payload.cam
        && let Some(ref actual_cam) = resolved_cam
        && token_cam.trim() != actual_cam.trim()
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Share link camera mismatch" })),
        )
            .into_response();
    }
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Shared clip not found" })),
        )
            .into_response();
    }
    if let Some(ref token_cam) = payload.cam
        && resolved_cam.as_deref().map(str::trim) != Some(token_cam.trim())
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "Share link camera mismatch" })),
        )
            .into_response();
    }
    if method == axum::http::Method::HEAD {
        return (
            [
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'",
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::REFERRER_POLICY, "no-referrer"),
                (header::X_FRAME_OPTIONS, "DENY"),
                (header::CACHE_CONTROL, "private, no-store"),
                (
                    header::HeaderName::from_static("cross-origin-resource-policy"),
                    "same-origin",
                ),
            ],
            Html(String::new()),
        )
            .into_response();
    }
    let safe_title = xml_escape_simple(&clip_title);
    if is_link_preview_bot_user_agent(&headers) {
        let preview_html = format!(
            r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><meta name="robots" content="noindex, nofollow"><title>NinjaNVR Shared Clip // {safe_title}</title></head>
<body></body>
</html>"#
        );
        return (
            [
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'",
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (header::REFERRER_POLICY, "no-referrer"),
                (header::X_FRAME_OPTIONS, "DENY"),
                (header::CACHE_CONTROL, "private, no-store"),
                (
                    header::HeaderName::from_static("cross-origin-resource-policy"),
                    "same-origin",
                ),
            ],
            Html(preview_html),
        )
            .into_response();
    }
    let safe_id = xml_escape_simple(&payload.id);
    let safe_token = xml_escape_simple(&token);
    let sid = format!("{:032x}", rand::random::<u128>());
    let ts_label = clip_ts
        .and_then(|ts| chrono::DateTime::from_timestamp(ts as i64, 0))
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "Timestamp recorded in clip".to_string());
    let exp_label = chrono::DateTime::from_timestamp(payload.exp as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| payload.exp.to_string());
    let html = format!(
        r#"<!doctype html>
<html lang="en" style="color-scheme:dark;background:#070a0f;color:#e2e8f0;font-family:system-ui,sans-serif">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="robots" content="noindex, nofollow"><title>NinjaNVR Shared Clip // {safe_title}</title></head>
<body style="margin:0;padding:24px;display:flex;flex-direction:column;align-items:center;gap:14px">
  <div style="width:100%;max-width:960px;background:#0d121b;border:1px solid #1e293b;border-radius:4px;padding:16px">
    <div style="display:flex;justify-content:space-between;align-items:center;gap:12px;flex-wrap:wrap;margin-bottom:12px;font-family:monospace;font-size:12px">
      <div>
        <div><strong>NINJANVR SHARED CLIP // {safe_title}</strong></div>
        <div style="color:#94a3b8;font-size:11px;margin-top:2px">CLIP TIME: {ts_label} · EXPIRES: {exp_label}</div>
      </div>
      <a href="/share/{safe_token}/video.mp4?sid={sid}" download="{safe_id}.mp4" style="color:#38bdf8;text-decoration:none;border:1px solid #0284c7;padding:4px 10px;border-radius:3px">Download MP4</a>
    </div>
    <video src="/share/{safe_token}/video.mp4?sid={sid}" controls autoplay playsinline style="width:100%;max-height:72vh;object-fit:contain;background:#000"></video>
  </div>
</body>
</html>"#
    );
    let resp = (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CACHE_CONTROL, "private, no-store"),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
        ],
        Html(html),
    )
        .into_response();
    record_share_access_or_gone(
        &state,
        &payload,
        payload.jti.is_some(),
        true,
        None,
        now_ts,
        Some(sid),
        false,
        false,
        None,
        resp,
    )
}

pub fn serve_mp4_bytes_with_range(bytes: Vec<u8>, headers: &HeaderMap) -> Response {
    let total = bytes.len();
    if headers.contains_key(header::RANGE) {
        let Some((start_u64, end_opt)) = parse_video_range_bounds(headers) else {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            )
                .into_response();
        };
        let Ok(start) = usize::try_from(start_u64) else {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            )
                .into_response();
        };
        if total == 0 || start >= total {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            )
                .into_response();
        }
        let end = end_opt
            .and_then(|e| usize::try_from(e).ok())
            .map(|e| e.min(total - 1))
            .unwrap_or(total - 1);
        let slice = bytes[start..=end].to_vec();
        let content_range = format!("bytes {start}-{end}/{total}");
        return (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, "video/mp4".to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_RANGE, content_range),
                (header::CONTENT_LENGTH, slice.len().to_string()),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'".to_string(),
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                (header::REFERRER_POLICY, "no-referrer".to_string()),
                (header::X_FRAME_OPTIONS, "DENY".to_string()),
                (header::CACHE_CONTROL, "private, no-store".to_string()),
                (
                    header::HeaderName::from_static("cross-origin-resource-policy"),
                    "same-origin".to_string(),
                ),
            ],
            slice,
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "video/mp4".to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (header::CONTENT_LENGTH, total.to_string()),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'".to_string(),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
            (header::X_FRAME_OPTIONS, "DENY".to_string()),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin".to_string(),
            ),
        ],
        bytes,
    )
        .into_response()
}

pub async fn serve_mp4_file_with_range(disk_path: &Path, headers: &HeaderMap) -> Option<Response> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let path_owned = disk_path.to_path_buf();
    let (std_file, fmeta) =
        tokio::task::spawn_blocking(move || open_regular_non_symlink_readonly(&path_owned).ok())
            .await
            .ok()
            .flatten()?;
    if !fmeta.is_file() {
        return None;
    }
    let total = fmeta.len();
    let mut file = tokio::fs::File::from_std(std_file);
    if headers.contains_key(header::RANGE) {
        let Some((start, end_opt)) = parse_video_range_bounds(headers) else {
            return Some(
                (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{total}"))],
                )
                    .into_response(),
            );
        };
        if total == 0 || start >= total {
            return Some(
                (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [(header::CONTENT_RANGE, format!("bytes */{total}"))],
                )
                    .into_response(),
            );
        }
        let end = end_opt.map(|e| e.min(total - 1)).unwrap_or(total - 1);
        file.seek(std::io::SeekFrom::Start(start)).await.ok()?;
        let slice_len = end - start + 1;
        let reader = file.take(slice_len);
        let stream = futures_util::stream::unfold(reader, |mut r| async move {
            let mut buf = vec![0u8; 65536];
            match r.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), r))
                }
                Err(e) => Some((Err(e), r)),
            }
        });
        let content_range = format!("bytes {start}-{end}/{total}");
        return Some(
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "video/mp4".to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_RANGE, content_range),
                    (header::CONTENT_LENGTH, slice_len.to_string()),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'".to_string(),
                    ),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                    (header::REFERRER_POLICY, "no-referrer".to_string()),
                    (header::X_FRAME_OPTIONS, "DENY".to_string()),
                    (header::CACHE_CONTROL, "private, no-store".to_string()),
                    (
                        header::HeaderName::from_static("cross-origin-resource-policy"),
                        "same-origin".to_string(),
                    ),
                ],
                Body::from_stream(stream),
            )
                .into_response(),
        );
    }

    let reader = file.take(total);
    let stream = futures_util::stream::unfold(reader, |mut r| async move {
        let mut buf = vec![0u8; 65536];
        match r.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), r))
            }
            Err(e) => Some((Err(e), r)),
        }
    });
    Some(
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "video/mp4".to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_LENGTH, total.to_string()),
                (
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'".to_string(),
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                (header::REFERRER_POLICY, "no-referrer".to_string()),
                (header::X_FRAME_OPTIONS, "DENY".to_string()),
                (header::CACHE_CONTROL, "private, no-store".to_string()),
                (
                    header::HeaderName::from_static("cross-origin-resource-policy"),
                    "same-origin".to_string(),
                ),
            ],
            Body::from_stream(stream),
        )
            .into_response(),
    )
}

pub(crate) fn has_active_privacy_mask(mask: Option<&Value>) -> bool {
    match mask {
        Some(Value::Array(arr)) => !arr.is_empty(),
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(_)) => true,
        _ => false,
    }
}

pub(crate) fn ensure_path_within_canon_media_dir(
    media_dir_canon: &Path,
    candidate: &Path,
) -> Option<PathBuf> {
    if candidate.as_os_str().is_empty()
        || candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    let lmeta = std::fs::symlink_metadata(candidate).ok()?;
    if lmeta.file_type().is_symlink() {
        return None;
    }
    let candidate_canon = std::fs::canonicalize(candidate).ok()?;
    let cmeta = std::fs::symlink_metadata(&candidate_canon).ok()?;
    if cmeta.file_type().is_symlink() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if lmeta.dev() != cmeta.dev() || lmeta.ino() != cmeta.ino() {
            return None;
        }
    }
    let rel = candidate_canon.strip_prefix(media_dir_canon).ok()?;
    if rel.as_os_str().is_empty()
        || rel
            .components()
            .any(|c| c.as_os_str().to_str().is_none_or(|s| s.starts_with('.')))
    {
        return None;
    }
    Some(candidate_canon)
}

pub fn ensure_path_within_media_dir(media_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    let media_dir_canon = std::fs::canonicalize(media_dir).ok()?;
    ensure_path_within_canon_media_dir(&media_dir_canon, candidate)
}

pub fn compute_privacy_mask_hash(privacy_mask: Option<&Value>) -> String {
    let serialized = privacy_mask
        .and_then(|v| serde_json::to_vec(v).ok())
        .unwrap_or_default();
    let digest = Sha256::digest(&serialized);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) async fn acquire_share_clip_lock(
    state: &AppState,
    clip_key: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    state.share_engine.acquire_clip_lock(clip_key).await
}

pub(crate) fn is_regular_non_symlink_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file() && m.len() > 0)
}

pub(crate) fn ensure_safe_media_subdir(media_dir: &Path, subdir: &str) -> Option<PathBuf> {
    if subdir.is_empty() || subdir.contains("..") || subdir.contains(['/', '\\']) {
        return None;
    }
    if std::fs::symlink_metadata(media_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }
    let dir_path = media_dir.join(subdir);
    if !dir_path.exists() {
        std::fs::create_dir_all(&dir_path).ok()?;
    }
    let meta = std::fs::symlink_metadata(&dir_path).ok()?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return None;
    }
    let media_canon = std::fs::canonicalize(media_dir).ok()?;
    let dir_canon = std::fs::canonicalize(&dir_path).ok()?;
    if !dir_canon.starts_with(&media_canon) || dir_canon == media_canon {
        return None;
    }
    Some(dir_canon)
}

pub(crate) fn read_non_symlink_string(path: &Path) -> Option<String> {
    let bytes = read_bounded_non_symlink_bytes_sync(path, 4096)?;
    String::from_utf8(bytes).ok()
}

pub(crate) fn write_atomic_non_symlink_file(dir: &Path, filename: &str, contents: &[u8]) -> bool {
    use std::io::Write;
    if filename.is_empty() || filename.contains("..") || filename.contains(['/', '\\']) {
        return false;
    }
    let Ok(dir_meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if dir_meta.file_type().is_symlink() || !dir_meta.file_type().is_dir() {
        return false;
    }
    let target_path = dir.join(filename);
    if let Ok(target_meta) = std::fs::symlink_metadata(&target_path)
        && (target_meta.file_type().is_symlink() || target_meta.file_type().is_dir())
    {
        let _ = std::fs::remove_file(&target_path);
        if std::fs::symlink_metadata(&target_path).is_ok() {
            return false;
        }
    }
    let nonce: u64 = rand::random();
    let tmp_path = dir.join(format!(".{filename}.{nonce:016x}.tmp"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let Ok(mut tmp_file) = opts.open(&tmp_path) else {
        return false;
    };
    if tmp_file.write_all(contents).is_err() {
        drop(tmp_file);
        let _ = std::fs::remove_file(&tmp_path);
        return false;
    }
    drop(tmp_file);
    if let Ok(target_meta) = std::fs::symlink_metadata(&target_path)
        && (target_meta.file_type().is_symlink() || target_meta.file_type().is_dir())
    {
        let _ = std::fs::remove_file(&tmp_path);
        return false;
    }
    let ok = std::fs::rename(&tmp_path, &target_path).is_ok();
    let _ = std::fs::remove_file(&tmp_path);
    ok
}

pub(crate) fn remove_non_symlink_dir_child(dir: &Path, filename: &str) {
    if filename.is_empty() || filename.contains("..") || filename.contains(['/', '\\']) {
        return;
    }
    if std::fs::symlink_metadata(dir)
        .is_ok_and(|m| !m.file_type().is_symlink() && m.file_type().is_dir())
    {
        let p = dir.join(filename);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| !m.file_type().is_dir()) {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub(crate) fn is_cached_share_artifact_stale(
    artifact_path: &Path,
    source_paths: &[PathBuf],
) -> bool {
    let Ok(art_meta) = std::fs::symlink_metadata(artifact_path) else {
        return true;
    };
    if !art_meta.file_type().is_file() || art_meta.len() == 0 {
        return true;
    }
    let Ok(art_mtime) = art_meta.modified() else {
        return false;
    };
    source_paths.iter().any(|src| {
        std::fs::metadata(src)
            .and_then(|m| m.modified())
            .is_ok_and(|src_mtime| src_mtime > art_mtime)
    })
}

pub fn cleanup_share_clip_artifacts(media_dir: &Path, target_id: &str) {
    crate::media_lifecycle::cleanup_share_clip_artifacts_in_dir(media_dir, "", target_id);
    let safe_id: String = target_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect();
    if safe_id.is_empty() || safe_id != target_id || target_id.contains("..") {
        return;
    }
    if std::fs::symlink_metadata(media_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return;
    }
    let exports_dir = media_dir.join("exports");
    let clips_dir = media_dir.join("clips");
    remove_non_symlink_dir_child(&exports_dir, &format!("share_redacted_{safe_id}.mp4"));
    remove_non_symlink_dir_child(
        &exports_dir,
        &format!(".share_redacted_{safe_id}.mask_hash"),
    );
    remove_non_symlink_dir_child(&exports_dir, &format!("share_clip_{safe_id}.mp4"));
    remove_non_symlink_dir_child(&clips_dir, &format!("share_redacted_{safe_id}.webp"));
    remove_non_symlink_dir_child(&clips_dir, &format!("share_clip_{safe_id}.webp"));
}

pub fn cleanup_camera_share_clip_artifacts_in_range(
    state: &AppState,
    camera: &str,
    after: f64,
    before: f64,
) {
    if !after.is_finite() || !before.is_finite() || after < 0.0 || before < after {
        return;
    }
    let cam_trimmed = camera.trim();
    let query_after = (after - 15.0).max(0.0);
    let query_before = before + 15.0;
    let mut revoked_target_ids: BTreeSet<String> = BTreeSet::new();
    let mut revoke_target = |tid: &str| {
        revoked_target_ids.insert(tid.to_string());
        if let Err(err) = state.db.revoke_share_links_for_target(None, tid) {
            warn!(
                "Failed to revoke share links for target '{tid}' during recording deletion cleanup: {err:#}"
            );
        }
    };
    if let Ok(events) = state.db.query_events(&EventFilter {
        cameras: Some(vec![cam_trimmed.to_string()]),
        after: Some(query_after),
        before: Some(query_before),
        limit: Some(2000),
        ..Default::default()
    }) {
        for ev in events {
            if let Some(id) = ev["id"].as_str() {
                cleanup_share_clip_artifacts(&state.media_dir, id);
                let ev_start = ev["start_time"].as_f64().unwrap_or(0.0);
                let ev_end = ev["end_time"].as_f64().unwrap_or(ev_start + 15.0);
                if !has_valid_recording_segments_for_window(state, cam_trimmed, ev_start, ev_end) {
                    revoke_target(id);
                }
            }
        }
    }
    if let Ok(reviews) = state.db.query_reviews(&ReviewFilter {
        cameras: Some(vec![cam_trimmed.to_string()]),
        after: Some(query_after),
        before: Some(query_before),
        limit: Some(2000),
        ..Default::default()
    }) {
        for rev in reviews {
            cleanup_share_clip_artifacts(&state.media_dir, &rev.id);
            let rev_end = rev.end_time.unwrap_or(rev.start_time + 15.0);
            if !has_valid_recording_segments_for_window(state, cam_trimmed, rev.start_time, rev_end)
            {
                revoke_target(&rev.id);
            }
        }
    }
    let existing_links = state.db.list_share_links().unwrap_or_default();
    for link in &existing_links {
        if link["revoked"].as_bool().unwrap_or(false) {
            continue;
        }
        let kind = link["kind"].as_str().unwrap_or("");
        if kind != "event" && kind != "review" {
            continue;
        }
        let link_cam = link["camera"].as_str().map(str::trim).unwrap_or("");
        if !link_cam.is_empty() && link_cam != cam_trimmed {
            continue;
        }
        let Some(tid) = link["target_id"].as_str() else {
            continue;
        };
        let (resolved_cam, _, _, has_recs) = resolve_share_target_metadata(state, kind, tid);
        if resolved_cam.as_deref() == Some(cam_trimmed) && !has_recs {
            revoke_target(tid);
            cleanup_share_clip_artifacts(&state.media_dir, tid);
        }
    }
    for dir in [
        state.media_dir.join("exports"),
        state.media_dir.join("clips"),
    ] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let Some(fname_str) = fname.to_str() else {
                continue;
            };
            let target_id = fname_str
                .strip_prefix(".share_redacted_")
                .and_then(|s| s.strip_suffix(".mask_hash"))
                .or_else(|| {
                    fname_str
                        .strip_prefix("share_redacted_")
                        .or_else(|| fname_str.strip_prefix("share_clip_"))
                        .and_then(|s| s.strip_suffix(".mp4").or_else(|| s.strip_suffix(".webp")))
                });
            let Some(tid) = target_id else {
                continue;
            };
            let mut should_clean = false;
            if let Ok(Some(ev)) = state.db.get_event(tid)
                && ev["camera"].as_str().map(str::trim) == Some(cam_trimmed)
            {
                let start = ev["start_time"].as_f64().unwrap_or(0.0);
                let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
                let no_recs =
                    !has_valid_recording_segments_for_window(state, cam_trimmed, start, end);
                if no_recs {
                    revoke_target(tid);
                }
                if (start <= query_before && end >= query_after) || no_recs {
                    should_clean = true;
                }
            }
            if !should_clean
                && let Ok(Some(rev)) = state.db.get_review_by_id(tid)
                && rev.camera.trim() == cam_trimmed
            {
                let start = rev.start_time;
                let end = rev.end_time.unwrap_or(start + 15.0);
                let no_recs =
                    !has_valid_recording_segments_for_window(state, cam_trimmed, start, end);
                if no_recs {
                    revoke_target(tid);
                }
                if (start <= query_before && end >= query_after) || no_recs {
                    should_clean = true;
                }
            }
            if should_clean {
                cleanup_share_clip_artifacts(&state.media_dir, tid);
            }
        }
    }
    if !revoked_target_ids.is_empty()
        && let Ok(mut credits) = state.share_engine.page_video_credits.lock()
    {
        for link in &existing_links {
            if let (Some(jti), Some(tid)) = (link["id"].as_str(), link["target_id"].as_str())
                && revoked_target_ids.contains(tid)
            {
                credits.remove(jti);
            }
        }
    }
    purge_revoked_share_credits(state);
}

const MAX_SERVER_PRIVACY_MASK_DEPTH: usize = 16;

fn collect_normalized_polygon_strings(val: &Value, out: &mut Vec<String>, depth: usize) -> bool {
    if depth > MAX_SERVER_PRIVACY_MASK_DEPTH {
        return false;
    }
    match val {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed.starts_with("invalid_") {
                return false;
            }
            if !out.contains(s) {
                out.push(s.clone());
            }
            true
        }
        Value::Array(items) => {
            if items.is_empty() {
                return false;
            }
            for item in items {
                if !collect_normalized_polygon_strings(item, out, depth + 1) {
                    return false;
                }
            }
            true
        }
        Value::Object(map) => {
            if map.is_empty() {
                return false;
            }
            let before = out.len();
            if let Some(zones) = map.get("zones") {
                if !collect_normalized_polygon_strings(zones, out, depth + 1) {
                    return false;
                }
            } else {
                for (k, v) in map {
                    if !matches!(k.as_str(), "mode" | "style" | "pixelate" | "block_size")
                        && !k.starts_with("_norm_poly_")
                        && !collect_normalized_polygon_strings(v, out, depth + 1)
                    {
                        return false;
                    }
                }
            }
            out.len() > before
        }
        _ => false,
    }
}

/// Normalizes pixel-space `privacy_mask` coordinates (strings `"x1,y1,x2,y2"`, flat numeric arrays
/// `[x1, y1, ...]`, `[x, y]` coordinate pair arrays, and nested objects) into `[0.0..1.0]` normalized
/// polygon coordinate strings (`>= 6` numbers, expanding 2-point bounding boxes to 4-vertex rectangles)
/// relative to `(src_w, src_h)` so both FFmpeg `drawbox` and `ninjanvr_vision::PolygonMask` apply
/// accurately across any scaled or main-stream resolution.
pub fn normalize_privacy_mask_value_for_resolution(
    mask_val: &Value,
    src_w: u32,
    src_h: u32,
) -> Value {
    normalize_privacy_mask_value_inner(mask_val, src_w, src_h, 0)
}

fn normalize_privacy_mask_value_inner(
    mask_val: &Value,
    src_w: u32,
    src_h: u32,
    depth: usize,
) -> Value {
    if depth > MAX_SERVER_PRIVACY_MASK_DEPTH {
        return Value::String("invalid_depth_overflow".to_string());
    }
    let fw = if src_w > 0 { src_w as f64 } else { 1280.0 };
    let fh = if src_h > 0 { src_h as f64 } else { 720.0 };

    let format_normalized_coords = |nums: &[f64]| -> Option<String> {
        if nums.len() < 4
            || !nums.len().is_multiple_of(2)
            || nums.iter().any(|&v| !v.is_finite() || v < 0.0)
        {
            return None;
        }
        let is_already_norm = nums.iter().all(|&v| (0.0..=1.0).contains(&v));
        let mut norm_pts: Vec<(f64, f64)> = Vec::with_capacity(nums.len() / 2);
        for &[rx, ry] in nums.as_chunks::<2>().0 {
            let nx = if is_already_norm {
                rx.clamp(0.0, 1.0)
            } else {
                (rx / fw).clamp(0.0, 1.0)
            };
            let ny = if is_already_norm {
                ry.clamp(0.0, 1.0)
            } else {
                (ry / fh).clamp(0.0, 1.0)
            };
            norm_pts.push((nx, ny));
        }
        let expanded: Vec<(f64, f64)> = if norm_pts.len() == 2 {
            let (x1, y1) = norm_pts[0];
            let (x2, y2) = norm_pts[1];
            vec![(x1, y1), (x2, y1), (x2, y2), (x1, y2)]
        } else {
            norm_pts
        };
        if expanded.len() < 3 {
            return None;
        }
        let mut x_min = f64::MAX;
        let mut y_min = f64::MAX;
        let mut x_max = f64::MIN;
        let mut y_max = f64::MIN;
        let mut area2 = 0.0f64;
        for i in 0..expanded.len() {
            let (x1, y1) = expanded[i];
            let (x2, y2) = expanded[(i + 1) % expanded.len()];
            x_min = x_min.min(x1);
            y_min = y_min.min(y1);
            x_max = x_max.max(x1);
            y_max = y_max.max(y1);
            area2 += x1 * y2 - x2 * y1;
        }
        if (x_max - x_min) < 0.001 || (y_max - y_min) < 0.001 || area2.abs() <= 1e-5 {
            return None;
        }
        let mut parts = Vec::with_capacity(expanded.len() * 2);
        for (x, y) in expanded {
            parts.push(format!("{x:.6}"));
            parts.push(format!("{y:.6}"));
        }
        Some(parts.join(","))
    };

    match mask_val {
        Value::Null => Value::Null,
        Value::Bool(false) => Value::Null,
        Value::Bool(true) | Value::Number(_) => Value::String("invalid_privacy_mask".to_string()),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Value::String(String::new());
            }
            let mut nums = Vec::new();
            for p in trimmed.split(',') {
                if let Ok(v) = p.trim().parse::<f64>()
                    && v.is_finite()
                    && v >= 0.0
                {
                    nums.push(v);
                } else {
                    return Value::String("invalid_privacy_mask".to_string());
                }
            }
            match format_normalized_coords(&nums) {
                Some(norm_s) => Value::String(norm_s),
                None => Value::String("invalid_privacy_mask".to_string()),
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                return Value::Array(Vec::new());
            }
            if arr.iter().all(|v| v.is_number()) {
                let nums: Vec<f64> = arr
                    .iter()
                    .filter_map(|v| v.as_f64())
                    .filter(|&v| v.is_finite() && v >= 0.0)
                    .collect();
                if nums.len() == arr.len()
                    && let Some(norm_s) = format_normalized_coords(&nums)
                {
                    return Value::String(norm_s);
                }
                return Value::String("invalid_privacy_mask".to_string());
            }
            if arr.len() >= 2
                && arr.iter().all(|v| {
                    v.as_array()
                        .is_some_and(|p| p.len() == 2 && p[0].is_number() && p[1].is_number())
                })
            {
                let mut nums = Vec::with_capacity(arr.len() * 2);
                for item in arr {
                    if let Some(pair) = item.as_array()
                        && let (Some(x), Some(y)) = (pair[0].as_f64(), pair[1].as_f64())
                        && x.is_finite()
                        && y.is_finite()
                        && x >= 0.0
                        && y >= 0.0
                    {
                        nums.push(x);
                        nums.push(y);
                    } else {
                        return Value::String("invalid_privacy_mask".to_string());
                    }
                }
                if let Some(norm_s) = format_normalized_coords(&nums) {
                    return Value::String(norm_s);
                }
                return Value::String("invalid_privacy_mask".to_string());
            }
            let mut flat_polys: Vec<String> = Vec::new();
            let mut meta_entries: serde_json::Map<String, Value> = serde_json::Map::new();
            for item in arr {
                let norm_item = normalize_privacy_mask_value_inner(item, src_w, src_h, depth + 1);
                if let Value::Object(ref obj) = norm_item {
                    for mk in ["mode", "style", "pixelate", "block_size"] {
                        if let Some(mv) = obj.get(mk) {
                            meta_entries.insert(mk.to_string(), mv.clone());
                        }
                    }
                }
                let before = flat_polys.len();
                if !collect_normalized_polygon_strings(&norm_item, &mut flat_polys, depth + 1)
                    || flat_polys.len() == before
                {
                    return Value::String("invalid_privacy_mask".to_string());
                }
            }
            if flat_polys.is_empty() {
                return Value::String("invalid_privacy_mask".to_string());
            }
            if !meta_entries.is_empty() {
                meta_entries.insert(
                    "zones".to_string(),
                    Value::Array(flat_polys.iter().cloned().map(Value::String).collect()),
                );
                for (idx, poly_s) in flat_polys.into_iter().enumerate() {
                    meta_entries.insert(format!("_norm_poly_{idx}"), Value::String(poly_s));
                }
                return Value::Object(meta_entries);
            }
            Value::Array(flat_polys.into_iter().map(Value::String).collect())
        }
        Value::Object(map) => {
            if map.is_empty() {
                return Value::Object(serde_json::Map::new());
            }
            let mut out = serde_json::Map::new();
            let mut extra_polys: Vec<String> = Vec::new();
            let has_explicit_zones_or_coords = map.keys().any(|k| {
                !k.starts_with("_norm_poly_")
                    && !matches!(k.as_str(), "mode" | "style" | "pixelate" | "block_size")
            });
            for (k, v) in map {
                match k.as_str() {
                    "mode" | "style" => {
                        if !v.is_string() {
                            return Value::String("invalid_privacy_mask".to_string());
                        }
                        out.insert(k.clone(), v.clone());
                    }
                    "pixelate" => {
                        if !v.is_boolean() {
                            return Value::String("invalid_privacy_mask".to_string());
                        }
                        out.insert(k.clone(), v.clone());
                    }
                    "block_size" => {
                        if !v.is_number() {
                            return Value::String("invalid_privacy_mask".to_string());
                        }
                        out.insert(k.clone(), v.clone());
                    }
                    _ if k.starts_with("_norm_poly_") => {
                        if !has_explicit_zones_or_coords {
                            let norm_v =
                                normalize_privacy_mask_value_inner(v, src_w, src_h, depth + 1);
                            if !collect_normalized_polygon_strings(
                                &norm_v,
                                &mut extra_polys,
                                depth + 1,
                            ) {
                                return Value::String("invalid_privacy_mask".to_string());
                            }
                        }
                    }
                    _ => {
                        let norm_v = normalize_privacy_mask_value_inner(v, src_w, src_h, depth + 1);
                        let before = extra_polys.len();
                        if !collect_normalized_polygon_strings(&norm_v, &mut extra_polys, depth + 1)
                            || extra_polys.len() == before
                        {
                            return Value::String("invalid_privacy_mask".to_string());
                        }
                    }
                }
            }
            if extra_polys.is_empty() {
                return Value::String("invalid_privacy_mask".to_string());
            }
            out.insert(
                "zones".to_string(),
                Value::Array(extra_polys.iter().cloned().map(Value::String).collect()),
            );
            for (idx, poly_s) in extra_polys.into_iter().enumerate() {
                out.insert(format!("_norm_poly_{idx}"), Value::String(poly_s));
            }
            Value::Object(out)
        }
    }
}

fn sanitize_local_ffmpeg_path_arg(path: &Path) -> String {
    let raw = path.to_string_lossy();
    if raw.starts_with('-') || raw.contains(':') {
        format!("file:{raw}")
    } else {
        raw.into_owned()
    }
}

pub async fn redact_mp4_file_with_privacy(
    source_path: &Path,
    exports_dir: &Path,
    redacted_id: &str,
    out_file: &Path,
    privacy_mask: Option<&Value>,
) -> bool {
    let Some(vf) = ninjanvr_media::build_privacy_drawbox_filter(privacy_mask) else {
        return false;
    };
    if std::fs::create_dir_all(exports_dir).is_err() {
        return false;
    }
    if !std::fs::symlink_metadata(exports_dir)
        .is_ok_and(|m| !m.file_type().is_symlink() && m.file_type().is_dir())
    {
        return false;
    }
    if let Ok(out_meta) = std::fs::symlink_metadata(out_file)
        && (out_meta.file_type().is_symlink() || out_meta.file_type().is_dir())
    {
        let _ = std::fs::remove_file(out_file);
        if std::fs::symlink_metadata(out_file).is_ok() {
            return false;
        }
    }
    let nonce: u64 = rand::random();
    let tmp_out_file = exports_dir.join(format!(".{redacted_id}.{nonce:016x}.tmp.mp4"));
    struct TempFileGuard(PathBuf);
    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = TempFileGuard(tmp_out_file.clone());

    let ffmpeg_bin = ninjanvr_media::resolve_ffmpeg_bin();
    let mut cmd = tokio::process::Command::new(&ffmpeg_bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(sanitize_local_ffmpeg_path_arg(source_path))
        .args(["-vf", &vf, "-an", "-movflags", "+faststart"])
        .arg(sanitize_local_ffmpeg_path_arg(&tmp_out_file));

    let status_res = tokio::time::timeout(Duration::from_secs(120), cmd.status()).await;
    if !matches!(status_res, Ok(Ok(s)) if s.success()) {
        return false;
    }
    let tmp_for_check = tmp_out_file.clone();
    let out_for_rename = out_file.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if !is_regular_non_symlink_file(&tmp_for_check)
            || share_mp4_file_has_audio_track(&tmp_for_check)
        {
            return false;
        }
        std::fs::rename(&tmp_for_check, &out_for_rename).is_ok()
    })
    .await
    .unwrap_or(false)
}

pub async fn strip_mp4_audio_to_file(
    source_path: &Path,
    exports_dir: &Path,
    clip_id: &str,
    out_file: &Path,
) -> bool {
    if std::fs::create_dir_all(exports_dir).is_err() {
        return false;
    }
    if !std::fs::symlink_metadata(exports_dir)
        .is_ok_and(|m| !m.file_type().is_symlink() && m.file_type().is_dir())
    {
        return false;
    }
    if let Ok(out_meta) = std::fs::symlink_metadata(out_file)
        && (out_meta.file_type().is_symlink() || out_meta.file_type().is_dir())
    {
        let _ = std::fs::remove_file(out_file);
        if std::fs::symlink_metadata(out_file).is_ok() {
            return false;
        }
    }
    let nonce: u64 = rand::random();
    let tmp_out_file = exports_dir.join(format!(".{clip_id}.{nonce:016x}.tmp.mp4"));
    struct TempFileGuard(PathBuf);
    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = TempFileGuard(tmp_out_file.clone());

    let ffmpeg_bin = ninjanvr_media::resolve_ffmpeg_bin();
    let mut cmd = tokio::process::Command::new(&ffmpeg_bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(sanitize_local_ffmpeg_path_arg(source_path))
        .args(["-c:v", "copy", "-an", "-movflags", "+faststart"])
        .arg(sanitize_local_ffmpeg_path_arg(&tmp_out_file));

    let status_res = tokio::time::timeout(Duration::from_secs(120), cmd.status()).await;
    if !matches!(status_res, Ok(Ok(s)) if s.success()) {
        return false;
    }
    let tmp_for_check = tmp_out_file.clone();
    let out_for_rename = out_file.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if !is_regular_non_symlink_file(&tmp_for_check)
            || share_mp4_file_has_audio_track(&tmp_for_check)
        {
            return false;
        }
        std::fs::rename(&tmp_for_check, &out_for_rename).is_ok()
    })
    .await
    .unwrap_or(false)
}

pub fn share_mp4_file_has_audio_track(path: &Path) -> bool {
    use std::io::Read;
    let lmeta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    if !lmeta.file_type().is_file() {
        return true;
    }
    let Ok((mut f, fmeta)) = open_regular_non_symlink_readonly(path) else {
        return true;
    };
    let len = fmeta.len();
    if len == 0 {
        return false;
    }
    if len <= 8192 {
        let mut buf = Vec::with_capacity(len as usize);
        if (&mut f).take(8193).read_to_end(&mut buf).is_err() || buf.len() > 8192 {
            return true;
        }
        let has_iso_root_box = buf.len() >= 8
            && matches!(
                &buf[4..8],
                b"ftyp"
                    | b"styp"
                    | b"moov"
                    | b"moof"
                    | b"mdat"
                    | b"free"
                    | b"skip"
                    | b"wide"
                    | b"pdin"
                    | b"uuid"
            );
        let has_mp4_atoms = buf
            .windows(4)
            .any(|w| matches!(w, b"moov" | b"moof" | b"soun" | b"trak"));
        if !has_iso_root_box && !has_mp4_atoms {
            return false;
        }
        if buf.len() <= 32 && buf.get(4..8) == Some(b"ftyp") && !has_mp4_atoms {
            return false;
        }
    }
    ninjanvr_media::mp4_file_has_audio_track(path)
}

async fn share_mp4_file_has_audio_track_async(path: &Path) -> bool {
    let path_owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || share_mp4_file_has_audio_track(&path_owned))
        .await
        .unwrap_or(true)
}

#[derive(Debug, Default, Deserialize)]
pub struct ShareVideoQuery {
    pub sid: Option<String>,
}

pub(crate) fn verify_post_build_share_target_and_jti(
    state: &AppState,
    payload: &ShareTokenPayload,
    is_initial: bool,
    provided_sid: Option<&str>,
    valid_continuation: bool,
) -> Option<Response> {
    if !resolve_share_target_metadata(state, &payload.k, &payload.id).3 {
        cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    let now_ts = unix_now();
    if let Err((status, msg)) = verify_share_jti_with_view_mode(
        state,
        payload,
        now_ts,
        is_initial,
        false,
        provided_sid,
        valid_continuation,
    ) {
        if msg.starts_with("Share link creator ")
            && let Some(jti) = payload
                .jti
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            if let Err(err) = state.db.revoke_share_link(jti) {
                warn!("Failed to revoke share link '{jti}' after post-build verification: {err:#}");
            }
            if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
                map.remove(jti);
            }
        }
        if !has_active_or_in_flight_share_for_target(state, &payload.id, now_ts) {
            cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
        }
        return Some((status, Json(json!({ "error": msg }))).into_response());
    }
    None
}

pub async fn share_video_mp4(
    State(state): State<AppState>,
    method: axum::http::Method,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
    Query(q): Query<ShareVideoQuery>,
) -> Response {
    let now_ts = unix_now();
    let payload = match verify_share_token(&state.jwt_secret, &token, now_ts) {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                Json(json!({ "error": "Share link invalid or expired" })),
            )
                .into_response();
        }
    };
    let request_range = parse_video_range_bounds(&headers);
    if headers.contains_key(header::RANGE) && request_range.is_none() {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    }
    let is_initial = is_initial_or_unranged_video_request(&headers);
    let provided_sid = q.sid.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let mut has_valid_sid = false;
    let mut valid_continuation = false;
    let mut prev_stream_cursor: Option<(usize, u64, u64)> = None;
    if let Some(jti) = payload
        .jti
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Some(sid_str) = provided_sid {
            if let Ok(mut map) = state.share_engine.page_video_credits.lock()
                && let Some(entry) = map.get_mut(jti)
            {
                entry.prune_expired_sessions(now_ts);
                if entry.is_valid_sid(sid_str, now_ts) {
                    has_valid_sid = true;
                }
            }
        } else if let Some((start, end_opt)) = request_range
            && let Ok(mut map) = state.share_engine.page_video_credits.lock()
            && let Some(entry) = map.get_mut(jti)
        {
            entry.prune_expired_sessions(now_ts);
            if entry.active_streams.is_empty() && entry.active_stream_started {
                entry.active_streams.push(entry.min_next_range_start);
                entry.active_stream_expires.push(now_ts + 900);
            }
            if let Some((idx, prev_cursor)) = entry
                .active_streams
                .iter()
                .copied()
                .enumerate()
                .filter(|&(_, c)| start >= c && (start > 0 || (c == 0 && end_opt != Some(1))))
                .max_by_key(|&(_, c)| c)
            {
                valid_continuation = true;
                if method != axum::http::Method::HEAD {
                    let prev_exp = entry
                        .active_stream_expires
                        .get(idx)
                        .copied()
                        .unwrap_or(now_ts + 900);
                    prev_stream_cursor = Some((idx, prev_cursor, prev_exp));
                    let next_min = match end_opt {
                        Some(end) => end.saturating_add(1).max(2),
                        None => start.saturating_add(1).max(2),
                    };
                    entry.active_streams[idx] = next_min;
                    if idx < entry.active_stream_expires.len() {
                        entry.active_stream_expires[idx] = now_ts + 900;
                    }
                    entry.sync_stream_summary();
                }
            }
        }
    }

    let mut stream_cursor_guard =
        ShareStreamCursorGuard::new(state.clone(), payload.clone(), prev_stream_cursor);

    let resp: Response = async {
        let count_view = match verify_share_jti_with_view_mode(
            &state,
            &payload,
            now_ts,
            is_initial,
            false,
            provided_sid,
            valid_continuation,
        ) {
            Ok(cv) => cv,
            Err((status, msg)) => {
                return (status, Json(json!({ "error": msg }))).into_response();
            }
        };

        let finalize_video_resp = |resp: Response| -> Response {
            if method == axum::http::Method::HEAD {
                if resp.status() != StatusCode::RANGE_NOT_SATISFIABLE
                    && let Some(jti) = payload
                        .jti
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    && !state
                        .db
                        .is_share_link_active(jti, now_ts as f64)
                        .unwrap_or(false)
                {
                    if let Ok(mut map) = state.share_engine.page_video_credits.lock() {
                        map.remove(jti);
                    }
                    return (
                        StatusCode::GONE,
                        Json(json!({ "error": "Share link has been revoked" })),
                    )
                        .into_response();
                }
                return resp;
            }
            record_share_access_or_gone(
                &state,
                &payload,
                count_view,
                false,
                request_range,
                now_ts,
                provided_sid.map(str::to_string),
                has_valid_sid,
                valid_continuation,
                prev_stream_cursor,
                resp,
            )
        };

        let safe_id: String = payload
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
            .collect();

        if payload.k == "export" {
            let mut resolved_export: Option<(PathBuf, Option<String>)> = None;
            if let Ok(Some(exp)) = state.db.get_export_by_id(&payload.id) {
                let db_exp_cam = exp["camera"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| infer_camera_prefix_from_id(&state, &payload.id))
                    .or_else(|| {
                        exp["video_path"]
                            .as_str()
                            .and_then(|vp| Path::new(vp).file_stem())
                            .and_then(|s| s.to_str())
                            .and_then(|stem| infer_camera_prefix_from_id(&state, stem))
                    });
                if let Some(ref token_cam) = payload.cam
                    && db_exp_cam.as_deref() != Some(token_cam.trim())
                {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": "Share link camera mismatch" })),
                    )
                        .into_response();
                }
                if exp["in_progress"].as_bool().unwrap_or(false) {
                    cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
                    return StatusCode::NOT_FOUND.into_response();
                }
                if let Some(vp) = exp["video_path"].as_str() {
                    let disk_path = resolve_recording_disk_path(&state.media_dir, vp);
                    if let Some(safe_path) =
                        ensure_path_within_media_dir(&state.media_dir, &disk_path)
                        && safe_path.is_file()
                    {
                        let cam = db_exp_cam.or_else(|| payload.cam.clone());
                        resolved_export = Some((safe_path, cam));
                    }
                }
                if resolved_export.is_none() {
                    cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
                    return StatusCode::NOT_FOUND.into_response();
                }
            } else if !is_disallowed_export_fallback_id(&payload.id) && payload.id == safe_id {
                let fallback_cam = infer_camera_prefix_from_id(&state, &payload.id).or_else(|| {
                    payload
                        .id
                        .rsplitn(3, '_')
                        .nth(2)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                });
                if let Some(ref token_cam) = payload.cam
                    && let Some(ref cam_prefix) = fallback_cam
                    && token_cam.trim() != cam_prefix.trim()
                {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": "Share link camera mismatch" })),
                    )
                        .into_response();
                }
                let file_path = state
                    .media_dir
                    .join("exports")
                    .join(format!("{safe_id}.mp4"));
                if let Some(safe_path) = ensure_path_within_media_dir(&state.media_dir, &file_path)
                    && safe_path.is_file()
                {
                    if let Some(ref token_cam) = payload.cam
                        && fallback_cam.as_deref() != Some(token_cam.trim())
                    {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({ "error": "Share link camera mismatch" })),
                        )
                            .into_response();
                    }
                    let cam = payload.cam.clone().or(fallback_cam);
                    resolved_export = Some((safe_path, cam));
                }
            }

            if let Some((source_path, export_cam)) = resolved_export {
                let (privacy_mask, normalized_privacy_mask) = if let Some(ref cam) = export_cam {
                    let cfg = state.config.read().await;
                    if let Some(c) = cfg.cameras.get(cam) {
                        let raw = c.privacy_mask.clone();
                        let norm = normalize_privacy_mask_value_for_resolution(
                            &raw,
                            c.detect.width.max(1),
                            c.detect.height.max(1),
                        );
                        (Some(raw), Some(norm))
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };
                if has_active_privacy_mask(privacy_mask.as_ref()) {
                    if safe_id.is_empty() || payload.id != safe_id || payload.id.contains("..") {
                        return StatusCode::BAD_REQUEST.into_response();
                    }
                    let prepared_opt = match state
                        .share_engine
                        .prepare_redacted_export_clip_with_head(
                            &state.media_dir,
                            &source_path,
                            &safe_id,
                            privacy_mask.as_ref(),
                            normalized_privacy_mask.as_ref(),
                            method == axum::http::Method::HEAD,
                        )
                        .await
                    {
                        Ok(opt) => opt,
                        Err(status) => return status.into_response(),
                    };
                    if let Some(err_resp) = verify_post_build_share_target_and_jti(
                        &state,
                        &payload,
                        is_initial,
                        provided_sid,
                        valid_continuation,
                    ) {
                        return err_resp;
                    }
                    let Some(safe_path) = prepared_opt else {
                        return finalize_video_resp(build_share_video_head_placeholder_response(
                            &headers,
                        ));
                    };
                    if let Some(resp) = serve_mp4_file_with_range(&safe_path, &headers).await {
                        return finalize_video_resp(resp);
                    }
                    return StatusCode::NOT_FOUND.into_response();
                }

                let src_has_audio = {
                    let sp = source_path.clone();
                    let transcoder = state.share_engine.transcoder.clone();
                    tokio::task::spawn_blocking(move || transcoder.has_audio_track(&sp))
                        .await
                        .unwrap_or(true)
                };
                if src_has_audio {
                    if safe_id.is_empty() || payload.id != safe_id || payload.id.contains("..") {
                        return StatusCode::BAD_REQUEST.into_response();
                    }
                    let prepared_opt = match state
                        .share_engine
                        .prepare_audio_stripped_export_clip_with_head(
                            &state.media_dir,
                            &source_path,
                            &safe_id,
                            method == axum::http::Method::HEAD,
                        )
                        .await
                    {
                        Ok(opt) => opt,
                        Err(status) => return status.into_response(),
                    };
                    if let Some(err_resp) = verify_post_build_share_target_and_jti(
                        &state,
                        &payload,
                        is_initial,
                        provided_sid,
                        valid_continuation,
                    ) {
                        return err_resp;
                    }
                    let Some(safe_path) = prepared_opt else {
                        return finalize_video_resp(build_share_video_head_placeholder_response(
                            &headers,
                        ));
                    };
                    if let Some(resp) = serve_mp4_file_with_range(&safe_path, &headers).await {
                        return finalize_video_resp(resp);
                    }
                    return StatusCode::NOT_FOUND.into_response();
                }

                if let Some(resp) = serve_mp4_file_with_range(&source_path, &headers).await {
                    return finalize_video_resp(resp);
                }
            } else {
                cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
            }
        } else if payload.k == "review" || payload.k == "event" {
            if let (Ok(Some(rev)), Ok(Some(ev))) = (
                state.db.get_review_by_id(&payload.id),
                state.db.get_event(&payload.id),
            ) && ev["camera"]
                .as_str()
                .is_some_and(|ec| ec.trim() != rev.camera.trim())
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "Share link camera mismatch" })),
                )
                    .into_response();
            }
            let mut resolved_target: Option<(String, f64, f64)> = None;
            if payload.k == "review" {
                if let Ok(Some(rev)) = state.db.get_review_by_id(&payload.id) {
                    let rev_cam = rev.camera.trim();
                    if !rev_cam.is_empty() {
                        if let Some(ref token_cam) = payload.cam
                            && token_cam.trim() != rev_cam
                        {
                            return (
                                StatusCode::FORBIDDEN,
                                Json(json!({ "error": "Share link camera mismatch" })),
                            )
                                .into_response();
                        }
                        let end = rev.end_time.unwrap_or(rev.start_time + 15.0);
                        resolved_target = Some((rev_cam.to_string(), rev.start_time, end));
                    }
                } else if let Ok(Some(ev)) = state.db.get_event(&payload.id)
                    && let Some(cam) = ev["camera"]
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                {
                    if let Some(ref token_cam) = payload.cam
                        && token_cam.trim() != cam
                    {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({ "error": "Share link camera mismatch" })),
                        )
                            .into_response();
                    }
                    let start = ev["start_time"].as_f64().unwrap_or(0.0);
                    let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
                    resolved_target = Some((cam.to_string(), start, end));
                }
            } else if let Ok(Some(ev)) = state.db.get_event(&payload.id) {
                if let Some(cam) = ev["camera"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if let Some(ref token_cam) = payload.cam
                        && token_cam.trim() != cam
                    {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({ "error": "Share link camera mismatch" })),
                        )
                            .into_response();
                    }
                    let start = ev["start_time"].as_f64().unwrap_or(0.0);
                    let end = ev["end_time"].as_f64().unwrap_or(start + 15.0);
                    resolved_target = Some((cam.to_string(), start, end));
                }
            } else if let Ok(Some(rev)) = state.db.get_review_by_id(&payload.id)
                && !rev.camera.trim().is_empty()
            {
                let rev_cam = rev.camera.trim();
                if let Some(ref token_cam) = payload.cam
                    && token_cam.trim() != rev_cam
                {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": "Share link camera mismatch" })),
                    )
                        .into_response();
                }
                let end = rev.end_time.unwrap_or(rev.start_time + 15.0);
                resolved_target = Some((rev_cam.to_string(), rev.start_time, end));
            }

            if let Some((cam, start, raw_end)) = resolved_target {
                let end = raw_end.max(start).min(start + 300.0);

                let filter_valid_recs = |rows: Vec<ninjanvr_db::RecordingRow>| -> Vec<PathBuf> {
                    rows.into_iter()
                        .filter_map(|rec| {
                            let disk_path =
                                resolve_recording_disk_path(&state.media_dir, &rec.path);
                            let safe_path =
                                ensure_path_within_media_dir(&state.media_dir, &disk_path)?;
                            if safe_path.is_file() {
                                Some(safe_path)
                            } else {
                                None
                            }
                        })
                        .collect()
                };

                let mut recs = state
                    .db
                    .query_recordings(&cam, start - 5.0, end + 5.0)
                    .map(filter_valid_recs)
                    .unwrap_or_default();
                if recs.is_empty() {
                    recs = state
                        .db
                        .query_recordings(&cam, start - 10.0, end + 10.0)
                        .map(filter_valid_recs)
                        .unwrap_or_default();
                }
                if recs.is_empty() {
                    cleanup_share_clip_artifacts(&state.media_dir, &payload.id);
                    return StatusCode::NOT_FOUND.into_response();
                }

                let (privacy_mask, normalized_privacy_mask) = {
                    let cfg = state.config.read().await;
                    if let Some(c) = cfg.cameras.get(&cam) {
                        let raw = c.privacy_mask.clone();
                        let norm = normalize_privacy_mask_value_for_resolution(
                            &raw,
                            c.detect.width.max(1),
                            c.detect.height.max(1),
                        );
                        (Some(raw), Some(norm))
                    } else {
                        (None, None)
                    }
                };
                if has_active_privacy_mask(privacy_mask.as_ref()) {
                    if safe_id.is_empty() || payload.id != safe_id || payload.id.contains("..") {
                        return StatusCode::BAD_REQUEST.into_response();
                    }
                    if ninjanvr_media::build_privacy_drawbox_filter(
                        normalized_privacy_mask.as_ref(),
                    )
                    .is_none()
                    {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let Some(exports_dir) = ensure_safe_media_subdir(&state.media_dir, "exports")
                    else {
                        return StatusCode::NOT_FOUND.into_response();
                    };
                    let redacted_id = format!("share_redacted_{safe_id}");
                    let redacted_fname = format!("{redacted_id}.mp4");
                    let mask_hash_fname = format!(".{redacted_id}.mask_hash");
                    let redacted_path = exports_dir.join(&redacted_fname);
                    let mask_hash_path = exports_dir.join(&mask_hash_fname);
                    let current_mask_hash = compute_privacy_mask_hash(privacy_mask.as_ref());

                    let clip_lock = acquire_share_clip_lock(&state, &redacted_id).await;
                    let _clip_guard = clip_lock.lock().await;

                    let cache_invalid = {
                        let red_p = redacted_path.clone();
                        let mh_p = mask_hash_path.clone();
                        let recs_p = recs.clone();
                        let exp_hash = current_mask_hash.clone();
                        tokio::task::spawn_blocking(move || {
                            if is_regular_non_symlink_file(&red_p) {
                                let hash_matches = read_non_symlink_string(&mh_p)
                                    .map(|existing_hash| existing_hash.trim() == exp_hash)
                                    .unwrap_or(false);
                                !hash_matches
                                    || is_cached_share_artifact_stale(&red_p, &recs_p)
                                    || share_mp4_file_has_audio_track(&red_p)
                            } else {
                                std::fs::symlink_metadata(&red_p).is_ok()
                            }
                        })
                        .await
                        .unwrap_or(true)
                    };
                    if cache_invalid {
                        remove_non_symlink_dir_child(&exports_dir, &redacted_fname);
                        remove_non_symlink_dir_child(&exports_dir, &mask_hash_fname);
                    }

                    if !is_regular_non_symlink_file(&redacted_path) {
                        if method == axum::http::Method::HEAD {
                            drop(_clip_guard);
                            if let Some(err_resp) = verify_post_build_share_target_and_jti(
                                &state,
                                &payload,
                                is_initial,
                                provided_sid,
                                valid_continuation,
                            ) {
                                return err_resp;
                            }
                            return finalize_video_resp(
                                build_share_video_head_placeholder_response(&headers),
                            );
                        }
                        let Ok(_sem_permit) = state.share_engine.ffmpeg_semaphore.acquire().await
                        else {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        };
                        let build_res = tokio::time::timeout(
                            Duration::from_secs(120),
                            build_mp4_export_with_privacy(
                                &state.db,
                                &state.media_dir,
                                &redacted_id,
                                &cam,
                                "shared_redacted",
                                start - 5.0,
                                end + 5.0,
                                false,
                                normalized_privacy_mask.as_ref(),
                            ),
                        )
                        .await;
                        let _ = state.db.delete_export(&redacted_id);
                        remove_non_symlink_dir_child(
                            &state.media_dir.join("clips"),
                            &format!("{redacted_id}.webp"),
                        );
                        match build_res {
                            Ok(Ok(()))
                                if is_regular_non_symlink_file(&redacted_path)
                                    && !share_mp4_file_has_audio_track_async(&redacted_path)
                                        .await =>
                            {
                                let exp_dir_clone = exports_dir.clone();
                                let mh_fname_clone = mask_hash_fname.clone();
                                let mh_bytes = current_mask_hash.into_bytes();
                                let _ = tokio::task::spawn_blocking(move || {
                                    write_atomic_non_symlink_file(
                                        &exp_dir_clone,
                                        &mh_fname_clone,
                                        &mh_bytes,
                                    )
                                })
                                .await;
                            }
                            _ => {
                                remove_non_symlink_dir_child(&exports_dir, &redacted_fname);
                                remove_non_symlink_dir_child(&exports_dir, &mask_hash_fname);
                                return StatusCode::NOT_FOUND.into_response();
                            }
                        }
                    }
                    drop(_clip_guard);
                    if let Some(err_resp) = verify_post_build_share_target_and_jti(
                        &state,
                        &payload,
                        is_initial,
                        provided_sid,
                        valid_continuation,
                    ) {
                        return err_resp;
                    }

                    if let Some(safe_path) =
                        ensure_path_within_media_dir(&state.media_dir, &redacted_path)
                        && let Some(resp) = serve_mp4_file_with_range(&safe_path, &headers).await
                    {
                        return finalize_video_resp(resp);
                    }
                    return StatusCode::NOT_FOUND.into_response();
                }

                if safe_id.is_empty() || payload.id != safe_id || payload.id.contains("..") {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                let Some(exports_dir) = ensure_safe_media_subdir(&state.media_dir, "exports")
                else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let clip_id = format!("share_clip_{safe_id}");
                let clip_fname = format!("{clip_id}.mp4");
                let clip_path = exports_dir.join(&clip_fname);
                let clip_lock = acquire_share_clip_lock(&state, &clip_id).await;
                let _clip_guard = clip_lock.lock().await;
                let clip_invalid = {
                    let cp = clip_path.clone();
                    let recs_p = recs.clone();
                    tokio::task::spawn_blocking(move || {
                        (is_regular_non_symlink_file(&cp)
                            && (is_cached_share_artifact_stale(&cp, &recs_p)
                                || share_mp4_file_has_audio_track(&cp)))
                            || (std::fs::symlink_metadata(&cp).is_ok()
                                && !is_regular_non_symlink_file(&cp))
                    })
                    .await
                    .unwrap_or(true)
                };
                if clip_invalid {
                    remove_non_symlink_dir_child(&exports_dir, &clip_fname);
                }
                if !is_regular_non_symlink_file(&clip_path) {
                    if method == axum::http::Method::HEAD {
                        drop(_clip_guard);
                        if let Some(err_resp) = verify_post_build_share_target_and_jti(
                            &state,
                            &payload,
                            is_initial,
                            provided_sid,
                            valid_continuation,
                        ) {
                            return err_resp;
                        }
                        return finalize_video_resp(build_share_video_head_placeholder_response(
                            &headers,
                        ));
                    }
                    let Ok(_sem_permit) = state.share_engine.ffmpeg_semaphore.acquire().await
                    else {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    };
                    let build_res = tokio::time::timeout(
                        Duration::from_secs(120),
                        build_mp4_export_with_privacy(
                            &state.db,
                            &state.media_dir,
                            &clip_id,
                            &cam,
                            "shared_clip",
                            start - 5.0,
                            end + 5.0,
                            false,
                            None,
                        ),
                    )
                    .await;
                    let _ = state.db.delete_export(&clip_id);
                    remove_non_symlink_dir_child(
                        &state.media_dir.join("clips"),
                        &format!("{clip_id}.webp"),
                    );
                    let post_build_ok = {
                        let build_succeeded = matches!(build_res, Ok(Ok(())));
                        let single_rec = if recs.len() == 1 {
                            Some(recs[0].clone())
                        } else {
                            None
                        };
                        let exp_dir_clone = exports_dir.clone();
                        let clip_fname_clone = clip_fname.clone();
                        let clip_path_clone = clip_path.clone();
                        tokio::task::spawn_blocking(move || {
                            if !build_succeeded
                                && let Some(ref rec0) = single_rec
                                && !share_mp4_file_has_audio_track(rec0)
                                && let Some(bytes) =
                                    read_bounded_non_symlink_bytes_sync(rec0, 256 * 1024 * 1024)
                            {
                                let _ = write_atomic_non_symlink_file(
                                    &exp_dir_clone,
                                    &clip_fname_clone,
                                    &bytes,
                                );
                            }
                            is_regular_non_symlink_file(&clip_path_clone)
                                && !share_mp4_file_has_audio_track(&clip_path_clone)
                        })
                        .await
                        .unwrap_or(false)
                    };
                    if !post_build_ok {
                        remove_non_symlink_dir_child(&exports_dir, &clip_fname);
                        return StatusCode::NOT_FOUND.into_response();
                    }
                }
                drop(_clip_guard);
                if let Some(err_resp) = verify_post_build_share_target_and_jti(
                    &state,
                    &payload,
                    is_initial,
                    provided_sid,
                    valid_continuation,
                ) {
                    return err_resp;
                }
                if let Some(safe_path) = ensure_path_within_media_dir(&state.media_dir, &clip_path)
                    && let Some(resp) = serve_mp4_file_with_range(&safe_path, &headers).await
                {
                    return finalize_video_resp(resp);
                }
                return StatusCode::NOT_FOUND.into_response();
            }
        }

        StatusCode::NOT_FOUND.into_response()
    }
    .await;

    if resp.status().is_success() {
        stream_cursor_guard.disarm();
    }
    resp
}

// ============================================================================
// Part 5 — Unit Tests (In-Memory Seam & State Machine Verification)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_app_state;

    async fn setup_test_state_with_fake_transcoder()
    -> (PathBuf, AppState, Arc<FakeShareClipTranscoder>) {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr-share-engine-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let db_path = tmp_dir.join("ninjanvr.db");
        let cfg_path = tmp_dir.join("config.yml");
        let web_dir = tmp_dir.join("web");
        let media_dir = tmp_dir.join("media");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::create_dir_all(media_dir.join("exports")).unwrap();
        std::fs::create_dir_all(media_dir.join("clips")).unwrap();
        std::fs::write(web_dir.join("index.html"), "<html></html>").unwrap();

        let yaml = format!(
            r#"
version: 0.18-0
database:
  path: {}
auth:
  enabled: true
  hash_iterations: 1000
cameras:
  front_door:
    enabled: true
    detect:
      width: 640
      height: 480
      fps: 5
  backyard:
    enabled: true
    detect:
      width: 640
      height: 480
      fps: 5
"#,
            db_path.display()
        );
        std::fs::write(&cfg_path, yaml).unwrap();

        let mut state = new_app_state(&cfg_path, web_dir, media_dir, false)
            .await
            .unwrap();
        let fake_transcoder = Arc::new(FakeShareClipTranscoder::new());
        state.share_engine = ShareLinkEngine::with_transcoder(fake_transcoder.clone());

        (tmp_dir, state, fake_transcoder)
    }

    #[tokio::test]
    async fn token_signing_tamper_rejection_expiry_and_jti_matching() {
        let (tmp_dir, state, _) = setup_test_state_with_fake_transcoder().await;
        let secret = "unit-test-jwt-secret-key-0123456789abcdef";
        let now = 1_750_000_000u64;

        let payload = ShareTokenPayload {
            k: "export".to_string(),
            id: "front_door_100_110".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some("shr_unit_jti_1".to_string()),
        };

        let token = ShareLinkEngine::sign_token(secret, &payload);
        let verified = ShareLinkEngine::verify_token(secret, &token, now).unwrap();
        assert_eq!(verified.k, "export");
        assert_eq!(verified.id, "front_door_100_110");
        assert_eq!(verified.exp, now + 3600);
        assert_eq!(verified.cam.as_deref(), Some("front_door"));
        assert_eq!(verified.jti.as_deref(), Some("shr_unit_jti_1"));

        // Tampered signature or wrong secret -> 403 FORBIDDEN
        assert_eq!(
            ShareLinkEngine::verify_token("wrong-secret", &token, now).unwrap_err(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ShareLinkEngine::verify_token(secret, &format!("{token}x"), now).unwrap_err(),
            StatusCode::FORBIDDEN
        );

        // Expired token -> 410 GONE
        assert_eq!(
            ShareLinkEngine::verify_token(secret, &token, now + 3600).unwrap_err(),
            StatusCode::GONE
        );

        // Missing JTI on DB-backed verification -> 403 FORBIDDEN
        let no_jti_payload = ShareTokenPayload {
            jti: None,
            ..payload.clone()
        };
        assert_eq!(
            verify_share_jti(&state, &no_jti_payload, now)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );

        // Unregistered JTI -> 410 GONE
        assert_eq!(
            verify_share_jti(&state, &payload, now).unwrap_err().0,
            StatusCode::GONE
        );

        // Persist share link and verify JTI matching + payload mismatch detection
        state
            .db
            .insert_share_link_with_options(
                "shr_unit_jti_1",
                "export",
                "front_door_100_110",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
                Some(2),
            )
            .unwrap();
        assert!(verify_share_jti(&state, &payload, now).is_ok());

        let mismatched_cam_payload = ShareTokenPayload {
            cam: Some("backyard".to_string()),
            ..payload.clone()
        };
        assert_eq!(
            verify_share_jti(&state, &mismatched_cam_payload, now)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn playback_credit_byte_range_safari_probe_continuation_and_cursor_rollback() {
        let (tmp_dir, state, _) = setup_test_state_with_fake_transcoder().await;
        let now = 1_750_000_000u64;
        let jti = "shr_range_state_1";

        state
            .db
            .insert_share_link_with_options(
                jti,
                "export",
                "exp_range_1",
                Some("front_door"),
                "admin",
                now as f64,
                (now + 3600) as f64,
                Some(2),
            )
            .unwrap();

        let payload = ShareTokenPayload {
            k: "export".to_string(),
            id: "exp_range_1".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some(jti.to_string()),
        };

        let sample_mp4 = vec![0x42u8; 4096];

        // 1. Safari initial 2-byte probe `bytes=0-1` consumes 1 view credit and sets cursor to 0
        let mut h_probe = HeaderMap::new();
        h_probe.insert(header::RANGE, "bytes=0-1".parse().unwrap());
        let range_probe = parse_video_range_bounds(&h_probe);
        assert_eq!(range_probe, Some((0, Some(1))));

        let count_view_probe =
            verify_share_jti_with_view_mode(&state, &payload, now, true, false, None, false)
                .unwrap();
        assert!(
            count_view_probe,
            "initial Safari probe must count as first view"
        );

        let resp_probe = serve_mp4_bytes_with_range(sample_mp4.clone(), &h_probe);
        assert_eq!(resp_probe.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            next_stream_state_from_video_response(&resp_probe, range_probe),
            (true, 0)
        );
        let resp_probe = record_share_access_or_gone(
            &state,
            &payload,
            count_view_probe,
            false,
            range_probe,
            now,
            None,
            false,
            false,
            None,
            resp_probe,
        );
        assert_eq!(resp_probe.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            state.db.get_share_link(jti).unwrap().unwrap()["access_count"].as_i64(),
            Some(1)
        );

        // 2. Follow-up full-header range `bytes=0-1023` + sequential continuation `bytes=1024-4095`
        //    must be recognized as a valid continuation without consuming a second view credit.
        let mut h_chunk1 = HeaderMap::new();
        h_chunk1.insert(header::RANGE, "bytes=0-1023".parse().unwrap());
        let range_chunk1 = parse_video_range_bounds(&h_chunk1);
        let prev_cursor_1 = {
            let mut map = state.share_engine.page_video_credits.lock().unwrap();
            let entry = map.get_mut(jti).unwrap();
            assert_eq!(entry.active_streams, vec![0]);
            let prev = (
                0usize,
                entry.active_streams[0],
                entry.active_stream_expires[0],
            );
            entry.active_streams[0] = 1024;
            entry.sync_stream_summary();
            Some(prev)
        };
        let count_view_chunk1 =
            verify_share_jti_with_view_mode(&state, &payload, now, true, false, None, true)
                .unwrap();
        assert!(
            !count_view_chunk1,
            "follow-up range after bytes=0-1 probe must not consume another view"
        );
        let resp_chunk1 = serve_mp4_bytes_with_range(sample_mp4.clone(), &h_chunk1);
        let resp_chunk1 = record_share_access_or_gone(
            &state,
            &payload,
            count_view_chunk1,
            false,
            range_chunk1,
            now,
            None,
            false,
            true,
            prev_cursor_1,
            resp_chunk1,
        );
        assert_eq!(resp_chunk1.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            state.db.get_share_link(jti).unwrap().unwrap()["access_count"].as_i64(),
            Some(1)
        );

        // 3. Test rollback_share_stream_cursor when an upstream/range response fails
        let prev_before_fail = {
            let mut map = state.share_engine.page_video_credits.lock().unwrap();
            let entry = map.get_mut(jti).unwrap();
            assert_eq!(entry.active_streams[0], 1024);
            let prev = (
                0usize,
                entry.active_streams[0],
                entry.active_stream_expires[0],
            );
            entry.active_streams[0] = 2048;
            entry.sync_stream_summary();
            Some(prev)
        };
        let failed_resp = StatusCode::INTERNAL_SERVER_ERROR.into_response();
        let out_failed = record_share_access_or_gone(
            &state,
            &payload,
            false,
            false,
            Some((1024, Some(2047))),
            now,
            None,
            false,
            true,
            prev_before_fail,
            failed_resp,
        );
        assert_eq!(out_failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
        {
            let map = state.share_engine.page_video_credits.lock().unwrap();
            let entry = map.get(jti).unwrap();
            assert_eq!(
                entry.active_streams[0], 1024,
                "failed video response must roll back stream cursor to 1024"
            );
        }

        // 4. Opening a new player page session consumes the 2nd view credit and grants a session `sid`
        let count_view_page =
            verify_share_jti_with_view_mode(&state, &payload, now, true, true, None, false)
                .unwrap();
        assert!(count_view_page);
        let page_resp = record_share_access_or_gone(
            &state,
            &payload,
            true,
            true,
            None,
            now,
            Some("sid_second_viewer".to_string()),
            false,
            false,
            None,
            StatusCode::OK.into_response(),
        );
        assert_eq!(page_resp.status(), StatusCode::OK);
        assert_eq!(
            state.db.get_share_link(jti).unwrap().unwrap()["access_count"].as_i64(),
            Some(2)
        );

        // Video fetch using that page's `sid` succeeds without exceeding max_views=2
        assert!(
            !verify_share_jti_with_view_mode(
                &state,
                &payload,
                now,
                true,
                false,
                Some("sid_second_viewer"),
                false
            )
            .unwrap()
        );

        // A 3rd independent player page load is rejected with 410 GONE (max_views=2 reached)
        assert_eq!(
            verify_share_jti_with_view_mode(&state, &payload, now, true, true, None, false)
                .unwrap_err()
                .0,
            StatusCode::GONE
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn creator_permission_demotion_auto_revocation_and_regrant_lifecycle() {
        let (tmp_dir, state, _) = setup_test_state_with_fake_transcoder().await;
        let now = unix_now();
        let jti = "shr_creator_rbac_1";

        state
            .db
            .create_user_with_cameras("op_alice", "pass1234", "operator", "front_door", 1000)
            .unwrap();
        state
            .db
            .insert_share_link_with_options(
                jti,
                "export",
                "front_door_100_110",
                Some("front_door"),
                "op_alice",
                now as f64,
                (now + 3600) as f64,
                None,
            )
            .unwrap();

        let payload = ShareTokenPayload {
            k: "export".to_string(),
            id: "front_door_100_110".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some(jti.to_string()),
        };

        assert!(verify_share_jti(&state, &payload, now).is_ok());

        // 1. An active share link created by `ext:alice_sso` before local RBAC registration auto-revokes
        //    and populates `share_creator_auto_revoked` when `alice_sso` is registered as `viewer`.
        let auto_jti = "shr_creator_auto_1";
        state
            .db
            .insert_share_link_with_options(
                auto_jti,
                "export",
                "front_door_100_110",
                Some("front_door"),
                "ext:alice_sso",
                now as f64,
                (now + 3600) as f64,
                None,
            )
            .unwrap();
        state
            .db
            .create_user_with_cameras("alice_sso", "pass1234", "viewer", "front_door", 1000)
            .unwrap();
        let auto_payload = ShareTokenPayload {
            k: "export".to_string(),
            id: "front_door_100_110".to_string(),
            exp: now + 3600,
            cam: Some("front_door".to_string()),
            jti: Some(auto_jti.to_string()),
        };
        let auto_err = verify_share_jti(&state, &auto_payload, now).unwrap_err();
        assert_eq!(auto_err.0, StatusCode::FORBIDDEN);
        assert_eq!(
            auto_err.1,
            "Share link creator role is not permitted to share"
        );
        assert!(
            state
                .share_engine
                .creator_auto_revoked
                .lock()
                .unwrap()
                .contains_key(auto_jti),
            "lazy creator RBAC failure must record JTI in share_creator_auto_revoked"
        );

        // Pruning at a timestamp after expiry evicts expired entries; removing creator clears it immediately
        {
            let mut auto_map = state.share_engine.creator_auto_revoked.lock().unwrap();
            prune_share_creator_auto_revoked(&state, &mut auto_map, now);
            assert!(auto_map.contains_key(auto_jti));
            remove_creator_from_auto_revoked(&state, &mut auto_map, Some("alice_sso"), None);
            assert!(!auto_map.contains_key(auto_jti));
        }

        // 2. Restricting op_alice's camera scope to `backyard` revokes `front_door` share with 403 FORBIDDEN,
        //    and demoting to `viewer` via `on_creator_permissions_changed` transitions it to 410 GONE.
        state
            .db
            .update_user_allowed_cameras("op_alice", "backyard")
            .unwrap();
        state
            .share_engine
            .on_creator_permissions_changed(&state, "op_alice", None)
            .unwrap();

        let err = verify_share_jti(&state, &payload, now).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert_eq!(err.1, "Share link creator camera access revoked");

        state
            .db
            .update_user_role_with_cameras("op_alice", "viewer", Some("backyard"))
            .unwrap();
        state
            .share_engine
            .on_creator_permissions_changed(&state, "op_alice", Some("viewer"))
            .unwrap();
        assert_eq!(
            verify_share_jti(&state, &payload, now).unwrap_err().0,
            StatusCode::GONE
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn redacted_clip_caching_and_mask_hash_invalidation_via_fake_transcoder() {
        let (tmp_dir, state, fake_transcoder) = setup_test_state_with_fake_transcoder().await;
        let exports_dir = state.media_dir.join("exports");
        let source_path = exports_dir.join("front_door_200_210.mp4");
        std::fs::write(&source_path, synthesize_video_only_mp4_bytes(b"source-raw")).unwrap();

        let mask_v1 = json!("0,0,160,120");
        let norm_v1 = normalize_privacy_mask_value_for_resolution(&mask_v1, 640, 480);

        // 1. First call invokes FakeShareClipTranscoder and writes `.mask_hash`
        let out_p1 = state
            .share_engine
            .prepare_redacted_export_clip(
                &state.media_dir,
                &source_path,
                "front_door_200_210",
                Some(&mask_v1),
                Some(&norm_v1),
            )
            .await
            .unwrap();
        assert!(out_p1.is_file());
        assert_eq!(fake_transcoder.redaction_count(), 1);

        // 2. Second call with identical mask hits cache without invoking transcoder again
        let out_p2 = state
            .share_engine
            .prepare_redacted_export_clip(
                &state.media_dir,
                &source_path,
                "front_door_200_210",
                Some(&mask_v1),
                Some(&norm_v1),
            )
            .await
            .unwrap();
        assert_eq!(out_p1, out_p2);
        assert_eq!(
            fake_transcoder.redaction_count(),
            1,
            "cached redacted MP4 with matching .mask_hash must not re-transcode"
        );

        // 3. Changing the privacy mask invalidates `.mask_hash` and triggers a 2nd transcode
        let mask_v2 = json!("320,240,640,480");
        let norm_v2 = normalize_privacy_mask_value_for_resolution(&mask_v2, 640, 480);
        let out_p3 = state
            .share_engine
            .prepare_redacted_export_clip(
                &state.media_dir,
                &source_path,
                "front_door_200_210",
                Some(&mask_v2),
                Some(&norm_v2),
            )
            .await
            .unwrap();
        assert!(out_p3.is_file());
        assert_eq!(
            fake_transcoder.redaction_count(),
            2,
            "modified privacy mask hash must invalidate cache and re-transcode"
        );

        // 4. `on_target_deleted` revokes share links and removes cached redacted MP4 + `.mask_hash`
        state
            .share_engine
            .on_target_deleted(&state, Some("export"), "front_door_200_210")
            .unwrap();
        assert!(
            !exports_dir
                .join("share_redacted_front_door_200_210.mp4")
                .exists()
        );
        assert!(
            !exports_dir
                .join(".share_redacted_front_door_200_210.mask_hash")
                .exists()
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
