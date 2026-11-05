use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use hmac::Hmac;
use pbkdf2::pbkdf2;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::info;

const PASSWORD_HASH_ALGORITHM: &str = "pbkdf2_sha256";

pub const NINJANVR_SECURITY_CLASSES: [&str; 15] = [
    "person",
    "face",
    "car",
    "motorcycle",
    "bicycle",
    "truck",
    "bus",
    "license_plate",
    "dog",
    "cat",
    "bird",
    "package",
    "waste_bin",
    "backpack",
    "umbrella",
];

pub fn security_class_id(label: &str) -> Option<usize> {
    let normalized = label.trim().to_ascii_lowercase().replace([' ', '-'], "_");
    NINJANVR_SECURITY_CLASSES
        .iter()
        .position(|&c| c == normalized)
}

pub fn security_class_label(class_id: usize) -> Option<&'static str> {
    NINJANVR_SECURITY_CLASSES.get(class_id).copied()
}

pub fn deterministic_dataset_split(id: &str) -> &'static str {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in id.as_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    if hash.is_multiple_of(5) {
        "val"
    } else {
        "train"
    }
}

fn default_annotation_score() -> f64 {
    1.0
}

fn default_annotation_source() -> String {
    "event".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetAnnotationBox {
    pub class_id: usize,
    pub label: String,
    pub cx: f64,
    pub cy: f64,
    pub w: f64,
    pub h: f64,
    #[serde(default = "default_annotation_score")]
    pub score: f64,
    #[serde(default = "default_annotation_source")]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetSampleDto {
    pub id: String,
    pub event_id: Option<String>,
    pub camera: String,
    #[serde(default)]
    pub split: String,
    pub sample_kind: String,
    pub image_path: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub annotations: Vec<DatasetAnnotationBox>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub night_mode: bool,
    pub created_at: f64,
}

#[derive(Clone)]
pub struct NinjaDatabase {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbUser {
    pub username: String,
    pub password_hash: String,
    pub role: String,
    #[serde(default = "default_wildcard_cameras")]
    pub allowed_cameras: String,
    pub notification_tokens: Value,
}

fn default_wildcard_cameras() -> String {
    "*".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingRow {
    pub id: String,
    pub camera: String,
    pub path: String,
    pub start_time: f64,
    pub end_time: f64,
    pub duration: f64,
    pub motion: i64,
    pub objects: i64,
    pub dbfs: i64,
    pub segment_size: f64,
    pub regions: i64,
    pub motion_heatmap: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItemDto {
    pub id: String,
    pub camera: String,
    pub start_time: f64,
    pub end_time: Option<f64>,
    pub severity: String,
    pub thumb_path: String,
    pub data: Value,
    pub has_been_reviewed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDto {
    pub id: String,
    pub camera: String,
    pub label: String,
    pub zones: Value,
    pub start_time: f64,
    pub end_time: Option<f64>,
    pub has_clip: bool,
    pub has_snapshot: bool,
    pub plus_id: Option<String>,
    pub retain_indefinitely: bool,
    pub sub_label: Option<String>,
    pub top_score: Option<f64>,
    pub false_positive: Option<bool>,
    #[serde(rename = "box")]
    pub box_coords: Option<Value>,
    pub data: Value,
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub area: Option<u32>,
    #[serde(default)]
    pub ratio: Option<f32>,
    #[serde(default)]
    pub score: Option<f32>,
    #[serde(default)]
    pub stationary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRow {
    pub id: String,
    pub camera: String,
    pub name: String,
    pub date: f64,
    pub video_path: String,
    pub thumb_path: String,
    pub in_progress: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ReviewFilter {
    pub cameras: Option<Vec<String>>,
    pub labels: Option<Vec<String>>,
    pub exclude_labels: Option<Vec<String>>,
    pub zones: Option<Vec<String>>,
    pub reviewed: Option<bool>,
    pub severity: Option<String>,
    pub min_score: Option<f64>,
    pub stationary: Option<bool>,
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub cameras: Option<Vec<String>>,
    pub labels: Option<Vec<String>>,
    pub exclude_labels: Option<Vec<String>>,
    pub zones: Option<Vec<String>>,
    pub sub_labels: Option<Vec<String>>,
    pub plate: Option<String>,
    pub recognized_license_plates: Option<Vec<String>>,
    pub favorites: Option<bool>,
    pub min_score: Option<f64>,
    pub stationary: Option<bool>,
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub limit: Option<usize>,
}

/// Strips any `/media/<root>/` prefix (such as `/media/ninjanvr/`) and returns the relative path
/// under the media mount (`recordings/...`, `clips/...`, `exports/...`).
pub fn strip_media_root(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/media/")?;
    let (_, rel) = rest.split_once('/')?;
    if rel.is_empty() { None } else { Some(rel) }
}

const MIN_PBKDF2_ITERATIONS: u32 = 1_000;
const MAX_PBKDF2_ITERATIONS: u32 = 10_000_000;

/// Hashes a password using the `pbkdf2_sha256$<iterations>$<salt>$<b64>` format.
pub fn hash_password(password: &str, salt: Option<&str>, iterations: u32) -> String {
    let effective_iterations = iterations.clamp(MIN_PBKDF2_ITERATIONS, MAX_PBKDF2_ITERATIONS);
    let owned_salt: String;
    let salt_str = match salt {
        Some(s) if !s.is_empty() && !s.contains('$') => s,
        _ => {
            let mut bytes = [0u8; 16];
            rand::fill(&mut bytes);
            owned_salt = bytes.iter().map(|b| format!("{b:02x}")).collect();
            &owned_salt
        }
    };

    let mut dk = [0u8; 32];
    let _ = pbkdf2::<Hmac<Sha256>>(
        password.as_bytes(),
        salt_str.as_bytes(),
        effective_iterations,
        &mut dk,
    );
    let b64 = base64::engine::general_purpose::STANDARD.encode(dk);
    format!("{PASSWORD_HASH_ALGORITHM}${effective_iterations}${salt_str}${b64}")
}

/// Verifies a plaintext password against a `pbkdf2_sha256$<iterations>$<salt>$<b64>` hash.
pub fn verify_password(password: &str, password_hash: &str) -> bool {
    let parts: Vec<&str> = password_hash.splitn(4, '$').collect();
    if parts.len() != 4 {
        return false;
    }
    if parts[0] != PASSWORD_HASH_ALGORITHM {
        return false;
    }
    let Ok(iterations) = parts[1].parse::<u32>() else {
        return false;
    };
    if !(MIN_PBKDF2_ITERATIONS..=MAX_PBKDF2_ITERATIONS).contains(&iterations) {
        return false;
    }
    let salt_str = parts[2];
    let digest_b64 = parts[3];
    if salt_str.is_empty() || digest_b64.is_empty() || digest_b64.contains('$') {
        return false;
    }
    let mut dk = [0u8; 32];
    let _ = pbkdf2::<Hmac<Sha256>>(
        password.as_bytes(),
        salt_str.as_bytes(),
        iterations,
        &mut dk,
    );
    let expected_b64 = base64::engine::general_purpose::STANDARD.encode(dk);
    constant_time_eq(expected_b64.as_bytes(), digest_b64.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    let mut diff = if a.len() == b.len() { 0u8 } else { 1u8 };
    for i in 0..max_len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0 && a.len() == b.len()
}

fn escape_sql_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn parse_json_or(raw: Option<String>, fallback: Value) -> Value {
    match raw {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or(fallback),
        _ => fallback,
    }
}

fn value_to_f64(val: &rusqlite::types::ValueRef<'_>) -> Option<f64> {
    match val {
        rusqlite::types::ValueRef::Real(f) => (*f).is_finite().then_some(*f),
        rusqlite::types::ValueRef::Integer(i) => Some(*i as f64),
        rusqlite::types::ValueRef::Text(bytes) => {
            let s = std::str::from_utf8(bytes).ok()?;
            if let Ok(f) = s.parse::<f64>() {
                return f.is_finite().then_some(f);
            }
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.timestamp_micros() as f64 / 1_000_000.0);
            }
            if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
                return Some(ndt.and_utc().timestamp_micros() as f64 / 1_000_000.0);
            }
            None
        }
        _ => None,
    }
}

fn json_array_matches_any(arr_val: &Value, allowed: &[String], also_verified: bool) -> bool {
    let Some(arr) = arr_val.as_array() else {
        return false;
    };
    for v in arr {
        if let Some(s) = v.as_str() {
            if allowed.iter().any(|a| a == s) {
                return true;
            }
            if also_verified
                && let Some(base) = s.strip_suffix("-verified")
                && allowed.iter().any(|a| a == base)
            {
                return true;
            }
        }
    }
    false
}

fn parse_four_f64(val: &Value) -> Option<[f64; 4]> {
    let arr = val.as_array()?;
    if arr.len() < 4 {
        return None;
    }
    let coords = [
        arr[0].as_f64()?,
        arr[1].as_f64()?,
        arr[2].as_f64()?,
        arr[3].as_f64()?,
    ];
    coords.iter().all(|v| v.is_finite()).then_some(coords)
}

fn extract_norm_box_from_event(ev: &Value) -> [f64; 4] {
    parse_four_f64(&ev["box"])
        .or_else(|| parse_four_f64(&ev["data"]["box"]))
        .or_else(|| parse_four_f64(&ev["data"]["provenance"]["norm_bbox"]))
        .unwrap_or([0.0, 0.0, 0.0, 0.0])
}

fn extract_dhash64_from_value(val: &Value) -> Option<u64> {
    if let Some(u) = val.as_u64() {
        return Some(u);
    }
    if let Some(i) = val.as_i64() {
        return Some(i as u64);
    }
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        let hex_part = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        return u64::from_str_radix(hex_part, 16)
            .ok()
            .or_else(|| trimmed.parse::<u64>().ok());
    }
    None
}

fn iou_corners(a: [f64; 4], b: [f64; 4]) -> f64 {
    if !a.iter().all(|v| v.is_finite()) || !b.iter().all(|v| v.is_finite()) {
        return 0.0;
    }
    let aw = a[2] - a[0];
    let ah = a[3] - a[1];
    let bw = b[2] - b[0];
    let bh = b[3] - b[1];
    if !aw.is_finite()
        || !ah.is_finite()
        || !bw.is_finite()
        || !bh.is_finite()
        || aw <= 0.0
        || ah <= 0.0
        || bw <= 0.0
        || bh <= 0.0
    {
        return 0.0;
    }
    let ix1 = a[0].max(b[0]);
    let iy1 = a[1].max(b[1]);
    let ix2 = a[2].min(b[2]);
    let iy2 = a[3].min(b[3]);
    if ix2 <= ix1 || iy2 <= iy1 {
        return 0.0;
    }
    let inter = (ix2 - ix1) * (iy2 - iy1);
    let union = aw * ah + bw * bh - inter;
    if !inter.is_finite() || !union.is_finite() || union <= 0.0 {
        0.0
    } else {
        (inter / union).clamp(0.0, 1.0)
    }
}

/// Computes normalized bounding-box IoU supporting both `[x1, y1, x2, y2]` and `[x, y, w, h]`
/// (as returned by `BoundingBox::to_normalized`).
pub fn compute_norm_box_iou(a: [f64; 4], b: [f64; 4]) -> f64 {
    if !a.iter().all(|v| v.is_finite()) || !b.iter().all(|v| v.is_finite()) {
        return 0.0;
    }
    let both_valid_xyxy = a[2] > a[0] && a[3] > a[1] && b[2] > b[0] && b[3] > b[1];
    let xyxy = iou_corners(a, b);
    if both_valid_xyxy {
        let aw = a[2] - a[0];
        let ah = a[3] - a[1];
        let bw = b[2] - b[0];
        let bh = b[3] - b[1];
        let y_fixed_xywh_shift = (a[1] - b[1]).abs() < 1e-6
            && (a[3] - b[3]).abs() < 1e-6
            && a[1] < 2.5 * ah
            && (bw < aw * 0.9 || aw < bw * 0.9);
        let x_fixed_xywh_shift = (a[0] - b[0]).abs() < 1e-6
            && (a[2] - b[2]).abs() < 1e-6
            && a[0] < 2.5 * aw
            && (bh < ah * 0.9 || ah < bh * 0.9);
        if ((aw < a[0] && ah < a[1]) || (bw < b[0] && bh < b[1]))
            && !y_fixed_xywh_shift
            && !x_fixed_xywh_shift
        {
            return xyxy;
        }
    }
    let can_be_xywh = a[2] > 0.0
        && a[3] > 0.0
        && b[2] > 0.0
        && b[3] > 0.0
        && a[0] + a[2] <= 1.0001
        && a[1] + a[3] <= 1.0001
        && b[0] + b[2] <= 1.0001
        && b[1] + b[3] <= 1.0001;
    if can_be_xywh {
        let xywh = iou_corners(
            [a[0], a[1], a[0] + a[2], a[1] + a[3]],
            [b[0], b[1], b[0] + b[2], b[1] + b[3]],
        );
        xyxy.max(xywh)
    } else {
        xyxy
    }
}

fn normalize_allowed_cameras_str(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "*";
    }
    let mut has_token = false;
    for part in trimmed.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if part == "*" {
            return "*";
        }
        has_token = true;
    }
    if has_token { trimmed } else { "*" }
}

fn sanitize_annotation_boxes(boxes: &[DatasetAnnotationBox]) -> Vec<DatasetAnnotationBox> {
    boxes
        .iter()
        .map(|b| DatasetAnnotationBox {
            class_id: b.class_id,
            label: b.label.clone(),
            cx: if b.cx.is_finite() { b.cx } else { 0.0 },
            cy: if b.cy.is_finite() { b.cy } else { 0.0 },
            w: if b.w.is_finite() && b.w >= 0.0 {
                b.w
            } else {
                0.0
            },
            h: if b.h.is_finite() && b.h >= 0.0 {
                b.h
            } else {
                0.0
            },
            score: if b.score.is_finite() { b.score } else { 1.0 },
            source: b.source.clone(),
        })
        .collect()
}

impl NinjaDatabase {
    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open SQLite DB at {}", path.display()))?;

        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA cache_size = -2000;
            PRAGMA mmap_size = 0;
            PRAGMA temp_store = MEMORY;
            PRAGMA busy_timeout = 30000;
            PRAGMA foreign_keys = ON;
            "#,
        )?;

        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        };
        db.init_schema()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[doc(hidden)]
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let conn = self.lock_conn();
        conn.execute_batch(sql)?;
        Ok(())
    }

    pub fn init_schema(&self) -> Result<()> {
        let conn = self.lock_conn();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS "recordings" (
                "id" VARCHAR(30) NOT NULL PRIMARY KEY,
                "camera" VARCHAR(20) NOT NULL,
                "path" VARCHAR(255) NOT NULL,
                "start_time" DATETIME NOT NULL,
                "end_time" DATETIME NOT NULL,
                "duration" REAL NOT NULL,
                "objects" INTEGER,
                "motion" INTEGER,
                "segment_size" REAL NOT NULL,
                "dBFS" INTEGER,
                "regions" INTEGER,
                "motion_heatmap" TEXT NULL
            );
            CREATE TABLE IF NOT EXISTS "timeline" (
                "timestamp" DATETIME NOT NULL,
                "camera" VARCHAR(20) NOT NULL,
                "source" VARCHAR(20) NOT NULL,
                "source_id" VARCHAR(30),
                "class_type" VARCHAR(50) NOT NULL,
                "data" JSON
            );
            CREATE TABLE IF NOT EXISTS "regions" (
                "camera" VARCHAR(20) NOT NULL PRIMARY KEY,
                "last_update" DATETIME NOT NULL,
                "grid" JSON
            );
            CREATE TABLE IF NOT EXISTS "previews" (
                "id" VARCHAR(30) NOT NULL PRIMARY KEY,
                "camera" VARCHAR(20) NOT NULL,
                "path" VARCHAR(255) NOT NULL,
                "start_time" DATETIME NOT NULL,
                "end_time" DATETIME NOT NULL,
                "duration" REAL NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "reviewsegment" (
                "id" VARCHAR(30) NOT NULL PRIMARY KEY,
                "camera" VARCHAR(20) NOT NULL,
                "start_time" DATETIME NOT NULL,
                "end_time" DATETIME,
                "severity" VARCHAR(30) NOT NULL,
                "thumb_path" VARCHAR(255) NOT NULL,
                "data" JSON NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "export" (
                "id" VARCHAR(30) NOT NULL PRIMARY KEY,
                "camera" VARCHAR(20) NOT NULL,
                "name" VARCHAR(100) NOT NULL,
                "date" DATETIME NOT NULL,
                "video_path" VARCHAR(255) NOT NULL,
                "thumb_path" VARCHAR(255) NOT NULL,
                "in_progress" INTEGER NOT NULL,
                "export_case_id" VARCHAR(30) NULL
            );
            CREATE TABLE IF NOT EXISTS "exportcase" (
                "id" VARCHAR(30) NOT NULL PRIMARY KEY,
                "name" VARCHAR(100) NOT NULL,
                "description" TEXT NULL,
                "created_at" DATETIME NOT NULL,
                "updated_at" DATETIME NOT NULL,
                "event_ids" JSON NOT NULL DEFAULT '[]',
                "export_ids" JSON NOT NULL DEFAULT '[]'
            );
            CREATE TABLE IF NOT EXISTS "user" (
                "username" VARCHAR(30) NOT NULL PRIMARY KEY,
                "password_hash" VARCHAR(120) NOT NULL,
                "notification_tokens" JSON NOT NULL,
                "role" VARCHAR(20) NOT NULL DEFAULT 'admin',
                "allowed_cameras" TEXT NOT NULL DEFAULT '*',
                "password_changed_at" DATETIME NULL
            );
            CREATE TABLE IF NOT EXISTS "event" (
                "id" VARCHAR(30) NOT NULL PRIMARY KEY,
                "label" VARCHAR(20) NOT NULL,
                "camera" VARCHAR(20) NOT NULL,
                "start_time" DATETIME NOT NULL,
                "end_time" DATETIME,
                "top_score" REAL,
                "false_positive" INTEGER,
                "zones" JSON NOT NULL,
                "thumbnail" TEXT,
                "has_clip" INTEGER NOT NULL,
                "has_snapshot" INTEGER NOT NULL,
                "region" JSON,
                "box" JSON,
                "area" INTEGER,
                "retain_indefinitely" INTEGER NOT NULL,
                "sub_label" VARCHAR(100),
                "sub_label_score" REAL,
                "ratio" REAL,
                "plus_id" VARCHAR(30),
                "score" REAL,
                "model_hash" VARCHAR(32),
                "detector_type" VARCHAR(32),
                "model_type" VARCHAR(32),
                "data" JSON NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "userreviewstatus" (
                "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
                "user_id" VARCHAR(30) NOT NULL,
                "review_segment_id" VARCHAR(30) NOT NULL,
                "has_been_reviewed" INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY ("review_segment_id") REFERENCES "reviewsegment" ("id") ON DELETE CASCADE
            );
            CREATE TABLE IF NOT EXISTS "trigger" (
                "camera" VARCHAR(20) NOT NULL,
                "name" VARCHAR NOT NULL,
                "type" VARCHAR(10) NOT NULL,
                "model" VARCHAR(30) NOT NULL,
                "data" TEXT NOT NULL,
                "threshold" REAL,
                "embedding" BLOB,
                "triggering_event_id" VARCHAR(30),
                "last_triggered" DATETIME,
                PRIMARY KEY ("camera", "name")
            );
            CREATE INDEX IF NOT EXISTS "recordings_camera_start_time_end_time" ON "recordings" ("camera", "start_time" DESC, "end_time" DESC);
            CREATE INDEX IF NOT EXISTS "recordings_camera_end_time" ON "recordings" ("camera", "end_time" ASC);
            CREATE UNIQUE INDEX IF NOT EXISTS "recordings_path" ON "recordings" ("path");
            CREATE INDEX IF NOT EXISTS "review_segment_camera" ON "reviewsegment" ("camera");
            CREATE INDEX IF NOT EXISTS "review_segment_start_time_end_time" ON "reviewsegment" ("start_time" DESC, "end_time" DESC);
            CREATE INDEX IF NOT EXISTS "review_segment_camera_start_time_end_time" ON "reviewsegment" ("camera", "start_time" ASC, "end_time" ASC);
            CREATE INDEX IF NOT EXISTS "event_camera" ON "event" ("camera");
            CREATE INDEX IF NOT EXISTS "event_start_time_end_time" ON "event" ("start_time" DESC, "end_time" DESC);
            CREATE INDEX IF NOT EXISTS "event_camera_start_time_end_time" ON "event" ("camera", "start_time" ASC, "end_time" ASC);
            CREATE UNIQUE INDEX IF NOT EXISTS "userreviewstatus_user_segment" ON "userreviewstatus" ("user_id", "review_segment_id");
            CREATE TABLE IF NOT EXISTS "api_token" (
                "id" TEXT NOT NULL PRIMARY KEY,
                "name" TEXT NOT NULL,
                "token_hash" TEXT NOT NULL UNIQUE,
                "prefix" TEXT NOT NULL,
                "scope" TEXT NOT NULL,
                "created_at" REAL NOT NULL,
                "last_used_at" REAL NULL,
                "expires_at" REAL NULL
            );
            CREATE TABLE IF NOT EXISTS false_positive_signature (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                camera TEXT NOT NULL,
                label TEXT NOT NULL,
                norm_x1 REAL NOT NULL,
                norm_y1 REAL NOT NULL,
                norm_x2 REAL NOT NULL,
                norm_y2 REAL NOT NULL,
                dhash64 INTEGER,
                dominant_color TEXT,
                hit_count INTEGER NOT NULL DEFAULT 1,
                source_event_id TEXT UNIQUE,
                reason TEXT,
                created_at REAL NOT NULL,
                last_seen_at REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_fp_sig_cam_label ON false_positive_signature(camera, label);
            CREATE TABLE IF NOT EXISTS dataset_sample (
                id TEXT PRIMARY KEY,
                event_id TEXT,
                camera TEXT NOT NULL,
                split TEXT NOT NULL DEFAULT 'train',
                sample_kind TEXT NOT NULL,
                image_path TEXT NOT NULL,
                width INTEGER NOT NULL DEFAULT 0,
                height INTEGER NOT NULL DEFAULT 0,
                annotations TEXT NOT NULL DEFAULT '[]',
                verified INTEGER NOT NULL DEFAULT 0,
                night_mode INTEGER NOT NULL DEFAULT 0,
                created_at REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_dataset_sample_cam ON dataset_sample(camera, sample_kind);
            CREATE TABLE IF NOT EXISTS "share_link" (
                "id" TEXT NOT NULL PRIMARY KEY,
                "kind" TEXT NOT NULL,
                "target_id" TEXT NOT NULL,
                "camera" TEXT NULL,
                "created_by" TEXT NOT NULL,
                "created_at" REAL NOT NULL,
                "expires_at" REAL NOT NULL,
                "revoked" INTEGER NOT NULL DEFAULT 0,
                "access_count" INTEGER NOT NULL DEFAULT 0,
                "last_accessed_at" REAL NULL,
                "max_views" INTEGER NULL
            );
            CREATE INDEX IF NOT EXISTS "idx_share_link_target" ON "share_link" ("kind", "target_id");
            "#,
        )?;
        let _ = conn.execute(
            "ALTER TABLE user ADD COLUMN role VARCHAR(20) NOT NULL DEFAULT 'admin'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE user ADD COLUMN allowed_cameras TEXT NOT NULL DEFAULT '*'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE user ADD COLUMN password_changed_at DATETIME NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE recordings ADD COLUMN motion_heatmap TEXT NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE recordings ADD COLUMN stream_tier TEXT NOT NULL DEFAULT 'main'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE export ADD COLUMN export_case_id VARCHAR(30) NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE exportcase ADD COLUMN event_ids JSON NOT NULL DEFAULT '[]'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE exportcase ADD COLUMN export_ids JSON NOT NULL DEFAULT '[]'",
            [],
        );
        let _ = conn.execute("ALTER TABLE event ADD COLUMN sub_label_score REAL", []);
        let _ = conn.execute(
            "ALTER TABLE share_link ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE share_link ADD COLUMN last_accessed_at REAL NULL",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE share_link ADD COLUMN max_views INTEGER NULL",
            [],
        );
        Ok(())
    }

    pub fn set_recording_stream_tier(&self, id: &str, tier: &str) -> Result<usize> {
        let conn = self.lock_conn();
        Ok(conn.execute(
            "UPDATE recordings SET stream_tier = ?2 WHERE id = ?1",
            params![id, tier],
        )?)
    }

    pub fn list_recordings_by_dbfs(
        &self,
        target_dbfs: i64,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let capped_limit = limit.min(10_000) as i64;
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, path FROM recordings WHERE dbfs = ?1 ORDER BY start_time DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![target_dbfs, capped_limit], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_recording_dbfs(&self, id: &str, dbfs: i64) -> Result<usize> {
        let conn = self.lock_conn();
        Ok(conn.execute(
            "UPDATE recordings SET dbfs = ?2 WHERE id = ?1",
            params![id, dbfs],
        )?)
    }

    pub fn delete_expired_idle_main_recordings(
        &self,
        camera: &str,
        idle_main_cutoff: f64,
    ) -> Result<Vec<String>> {
        if !idle_main_cutoff.is_finite() {
            return Ok(Vec::new());
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut to_delete = Vec::new();
        {
            let mut stmt = tx.prepare(
                r#"
                SELECT r.id, r.path
                FROM recordings r
                WHERE r.camera = ?1
                  AND r.end_time < ?2
                  AND COALESCE(r.objects, 0) = 0
                  AND COALESCE(r.motion, 0) = 0
                  AND COALESCE(r.stream_tier, 'main') = 'main'
                  AND NOT EXISTS (
                      SELECT 1 FROM event e
                      WHERE e.camera = r.camera
                        AND e.start_time <= r.end_time
                        AND (
                          COALESCE(e.end_time, e.start_time + 15.0) >= r.start_time
                          OR (e.retain_indefinitely = 1 AND COALESCE(e.end_time, e.start_time + 30.0) >= r.start_time)
                        )
                  )
                ORDER BY r.start_time ASC
                LIMIT 200
                "#,
            )?;
            let rows = stmt.query_map(params![camera, idle_main_cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for r in rows {
                to_delete.push(r?);
            }
        }
        let mut paths = Vec::new();
        if !to_delete.is_empty() {
            let mut del_stmt = tx.prepare("DELETE FROM recordings WHERE id = ?1")?;
            for (id, path) in to_delete {
                del_stmt.execute(params![id])?;
                paths.push(path);
            }
        }
        tx.commit()?;
        Ok(paths)
    }

    pub fn delete_recording_by_id(&self, id: &str) -> Result<usize> {
        let conn = self.lock_conn();
        Ok(conn.execute("DELETE FROM recordings WHERE id = ?1", params![id])?)
    }

    pub fn ensure_default_admin(&self, iterations: u32) -> Result<Option<String>> {
        {
            let conn = self.lock_conn();
            let existing: Option<(String, String)> = conn
                .query_row(
                    "SELECT username, password_hash FROM user WHERE username = 'admin'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;

            if let Some((_, ref hash)) = existing
                && !hash.is_empty()
            {
                return Ok(None);
            }
        }

        let mut bytes = [0u8; 16];
        rand::fill(&mut bytes);
        let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let pw_hash = hash_password(&password, None, iterations.max(MIN_PBKDF2_ITERATIONS));

        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT username, password_hash FROM user WHERE username = 'admin'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((_, ref hash)) = existing
            && !hash.is_empty()
        {
            return Ok(None);
        }

        tx.execute(
            r#"
            INSERT INTO user (username, role, allowed_cameras, password_hash, notification_tokens)
            VALUES ('admin', 'admin', '*', ?1, '[]')
            ON CONFLICT(username) DO UPDATE SET
                role = 'admin',
                allowed_cameras = '*',
                password_hash = excluded.password_hash
            "#,
            params![pw_hash],
        )?;
        tx.commit()?;
        info!("Created default admin user in SQLite database");
        Ok(Some(password))
    }

    pub fn get_user(&self, username: &str) -> Result<Option<DbUser>> {
        let target = username.trim();
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT username, password_hash, role, notification_tokens, COALESCE(allowed_cameras, '*') FROM user WHERE username = ?1",
                params![target],
                |r| {
                    let tokens_raw: Option<String> = r.get(3).ok();
                    let allowed_cameras: String = r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "*".to_string());
                    Ok(DbUser {
                        username: r.get(0)?,
                        password_hash: r.get(1)?,
                        role: r.get::<_, Option<String>>(2)?.unwrap_or_else(|| "admin".to_string()),
                        allowed_cameras,
                        notification_tokens: parse_json_or(tokens_raw, json!([])),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_users(&self) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT username, role, password_changed_at, COALESCE(allowed_cameras, '*') FROM user ORDER BY username",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "username": r.get::<_, String>(0)?,
                "role": r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "admin".to_string()),
                "password_changed_at": r.get::<_, Option<String>>(2).ok().flatten(),
                "allowed_cameras": r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "*".to_string()),
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn create_user(
        &self,
        username: &str,
        password: &str,
        role: &str,
        iterations: u32,
    ) -> Result<()> {
        self.create_user_with_cameras(username, password, role, "*", iterations)
    }

    pub fn create_user_with_cameras(
        &self,
        username: &str,
        password: &str,
        role: &str,
        allowed_cameras: &str,
        iterations: u32,
    ) -> Result<()> {
        let trimmed = username.trim();
        if trimmed.is_empty() || trimmed.len() > 30 {
            anyhow::bail!("Username must be between 1 and 30 characters");
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
        {
            anyhow::bail!("Username may only contain letters, numbers, '_', '.', or '-'");
        }
        if password.is_empty() {
            anyhow::bail!("Password cannot be empty");
        }
        let norm_role = match role.trim().to_ascii_lowercase().as_str() {
            "viewer" => "viewer",
            "operator" => "operator",
            "admin" => "admin",
            _ => anyhow::bail!("Role must be 'admin', 'operator', or 'viewer'"),
        };
        let norm_cams = normalize_allowed_cameras_str(allowed_cameras);

        {
            let conn = self.lock_conn();
            let exists: Option<String> = conn
                .query_row(
                    "SELECT username FROM user WHERE username = ?1",
                    params![trimmed],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_some() {
                anyhow::bail!("User '{trimmed}' already exists");
            }
        }

        let pw_hash = hash_password(password, None, iterations.max(MIN_PBKDF2_ITERATIONS));
        let now_str = Utc::now().to_rfc3339();
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let exists: Option<String> = tx
            .query_row(
                "SELECT username FROM user WHERE username = ?1",
                params![trimmed],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            anyhow::bail!("User '{trimmed}' already exists");
        }
        tx.execute(
            r#"
            INSERT INTO user (username, role, allowed_cameras, password_hash, notification_tokens, password_changed_at)
            VALUES (?1, ?2, ?3, ?4, '[]', ?5)
            "#,
            params![trimmed, norm_role, norm_cams, pw_hash, now_str],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_user_password(
        &self,
        username: &str,
        password: &str,
        iterations: u32,
    ) -> Result<bool> {
        if password.is_empty() {
            anyhow::bail!("Password cannot be empty");
        }
        let pw_hash = hash_password(password, None, iterations.max(MIN_PBKDF2_ITERATIONS));
        let now_str = Utc::now().to_rfc3339();
        let conn = self.lock_conn();
        let updated = conn.execute(
            "UPDATE user SET password_hash = ?1, password_changed_at = ?2 WHERE username = ?3",
            params![pw_hash, now_str, username.trim()],
        )?;
        Ok(updated > 0)
    }

    fn revoke_disallowed_camera_share_links_in_tx(
        tx: &Connection,
        target: &str,
        norm_allowed_cameras: &str,
    ) -> Result<()> {
        let norm_cams = normalize_allowed_cameras_str(norm_allowed_cameras);
        if norm_cams == "*" {
            return Ok(());
        }
        let allowed: Vec<&str> = norm_cams
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if allowed.is_empty() || allowed.contains(&"*") {
            return Ok(());
        }
        let ext_target = format!("ext:{target}");
        let mut stmt = tx.prepare(
            "SELECT id, camera FROM share_link WHERE revoked = 0 AND (created_by = ?1 OR created_by = ?2)",
        )?;
        let rows = stmt.query_map(params![target, ext_target], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1).ok().flatten(),
            ))
        })?;
        let mut revoke_ids = Vec::new();
        for row in rows {
            let (id, cam) = row?;
            if !cam.as_deref().is_some_and(|c| allowed.contains(&c.trim())) {
                revoke_ids.push(id);
            }
        }
        drop(stmt);
        for id in revoke_ids {
            tx.execute(
                "UPDATE share_link SET revoked = 1 WHERE id = ?1",
                params![id],
            )?;
        }
        Ok(())
    }

    pub fn update_user_role(&self, username: &str, role: &str) -> Result<bool> {
        let norm_role = match role.trim().to_ascii_lowercase().as_str() {
            "viewer" => "viewer",
            "operator" => "operator",
            "admin" => "admin",
            _ => anyhow::bail!("Role must be 'admin', 'operator', or 'viewer'"),
        };
        let target = username.trim();
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let current_role: Option<String> = tx
            .query_row(
                "SELECT COALESCE(role, 'admin') FROM user WHERE username = ?1",
                params![target],
                |r| r.get(0),
            )
            .optional()?;
        let Some(existing_role) = current_role else {
            return Ok(false);
        };
        if norm_role != "admin" && existing_role.trim().eq_ignore_ascii_case("admin") {
            let other_admins: i64 = tx.query_row(
                "SELECT COUNT(*) FROM user WHERE LOWER(TRIM(COALESCE(role, 'admin'))) = 'admin' AND username != ?1",
                params![target],
                |r| r.get(0),
            )?;
            if other_admins == 0 {
                anyhow::bail!("Cannot demote the last admin account");
            }
        }
        let updated = tx.execute(
            "UPDATE user SET role = ?1 WHERE username = ?2",
            params![norm_role, target],
        )?;
        if updated > 0 {
            if norm_role == "viewer" {
                let ext_target = format!("ext:{target}");
                tx.execute(
                    "UPDATE share_link SET revoked = 1 WHERE created_by = ?1 OR created_by = ?2",
                    params![target, ext_target],
                )?;
            } else if norm_role == "operator" {
                let allowed_cams: String = tx
                    .query_row(
                        "SELECT COALESCE(allowed_cameras, '*') FROM user WHERE username = ?1",
                        params![target],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or_else(|| "*".to_string());
                let norm_cams = normalize_allowed_cameras_str(&allowed_cams);
                Self::revoke_disallowed_camera_share_links_in_tx(&tx, target, norm_cams)?;
            }
        }
        tx.commit()?;
        Ok(updated > 0)
    }

    pub fn update_user_allowed_cameras(
        &self,
        username: &str,
        allowed_cameras: &str,
    ) -> Result<bool> {
        let norm = normalize_allowed_cameras_str(allowed_cameras);
        let target = username.trim();
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE user SET allowed_cameras = ?1 WHERE username = ?2",
            params![norm, target],
        )?;
        if updated > 0 && norm != "*" {
            Self::revoke_disallowed_camera_share_links_in_tx(&tx, target, norm)?;
        }
        tx.commit()?;
        Ok(updated > 0)
    }

    /// Generates a high-entropy `nnvr_<32_hex>` API bearer token, stores ONLY its SHA-256 hash
    /// and 8-char display prefix in SQLite, and returns `(token_metadata_json, plaintext_token)`.
    pub fn create_api_token(
        &self,
        name: &str,
        scope: &str,
        expires_at: Option<f64>,
    ) -> Result<(Value, String)> {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            anyhow::bail!("Token name cannot be empty");
        }
        if let Some(exp) = expires_at
            && (!exp.is_finite() || exp <= 0.0)
        {
            anyhow::bail!("Token expiration timestamp must be a finite positive number");
        }
        let norm_scope = match scope.trim().to_ascii_lowercase().as_str() {
            "read_only" => "read_only",
            "metrics" => "metrics",
            "full" => "full",
            _ => anyhow::bail!("Scope must be 'read_only', 'metrics', or 'full'"),
        };

        let mut rand_bytes = [0u8; 16];
        rand::fill(&mut rand_bytes);
        let hex_part: String = rand_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let plaintext = format!("nnvr_{hex_part}");
        let prefix = plaintext[..13].to_string(); // e.g. "nnvr_a1b2c3d4"

        let hash_bytes = Sha256::digest(plaintext.as_bytes());
        let token_hash: String = hash_bytes.iter().map(|b| format!("{b:02x}")).collect();

        let mut id_bytes = [0u8; 6];
        rand::fill(&mut id_bytes);
        let id: String = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let created_at = Utc::now().timestamp() as f64;

        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let existing_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM api_token WHERE name = ?1 OR id = ?1",
            params![trimmed_name],
            |r| r.get(0),
        )?;
        if existing_count > 0 {
            anyhow::bail!("An API token with name '{trimmed_name}' already exists");
        }
        tx.execute(
            r#"
            INSERT INTO api_token (id, name, token_hash, prefix, scope, created_at, last_used_at, expires_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)
            "#,
            params![id, trimmed_name, token_hash, prefix, norm_scope, created_at, expires_at],
        )?;
        tx.commit()?;

        Ok((
            json!({
                "id": id,
                "name": trimmed_name,
                "prefix": prefix,
                "scope": norm_scope,
                "created_at": created_at,
                "last_used_at": Value::Null,
                "expires_at": expires_at
            }),
            plaintext,
        ))
    }

    pub fn list_api_tokens(&self) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, prefix, scope, created_at, last_used_at, expires_at FROM api_token ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let created_at = value_to_f64(&r.get_ref(4)?).unwrap_or(0.0);
            let last_used_at = value_to_f64(&r.get_ref(5)?);
            let expires_at = value_to_f64(&r.get_ref(6)?);
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "prefix": r.get::<_, String>(2)?,
                "scope": r.get::<_, String>(3)?,
                "created_at": created_at,
                "last_used_at": last_used_at,
                "expires_at": expires_at,
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_api_token(&self, id: &str) -> Result<bool> {
        let id_trimmed = id.trim();
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let token_name: Option<String> = tx
            .query_row(
                "SELECT name FROM api_token WHERE id = ?1",
                params![id_trimmed],
                |r| r.get(0),
            )
            .optional()?;
        let deleted = tx.execute("DELETE FROM api_token WHERE id = ?1", params![id_trimmed])?;
        if deleted > 0 {
            let creator_id = format!("token:{id_trimmed}");
            tx.execute(
                "UPDATE share_link SET revoked = 1 WHERE created_by = ?1",
                params![creator_id],
            )?;
            if let Some(name) = token_name {
                let creator_name = format!("token:{name}");
                tx.execute(
                    "UPDATE share_link SET revoked = 1 WHERE created_by = ?1",
                    params![creator_name],
                )?;
            }
        }
        tx.commit()?;
        Ok(deleted > 0)
    }

    pub fn is_api_token_active_by_name(&self, name_or_id: &str, now_ts: f64) -> Result<bool> {
        let target = name_or_id.trim();
        if target.is_empty() || !now_ts.is_finite() {
            return Ok(false);
        }
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            r#"
            SELECT scope, expires_at
            FROM api_token
            WHERE id = ?1 OR name = ?1
            "#,
        )?;
        let rows = stmt.query_map(params![target], |r| {
            let scope: String = r.get(0)?;
            let exp_ref = r.get_ref(1)?;
            let (exp_val, exp_invalid) = match exp_ref {
                rusqlite::types::ValueRef::Null => (None, false),
                other => match value_to_f64(&other) {
                    Some(v) => (Some(v), false),
                    None => (None, true),
                },
            };
            Ok((scope, exp_val, exp_invalid))
        })?;
        for r in rows {
            let (scope, expires_at, exp_invalid) = r?;
            if exp_invalid {
                continue;
            }
            if !scope.trim().eq_ignore_ascii_case("full") {
                continue;
            }
            if let Some(exp) = expires_at
                && (!exp.is_finite() || now_ts >= exp)
            {
                continue;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Validates an incoming `nnvr_...` bearer token by its SHA-256 hash, checks expiration,
    /// updates `last_used_at`, and returns `Some((token_name, scope))` if valid.
    pub fn verify_api_token(
        &self,
        raw_token: &str,
        now_ts: f64,
    ) -> Result<Option<(String, String)>> {
        if !raw_token.starts_with("nnvr_") || !now_ts.is_finite() {
            return Ok(None);
        }
        let hash_bytes = Sha256::digest(raw_token.as_bytes());
        let token_hash: String = hash_bytes.iter().map(|b| format!("{b:02x}")).collect();

        let conn = self.lock_conn();
        let row: Option<(String, String, String, Option<f64>, bool)> = conn
            .query_row(
                "SELECT id, name, scope, expires_at FROM api_token WHERE token_hash = ?1",
                params![token_hash],
                |r| {
                    let exp_ref = r.get_ref(3)?;
                    let (exp_val, exp_invalid) = match exp_ref {
                        rusqlite::types::ValueRef::Null => (None, false),
                        other => match value_to_f64(&other) {
                            Some(v) => (Some(v), false),
                            None => (None, true),
                        },
                    };
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, exp_val, exp_invalid))
                },
            )
            .optional()?;

        let Some((id, name, scope, expires_at, exp_invalid)) = row else {
            return Ok(None);
        };
        if exp_invalid {
            return Ok(None);
        }
        let norm_scope = match scope.trim().to_ascii_lowercase().as_str() {
            "read_only" => "read_only",
            "metrics" => "metrics",
            "full" => "full",
            _ => return Ok(None),
        };
        if let Some(exp) = expires_at
            && (!exp.is_finite() || now_ts >= exp)
        {
            return Ok(None);
        }
        let _ = conn.execute(
            "UPDATE api_token SET last_used_at = ?1 WHERE id = ?2",
            params![now_ts, id],
        );
        Ok(Some((name, norm_scope.to_string())))
    }

    pub fn delete_user(&self, username: &str) -> Result<bool> {
        let target = username.trim();
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let target_role: Option<String> = tx
            .query_row(
                "SELECT COALESCE(role, 'admin') FROM user WHERE username = ?1",
                params![target],
                |r| r.get(0),
            )
            .optional()?;
        let Some(role) = target_role else {
            return Ok(false);
        };
        if role.trim().eq_ignore_ascii_case("admin") {
            let other_admins: i64 = tx.query_row(
                "SELECT COUNT(*) FROM user WHERE LOWER(TRIM(COALESCE(role, 'admin'))) = 'admin' AND username != ?1",
                params![target],
                |r| r.get(0),
            )?;
            if other_admins == 0 {
                anyhow::bail!("Cannot delete the last admin account");
            }
        }
        tx.execute(
            "DELETE FROM userreviewstatus WHERE user_id = ?1",
            params![target],
        )?;
        let ext_target = format!("ext:{target}");
        tx.execute(
            "UPDATE share_link SET revoked = 1 WHERE created_by = ?1 OR created_by = ?2",
            params![target, ext_target],
        )?;
        let deleted = tx.execute("DELETE FROM user WHERE username = ?1", params![target])?;
        tx.commit()?;
        Ok(deleted > 0)
    }

    pub fn query_reviews(&self, filter: &ReviewFilter) -> Result<Vec<ReviewItemDto>> {
        let requested_limit = filter.limit.unwrap_or(250).min(2000);
        if requested_limit == 0 {
            return Ok(Vec::new());
        }
        if filter.before.is_some_and(|v| !v.is_finite())
            || filter.after.is_some_and(|v| !v.is_finite())
            || filter.min_score.is_some_and(|v| !v.is_finite())
        {
            return Ok(Vec::new());
        }

        let conn = self.lock_conn();
        let mut sql = String::from(
            r#"
            SELECT
                r.id,
                r.camera,
                r.start_time,
                r.end_time,
                r.severity,
                r.thumb_path,
                r.data,
                COALESCE(MAX(u.has_been_reviewed), 0) AS reviewed
            FROM reviewsegment r
            LEFT JOIN userreviewstatus u ON u.review_segment_id = r.id
            WHERE 1=1
            "#,
        );
        let mut bind_vals: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(before) = filter.before {
            bind_vals.push(rusqlite::types::Value::Real(before));
            sql.push_str(&format!(" AND r.start_time <= ?{}", bind_vals.len()));
        }
        if let Some(after) = filter.after {
            bind_vals.push(rusqlite::types::Value::Real(after));
            sql.push_str(&format!(
                " AND COALESCE(r.end_time, r.start_time) >= ?{}",
                bind_vals.len()
            ));
        }
        if let Some(ref sev) = filter.severity
            && !sev.is_empty()
            && sev != "all"
        {
            bind_vals.push(rusqlite::types::Value::Text(sev.clone()));
            sql.push_str(&format!(" AND r.severity = ?{}", bind_vals.len()));
        }
        if let Some(ref cams) = filter.cameras
            && !cams.is_empty()
            && !cams.iter().any(|c| c == "all")
        {
            let mut placeholders = Vec::with_capacity(cams.len());
            for c in cams {
                bind_vals.push(rusqlite::types::Value::Text(c.clone()));
                placeholders.push(format!("?{}", bind_vals.len()));
            }
            sql.push_str(&format!(" AND r.camera IN ({})", placeholders.join(",")));
        }

        sql.push_str(" GROUP BY r.id");
        if let Some(want_reviewed) = filter.reviewed {
            let rv = if want_reviewed { 1i64 } else { 0i64 };
            bind_vals.push(rusqlite::types::Value::Integer(rv));
            sql.push_str(&format!(" HAVING reviewed = ?{}", bind_vals.len()));
        }
        sql.push_str(" ORDER BY r.start_time DESC");

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind_vals.iter()), |row| {
            let start_time = value_to_f64(&row.get_ref(2)?).unwrap_or(0.0);
            let end_time = value_to_f64(&row.get_ref(3)?);
            let data_raw: Option<String> = row.get(6).ok();
            let reviewed_int: i64 = row.get(7).unwrap_or(0);
            Ok(ReviewItemDto {
                id: row.get(0)?,
                camera: row.get(1)?,
                start_time,
                end_time,
                severity: row.get(4)?,
                thumb_path: row.get(5)?,
                data: parse_json_or(
                    data_raw,
                    json!({
                        "detections": [],
                        "objects": [],
                        "verified_objects": [],
                        "sub_labels": [],
                        "zones": [],
                        "audio": []
                    }),
                ),
                has_been_reviewed: reviewed_int != 0,
            })
        })?;

        let mut items = Vec::new();
        for r in rows {
            let item = r?;
            if let Some(ref labels) = filter.labels
                && !labels.is_empty()
                && !labels.iter().any(|l| l == "all")
            {
                let obj_match = json_array_matches_any(&item.data["objects"], labels, true);
                let audio_match = json_array_matches_any(&item.data["audio"], labels, false);
                if !obj_match && !audio_match {
                    continue;
                }
            }
            if let Some(ref ex_labels) = filter.exclude_labels
                && !ex_labels.is_empty()
            {
                let obj_excluded = json_array_matches_any(&item.data["objects"], ex_labels, true);
                let audio_excluded = json_array_matches_any(&item.data["audio"], ex_labels, false);
                if obj_excluded || audio_excluded {
                    continue;
                }
            }
            if let Some(ref zones) = filter.zones
                && !zones.is_empty()
                && !zones.iter().any(|z| z == "all")
                && !json_array_matches_any(&item.data["zones"], zones, false)
            {
                continue;
            }
            if let Some(min_sc) = filter.min_score {
                let sc = item.data["provenance"]["top_score"]
                    .as_f64()
                    .or_else(|| item.data["top_score"].as_f64())
                    .unwrap_or(0.85);
                if sc < min_sc {
                    continue;
                }
            }
            if let Some(req_stat) = filter.stationary {
                let is_stat = item.data["provenance"]["stationary"]
                    .as_bool()
                    .unwrap_or(false);
                if is_stat != req_stat {
                    continue;
                }
            }
            items.push(item);
            if items.len() >= requested_limit {
                break;
            }
        }
        Ok(items)
    }

    pub fn get_review_by_id(&self, id: &str) -> Result<Option<ReviewItemDto>> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                r#"
                SELECT
                    r.id, r.camera, r.start_time, r.end_time, r.severity, r.thumb_path, r.data,
                    COALESCE(MAX(u.has_been_reviewed), 0) AS reviewed
                FROM reviewsegment r
                LEFT JOIN userreviewstatus u ON u.review_segment_id = r.id
                WHERE r.id = ?1
                GROUP BY r.id
                "#,
                params![id],
                |row| {
                    let start_time = value_to_f64(&row.get_ref(2)?).unwrap_or(0.0);
                    let end_time = value_to_f64(&row.get_ref(3)?);
                    let data_raw: Option<String> = row.get(6).ok();
                    let reviewed_int: i64 = row.get(7).unwrap_or(0);
                    Ok(ReviewItemDto {
                        id: row.get(0)?,
                        camera: row.get(1)?,
                        start_time,
                        end_time,
                        severity: row.get(4)?,
                        thumb_path: row.get(5)?,
                        data: parse_json_or(data_raw, json!({})),
                        has_been_reviewed: reviewed_int != 0,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_review_by_event_id(&self, event_id: &str) -> Result<Option<ReviewItemDto>> {
        let trimmed_event_id = event_id.trim();
        if trimmed_event_id.is_empty() || trimmed_event_id.contains('"') {
            return Ok(None);
        }
        let conn = self.lock_conn();
        let like_pattern = format!("%\"{}\"%", escape_sql_like(trimmed_event_id));
        let mut stmt = conn.prepare(
            r#"
            SELECT
                r.id, r.camera, r.start_time, r.end_time, r.severity, r.thumb_path, r.data,
                COALESCE(MAX(u.has_been_reviewed), 0) AS reviewed
            FROM reviewsegment r
            LEFT JOIN userreviewstatus u ON u.review_segment_id = r.id
            WHERE r.data LIKE ?1 ESCAPE '\'
            GROUP BY r.id
            ORDER BY r.start_time DESC
            LIMIT 50
            "#,
        )?;
        let rows = stmt.query_map(params![like_pattern], |row| {
            let start_time = value_to_f64(&row.get_ref(2)?).unwrap_or(0.0);
            let end_time = value_to_f64(&row.get_ref(3)?);
            let data_raw: Option<String> = row.get(6).ok();
            let reviewed_int: i64 = row.get(7).unwrap_or(0);
            Ok(ReviewItemDto {
                id: row.get(0)?,
                camera: row.get(1)?,
                start_time,
                end_time,
                severity: row.get(4)?,
                thumb_path: row.get(5)?,
                data: parse_json_or(data_raw, json!({})),
                has_been_reviewed: reviewed_int != 0,
            })
        })?;
        for r in rows {
            let item = r?;
            let matches_detection = item
                .data
                .get("detections")
                .and_then(Value::as_array)
                .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(trimmed_event_id)))
                || item
                    .data
                    .get("event_id")
                    .and_then(Value::as_str)
                    .is_some_and(|eid| eid == trimmed_event_id);
            if matches_detection {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    pub fn get_reviews_by_ids(&self, ids: &[String]) -> Result<Vec<ReviewItemDto>> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(item) = self.get_review_by_id(id)? {
                out.push(item);
            }
        }
        Ok(out)
    }

    pub fn query_review_ids(&self, ids: &[String]) -> Result<Vec<ReviewItemDto>> {
        self.get_reviews_by_ids(ids)
    }

    pub fn query_review_summary(&self, cameras: Option<&[String]>) -> Result<Value> {
        let conn = self.lock_conn();
        let now = Utc::now().timestamp() as f64;
        let last_24h = now - 86400.0;
        let last_30d = now - 86400.0 * 30.0;

        let mut bind_vals: Vec<rusqlite::types::Value> =
            vec![rusqlite::types::Value::Real(last_30d)];
        let mut cam_clause = String::new();
        if let Some(cams) = cameras
            && !cams.is_empty()
            && !cams.iter().any(|c| c == "all")
        {
            let mut placeholders = Vec::with_capacity(cams.len());
            for c in cams {
                bind_vals.push(rusqlite::types::Value::Text(c.clone()));
                placeholders.push(format!("?{}", bind_vals.len()));
            }
            cam_clause = format!(" AND r.camera IN ({})", placeholders.join(","));
        }

        let sql = format!(
            r#"
            SELECT
                r.start_time,
                r.severity,
                COALESCE(MAX(u.has_been_reviewed), 0) AS reviewed
            FROM reviewsegment r
            LEFT JOIN userreviewstatus u ON u.review_segment_id = r.id
            WHERE r.start_time >= ?1 {cam_clause}
            GROUP BY r.id
            ORDER BY r.start_time DESC
            "#
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind_vals.iter()), |row| {
            let ts = value_to_f64(&row.get_ref(0)?).unwrap_or(0.0);
            let sev: String = row.get(1)?;
            let rev: i64 = row.get(2).unwrap_or(0);
            Ok((ts, sev, rev != 0))
        })?;

        let mut last24_total_alert = 0u64;
        let mut last24_rev_alert = 0u64;
        let mut last24_total_det = 0u64;
        let mut last24_rev_det = 0u64;

        #[derive(Default)]
        struct DayTotals {
            reviewed_alert: u64,
            total_alert: u64,
            reviewed_detection: u64,
            total_detection: u64,
        }
        let mut by_day: BTreeMap<String, DayTotals> = BTreeMap::new();

        for row in rows {
            let (ts, sev, reviewed) = row?;
            let is_alert = sev == "alert";
            if ts >= last_24h {
                if is_alert {
                    last24_total_alert += 1;
                    if reviewed {
                        last24_rev_alert += 1;
                    }
                } else {
                    last24_total_det += 1;
                    if reviewed {
                        last24_rev_det += 1;
                    }
                }
            }

            if let Some(dt) = Utc.timestamp_opt(ts as i64, 0).single() {
                let day_str = dt.format("%Y-%m-%d").to_string();
                let entry = by_day.entry(day_str).or_default();
                if is_alert {
                    entry.total_alert += 1;
                    if reviewed {
                        entry.reviewed_alert += 1;
                    }
                } else {
                    entry.total_detection += 1;
                    if reviewed {
                        entry.reviewed_detection += 1;
                    }
                }
            }
        }

        let mut out = serde_json::Map::new();
        out.insert(
            "last24Hours".to_string(),
            json!({
                "reviewed_alert": last24_rev_alert,
                "reviewed_detection": last24_rev_det,
                "total_alert": last24_total_alert,
                "total_detection": last24_total_det
            }),
        );

        for (day, totals) in by_day.into_iter().rev() {
            out.insert(
                day.clone(),
                json!({
                    "day": day,
                    "reviewed_alert": totals.reviewed_alert,
                    "reviewed_detection": totals.reviewed_detection,
                    "total_alert": totals.total_alert,
                    "total_detection": totals.total_detection
                }),
            );
        }

        Ok(Value::Object(out))
    }

    pub fn mark_reviews_viewed(&self, ids: &[String], reviewed: bool) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let val = if reviewed { 1 } else { 0 };
        let mut count = 0;
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO userreviewstatus (user_id, review_segment_id, has_been_reviewed)
                SELECT 'admin', id, ?2 FROM reviewsegment WHERE id = ?1
                ON CONFLICT(user_id, review_segment_id) DO UPDATE SET has_been_reviewed = excluded.has_been_reviewed
                "#,
            )?;
            let mut clear_other_stmt = tx.prepare(
                "UPDATE userreviewstatus SET has_been_reviewed = 0 WHERE review_segment_id = ?1",
            )?;
            for id in ids {
                count += stmt.execute(params![id, val])?;
                if !reviewed {
                    clear_other_stmt.execute(params![id])?;
                }
            }
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn delete_reviews(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut count = 0;
        {
            let mut del_status =
                tx.prepare("DELETE FROM userreviewstatus WHERE review_segment_id = ?1")?;
            let mut del_seg = tx.prepare("DELETE FROM reviewsegment WHERE id = ?1")?;
            let mut rev_share =
                tx.prepare("UPDATE share_link SET revoked = 1 WHERE target_id = ?1")?;
            for id in ids {
                del_status.execute(params![id])?;
                let removed = del_seg.execute(params![id])?;
                if removed > 0 {
                    rev_share.execute(params![id])?;
                }
                count += removed;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn insert_review_segment(&self, item: &ReviewItemDto) -> Result<()> {
        let conn = self.lock_conn();
        let data_str = serde_json::to_string(&item.data)?;
        conn.execute(
            r#"
            INSERT INTO reviewsegment (id, camera, start_time, end_time, severity, thumb_path, data)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                end_time = excluded.end_time,
                severity = excluded.severity,
                thumb_path = excluded.thumb_path,
                data = excluded.data
            "#,
            params![
                item.id,
                item.camera,
                item.start_time,
                item.end_time,
                item.severity,
                item.thumb_path,
                data_str
            ],
        )?;
        Ok(())
    }

    pub fn query_events(&self, filter: &EventFilter) -> Result<Vec<Value>> {
        let requested_limit = filter.limit.unwrap_or(100).min(1000);
        if requested_limit == 0 {
            return Ok(Vec::new());
        }
        if filter.before.is_some_and(|v| !v.is_finite())
            || filter.after.is_some_and(|v| !v.is_finite())
            || filter.min_score.is_some_and(|v| !v.is_finite())
        {
            return Ok(Vec::new());
        }

        let mut req_plates: Vec<String> = Vec::new();
        let mut had_non_all_plate_token = false;
        if let Some(ref p) = filter.plate {
            for tok in p.split(',') {
                let trimmed = tok.trim();
                if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("all") {
                    had_non_all_plate_token = true;
                }
                let norm: String = trimmed
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                if !norm.is_empty() && !norm.eq_ignore_ascii_case("all") {
                    req_plates.push(norm);
                }
            }
        }
        if let Some(ref list) = filter.recognized_license_plates {
            for tok in list {
                let trimmed = tok.trim();
                if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("all") {
                    had_non_all_plate_token = true;
                }
                let norm: String = trimmed
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                if !norm.is_empty() && !norm.eq_ignore_ascii_case("all") {
                    req_plates.push(norm);
                }
            }
        }
        if had_non_all_plate_token && req_plates.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.lock_conn();
        let mut sql = String::from(
            r#"
            SELECT
                id, camera, label, zones, start_time, end_time,
                has_clip, has_snapshot, plus_id, retain_indefinitely,
                sub_label, top_score, false_positive, box, data, thumbnail, sub_label_score
            FROM event
            WHERE 1=1
            "#,
        );
        let mut bind_vals: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(before) = filter.before {
            bind_vals.push(rusqlite::types::Value::Real(before));
            sql.push_str(&format!(" AND start_time <= ?{}", bind_vals.len()));
        }
        if let Some(after) = filter.after {
            bind_vals.push(rusqlite::types::Value::Real(after));
            sql.push_str(&format!(
                " AND COALESCE(end_time, start_time) >= ?{}",
                bind_vals.len()
            ));
        }
        if let Some(fav) = filter.favorites {
            if fav {
                sql.push_str(" AND retain_indefinitely = 1");
            } else {
                sql.push_str(" AND COALESCE(retain_indefinitely, 0) = 0");
            }
        }
        if let Some(ref cams) = filter.cameras
            && !cams.is_empty()
            && !cams.iter().any(|c| c == "all")
        {
            let mut placeholders = Vec::with_capacity(cams.len());
            for c in cams {
                bind_vals.push(rusqlite::types::Value::Text(c.clone()));
                placeholders.push(format!("?{}", bind_vals.len()));
            }
            sql.push_str(&format!(" AND camera IN ({})", placeholders.join(",")));
        }
        if let Some(ref labels) = filter.labels
            && !labels.is_empty()
            && !labels.iter().any(|l| l == "all")
        {
            let mut placeholders = Vec::with_capacity(labels.len());
            for l in labels {
                bind_vals.push(rusqlite::types::Value::Text(l.clone()));
                placeholders.push(format!("?{}", bind_vals.len()));
            }
            sql.push_str(&format!(" AND label IN ({})", placeholders.join(",")));
        }
        if let Some(ref ex_labels) = filter.exclude_labels
            && !ex_labels.is_empty()
        {
            let mut placeholders = Vec::with_capacity(ex_labels.len());
            for l in ex_labels {
                bind_vals.push(rusqlite::types::Value::Text(l.clone()));
                placeholders.push(format!("?{}", bind_vals.len()));
            }
            sql.push_str(&format!(" AND label NOT IN ({})", placeholders.join(",")));
        }
        if let Some(min_sc) = filter.min_score {
            bind_vals.push(rusqlite::types::Value::Real(min_sc));
            sql.push_str(&format!(
                " AND COALESCE(top_score, score, 0.0) >= ?{}",
                bind_vals.len()
            ));
        }
        if let Some(ref subs) = filter.sub_labels
            && !subs.is_empty()
            && !subs.iter().any(|s| s == "all")
        {
            let mut placeholders = Vec::with_capacity(subs.len());
            for s in subs {
                bind_vals.push(rusqlite::types::Value::Text(s.clone()));
                placeholders.push(format!("?{}", bind_vals.len()));
            }
            sql.push_str(&format!(" AND sub_label IN ({})", placeholders.join(",")));
        }

        sql.push_str(" ORDER BY start_time DESC");

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind_vals.iter()), |row| {
            let start_time = value_to_f64(&row.get_ref(4)?).unwrap_or(0.0);
            let end_time = value_to_f64(&row.get_ref(5)?);
            let zones_raw: Option<String> = row.get(3).ok();
            let box_raw: Option<String> = row.get(13).ok();
            let data_raw: Option<String> = row.get(14).ok();
            let mut data_json = parse_json_or(data_raw, json!({}));
            let sub_label_col: Option<String> = row.get::<_, Option<String>>(10).ok().flatten();
            let sub_label_score_col: Option<f64> = row
                .get::<_, Option<f64>>(16)
                .ok()
                .flatten()
                .or_else(|| data_json.get("sub_label_score").and_then(|v| v.as_f64()))
                .filter(|v| v.is_finite());
            if let Some(sc) = sub_label_score_col
                && let Some(obj) = data_json.as_object_mut()
            {
                obj.entry("sub_label_score".to_string())
                    .or_insert_with(|| json!(sc));
            }
            let is_stationary = data_json["provenance"]["stationary"]
                .as_bool()
                .or_else(|| data_json["stationary"].as_bool())
                .unwrap_or(false);

            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "camera": row.get::<_, String>(1)?,
                "label": row.get::<_, String>(2)?,
                "zones": parse_json_or(zones_raw, json!([])),
                "start_time": start_time,
                "end_time": end_time,
                "has_clip": row.get::<_, i64>(6).unwrap_or(0) != 0,
                "has_snapshot": row.get::<_, i64>(7).unwrap_or(0) != 0,
                "plus_id": row.get::<_, Option<String>>(8).ok().flatten(),
                "retain_indefinitely": row.get::<_, i64>(9).unwrap_or(0) != 0,
                "sub_label": sub_label_col,
                "sub_label_score": sub_label_score_col,
                "top_score": row.get::<_, Option<f64>>(11).ok().flatten().filter(|v| v.is_finite()),
                "false_positive": row.get::<_, Option<i64>>(12).ok().flatten().map(|v| v != 0),
                "box": parse_json_or(box_raw, Value::Null),
                "stationary": is_stationary,
                "data": data_json,
                "thumbnail": row.get::<_, Option<String>>(15).ok().flatten()
            }))
        })?;

        let mut out = Vec::new();
        for r in rows {
            let item = r?;
            if let Some(ref zones) = filter.zones
                && !zones.is_empty()
                && !zones.iter().any(|z| z == "all")
                && !json_array_matches_any(&item["zones"], zones, false)
            {
                continue;
            }
            if let Some(req_stat) = filter.stationary {
                let is_stat = item["stationary"].as_bool().unwrap_or(false);
                if is_stat != req_stat {
                    continue;
                }
            }
            if !req_plates.is_empty() {
                let ev_plate_norm: String = item["data"]["recognized_license_plate"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                if ev_plate_norm.is_empty()
                    || !req_plates
                        .iter()
                        .any(|rp| ev_plate_norm == *rp || ev_plate_norm.contains(rp))
                {
                    continue;
                }
            }
            out.push(item);
            if out.len() >= requested_limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn query_events_explore(&self, limit_per_label: usize) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT label, COUNT(*) as cnt FROM event GROUP BY label ORDER BY cnt DESC LIMIT 100",
        )?;
        let label_counts: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);

        let per_label = limit_per_label.clamp(1, 100);
        let mut out = Vec::new();
        for (label, count) in label_counts {
            let events = self.query_events(&EventFilter {
                labels: Some(vec![label]),
                limit: Some(per_label),
                ..Default::default()
            })?;
            for mut ev in events {
                if let Some(obj) = ev.as_object_mut() {
                    obj.insert("event_count".to_string(), json!(count));
                }
                out.push(ev);
            }
        }
        Ok(out)
    }

    pub fn query_events_summary(&self) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT camera, label, sub_label, zones, start_time FROM event ORDER BY start_time DESC LIMIT 5000",
        )?;
        let rows = stmt.query_map([], |r| {
            let cam: String = r.get(0)?;
            let label: String = r.get(1)?;
            let sub: Option<String> = r.get(2).ok().flatten();
            let zones_raw: Option<String> = r.get(3).ok();
            let ts = value_to_f64(&r.get_ref(4)?).unwrap_or(0.0);
            Ok((cam, label, sub, parse_json_or(zones_raw, json!([])), ts))
        })?;

        let mut counts: BTreeMap<(String, String, Option<String>, String), i64> = BTreeMap::new();
        for r in rows {
            let (cam, label, sub, _zones, ts) = r?;
            if let Some(dt) = Utc.timestamp_opt(ts as i64, 0).single() {
                let day = dt.format("%Y-%m-%d").to_string();
                *counts.entry((cam, label, sub, day)).or_default() += 1;
            }
        }

        let mut out = Vec::new();
        for ((cam, label, sub, day), count) in counts {
            out.push(json!({
                "camera": cam,
                "label": label,
                "sub_label": sub,
                "day": day,
                "zones": [],
                "count": count
            }));
        }
        Ok(out)
    }

    /// Computes aggregated analytics for AI/LLM context digests and MCP tool calls over `[after, before]`,
    /// including counts by camera, label, zone, severity, hourly histogram, and top unreviewed alerts with deep links.
    pub fn query_ai_digest_summary(
        &self,
        after: f64,
        before: f64,
        allowed_cameras: Option<&[String]>,
    ) -> Result<Value> {
        if !after.is_finite() || !before.is_finite() {
            return Ok(json!({
                "window": {
                    "after": 0.0,
                    "before": 0.0
                },
                "total_events": 0,
                "counts_by_camera": {},
                "counts_by_label": {},
                "counts_by_zone": {},
                "counts_by_severity": {},
                "hourly_histogram": {},
                "unreviewed_alerts": []
            }));
        }
        let conn = self.lock_conn();
        let mut bind_vals: Vec<rusqlite::types::Value> = vec![
            rusqlite::types::Value::Real(after),
            rusqlite::types::Value::Real(before),
        ];
        let mut cam_placeholders = Vec::new();
        let mut cams_empty = false;
        if let Some(cams) = allowed_cameras {
            if cams.is_empty() {
                cams_empty = true;
            } else {
                for c in cams {
                    bind_vals.push(rusqlite::types::Value::Text(c.clone()));
                    cam_placeholders.push(format!("?{}", bind_vals.len()));
                }
            }
        }

        let mut ev_sql = String::from(
            "SELECT id, camera, label, zones, start_time, top_score FROM event WHERE start_time >= ?1 AND start_time <= ?2",
        );
        if cams_empty {
            ev_sql.push_str(" AND 1=0");
        } else if !cam_placeholders.is_empty() {
            ev_sql.push_str(&format!(" AND camera IN ({})", cam_placeholders.join(",")));
        }
        ev_sql.push_str(" ORDER BY start_time DESC LIMIT 5000");

        let mut total_events = 0usize;
        let mut by_camera: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_label: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_zone: BTreeMap<String, usize> = BTreeMap::new();
        let mut hourly_histogram: BTreeMap<String, usize> = BTreeMap::new();

        {
            let mut stmt = conn.prepare(&ev_sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(bind_vals.iter()), |r| {
                let cam: String = r.get(1)?;
                let label: String = r.get(2)?;
                let zones_raw: Option<String> = r.get(3).ok();
                let ts = value_to_f64(&r.get_ref(4)?).unwrap_or(0.0);
                Ok((cam, label, parse_json_or(zones_raw, json!([])), ts))
            })?;
            for r in rows {
                let (cam, label, zones_val, ts) = r?;
                total_events += 1;
                *by_camera.entry(cam).or_default() += 1;
                *by_label.entry(label).or_default() += 1;
                if let Some(z_arr) = zones_val.as_array() {
                    for z in z_arr.iter().filter_map(|v| v.as_str()) {
                        if !z.is_empty() {
                            *by_zone.entry(z.to_string()).or_default() += 1;
                        }
                    }
                }
                if let Some(dt) = Utc.timestamp_opt(ts as i64, 0).single() {
                    let hour_bucket = dt.format("%Y-%m-%dT%H:00Z").to_string();
                    *hourly_histogram.entry(hour_bucket).or_default() += 1;
                }
            }
        }

        let mut rev_sql = String::from(
            r#"
            SELECT
                r.id, r.camera, r.start_time, r.end_time, r.severity, r.data,
                COALESCE(MAX(u.has_been_reviewed), 0) as reviewed
            FROM reviewsegment r
            LEFT JOIN userreviewstatus u ON u.review_segment_id = r.id
            WHERE r.start_time >= ?1 AND r.start_time <= ?2
            "#,
        );
        if cams_empty {
            rev_sql.push_str(" AND 1=0");
        } else if !cam_placeholders.is_empty() {
            rev_sql.push_str(&format!(
                " AND r.camera IN ({})",
                cam_placeholders.join(",")
            ));
        }
        rev_sql.push_str(" GROUP BY r.id ORDER BY r.start_time DESC LIMIT 1000");

        let mut by_severity: BTreeMap<String, usize> = BTreeMap::new();
        let mut unreviewed_alerts = Vec::new();
        {
            let mut stmt = conn.prepare(&rev_sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(bind_vals.iter()), |r| {
                let id: String = r.get(0)?;
                let cam: String = r.get(1)?;
                let start_time = value_to_f64(&r.get_ref(2)?).unwrap_or(0.0);
                let end_time = value_to_f64(&r.get_ref(3)?);
                let sev: String = r.get(4)?;
                let data_raw: Option<String> = r.get(5).ok();
                let reviewed = r.get::<_, i64>(6).unwrap_or(0) != 0;
                Ok((
                    id,
                    cam,
                    start_time,
                    end_time,
                    sev,
                    parse_json_or(data_raw, json!({})),
                    reviewed,
                ))
            })?;
            for r in rows {
                let (id, cam, start_time, end_time, sev, data_val, reviewed) = r?;
                *by_severity.entry(sev.clone()).or_default() += 1;
                if sev == "alert" && !reviewed && unreviewed_alerts.len() < 20 {
                    let objs = data_val
                        .get("objects")
                        .cloned()
                        .unwrap_or_else(|| json!([]));
                    let zones = data_val.get("zones").cloned().unwrap_or_else(|| json!([]));
                    let top_score = data_val
                        .get("provenance")
                        .and_then(|p| p.get("top_score"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.85);
                    let first_det = data_val
                        .get("detections")
                        .and_then(|d| d.as_array())
                        .and_then(|a| a.first())
                        .and_then(|v| v.as_str())
                        .unwrap_or(&id);
                    unreviewed_alerts.push(json!({
                        "id": id,
                        "camera": cam,
                        "start_time": start_time,
                        "end_time": end_time,
                        "objects": objs,
                        "zones": zones,
                        "top_score": top_score,
                        "review_url": format!("/review?id={id}"),
                        "snapshot_url": format!("/api/events/{first_det}/snapshot.jpg")
                    }));
                }
            }
        }

        Ok(json!({
            "window": {
                "after": after,
                "before": before
            },
            "total_events": total_events,
            "counts_by_camera": by_camera,
            "counts_by_label": by_label,
            "counts_by_zone": by_zone,
            "counts_by_severity": by_severity,
            "hourly_histogram": hourly_histogram,
            "unreviewed_alerts": unreviewed_alerts
        }))
    }

    pub fn get_event(&self, id: &str) -> Result<Option<Value>> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                r#"
                SELECT
                    id, camera, label, zones, start_time, end_time,
                    has_clip, has_snapshot, plus_id, retain_indefinitely,
                    sub_label, top_score, false_positive, box, data, thumbnail, sub_label_score
                FROM event
                WHERE id = ?1
                "#,
                params![id],
                |row| {
                    let start_time = value_to_f64(&row.get_ref(4)?).unwrap_or(0.0);
                    let end_time = value_to_f64(&row.get_ref(5)?);
                    let zones_raw: Option<String> = row.get(3).ok();
                    let box_raw: Option<String> = row.get(13).ok();
                    let data_raw: Option<String> = row.get(14).ok();
                    let mut data_json = parse_json_or(data_raw, json!({}));
                    let sub_label_col: Option<String> = row.get::<_, Option<String>>(10).ok().flatten();
                    let sub_label_score_col: Option<f64> = row
                        .get::<_, Option<f64>>(16)
                        .ok()
                        .flatten()
                        .or_else(|| data_json.get("sub_label_score").and_then(|v| v.as_f64()))
                        .filter(|v| v.is_finite());
                    if let Some(sc) = sub_label_score_col
                        && let Some(obj) = data_json.as_object_mut()
                    {
                        obj.entry("sub_label_score".to_string())
                            .or_insert_with(|| json!(sc));
                    }
                    let is_stationary = data_json["provenance"]["stationary"]
                        .as_bool()
                        .or_else(|| data_json["stationary"].as_bool())
                        .unwrap_or(false);
                    Ok(json!({
                        "id": row.get::<_, String>(0)?,
                        "camera": row.get::<_, String>(1)?,
                        "label": row.get::<_, String>(2)?,
                        "zones": parse_json_or(zones_raw, json!([])),
                        "start_time": start_time,
                        "end_time": end_time,
                        "has_clip": row.get::<_, i64>(6).unwrap_or(0) != 0,
                        "has_snapshot": row.get::<_, i64>(7).unwrap_or(0) != 0,
                        "plus_id": row.get::<_, Option<String>>(8).ok().flatten(),
                        "retain_indefinitely": row.get::<_, i64>(9).unwrap_or(0) != 0,
                        "sub_label": sub_label_col,
                        "sub_label_score": sub_label_score_col,
                        "top_score": row.get::<_, Option<f64>>(11).ok().flatten().filter(|v| v.is_finite()),
                        "false_positive": row.get::<_, Option<i64>>(12).ok().flatten().map(|v| v != 0),
                        "box": parse_json_or(box_raw, Value::Null),
                        "stationary": is_stationary,
                        "data": data_json,
                        "thumbnail": row.get::<_, Option<String>>(15).ok().flatten()
                    }))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_events_by_ids(&self, ids: &[String]) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(ev) = self.get_event(id)? {
                out.push(ev);
            }
        }
        Ok(out)
    }

    pub fn query_event_ids(&self, ids: &[String]) -> Result<Vec<Value>> {
        self.get_events_by_ids(ids)
    }

    pub fn set_event_retain(&self, id: &str, retain: bool) -> Result<usize> {
        let conn = self.lock_conn();
        let val = if retain { 1 } else { 0 };
        Ok(conn.execute(
            "UPDATE event SET retain_indefinitely = ?1 WHERE id = ?2",
            params![val, id],
        )?)
    }

    pub fn set_event_false_positive(&self, id: &str, fp: bool) -> Result<usize> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let val = if fp { 1 } else { 0 };
        let updated = tx.execute(
            "UPDATE event SET false_positive = ?1 WHERE id = ?2",
            params![val, id],
        )?;
        if fp && updated > 0 {
            let row_opt: Option<(String, String, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT camera, label, box, data FROM event WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2).ok(), r.get(3).ok())),
                )
                .optional()?;
            if let Some((camera, label, box_raw, data_raw)) = row_opt {
                let ev = json!({
                    "camera": camera,
                    "label": label,
                    "box": parse_json_or(box_raw, Value::Null),
                    "data": parse_json_or(data_raw, json!({})),
                });
                let [nx1, ny1, nx2, ny2] = extract_norm_box_from_event(&ev);
                let dhash64 = extract_dhash64_from_value(&ev["data"]["dhash64"]);
                let dominant_color = ev["data"]["attributes"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .or_else(|| ev["data"]["dominant_color"].as_str())
                    .map(String::from);
                let reason = ev["data"]["ai_verification"]["reason"]
                    .as_str()
                    .or_else(|| ev["data"]["fp_reason"].as_str())
                    .map(String::from);
                let now = Utc::now().timestamp_millis() as f64 / 1000.0;
                if !camera.is_empty() && !label.is_empty() {
                    tx.execute(
                        r#"
                        INSERT INTO false_positive_signature (
                            camera, label, norm_x1, norm_y1, norm_x2, norm_y2,
                            dhash64, dominant_color, hit_count, source_event_id,
                            reason, created_at, last_seen_at
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?11)
                        ON CONFLICT(source_event_id) DO UPDATE SET
                            camera = excluded.camera,
                            label = excluded.label,
                            norm_x1 = excluded.norm_x1,
                            norm_y1 = excluded.norm_y1,
                            norm_x2 = excluded.norm_x2,
                            norm_y2 = excluded.norm_y2,
                            dhash64 = excluded.dhash64,
                            dominant_color = excluded.dominant_color,
                            reason = excluded.reason,
                            last_seen_at = excluded.last_seen_at
                        "#,
                        params![
                            camera,
                            label,
                            nx1,
                            ny1,
                            nx2,
                            ny2,
                            dhash64.map(|h| h as i64),
                            dominant_color,
                            id,
                            reason,
                            now
                        ],
                    )?;
                }
            }
        } else if !fp {
            tx.execute(
                "DELETE FROM false_positive_signature WHERE source_event_id = ?1",
                params![id],
            )?;
        }
        tx.commit()?;
        Ok(updated)
    }

    pub fn set_event_ai_verification(&self, id: &str, verification: &Value) -> Result<usize> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let Some(data_raw): Option<Option<String>> = tx
            .query_row("SELECT data FROM event WHERE id = ?1", params![id], |r| {
                Ok(r.get(0).ok())
            })
            .optional()?
        else {
            return Ok(0);
        };
        let mut data_val = parse_json_or(data_raw, json!({}));
        if !data_val.is_object() {
            data_val = json!({});
        }
        if let Some(data_obj) = data_val.as_object_mut() {
            data_obj.insert("ai_verification".to_string(), verification.clone());
        }
        let data_str = serde_json::to_string(&data_val)?;
        let updated = tx.execute(
            "UPDATE event SET data = ?1 WHERE id = ?2",
            params![data_str, id],
        )?;
        tx.commit()?;
        Ok(updated)
    }

    /// Matches `(camera, label)` against active `false_positive_signature` rows where normalized
    /// bounding-box IoU `>= 0.72` AND (if both `dhash64` values are present, Hamming distance `<= 12`).
    /// Increments `hit_count` and updates `last_seen_at` when matched.
    pub fn match_false_positive_signature(
        &self,
        camera: &str,
        label: &str,
        norm_box: [f64; 4],
        dhash64: Option<u64>,
    ) -> Result<Option<Value>> {
        if !norm_box.iter().all(|v| v.is_finite()) {
            return Ok(None);
        }
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            r#"
            SELECT
                id, camera, label, norm_x1, norm_y1, norm_x2, norm_y2,
                dhash64, dominant_color, hit_count, source_event_id,
                reason, created_at, last_seen_at
            FROM false_positive_signature
            WHERE camera = ?1 AND label = ?2
            ORDER BY id DESC
            "#,
        )?;
        type SigRow = (
            i64,
            String,
            String,
            f64,
            f64,
            f64,
            f64,
            Option<i64>,
            Option<String>,
            i64,
            Option<String>,
            Option<String>,
            f64,
            f64,
        );
        let rows = stmt.query_map(params![camera, label], |r| -> rusqlite::Result<SigRow> {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                value_to_f64(&r.get_ref(3)?).unwrap_or(0.0),
                value_to_f64(&r.get_ref(4)?).unwrap_or(0.0),
                value_to_f64(&r.get_ref(5)?).unwrap_or(0.0),
                value_to_f64(&r.get_ref(6)?).unwrap_or(0.0),
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                value_to_f64(&r.get_ref(12)?).unwrap_or(0.0),
                value_to_f64(&r.get_ref(13)?).unwrap_or(0.0),
            ))
        })?;

        let mut matched: Option<(SigRow, f64, Option<u32>)> = None;
        for row_res in rows {
            let row = row_res?;
            let sig_box = [row.3, row.4, row.5, row.6];
            let iou = compute_norm_box_iou(sig_box, norm_box);
            if iou < 0.72 {
                continue;
            }
            let sig_hash = row.7.map(|v| v as u64);
            let hamming = match (sig_hash, dhash64) {
                (Some(sh), Some(ch)) => {
                    let dist = (sh ^ ch).count_ones();
                    if dist > 12 {
                        continue;
                    }
                    Some(dist)
                }
                _ => None,
            };
            matched = Some((row, iou, hamming));
            break;
        }
        drop(stmt);

        if let Some((row, iou, hamming)) = matched {
            let new_hits = row.9.max(0).saturating_add(1);
            let now = Utc::now().timestamp_millis() as f64 / 1000.0;
            conn.execute(
                "UPDATE false_positive_signature SET hit_count = ?2, last_seen_at = ?3 WHERE id = ?1",
                params![row.0, new_hits, now],
            )?;
            let sig_hash_u64 = row.7.map(|v| v as u64);
            return Ok(Some(json!({
                "id": row.0,
                "camera": row.1,
                "label": row.2,
                "norm_bbox": [row.3, row.4, row.5, row.6],
                "norm_x1": row.3,
                "norm_y1": row.4,
                "norm_x2": row.5,
                "norm_y2": row.6,
                "dhash64": sig_hash_u64,
                "dominant_color": row.8,
                "hit_count": new_hits,
                "source_event_id": row.10,
                "reason": row.11,
                "created_at": row.12,
                "last_seen_at": now,
                "iou": iou,
                "hamming_distance": hamming
            })));
        }
        Ok(None)
    }

    pub fn list_false_positive_signatures(&self, camera: Option<&str>) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let filter_cam = camera.filter(|c| !c.is_empty() && *c != "all");
        let sql = if filter_cam.is_some() {
            r#"
            SELECT
                id, camera, label, norm_x1, norm_y1, norm_x2, norm_y2,
                dhash64, dominant_color, hit_count, source_event_id,
                reason, created_at, last_seen_at
            FROM false_positive_signature
            WHERE camera = ?1
            ORDER BY last_seen_at DESC, id DESC
            LIMIT 5000
            "#
        } else {
            r#"
            SELECT
                id, camera, label, norm_x1, norm_y1, norm_x2, norm_y2,
                dhash64, dominant_color, hit_count, source_event_id,
                reason, created_at, last_seen_at
            FROM false_positive_signature
            ORDER BY last_seen_at DESC, id DESC
            LIMIT 5000
            "#
        };
        let mut stmt = conn.prepare(sql)?;
        let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
            let nx1 = value_to_f64(&r.get_ref(3)?).unwrap_or(0.0);
            let ny1 = value_to_f64(&r.get_ref(4)?).unwrap_or(0.0);
            let nx2 = value_to_f64(&r.get_ref(5)?).unwrap_or(0.0);
            let ny2 = value_to_f64(&r.get_ref(6)?).unwrap_or(0.0);
            let dhash_i64: Option<i64> = r.get(7)?;
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "camera": r.get::<_, String>(1)?,
                "label": r.get::<_, String>(2)?,
                "norm_bbox": [nx1, ny1, nx2, ny2],
                "norm_x1": nx1,
                "norm_y1": ny1,
                "norm_x2": nx2,
                "norm_y2": ny2,
                "dhash64": dhash_i64.map(|v| v as u64),
                "dominant_color": r.get::<_, Option<String>>(8)?,
                "hit_count": r.get::<_, i64>(9)?,
                "source_event_id": r.get::<_, Option<String>>(10)?,
                "reason": r.get::<_, Option<String>>(11)?,
                "created_at": value_to_f64(&r.get_ref(12)?).unwrap_or(0.0),
                "last_seen_at": value_to_f64(&r.get_ref(13)?).unwrap_or(0.0),
            }))
        };
        let mut out = Vec::new();
        if let Some(cam) = filter_cam {
            let rows = stmt.query_map(params![cam], map_row)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let rows = stmt.query_map([], map_row)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn delete_false_positive_signature(&self, id: i64) -> Result<usize> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let src_ev: Option<String> = tx
            .query_row(
                "SELECT source_event_id FROM false_positive_signature WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let deleted = tx.execute(
            "DELETE FROM false_positive_signature WHERE id = ?1",
            params![id],
        )?;
        if let Some(ev_id) = src_ev {
            tx.execute(
                "UPDATE event SET false_positive = 0 WHERE id = ?1",
                params![ev_id],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    pub fn set_event_sub_label(&self, id: &str, sub_label: Option<&str>) -> Result<usize> {
        self.set_event_sub_label_with_score(id, sub_label, None)
    }

    fn set_event_sub_label_with_score_in_tx(
        tx: &Connection,
        id: &str,
        sub_label: Option<&str>,
        score: Option<f64>,
    ) -> Result<usize> {
        let normalized_sub = sub_label.map(str::trim).filter(|s| !s.is_empty());
        let finite_score = score.filter(|v| v.is_finite());
        let prev_sub_label: Option<String> = tx
            .query_row(
                "SELECT COALESCE(NULLIF(TRIM(sub_label), ''), json_extract(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.sub_label')) FROM event WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let updated = match (normalized_sub, finite_score) {
            (Some(sl), Some(sc)) => tx.execute(
                "UPDATE event SET sub_label = ?1, sub_label_score = ?2, data = json_set(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.sub_label', ?1, '$.sub_label_score', ?2) WHERE id = ?3",
                params![sl, sc, id],
            )?,
            (Some(sl), None) => tx.execute(
                "UPDATE event SET sub_label = ?1, data = json_set(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.sub_label', ?1) WHERE id = ?2",
                params![sl, id],
            )?,
            (None, _) => tx.execute(
                "UPDATE event SET sub_label = NULL, sub_label_score = NULL, data = json_remove(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.sub_label', '$.sub_label_score') WHERE id = ?1",
                params![id],
            )?,
        };

        // Propagate sub_label & sub_label_score to any reviewsegment referencing this event ID
        let clean_id = id.trim();
        if clean_id.is_empty() || clean_id.contains('"') {
            return Ok(updated);
        }
        let like_pattern = format!("%\"{}\"%", escape_sql_like(clean_id));
        let mut stmt =
            tx.prepare("SELECT id, data FROM reviewsegment WHERE data LIKE ?1 ESCAPE '\\'")?;
        let rev_rows: Vec<(String, String)> = stmt
            .query_map(params![like_pattern], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })?
            .flatten()
            .collect();
        drop(stmt);

        for (rev_id, raw_data) in rev_rows {
            let mut data_val = parse_json_or(Some(raw_data), json!({}));
            let Some(obj) = data_val.as_object_mut() else {
                continue;
            };
            let det_ids: Vec<String> = obj
                .get("detections")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if !det_ids.iter().any(|d| d == id) {
                continue;
            }
            let mut sub_labels: Vec<String> = obj
                .get("sub_labels")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();

            // If this event previously had a sub_label that changed or was cleared,
            // remove it unless another sibling detection in the same reviewsegment still carries it.
            if let Some(ref old_sl) = prev_sub_label
                && !normalized_sub.is_some_and(|ns| ns.eq_ignore_ascii_case(old_sl))
            {
                let sibling_still_has_old =
                    det_ids
                        .iter()
                        .filter(|d| d.as_str() != id)
                        .any(|sibling_id| {
                            tx.query_row(
                                "SELECT COALESCE(NULLIF(TRIM(sub_label), ''), json_extract(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.sub_label')) FROM event WHERE id = ?1",
                                params![sibling_id],
                                |r| r.get::<_, Option<String>>(0),
                            )
                            .ok()
                            .flatten()
                            .is_some_and(|s| s.trim().eq_ignore_ascii_case(old_sl))
                        });
                if !sibling_still_has_old {
                    sub_labels.retain(|x| !x.trim().eq_ignore_ascii_case(old_sl));
                }
            } else if normalized_sub.is_none() && det_ids.len() == 1 {
                sub_labels.clear();
            }

            if let Some(sl) = normalized_sub {
                if !sub_labels.iter().any(|x| x.trim().eq_ignore_ascii_case(sl)) {
                    sub_labels.push(sl.to_string());
                }
                obj.insert("sub_labels".to_string(), json!(sub_labels));
                if let Some(sc) = finite_score {
                    obj.insert("sub_label_score".to_string(), json!(sc));
                }
            } else {
                obj.insert("sub_labels".to_string(), json!(sub_labels));
                if sub_labels.is_empty() {
                    obj.remove("sub_label_score");
                }
            }
            if let Ok(new_data) = serde_json::to_string(&data_val) {
                tx.execute(
                    "UPDATE reviewsegment SET data = ?1 WHERE id = ?2",
                    params![new_data, rev_id],
                )?;
            }
        }

        Ok(updated)
    }

    pub fn set_event_sub_label_with_score(
        &self,
        id: &str,
        sub_label: Option<&str>,
        score: Option<f64>,
    ) -> Result<usize> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let updated = Self::set_event_sub_label_with_score_in_tx(&tx, id, sub_label, score)?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn list_distinct_sub_labels(&self) -> Result<Vec<String>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT sub_label FROM event WHERE sub_label IS NOT NULL AND TRIM(sub_label) != '' ORDER BY sub_label ASC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Clears `sub_label`, `sub_label_score`, and `data.sub_label` on all events matching
    /// `profile_name` case-insensitively, and removes the deleted profile's `sub_label` from
    /// all referencing `reviewsegment` rows.
    pub fn clear_face_profile_sub_labels(&self, profile_name: &str) -> Result<usize> {
        let clean = profile_name.trim();
        if clean.is_empty() {
            return Ok(0);
        }

        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let matching_event_ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM event
                 WHERE LOWER(TRIM(COALESCE(sub_label, ''))) = LOWER(TRIM(?1))
                    OR LOWER(TRIM(COALESCE(json_extract(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.sub_label'), ''))) = LOWER(TRIM(?1))",
            )?;
            let rows = stmt.query_map(params![clean], |r| r.get::<_, String>(0))?;
            rows.flatten().collect()
        };

        let mut cleared_events = 0usize;
        for ev_id in &matching_event_ids {
            cleared_events += Self::set_event_sub_label_with_score_in_tx(&tx, ev_id, None, None)?;
        }

        // Also scrub any reviewsegment rows whose `data.sub_labels` still contains `clean` case-insensitively
        {
            let like_pattern = format!(
                "%{}%",
                escape_sql_like(&clean.to_lowercase().replace('"', ""))
            );
            let mut stmt = tx.prepare(
                "SELECT id, data FROM reviewsegment WHERE LOWER(data) LIKE ?1 ESCAPE '\\'",
            )?;
            let rev_rows: Vec<(String, String)> = stmt
                .query_map(params![like_pattern], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    ))
                })?
                .flatten()
                .collect();
            drop(stmt);

            for (rev_id, raw_data) in rev_rows {
                let mut data_val = parse_json_or(Some(raw_data), json!({}));
                let Some(obj) = data_val.as_object_mut() else {
                    continue;
                };
                let Some(arr) = obj.get("sub_labels").and_then(|v| v.as_array()) else {
                    continue;
                };
                let prev_len = arr.len();
                let filtered: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .filter(|s| !s.trim().eq_ignore_ascii_case(clean))
                    .collect();
                if filtered.len() != prev_len {
                    if filtered.is_empty() {
                        obj.remove("sub_label_score");
                    }
                    obj.insert("sub_labels".to_string(), json!(filtered));
                    if let Ok(new_data) = serde_json::to_string(&data_val) {
                        tx.execute(
                            "UPDATE reviewsegment SET data = ?1 WHERE id = ?2",
                            params![new_data, rev_id],
                        )?;
                    }
                }
            }
        }

        tx.commit()?;
        Ok(cleared_events)
    }

    pub fn set_event_description(&self, id: &str, description: &str) -> Result<usize> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let Some(data_raw): Option<Option<String>> = tx
            .query_row("SELECT data FROM event WHERE id = ?1", params![id], |r| {
                Ok(r.get(0).ok())
            })
            .optional()?
        else {
            return Ok(0);
        };
        let mut data_val = parse_json_or(data_raw, json!({}));
        if !data_val.is_object() {
            data_val = json!({});
        }
        if let Some(data_obj) = data_val.as_object_mut() {
            data_obj.insert("description".to_string(), json!(description));
        }
        let data_str = serde_json::to_string(&data_val)?;
        let updated = tx.execute(
            "UPDATE event SET data = ?1 WHERE id = ?2",
            params![data_str, id],
        )?;
        tx.commit()?;
        Ok(updated)
    }

    pub fn set_event_notes(&self, id: &str, notes: &str) -> Result<usize> {
        let conn = self.lock_conn();
        Ok(conn.execute(
            "UPDATE event SET data = json_set(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.notes', ?1) WHERE id = ?2",
            params![notes, id],
        )?)
    }

    /// Manually sets or clears `data.recognized_license_plate` on an event (normalizing alphanumeric uppercase).
    pub fn set_event_license_plate(&self, id: &str, plate: Option<&str>) -> Result<usize> {
        let normalized: Option<String> = plate
            .map(|p| {
                p.chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .map(|c| c.to_ascii_uppercase())
                    .collect::<String>()
            })
            .filter(|s| !s.is_empty());

        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let Some(data_raw): Option<Option<String>> = tx
            .query_row("SELECT data FROM event WHERE id = ?1", params![id], |r| {
                Ok(r.get(0).ok())
            })
            .optional()?
        else {
            return Ok(0);
        };
        let mut data_val = parse_json_or(data_raw, json!({}));
        if !data_val.is_object() {
            data_val = json!({});
        }
        if let Some(data_obj) = data_val.as_object_mut() {
            let prev_plate = data_obj
                .get("recognized_license_plate")
                .and_then(|v| v.as_str())
                .map(|p| {
                    p.chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .map(|c| c.to_ascii_uppercase())
                        .collect::<String>()
                })
                .filter(|s| !s.is_empty());
            match normalized {
                Some(ref norm_plate) => {
                    data_obj.insert("recognized_license_plate".to_string(), json!(norm_plate));
                    if let Some(attrs) = data_obj
                        .get_mut("attributes")
                        .and_then(|v| v.as_array_mut())
                    {
                        if let Some(ref old_p) = prev_plate
                            && old_p != norm_plate
                        {
                            attrs.retain(|a| a.as_str() != Some(old_p.as_str()));
                        }
                        if !attrs
                            .iter()
                            .any(|a| a.as_str() == Some(norm_plate.as_str()))
                        {
                            attrs.push(json!(norm_plate));
                        }
                    }
                }
                None => {
                    data_obj.insert("recognized_license_plate".to_string(), Value::Null);
                    if let Some(ref old_p) = prev_plate
                        && let Some(attrs) = data_obj
                            .get_mut("attributes")
                            .and_then(|v| v.as_array_mut())
                    {
                        attrs.retain(|a| a.as_str() != Some(old_p.as_str()));
                    }
                }
            }
        }
        let data_str = serde_json::to_string(&data_val)?;
        let updated = tx.execute(
            "UPDATE event SET data = ?1 WHERE id = ?2",
            params![data_str, id],
        )?;
        tx.commit()?;
        Ok(updated)
    }

    /// Returns a sorted list of distinct non-empty `recognized_license_plate` values stored in `event.data`.
    pub fn query_recognized_license_plates(&self) -> Result<Vec<String>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT data FROM event WHERE data LIKE '%recognized_license_plate%' ORDER BY start_time DESC LIMIT 5000",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
        let mut set = std::collections::BTreeSet::new();
        for r in rows {
            if let Some(raw) = r?
                && let Ok(val) = serde_json::from_str::<Value>(&raw)
                && let Some(p) = val["recognized_license_plate"].as_str()
            {
                let norm: String = p
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .map(|c| c.to_ascii_uppercase())
                    .collect();
                if !norm.is_empty() {
                    set.insert(norm);
                }
            }
        }
        Ok(set.into_iter().collect())
    }

    pub fn set_review_notes(&self, id: &str, notes: &str) -> Result<usize> {
        let conn = self.lock_conn();
        Ok(conn.execute(
            "UPDATE reviewsegment SET data = json_set(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.notes', ?1) WHERE id = ?2",
            params![notes, id],
        )?)
    }

    /// Executes SQLite query-planner optimization and bounded WAL checkpointing (`PRAGMA optimize`
    /// and `PRAGMA wal_checkpoint(TRUNCATE)`).
    pub fn run_maintenance(&self) -> Result<()> {
        let conn = self.lock_conn();
        conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Returns total event counts grouped by `(camera, label)` for Prometheus `/api/metrics`.
    pub fn query_event_counts_by_camera_and_label(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT camera, label, COUNT(*) FROM event GROUP BY camera, label ORDER BY camera, label",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn upsert_event(&self, ev: &EventDto) -> Result<()> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let zones_str = serde_json::to_string(&ev.zones)?;
        let box_str = ev
            .box_coords
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let mut merged_data = if ev.data.is_object() {
            ev.data.clone()
        } else {
            json!({})
        };
        if let Some(obj) = merged_data.as_object_mut() {
            if ev.stationary
                && obj.get("stationary").is_none()
                && obj
                    .get("provenance")
                    .and_then(|p| p.get("stationary"))
                    .is_none()
            {
                obj.insert("stationary".to_string(), json!(true));
            }
            if let Ok(Some(existing_raw)) = tx
                .query_row(
                    "SELECT data FROM event WHERE id = ?1",
                    params![ev.id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .map(|opt| opt.flatten())
                && let Ok(Value::Object(prev_obj)) = serde_json::from_str::<Value>(&existing_raw)
            {
                for preserved_key in [
                    "ai_verification",
                    "suppressed_by_fp_signature",
                    "description",
                    "notes",
                    "sub_label",
                    "sub_label_score",
                    "recognized_license_plate",
                    "attributes",
                ] {
                    if obj.get(preserved_key).is_none_or(Value::is_null)
                        && let Some(prev_val) = prev_obj.get(preserved_key)
                        && !prev_val.is_null()
                    {
                        obj.insert(preserved_key.to_string(), prev_val.clone());
                    }
                }
            }
        }
        let data_str = serde_json::to_string(&merged_data)?;
        let sub_label_score = merged_data
            .get("sub_label_score")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite());

        let safe_start_time = if ev.start_time.is_finite() {
            ev.start_time
        } else {
            0.0
        };
        let safe_end_time = ev.end_time.filter(|v| v.is_finite());
        let safe_top_score = ev.top_score.filter(|v| v.is_finite());
        let safe_ratio = ev.ratio.map(|r| r as f64).filter(|v| v.is_finite());
        let safe_score = ev.score.map(|s| s as f64).filter(|v| v.is_finite());

        tx.execute(
            r#"
            INSERT INTO event (
                id, label, camera, start_time, end_time, top_score, false_positive,
                zones, thumbnail, has_clip, has_snapshot, box, retain_indefinitely,
                sub_label, sub_label_score, plus_id, data, area, ratio, score
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
            ON CONFLICT(id) DO UPDATE SET
                end_time = COALESCE(excluded.end_time, event.end_time),
                top_score = COALESCE(excluded.top_score, event.top_score),
                false_positive = CASE
                    WHEN event.false_positive = 1 OR excluded.false_positive = 1 THEN 1
                    ELSE COALESCE(excluded.false_positive, event.false_positive)
                END,
                retain_indefinitely = CASE
                    WHEN event.retain_indefinitely = 1 OR excluded.retain_indefinitely = 1 THEN 1
                    ELSE 0
                END,
                zones = excluded.zones,
                thumbnail = COALESCE(excluded.thumbnail, event.thumbnail),
                has_clip = excluded.has_clip,
                has_snapshot = excluded.has_snapshot,
                box = excluded.box,
                sub_label = COALESCE(excluded.sub_label, event.sub_label),
                sub_label_score = COALESCE(excluded.sub_label_score, event.sub_label_score),
                data = excluded.data,
                area = COALESCE(excluded.area, event.area),
                ratio = COALESCE(excluded.ratio, event.ratio),
                score = COALESCE(excluded.score, event.score)
            "#,
            params![
                ev.id,
                ev.label,
                ev.camera,
                safe_start_time,
                safe_end_time,
                safe_top_score,
                ev.false_positive.map(|b| if b { 1 } else { 0 }),
                zones_str,
                ev.thumbnail,
                if ev.has_clip { 1 } else { 0 },
                if ev.has_snapshot { 1 } else { 0 },
                box_str,
                if ev.retain_indefinitely { 1 } else { 0 },
                ev.sub_label,
                sub_label_score,
                ev.plus_id,
                data_str,
                ev.area,
                safe_ratio,
                safe_score
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_event(&self, id: &str) -> Result<usize> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let count = tx.execute("DELETE FROM event WHERE id = ?1", params![id])?;
        if count > 0 {
            tx.execute(
                "UPDATE share_link SET revoked = 1 WHERE target_id = ?1",
                params![id],
            )?;
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn delete_events_bulk(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut count = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM event WHERE id = ?1")?;
            let mut rev_share =
                tx.prepare("UPDATE share_link SET revoked = 1 WHERE target_id = ?1")?;
            for id in ids {
                let removed = stmt.execute(params![id])?;
                if removed > 0 {
                    rev_share.execute(params![id])?;
                }
                count += removed;
            }
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn query_recordings(
        &self,
        camera: &str,
        after: f64,
        before: f64,
    ) -> Result<Vec<RecordingRow>> {
        if !after.is_finite() || !before.is_finite() {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            r#"
            SELECT
                id, camera, path, start_time, end_time, duration,
                COALESCE(motion, 0), COALESCE(objects, 0), COALESCE(dBFS, 0),
                segment_size, COALESCE(regions, 0), motion_heatmap
            FROM recordings
            WHERE camera = ?1 AND end_time >= ?2 AND start_time <= ?3
            ORDER BY start_time ASC
            LIMIT 50000
            "#,
        )?;

        let rows = stmt.query_map(params![camera, after, before], |r| {
            let start_time = value_to_f64(&r.get_ref(3)?).unwrap_or(0.0);
            let end_time = value_to_f64(&r.get_ref(4)?).unwrap_or(0.0);
            let duration = value_to_f64(&r.get_ref(5)?).unwrap_or(0.0);
            let segment_size = value_to_f64(&r.get_ref(9)?).unwrap_or(0.0);
            let hm_raw: Option<String> = r.get(11).ok();
            let motion_heatmap = hm_raw.and_then(|s| serde_json::from_str(&s).ok());
            Ok(RecordingRow {
                id: r.get(0)?,
                camera: r.get(1)?,
                path: r.get(2)?,
                start_time,
                end_time,
                duration,
                motion: r.get(6)?,
                objects: r.get(7)?,
                dbfs: r.get(8)?,
                segment_size,
                regions: r.get(10)?,
                motion_heatmap,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn query_all_recordings_summary(&self) -> Result<Value> {
        let conn = self.lock_conn();
        let cutoff = (Utc::now().timestamp() - 86400 * 60) as f64;
        let max_valid_ts = 253_402_300_800.0f64;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT CAST(start_time / 86400 AS INTEGER) * 86400 FROM recordings WHERE typeof(start_time) IN ('real', 'integer') AND start_time >= ?1 AND start_time <= ?2 ORDER BY start_time DESC",
        )?;
        let rows = stmt.query_map(params![cutoff, max_valid_ts], |r| {
            Ok(value_to_f64(&r.get_ref(0)?))
        })?;
        let mut map = serde_json::Map::new();
        for r in rows {
            if let Some(day_ts_f) = r?
                && day_ts_f.is_finite()
                && let Some(dt) = Utc.timestamp_opt(day_ts_f as i64, 0).single()
            {
                map.insert(dt.format("%Y-%m-%d").to_string(), json!(true));
            }
        }
        Ok(Value::Object(map))
    }

    pub fn query_recordings_storage_usage(&self) -> Result<Value> {
        let conn = self.lock_conn();
        let cutoff = (Utc::now().timestamp() - 86400) as f64;
        let mut stmt = conn.prepare(
            r#"
            SELECT
                camera,
                COALESCE(SUM(CASE WHEN typeof(segment_size) IN ('real', 'integer') AND segment_size > 0.0 AND segment_size < 1.0e12 THEN segment_size ELSE 0.0 END), 0.0),
                COALESCE(SUM(CASE WHEN typeof(duration) IN ('real', 'integer') AND duration > 0.0 AND duration < 1.0e12 THEN duration ELSE 0.0 END), 0.0),
                COALESCE(SUM(CASE WHEN start_time >= ?1 AND typeof(segment_size) IN ('real', 'integer') AND segment_size > 0.0 AND segment_size < 1.0e12 THEN segment_size ELSE 0.0 END), 0.0),
                COALESCE(SUM(CASE WHEN start_time >= ?1 AND typeof(duration) IN ('real', 'integer') AND duration > 0.0 AND duration < 1.0e12 THEN duration ELSE 0.0 END), 0.0)
            FROM recordings
            GROUP BY camera
            "#,
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok((
                r.get::<_, String>(0)?,
                value_to_f64(&r.get_ref(1)?).unwrap_or(0.0),
                value_to_f64(&r.get_ref(2)?).unwrap_or(0.0),
                value_to_f64(&r.get_ref(3)?).unwrap_or(0.0),
                value_to_f64(&r.get_ref(4)?).unwrap_or(0.0),
            ))
        })?;

        let mut out = serde_json::Map::new();
        for r in rows {
            let (cam, raw_total_mb, raw_dur_s, raw_24h_mb, raw_24h_dur_s) = r?;
            let total_mb = if raw_total_mb.is_finite() && raw_total_mb > 0.0 {
                raw_total_mb
            } else {
                0.0
            };
            let dur_s = if raw_dur_s.is_finite() && raw_dur_s > 0.0 {
                raw_dur_s
            } else {
                0.0
            };
            let rolling_24h_mb = if raw_24h_mb.is_finite() && raw_24h_mb > 0.0 {
                raw_24h_mb
            } else {
                0.0
            };
            let rolling_24h_dur_s = if raw_24h_dur_s.is_finite() && raw_24h_dur_s > 0.0 {
                raw_24h_dur_s
            } else {
                0.0
            };
            let raw_bw = if rolling_24h_dur_s > 0.0 {
                (rolling_24h_mb / (rolling_24h_dur_s / 3600.0) * 100.0).round() / 100.0
            } else if dur_s > 0.0 {
                (total_mb / (dur_s / 3600.0) * 100.0).round() / 100.0
            } else {
                0.0
            };
            let bw_mb_hr = if raw_bw.is_finite() && raw_bw > 0.0 {
                raw_bw
            } else {
                0.0
            };
            let usage_rounded = (total_mb * 100.0).round() / 100.0;
            out.insert(
                cam,
                json!({
                    "usage": if usage_rounded.is_finite() { usage_rounded } else { 0.0 },
                    "usage_percent": 0.0,
                    "bandwidth": bw_mb_hr
                }),
            );
        }
        Ok(Value::Object(out))
    }

    pub fn query_motion_activity(
        &self,
        cameras: Option<&[String]>,
        after: f64,
        before: f64,
    ) -> Result<Vec<Value>> {
        if !after.is_finite() || !before.is_finite() {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn();
        let mut bind_vals: Vec<rusqlite::types::Value> = vec![
            rusqlite::types::Value::Real(after),
            rusqlite::types::Value::Real(before),
        ];
        let mut cam_clause = String::new();
        if let Some(cams) = cameras
            && !cams.is_empty()
            && !cams.iter().any(|c| c == "all")
        {
            let mut placeholders = Vec::with_capacity(cams.len());
            for c in cams {
                bind_vals.push(rusqlite::types::Value::Text(c.clone()));
                placeholders.push(format!("?{}", bind_vals.len()));
            }
            cam_clause = format!(" AND camera IN ({})", placeholders.join(","));
        }
        let sql = format!(
            r#"
            SELECT start_time, COALESCE(motion, 0), camera
            FROM recordings
            WHERE end_time >= ?1 AND start_time <= ?2 {cam_clause}
            ORDER BY start_time ASC
            LIMIT 2000
            "#
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind_vals.iter()), |r| {
            Ok(json!({
                "start_time": value_to_f64(&r.get_ref(0)?).unwrap_or(0.0),
                "motion": r.get::<_, i64>(1).unwrap_or(0),
                "camera": r.get::<_, String>(2)?
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn query_recordings_summary(&self, camera: &str) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let cutoff = (Utc::now().timestamp() - 86400 * 30) as f64;
        let mut stmt = conn.prepare(
            r#"
            SELECT start_time, duration, COALESCE(motion, 0), COALESCE(objects, 0), COALESCE(dbfs, 0)
            FROM recordings
            WHERE camera = ?1 AND start_time >= ?2
            ORDER BY start_time DESC
            "#,
        )?;
        let rows = stmt.query_map(params![camera, cutoff], |r| {
            Ok((
                value_to_f64(&r.get_ref(0)?).unwrap_or(0.0),
                value_to_f64(&r.get_ref(1)?).unwrap_or(0.0),
                r.get::<_, i64>(2).unwrap_or(0),
                r.get::<_, i64>(3).unwrap_or(0),
                r.get::<_, i64>(4).unwrap_or(0),
            ))
        })?;

        #[derive(Default)]
        struct HourAgg {
            duration: f64,
            motion: i64,
            objects: i64,
            dbfs: i64,
            events: i64,
            start_ts: i64,
            end_ts: i64,
        }
        let mut day_hours: BTreeMap<String, BTreeMap<String, HourAgg>> = BTreeMap::new();

        for r in rows {
            let (ts, raw_dur, motion, objs, dbfs) = r?;
            if !ts.is_finite() {
                continue;
            }
            let dur = if raw_dur.is_finite() && raw_dur > 0.0 {
                raw_dur
            } else {
                0.0
            };
            if let Some(dt) = Local.timestamp_opt(ts as i64, 0).single() {
                let day = dt.format("%Y-%m-%d").to_string();
                let hour = dt.format("%H").to_string();
                let seg_start = ts as i64;
                let seg_end_f = (ts + dur).ceil();
                let seg_end = if seg_end_f.is_finite() {
                    seg_end_f as i64
                } else {
                    seg_start
                };
                let h = day_hours.entry(day).or_default().entry(hour).or_default();
                h.duration += dur;
                h.motion = h.motion.saturating_add(motion.max(0));
                h.objects = h.objects.saturating_add(objs.max(0));
                if dbfs != 0 && (h.dbfs == 0 || dbfs > h.dbfs) {
                    h.dbfs = dbfs;
                }
                if h.start_ts == 0 || seg_start < h.start_ts {
                    h.start_ts = seg_start;
                }
                if seg_end > h.end_ts {
                    h.end_ts = seg_end;
                }
            }
        }

        let mut ev_stmt = conn.prepare(
            r#"
            SELECT start_time FROM event WHERE camera = ?1 AND start_time >= ?2
            "#,
        )?;
        let ev_rows = ev_stmt.query_map(params![camera, cutoff], |r| {
            Ok(value_to_f64(&r.get_ref(0)?).unwrap_or(0.0))
        })?;
        for r in ev_rows {
            let ts = r?;
            if !ts.is_finite() {
                continue;
            }
            if let Some(dt) = Local.timestamp_opt(ts as i64, 0).single() {
                let day = dt.format("%Y-%m-%d").to_string();
                let hour = dt.format("%H").to_string();
                if let Some(hours_map) = day_hours.get_mut(&day)
                    && let Some(h) = hours_map.get_mut(&hour)
                {
                    h.events = h.events.saturating_add(1);
                }
            }
        }

        let mut summary = Vec::new();
        for (day, hours_map) in day_hours.into_iter().rev() {
            let mut total_events = 0i64;
            let mut total_motion = 0i64;
            let mut total_duration = 0i64;
            let mut day_start = i64::MAX;
            let mut day_end = 0i64;
            let mut hours_vec = Vec::new();
            for (hour, agg) in hours_map.into_iter().rev() {
                total_events = total_events.saturating_add(agg.events);
                total_motion = total_motion.saturating_add(agg.motion);
                let dur_i64 = if agg.duration.is_finite() && agg.duration > 0.0 {
                    agg.duration.round() as i64
                } else {
                    0
                };
                total_duration = total_duration.saturating_add(dur_i64);
                if agg.start_ts > 0 && agg.start_ts < day_start {
                    day_start = agg.start_ts;
                }
                if agg.end_ts > day_end {
                    day_end = agg.end_ts;
                }
                hours_vec.push(json!({
                    "hour": hour,
                    "events": agg.events,
                    "motion": agg.motion,
                    "objects": agg.objects,
                    "dbfs": agg.dbfs,
                    "duration": dur_i64,
                    "start_ts": agg.start_ts,
                    "end_ts": agg.end_ts
                }));
            }
            summary.push(json!({
                "day": day,
                "events": total_events,
                "motion": total_motion,
                "duration": total_duration,
                "start_ts": if day_start == i64::MAX { 0 } else { day_start },
                "end_ts": day_end,
                "hours": hours_vec
            }));
        }

        Ok(summary)
    }

    pub fn insert_recording(&self, rec: &RecordingRow) -> Result<()> {
        let conn = self.lock_conn();
        let hm_str = rec
            .motion_heatmap
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let safe_start = if rec.start_time.is_finite() {
            rec.start_time
        } else {
            0.0
        };
        let safe_end = if rec.end_time.is_finite() {
            rec.end_time
        } else {
            safe_start
        };
        let safe_dur = if rec.duration.is_finite() && rec.duration >= 0.0 {
            rec.duration
        } else {
            0.0
        };
        let safe_size = if rec.segment_size.is_finite() && rec.segment_size >= 0.0 {
            rec.segment_size
        } else {
            0.0
        };
        conn.execute(
            r#"
            INSERT INTO recordings (
                id, camera, path, start_time, end_time, duration,
                objects, motion, segment_size, dBFS, regions, motion_heatmap
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ON CONFLICT(path) DO UPDATE SET
                end_time = excluded.end_time,
                duration = excluded.duration,
                objects = excluded.objects,
                motion = excluded.motion,
                segment_size = excluded.segment_size
            "#,
            params![
                rec.id,
                rec.camera,
                rec.path,
                safe_start,
                safe_end,
                safe_dur,
                rec.objects,
                rec.motion,
                safe_size,
                rec.dbfs,
                rec.regions,
                hm_str
            ],
        )?;
        Ok(())
    }

    pub fn delete_expired_recordings(
        &self,
        camera: &str,
        continuous_before_ts: f64,
        event_before_ts: f64,
    ) -> Result<Vec<String>> {
        self.delete_expired_recordings_with_window(
            camera,
            continuous_before_ts,
            continuous_before_ts,
            event_before_ts,
            0.0,
            0.0,
        )
    }

    pub fn delete_expired_recordings_with_window(
        &self,
        camera: &str,
        continuous_before_ts: f64,
        motion_before_ts: f64,
        event_before_ts: f64,
        pre_capture_sec: f64,
        post_capture_sec: f64,
    ) -> Result<Vec<String>> {
        if !continuous_before_ts.is_finite()
            || !motion_before_ts.is_finite()
            || !event_before_ts.is_finite()
            || !pre_capture_sec.is_finite()
            || !post_capture_sec.is_finite()
        {
            return Ok(Vec::new());
        }
        let pre_sec = pre_capture_sec.max(0.0);
        let post_sec = post_capture_sec.max(0.0);
        let max_cutoff_ts = continuous_before_ts
            .max(motion_before_ts)
            .max(event_before_ts);
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut ids = Vec::new();
        let mut paths = Vec::new();
        {
            let mut stmt = tx.prepare(
                r#"
                SELECT r.id, r.path
                FROM recordings r
                WHERE r.camera = ?1
                  AND r.end_time < ?7
                  AND NOT EXISTS (
                    SELECT 1 FROM event e
                    WHERE e.camera = r.camera
                      AND e.retain_indefinitely = 1
                      AND e.start_time <= (r.end_time + ?4)
                      AND COALESCE(e.end_time, e.start_time + 30.0) >= (r.start_time - ?5)
                  )
                  AND (
                    r.end_time < ?3
                    OR (
                      COALESCE(r.objects, 0) = 0
                      AND (
                        (COALESCE(r.motion, 0) = 0 AND r.end_time < ?2)
                        OR r.end_time < ?6
                      )
                      AND (
                        (?4 <= 0.0 AND ?5 <= 0.0)
                        OR (
                          NOT EXISTS (
                            SELECT 1 FROM event e
                            WHERE e.camera = r.camera
                              AND e.start_time >= (r.start_time - ?5 - 86400.0)
                              AND e.start_time <= (r.end_time + ?4)
                              AND COALESCE(e.end_time, e.start_time + 15.0) >= (r.start_time - ?5)
                          )
                          AND NOT EXISTS (
                            SELECT 1 FROM reviewsegment rv
                            WHERE rv.camera = r.camera
                              AND rv.start_time >= (r.start_time - ?5 - 86400.0)
                              AND rv.start_time <= (r.end_time + ?4)
                              AND COALESCE(rv.end_time, rv.start_time + 15.0) >= (r.start_time - ?5)
                          )
                          AND NOT EXISTS (
                            SELECT 1 FROM recordings r2
                            WHERE r2.camera = r.camera
                              AND r2.start_time >= (r.start_time - ?5 - 300.0)
                              AND r2.start_time <= (r.end_time + ?4)
                              AND r2.end_time >= (r.start_time - ?5)
                              AND COALESCE(r2.objects, 0) > 0
                              AND r2.id != r.id
                          )
                        )
                      )
                    )
                  )
                ORDER BY r.end_time ASC
                LIMIT 500
                "#,
            )?;
            let rows = stmt.query_map(
                params![
                    camera,
                    continuous_before_ts,
                    event_before_ts,
                    pre_sec,
                    post_sec,
                    motion_before_ts,
                    max_cutoff_ts
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            for r in rows {
                let (id, path) = r?;
                ids.push(id);
                paths.push(path);
            }
        }

        if !ids.is_empty() {
            let mut del_stmt = tx.prepare("DELETE FROM recordings WHERE id = ?1")?;
            for id in &ids {
                del_stmt.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(paths)
    }

    pub fn list_cold_compaction_candidates(
        &self,
        camera: &str,
        before_ts: f64,
        idle_only: bool,
        pre_capture_sec: f64,
        post_capture_sec: f64,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if limit == 0
            || !before_ts.is_finite()
            || !pre_capture_sec.is_finite()
            || !post_capture_sec.is_finite()
        {
            return Ok(Vec::new());
        }
        let pre_sec = pre_capture_sec.max(0.0);
        let post_sec = post_capture_sec.max(0.0);
        let capped_limit = limit.min(10_000) as i64;
        let conn = self.lock_conn();
        let sql = if idle_only {
            r#"
            SELECT r.id, r.path
            FROM recordings r
            WHERE r.camera = ?1
              AND r.end_time < ?2
              AND COALESCE(r.stream_tier, 'main') != 'compacted'
              AND COALESCE(r.motion, 0) = 0
              AND COALESCE(r.objects, 0) = 0
              AND NOT EXISTS (
                SELECT 1 FROM event e
                WHERE e.camera = r.camera
                  AND e.start_time >= (r.start_time - ?4 - 86400.0)
                  AND e.start_time <= (r.end_time + ?3)
                  AND COALESCE(e.end_time, e.start_time + 15.0) >= (r.start_time - ?4)
              )
              AND NOT EXISTS (
                SELECT 1 FROM reviewsegment rv
                WHERE rv.camera = r.camera
                  AND rv.start_time >= (r.start_time - ?4 - 86400.0)
                  AND rv.start_time <= (r.end_time + ?3)
                  AND COALESCE(rv.end_time, rv.start_time + 15.0) >= (r.start_time - ?4)
              )
            ORDER BY r.end_time ASC
            LIMIT ?5
            "#
        } else {
            r#"
            SELECT r.id, r.path
            FROM recordings r
            WHERE r.camera = ?1
              AND r.end_time < ?2
              AND COALESCE(r.stream_tier, 'main') != 'compacted'
              AND (?3 >= 0.0 AND ?4 >= 0.0)
            ORDER BY r.end_time ASC
            LIMIT ?5
            "#
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(
            params![camera, before_ts, pre_sec, post_sec, capped_limit],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_recording_compacted(&self, id: &str, new_size_mb: f64) -> Result<()> {
        let safe_size = if new_size_mb.is_finite() && new_size_mb >= 0.0 {
            new_size_mb
        } else {
            0.0
        };
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE recordings SET segment_size = ?2, stream_tier = 'compacted' WHERE id = ?1",
            params![id, safe_size],
        )?;
        Ok(())
    }

    /// Deletes recording rows for `camera` overlapping `[after, before]` in a single transaction
    /// and returns `(id, path, segment_size_mb)` for each deleted segment.
    pub fn delete_recordings_in_range(
        &self,
        camera: &str,
        after: f64,
        before: f64,
    ) -> Result<Vec<(String, String, f64)>> {
        if !after.is_finite() || !before.is_finite() {
            return Ok(Vec::new());
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut matched = Vec::new();
        {
            let mut stmt = tx.prepare(
                r#"
                SELECT id, path, COALESCE(segment_size, 0.0)
                FROM recordings
                WHERE camera = ?1 AND end_time >= ?2 AND start_time <= ?3
                "#,
            )?;
            let rows = stmt.query_map(params![camera, after, before], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    value_to_f64(&r.get_ref(2)?).unwrap_or(0.0).max(0.0),
                ))
            })?;
            for r in rows {
                matched.push(r?);
            }
        }
        if !matched.is_empty() {
            let mut del_stmt = tx.prepare("DELETE FROM recordings WHERE id = ?1")?;
            for (id, _, _) in &matched {
                del_stmt.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(matched)
    }

    /// Scans all rows in `recordings` and removes any entries whose on-disk file no longer exists.
    pub fn prune_missing_recording_files<F>(&self, file_exists: F) -> Result<usize>
    where
        F: Fn(&str) -> bool,
    {
        let candidates: Vec<(String, String)> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare("SELECT id, path FROM recordings")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut list = Vec::new();
            for r in rows {
                list.push(r?);
            }
            list
        };

        let mut orphan_entries = Vec::new();
        for (id, path) in candidates {
            if !file_exists(&path) {
                orphan_entries.push((id, path));
            }
        }

        if orphan_entries.is_empty() {
            return Ok(0);
        }

        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut deleted = 0usize;
        {
            let mut del_stmt = tx.prepare("DELETE FROM recordings WHERE id = ?1 AND path = ?2")?;
            for (id, path) in &orphan_entries {
                deleted += del_stmt.execute(params![id, path])?;
            }
        }
        tx.commit()?;
        Ok(deleted)
    }

    /// Lists up to `limit` recording segments with `start_time < before_ts` whose `path` matches `hot_prefix_like`
    /// so the Hot-to-Cold storage tier migration worker can move them to the cold archive tier.
    pub fn list_recordings_for_tier_migration(
        &self,
        before_ts: f64,
        hot_prefix_like: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if limit == 0 || !before_ts.is_finite() {
            return Ok(Vec::new());
        }
        let capped_limit = limit.min(10_000) as i64;
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, path
            FROM recordings
            WHERE start_time < ?1 AND path LIKE ?2
            ORDER BY start_time ASC
            LIMIT ?3
            "#,
        )?;
        let rows = stmt.query_map(params![before_ts, hot_prefix_like, capped_limit], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Updates `recordings.path` atomically after migrating a segment file to the cold archive tier.
    pub fn update_recording_path(&self, id: &str, new_path: &str) -> Result<bool> {
        let conn = self.lock_conn();
        let rows = conn.execute(
            "UPDATE recordings SET path = ?2 WHERE id = ?1",
            params![id, new_path],
        )?;
        Ok(rows > 0)
    }

    /// Evicts the oldest recording segments for `camera` until total `segment_size` (in MB)
    /// is at or below `max_size_mb`, returning the `path` of each evicted segment.
    pub fn delete_recordings_over_quota_mb(
        &self,
        camera: &str,
        max_size_mb: f64,
    ) -> Result<Vec<String>> {
        if !max_size_mb.is_finite() || max_size_mb <= 0.0 {
            return Ok(Vec::new());
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let total_mb: f64 = tx
            .query_row(
                "SELECT COALESCE(SUM(CASE WHEN typeof(segment_size) IN ('real', 'integer') AND segment_size > 0.0 AND segment_size < 1.0e12 THEN segment_size ELSE 0.0 END), 0.0) FROM recordings WHERE camera = ?1",
                params![camera],
                |r| r.get(0),
            )
            .unwrap_or(0.0);
        if !total_mb.is_finite() || total_mb <= max_size_mb {
            return Ok(Vec::new());
        }
        let excess_mb = total_mb - max_size_mb;
        let mut to_delete = Vec::new();
        let mut accumulated_mb = 0.0;
        {
            let mut stmt = tx.prepare(
                r#"
                SELECT r.id, r.path, COALESCE(r.segment_size, 0.0)
                FROM recordings r
                WHERE r.camera = ?1
                  AND NOT EXISTS (
                      SELECT 1 FROM event e
                      WHERE e.camera = r.camera
                        AND e.retain_indefinitely = 1
                        AND e.start_time <= r.end_time
                        AND COALESCE(e.end_time, e.start_time + 30.0) >= r.start_time
                  )
                ORDER BY r.start_time ASC
                "#,
            )?;
            let rows = stmt.query_map(params![camera], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    value_to_f64(&r.get_ref(2)?).unwrap_or(0.0),
                ))
            })?;
            for r in rows {
                let (id, path, sz) = r?;
                let safe_sz = if sz.is_finite() && sz > 0.0 {
                    sz
                } else {
                    0.001
                };
                to_delete.push((id, path));
                accumulated_mb += safe_sz.max(0.001);
                if accumulated_mb >= excess_mb {
                    break;
                }
            }
        }
        let mut paths = Vec::new();
        if !to_delete.is_empty() {
            let mut del_stmt = tx.prepare("DELETE FROM recordings WHERE id = ?1")?;
            for (id, path) in to_delete {
                del_stmt.execute(params![id])?;
                paths.push(path);
            }
        }
        tx.commit()?;
        Ok(paths)
    }

    pub fn delete_global_recordings_over_quota_mb(&self, max_size_mb: f64) -> Result<Vec<String>> {
        if !max_size_mb.is_finite() || max_size_mb <= 0.0 {
            return Ok(Vec::new());
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let total_mb: f64 = tx
            .query_row(
                "SELECT COALESCE(SUM(CASE WHEN typeof(segment_size) IN ('real', 'integer') AND segment_size > 0.0 AND segment_size < 1.0e12 THEN segment_size ELSE 0.0 END), 0.0) FROM recordings",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0.0);
        if !total_mb.is_finite() || total_mb <= max_size_mb {
            return Ok(Vec::new());
        }
        let excess_mb = total_mb - max_size_mb;
        let mut to_delete = Vec::new();
        let mut accumulated_mb = 0.0;
        {
            let mut stmt = tx.prepare(
                r#"
                SELECT r.id, r.path, COALESCE(r.segment_size, 0.0)
                FROM recordings r
                WHERE NOT EXISTS (
                    SELECT 1 FROM event e
                    WHERE e.camera = r.camera
                      AND e.retain_indefinitely = 1
                      AND e.start_time <= r.end_time
                      AND COALESCE(e.end_time, e.start_time + 30.0) >= r.start_time
                )
                ORDER BY r.start_time ASC
                "#,
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    value_to_f64(&r.get_ref(2)?).unwrap_or(0.0),
                ))
            })?;
            for r in rows {
                let (id, path, sz) = r?;
                let safe_sz = if sz.is_finite() && sz > 0.0 {
                    sz
                } else {
                    0.001
                };
                to_delete.push((id, path));
                accumulated_mb += safe_sz.max(0.001);
                if accumulated_mb >= excess_mb {
                    break;
                }
            }
        }
        let mut paths = Vec::new();
        if !to_delete.is_empty() {
            let mut del_stmt = tx.prepare("DELETE FROM recordings WHERE id = ?1")?;
            for (id, path) in to_delete {
                del_stmt.execute(params![id])?;
                paths.push(path);
            }
        }
        tx.commit()?;
        Ok(paths)
    }

    pub fn query_timeline(
        &self,
        camera: Option<&str>,
        source_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn();
        let mut sql = String::from(
            "SELECT timestamp, camera, source, source_id, class_type, data FROM timeline WHERE 1=1",
        );
        let mut bind_vals: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(cam) = camera
            && !cam.is_empty()
            && cam != "all"
        {
            bind_vals.push(rusqlite::types::Value::Text(cam.to_string()));
            sql.push_str(&format!(" AND camera = ?{}", bind_vals.len()));
        }
        if let Some(sid) = source_id
            && !sid.is_empty()
        {
            let mut placeholders = Vec::new();
            for s in sid.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                bind_vals.push(rusqlite::types::Value::Text(s.to_string()));
                placeholders.push(format!("?{}", bind_vals.len()));
            }
            if !placeholders.is_empty() {
                sql.push_str(&format!(" AND source_id IN ({})", placeholders.join(",")));
            }
        }
        let lim = limit.min(1000) as i64;
        bind_vals.push(rusqlite::types::Value::Integer(lim));
        sql.push_str(&format!(
            " ORDER BY timestamp ASC LIMIT ?{}",
            bind_vals.len()
        ));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind_vals.iter()), |r| {
            let ts = value_to_f64(&r.get_ref(0)?).unwrap_or(0.0);
            let data_raw: Option<String> = r.get(5).ok();
            Ok(json!({
                "timestamp": ts,
                "camera": r.get::<_, String>(1)?,
                "source": r.get::<_, String>(2)?,
                "source_id": r.get::<_, Option<String>>(3).ok().flatten(),
                "class_type": r.get::<_, String>(4)?,
                "data": parse_json_or(data_raw, json!({}))
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn insert_timeline_entry(
        &self,
        timestamp: f64,
        camera: &str,
        source: &str,
        source_id: Option<&str>,
        class_type: &str,
        data: &Value,
    ) -> Result<()> {
        let conn = self.lock_conn();
        let data_str = serde_json::to_string(data)?;
        conn.execute(
            r#"
            INSERT INTO timeline (timestamp, camera, source, source_id, class_type, data)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![timestamp, camera, source, source_id, class_type, data_str],
        )?;
        Ok(())
    }

    pub fn query_exports(&self) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, camera, name, date, video_path, thumb_path, in_progress, export_case_id FROM export ORDER BY date DESC LIMIT 5000",
        )?;
        let rows = stmt.query_map([], |r| {
            let date = value_to_f64(&r.get_ref(3)?).unwrap_or(0.0);
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "camera": r.get::<_, String>(1)?,
                "name": r.get::<_, String>(2)?,
                "date": date,
                "video_path": r.get::<_, String>(4)?,
                "thumb_path": r.get::<_, String>(5)?,
                "in_progress": r.get::<_, i64>(6).unwrap_or(0) != 0,
                "export_case_id": r.get::<_, Option<String>>(7).ok().flatten()
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_exports(&self) -> Result<Vec<Value>> {
        self.query_exports()
    }

    pub fn get_export(&self, id: &str) -> Result<Option<Value>> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT id, camera, name, date, video_path, thumb_path, in_progress, export_case_id FROM export WHERE id = ?1",
                params![id],
                |r| {
                    let date = value_to_f64(&r.get_ref(3)?).unwrap_or(0.0);
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "camera": r.get::<_, String>(1)?,
                        "name": r.get::<_, String>(2)?,
                        "date": date,
                        "video_path": r.get::<_, String>(4)?,
                        "thumb_path": r.get::<_, String>(5)?,
                        "in_progress": r.get::<_, i64>(6).unwrap_or(0) != 0,
                        "export_case_id": r.get::<_, Option<String>>(7).ok().flatten()
                    }))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_export_by_id(&self, id: &str) -> Result<Option<Value>> {
        self.get_export(id)
    }

    fn map_share_link_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "kind": r.get::<_, String>(1)?,
            "target_id": r.get::<_, String>(2)?,
            "camera": r.get::<_, Option<String>>(3).ok().flatten(),
            "created_by": r.get::<_, String>(4)?,
            "created_at": value_to_f64(&r.get_ref(5)?).unwrap_or(0.0),
            "expires_at": value_to_f64(&r.get_ref(6)?).unwrap_or(0.0),
            "revoked": r.get::<_, i64>(7).unwrap_or(1) != 0,
            "access_count": r.get::<_, i64>(8).unwrap_or(0),
            "last_accessed_at": value_to_f64(&r.get_ref(9)?),
            "max_views": r.get::<_, Option<i64>>(10).ok().flatten(),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_share_link(
        &self,
        id: &str,
        kind: &str,
        target_id: &str,
        camera: Option<&str>,
        created_by: &str,
        created_at: f64,
        expires_at: f64,
    ) -> Result<Value> {
        self.insert_share_link_with_options(
            id, kind, target_id, camera, created_by, created_at, expires_at, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_share_link_with_options(
        &self,
        id: &str,
        kind: &str,
        target_id: &str,
        camera: Option<&str>,
        created_by: &str,
        created_at: f64,
        expires_at: f64,
        max_views: Option<u64>,
    ) -> Result<Value> {
        if !created_at.is_finite() || !expires_at.is_finite() {
            anyhow::bail!("Share link timestamps must be finite");
        }
        let max_views_i64 = max_views.map(|v| v.min(i64::MAX as u64) as i64);
        let conn = self.lock_conn();
        conn.execute(
            r#"
            INSERT OR REPLACE INTO share_link (
                id, kind, target_id, camera, created_by, created_at, expires_at,
                revoked, access_count, last_accessed_at, max_views
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, NULL, ?8)
            "#,
            params![
                id,
                kind,
                target_id,
                camera,
                created_by,
                created_at,
                expires_at,
                max_views_i64
            ],
        )?;
        Ok(json!({
            "id": id,
            "kind": kind,
            "target_id": target_id,
            "camera": camera,
            "created_by": created_by,
            "created_at": created_at,
            "expires_at": expires_at,
            "revoked": false,
            "access_count": 0,
            "last_accessed_at": Value::Null,
            "max_views": max_views_i64
        }))
    }

    pub fn get_share_link(&self, id: &str) -> Result<Option<Value>> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                r#"
                SELECT id, kind, target_id, camera, created_by, created_at, expires_at,
                       revoked, access_count, last_accessed_at, max_views
                FROM share_link
                WHERE id = ?1
                "#,
                params![id],
                Self::map_share_link_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_share_links(&self) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, kind, target_id, camera, created_by, created_at, expires_at,
                   revoked, access_count, last_accessed_at, max_views
            FROM share_link
            ORDER BY created_at DESC
            LIMIT 500
            "#,
        )?;
        let rows = stmt.query_map([], Self::map_share_link_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn record_share_link_access(&self, id: &str, accessed_at: f64) -> Result<bool> {
        if !accessed_at.is_finite() {
            return Ok(false);
        }
        let conn = self.lock_conn();
        let updated = conn.execute(
            r#"
            UPDATE share_link
            SET access_count = access_count + 1,
                last_accessed_at = ?2
            WHERE id = ?1
              AND typeof(revoked) = 'integer' AND revoked = 0
              AND typeof(expires_at) IN ('real', 'integer') AND expires_at < 1.0e300 AND expires_at > ?2
              AND typeof(access_count) = 'integer' AND access_count >= 0
              AND (max_views IS NULL OR (typeof(max_views) = 'integer' AND max_views > 0 AND access_count < max_views))
            "#,
            params![id, accessed_at],
        )?;
        Ok(updated > 0)
    }

    pub fn touch_share_link_last_accessed(&self, id: &str, accessed_at: f64) -> Result<bool> {
        if !accessed_at.is_finite() {
            return Ok(false);
        }
        let conn = self.lock_conn();
        let updated = conn.execute(
            r#"
            UPDATE share_link
            SET last_accessed_at = CASE
                WHEN last_accessed_at IS NULL OR last_accessed_at < ?2 THEN ?2
                ELSE last_accessed_at
            END
            WHERE id = ?1
              AND typeof(revoked) = 'integer' AND revoked = 0
              AND typeof(expires_at) IN ('real', 'integer') AND expires_at < 1.0e300 AND expires_at > ?2
            "#,
            params![id, accessed_at],
        )?;
        Ok(updated > 0)
    }

    pub fn prune_expired_share_links(&self, before_ts: f64) -> Result<usize> {
        if !before_ts.is_finite() {
            return Ok(0);
        }
        let conn = self.lock_conn();
        let deleted = conn.execute(
            "DELETE FROM share_link WHERE expires_at <= ?1",
            params![before_ts],
        )?;
        Ok(deleted)
    }

    pub fn list_expired_or_revoked_share_link_targets(
        &self,
        before_ts: f64,
    ) -> Result<Vec<String>> {
        if !before_ts.is_finite() {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            r#"
            SELECT DISTINCT target_id
            FROM share_link
            WHERE expires_at <= ?1
               OR revoked != 0
               OR (max_views IS NOT NULL AND access_count >= max_views)
            LIMIT 5000
            "#,
        )?;
        let rows = stmt.query_map(params![before_ts], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn has_active_share_links_for_target(&self, target_id: &str, now_ts: f64) -> Result<bool> {
        if !now_ts.is_finite() {
            return Ok(false);
        }
        let conn = self.lock_conn();
        let count: i64 = conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM share_link
            WHERE target_id = ?1
              AND typeof(revoked) = 'integer' AND revoked = 0
              AND typeof(expires_at) IN ('real', 'integer') AND expires_at < 1.0e300 AND expires_at > ?2
              AND typeof(access_count) = 'integer' AND access_count >= 0
              AND (max_views IS NULL OR (typeof(max_views) = 'integer' AND max_views > 0 AND access_count < max_views))
            "#,
            params![target_id, now_ts],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn has_recently_accessed_share_links_for_target(
        &self,
        target_id: &str,
        now_ts: f64,
        grace_sec: f64,
    ) -> Result<bool> {
        if !now_ts.is_finite() || !grace_sec.is_finite() || grace_sec <= 0.0 {
            return Ok(false);
        }
        let conn = self.lock_conn();
        let count: i64 = conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM share_link
            WHERE target_id = ?1
              AND typeof(revoked) = 'integer' AND revoked = 0
              AND typeof(expires_at) IN ('real', 'integer') AND expires_at < 1.0e300 AND expires_at > ?2
              AND typeof(last_accessed_at) IN ('real', 'integer') AND last_accessed_at < 1.0e300
              AND last_accessed_at + ?3 > ?2
            "#,
            params![target_id, now_ts, grace_sec],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn revoke_share_link(&self, id: &str) -> Result<bool> {
        let conn = self.lock_conn();
        let updated = conn.execute(
            "UPDATE share_link SET revoked = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(updated > 0)
    }

    pub fn revoke_share_links_for_target(
        &self,
        kind: Option<&str>,
        target_id: &str,
    ) -> Result<usize> {
        let conn = self.lock_conn();
        let updated = match kind {
            Some(k) => conn.execute(
                "UPDATE share_link SET revoked = 1 WHERE kind = ?1 AND target_id = ?2",
                params![k, target_id],
            )?,
            None => conn.execute(
                "UPDATE share_link SET revoked = 1 WHERE target_id = ?1",
                params![target_id],
            )?,
        };
        Ok(updated)
    }

    pub fn revoke_share_links_by_creator(&self, created_by: &str) -> Result<usize> {
        let trimmed = created_by.trim();
        let ext_creator = format!("ext:{trimmed}");
        let conn = self.lock_conn();
        let updated = conn.execute(
            "UPDATE share_link SET revoked = 1 WHERE created_by = ?1 OR created_by = ?2",
            params![trimmed, ext_creator],
        )?;
        Ok(updated)
    }

    pub fn is_share_link_revoked(&self, id: &str) -> Result<bool> {
        let conn = self.lock_conn();
        let revoked: Option<i64> = conn
            .query_row(
                "SELECT revoked FROM share_link WHERE id = ?1",
                params![id],
                |r| Ok(r.get::<_, i64>(0).unwrap_or(1)),
            )
            .optional()?;
        Ok(revoked.unwrap_or(0) != 0)
    }

    pub fn is_share_link_active(&self, id: &str, now_ts: f64) -> Result<bool> {
        if !now_ts.is_finite() {
            return Ok(false);
        }
        let conn = self.lock_conn();
        let row: Option<(i64, Option<f64>)> = conn
            .query_row(
                "SELECT revoked, expires_at FROM share_link WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0).unwrap_or(1),
                        value_to_f64(&r.get_ref(1)?),
                    ))
                },
            )
            .optional()?;
        Ok(row.is_some_and(|(revoked, expires_at)| {
            revoked == 0 && expires_at.is_some_and(|exp| exp > now_ts)
        }))
    }

    pub fn insert_export(&self, exp: &ExportRow) -> Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            r#"
            INSERT INTO export (id, camera, name, date, video_path, thumb_path, in_progress)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                video_path = excluded.video_path,
                thumb_path = excluded.thumb_path,
                in_progress = excluded.in_progress
            "#,
            params![
                exp.id,
                exp.camera,
                exp.name,
                exp.date,
                exp.video_path,
                exp.thumb_path,
                if exp.in_progress { 1 } else { 0 }
            ],
        )?;
        Ok(())
    }

    pub fn rename_export(&self, id: &str, new_name: &str) -> Result<usize> {
        let conn = self.lock_conn();
        Ok(conn.execute(
            "UPDATE export SET name = ?1 WHERE id = ?2",
            params![new_name, id],
        )?)
    }

    pub fn delete_export(&self, id: &str) -> Result<Option<(String, String)>> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let paths: Option<(String, String)> = tx
            .query_row(
                "SELECT video_path, thumb_path FROM export WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if paths.is_some() {
            tx.execute("DELETE FROM export WHERE id = ?1", params![id])?;
            tx.execute(
                "UPDATE share_link SET revoked = 1 WHERE kind = 'export' AND target_id = ?1",
                params![id],
            )?;
        }
        tx.commit()?;
        Ok(paths)
    }

    pub fn delete_exports_bulk(&self, ids: &[String]) -> Result<Vec<(String, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut deleted_paths = Vec::new();
        {
            let mut sel = tx.prepare("SELECT video_path, thumb_path FROM export WHERE id = ?1")?;
            let mut del = tx.prepare("DELETE FROM export WHERE id = ?1")?;
            let mut rev_share = tx.prepare(
                "UPDATE share_link SET revoked = 1 WHERE kind = 'export' AND target_id = ?1",
            )?;
            for id in ids {
                let paths: Option<(String, String)> = sel
                    .query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                if let Some(p) = paths {
                    del.execute(params![id])?;
                    rev_share.execute(params![id])?;
                    deleted_paths.push(p);
                }
            }
        }
        tx.commit()?;
        Ok(deleted_paths)
    }

    pub fn create_case(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
        event_ids: &[String],
        export_ids: &[String],
    ) -> Result<Value> {
        let now = Utc::now().timestamp() as f64;
        let events_json = serde_json::to_string(event_ids)?;
        let exports_json = serde_json::to_string(export_ids)?;
        let conn = self.lock_conn();
        conn.execute(
            r#"
            INSERT INTO exportcase (id, name, description, created_at, updated_at, event_ids, export_ids)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                updated_at = excluded.updated_at,
                event_ids = excluded.event_ids,
                export_ids = excluded.export_ids
            "#,
            params![id, name, description, now, now, events_json, exports_json],
        )?;
        Ok(json!({
            "id": id,
            "name": name,
            "description": description,
            "created_at": now,
            "updated_at": now,
            "event_ids": event_ids,
            "export_ids": export_ids
        }))
    }

    pub fn query_cases(&self) -> Result<Vec<Value>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at, updated_at, COALESCE(event_ids, '[]'), COALESCE(export_ids, '[]') FROM exportcase ORDER BY created_at DESC LIMIT 1000",
        )?;
        let rows = stmt.query_map([], |r| {
            let ev_raw: Option<String> = r.get(5).ok();
            let ex_raw: Option<String> = r.get(6).ok();
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "description": r.get::<_, Option<String>>(2).ok().flatten(),
                "created_at": value_to_f64(&r.get_ref(3)?).unwrap_or(0.0),
                "updated_at": value_to_f64(&r.get_ref(4)?).unwrap_or(0.0),
                "event_ids": parse_json_or(ev_raw, json!([])),
                "export_ids": parse_json_or(ex_raw, json!([]))
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_cases(&self) -> Result<Vec<Value>> {
        self.query_cases()
    }

    pub fn get_case(&self, id: &str) -> Result<Option<Value>> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT id, name, description, created_at, updated_at, COALESCE(event_ids, '[]'), COALESCE(export_ids, '[]') FROM exportcase WHERE id = ?1",
                params![id],
                |r| {
                    let ev_raw: Option<String> = r.get(5).ok();
                    let ex_raw: Option<String> = r.get(6).ok();
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "name": r.get::<_, String>(1)?,
                        "description": r.get::<_, Option<String>>(2).ok().flatten(),
                        "created_at": value_to_f64(&r.get_ref(3)?).unwrap_or(0.0),
                        "updated_at": value_to_f64(&r.get_ref(4)?).unwrap_or(0.0),
                        "event_ids": parse_json_or(ev_raw, json!([])),
                        "export_ids": parse_json_or(ex_raw, json!([]))
                    }))
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn query_previews(&self, camera: &str, after: f64, before: f64) -> Result<Vec<Value>> {
        if !after.is_finite() || !before.is_finite() {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            r#"
            SELECT camera, path, start_time, end_time, duration
            FROM previews
            WHERE (?1 = 'all' OR camera = ?1) AND end_time >= ?2 AND start_time <= ?3
            ORDER BY start_time ASC
            LIMIT 10000
            "#,
        )?;
        let rows = stmt.query_map(params![camera, after, before], |r| {
            let raw_path: String = r.get(1)?;
            let src = strip_media_root(&raw_path)
                .map(|rel| format!("/{rel}"))
                .unwrap_or_else(|| raw_path.clone());
            Ok(json!({
                "camera": r.get::<_, String>(0)?,
                "src": src,
                "type": "video/mp4",
                "start": value_to_f64(&r.get_ref(2)?).unwrap_or(0.0),
                "end": value_to_f64(&r.get_ref(3)?).unwrap_or(0.0),
                "duration": value_to_f64(&r.get_ref(4)?).unwrap_or(0.0)
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn insert_preview(
        &self,
        id: &str,
        camera: &str,
        path: &str,
        start_time: f64,
        end_time: f64,
        duration: f64,
    ) -> Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            r#"
            INSERT INTO previews (id, camera, path, start_time, end_time, duration)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET
                path = excluded.path,
                start_time = excluded.start_time,
                end_time = excluded.end_time,
                duration = excluded.duration
            "#,
            params![id, camera, path, start_time, end_time, duration],
        )?;
        Ok(())
    }

    pub fn last_preview_end_time(&self, camera: &str) -> Result<Option<f64>> {
        let conn = self.lock_conn();
        let val: Option<f64> = conn
            .query_row(
                "SELECT MAX(end_time) FROM previews WHERE camera = ?1",
                params![camera],
                |r| Ok(value_to_f64(&r.get_ref(0)?)),
            )
            .optional()?
            .flatten();
        Ok(val)
    }

    pub fn cleanup_expired_events_and_reviews(
        &self,
        camera: &str,
        alerts_before_ts: f64,
        detections_before_ts: f64,
    ) -> Result<Vec<String>> {
        if !alerts_before_ts.is_finite() || !detections_before_ts.is_finite() {
            return Ok(Vec::new());
        }
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut files_to_remove = Vec::new();
        let is_safe_id = |id: &str| -> bool {
            !id.is_empty()
                && !id.contains("..")
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        };
        let is_safe_path = |p: &str| -> bool { !p.is_empty() && !p.contains("..") };
        let safe_camera = is_safe_id(camera);

        // 1. Expired review segments
        {
            let mut stmt = tx.prepare(
                r#"
                SELECT id, thumb_path
                FROM reviewsegment
                WHERE camera = ?1
                  AND end_time IS NOT NULL
                  AND (
                    (severity = 'alert' AND end_time < ?2)
                    OR (severity != 'alert' AND end_time < ?3)
                  )
                LIMIT 500
                "#,
            )?;
            let rows = stmt.query_map(
                params![camera, alerts_before_ts, detections_before_ts],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            let mut ids = Vec::new();
            for r in rows {
                let (id, thumb) = r?;
                ids.push(id);
                if is_safe_path(&thumb) {
                    files_to_remove.push(thumb);
                }
            }
            drop(stmt);
            for id in ids {
                tx.execute(
                    "DELETE FROM userreviewstatus WHERE review_segment_id = ?1",
                    params![id],
                )?;
                tx.execute("DELETE FROM reviewsegment WHERE id = ?1", params![id])?;
                tx.execute(
                    "UPDATE share_link SET revoked = 1 WHERE target_id = ?1",
                    params![id],
                )?;
                if is_safe_id(&id) {
                    files_to_remove
                        .push(format!("/media/ninjanvr/exports/share_redacted_{id}.mp4"));
                    files_to_remove.push(format!(
                        "/media/ninjanvr/exports/.share_redacted_{id}.mask_hash"
                    ));
                    files_to_remove.push(format!("/media/ninjanvr/exports/share_clip_{id}.mp4"));
                    files_to_remove.push(format!("/media/ninjanvr/clips/share_redacted_{id}.webp"));
                    files_to_remove.push(format!("/media/ninjanvr/clips/share_clip_{id}.webp"));
                }
            }
        }

        // 2. Expired events (excluding retain_indefinitely = 1), respecting alert vs detection cutoffs
        let min_cutoff = alerts_before_ts.min(detections_before_ts);
        {
            let mut stmt = tx.prepare(
                r#"
                SELECT id
                FROM event
                WHERE camera = ?1
                  AND COALESCE(retain_indefinitely, 0) = 0
                  AND end_time IS NOT NULL
                  AND (
                    (COALESCE(json_extract(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.max_severity'), 'alert') = 'alert' AND end_time < ?2)
                    OR (COALESCE(json_extract(CASE WHEN json_valid(data) AND json_type(data) = 'object' THEN data ELSE '{}' END, '$.max_severity'), 'alert') != 'alert' AND end_time < ?3)
                  )
                LIMIT 500
                "#,
            )?;
            let ids: Vec<String> = stmt
                .query_map(
                    params![camera, alerts_before_ts, detections_before_ts],
                    |r| r.get(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);
            for id in ids {
                if is_safe_id(&id) {
                    files_to_remove.push(format!("/media/ninjanvr/clips/{id}.jpg"));
                    if safe_camera {
                        files_to_remove.push(format!("/media/ninjanvr/clips/{camera}-{id}.webp"));
                        files_to_remove
                            .push(format!("/media/ninjanvr/clips/{camera}-{id}-clean.webp"));
                    }
                    files_to_remove
                        .push(format!("/media/ninjanvr/exports/share_redacted_{id}.mp4"));
                    files_to_remove.push(format!(
                        "/media/ninjanvr/exports/.share_redacted_{id}.mask_hash"
                    ));
                    files_to_remove.push(format!("/media/ninjanvr/exports/share_clip_{id}.mp4"));
                    files_to_remove.push(format!("/media/ninjanvr/clips/share_redacted_{id}.webp"));
                    files_to_remove.push(format!("/media/ninjanvr/clips/share_clip_{id}.webp"));
                }
                tx.execute(
                    "UPDATE share_link SET revoked = 1 WHERE target_id = ?1",
                    params![id],
                )?;
                tx.execute("DELETE FROM event WHERE id = ?1", params![id])?;
            }
        }

        // 3. Expired timeline & previews
        tx.execute(
            "DELETE FROM timeline WHERE camera = ?1 AND timestamp < ?2",
            params![camera, min_cutoff],
        )?;
        {
            let mut stmt = tx.prepare(
                "SELECT id, path FROM previews WHERE camera = ?1 AND end_time < ?2 LIMIT 200",
            )?;
            let rows = stmt.query_map(params![camera, min_cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut pids = Vec::new();
            for r in rows {
                let (pid, ppath) = r?;
                pids.push(pid);
                if is_safe_path(&ppath) {
                    files_to_remove.push(ppath);
                }
            }
            drop(stmt);
            for pid in pids {
                tx.execute("DELETE FROM previews WHERE id = ?1", params![pid])?;
            }
        }

        tx.commit()?;
        Ok(files_to_remove)
    }

    pub fn upsert_dataset_sample(&self, sample: &DatasetSampleDto) -> Result<DatasetSampleDto> {
        let split = if sample.split == "train" || sample.split == "val" {
            sample.split.clone()
        } else {
            deterministic_dataset_split(&sample.id).to_string()
        };
        let sanitized_anns = sanitize_annotation_boxes(&sample.annotations);
        let annotations_json = serde_json::to_string(&sanitized_anns)?;
        let verified_int = if sample.verified { 1 } else { 0 };
        let night_int = if sample.night_mode { 1 } else { 0 };
        let created_at = if sample.created_at.is_finite() {
            sample.created_at
        } else {
            0.0
        };
        let conn = self.lock_conn();
        conn.execute(
            r#"
            INSERT INTO dataset_sample (
                id, event_id, camera, split, sample_kind, image_path,
                width, height, annotations, verified, night_mode, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ON CONFLICT(id) DO UPDATE SET
                event_id = COALESCE(excluded.event_id, dataset_sample.event_id),
                camera = excluded.camera,
                split = excluded.split,
                sample_kind = excluded.sample_kind,
                image_path = excluded.image_path,
                width = CASE WHEN excluded.width > 0 THEN excluded.width ELSE dataset_sample.width END,
                height = CASE WHEN excluded.height > 0 THEN excluded.height ELSE dataset_sample.height END,
                annotations = excluded.annotations,
                verified = excluded.verified,
                night_mode = excluded.night_mode
            "#,
            params![
                sample.id,
                sample.event_id,
                sample.camera,
                split,
                sample.sample_kind,
                sample.image_path,
                sample.width as i64,
                sample.height as i64,
                annotations_json,
                verified_int,
                night_int,
                created_at
            ],
        )?;
        let mut out = sample.clone();
        out.annotations = sanitized_anns;
        out.split = split;
        out.created_at = created_at;
        Ok(out)
    }

    fn map_dataset_sample_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DatasetSampleDto> {
        let raw_ann: String = r.get(8)?;
        let annotations: Vec<DatasetAnnotationBox> =
            serde_json::from_str(&raw_ann).unwrap_or_default();
        Ok(DatasetSampleDto {
            id: r.get(0)?,
            event_id: r.get(1)?,
            camera: r.get(2)?,
            split: r.get(3)?,
            sample_kind: r.get(4)?,
            image_path: r.get(5)?,
            width: r.get::<_, i64>(6)?.clamp(0, u32::MAX as i64) as u32,
            height: r.get::<_, i64>(7)?.clamp(0, u32::MAX as i64) as u32,
            annotations,
            verified: r.get::<_, i64>(9)? != 0,
            night_mode: r.get::<_, i64>(10)? != 0,
            created_at: value_to_f64(&r.get_ref(11)?).unwrap_or(0.0),
        })
    }

    pub fn list_dataset_samples(
        &self,
        camera: Option<&str>,
        sample_kind: Option<&str>,
        verified: Option<bool>,
    ) -> Result<Vec<DatasetSampleDto>> {
        let conn = self.lock_conn();
        let mut sql = String::from(
            r#"
            SELECT
                id, event_id, camera, split, sample_kind, image_path,
                width, height, annotations, verified, night_mode, created_at
            FROM dataset_sample
            WHERE 1=1
            "#,
        );
        let mut bind_vals: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(cam) = camera.filter(|c| !c.is_empty() && *c != "all") {
            bind_vals.push(rusqlite::types::Value::Text(cam.to_string()));
            sql.push_str(&format!(" AND camera = ?{}", bind_vals.len()));
        }
        if let Some(kind) = sample_kind.filter(|k| !k.is_empty() && *k != "all") {
            bind_vals.push(rusqlite::types::Value::Text(kind.to_string()));
            sql.push_str(&format!(" AND sample_kind = ?{}", bind_vals.len()));
        }
        if let Some(ver) = verified {
            bind_vals.push(rusqlite::types::Value::Integer(if ver { 1 } else { 0 }));
            sql.push_str(&format!(" AND verified = ?{}", bind_vals.len()));
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT 10000");

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(bind_vals.iter()),
            Self::map_dataset_sample_row,
        )?;
        let mut out = Vec::new();
        for row_res in rows {
            out.push(row_res?);
        }
        Ok(out)
    }

    pub fn get_dataset_sample(&self, id: &str) -> Result<Option<DatasetSampleDto>> {
        let conn = self.lock_conn();
        let sample = conn
            .query_row(
                r#"
                SELECT
                    id, event_id, camera, split, sample_kind, image_path,
                    width, height, annotations, verified, night_mode, created_at
                FROM dataset_sample
                WHERE id = ?1
                "#,
                params![id],
                Self::map_dataset_sample_row,
            )
            .optional()?;
        Ok(sample)
    }

    pub fn update_dataset_sample_annotations(
        &self,
        id: &str,
        annotations: Option<&[DatasetAnnotationBox]>,
        sample_kind: Option<&str>,
        verified: Option<bool>,
        split: Option<&str>,
    ) -> Result<Option<DatasetSampleDto>> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let Some(mut current) = tx
            .query_row(
                r#"
                SELECT
                    id, event_id, camera, split, sample_kind, image_path,
                    width, height, annotations, verified, night_mode, created_at
                FROM dataset_sample
                WHERE id = ?1
                "#,
                params![id],
                Self::map_dataset_sample_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        if let Some(anns) = annotations {
            current.annotations = sanitize_annotation_boxes(anns);
        }
        if let Some(kind) = sample_kind
            && !kind.trim().is_empty()
        {
            current.sample_kind = kind.trim().to_string();
        }
        if let Some(ver) = verified {
            current.verified = ver;
        }
        if let Some(sp) = split
            && (sp == "train" || sp == "val")
        {
            current.split = sp.to_string();
        }
        let annotations_json = serde_json::to_string(&current.annotations)?;
        let verified_int = if current.verified { 1 } else { 0 };
        tx.execute(
            r#"
            UPDATE dataset_sample
            SET annotations = ?2, sample_kind = ?3, verified = ?4, split = ?5
            WHERE id = ?1
            "#,
            params![
                id,
                annotations_json,
                current.sample_kind,
                verified_int,
                current.split
            ],
        )?;
        tx.commit()?;
        Ok(Some(current))
    }

    pub fn delete_dataset_sample(&self, id: &str) -> Result<usize> {
        let conn = self.lock_conn();
        Ok(conn.execute("DELETE FROM dataset_sample WHERE id = ?1", params![id])?)
    }

    pub fn get_dataset_summary(&self) -> Result<Value> {
        let samples = self.list_dataset_samples(None, None, None)?;
        let total_samples = samples.len();
        let mut true_positive_count = 0usize;
        let mut hard_negative_count = 0usize;
        let mut ai_annotated_count = 0usize;
        let mut train_count = 0usize;
        let mut val_count = 0usize;
        let mut verified_count = 0usize;
        let mut night_mode_count = 0usize;
        let mut total_annotations = 0usize;
        let mut by_camera: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_class: BTreeMap<String, usize> = BTreeMap::new();
        for &cls in &NINJANVR_SECURITY_CLASSES {
            by_class.insert(cls.to_string(), 0);
        }

        for s in &samples {
            match s.sample_kind.as_str() {
                "hard_negative" => hard_negative_count += 1,
                "ai_annotated" => ai_annotated_count += 1,
                _ => true_positive_count += 1,
            }
            if s.split == "val" {
                val_count += 1;
            } else {
                train_count += 1;
            }
            if s.verified {
                verified_count += 1;
            }
            if s.night_mode {
                night_mode_count += 1;
            }
            *by_camera.entry(s.camera.clone()).or_insert(0) += 1;
            total_annotations += s.annotations.len();
            for ann in &s.annotations {
                let cls_name = security_class_label(ann.class_id)
                    .map(String::from)
                    .unwrap_or_else(|| ann.label.clone());
                *by_class.entry(cls_name).or_insert(0) += 1;
            }
        }

        let positive_samples = true_positive_count + ai_annotated_count;
        let positive_ratio = if total_samples > 0 {
            positive_samples as f64 / total_samples as f64
        } else {
            0.0
        };
        let hard_negative_ratio = if total_samples > 0 {
            hard_negative_count as f64 / total_samples as f64
        } else {
            0.0
        };
        let unverified_count = total_samples.saturating_sub(verified_count);

        Ok(json!({
            "total_samples": total_samples,
            "true_positive_count": true_positive_count,
            "hard_negative_count": hard_negative_count,
            "ai_annotated_count": ai_annotated_count,
            "positive_samples": positive_samples,
            "train_count": train_count,
            "val_count": val_count,
            "verified_count": verified_count,
            "unverified_count": unverified_count,
            "night_mode_count": night_mode_count,
            "total_annotations": total_annotations,
            "positive_ratio": positive_ratio,
            "hard_negative_ratio": hard_negative_ratio,
            "target_positive_ratio": 0.8,
            "target_hard_negative_ratio": 0.2,
            "by_camera": by_camera,
            "by_class": by_class,
            "classes": NINJANVR_SECURITY_CLASSES,
            "ready_for_export": total_samples > 0
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_password_roundtrip_verifies_password_hash() {
        let hash = hash_password("my-secret-pass", Some("e1c16db9bfff2200"), 10_000);
        assert!(hash.starts_with("pbkdf2_sha256$10000$e1c16db9bfff2200$"));
        assert!(verify_password("my-secret-pass", &hash));
        assert!(!verify_password("wrong-pass", &hash));
    }

    #[test]
    fn sqlite_schema_and_queries_work() {
        let tmp = std::env::temp_dir().join(format!("ninjanvr-test-{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();
        let created = db.ensure_default_admin(1000).unwrap();
        assert!(created.is_some());

        let admin = db.get_user("admin").unwrap().unwrap();
        assert!(verify_password(
            created.as_ref().unwrap(),
            &admin.password_hash
        ));

        db.insert_recording(&RecordingRow {
            id: "1700000000.0-abc".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
            start_time: 1700000000.0,
            end_time: 1700000010.0,
            duration: 10.0,
            motion: 120,
            objects: 2,
            dbfs: 0,
            segment_size: 1.25,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        let recs = db
            .query_recordings("front_door", 1699999990.0, 1700000020.0)
            .unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].duration, 10.0);

        // Test orphan file pruning and manual time-range deletion
        db.insert_recording(&RecordingRow {
            id: "1700000020.0-orphan".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/2026-11-01/00/front_door/00.20.mp4".to_string(),
            start_time: 1700000020.0,
            end_time: 1700000030.0,
            duration: 10.0,
            motion: 0,
            objects: 0,
            dbfs: 0,
            segment_size: 1.0,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

        let removed_orphans = db
            .prune_missing_recording_files(|p| p.ends_with("00.00.mp4"))
            .unwrap();
        assert_eq!(removed_orphans, 1);

        let deleted_range = db
            .delete_recordings_in_range("front_door", 1699999990.0, 1700000015.0)
            .unwrap();
        assert_eq!(deleted_range.len(), 1);
        assert!(
            db.query_recordings("front_door", 1699999990.0, 1700000050.0)
                .unwrap()
                .is_empty()
        );

        // Test combined boolean EventFilter (exclude_labels NOT, zones, min_score)
        db.upsert_event(&EventDto {
            id: "ev_person_driveway".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["driveway"]),
            start_time: 1700000100.0,
            end_time: Some(1700000110.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.91),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"score": 0.91, "top_score": 0.91}),
            thumbnail: None,
            area: Some(15000),
            ratio: Some(0.5),
            score: Some(0.91),
            stationary: false,
        })
        .unwrap();
        db.upsert_event(&EventDto {
            id: "ev_cat_driveway".to_string(),
            camera: "front_door".to_string(),
            label: "cat".to_string(),
            zones: json!(["driveway"]),
            start_time: 1700000120.0,
            end_time: Some(1700000125.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.82),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"score": 0.82, "top_score": 0.82}),
            thumbnail: None,
            area: Some(4000),
            ratio: Some(1.0),
            score: Some(0.82),
            stationary: false,
        })
        .unwrap();

        let filtered_evs = db
            .query_events(&EventFilter {
                zones: Some(vec!["driveway".to_string()]),
                exclude_labels: Some(vec!["cat".to_string()]),
                min_score: Some(0.85),
                stationary: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered_evs.len(), 1);
        assert_eq!(filtered_evs[0]["id"], "ev_person_driveway");

        // Test disk quota eviction (oldest segments pruned first when total exceeds max_size_mb)
        for i in 0..4 {
            db.insert_recording(&RecordingRow {
                id: format!("quota_seg_{i}"),
                camera: "front_door".to_string(),
                path: format!("/media/ninjanvr/recordings/2026-11-01/01/front_door/00.{i}0.mp4"),
                start_time: 1700001000.0 + (i as f64) * 10.0,
                end_time: 1700001010.0 + (i as f64) * 10.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 100.0, // 4 * 100 MB = 400 MB total
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
        }
        let evicted = db
            .delete_recordings_over_quota_mb("front_door", 220.0)
            .unwrap();
        assert_eq!(evicted.len(), 2);
        assert!(evicted[0].ends_with("00.00.mp4"));
        assert!(evicted[1].ends_with("00.10.mp4"));

        // User management CRUD & last-admin guardrails
        assert!(db.delete_user("admin").is_err());
        assert!(db.update_user_role("admin", "viewer").is_err());

        db.create_user("operator1", "op-pass-123", "viewer", 1000)
            .unwrap();
        let op1 = db.get_user("operator1").unwrap().unwrap();
        assert_eq!(op1.role, "viewer");
        assert!(verify_password("op-pass-123", &op1.password_hash));

        assert!(
            db.update_user_password("operator1", "new-pass-456", 1000)
                .unwrap()
        );
        let op1_updated = db.get_user("operator1").unwrap().unwrap();
        assert!(verify_password("new-pass-456", &op1_updated.password_hash));

        assert!(db.update_user_role("operator1", "admin").unwrap());
        assert!(db.delete_user("operator1").unwrap());
        assert!(db.get_user("operator1").unwrap().is_none());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn retention_window_cleanup_uses_indexed_bounds_and_protects_pre_post_capture() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-retention-win-{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // Insert 3 segments around an event at [1010, 1020] with pre=15s, post=15s:
        // seg_far_old [900, 910] -> outside pre_capture window -> should be deleted
        // seg_pre     [998, 1008] -> within 15s pre_capture of event [1010, 1020] -> protected
        // seg_event   [1010, 1020] -> has objects=1 -> protected until event_before_ts
        // seg_post    [1022, 1032] -> within 15s post_capture of event [1010, 1020] -> protected
        for (id, start, end, objects) in [
            ("seg_far_old", 900.0, 910.0, 0),
            ("seg_pre", 998.0, 1008.0, 0),
            ("seg_event", 1010.0, 1020.0, 1),
            ("seg_post", 1022.0, 1032.0, 0),
        ] {
            db.insert_recording(&RecordingRow {
                id: id.to_string(),
                camera: "sala".to_string(),
                path: format!("/media/ninjanvr/recordings/{id}.mp4"),
                start_time: start,
                end_time: end,
                duration: 10.0,
                motion: 0,
                objects,
                dbfs: 0,
                segment_size: 1.0,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
        }

        let pruned = db
            .delete_expired_recordings_with_window(
                "sala", 2000.0, // continuous_before_ts
                2000.0, // motion_before_ts
                500.0,  // event_before_ts
                15.0,   // pre_capture_sec
                15.0,   // post_capture_sec
            )
            .unwrap();
        assert_eq!(pruned, vec!["/media/ninjanvr/recordings/seg_far_old.mp4"]);

        let remaining = db.query_recordings("sala", 800.0, 1100.0).unwrap();
        assert_eq!(remaining.len(), 3);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn review_segment_lifecycle_filters_notes_and_summary_work() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-review-lifecycle-{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        let now = Utc::now().timestamp() as f64;
        let t_recent_1 = now - 1800.0;
        let t_recent_2 = now - 900.0;
        let t_older_day = now - 3.0 * 86400.0;

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_alert_1".to_string(),
            camera: "front_door".to_string(),
            start_time: t_recent_1,
            end_time: Some(t_recent_1 + 15.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-rev_alert_1.webp".to_string(),
            data: json!({
                "detections": ["ev_101", "ev_102"],
                "objects": ["person-verified", "car"],
                "zones": ["driveway", "porch"],
                "audio": ["speech"],
                "provenance": {
                    "top_score": 0.94,
                    "stationary": false
                }
            }),
            has_been_reviewed: false,
        })
        .unwrap();

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_det_2".to_string(),
            camera: "backyard".to_string(),
            start_time: t_recent_2,
            end_time: Some(t_recent_2 + 10.0),
            severity: "detection".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-rev_det_2.webp".to_string(),
            data: json!({
                "detections": ["ev_202"],
                "objects": ["dog"],
                "zones": ["lawn"],
                "audio": ["bark"],
                "provenance": {
                    "top_score": 0.78,
                    "stationary": true
                }
            }),
            has_been_reviewed: false,
        })
        .unwrap();

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_old_alert_3".to_string(),
            camera: "front_door".to_string(),
            start_time: t_older_day,
            end_time: Some(t_older_day + 20.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-rev_old_alert_3.webp".to_string(),
            data: json!({
                "detections": ["ev_303"],
                "objects": ["car"],
                "zones": ["street"],
                "audio": [],
                "provenance": {
                    "top_score": 0.88,
                    "stationary": false
                }
            }),
            has_been_reviewed: false,
        })
        .unwrap();

        // Filter by camera
        let by_cam = db
            .query_reviews(&ReviewFilter {
                cameras: Some(vec!["front_door".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_cam.len(), 2);

        // Filter by label (matches "person-verified" via base label "person", and audio label "bark")
        let by_person = db
            .query_reviews(&ReviewFilter {
                labels: Some(vec!["person".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_person.len(), 1);
        assert_eq!(by_person[0].id, "rev_alert_1");

        let by_audio = db
            .query_reviews(&ReviewFilter {
                labels: Some(vec!["bark".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_audio.len(), 1);
        assert_eq!(by_audio[0].id, "rev_det_2");

        // Filter by exclude_labels, zones, severity, min_score, stationary, before/after
        let filtered = db
            .query_reviews(&ReviewFilter {
                cameras: Some(vec!["front_door".to_string()]),
                labels: Some(vec!["car".to_string()]),
                exclude_labels: Some(vec!["dog".to_string()]),
                zones: Some(vec!["driveway".to_string()]),
                severity: Some("alert".to_string()),
                reviewed: Some(false),
                min_score: Some(0.90),
                stationary: Some(false),
                after: Some(t_recent_1 - 5.0),
                before: Some(t_recent_1 + 5.0),
                limit: Some(10),
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "rev_alert_1");

        // Direct lookups: get_review_by_id, get_review_by_event_id, get_reviews_by_ids, query_review_ids
        let one = db.get_review_by_id("rev_alert_1").unwrap().unwrap();
        assert_eq!(one.camera, "front_door");
        assert!(!one.has_been_reviewed);
        assert!(db.get_review_by_id("non_existent").unwrap().is_none());

        let by_ev = db.get_review_by_event_id("ev_102").unwrap().unwrap();
        assert_eq!(by_ev.id, "rev_alert_1");
        assert!(db.get_review_by_event_id("ev_999").unwrap().is_none());

        let batch_ids = vec![
            "rev_alert_1".to_string(),
            "rev_det_2".to_string(),
            "missing".to_string(),
        ];
        let by_ids = db.get_reviews_by_ids(&batch_ids).unwrap();
        assert_eq!(by_ids.len(), 2);
        let queried_ids = db.query_review_ids(&batch_ids).unwrap();
        assert_eq!(queried_ids.len(), 2);

        // Mark viewed (true) and unview (false), and set_review_notes
        assert_eq!(
            db.mark_reviews_viewed(&["rev_alert_1".to_string()], true)
                .unwrap(),
            1
        );
        let viewed = db
            .query_reviews(&ReviewFilter {
                reviewed: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(viewed.len(), 1);
        assert_eq!(viewed[0].id, "rev_alert_1");
        assert!(viewed[0].has_been_reviewed);

        assert_eq!(
            db.set_review_notes("rev_alert_1", "Verified delivery van in driveway")
                .unwrap(),
            1
        );
        let with_notes = db.get_review_by_id("rev_alert_1").unwrap().unwrap();
        assert_eq!(
            with_notes.data["notes"],
            "Verified delivery van in driveway"
        );

        // Summary query (24h and per-day breakdown)
        let summary_all = db.query_review_summary(None).unwrap();
        assert_eq!(summary_all["last24Hours"]["total_alert"], 1);
        assert_eq!(summary_all["last24Hours"]["reviewed_alert"], 1);
        assert_eq!(summary_all["last24Hours"]["total_detection"], 1);
        assert_eq!(summary_all["last24Hours"]["reviewed_detection"], 0);

        let summary_front = db
            .query_review_summary(Some(&["front_door".to_string()]))
            .unwrap();
        assert_eq!(summary_front["last24Hours"]["total_alert"], 1);
        assert_eq!(summary_front["last24Hours"]["total_detection"], 0);

        // Unview (reviewed = false)
        assert_eq!(
            db.mark_reviews_viewed(&["rev_alert_1".to_string()], false)
                .unwrap(),
            1
        );
        let after_unview = db.get_review_by_id("rev_alert_1").unwrap().unwrap();
        assert!(!after_unview.has_been_reviewed);

        // Batch delete_reviews
        let deleted = db
            .delete_reviews(&["rev_alert_1".to_string(), "rev_det_2".to_string()])
            .unwrap();
        assert_eq!(deleted, 2);
        let remaining = db.query_reviews(&ReviewFilter::default()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "rev_old_alert_3");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn retention_tier_expiration_and_retain_indefinitely_guardrails_work() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-retention-tiers-{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. cleanup_expired_events_and_reviews:
        //    - expired alert event (retain_indefinitely = false) -> deleted + disk paths returned
        //    - expired detection event (retain_indefinitely = false) -> deleted + disk paths returned
        //    - pinned old event (retain_indefinitely = true) -> strictly preserved
        //    - expired review segment -> deleted + thumb_path returned
        //    - expired preview & timeline entry -> deleted + preview path returned
        db.upsert_event(&EventDto {
            id: "ev_expired_alert".to_string(),
            camera: "cam_a".to_string(),
            label: "person".to_string(),
            zones: json!(["gate"]),
            start_time: 1000.0,
            end_time: Some(1010.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.92),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"max_severity": "alert"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.92),
            stationary: false,
        })
        .unwrap();

        db.upsert_event(&EventDto {
            id: "ev_expired_det".to_string(),
            camera: "cam_a".to_string(),
            label: "car".to_string(),
            zones: json!([]),
            start_time: 1100.0,
            end_time: Some(1110.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.81),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"max_severity": "detection"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.81),
            stationary: false,
        })
        .unwrap();

        db.upsert_event(&EventDto {
            id: "ev_pinned_forever".to_string(),
            camera: "cam_a".to_string(),
            label: "person".to_string(),
            zones: json!(["gate"]),
            start_time: 900.0,
            end_time: Some(910.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: true,
            sub_label: Some("VIP".to_string()),
            top_score: Some(0.99),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"max_severity": "alert"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.99),
            stationary: false,
        })
        .unwrap();

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_expired_1".to_string(),
            camera: "cam_a".to_string(),
            start_time: 1000.0,
            end_time: Some(1015.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-rev_expired_1.webp".to_string(),
            data: json!({"detections": ["ev_expired_alert"]}),
            has_been_reviewed: true,
        })
        .unwrap();
        db.mark_reviews_viewed(&["rev_expired_1".to_string()], true)
            .unwrap();

        db.insert_preview(
            "prev_expired_1",
            "cam_a",
            "/media/ninjanvr/previews/cam_a/1000.mp4",
            1000.0,
            1060.0,
            60.0,
        )
        .unwrap();
        db.insert_timeline_entry(
            1005.0,
            "cam_a",
            "tracked_object",
            Some("ev_expired_alert"),
            " entered_zone",
            &json!({"zone": "gate"}),
        )
        .unwrap();

        let removed_files = db
            .cleanup_expired_events_and_reviews("cam_a", 2000.0, 2000.0)
            .unwrap();
        assert!(
            removed_files.contains(&"/media/ninjanvr/clips/review/thumb-rev_expired_1.webp".into())
        );
        assert!(removed_files.contains(&"/media/ninjanvr/clips/ev_expired_alert.jpg".into()));
        assert!(
            removed_files.contains(&"/media/ninjanvr/clips/cam_a-ev_expired_alert.webp".into())
        );
        assert!(removed_files.contains(&"/media/ninjanvr/clips/ev_expired_det.jpg".into()));
        assert!(removed_files.contains(&"/media/ninjanvr/previews/cam_a/1000.mp4".into()));
        assert!(
            !removed_files
                .iter()
                .any(|p| p.contains("ev_pinned_forever"))
        );

        assert!(db.get_event("ev_expired_alert").unwrap().is_none());
        assert!(db.get_event("ev_expired_det").unwrap().is_none());
        assert!(db.get_event("ev_pinned_forever").unwrap().is_some());
        assert!(db.get_review_by_id("rev_expired_1").unwrap().is_none());
        assert!(
            db.query_timeline(Some("cam_a"), None, 50)
                .unwrap()
                .is_empty()
        );

        // 2. Hybrid stream tiering: set_recording_stream_tier & delete_expired_idle_main_recordings
        //    Event at [5000, 5010] on cam_tier
        db.upsert_event(&EventDto {
            id: "ev_active_overlap".to_string(),
            camera: "cam_tier".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 5000.0,
            end_time: Some(5010.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();

        for (id, start, end, motion, objects, tier) in [
            ("tier_idle_sub", 4000.0, 4010.0, 0, 0, "sub"),
            ("tier_idle_main_old", 4010.0, 4020.0, 0, 0, "main"),
            ("tier_obj_main_old", 4020.0, 4030.0, 0, 2, "main"),
            ("tier_ev_overlap_main", 5000.0, 5010.0, 0, 0, "main"),
            ("tier_idle_main_recent", 7000.0, 7010.0, 0, 0, "main"),
        ] {
            db.insert_recording(&RecordingRow {
                id: id.to_string(),
                camera: "cam_tier".to_string(),
                path: format!("/media/ninjanvr/recordings/cam_tier/{id}.mp4"),
                start_time: start,
                end_time: end,
                duration: 10.0,
                motion,
                objects,
                dbfs: -30,
                segment_size: 2.0,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
            assert_eq!(db.set_recording_stream_tier(id, tier).unwrap(), 1);
        }

        let pruned_idle_main = db
            .delete_expired_idle_main_recordings("cam_tier", 6000.0)
            .unwrap();
        assert_eq!(
            pruned_idle_main,
            vec!["/media/ninjanvr/recordings/cam_tier/tier_idle_main_old.mp4"]
        );
        let rem_tier = db.query_recordings("cam_tier", 3900.0, 7100.0).unwrap();
        let rem_ids: Vec<&str> = rem_tier.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            rem_ids,
            vec![
                "tier_idle_sub",
                "tier_obj_main_old",
                "tier_ev_overlap_main",
                "tier_idle_main_recent"
            ]
        );

        // 3. delete_expired_recordings & delete_expired_recordings_with_window (all vs motion vs active_objects)
        for (id, start, end, motion, objects) in [
            ("rec_all_idle", 1000.0, 1010.0, 0, 0),
            ("rec_motion_only", 1020.0, 1030.0, 45, 0),
            ("rec_active_objects", 1040.0, 1050.0, 80, 3),
        ] {
            db.insert_recording(&RecordingRow {
                id: id.to_string(),
                camera: "cam_ret".to_string(),
                path: format!("/media/ninjanvr/recordings/cam_ret/{id}.mp4"),
                start_time: start,
                end_time: end,
                duration: 10.0,
                motion,
                objects,
                dbfs: 0,
                segment_size: 1.5,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
        }

        // Step A: continuous_before_ts=1500, motion_before_ts=900, event_before_ts=900 -> prunes only idle continuous segment
        let pruned_continuous = db
            .delete_expired_recordings_with_window("cam_ret", 1500.0, 900.0, 900.0, 0.0, 0.0)
            .unwrap();
        assert_eq!(
            pruned_continuous,
            vec!["/media/ninjanvr/recordings/cam_ret/rec_all_idle.mp4"]
        );

        // Step B: continuous_before_ts=1500, motion_before_ts=1500, event_before_ts=900 -> prunes motion_only while keeping active_objects
        let pruned_motion = db
            .delete_expired_recordings("cam_ret", 1500.0, 900.0)
            .unwrap();
        assert_eq!(
            pruned_motion,
            vec!["/media/ninjanvr/recordings/cam_ret/rec_motion_only.mp4"]
        );

        // Step C: event_before_ts=1500 -> prunes active_objects
        let pruned_objects = db
            .delete_expired_recordings("cam_ret", 1500.0, 1500.0)
            .unwrap();
        assert_eq!(
            pruned_objects,
            vec!["/media/ninjanvr/recordings/cam_ret/rec_active_objects.mp4"]
        );
        assert!(
            db.query_recordings("cam_ret", 900.0, 1600.0)
                .unwrap()
                .is_empty()
        );

        // 4. delete_global_recordings_over_quota_mb across multiple cameras + compaction/tier helpers
        for (id, cam, start, size_mb) in [
            ("g_1", "cam_a", 2000.0, 100.0),
            ("g_2", "cam_b", 2010.0, 100.0),
            ("g_3", "cam_a", 2020.0, 100.0),
            ("g_4", "cam_b", 2030.0, 100.0),
        ] {
            db.insert_recording(&RecordingRow {
                id: id.to_string(),
                camera: cam.to_string(),
                path: format!("/media/ninjanvr/recordings/{cam}/{id}.mp4"),
                start_time: start,
                end_time: start + 10.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: -999,
                segment_size: size_mb,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
        }

        // Verify dbfs listing/updating, cold compaction candidates, and tier migration helpers
        let need_dbfs = db.list_recordings_by_dbfs(-999, 10).unwrap();
        assert!(need_dbfs.len() >= 4);
        assert_eq!(db.update_recording_dbfs("g_1", -24).unwrap(), 1);

        let compact_cands = db
            .list_cold_compaction_candidates("cam_a", 2500.0, true, 5.0, 5.0, 10)
            .unwrap();
        assert!(!compact_cands.is_empty());
        let compact_all_cands = db
            .list_cold_compaction_candidates("cam_a", 2500.0, false, 5.0, 5.0, 10)
            .unwrap();
        assert!(!compact_all_cands.is_empty());
        db.update_recording_compacted("g_1", 80.0).unwrap();

        let mig = db
            .list_recordings_for_tier_migration(2500.0, "/media/ninjanvr/recordings/%", 10)
            .unwrap();
        assert!(!mig.is_empty());
        assert!(
            db.update_recording_path("g_1", "/media/ninjanvr/recordings/cam_a/g_1_cold.mp4")
                .unwrap()
        );

        // Remove cam_tier segments so global quota test only evaluates cam_a + cam_b (80 + 100 + 100 + 100 = 380 MB)
        for id in [
            "tier_idle_sub",
            "tier_obj_main_old",
            "tier_ev_overlap_main",
            "tier_idle_main_recent",
        ] {
            assert_eq!(db.delete_recording_by_id(id).unwrap(), 1);
        }

        assert!(
            db.delete_global_recordings_over_quota_mb(0.0)
                .unwrap()
                .is_empty()
        );
        assert!(
            db.delete_global_recordings_over_quota_mb(500.0)
                .unwrap()
                .is_empty()
        );
        let global_evicted = db.delete_global_recordings_over_quota_mb(200.0).unwrap();
        assert_eq!(
            global_evicted,
            vec![
                "/media/ninjanvr/recordings/cam_a/g_1_cold.mp4",
                "/media/ninjanvr/recordings/cam_b/g_2.mp4"
            ]
        );

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn cases_api_tokens_and_exports_lifecycle_work() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-cases-tokens-{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. Cases (create_case, list_cases, query_cases, get_case) with JSON event_ids & export_ids
        let created_case = db
            .create_case(
                "case_001",
                "Perimeter Breach Investigation",
                Some("Gate entry at 02:14 UTC"),
                &["ev_101".to_string(), "ev_102".to_string()],
                &["exp_1".to_string()],
            )
            .unwrap();
        assert_eq!(created_case["id"], "case_001");
        assert_eq!(created_case["event_ids"], json!(["ev_101", "ev_102"]));
        assert_eq!(created_case["export_ids"], json!(["exp_1"]));

        let fetched_case = db.get_case("case_001").unwrap().unwrap();
        assert_eq!(fetched_case["name"], "Perimeter Breach Investigation");
        assert_eq!(
            fetched_case["description"],
            json!("Gate entry at 02:14 UTC")
        );
        assert_eq!(fetched_case["event_ids"], json!(["ev_101", "ev_102"]));
        assert_eq!(fetched_case["export_ids"], json!(["exp_1"]));
        assert!(db.get_case("missing_case").unwrap().is_none());

        let all_cases = db.list_cases().unwrap();
        assert_eq!(all_cases.len(), 1);
        assert_eq!(db.query_cases().unwrap().len(), 1);

        // 2. API Tokens (create_api_token, list_api_tokens, verify_api_token, delete_api_token)
        assert!(db.create_api_token("", "read_only", None).is_err());
        assert!(
            db.create_api_token("bad-scope", "superadmin", None)
                .is_err()
        );

        let (tok_meta, plaintext) = db
            .create_api_token("prometheus-scraper", "metrics", Some(5000.0))
            .unwrap();
        let tok_id = tok_meta["id"].as_str().unwrap().to_string();
        assert_eq!(tok_meta["name"], "prometheus-scraper");
        assert_eq!(tok_meta["scope"], "metrics");
        assert!(plaintext.starts_with("nnvr_"));

        let (_full_meta, full_plain) = db.create_api_token("automation", "full", None).unwrap();
        let (_ro_meta, _ro_plain) = db
            .create_api_token("dashboard-ro", "read_only", None)
            .unwrap();

        let listed_tokens = db.list_api_tokens().unwrap();
        assert_eq!(listed_tokens.len(), 3);

        // Valid verification updates last_used_at
        let verified = db.verify_api_token(&plaintext, 4000.0).unwrap();
        assert_eq!(
            verified,
            Some(("prometheus-scraper".to_string(), "metrics".to_string()))
        );
        let after_use = db
            .list_api_tokens()
            .unwrap()
            .into_iter()
            .find(|t| t["id"] == tok_id)
            .unwrap();
        assert_eq!(after_use["last_used_at"], json!(4000.0));

        // Expired token rejected
        assert!(db.verify_api_token(&plaintext, 5000.0).unwrap().is_none());
        assert!(db.verify_api_token(&plaintext, 6000.0).unwrap().is_none());
        // Non-expired token without expires_at succeeds
        assert_eq!(
            db.verify_api_token(&full_plain, 999999.0).unwrap(),
            Some(("automation".to_string(), "full".to_string()))
        );
        // Invalid prefix or unknown token rejected
        assert!(
            db.verify_api_token("invalid_token", 1000.0)
                .unwrap()
                .is_none()
        );
        assert!(
            db.verify_api_token("nnvr_00000000000000000000000000000000", 1000.0)
                .unwrap()
                .is_none()
        );

        assert!(db.delete_api_token(&tok_id).unwrap());
        assert!(!db.delete_api_token(&tok_id).unwrap());
        assert_eq!(db.list_api_tokens().unwrap().len(), 2);

        // 3. Exports (insert_export, list_exports, get_export, rename_export, delete_export, delete_exports_bulk)
        for (id, cam, name, date) in [
            ("exp_1", "front_door", "Front Clip 1", 1700000100.0),
            ("exp_2", "backyard", "Backyard Clip 2", 1700000200.0),
            ("exp_3", "garage", "Garage Clip 3", 1700000300.0),
        ] {
            db.insert_export(&ExportRow {
                id: id.to_string(),
                camera: cam.to_string(),
                name: name.to_string(),
                date,
                video_path: format!("/media/ninjanvr/exports/{id}.mp4"),
                thumb_path: format!("/media/ninjanvr/clips/{id}.webp"),
                in_progress: false,
            })
            .unwrap();
        }

        let exports = db.list_exports().unwrap();
        assert_eq!(exports.len(), 3);
        assert_eq!(db.query_exports().unwrap().len(), 3);

        let exp1 = db.get_export("exp_1").unwrap().unwrap();
        assert_eq!(exp1["name"], "Front Clip 1");
        assert_eq!(exp1["camera"], "front_door");
        assert!(db.get_export("exp_missing").unwrap().is_none());

        assert_eq!(db.rename_export("exp_1", "Renamed Front Clip").unwrap(), 1);
        assert_eq!(
            db.get_export("exp_1").unwrap().unwrap()["name"],
            "Renamed Front Clip"
        );

        let del_single = db.delete_export("exp_1").unwrap().unwrap();
        assert_eq!(
            del_single,
            (
                "/media/ninjanvr/exports/exp_1.mp4".to_string(),
                "/media/ninjanvr/clips/exp_1.webp".to_string()
            )
        );
        assert!(db.delete_export("exp_1").unwrap().is_none());

        let del_bulk = db
            .delete_exports_bulk(&[
                "exp_2".to_string(),
                "exp_3".to_string(),
                "exp_non_existent".to_string(),
            ])
            .unwrap();
        assert_eq!(del_bulk.len(), 2);
        assert!(db.list_exports().unwrap().is_empty());

        // Additional user camera scoping helpers
        db.ensure_default_admin(1000).unwrap();
        assert!(db.ensure_default_admin(1000).unwrap().is_none());
        db.create_user_with_cameras(
            "scoped_op",
            "pass1234",
            "operator",
            "front_door,garage",
            1000,
        )
        .unwrap();
        let scoped = db.get_user("scoped_op").unwrap().unwrap();
        assert_eq!(scoped.allowed_cameras, "front_door,garage");
        assert!(
            db.update_user_allowed_cameras("scoped_op", "backyard")
                .unwrap()
        );
        assert_eq!(
            db.get_user("scoped_op").unwrap().unwrap().allowed_cameras,
            "backyard"
        );
        assert_eq!(db.list_users().unwrap().len(), 2);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn event_mutations_timeline_previews_summaries_and_maintenance_work() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-events-summaries-{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();
        assert_eq!(db.path(), tmp.as_path());
        db.init_schema().unwrap();

        let now = Utc::now().timestamp() as f64;
        let t1 = now - 600.0;
        let t2 = now - 300.0;
        let t3 = now - 120.0;

        for (id, cam, label, start, score) in [
            ("ev_m1", "front_door", "person", t1, 0.95),
            ("ev_m2", "front_door", "person", t2, 0.89),
            ("ev_m3", "backyard", "car", t3, 0.91),
        ] {
            db.upsert_event(&EventDto {
                id: id.to_string(),
                camera: cam.to_string(),
                label: label.to_string(),
                zones: json!(["zone_a"]),
                start_time: start,
                end_time: Some(start + 12.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(score),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.2, 0.4, 0.8])),
                data: json!({"top_score": score}),
                thumbnail: Some("base64thumb".to_string()),
                area: Some(1200),
                ratio: Some(0.75),
                score: Some(score as f32),
                stationary: false,
            })
            .unwrap();
        }

        // get_event, get_events_by_ids, query_event_ids, query_events_explore, query_events_summary
        let ev1 = db.get_event("ev_m1").unwrap().unwrap();
        assert_eq!(ev1["camera"], "front_door");
        assert_eq!(ev1["label"], "person");

        let batch = db
            .get_events_by_ids(&["ev_m1".to_string(), "ev_m3".to_string()])
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(
            db.query_event_ids(&["ev_m1".to_string(), "ev_m3".to_string()])
                .unwrap()
                .len(),
            2
        );

        let explore = db.query_events_explore(1).unwrap();
        assert_eq!(explore.len(), 2); // 1 per distinct label ("person" and "car")
        let person_explore = explore.iter().find(|e| e["label"] == "person").unwrap();
        assert_eq!(person_explore["event_count"], 2);

        let ev_summary = db.query_events_summary().unwrap();
        assert!(!ev_summary.is_empty());

        let counts_by_cam = db.query_event_counts_by_camera_and_label().unwrap();
        assert_eq!(
            counts_by_cam,
            vec![
                ("backyard".to_string(), "car".to_string(), 1),
                ("front_door".to_string(), "person".to_string(), 2),
            ]
        );

        // AI digest summary
        let digest = db
            .query_ai_digest_summary(t1 - 60.0, now + 60.0, Some(&["front_door".to_string()]))
            .unwrap();
        assert_eq!(digest["total_events"], 2);
        assert_eq!(digest["counts_by_camera"]["front_door"], 2);

        // Event mutations: retain, false_positive, sub_label, description, notes
        assert_eq!(db.set_event_retain("ev_m1", true).unwrap(), 1);
        assert_eq!(db.set_event_false_positive("ev_m1", true).unwrap(), 1);
        assert_eq!(db.set_event_sub_label("ev_m1", Some("Courier")).unwrap(), 1);
        assert_eq!(
            db.set_event_description("ev_m1", "Package left at door")
                .unwrap(),
            1
        );
        assert_eq!(db.set_event_description("missing", "noop").unwrap(), 0);
        assert_eq!(
            db.set_event_notes("ev_m1", "Confirmed by operator")
                .unwrap(),
            1
        );

        let mutated = db.get_event("ev_m1").unwrap().unwrap();
        assert_eq!(mutated["retain_indefinitely"], true);
        assert_eq!(mutated["false_positive"], true);
        assert_eq!(mutated["sub_label"], "Courier");
        assert_eq!(mutated["data"]["description"], "Package left at door");
        assert_eq!(mutated["data"]["notes"], "Confirmed by operator");

        assert_eq!(
            db.set_event_license_plate("ev_m1", Some("abc-1d23"))
                .unwrap(),
            1
        );
        assert_eq!(
            db.set_event_license_plate("missing", Some("ABC1D23"))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_recognized_license_plates().unwrap(),
            vec!["ABC1D23".to_string()]
        );
        let by_plate = db
            .query_events(&EventFilter {
                plate: Some("ABC1D23".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_plate.len(), 1);
        assert_eq!(by_plate[0]["id"], "ev_m1");
        assert_eq!(by_plate[0]["data"]["recognized_license_plate"], "ABC1D23");
        assert_eq!(db.set_event_license_plate("ev_m1", None).unwrap(), 1);
        assert!(db.query_recognized_license_plates().unwrap().is_empty());

        assert_eq!(db.set_event_retain("ev_m1", false).unwrap(), 1);
        assert_eq!(db.set_event_false_positive("ev_m1", false).unwrap(), 1);
        assert_eq!(db.set_event_sub_label("ev_m1", None).unwrap(), 1);

        // delete_event and delete_events_bulk
        assert_eq!(db.delete_event("ev_m1").unwrap(), 1);
        assert!(db.get_event("ev_m1").unwrap().is_none());
        assert_eq!(
            db.delete_events_bulk(&["ev_m2".to_string(), "ev_m3".to_string()])
                .unwrap(),
            2
        );
        assert!(db.query_events(&EventFilter::default()).unwrap().is_empty());

        // Timeline: insert_timeline_entry & query_timeline
        db.insert_timeline_entry(
            t1,
            "front_door",
            "tracked_object",
            Some("src_1"),
            "visible",
            &json!({"label": "person"}),
        )
        .unwrap();
        db.insert_timeline_entry(
            t2,
            "front_door",
            "tracked_object",
            Some("src_2"),
            "entered_zone",
            &json!({"zone": "zone_a"}),
        )
        .unwrap();
        db.insert_timeline_entry(
            t3,
            "backyard",
            "audio",
            Some("src_3"),
            "heard",
            &json!({"label": "bark"}),
        )
        .unwrap();

        let tl_cam = db.query_timeline(Some("front_door"), None, 10).unwrap();
        assert_eq!(tl_cam.len(), 2);
        let tl_src = db
            .query_timeline(Some("all"), Some("src_1,src_3"), 10)
            .unwrap();
        assert_eq!(tl_src.len(), 2);

        // Recordings summaries, storage usage, and motion activity
        db.insert_recording(&RecordingRow {
            id: "sum_rec_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/front_door/sum_rec_1.mp4".to_string(),
            start_time: t1,
            end_time: t1 + 10.0,
            duration: 10.0,
            motion: 150,
            objects: 2,
            dbfs: -18,
            segment_size: 4.5,
            regions: 3,
            motion_heatmap: Some(json!([[1, 2], [3, 4]])),
        })
        .unwrap();
        db.upsert_event(&EventDto {
            id: "ev_for_rec_summary".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: t1 + 2.0,
            end_time: Some(t1 + 8.0),
            has_clip: false,
            has_snapshot: false,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: None,
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: None,
            stationary: false,
        })
        .unwrap();

        let rec_summary = db.query_recordings_summary("front_door").unwrap();
        assert_eq!(rec_summary.len(), 1);
        assert_eq!(rec_summary[0]["duration"], 10);
        assert_eq!(rec_summary[0]["motion"], 150);
        assert_eq!(rec_summary[0]["events"], 1);

        let all_rec_summary = db.query_all_recordings_summary().unwrap();
        assert!(!all_rec_summary.as_object().unwrap().is_empty());

        let storage_usage = db.query_recordings_storage_usage().unwrap();
        assert_eq!(storage_usage["front_door"]["usage"], 4.5);
        assert!(storage_usage["front_door"]["bandwidth"].as_f64().unwrap() > 0.0);

        let motion_act = db
            .query_motion_activity(Some(&["front_door".to_string()]), t1 - 10.0, t1 + 20.0)
            .unwrap();
        assert_eq!(motion_act.len(), 1);
        assert_eq!(motion_act[0]["motion"], 150);

        // Previews & strip_media_root & run_maintenance
        assert_eq!(
            strip_media_root("/media/ninjanvr/previews/front_door/p1.mp4"),
            Some("previews/front_door/p1.mp4")
        );
        assert_eq!(strip_media_root("/media/ninjanvr/"), None);
        assert_eq!(strip_media_root("/other/path.mp4"), None);

        assert!(db.last_preview_end_time("front_door").unwrap().is_none());
        db.insert_preview(
            "prev_1",
            "front_door",
            "/media/ninjanvr/previews/front_door/p1.mp4",
            t1,
            t1 + 60.0,
            60.0,
        )
        .unwrap();
        assert_eq!(
            db.last_preview_end_time("front_door").unwrap(),
            Some(t1 + 60.0)
        );

        let previews_cam = db
            .query_previews("front_door", t1 - 5.0, t1 + 65.0)
            .unwrap();
        assert_eq!(previews_cam.len(), 1);
        assert_eq!(previews_cam[0]["src"], "/previews/front_door/p1.mp4");

        let previews_all = db.query_previews("all", t1 - 5.0, t1 + 65.0).unwrap();
        assert_eq!(previews_all.len(), 1);

        db.run_maintenance().unwrap();

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn false_positive_signature_lifecycle_and_iou_dhash_matching() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_fp_sig_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        let base_hash: u64 = 0xA5A5_5A5A_F0F0_0F0F;
        db.upsert_event(&EventDto {
            id: "ev_fp_patio_plant".to_string(),
            camera: "patio".to_string(),
            label: "person".to_string(),
            zones: json!(["yard"]),
            start_time: 1000.0,
            end_time: Some(1008.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.76),
            false_positive: Some(false),
            box_coords: Some(json!([0.20, 0.30, 0.40, 0.80])),
            data: json!({
                "box": [0.20, 0.30, 0.40, 0.80],
                "dhash64": base_hash,
                "attributes": ["color:green"]
            }),
            thumbnail: None,
            area: Some(12000),
            ratio: Some(0.40),
            score: Some(0.76),
            stationary: true,
        })
        .unwrap();

        // 1. Mark event as false positive -> creates false_positive_signature in SQLite
        assert_eq!(
            db.set_event_false_positive("ev_fp_patio_plant", true)
                .unwrap(),
            1
        );
        let sigs = db.list_false_positive_signatures(Some("patio")).unwrap();
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0]["camera"], "patio");
        assert_eq!(sigs[0]["label"], "person");
        assert_eq!(sigs[0]["dhash64"].as_u64(), Some(base_hash));
        assert_eq!(sigs[0]["dominant_color"], "color:green");
        assert_eq!(sigs[0]["hit_count"], 1);

        // 2. Matching box (IoU >= 0.72) and visually close crop (hamming <= 12) matches & increments hit_count
        let minor_noise_hash = base_hash ^ 0b1011; // hamming distance = 3 <= 12
        let matched = db
            .match_false_positive_signature(
                "patio",
                "person",
                [0.21, 0.31, 0.40, 0.79],
                Some(minor_noise_hash),
            )
            .unwrap();
        assert!(matched.is_some());
        let matched_val = matched.unwrap();
        assert_eq!(matched_val["hit_count"], 2);
        assert!(matched_val["iou"].as_f64().unwrap() >= 0.72);
        assert_eq!(matched_val["hamming_distance"], 3);

        // 3. Visually distinct crop at the same coordinates (hamming > 12) is NOT suppressed
        let distinct_hash = !base_hash; // hamming distance = 64 > 12
        let rejected_hash = db
            .match_false_positive_signature(
                "patio",
                "person",
                [0.20, 0.30, 0.40, 0.80],
                Some(distinct_hash),
            )
            .unwrap();
        assert!(rejected_hash.is_none());

        // 4. Non-overlapping bounding box (IoU < 0.72) is NOT suppressed
        let rejected_box = db
            .match_false_positive_signature(
                "patio",
                "person",
                [0.65, 0.15, 0.85, 0.55],
                Some(base_hash),
            )
            .unwrap();
        assert!(rejected_box.is_none());

        // 5. Unmarking false positive (fp = false) removes the signature
        assert_eq!(
            db.set_event_false_positive("ev_fp_patio_plant", false)
                .unwrap(),
            1
        );
        assert!(db.list_false_positive_signatures(None).unwrap().is_empty());

        // 6. Re-mark and delete via delete_false_positive_signature(id)
        db.set_event_false_positive("ev_fp_patio_plant", true)
            .unwrap();
        let sigs_again = db.list_false_positive_signatures(None).unwrap();
        assert_eq!(sigs_again.len(), 1);
        let sig_id = sigs_again[0]["id"].as_i64().unwrap();
        assert_eq!(db.delete_false_positive_signature(sig_id).unwrap(), 1);
        assert!(db.list_false_positive_signatures(None).unwrap().is_empty());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn set_event_sub_label_persists_score_and_propagates_to_reviewsegment() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_db_sub_label_test_{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);
        let db = NinjaDatabase::open(&tmp).unwrap();

        db.upsert_event(&EventDto {
            id: "ev_face_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["porch"]),
            start_time: 1000.0,
            end_time: Some(1005.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.92),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.4, 0.8])),
            data: json!({ "box": [0.1, 0.1, 0.4, 0.8] }),
            thumbnail: None,
            area: Some(12000),
            ratio: Some(0.5),
            score: Some(0.92),
            stationary: false,
        })
        .unwrap();

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_face_1".to_string(),
            camera: "front_door".to_string(),
            start_time: 1000.0,
            end_time: Some(1010.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb.webp".to_string(),
            data: json!({
                "detections": ["ev_face_1"],
                "objects": ["person"],
                "sub_labels": [],
                "zones": ["porch"]
            }),
            has_been_reviewed: false,
        })
        .unwrap();

        let updated = db
            .set_event_sub_label_with_score("ev_face_1", Some("Bruno"), Some(0.84))
            .unwrap();
        assert_eq!(updated, 1);

        let ev = db.get_event("ev_face_1").unwrap().unwrap();
        assert_eq!(ev["sub_label"], "Bruno");
        assert!((ev["sub_label_score"].as_f64().unwrap() - 0.84).abs() < 1e-6);
        assert!((ev["data"]["sub_label_score"].as_f64().unwrap() - 0.84).abs() < 1e-6);

        let rev = db.get_review_by_id("rev_face_1").unwrap().unwrap();
        assert_eq!(rev.data["sub_labels"], json!(["Bruno"]));
        assert!((rev.data["sub_label_score"].as_f64().unwrap() - 0.84).abs() < 1e-6);

        let distinct = db.list_distinct_sub_labels().unwrap();
        assert_eq!(distinct, vec!["Bruno"]);

        db.set_event_sub_label("ev_face_1", None).unwrap();
        let ev_cleared = db.get_event("ev_face_1").unwrap().unwrap();
        assert!(ev_cleared["sub_label"].is_null());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn dataset_sample_crud_split_assignment_and_summary_counts() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_dataset_sample_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        assert_eq!(NINJANVR_SECURITY_CLASSES.len(), 15);
        assert_eq!(security_class_id("person"), Some(0));
        assert_eq!(security_class_id("face"), Some(1));
        assert_eq!(security_class_id("license_plate"), Some(7));
        assert_eq!(security_class_id("umbrella"), Some(14));
        assert_eq!(security_class_label(11), Some("package"));
        assert_eq!(security_class_label(99), None);

        // Verify deterministic 80/20 split across 50 sample IDs produces both train and val
        let mut seen_train = 0;
        let mut seen_val = 0;
        for i in 0..50 {
            let sid = format!("sample_det_{i}");
            let s1 = deterministic_dataset_split(&sid);
            let s2 = deterministic_dataset_split(&sid);
            assert_eq!(s1, s2);
            if s1 == "val" {
                seen_val += 1;
            } else {
                assert_eq!(s1, "train");
                seen_train += 1;
            }
        }
        assert!(seen_train > 0 && seen_val > 0);

        // 1. Insert true_positive, hard_negative, and ai_annotated samples
        let tp_sample = db
            .upsert_dataset_sample(&DatasetSampleDto {
                id: "ds_ev_tp_1".to_string(),
                event_id: Some("ev_tp_1".to_string()),
                camera: "front_door".to_string(),
                split: String::new(),
                sample_kind: "true_positive".to_string(),
                image_path: "/media/ninjanvr/dataset/ds_ev_tp_1.webp".to_string(),
                width: 1280,
                height: 720,
                annotations: vec![
                    DatasetAnnotationBox {
                        class_id: 0,
                        label: "person".to_string(),
                        cx: 0.45,
                        cy: 0.55,
                        w: 0.20,
                        h: 0.60,
                        score: 0.92,
                        source: "event".to_string(),
                    },
                    DatasetAnnotationBox {
                        class_id: 11,
                        label: "package".to_string(),
                        cx: 0.48,
                        cy: 0.78,
                        w: 0.08,
                        h: 0.10,
                        score: 0.88,
                        source: "event".to_string(),
                    },
                ],
                verified: true,
                night_mode: false,
                created_at: 1000.0,
            })
            .unwrap();
        assert!(tp_sample.split == "train" || tp_sample.split == "val");

        db.upsert_dataset_sample(&DatasetSampleDto {
            id: "ds_ev_hn_1".to_string(),
            event_id: Some("ev_hn_1".to_string()),
            camera: "patio".to_string(),
            split: "val".to_string(),
            sample_kind: "hard_negative".to_string(),
            image_path: "/media/ninjanvr/dataset/ds_ev_hn_1.webp".to_string(),
            width: 1280,
            height: 720,
            annotations: vec![],
            verified: true,
            night_mode: true,
            created_at: 1005.0,
        })
        .unwrap();

        db.upsert_dataset_sample(&DatasetSampleDto {
            id: "ds_ev_ai_1".to_string(),
            event_id: Some("ev_ai_1".to_string()),
            camera: "front_door".to_string(),
            split: "train".to_string(),
            sample_kind: "ai_annotated".to_string(),
            image_path: "/media/ninjanvr/dataset/ds_ev_ai_1.webp".to_string(),
            width: 1920,
            height: 1080,
            annotations: vec![DatasetAnnotationBox {
                class_id: 2,
                label: "car".to_string(),
                cx: 0.50,
                cy: 0.50,
                w: 0.40,
                h: 0.30,
                score: 0.96,
                source: "ai_teacher".to_string(),
            }],
            verified: false,
            night_mode: false,
            created_at: 1010.0,
        })
        .unwrap();

        // 2. List & filter samples
        let all = db.list_dataset_samples(None, None, None).unwrap();
        assert_eq!(all.len(), 3);
        let front_only = db
            .list_dataset_samples(Some("front_door"), None, None)
            .unwrap();
        assert_eq!(front_only.len(), 2);
        let unverified = db.list_dataset_samples(None, None, Some(false)).unwrap();
        assert_eq!(unverified.len(), 1);
        assert_eq!(unverified[0].id, "ds_ev_ai_1");

        // 3. Update annotations, verified flag, and split
        let updated = db
            .update_dataset_sample_annotations(
                "ds_ev_ai_1",
                Some(&[
                    DatasetAnnotationBox {
                        class_id: 2,
                        label: "car".to_string(),
                        cx: 0.52,
                        cy: 0.48,
                        w: 0.38,
                        h: 0.28,
                        score: 0.99,
                        source: "manual".to_string(),
                    },
                    DatasetAnnotationBox {
                        class_id: 7,
                        label: "license_plate".to_string(),
                        cx: 0.52,
                        cy: 0.56,
                        w: 0.08,
                        h: 0.04,
                        score: 0.95,
                        source: "manual".to_string(),
                    },
                ]),
                None,
                Some(true),
                Some("train"),
            )
            .unwrap()
            .unwrap();
        assert!(updated.verified);
        assert_eq!(updated.annotations.len(), 2);

        // 4. Verify get_dataset_summary aggregation
        let summary = db.get_dataset_summary().unwrap();
        assert_eq!(summary["total_samples"], 3);
        assert_eq!(summary["true_positive_count"], 1);
        assert_eq!(summary["hard_negative_count"], 1);
        assert_eq!(summary["ai_annotated_count"], 1);
        assert_eq!(summary["verified_count"], 3);
        assert_eq!(summary["unverified_count"], 0);
        assert_eq!(summary["night_mode_count"], 1);
        assert_eq!(summary["total_annotations"], 4);
        assert_eq!(summary["by_camera"]["front_door"], 2);
        assert_eq!(summary["by_camera"]["patio"], 1);
        assert_eq!(summary["by_class"]["person"], 1);
        assert_eq!(summary["by_class"]["package"], 1);
        assert_eq!(summary["by_class"]["car"], 1);
        assert_eq!(summary["by_class"]["license_plate"], 1);
        assert_eq!(summary["ready_for_export"], true);

        // 5. Delete sample and verify count decreases
        assert_eq!(db.delete_dataset_sample("ds_ev_hn_1").unwrap(), 1);
        assert!(db.get_dataset_sample("ds_ev_hn_1").unwrap().is_none());
        let summary_after = db.get_dataset_summary().unwrap();
        assert_eq!(summary_after["total_samples"], 2);
        assert_eq!(summary_after["hard_negative_count"], 0);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn security_taxonomy_class_lookup_normalizes_case_hyphens_and_spaces() {
        assert_eq!(NINJANVR_SECURITY_CLASSES.len(), 15);
        for (idx, &canonical) in NINJANVR_SECURITY_CLASSES.iter().enumerate() {
            assert_eq!(security_class_label(idx), Some(canonical));
            assert_eq!(security_class_id(canonical), Some(idx));
        }
        assert_eq!(security_class_label(15), None);
        assert_eq!(security_class_id("  License-Plate  "), Some(7));
        assert_eq!(security_class_id("LICENSE PLATE"), Some(7));
        assert_eq!(security_class_id("Waste-Bin"), Some(12));
        assert_eq!(security_class_id("  waste bin "), Some(12));
        assert_eq!(security_class_id("MotorCycle"), Some(3));
        assert_eq!(security_class_id("unknown_ufo"), None);
        assert_eq!(security_class_id(""), None);
    }

    #[test]
    fn password_hashing_generates_unique_salts_and_rejects_malformed_hashes() {
        let h1 = hash_password("TacticalPass!2026", None, 1_000);
        let h2 = hash_password("TacticalPass!2026", None, 1_000);
        assert_ne!(h1, h2, "random salt must make each hash unique");
        assert!(verify_password("TacticalPass!2026", &h1));
        assert!(verify_password("TacticalPass!2026", &h2));
        assert!(!verify_password("WrongPassword", &h1));

        // Malformed hash strings must safely return false without panicking
        assert!(!verify_password("pass", ""));
        assert!(!verify_password("pass", "pbkdf2_sha256$1000$only_three"));
        assert!(!verify_password("pass", "argon2id$1000$salt$abcd"));
        assert!(!verify_password(
            "pass",
            "pbkdf2_sha256$not_a_number$salt$abcd"
        ));
        assert!(!verify_password("pass", "pbkdf2_sha256$1000$salt$short"));
    }

    #[test]
    fn pure_json_and_sqlite_value_helpers_handle_edge_cases() {
        use rusqlite::types::ValueRef;

        // 1. extract_dhash64_from_value
        assert_eq!(
            extract_dhash64_from_value(&json!(0x1234_5678_9abc_def0u64)),
            Some(0x1234_5678_9abc_def0u64)
        );
        assert_eq!(extract_dhash64_from_value(&json!(-1i64)), Some(u64::MAX));
        assert_eq!(
            extract_dhash64_from_value(&json!("  0xdeadbeefcafebabe ")),
            Some(0xdead_beef_cafe_babeu64)
        );
        assert_eq!(
            extract_dhash64_from_value(&json!("0X00000000000000ff")),
            Some(255)
        );
        assert_eq!(extract_dhash64_from_value(&json!("not_a_hex_hash")), None);
        assert_eq!(extract_dhash64_from_value(&json!(null)), None);

        // 2. extract_norm_box_from_event priority chain
        let ev_top = json!({
            "box": [0.1, 0.2, 0.3, 0.4],
            "data": { "box": [0.5, 0.5, 0.6, 0.6] }
        });
        assert_eq!(extract_norm_box_from_event(&ev_top), [0.1, 0.2, 0.3, 0.4]);

        let ev_data = json!({
            "data": {
                "box": [0.2, 0.3, 0.4, 0.5],
                "provenance": { "norm_bbox": [0.7, 0.7, 0.8, 0.8] }
            }
        });
        assert_eq!(extract_norm_box_from_event(&ev_data), [0.2, 0.3, 0.4, 0.5]);

        let ev_prov = json!({
            "data": { "provenance": { "norm_bbox": [0.15, 0.25, 0.35, 0.45] } }
        });
        assert_eq!(
            extract_norm_box_from_event(&ev_prov),
            [0.15, 0.25, 0.35, 0.45]
        );
        assert_eq!(
            extract_norm_box_from_event(&json!({})),
            [0.0, 0.0, 0.0, 0.0]
        );

        // 3. json_array_matches_any with and without "-verified" suffix matching
        let arr = json!(["person-verified", "car"]);
        let allowed_person = vec!["person".to_string()];
        assert!(json_array_matches_any(&arr, &allowed_person, true));
        assert!(!json_array_matches_any(&arr, &allowed_person, false));
        assert!(!json_array_matches_any(
            &json!("not_an_array"),
            &allowed_person,
            true
        ));

        // 4. value_to_f64 across SQLite storage classes
        assert_eq!(value_to_f64(&ValueRef::Real(123.5)), Some(123.5));
        assert_eq!(value_to_f64(&ValueRef::Integer(42)), Some(42.0));
        assert_eq!(
            value_to_f64(&ValueRef::Text(b"1700000000.25")),
            Some(1700000000.25)
        );
        assert_eq!(
            value_to_f64(&ValueRef::Text(b"1970-01-01T00:01:00Z")),
            Some(60.0)
        );
        assert_eq!(
            value_to_f64(&ValueRef::Text(b"1970-01-01 00:02:00.500")),
            Some(120.5)
        );
        assert_eq!(value_to_f64(&ValueRef::Text(b"invalid_timestamp")), None);
        assert_eq!(value_to_f64(&ValueRef::Null), None);
    }

    #[test]
    fn user_creation_validates_username_password_role_and_camera_scope_normalization() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_user_validation_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // Reject invalid usernames, passwords, and roles
        assert!(
            db.create_user_with_cameras("", "secret123", "admin", "*", 1_000)
                .is_err()
        );
        assert!(
            db.create_user_with_cameras(&"a".repeat(31), "secret123", "admin", "*", 1_000)
                .is_err()
        );
        assert!(
            db.create_user_with_cameras("bad user!", "secret123", "admin", "*", 1_000)
                .is_err()
        );
        assert!(
            db.create_user_with_cameras("valid_user", "", "admin", "*", 1_000)
                .is_err()
        );
        assert!(
            db.create_user_with_cameras("valid_user", "secret123", "superadmin", "*", 1_000)
                .is_err()
        );

        // Normalize role case/whitespace and blank allowed_cameras -> "*"
        db.create_user_with_cameras("op.user-1", "secret123", "  OpErAtOr ", "   ", 1_000)
            .unwrap();
        let users = db.list_users().unwrap();
        let created = users.iter().find(|u| u["username"] == "op.user-1").unwrap();
        assert_eq!(created["role"], "operator");
        assert_eq!(created["allowed_cameras"], "*");

        // Duplicate username must fail
        assert!(
            db.create_user_with_cameras("op.user-1", "otherpass", "viewer", "*", 1_000)
                .is_err()
        );

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn db_bugfixes_disjoint_xyxy_iou_upsert_preservation_digest_dedup_and_sibling_sub_labels() {
        // 1. Disjoint [x1, y1, x2, y2] boxes in bottom-right quadrant must have IoU == 0.0
        let box_a = [0.75, 0.75, 0.85, 0.85];
        let box_b = [0.75, 0.85, 0.85, 0.95];
        assert_eq!(compute_norm_box_iou(box_a, box_b), 0.0);

        // Normalized [x, y, w, h] boxes from BoundingBox::to_normalized still compute exact IoU
        assert!(
            (compute_norm_box_iou([0.75, 0.75, 0.10, 0.10], [0.75, 0.75, 0.10, 0.10]) - 1.0).abs()
                < 1e-6
        );

        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_db_bugfixes_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 2. upsert_event persists stationary: true, and subsequent upserts preserve ai_verification,
        // suppressed_by_fp_signature, description, and notes
        let mut ev1 = EventDto {
            id: "ev_1".to_string(),
            camera: "front_door".to_string(),
            label: "car".to_string(),
            zones: json!(["driveway"]),
            start_time: 1_000.0,
            end_time: None,
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: true,
            sub_label: Some("Alice".to_string()),
            top_score: Some(0.88),
            false_positive: Some(false),
            box_coords: Some(json!([0.1, 0.1, 0.3, 0.3])),
            data: json!({
                "suppressed_by_fp_signature": 7,
                "description": "Parked courier car",
                "notes": "Verified by operator"
            }),
            thumbnail: None,
            area: Some(5000),
            ratio: Some(1.0),
            score: Some(0.88),
            stationary: true,
        };
        db.upsert_event(&ev1).unwrap();
        db.set_event_ai_verification(
            "ev_1",
            &json!({ "verdict": "true_positive", "confidence": 0.95 }),
        )
        .unwrap();

        // Simulate track termination upsert where pipeline reconstructs ev.data without those fields
        ev1.end_time = Some(1_015.0);
        ev1.data = json!({ "box": [0.1, 0.1, 0.3, 0.3], "type": "object" });
        db.upsert_event(&ev1).unwrap();

        let fetched = db.get_event("ev_1").unwrap().unwrap();
        assert_eq!(fetched["stationary"], true);
        assert_eq!(fetched["data"]["suppressed_by_fp_signature"], 7);
        assert_eq!(fetched["data"]["description"], "Parked courier car");
        assert_eq!(fetched["data"]["notes"], "Verified by operator");
        assert_eq!(
            fetched["data"]["ai_verification"]["verdict"],
            "true_positive"
        );

        // 3. Second event with retain_indefinitely: false -> test favorites: Some(false) filter
        let ev2 = EventDto {
            id: "ev_2".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["driveway"]),
            start_time: 1_002.0,
            end_time: Some(1_010.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: Some("Bruno".to_string()),
            top_score: Some(0.91),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.91),
            stationary: false,
        };
        db.upsert_event(&ev2).unwrap();

        let non_favs = db
            .query_events(&EventFilter {
                favorites: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(non_favs.len(), 1);
        assert_eq!(non_favs[0]["id"], "ev_2");

        // 4. Multi-detection reviewsegment + multi-user review status + sibling sub_label preservation
        db.insert_review_segment(&ReviewItemDto {
            id: "rev_multi_1".to_string(),
            camera: "front_door".to_string(),
            start_time: 1_000.0,
            end_time: Some(1_015.0),
            severity: "alert".to_string(),
            thumb_path: "/clips/rev_multi_1.webp".to_string(),
            data: json!({
                "detections": ["ev_1", "ev_2"],
                "objects": ["car", "person"],
                "sub_labels": ["Alice", "Bruno"],
                "zones": ["driveway"]
            }),
            has_been_reviewed: false,
        })
        .unwrap();

        // Insert unreviewed status rows for two distinct users -> query_ai_digest_summary must not duplicate rev_multi_1
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO userreviewstatus (user_id, review_segment_id, has_been_reviewed) VALUES ('user_a', 'rev_multi_1', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO userreviewstatus (user_id, review_segment_id, has_been_reviewed) VALUES ('user_b', 'rev_multi_1', 0)",
                [],
            )
            .unwrap();
        }
        let digest = db.query_ai_digest_summary(900.0, 1_100.0, None).unwrap();
        assert_eq!(digest["counts_by_severity"]["alert"], 1);
        assert_eq!(digest["unreviewed_alerts"].as_array().unwrap().len(), 1);

        // Clear ev_1's sub_label ("Alice") -> "Bruno" (from sibling ev_2) must remain on rev_multi_1
        db.set_event_sub_label_with_score("ev_1", None, None)
            .unwrap();
        let revs = db.query_reviews(&ReviewFilter::default()).unwrap();
        assert_eq!(
            revs[0].data["sub_labels"],
            json!(["Bruno"]),
            "clearing ev_1 sub_label must preserve sibling ev_2 sub_label on shared reviewsegment"
        );

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn clear_face_profile_sub_labels_removes_sub_labels_from_events_and_reviews_case_insensitively()
    {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_clear_face_sub_labels_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        for (ev_id, sub) in [
            ("ev_erik_1", "Erik"),
            ("ev_erik_2", "eRIK"),
            ("ev_alice_1", "Alice"),
        ] {
            db.upsert_event(&EventDto {
                id: ev_id.to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!(["porch"]),
                start_time: 1000.0,
                end_time: Some(1005.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: Some(sub.to_string()),
                top_score: Some(0.91),
                false_positive: Some(false),
                box_coords: Some(json!([0.1, 0.1, 0.4, 0.8])),
                data: json!({
                    "box": [0.1, 0.1, 0.4, 0.8],
                    "sub_label": sub,
                    "sub_label_score": 0.88
                }),
                thumbnail: None,
                area: Some(10000),
                ratio: Some(0.5),
                score: Some(0.91),
                stationary: false,
            })
            .unwrap();
        }

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_erik_1".to_string(),
            camera: "front_door".to_string(),
            start_time: 1000.0,
            end_time: Some(1010.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb.webp".to_string(),
            data: json!({
                "detections": ["ev_erik_1", "ev_erik_2"],
                "objects": ["person"],
                "sub_labels": ["Erik"],
                "sub_label_score": 0.88,
                "zones": ["porch"]
            }),
            has_been_reviewed: false,
        })
        .unwrap();

        let cleared = db.clear_face_profile_sub_labels("ERIK").unwrap();
        assert_eq!(cleared, 2);

        let ev1 = db.get_event("ev_erik_1").unwrap().unwrap();
        assert!(ev1["sub_label"].is_null());
        assert!(ev1["sub_label_score"].is_null());
        assert!(ev1["data"]["sub_label"].is_null());

        let ev2 = db.get_event("ev_erik_2").unwrap().unwrap();
        assert!(ev2["sub_label"].is_null());

        let ev_alice = db.get_event("ev_alice_1").unwrap().unwrap();
        assert_eq!(ev_alice["sub_label"], "Alice");

        let rev = db.get_review_by_id("rev_erik_1").unwrap().unwrap();
        assert_eq!(rev.data["sub_labels"], json!([]));
        assert!(rev.data.get("sub_label_score").is_none());

        let distinct = db.list_distinct_sub_labels().unwrap();
        assert_eq!(distinct, vec!["Alice"]);

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn share_link_and_get_export_by_id_crud_and_revocation_transitions_work() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_share_links_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. get_export_by_id for existing and non-existent IDs
        assert!(db.get_export_by_id("exp_missing").unwrap().is_none());
        db.insert_export(&ExportRow {
            id: "exp_share_1".to_string(),
            camera: "front_door".to_string(),
            name: "Incident Clip".to_string(),
            date: 1700000500.0,
            video_path: "/media/ninjanvr/exports/exp_share_1.mp4".to_string(),
            thumb_path: "/media/ninjanvr/clips/exp_share_1.webp".to_string(),
            in_progress: false,
        })
        .unwrap();

        let exp = db.get_export_by_id("exp_share_1").unwrap().unwrap();
        assert_eq!(exp["id"], "exp_share_1");
        assert_eq!(exp["camera"], "front_door");
        assert_eq!(exp["name"], "Incident Clip");
        assert_eq!(exp["date"], 1700000500.0);
        assert_eq!(exp["video_path"], "/media/ninjanvr/exports/exp_share_1.mp4");
        assert_eq!(exp["thumb_path"], "/media/ninjanvr/clips/exp_share_1.webp");
        assert_eq!(exp["in_progress"], false);
        assert!(exp["export_case_id"].is_null());

        // 2. Non-existent share link queries
        assert!(db.get_share_link("sl_missing").unwrap().is_none());
        assert!(!db.is_share_link_revoked("sl_missing").unwrap());
        assert!(!db.revoke_share_link("sl_missing").unwrap());
        assert!(db.list_share_links().unwrap().is_empty());

        // 3. Insert share links with camera Some(...) and None
        let s1 = db
            .insert_share_link(
                "sl_1",
                "export",
                "exp_share_1",
                Some("front_door"),
                "admin",
                1000.0,
                2000.0,
            )
            .unwrap();
        assert_eq!(s1["id"], "sl_1");
        assert_eq!(s1["kind"], "export");
        assert_eq!(s1["target_id"], "exp_share_1");
        assert_eq!(s1["camera"], "front_door");
        assert_eq!(s1["created_by"], "admin");
        assert_eq!(s1["created_at"], 1000.0);
        assert_eq!(s1["expires_at"], 2000.0);
        assert_eq!(s1["revoked"], false);

        let s2 = db
            .insert_share_link(
                "sl_2",
                "event",
                "ev_share_2",
                None,
                "operator1",
                1500.0,
                3000.0,
            )
            .unwrap();
        assert_eq!(s2["id"], "sl_2");
        assert!(s2["camera"].is_null());
        assert_eq!(s2["revoked"], false);

        // 4. get_share_link and list_share_links (ordered by created_at DESC)
        let fetched_s1 = db.get_share_link("sl_1").unwrap().unwrap();
        assert_eq!(fetched_s1["camera"], "front_door");
        assert_eq!(fetched_s1["revoked"], false);

        let fetched_s2 = db.get_share_link("sl_2").unwrap().unwrap();
        assert!(fetched_s2["camera"].is_null());

        let listed = db.list_share_links().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0]["id"], "sl_2");
        assert_eq!(listed[1]["id"], "sl_1");

        // 5. Revocation state transitions and re-insertion reset
        assert!(!db.is_share_link_revoked("sl_1").unwrap());
        assert!(db.revoke_share_link("sl_1").unwrap());
        assert!(db.is_share_link_revoked("sl_1").unwrap());
        assert_eq!(db.get_share_link("sl_1").unwrap().unwrap()["revoked"], true);
        assert!(!db.is_share_link_revoked("sl_2").unwrap());

        // Replacing an existing share link resets revoked to 0
        db.insert_share_link(
            "sl_1",
            "export",
            "exp_share_1",
            Some("front_door"),
            "admin",
            1800.0,
            3600.0,
        )
        .unwrap();
        assert!(!db.is_share_link_revoked("sl_1").unwrap());
        assert_eq!(
            db.get_share_link("sl_1").unwrap().unwrap()["revoked"],
            false
        );

        // 6. Bulk revocation by target (with Some(kind) and None) and by creator
        db.insert_share_link(
            "sl_3",
            "review",
            "exp_share_1",
            Some("front_door"),
            "operator2",
            1900.0,
            3600.0,
        )
        .unwrap();
        assert_eq!(
            db.revoke_share_links_for_target(Some("event"), "exp_share_1")
                .unwrap(),
            0
        );
        assert!(!db.is_share_link_revoked("sl_1").unwrap());
        assert!(!db.is_share_link_revoked("sl_3").unwrap());

        assert_eq!(
            db.revoke_share_links_for_target(Some("export"), "exp_share_1")
                .unwrap(),
            1
        );
        assert!(db.is_share_link_revoked("sl_1").unwrap());
        assert!(!db.is_share_link_revoked("sl_3").unwrap());

        // Reset sl_1 and test kind = None revoking across all kinds for target_id
        db.insert_share_link(
            "sl_1",
            "export",
            "exp_share_1",
            Some("front_door"),
            "admin",
            1950.0,
            3600.0,
        )
        .unwrap();
        assert_eq!(
            db.revoke_share_links_for_target(None, "exp_share_1")
                .unwrap(),
            2
        );
        assert!(db.is_share_link_revoked("sl_1").unwrap());
        assert!(db.is_share_link_revoked("sl_3").unwrap());
        assert!(!db.is_share_link_revoked("sl_2").unwrap());

        // Revoke by creator
        assert_eq!(db.revoke_share_links_by_creator("unknown_op").unwrap(), 0);
        assert!(!db.is_share_link_revoked("sl_2").unwrap());
        assert_eq!(db.revoke_share_links_by_creator("operator1").unwrap(), 1);
        assert!(db.is_share_link_revoked("sl_2").unwrap());

        // 7. Access counter, last_accessed_at, max_views, prune_expired_share_links, and LIMIT 500
        assert!(!db.record_share_link_access("sl_missing", 2100.0).unwrap());
        assert!(!db.record_share_link_access("sl_2", 2100.0).unwrap()); // sl_2 is revoked

        let s_views = db
            .insert_share_link_with_options(
                "sl_views_2",
                "export",
                "exp_share_1",
                Some("front_door"),
                "admin",
                2000.0,
                5000.0,
                Some(2),
            )
            .unwrap();
        assert_eq!(s_views["access_count"], 0);
        assert!(s_views["last_accessed_at"].is_null());
        assert_eq!(s_views["max_views"], 2);

        // View 1 succeeds
        assert!(db.record_share_link_access("sl_views_2", 2100.5).unwrap());
        let after_v1 = db.get_share_link("sl_views_2").unwrap().unwrap();
        assert_eq!(after_v1["access_count"], 1);
        assert_eq!(after_v1["last_accessed_at"], 2100.5);
        assert_eq!(after_v1["max_views"], 2);

        // View 2 succeeds
        assert!(db.record_share_link_access("sl_views_2", 2200.25).unwrap());
        let after_v2 = db.get_share_link("sl_views_2").unwrap().unwrap();
        assert_eq!(after_v2["access_count"], 2);
        assert_eq!(after_v2["last_accessed_at"], 2200.25);

        // View 3 fails (max_views = 2 reached) and does not mutate access_count or last_accessed_at
        assert!(!db.record_share_link_access("sl_views_2", 2300.0).unwrap());
        let after_v3 = db.get_share_link("sl_views_2").unwrap().unwrap();
        assert_eq!(after_v3["access_count"], 2);
        assert_eq!(after_v3["last_accessed_at"], 2200.25);

        // Unlimited link (max_views = None) increments without cap until revoked
        db.insert_share_link(
            "sl_unlim",
            "export",
            "exp_share_1",
            Some("front_door"),
            "admin",
            2050.0,
            6000.0,
        )
        .unwrap();
        for i in 1..=5 {
            assert!(
                db.record_share_link_access("sl_unlim", 2100.0 + i as f64)
                    .unwrap()
            );
        }
        let unlim_row = db.get_share_link("sl_unlim").unwrap().unwrap();
        assert_eq!(unlim_row["access_count"], 5);
        assert_eq!(unlim_row["last_accessed_at"], 2105.0);
        assert!(unlim_row["max_views"].is_null());

        // Prune expired share links (expires_at <= 3600.0 removes sl_1, sl_2, sl_3)
        let pruned = db.prune_expired_share_links(3600.0).unwrap();
        assert_eq!(pruned, 3);
        assert!(db.get_share_link("sl_1").unwrap().is_none());
        assert!(db.get_share_link("sl_2").unwrap().is_none());
        assert!(db.get_share_link("sl_3").unwrap().is_none());
        assert!(db.get_share_link("sl_views_2").unwrap().is_some());
        assert!(db.get_share_link("sl_unlim").unwrap().is_some());

        // Cap list_share_links at 500 rows
        for idx in 0..510 {
            db.insert_share_link(
                &format!("sl_bulk_{idx:04}"),
                "export",
                "exp_share_1",
                Some("front_door"),
                "admin",
                3000.0 + idx as f64,
                9000.0,
            )
            .unwrap();
        }
        let capped_list = db.list_share_links().unwrap();
        assert_eq!(capped_list.len(), 500);
        assert_eq!(capped_list[0]["id"], "sl_bulk_0509");

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn share_link_lifecycle_coupling_with_api_tokens_user_roles_and_retention_cleanup_works() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_share_lifecycle_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. API token scope/expiry check and delete_api_token share link revocation
        assert!(!db.is_api_token_active_by_name("", 1000.0).unwrap());
        assert!(!db.is_api_token_active_by_name("missing", 1000.0).unwrap());

        let (_ro_meta, _) = db.create_api_token("ro_bot", "read_only", None).unwrap();
        assert!(!db.is_api_token_active_by_name("ro_bot", 1000.0).unwrap());

        let (exp_meta, _) = db
            .create_api_token("temp_bot", "full", Some(2000.0))
            .unwrap();
        assert!(db.is_api_token_active_by_name("temp_bot", 1500.0).unwrap());
        assert!(!db.is_api_token_active_by_name("temp_bot", 2000.0).unwrap());
        assert!(!db.is_api_token_active_by_name("temp_bot", 2500.0).unwrap());
        let exp_tok_id = exp_meta["id"].as_str().unwrap();
        assert!(db.delete_api_token(exp_tok_id).unwrap());

        let (full_meta, _) = db.create_api_token("ci_bot", "full", None).unwrap();
        let full_tok_id = full_meta["id"].as_str().unwrap();
        assert!(db.is_api_token_active_by_name("ci_bot", 5000.0).unwrap());
        assert!(db.is_api_token_active_by_name(full_tok_id, 5000.0).unwrap());

        // Duplicate token name must be rejected
        let dup_err = db
            .create_api_token("  ci_bot  ", "read_only", None)
            .expect_err("duplicate API token name must be rejected");
        assert!(
            dup_err
                .to_string()
                .contains("An API token with name 'ci_bot' already exists"),
            "unexpected error: {dup_err}"
        );

        db.insert_share_link(
            "sl_tok_ci",
            "export",
            "exp_1",
            Some("front_door"),
            "token:ci_bot",
            1000.0,
            4000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_tok_ci_by_id",
            "export",
            "exp_1",
            Some("front_door"),
            &format!("token:{full_tok_id}"),
            1000.0,
            4000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_tok_other",
            "export",
            "exp_1",
            Some("front_door"),
            "token:other_bot",
            1000.0,
            4000.0,
        )
        .unwrap();

        assert!(db.delete_api_token(full_tok_id).unwrap());
        assert!(!db.is_api_token_active_by_name("ci_bot", 5000.0).unwrap());
        assert!(!db.is_api_token_active_by_name(full_tok_id, 5000.0).unwrap());
        assert!(db.is_share_link_revoked("sl_tok_ci").unwrap());
        assert!(db.is_share_link_revoked("sl_tok_ci_by_id").unwrap());
        assert!(!db.is_share_link_revoked("sl_tok_other").unwrap());

        // 2. update_user_role to viewer revokes share links created by that user
        db.create_user_with_cameras("admin_root", "StrongPass123!", "admin", "*", 1000)
            .unwrap();
        db.create_user_with_cameras("op_user", "StrongPass123!", "operator", "*", 1000)
            .unwrap();

        db.insert_share_link(
            "sl_op_1",
            "event",
            "ev_1",
            Some("front_door"),
            "op_user",
            1000.0,
            4000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_admin_1",
            "event",
            "ev_1",
            Some("front_door"),
            "admin_root",
            1000.0,
            4000.0,
        )
        .unwrap();

        assert!(db.update_user_role("op_user", "admin").unwrap());
        assert!(!db.is_share_link_revoked("sl_op_1").unwrap());

        assert!(db.update_user_role("op_user", "viewer").unwrap());
        assert!(db.is_share_link_revoked("sl_op_1").unwrap());
        assert!(!db.is_share_link_revoked("sl_admin_1").unwrap());

        // 3. cleanup_expired_events_and_reviews revokes share links and returns share_redacted_* / share_clip_* paths
        db.upsert_event(&EventDto {
            id: "ev_exp_share".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1010.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"max_severity": "alert"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();

        db.upsert_event(&EventDto {
            id: "ev_keep_share".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1010.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: true,
            sub_label: None,
            top_score: Some(0.95),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"max_severity": "alert"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.95),
            stationary: false,
        })
        .unwrap();

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_exp_share".to_string(),
            camera: "front_door".to_string(),
            start_time: 1000.0,
            end_time: Some(1015.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/review/thumb-rev_exp_share.webp".to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();

        db.insert_share_link(
            "sl_ev_exp",
            "event",
            "ev_exp_share",
            Some("front_door"),
            "admin_root",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_ev_keep",
            "event",
            "ev_keep_share",
            Some("front_door"),
            "admin_root",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_rev_exp",
            "review",
            "rev_exp_share",
            Some("front_door"),
            "admin_root",
            1000.0,
            5000.0,
        )
        .unwrap();

        let removed = db
            .cleanup_expired_events_and_reviews("front_door", 2000.0, 2000.0)
            .unwrap();

        assert!(db.is_share_link_revoked("sl_ev_exp").unwrap());
        assert!(db.is_share_link_revoked("sl_rev_exp").unwrap());
        assert!(!db.is_share_link_revoked("sl_ev_keep").unwrap());

        for id in ["ev_exp_share", "rev_exp_share"] {
            assert!(
                removed.contains(&format!("/media/ninjanvr/exports/share_redacted_{id}.mp4")),
                "missing share_redacted mp4 for {id}"
            );
            assert!(
                removed.contains(&format!(
                    "/media/ninjanvr/exports/.share_redacted_{id}.mask_hash"
                )),
                "missing mask_hash for {id}"
            );
            assert!(
                removed.contains(&format!("/media/ninjanvr/exports/share_clip_{id}.mp4")),
                "missing share_clip mp4 for {id}"
            );
            assert!(
                removed.contains(&format!("/media/ninjanvr/clips/share_redacted_{id}.webp")),
                "missing share_redacted webp for {id}"
            );
            assert!(
                removed.contains(&format!("/media/ninjanvr/clips/share_clip_{id}.webp")),
                "missing share_clip webp for {id}"
            );
        }

        // 4. list_expired_or_revoked_share_link_targets & has_active_share_links_for_target
        let targets = db
            .list_expired_or_revoked_share_link_targets(2500.0)
            .unwrap();
        assert!(targets.contains(&"ev_exp_share".to_string()));
        assert!(targets.contains(&"rev_exp_share".to_string()));
        assert!(!targets.contains(&"ev_keep_share".to_string()));

        assert!(
            !db.has_active_share_links_for_target("ev_exp_share", 2500.0)
                .unwrap()
        );
        assert!(
            !db.has_active_share_links_for_target("rev_exp_share", 2500.0)
                .unwrap()
        );
        assert!(
            db.has_active_share_links_for_target("ev_keep_share", 2500.0)
                .unwrap()
        );
        assert!(
            !db.has_active_share_links_for_target("ev_keep_share", 5000.0)
                .unwrap()
        );

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn share_link_ext_creator_camera_scoping_revocation_and_is_share_link_active_work() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_share_r4_db_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        db.create_user_with_cameras("admin_root", "StrongPass123!", "admin", "*", 1000)
            .unwrap();
        db.create_user_with_cameras(
            "scoped_op",
            "StrongPass123!",
            "operator",
            "front_door,backyard,garage",
            1000,
        )
        .unwrap();
        db.create_user_with_cameras("role_op", "StrongPass123!", "operator", "*", 1000)
            .unwrap();

        // 1. revoke_share_links_by_creator revokes both `<user>` and `ext:<user>` (trimmed)
        db.insert_share_link(
            "sl_creator_direct",
            "event",
            "ev_1",
            Some("front_door"),
            "del_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_creator_ext",
            "event",
            "ev_2",
            Some("front_door"),
            "ext:del_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_creator_other",
            "event",
            "ev_3",
            Some("front_door"),
            "ext:other_op",
            1000.0,
            5000.0,
        )
        .unwrap();

        let revoked_count = db.revoke_share_links_by_creator("  del_op  ").unwrap();
        assert_eq!(revoked_count, 2);
        assert!(db.is_share_link_revoked("sl_creator_direct").unwrap());
        assert!(db.is_share_link_revoked("sl_creator_ext").unwrap());
        assert!(!db.is_share_link_revoked("sl_creator_other").unwrap());

        // 2. update_user_role to viewer revokes both `<user>` and `ext:<user>` links
        db.insert_share_link(
            "sl_role_direct",
            "event",
            "ev_4",
            Some("front_door"),
            "role_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_role_ext",
            "event",
            "ev_5",
            Some("front_door"),
            "ext:role_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        assert!(db.update_user_role("  role_op  ", "viewer").unwrap());
        assert!(db.is_share_link_revoked("sl_role_direct").unwrap());
        assert!(db.is_share_link_revoked("sl_role_ext").unwrap());
        assert!(!db.is_share_link_revoked("sl_creator_other").unwrap());

        // 3. update_user_allowed_cameras revokes disallowed or NULL camera links for `<user>` and `ext:<user>`
        db.insert_share_link(
            "sl_cam_fd_direct",
            "event",
            "ev_fd",
            Some("front_door"),
            "scoped_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_cam_by_ext",
            "event",
            "ev_by",
            Some("backyard"),
            "ext:scoped_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_cam_gar_direct",
            "event",
            "ev_gar",
            Some("garage"),
            "scoped_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_cam_gar_ext",
            "event",
            "ev_gar_ext",
            Some("garage"),
            "ext:scoped_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_cam_null_direct",
            "export",
            "exp_null",
            None,
            "scoped_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_cam_null_ext",
            "export",
            "exp_null_ext",
            None,
            "ext:scoped_op",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_cam_admin_gar",
            "event",
            "ev_admin_gar",
            Some("garage"),
            "admin_root",
            1000.0,
            5000.0,
        )
        .unwrap();

        assert!(
            db.update_user_allowed_cameras("  scoped_op  ", " front_door , backyard ")
                .unwrap()
        );
        assert!(!db.is_share_link_revoked("sl_cam_fd_direct").unwrap());
        assert!(!db.is_share_link_revoked("sl_cam_by_ext").unwrap());
        assert!(db.is_share_link_revoked("sl_cam_gar_direct").unwrap());
        assert!(db.is_share_link_revoked("sl_cam_gar_ext").unwrap());
        assert!(db.is_share_link_revoked("sl_cam_null_direct").unwrap());
        assert!(db.is_share_link_revoked("sl_cam_null_ext").unwrap());
        assert!(!db.is_share_link_revoked("sl_cam_admin_gar").unwrap());

        // 4. is_share_link_active fail-closed semantics (missing, pruned, revoked, expired vs active)
        assert!(!db.is_share_link_active("sl_nonexistent", 2000.0).unwrap());
        // Active link (revoked == 0, expires_at == 5000.0 > now_ts)
        assert!(db.is_share_link_active("sl_cam_fd_direct", 2000.0).unwrap());
        assert!(db.is_share_link_active("sl_cam_fd_direct", 4999.9).unwrap());
        // Expired boundary and past expiry (expires_at <= now_ts)
        assert!(!db.is_share_link_active("sl_cam_fd_direct", 5000.0).unwrap());
        assert!(!db.is_share_link_active("sl_cam_fd_direct", 6000.0).unwrap());
        // Revoked link (revoked != 0 even when expires_at > now_ts)
        assert!(
            !db.is_share_link_active("sl_cam_gar_direct", 2000.0)
                .unwrap()
        );
        // Pruned link returns false
        db.prune_expired_share_links(6000.0).unwrap();
        assert!(db.get_share_link("sl_cam_fd_direct").unwrap().is_none());
        assert!(!db.is_share_link_active("sl_cam_fd_direct", 2000.0).unwrap());

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn share_link_expiry_access_guard_and_exhausted_max_views_target_lifecycle_work() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_share_quota_expiry_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. record_share_link_access rejects accesses at or after expires_at without mutating counters
        db.insert_share_link_with_options(
            "sl_exp_bound",
            "export",
            "exp_target_bound",
            Some("front_door"),
            "admin",
            1000.0,
            2000.0,
            Some(5),
        )
        .unwrap();

        // Access strictly before expires_at succeeds
        assert!(db.record_share_link_access("sl_exp_bound", 1999.5).unwrap());
        let before_exp = db.get_share_link("sl_exp_bound").unwrap().unwrap();
        assert_eq!(before_exp["access_count"], 1);
        assert_eq!(before_exp["last_accessed_at"], 1999.5);

        // Access at exact expires_at boundary (accessed_at == expires_at) and after expires_at returns Ok(false)
        assert!(!db.record_share_link_access("sl_exp_bound", 2000.0).unwrap());
        assert!(!db.record_share_link_access("sl_exp_bound", 2500.0).unwrap());
        let after_exp_attempt = db.get_share_link("sl_exp_bound").unwrap().unwrap();
        assert_eq!(after_exp_attempt["access_count"], 1);
        assert_eq!(after_exp_attempt["last_accessed_at"], 1999.5);

        // 2. Exhausted max_views link is included in list_expired_or_revoked_share_link_targets
        //    and excluded from has_active_share_links_for_target even before expires_at
        db.insert_share_link_with_options(
            "sl_single_use",
            "review",
            "rev_single_target",
            Some("front_door"),
            "admin",
            1000.0,
            9000.0,
            Some(1),
        )
        .unwrap();

        // Before first view: target is active and not in expired/revoked/exhausted candidate list
        assert!(
            db.has_active_share_links_for_target("rev_single_target", 1500.0)
                .unwrap()
        );
        let initial_candidates = db
            .list_expired_or_revoked_share_link_targets(1500.0)
            .unwrap();
        assert!(!initial_candidates.contains(&"rev_single_target".to_string()));

        // Consume the single allowed view well before expires_at (1600.0 < 9000.0)
        assert!(
            db.record_share_link_access("sl_single_use", 1600.0)
                .unwrap()
        );
        assert!(
            !db.record_share_link_access("sl_single_use", 1601.0)
                .unwrap()
        );

        // Once access_count reaches max_views, has_active_share_links_for_target returns Ok(false)
        assert!(
            !db.has_active_share_links_for_target("rev_single_target", 1700.0)
                .unwrap()
        );

        // And list_expired_or_revoked_share_link_targets includes the exhausted link's target before expires_at
        let exhausted_candidates = db
            .list_expired_or_revoked_share_link_targets(1700.0)
            .unwrap();
        assert!(exhausted_candidates.contains(&"rev_single_target".to_string()));

        // If a sibling non-exhausted link is created for the same target, has_active_share_links_for_target becomes true again
        db.insert_share_link_with_options(
            "sl_sibling_active",
            "review",
            "rev_single_target",
            Some("front_door"),
            "admin",
            1750.0,
            9000.0,
            Some(2),
        )
        .unwrap();
        assert!(
            db.has_active_share_links_for_target("rev_single_target", 1800.0)
                .unwrap()
        );

        // Exhausting the sibling link transitions has_active_share_links_for_target back to false
        assert!(
            db.record_share_link_access("sl_sibling_active", 1810.0)
                .unwrap()
        );
        assert!(
            db.has_active_share_links_for_target("rev_single_target", 1820.0)
                .unwrap()
        );
        assert!(
            db.record_share_link_access("sl_sibling_active", 1830.0)
                .unwrap()
        );
        assert!(
            !db.has_active_share_links_for_target("rev_single_target", 1840.0)
                .unwrap()
        );

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn has_recently_accessed_share_links_for_target_respects_grace_window_revocation_and_expiry() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_share_recent_grace_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        db.insert_share_link_with_options(
            "sl_grace_1",
            "review",
            "rev_grace_target",
            Some("front_door"),
            "admin",
            1000.0,
            9000.0,
            Some(1),
        )
        .unwrap();

        // Before access (last_accessed_at IS NULL): returns false even though link is active
        assert!(
            !db.has_recently_accessed_share_links_for_target("rev_grace_target", 1500.0, 900.0)
                .unwrap()
        );

        // Exhaust the max_views = 1 link at t = 1600.0
        assert!(db.record_share_link_access("sl_grace_1", 1600.0).unwrap());
        assert!(
            !db.has_active_share_links_for_target("rev_single_target", 1610.0)
                .unwrap()
        );
        assert!(
            !db.has_active_share_links_for_target("rev_grace_target", 1610.0)
                .unwrap()
        );

        // Within the 900s grace window (1600.0 + 900.0 = 2500.0 > now_ts): returns true
        assert!(
            db.has_recently_accessed_share_links_for_target("rev_grace_target", 1600.0, 900.0)
                .unwrap()
        );
        assert!(
            db.has_recently_accessed_share_links_for_target("rev_grace_target", 1700.0, 900.0)
                .unwrap()
        );
        assert!(
            db.has_recently_accessed_share_links_for_target("rev_grace_target", 2499.9, 900.0)
                .unwrap()
        );

        // At or after the 900s grace window elapses (1600.0 + 900.0 <= now_ts): returns false
        assert!(
            !db.has_recently_accessed_share_links_for_target("rev_grace_target", 2500.0, 900.0)
                .unwrap()
        );
        assert!(
            !db.has_recently_accessed_share_links_for_target("rev_grace_target", 3000.0, 900.0)
                .unwrap()
        );

        // Expired link within 900s of last_accessed_at returns false
        db.insert_share_link_with_options(
            "sl_grace_exp",
            "review",
            "rev_grace_exp_target",
            Some("front_door"),
            "admin",
            1000.0,
            2000.0,
            Some(1),
        )
        .unwrap();
        assert!(db.record_share_link_access("sl_grace_exp", 1950.0).unwrap());
        // Before expiry (1990.0 < 2000.0) and within 900s: true
        assert!(
            db.has_recently_accessed_share_links_for_target("rev_grace_exp_target", 1990.0, 900.0)
                .unwrap()
        );
        // At or after expiry (2000.0 and 2100.0) despite 1950.0 + 900.0 > now_ts: false
        assert!(
            !db.has_recently_accessed_share_links_for_target("rev_grace_exp_target", 2000.0, 900.0)
                .unwrap()
        );
        assert!(
            !db.has_recently_accessed_share_links_for_target("rev_grace_exp_target", 2100.0, 900.0)
                .unwrap()
        );

        // Revoked link within 900s of last_accessed_at returns false
        db.insert_share_link_with_options(
            "sl_grace_rev",
            "review",
            "rev_grace_rev_target",
            Some("front_door"),
            "admin",
            1000.0,
            9000.0,
            Some(1),
        )
        .unwrap();
        assert!(db.record_share_link_access("sl_grace_rev", 1600.0).unwrap());
        assert!(
            db.has_recently_accessed_share_links_for_target("rev_grace_rev_target", 1700.0, 900.0)
                .unwrap()
        );
        assert!(db.revoke_share_link("sl_grace_rev").unwrap());
        assert!(
            !db.has_recently_accessed_share_links_for_target("rev_grace_rev_target", 1700.0, 900.0)
                .unwrap()
        );

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn touch_share_link_last_accessed_extends_grace_window_and_cleanup_filters_unsafe_ids() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_share_touch_cleanup_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. touch_share_link_last_accessed on missing link returns false
        assert!(
            !db.touch_share_link_last_accessed("sl_missing", 1500.0)
                .unwrap()
        );

        db.insert_share_link_with_options(
            "sl_touch_1",
            "review",
            "rev_touch_target",
            Some("front_door"),
            "admin",
            1000.0,
            9000.0,
            Some(1),
        )
        .unwrap();

        // Touching when last_accessed_at IS NULL sets timestamp without incrementing access_count
        assert!(
            db.touch_share_link_last_accessed("sl_touch_1", 1500.0)
                .unwrap()
        );
        let after_initial_touch = db.get_share_link("sl_touch_1").unwrap().unwrap();
        assert_eq!(after_initial_touch["access_count"], 0);
        assert_eq!(after_initial_touch["last_accessed_at"], 1500.0);

        // Consume the single view at t = 1600.0 (access_count == 1 == max_views)
        assert!(db.record_share_link_access("sl_touch_1", 1600.0).unwrap());
        assert!(!db.record_share_link_access("sl_touch_1", 1605.0).unwrap());

        // Without extending last_accessed_at, at t = 2505.0 (> 1600.0 + 900.0), grace window would be expired
        assert!(
            !db.has_recently_accessed_share_links_for_target("rev_touch_target", 2505.0, 900.0)
                .unwrap()
        );

        // Active continuation playback touches last_accessed_at at t = 2300.0 on the exhausted link
        assert!(
            db.touch_share_link_last_accessed("sl_touch_1", 2300.0)
                .unwrap()
        );
        let after_stream_touch = db.get_share_link("sl_touch_1").unwrap().unwrap();
        assert_eq!(after_stream_touch["access_count"], 1);
        assert_eq!(after_stream_touch["last_accessed_at"], 2300.0);

        // Older timestamp does not regress last_accessed_at
        assert!(
            db.touch_share_link_last_accessed("sl_touch_1", 2000.0)
                .unwrap()
        );
        assert_eq!(
            db.get_share_link("sl_touch_1").unwrap().unwrap()["last_accessed_at"],
            2300.0
        );

        // Now at t = 2505.0 (< 2300.0 + 900.0 = 3200.0), target remains protected by grace window
        assert!(
            db.has_recently_accessed_share_links_for_target("rev_touch_target", 2505.0, 900.0)
                .unwrap()
        );
        assert!(
            !db.has_recently_accessed_share_links_for_target("rev_touch_target", 3200.0, 900.0)
                .unwrap()
        );

        // Revoking the share link causes touch_share_link_last_accessed to reject updates
        assert!(db.revoke_share_link("sl_touch_1").unwrap());
        assert!(
            !db.touch_share_link_last_accessed("sl_touch_1", 4000.0)
                .unwrap()
        );
        assert_eq!(
            db.get_share_link("sl_touch_1").unwrap().unwrap()["last_accessed_at"],
            2300.0
        );

        // 2. cleanup_expired_events_and_reviews only pushes derived paths for safe IDs
        for id in [
            "ev_safe_ok.1",
            "../ev_traversal",
            "ev/slash",
            "ev..dots",
            "ev$bad",
        ] {
            db.upsert_event(&EventDto {
                id: id.to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1000.0,
                end_time: Some(1010.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({"max_severity": "alert"}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
            db.insert_share_link(
                &format!("sl_ev_{id}"),
                "event",
                id,
                Some("front_door"),
                "admin",
                1000.0,
                9000.0,
            )
            .unwrap();
        }

        for id in [
            "rev_safe-ok_1",
            "../rev_traversal",
            "rev/slash",
            "rev..dots",
        ] {
            db.insert_review_segment(&ReviewItemDto {
                id: id.to_string(),
                camera: "front_door".to_string(),
                start_time: 1000.0,
                end_time: Some(1010.0),
                severity: "alert".to_string(),
                thumb_path: String::new(),
                data: json!({}),
                has_been_reviewed: false,
            })
            .unwrap();
            db.insert_share_link(
                &format!("sl_rev_{id}"),
                "review",
                id,
                Some("front_door"),
                "admin",
                1000.0,
                9000.0,
            )
            .unwrap();
        }

        let removed = db
            .cleanup_expired_events_and_reviews("front_door", 2000.0, 2000.0)
            .unwrap();

        // Safe IDs have their clip and share paths included
        assert!(removed.contains(&"/media/ninjanvr/clips/ev_safe_ok.1.jpg".to_string()));
        assert!(
            removed.contains(&"/media/ninjanvr/clips/front_door-ev_safe_ok.1.webp".to_string())
        );
        assert!(
            removed
                .contains(&"/media/ninjanvr/exports/share_redacted_ev_safe_ok.1.mp4".to_string())
        );
        assert!(
            removed
                .contains(&"/media/ninjanvr/exports/share_redacted_rev_safe-ok_1.mp4".to_string())
        );

        // Unsafe IDs never produce derived paths in files_to_remove, though their DB rows and share links are cleaned up
        for unsafe_id in [
            "../ev_traversal",
            "ev/slash",
            "ev..dots",
            "ev$bad",
            "../rev_traversal",
            "rev/slash",
            "rev..dots",
        ] {
            assert!(
                !removed.iter().any(|p| p.contains(unsafe_id)),
                "unsafe id {unsafe_id} must not be interpolated into files_to_remove: {removed:?}"
            );
        }
        assert!(db.get_event("../ev_traversal").unwrap().is_none());
        assert!(db.get_review_by_id("../rev_traversal").unwrap().is_none());
        assert!(db.is_share_link_revoked("sl_ev_../ev_traversal").unwrap());
        assert!(db.is_share_link_revoked("sl_rev_../rev_traversal").unwrap());

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn audit_p0_01_pbkdf2_zero_and_extreme_iterations_and_salt_sanitization_work() {
        // 1. Zero-iteration PBKDF2 hash string must never verify against any password
        let zero_iter_hash =
            "pbkdf2_sha256$0$e1c16db9bfff2200$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        assert!(!verify_password("any-password", zero_iter_hash));
        assert!(!verify_password("", zero_iter_hash));

        // Sub-minimum (< 1000) and excessive (> 10_000_000) iterations are rejected immediately
        assert!(!verify_password(
            "pass",
            "pbkdf2_sha256$999$e1c16db9bfff2200$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        ));
        assert!(!verify_password(
            "pass",
            "pbkdf2_sha256$10000001$e1c16db9bfff2200$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        ));
        assert!(!verify_password(
            "pass",
            "pbkdf2_sha256$4294967295$e1c16db9bfff2200$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        ));

        // 2. hash_password(0) clamps to MIN_PBKDF2_ITERATIONS (1000) and sanitizes invalid salts
        let clamped = hash_password("Secret!2026", Some("salthex"), 0);
        assert!(clamped.starts_with("pbkdf2_sha256$1000$salthex$"));
        assert!(verify_password("Secret!2026", &clamped));
        assert!(!verify_password("Wrong!2026", &clamped));

        let sanitized_dollar = hash_password("Secret!2026", Some("bad$salt"), 1000);
        assert_eq!(sanitized_dollar.split('$').count(), 4);
        assert!(verify_password("Secret!2026", &sanitized_dollar));

        let sanitized_empty = hash_password("Secret!2026", Some(""), 1000);
        assert!(verify_password("Secret!2026", &sanitized_empty));

        // 3. ensure_default_admin(0) clamps iterations and restores admin role/wildcard cameras
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_audit_p0_01_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();
        db.execute_batch(
            "INSERT INTO user (username, role, allowed_cameras, password_hash, notification_tokens)
             VALUES ('admin', 'viewer', 'front_door', '', '[]');",
        )
        .unwrap();
        let gen_pw = db.ensure_default_admin(0).unwrap().unwrap();
        let admin = db.get_user("  admin  ").unwrap().unwrap();
        assert_eq!(admin.role, "admin");
        assert_eq!(admin.allowed_cameras, "*");
        assert!(admin.password_hash.starts_with("pbkdf2_sha256$1000$"));
        assert!(verify_password(&gen_pw, &admin.password_hash));

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn audit_p0_02_api_token_and_share_link_nan_inf_and_corrupted_column_fail_closed() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_audit_p0_02_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. create_api_token rejects NaN, Infinity, zero, and negative expires_at
        for bad_exp in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -100.0] {
            assert!(
                db.create_api_token("nan_tok", "full", Some(bad_exp))
                    .is_err(),
                "create_api_token must reject non-finite or non-positive expires_at={bad_exp}"
            );
        }

        let (meta, plain) = db
            .create_api_token("exp_tok", "full", Some(2000.0))
            .unwrap();
        let tok_id = meta["id"].as_str().unwrap().to_string();

        // 2. verify_api_token and is_api_token_active_by_name reject non-finite now_ts
        for bad_now in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(db.verify_api_token(&plain, bad_now).unwrap().is_none());
            assert!(!db.is_api_token_active_by_name("exp_tok", bad_now).unwrap());
        }

        // Corrupted expires_at text or invalid scope in api_token fails closed
        db.execute_batch(&format!(
            "UPDATE api_token SET expires_at = 'not_a_timestamp' WHERE id = '{tok_id}';"
        ))
        .unwrap();
        assert!(db.verify_api_token(&plain, 1000.0).unwrap().is_none());

        db.execute_batch(&format!(
            "UPDATE api_token SET expires_at = 5000.0, scope = 'invalid_root' WHERE id = '{tok_id}';"
        ))
        .unwrap();
        assert!(db.verify_api_token(&plain, 1000.0).unwrap().is_none());

        // 3. Share link non-finite timestamps and corrupted revoked column fail closed
        assert!(
            db.insert_share_link("sl_nan", "event", "ev_1", None, "admin", f64::NAN, 2000.0)
                .is_err()
        );
        assert!(
            db.insert_share_link(
                "sl_inf",
                "event",
                "ev_1",
                None,
                "admin",
                1000.0,
                f64::INFINITY
            )
            .is_err()
        );

        db.insert_share_link_with_options(
            "sl_ok",
            "event",
            "ev_1",
            Some("front_door"),
            "admin",
            1000.0,
            2000.0,
            Some(5),
        )
        .unwrap();
        assert!(db.record_share_link_access("sl_ok", 1500.0).unwrap());

        for bad_ts in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(!db.is_share_link_active("sl_ok", bad_ts).unwrap());
            assert!(
                !db.has_active_share_links_for_target("ev_1", bad_ts)
                    .unwrap()
            );
            assert!(
                !db.has_recently_accessed_share_links_for_target("ev_1", bad_ts, 900.0)
                    .unwrap()
            );
            assert!(
                !db.has_recently_accessed_share_links_for_target("ev_1", 1600.0, bad_ts)
                    .unwrap()
            );
            assert!(!db.record_share_link_access("sl_ok", bad_ts).unwrap());
            assert!(!db.touch_share_link_last_accessed("sl_ok", bad_ts).unwrap());
        }

        // Corrupting share_link.revoked to non-integer text defaults to revoked=true in map_share_link_row
        db.execute_batch("UPDATE share_link SET revoked = 'corrupted' WHERE id = 'sl_ok';")
            .unwrap();
        let row = db.get_share_link("sl_ok").unwrap().unwrap();
        assert_eq!(row["revoked"], true);

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn audit_p0_03_user_deletion_and_admin_to_operator_demotion_cascade_share_links() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_audit_p0_03_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        db.create_user_with_cameras("root_admin", "StrongPass!1", "admin", "*", 1000)
            .unwrap();
        db.create_user_with_cameras("scoped_admin", "StrongPass!2", "admin", "front_door", 1000)
            .unwrap();
        db.create_user_with_cameras("temp_operator", "StrongPass!3", "operator", "*", 1000)
            .unwrap();

        // 1. Demoting scoped_admin (allowed_cameras="front_door") from admin to operator
        //    revokes backyard and NULL-camera links while preserving front_door link
        db.insert_share_link(
            "sl_sa_fd",
            "event",
            "ev_1",
            Some("front_door"),
            "scoped_admin",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_sa_by",
            "event",
            "ev_2",
            Some("backyard"),
            "scoped_admin",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_sa_ext_null",
            "export",
            "exp_1",
            None,
            "ext:scoped_admin",
            1000.0,
            5000.0,
        )
        .unwrap();

        assert!(db.update_user_role("scoped_admin", "operator").unwrap());
        assert!(!db.is_share_link_revoked("sl_sa_fd").unwrap());
        assert!(db.is_share_link_revoked("sl_sa_by").unwrap());
        assert!(db.is_share_link_revoked("sl_sa_ext_null").unwrap());

        // 2. Deleting a user atomically revokes both `<user>` and `ext:<user>` share links
        db.insert_share_link(
            "sl_del_u1",
            "event",
            "ev_1",
            Some("front_door"),
            "temp_operator",
            1000.0,
            5000.0,
        )
        .unwrap();
        db.insert_share_link(
            "sl_del_u1_ext",
            "event",
            "ev_2",
            Some("front_door"),
            "ext:temp_operator",
            1000.0,
            5000.0,
        )
        .unwrap();

        assert!(db.delete_user("temp_operator").unwrap());
        assert!(db.is_share_link_revoked("sl_del_u1").unwrap());
        assert!(db.is_share_link_revoked("sl_del_u1_ext").unwrap());
        assert!(!db.is_share_link_revoked("sl_sa_fd").unwrap());

        // 3. create_api_token prevents creating a token whose name collides with an existing token's id
        let (tok_meta, _) = db.create_api_token("bot_one", "read_only", None).unwrap();
        let tok_id = tok_meta["id"].as_str().unwrap();
        assert!(db.create_api_token(tok_id, "full", None).is_err());

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn audit_p0_04_quota_and_retention_reject_nan_inf_and_unsafe_camera_paths() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_audit_p0_04_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        db.insert_recording(&RecordingRow {
            id: "rec_keep_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/front_door/rec_keep_1.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 0,
            objects: 0,
            dbfs: 0,
            segment_size: 25.0,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();

        // 1. Quota eviction with 0.0, negative, NaN, or Infinity must never wipe recordings
        for bad_quota in [0.0, -10.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                db.delete_recordings_over_quota_mb("front_door", bad_quota)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                db.delete_global_recordings_over_quota_mb(bad_quota)
                    .unwrap()
                    .is_empty()
            );
        }

        // 2. Retention cleanup with NaN or Infinity cutoff timestamps must not wipe live recordings
        for bad_cutoff in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                db.delete_expired_recordings_with_window(
                    "front_door",
                    bad_cutoff,
                    bad_cutoff,
                    bad_cutoff,
                    5.0,
                    5.0
                )
                .unwrap()
                .is_empty()
            );
            assert!(
                db.delete_expired_idle_main_recordings("front_door", bad_cutoff)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                db.cleanup_expired_events_and_reviews("front_door", bad_cutoff, bad_cutoff)
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            db.query_recordings("front_door", 900.0, 1100.0)
                .unwrap()
                .len(),
            1
        );

        // 3. cleanup_expired_events_and_reviews filters unsafe camera and traversal thumb_path/preview paths
        db.upsert_event(&EventDto {
            id: "ev_cam_trav".to_string(),
            camera: "../bad_cam".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 100.0,
            end_time: Some(110.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"max_severity": "alert"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();
        db.insert_review_segment(&ReviewItemDto {
            id: "rev_thumb_trav".to_string(),
            camera: "../bad_cam".to_string(),
            start_time: 100.0,
            end_time: Some(110.0),
            severity: "alert".to_string(),
            thumb_path: "/media/ninjanvr/clips/../../etc/passwd".to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();

        let removed = db
            .cleanup_expired_events_and_reviews("../bad_cam", 500.0, 500.0)
            .unwrap();
        assert!(!removed.iter().any(|p| p.contains("..")));

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn audit_p1_01_parameterized_queries_like_escaping_and_zero_limit_work() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_audit_p1_01_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. Sub-labels and camera filters with single quotes work accurately and safely
        db.upsert_event(&EventDto {
            id: "ev_quote_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!(["porch"]),
            start_time: 1000.0,
            end_time: Some(1005.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: Some("O'Connor".to_string()),
            top_score: Some(0.92),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.92),
            stationary: false,
        })
        .unwrap();

        let matched_quote = db
            .query_events(&EventFilter {
                sub_labels: Some(vec!["O'Connor".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(matched_quote.len(), 1);
        assert_eq!(matched_quote[0]["id"], "ev_quote_1");

        let sqli_attempt = db
            .query_events(&EventFilter {
                sub_labels: Some(vec!["' OR 1=1 --".to_string()]),
                ..Default::default()
            })
            .unwrap();
        assert!(sqli_attempt.is_empty());

        // limit: Some(0) returns empty Vec and NaN bounds return empty Vec without SQL error
        assert!(
            db.query_events(&EventFilter {
                limit: Some(0),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
        );
        assert!(
            db.query_events(&EventFilter {
                before: Some(f64::NAN),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
        );

        // 2. get_review_by_event_id escapes LIKE wildcards (%, _) and checks data.detections
        db.insert_review_segment(&ReviewItemDto {
            id: "rev_real".to_string(),
            camera: "front_door".to_string(),
            start_time: 1000.0,
            end_time: Some(1010.0),
            severity: "alert".to_string(),
            thumb_path: "/clips/rev_real.webp".to_string(),
            data: json!({
                "detections": ["ev_1_1"],
                "objects": ["person"]
            }),
            has_been_reviewed: false,
        })
        .unwrap();
        db.insert_review_segment(&ReviewItemDto {
            id: "rev_newer_false_match".to_string(),
            camera: "backyard".to_string(),
            start_time: 2000.0,
            end_time: Some(2010.0),
            severity: "alert".to_string(),
            thumb_path: "/clips/rev_newer.webp".to_string(),
            data: json!({
                "detections": ["ev_1X1"],
                "notes": "unrelated to \"ev_1_1\""
            }),
            has_been_reviewed: false,
        })
        .unwrap();

        assert!(db.get_review_by_event_id("%").unwrap().is_none());
        assert!(db.get_review_by_event_id("___").unwrap().is_none());
        let found = db.get_review_by_event_id("ev_1_1").unwrap().unwrap();
        assert_eq!(found.id, "rev_real");

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn audit_p1_02_corrupted_json_resilience_and_upsert_event_plate_preservation() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_audit_p1_02_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. Seed an event and reviewsegment with corrupted non-JSON `data`
        db.upsert_event(&EventDto {
            id: "ev_corrupt".to_string(),
            camera: "front_door".to_string(),
            label: "car".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1010.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: Some("Erik".to_string()),
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: None,
            data: json!({}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();
        db.execute_batch("UPDATE event SET data = '{corrupted_json' WHERE id = 'ev_corrupt';")
            .unwrap();

        // Mutations and retention cleanup must not fail with SQLite `malformed JSON`
        assert_eq!(
            db.set_event_notes("ev_corrupt", "repaired note").unwrap(),
            1
        );
        assert_eq!(
            db.set_event_sub_label_with_score("ev_corrupt", Some("Erik"), Some(0.95))
                .unwrap(),
            1
        );
        db.execute_batch("UPDATE event SET data = '' WHERE id = 'ev_corrupt';")
            .unwrap();
        assert_eq!(db.clear_face_profile_sub_labels("Erik").unwrap(), 1);

        // 2. upsert_event preserves recognized_license_plate and attributes across track updates
        let mut ev_plate = EventDto {
            id: "ev_plate_track".to_string(),
            camera: "front_door".to_string(),
            label: "car".to_string(),
            zones: json!(["driveway"]),
            start_time: 3000.0,
            end_time: None,
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.93),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"attributes": ["color:silver"]}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.93),
            stationary: false,
        };
        db.upsert_event(&ev_plate).unwrap();
        db.set_event_license_plate("ev_plate_track", Some("xyz-9876"))
            .unwrap();

        // Track ends with a fresh data payload lacking recognized_license_plate
        ev_plate.end_time = Some(3012.0);
        ev_plate.data = json!({"type": "object"});
        db.upsert_event(&ev_plate).unwrap();

        let fetched = db.get_event("ev_plate_track").unwrap().unwrap();
        assert_eq!(fetched["data"]["recognized_license_plate"], "XYZ9876");

        // 3. cleanup_expired_events_and_reviews succeeds even when event.data is malformed JSON
        db.execute_batch("UPDATE event SET data = '{bad_json' WHERE id = 'ev_corrupt';")
            .unwrap();
        let cleaned = db
            .cleanup_expired_events_and_reviews("front_door", 2000.0, 2000.0)
            .unwrap();
        assert!(!cleaned.is_empty());
        assert!(db.get_event("ev_corrupt").unwrap().is_none());

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn audit_p1_03_mutex_poison_recovery_callback_lock_safety_and_overflow_guards() {
        let tmp =
            std::env::temp_dir().join(format!("ninjanvr_audit_p1_03_{}.db", rand::random::<u32>()));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. Poisoning the internal mutex on another thread does not brick NinjaDatabase
        let db_clone = db.clone();
        let _ = std::thread::spawn(move || {
            let _guard = db_clone.conn.lock().unwrap();
            panic!("simulated thread panic while holding db lock");
        })
        .join();
        assert!(db.list_users().is_ok());

        // 2. prune_missing_recording_files releases the DB lock before invoking file_exists,
        //    so calling a DB read method inside file_exists does not deadlock
        db.insert_recording(&RecordingRow {
            id: "rec_cb_1".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/front_door/rec_cb_1.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 0,
            objects: 0,
            dbfs: 0,
            segment_size: 1.0,
            regions: 0,
            motion_heatmap: None,
        })
        .unwrap();
        let pruned = db
            .prune_missing_recording_files(|_path| {
                let _ = db.list_users().unwrap();
                false
            })
            .unwrap();
        assert_eq!(pruned, 1);

        // 3. compute_norm_box_iou and match_false_positive_signature handle NaN/Inf and i64::MAX hit_count safely
        assert_eq!(
            compute_norm_box_iou([f64::NAN, 0.0, 1.0, 1.0], [0.0, 0.0, 1.0, 1.0]),
            0.0
        );
        assert_eq!(
            compute_norm_box_iou(
                [f64::NEG_INFINITY, 0.0, f64::INFINITY, 1.0],
                [0.0, 0.0, 1.0, 1.0]
            ),
            0.0
        );

        db.execute_batch(&format!(
            "INSERT INTO false_positive_signature (
                camera, label, norm_x1, norm_y1, norm_x2, norm_y2,
                dhash64, dominant_color, hit_count, source_event_id,
                reason, created_at, last_seen_at
            ) VALUES ('front_door', 'person', 0.1, 0.1, 0.5, 0.5, NULL, NULL, {}, 'ev_max', NULL, 1000.0, 1000.0);",
            i64::MAX
        ))
        .unwrap();
        let matched = db
            .match_false_positive_signature("front_door", "person", [0.1, 0.1, 0.5, 0.5], None)
            .unwrap()
            .unwrap();
        assert_eq!(matched["hit_count"], i64::MAX);

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn cycle2_p0_01_disjoint_xyxy_boxes_in_upper_left_and_center_quadrants_never_match_fp_signature()
     {
        // Disjoint [x1, y1, x2, y2] boxes where x1 + x2 <= 1.0 and y1 + y2 <= 1.0
        // must evaluate to 0.0 IoU instead of being misinterpreted as [x, y, w, h].
        assert_eq!(
            compute_norm_box_iou([0.40, 0.40, 0.45, 0.45], [0.40, 0.45, 0.45, 0.50]),
            0.0
        );
        assert_eq!(
            compute_norm_box_iou([0.47, 0.47, 0.49, 0.49], [0.495, 0.495, 0.50, 0.50]),
            0.0
        );
        assert_eq!(
            compute_norm_box_iou([0.10, 0.10, 0.20, 0.20], [0.25, 0.25, 0.35, 0.35]),
            0.0
        );

        // Barely-overlapping [x1, y1, x2, y2] boxes in the upper-left/center quadrant
        // must return their true corner IoU (< 0.10, NOT >= 0.72).
        let iou_touch_y =
            compute_norm_box_iou([0.40, 0.40, 0.45, 0.45], [0.40, 0.4499, 0.45, 0.50]);
        assert!(iou_touch_y > 0.0 && iou_touch_y < 0.01);

        let iou_corner_overlap =
            compute_norm_box_iou([0.44, 0.44, 0.47, 0.47], [0.46, 0.46, 0.50, 0.50]);
        assert!((iou_corner_overlap - (0.0001 / 0.0024)).abs() < 1e-6);
        assert!(iou_corner_overlap < 0.10);

        // Shifted [x, y, w, h] boxes where x < w and y < h and a[2] <= b[0]
        // must return their true [x, y, w, h] IoU (~0.80 >= 0.72, NOT 0.0).
        let iou_shifted_xywh =
            compute_norm_box_iou([0.20, 0.15, 0.22, 0.50], [0.22, 0.15, 0.23, 0.50]);
        assert!((iou_shifted_xywh - 0.80).abs() < 1e-6);

        // 1. Disjoint [x1, y1, x2, y2] with different heights -> 0.0
        assert_eq!(
            compute_norm_box_iou([0.40, 0.40, 0.45, 0.45], [0.40, 0.45, 0.45, 0.465]),
            0.0
        );

        // 2. Concentric / corner-scaled [x1, y1, x2, y2] -> ~0.1111 and 0.25 (< 0.72)
        let iou_concentric =
            compute_norm_box_iou([0.40, 0.40, 0.45, 0.45], [0.35, 0.35, 0.50, 0.50]);
        assert!((iou_concentric - (1.0 / 9.0)).abs() < 1e-6);
        assert!(iou_concentric < 0.72);

        let iou_corner_scaled =
            compute_norm_box_iou([0.40, 0.40, 0.44, 0.44], [0.40, 0.40, 0.48, 0.48]);
        assert!((iou_corner_scaled - 0.25).abs() < 1e-6);
        assert!(iou_corner_scaled < 0.72);

        // 3. Disjoint slender [x1, y1, x2, y2] -> 0.0
        assert_eq!(
            compute_norm_box_iou([0.40, 0.30, 0.43, 0.46], [0.43, 0.30, 0.46, 0.46]),
            0.0
        );

        // 4. Shifted [x, y, w, h] boxes -> ~0.7843 and 0.75 (>= 0.72)
        let iou_shifted_xywh_2 =
            compute_norm_box_iou([0.20, 0.15, 0.22, 0.22], [0.22, 0.15, 0.235, 0.22]);
        assert!((iou_shifted_xywh_2 - (0.20 / 0.255)).abs() < 1e-6);
        assert!(iou_shifted_xywh_2 >= 0.72);

        let iou_shifted_xywh_3 =
            compute_norm_box_iou([0.10, 0.10, 0.20, 0.20], [0.12, 0.10, 0.22, 0.20]);
        assert!((iou_shifted_xywh_3 - 0.75).abs() < 1e-6);
        assert!(iou_shifted_xywh_3 >= 0.72);

        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_cycle2_p0_01_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        db.upsert_event(&EventDto {
            id: "ev_fp_center".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1005.0),
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.85),
            false_positive: Some(false),
            box_coords: Some(json!([0.40, 0.40, 0.45, 0.45])),
            data: json!({"box": [0.40, 0.40, 0.45, 0.45]}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.85),
            stationary: true,
        })
        .unwrap();
        assert_eq!(
            db.set_event_false_positive("ev_fp_center", true).unwrap(),
            1
        );

        // Disjoint or barely-overlapping [x1, y1, x2, y2] boxes must NOT match the false-positive signature
        let disjoint_match = db
            .match_false_positive_signature("front_door", "person", [0.40, 0.45, 0.45, 0.50], None)
            .unwrap();
        assert!(disjoint_match.is_none());

        let barely_overlapping_match = db
            .match_false_positive_signature(
                "front_door",
                "person",
                [0.40, 0.4499, 0.45, 0.50],
                None,
            )
            .unwrap();
        assert!(barely_overlapping_match.is_none());

        // Shifted [x, y, w, h] false-positive signature at [0.20, 0.15, 0.22, 0.50] matches [0.22, 0.15, 0.23, 0.50]
        db.upsert_event(&EventDto {
            id: "ev_fp_xywh".to_string(),
            camera: "patio".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1010.0,
            end_time: Some(1015.0),
            has_clip: false,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.85),
            false_positive: Some(false),
            box_coords: Some(json!([0.20, 0.15, 0.22, 0.50])),
            data: json!({"box": [0.20, 0.15, 0.22, 0.50]}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.85),
            stationary: true,
        })
        .unwrap();
        assert_eq!(db.set_event_false_positive("ev_fp_xywh", true).unwrap(), 1);
        let shifted_xywh_match = db
            .match_false_positive_signature("patio", "person", [0.22, 0.15, 0.23, 0.50], None)
            .unwrap();
        assert!(shifted_xywh_match.is_some());

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn cycle2_p0_02_sqlite_type_affinity_corrupted_token_and_share_link_columns_fail_closed() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_cycle2_p0_02_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        let (tok_meta, _plain) = db
            .create_api_token("ci_full_tok", "full", Some(3000.0))
            .unwrap();
        let tok_id = tok_meta["id"].as_str().unwrap().to_string();
        assert!(
            db.is_api_token_active_by_name("ci_full_tok", 2000.0)
                .unwrap()
        );

        // Corrupt expires_at to non-numeric TEXT or +Inf: is_api_token_active_by_name must fail closed
        db.execute_batch(&format!(
            "UPDATE api_token SET expires_at = 'corrupted_text' WHERE id = '{tok_id}';"
        ))
        .unwrap();
        assert!(
            !db.is_api_token_active_by_name("ci_full_tok", 2000.0)
                .unwrap()
        );
        db.execute_batch(&format!(
            "UPDATE api_token SET expires_at = 9e999 WHERE id = '{tok_id}';"
        ))
        .unwrap();
        assert!(
            !db.is_api_token_active_by_name("ci_full_tok", 2000.0)
                .unwrap()
        );

        // Share link: touch_share_link_last_accessed must reject expired links (accessed_at >= expires_at)
        db.insert_share_link_with_options(
            "sl_c2_exp",
            "event",
            "ev_c2_target",
            Some("front_door"),
            "admin",
            1000.0,
            2000.0,
            Some(2),
        )
        .unwrap();
        assert!(
            db.touch_share_link_last_accessed("sl_c2_exp", 1500.0)
                .unwrap()
        );
        assert!(
            !db.touch_share_link_last_accessed("sl_c2_exp", 2000.0)
                .unwrap()
        );
        assert!(
            !db.touch_share_link_last_accessed("sl_c2_exp", 2500.0)
                .unwrap()
        );
        assert_eq!(
            db.get_share_link("sl_c2_exp").unwrap().unwrap()["last_accessed_at"],
            1500.0
        );

        // Corrupt share_link columns to TEXT / +Inf: all access and active checks must fail closed
        db.execute_batch(
            "UPDATE share_link SET expires_at = 'corrupted_exp' WHERE id = 'sl_c2_exp';",
        )
        .unwrap();
        assert!(!db.record_share_link_access("sl_c2_exp", 1200.0).unwrap());
        assert!(
            !db.touch_share_link_last_accessed("sl_c2_exp", 1600.0)
                .unwrap()
        );
        assert!(
            !db.has_active_share_links_for_target("ev_c2_target", 1200.0)
                .unwrap()
        );
        assert!(
            !db.has_recently_accessed_share_links_for_target("ev_c2_target", 1600.0, 900.0)
                .unwrap()
        );

        db.execute_batch(
            "UPDATE share_link SET expires_at = 5000.0, max_views = 'corrupted_max' WHERE id = 'sl_c2_exp';",
        )
        .unwrap();
        assert!(!db.record_share_link_access("sl_c2_exp", 1200.0).unwrap());
        assert!(
            !db.has_active_share_links_for_target("ev_c2_target", 1200.0)
                .unwrap()
        );

        db.execute_batch(
            "UPDATE share_link SET max_views = 5, revoked = 'corrupted_rev' WHERE id = 'sl_c2_exp';",
        )
        .unwrap();
        assert!(db.is_share_link_revoked("sl_c2_exp").unwrap());
        assert!(!db.is_share_link_active("sl_c2_exp", 1200.0).unwrap());

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn cycle2_p0_03_retention_preserves_pinned_event_recordings_clamps_negative_margins_and_cleans_clean_webp()
     {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_cycle2_p0_03_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. Pinned event (retain_indefinitely = true) at [1000, 1010] protects overlapping recording
        //    even when event_before_ts (3000.0) > r.end_time (1010.0)
        db.upsert_event(&EventDto {
            id: "ev_pinned_1".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1010.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: true,
            sub_label: None,
            top_score: Some(0.95),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"max_severity": "alert"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.95),
            stationary: false,
        })
        .unwrap();

        db.insert_recording(&RecordingRow {
            id: "rec_pinned_seg".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/front_door/rec_pinned_seg.mp4".to_string(),
            start_time: 1000.0,
            end_time: 1010.0,
            duration: 10.0,
            motion: 100,
            objects: 2,
            dbfs: 0,
            segment_size: 2.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();
        db.insert_recording(&RecordingRow {
            id: "rec_unpinned_old".to_string(),
            camera: "front_door".to_string(),
            path: "/media/ninjanvr/recordings/front_door/rec_unpinned_old.mp4".to_string(),
            start_time: 500.0,
            end_time: 510.0,
            duration: 10.0,
            motion: 100,
            objects: 1,
            dbfs: 0,
            segment_size: 2.0,
            regions: 1,
            motion_heatmap: None,
        })
        .unwrap();

        let pruned = db
            .delete_expired_recordings_with_window(
                "front_door",
                3000.0,
                3000.0,
                3000.0,
                -50.0,
                -50.0,
            )
            .unwrap();
        assert_eq!(
            pruned,
            vec!["/media/ninjanvr/recordings/front_door/rec_unpinned_old.mp4"]
        );
        let remaining = db.query_recordings("front_door", 900.0, 1100.0).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "rec_pinned_seg");

        // 2. cleanup_expired_events_and_reviews includes {camera}-{id}-clean.webp for expired events
        db.upsert_event(&EventDto {
            id: "ev_exp_clean_snap".to_string(),
            camera: "front_door".to_string(),
            label: "person".to_string(),
            zones: json!([]),
            start_time: 1200.0,
            end_time: Some(1210.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: None,
            top_score: Some(0.9),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"max_severity": "alert"}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.9),
            stationary: false,
        })
        .unwrap();

        let removed_files = db
            .cleanup_expired_events_and_reviews("front_door", 2000.0, 2000.0)
            .unwrap();
        assert!(removed_files.contains(
            &"/media/ninjanvr/clips/front_door-ev_exp_clean_snap-clean.webp".to_string()
        ));
        assert!(
            !removed_files.iter().any(|p| p.contains("ev_pinned_1")),
            "pinned event snapshots must never be scheduled for removal"
        );

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn cycle2_p1_01_target_deletion_cascades_share_links_and_role_camera_scoping_edge_cases_work() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_cycle2_p1_01_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. update_user_role on non-existent user returns Ok(false) even when 0 admins exist
        assert!(!db.update_user_role("ghost_user", "viewer").unwrap());

        // Comma-only and wildcard-mixed allowed_cameras normalize to "*"
        db.create_user_with_cameras("op_comma", "Pass123!", "operator", " , , ", 1000)
            .unwrap();
        assert_eq!(
            db.get_user("op_comma").unwrap().unwrap().allowed_cameras,
            "*"
        );
        assert!(
            db.update_user_allowed_cameras("op_comma", "*,front_door")
                .unwrap()
        );
        assert_eq!(
            db.get_user("op_comma").unwrap().unwrap().allowed_cameras,
            "*"
        );

        // 2. delete_event, delete_events_bulk, delete_reviews, delete_export, delete_exports_bulk
        //    atomically revoke associated share links
        for id in ["ev_del_1", "ev_del_2", "ev_del_3"] {
            db.upsert_event(&EventDto {
                id: id.to_string(),
                camera: "front_door".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 1000.0,
                end_time: Some(1005.0),
                has_clip: true,
                has_snapshot: true,
                plus_id: None,
                retain_indefinitely: false,
                sub_label: None,
                top_score: Some(0.9),
                false_positive: Some(false),
                box_coords: None,
                data: json!({}),
                thumbnail: None,
                area: None,
                ratio: None,
                score: Some(0.9),
                stationary: false,
            })
            .unwrap();
            db.insert_share_link(
                &format!("sl_{id}"),
                "event",
                id,
                Some("front_door"),
                "admin",
                1000.0,
                5000.0,
            )
            .unwrap();
        }
        assert_eq!(db.delete_event("ev_del_1").unwrap(), 1);
        assert!(db.is_share_link_revoked("sl_ev_del_1").unwrap());
        assert!(!db.is_share_link_revoked("sl_ev_del_2").unwrap());

        assert_eq!(
            db.delete_events_bulk(&["ev_del_2".to_string(), "ev_del_3".to_string()])
                .unwrap(),
            2
        );
        assert!(db.is_share_link_revoked("sl_ev_del_2").unwrap());
        assert!(db.is_share_link_revoked("sl_ev_del_3").unwrap());

        db.insert_review_segment(&ReviewItemDto {
            id: "rev_del_1".to_string(),
            camera: "front_door".to_string(),
            start_time: 1000.0,
            end_time: Some(1010.0),
            severity: "alert".to_string(),
            thumb_path: "/clips/rev_del_1.webp".to_string(),
            data: json!({}),
            has_been_reviewed: false,
        })
        .unwrap();
        db.insert_share_link(
            "sl_rev_del_1",
            "review",
            "rev_del_1",
            Some("front_door"),
            "admin",
            1000.0,
            5000.0,
        )
        .unwrap();
        assert_eq!(db.delete_reviews(&["rev_del_1".to_string()]).unwrap(), 1);
        assert!(db.is_share_link_revoked("sl_rev_del_1").unwrap());

        for id in ["exp_del_1", "exp_del_2"] {
            db.insert_export(&ExportRow {
                id: id.to_string(),
                camera: "front_door".to_string(),
                name: id.to_string(),
                date: 1000.0,
                video_path: format!("/media/ninjanvr/exports/{id}.mp4"),
                thumb_path: format!("/media/ninjanvr/clips/{id}.webp"),
                in_progress: false,
            })
            .unwrap();
            db.insert_share_link(
                &format!("sl_{id}"),
                "export",
                id,
                Some("front_door"),
                "admin",
                1000.0,
                5000.0,
            )
            .unwrap();
        }
        assert!(db.delete_export("exp_del_1").unwrap().is_some());
        assert!(db.is_share_link_revoked("sl_exp_del_1").unwrap());
        assert!(!db.is_share_link_revoked("sl_exp_del_2").unwrap());

        assert_eq!(
            db.delete_exports_bulk(&["exp_del_2".to_string()])
                .unwrap()
                .len(),
            1
        );
        assert!(db.is_share_link_revoked("sl_exp_del_2").unwrap());

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn cycle2_p1_02_plate_filter_non_alnum_mark_reviews_viewed_fk_and_stale_plate_sub_label_scrubbing()
     {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_cycle2_p1_02_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        db.upsert_event(&EventDto {
            id: "ev_plate_c2".to_string(),
            camera: "front_door".to_string(),
            label: "car".to_string(),
            zones: json!([]),
            start_time: 1000.0,
            end_time: Some(1005.0),
            has_clip: true,
            has_snapshot: true,
            plus_id: None,
            retain_indefinitely: false,
            sub_label: Some("eRIK".to_string()),
            top_score: Some(0.92),
            false_positive: Some(false),
            box_coords: None,
            data: json!({"attributes": ["color:blue"]}),
            thumbnail: None,
            area: None,
            ratio: None,
            score: Some(0.92),
            stationary: false,
        })
        .unwrap();

        // 1. Non-alphanumeric plate filter ("---") must return 0 events, not all events
        assert!(
            db.query_events(&EventFilter {
                plate: Some("---".to_string()),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
        );
        assert!(
            db.query_events(&EventFilter {
                recognized_license_plates: Some(vec!["!@#$".to_string()]),
                ..Default::default()
            })
            .unwrap()
            .is_empty()
        );

        // 2. set_event_license_plate scrubs previous plate from data.attributes on update and clear
        db.set_event_license_plate("ev_plate_c2", Some("OLD-1111"))
            .unwrap();
        let ev_old = db.get_event("ev_plate_c2").unwrap().unwrap();
        assert_eq!(
            ev_old["data"]["attributes"],
            json!(["color:blue", "OLD1111"])
        );

        db.set_event_license_plate("ev_plate_c2", Some("NEW-2222"))
            .unwrap();
        let ev_new = db.get_event("ev_plate_c2").unwrap().unwrap();
        assert_eq!(
            ev_new["data"]["attributes"],
            json!(["color:blue", "NEW2222"])
        );

        db.set_event_license_plate("ev_plate_c2", None).unwrap();
        let ev_cleared = db.get_event("ev_plate_c2").unwrap().unwrap();
        assert_eq!(ev_cleared["data"]["attributes"], json!(["color:blue"]));

        // 3. set_event_sub_label removes case-mismatched old sub_label from reviewsegment
        db.insert_review_segment(&ReviewItemDto {
            id: "rev_c2_1".to_string(),
            camera: "front_door".to_string(),
            start_time: 1000.0,
            end_time: Some(1010.0),
            severity: "alert".to_string(),
            thumb_path: "/clips/rev_c2_1.webp".to_string(),
            data: json!({
                "detections": ["ev_plate_c2", "ev_other"],
                "sub_labels": ["Erik"]
            }),
            has_been_reviewed: false,
        })
        .unwrap();
        db.set_event_sub_label("ev_plate_c2", Some("Alice"))
            .unwrap();
        let rev_after_sub = db.get_review_by_id("rev_c2_1").unwrap().unwrap();
        assert_eq!(rev_after_sub.data["sub_labels"], json!(["Alice"]));

        // 4. mark_reviews_viewed ignores non-existent review IDs without FK failure and clears multi-user status on unview
        let marked = db
            .mark_reviews_viewed(
                &["rev_c2_1".to_string(), "rev_missing_fk".to_string()],
                true,
            )
            .unwrap();
        assert_eq!(marked, 1);
        db.execute_batch(
            "INSERT INTO userreviewstatus (user_id, review_segment_id, has_been_reviewed) VALUES ('operator1', 'rev_c2_1', 1);",
        )
        .unwrap();
        db.mark_reviews_viewed(&["rev_c2_1".to_string()], false)
            .unwrap();
        assert!(
            !db.get_review_by_id("rev_c2_1")
                .unwrap()
                .unwrap()
                .has_been_reviewed
        );

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn cycle2_p1_03_infinite_segment_size_quota_resilience_dataset_nan_sanitization_and_legacy_migrations()
     {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr_cycle2_p1_03_{}.db",
            rand::random::<u32>()
        ));
        let db = NinjaDatabase::open(&tmp).unwrap();

        // 1. Corrupted +Inf segment_size or start_time row in SQLite must not brick quota eviction or summary queries
        for i in 0..3 {
            db.insert_recording(&RecordingRow {
                id: format!("rec_q_{i}"),
                camera: "front_door".to_string(),
                path: format!("/media/ninjanvr/recordings/front_door/rec_q_{i}.mp4"),
                start_time: 1000.0 + (i as f64) * 10.0,
                end_time: 1010.0 + (i as f64) * 10.0,
                duration: 10.0,
                motion: 0,
                objects: 0,
                dbfs: 0,
                segment_size: 100.0,
                regions: 0,
                motion_heatmap: None,
            })
            .unwrap();
        }
        db.execute_batch(
            "INSERT INTO recordings (id, camera, path, start_time, end_time, duration, segment_size)
             VALUES ('rec_inf', 'front_door', '/media/ninjanvr/recordings/front_door/rec_inf.mp4', 9e999, 9e999, 9e999, 9e999);",
        )
        .unwrap();

        assert!(db.query_all_recordings_summary().is_ok());
        let usage = db.query_recordings_storage_usage().unwrap();
        assert_eq!(usage["front_door"]["usage"], 300.0);

        let evicted = db
            .delete_recordings_over_quota_mb("front_door", 150.0)
            .unwrap();
        assert_eq!(evicted.len(), 2);

        // 2. upsert_dataset_sample sanitizes NaN/Inf annotation coordinates/scores so JSON roundtrip never drops boxes
        let sample = db
            .upsert_dataset_sample(&DatasetSampleDto {
                id: "ds_nan_box".to_string(),
                event_id: None,
                camera: "front_door".to_string(),
                split: "train".to_string(),
                sample_kind: "true_positive".to_string(),
                image_path: "/media/ninjanvr/dataset/ds_nan_box.webp".to_string(),
                width: 640,
                height: 360,
                annotations: vec![DatasetAnnotationBox {
                    class_id: 0,
                    label: "person".to_string(),
                    cx: 0.5,
                    cy: 0.5,
                    w: 0.2,
                    h: 0.4,
                    score: f64::NAN,
                    source: "manual".to_string(),
                }],
                verified: true,
                night_mode: false,
                created_at: 1000.0,
            })
            .unwrap();
        assert_eq!(sample.annotations.len(), 1);
        let loaded = db.get_dataset_sample("ds_nan_box").unwrap().unwrap();
        assert_eq!(loaded.annotations.len(), 1);
        assert_eq!(loaded.annotations[0].score, 1.0);

        drop(db);
        let _ = std::fs::remove_file(&tmp);
    }
}
