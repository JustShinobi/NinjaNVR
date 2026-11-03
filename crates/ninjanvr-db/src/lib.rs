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

/// Hashes a password using the `pbkdf2_sha256$<iterations>$<salt>$<b64>` format.
pub fn hash_password(password: &str, salt: Option<&str>, iterations: u32) -> String {
    let owned_salt: String;
    let salt_str = match salt {
        Some(s) => s,
        None => {
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
        iterations,
        &mut dk,
    );
    let b64 = base64::engine::general_purpose::STANDARD.encode(dk);
    format!("{PASSWORD_HASH_ALGORITHM}${iterations}${salt_str}${b64}")
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
    let expected = hash_password(password, Some(parts[2]), iterations);
    constant_time_eq(expected.as_bytes(), password_hash.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn parse_json_or(raw: Option<String>, fallback: Value) -> Value {
    match raw {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or(fallback),
        _ => fallback,
    }
}

fn value_to_f64(val: &rusqlite::types::ValueRef<'_>) -> Option<f64> {
    match val {
        rusqlite::types::ValueRef::Real(f) => Some(*f),
        rusqlite::types::ValueRef::Integer(i) => Some(*i as f64),
        rusqlite::types::ValueRef::Text(bytes) => {
            let s = std::str::from_utf8(bytes).ok()?;
            if let Ok(f) = s.parse::<f64>() {
                return Some(f);
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
    Some([
        arr[0].as_f64()?,
        arr[1].as_f64()?,
        arr[2].as_f64()?,
        arr[3].as_f64()?,
    ])
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
    let aw = a[2] - a[0];
    let ah = a[3] - a[1];
    let bw = b[2] - b[0];
    let bh = b[3] - b[1];
    if aw <= 0.0 || ah <= 0.0 || bw <= 0.0 || bh <= 0.0 {
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
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// Computes normalized bounding-box IoU supporting both `[x1, y1, x2, y2]` and `[x, y, w, h]`
/// (as returned by `BoundingBox::to_normalized`).
pub fn compute_norm_box_iou(a: [f64; 4], b: [f64; 4]) -> f64 {
    let xyxy = iou_corners(a, b);
    let xywh = iou_corners(
        [a[0], a[1], a[0] + a[2], a[1] + a[3]],
        [b[0], b[1], b[0] + b[2], b[1] + b[3]],
    );
    xyxy.max(xywh)
}

impl NinjaDatabase {
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

    pub fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
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
            "#,
        )?;
        let _ = conn.execute(
            "ALTER TABLE user ADD COLUMN allowed_cameras TEXT NOT NULL DEFAULT '*'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE recordings ADD COLUMN stream_tier TEXT NOT NULL DEFAULT 'main'",
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
        Ok(())
    }

    pub fn set_recording_stream_tier(&self, id: &str, tier: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, path FROM recordings WHERE dbfs = ?1 ORDER BY start_time DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![target_dbfs, limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_recording_dbfs(&self, id: &str, dbfs: i64) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let mut conn = self.conn.lock().expect("db mutex poisoned");
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
                        AND COALESCE(e.end_time, e.start_time + 15.0) >= r.start_time
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        Ok(conn.execute("DELETE FROM recordings WHERE id = ?1", params![id])?)
    }

    pub fn ensure_default_admin(&self, iterations: u32) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
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

        let mut bytes = [0u8; 16];
        rand::fill(&mut bytes);
        let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let pw_hash = hash_password(&password, None, iterations);

        conn.execute(
            r#"
            INSERT INTO user (username, role, password_hash, notification_tokens)
            VALUES ('admin', 'admin', ?1, '[]')
            ON CONFLICT(username) DO UPDATE SET password_hash = excluded.password_hash
            "#,
            params![pw_hash],
        )?;
        info!("Created default admin user in SQLite database");
        Ok(Some(password))
    }

    pub fn get_user(&self, username: &str) -> Result<Option<DbUser>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let row = conn
            .query_row(
                "SELECT username, password_hash, role, notification_tokens, COALESCE(allowed_cameras, '*') FROM user WHERE username = ?1",
                params![username],
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let norm_cams = if allowed_cameras.trim().is_empty() {
            "*"
        } else {
            allowed_cameras.trim()
        };

        let conn = self.conn.lock().expect("db mutex poisoned");
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

        let pw_hash = hash_password(password, None, iterations.max(1000));
        let now_str = Utc::now().to_rfc3339();
        conn.execute(
            r#"
            INSERT INTO user (username, role, allowed_cameras, password_hash, notification_tokens, password_changed_at)
            VALUES (?1, ?2, ?3, ?4, '[]', ?5)
            "#,
            params![trimmed, norm_role, norm_cams, pw_hash, now_str],
        )?;
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
        let pw_hash = hash_password(password, None, iterations.max(1000));
        let now_str = Utc::now().to_rfc3339();
        let conn = self.conn.lock().expect("db mutex poisoned");
        let updated = conn.execute(
            "UPDATE user SET password_hash = ?1, password_changed_at = ?2 WHERE username = ?3",
            params![pw_hash, now_str, username.trim()],
        )?;
        Ok(updated > 0)
    }

    pub fn update_user_role(&self, username: &str, role: &str) -> Result<bool> {
        let norm_role = match role.trim().to_ascii_lowercase().as_str() {
            "viewer" => "viewer",
            "operator" => "operator",
            "admin" => "admin",
            _ => anyhow::bail!("Role must be 'admin', 'operator', or 'viewer'"),
        };
        let target = username.trim();
        let conn = self.conn.lock().expect("db mutex poisoned");
        if norm_role != "admin" {
            let other_admins: i64 = conn.query_row(
                "SELECT COUNT(*) FROM user WHERE COALESCE(role, 'admin') = 'admin' AND username != ?1",
                params![target],
                |r| r.get(0),
            )?;
            if other_admins == 0 {
                anyhow::bail!("Cannot demote the last admin account");
            }
        }
        let updated = conn.execute(
            "UPDATE user SET role = ?1 WHERE username = ?2",
            params![norm_role, target],
        )?;
        Ok(updated > 0)
    }

    pub fn update_user_allowed_cameras(
        &self,
        username: &str,
        allowed_cameras: &str,
    ) -> Result<bool> {
        let norm = if allowed_cameras.trim().is_empty() {
            "*"
        } else {
            allowed_cameras.trim()
        };
        let conn = self.conn.lock().expect("db mutex poisoned");
        let updated = conn.execute(
            "UPDATE user SET allowed_cameras = ?1 WHERE username = ?2",
            params![norm, username.trim()],
        )?;
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

        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            r#"
            INSERT INTO api_token (id, name, token_hash, prefix, scope, created_at, last_used_at, expires_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)
            "#,
            params![id, trimmed_name, token_hash, prefix, norm_scope, created_at, expires_at],
        )?;

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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, prefix, scope, created_at, last_used_at, expires_at FROM api_token ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "prefix": r.get::<_, String>(2)?,
                "scope": r.get::<_, String>(3)?,
                "created_at": r.get::<_, f64>(4).unwrap_or(0.0),
                "last_used_at": r.get::<_, Option<f64>>(5).ok().flatten(),
                "expires_at": r.get::<_, Option<f64>>(6).ok().flatten(),
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_api_token(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let deleted = conn.execute("DELETE FROM api_token WHERE id = ?1", params![id.trim()])?;
        Ok(deleted > 0)
    }

    /// Validates an incoming `nnvr_...` bearer token by its SHA-256 hash, checks expiration,
    /// updates `last_used_at`, and returns `Some((token_name, scope))` if valid.
    pub fn verify_api_token(
        &self,
        raw_token: &str,
        now_ts: f64,
    ) -> Result<Option<(String, String)>> {
        if !raw_token.starts_with("nnvr_") {
            return Ok(None);
        }
        let hash_bytes = Sha256::digest(raw_token.as_bytes());
        let token_hash: String = hash_bytes.iter().map(|b| format!("{b:02x}")).collect();

        let conn = self.conn.lock().expect("db mutex poisoned");
        let row: Option<(String, String, String, Option<f64>)> = conn
            .query_row(
                "SELECT id, name, scope, expires_at FROM api_token WHERE token_hash = ?1",
                params![token_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3).ok().flatten())),
            )
            .optional()?;

        let Some((id, name, scope, expires_at)) = row else {
            return Ok(None);
        };
        if let Some(exp) = expires_at
            && now_ts >= exp
        {
            return Ok(None);
        }
        let _ = conn.execute(
            "UPDATE api_token SET last_used_at = ?1 WHERE id = ?2",
            params![now_ts, id],
        );
        Ok(Some((name, scope)))
    }

    pub fn delete_user(&self, username: &str) -> Result<bool> {
        let target = username.trim();
        let conn = self.conn.lock().expect("db mutex poisoned");
        let target_role: Option<String> = conn
            .query_row(
                "SELECT COALESCE(role, 'admin') FROM user WHERE username = ?1",
                params![target],
                |r| r.get(0),
            )
            .optional()?;
        let Some(role) = target_role else {
            return Ok(false);
        };
        if role == "admin" {
            let other_admins: i64 = conn.query_row(
                "SELECT COUNT(*) FROM user WHERE COALESCE(role, 'admin') = 'admin' AND username != ?1",
                params![target],
                |r| r.get(0),
            )?;
            if other_admins == 0 {
                anyhow::bail!("Cannot delete the last admin account");
            }
        }
        let _ = conn.execute(
            "DELETE FROM userreviewstatus WHERE user_id = ?1",
            params![target],
        );
        let deleted = conn.execute("DELETE FROM user WHERE username = ?1", params![target])?;
        Ok(deleted > 0)
    }

    pub fn query_reviews(&self, filter: &ReviewFilter) -> Result<Vec<ReviewItemDto>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
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

        if let Some(before) = filter.before {
            sql.push_str(&format!(" AND r.start_time <= {before}"));
        }
        if let Some(after) = filter.after {
            sql.push_str(&format!(
                " AND COALESCE(r.end_time, r.start_time) >= {after}"
            ));
        }
        if let Some(ref sev) = filter.severity
            && !sev.is_empty()
            && sev != "all"
        {
            let sanitized = sev.replace('\'', "");
            sql.push_str(&format!(" AND r.severity = '{sanitized}'"));
        }
        if let Some(ref cams) = filter.cameras
            && !cams.is_empty()
            && !cams.iter().any(|c| c == "all")
        {
            let list: Vec<String> = cams
                .iter()
                .map(|c| format!("'{}'", c.replace('\'', "")))
                .collect();
            sql.push_str(&format!(" AND r.camera IN ({})", list.join(",")));
        }

        sql.push_str(" GROUP BY r.id");
        if let Some(want_reviewed) = filter.reviewed {
            let rv = if want_reviewed { 1 } else { 0 };
            sql.push_str(&format!(" HAVING reviewed = {rv}"));
        }
        sql.push_str(" ORDER BY r.start_time DESC");

        let requested_limit = filter.limit.unwrap_or(250).min(2000);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let like_pattern = format!("%\"{}\"%", event_id.replace('"', ""));
        let row = conn
            .query_row(
                r#"
                SELECT
                    r.id, r.camera, r.start_time, r.end_time, r.severity, r.thumb_path, r.data,
                    COALESCE(MAX(u.has_been_reviewed), 0) AS reviewed
                FROM reviewsegment r
                LEFT JOIN userreviewstatus u ON u.review_segment_id = r.id
                WHERE r.data LIKE ?1
                GROUP BY r.id
                ORDER BY r.start_time DESC
                LIMIT 1
                "#,
                params![like_pattern],
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let now = Utc::now().timestamp() as f64;
        let last_24h = now - 86400.0;
        let last_30d = now - 86400.0 * 30.0;

        let mut cam_clause = String::new();
        if let Some(cams) = cameras
            && !cams.is_empty()
            && !cams.iter().any(|c| c == "all")
        {
            let list: Vec<String> = cams
                .iter()
                .map(|c| format!("'{}'", c.replace('\'', "")))
                .collect();
            cam_clause = format!(" AND r.camera IN ({})", list.join(","));
        }

        let sql = format!(
            r#"
            SELECT
                r.start_time,
                r.severity,
                COALESCE(MAX(u.has_been_reviewed), 0) AS reviewed
            FROM reviewsegment r
            LEFT JOIN userreviewstatus u ON u.review_segment_id = r.id
            WHERE r.start_time >= {last_30d} {cam_clause}
            GROUP BY r.id
            ORDER BY r.start_time DESC
            "#
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let val = if reviewed { 1 } else { 0 };
        let mut count = 0;
        for id in ids {
            count += conn.execute(
                r#"
                INSERT INTO userreviewstatus (user_id, review_segment_id, has_been_reviewed)
                VALUES ('admin', ?1, ?2)
                ON CONFLICT(user_id, review_segment_id) DO UPDATE SET has_been_reviewed = excluded.has_been_reviewed
                "#,
                params![id, val],
            )?;
        }
        Ok(count)
    }

    pub fn delete_reviews(&self, ids: &[String]) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut count = 0;
        for id in ids {
            let _ = conn.execute(
                "DELETE FROM userreviewstatus WHERE review_segment_id = ?1",
                params![id],
            );
            count += conn.execute("DELETE FROM reviewsegment WHERE id = ?1", params![id])?;
        }
        Ok(count)
    }

    pub fn insert_review_segment(&self, item: &ReviewItemDto) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut sql = String::from(
            r#"
            SELECT
                id, camera, label, zones, start_time, end_time,
                has_clip, has_snapshot, plus_id, retain_indefinitely,
                sub_label, top_score, false_positive, box, data, thumbnail
            FROM event
            WHERE 1=1
            "#,
        );

        if let Some(before) = filter.before {
            sql.push_str(&format!(" AND start_time <= {before}"));
        }
        if let Some(after) = filter.after {
            sql.push_str(&format!(" AND COALESCE(end_time, start_time) >= {after}"));
        }
        if let Some(fav) = filter.favorites
            && fav
        {
            sql.push_str(" AND retain_indefinitely = 1");
        }
        if let Some(ref cams) = filter.cameras
            && !cams.is_empty()
            && !cams.iter().any(|c| c == "all")
        {
            let list: Vec<String> = cams
                .iter()
                .map(|c| format!("'{}'", c.replace('\'', "")))
                .collect();
            sql.push_str(&format!(" AND camera IN ({})", list.join(",")));
        }
        if let Some(ref labels) = filter.labels
            && !labels.is_empty()
            && !labels.iter().any(|l| l == "all")
        {
            let list: Vec<String> = labels
                .iter()
                .map(|l| format!("'{}'", l.replace('\'', "")))
                .collect();
            sql.push_str(&format!(" AND label IN ({})", list.join(",")));
        }
        if let Some(ref ex_labels) = filter.exclude_labels
            && !ex_labels.is_empty()
        {
            let list: Vec<String> = ex_labels
                .iter()
                .map(|l| format!("'{}'", l.replace('\'', "")))
                .collect();
            sql.push_str(&format!(" AND label NOT IN ({})", list.join(",")));
        }
        if let Some(min_sc) = filter.min_score {
            sql.push_str(&format!(" AND COALESCE(top_score, score, 0.0) >= {min_sc}"));
        }
        if let Some(ref subs) = filter.sub_labels
            && !subs.is_empty()
            && !subs.iter().any(|s| s == "all")
        {
            let list: Vec<String> = subs
                .iter()
                .map(|s| format!("'{}'", s.replace('\'', "")))
                .collect();
            sql.push_str(&format!(" AND sub_label IN ({})", list.join(",")));
        }

        sql.push_str(" ORDER BY start_time DESC");
        let requested_limit = filter.limit.unwrap_or(100).min(1000);

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            let start_time = value_to_f64(&row.get_ref(4)?).unwrap_or(0.0);
            let end_time = value_to_f64(&row.get_ref(5)?);
            let zones_raw: Option<String> = row.get(3).ok();
            let box_raw: Option<String> = row.get(13).ok();
            let data_raw: Option<String> = row.get(14).ok();
            let data_json = parse_json_or(data_raw, json!({}));
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
                "sub_label": row.get::<_, Option<String>>(10).ok().flatten(),
                "top_score": row.get::<_, Option<f64>>(11).ok().flatten(),
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
            out.push(item);
            if out.len() >= requested_limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn query_events_explore(&self, limit_per_label: usize) -> Result<Vec<Value>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT label, COUNT(*) as cnt FROM event GROUP BY label ORDER BY cnt DESC")?;
        let label_counts: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        drop(conn);

        let mut out = Vec::new();
        for (label, count) in label_counts {
            let events = self.query_events(&EventFilter {
                labels: Some(vec![label]),
                limit: Some(limit_per_label.max(1)),
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut ev_sql = String::from(
            "SELECT id, camera, label, zones, start_time, top_score FROM event WHERE start_time >= ?1 AND start_time <= ?2",
        );
        if let Some(cams) = allowed_cameras {
            let list: Vec<String> = cams
                .iter()
                .map(|c| format!("'{}'", c.replace('\'', "")))
                .collect();
            if list.is_empty() {
                ev_sql.push_str(" AND 1=0");
            } else {
                ev_sql.push_str(&format!(" AND camera IN ({})", list.join(",")));
            }
        }
        ev_sql.push_str(" ORDER BY start_time DESC LIMIT 5000");

        let mut total_events = 0usize;
        let mut by_camera: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_label: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_zone: BTreeMap<String, usize> = BTreeMap::new();
        let mut hourly_histogram: BTreeMap<String, usize> = BTreeMap::new();

        {
            let mut stmt = conn.prepare(&ev_sql)?;
            let rows = stmt.query_map(params![after, before], |r| {
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
                COALESCE(u.has_been_reviewed, 0) as reviewed
            FROM reviewsegment r
            LEFT JOIN userreviewstatus u ON u.review_segment_id = r.id
            WHERE r.start_time >= ?1 AND r.start_time <= ?2
            "#,
        );
        if let Some(cams) = allowed_cameras {
            let list: Vec<String> = cams
                .iter()
                .map(|c| format!("'{}'", c.replace('\'', "")))
                .collect();
            if list.is_empty() {
                rev_sql.push_str(" AND 1=0");
            } else {
                rev_sql.push_str(&format!(" AND r.camera IN ({})", list.join(",")));
            }
        }
        rev_sql.push_str(" ORDER BY r.start_time DESC LIMIT 1000");

        let mut by_severity: BTreeMap<String, usize> = BTreeMap::new();
        let mut unreviewed_alerts = Vec::new();
        {
            let mut stmt = conn.prepare(&rev_sql)?;
            let rows = stmt.query_map(params![after, before], |r| {
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let row = conn
            .query_row(
                r#"
                SELECT
                    id, camera, label, zones, start_time, end_time,
                    has_clip, has_snapshot, plus_id, retain_indefinitely,
                    sub_label, top_score, false_positive, box, data, thumbnail
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
                        "sub_label": row.get::<_, Option<String>>(10).ok().flatten(),
                        "top_score": row.get::<_, Option<f64>>(11).ok().flatten(),
                        "false_positive": row.get::<_, Option<i64>>(12).ok().flatten().map(|v| v != 0),
                        "box": parse_json_or(box_raw, Value::Null),
                        "data": parse_json_or(data_raw, json!({})),
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let val = if retain { 1 } else { 0 };
        Ok(conn.execute(
            "UPDATE event SET retain_indefinitely = ?1 WHERE id = ?2",
            params![val, id],
        )?)
    }

    pub fn set_event_false_positive(&self, id: &str, fp: bool) -> Result<usize> {
        let ev_opt = self.get_event(id)?;
        let conn = self.conn.lock().expect("db mutex poisoned");
        let val = if fp { 1 } else { 0 };
        let updated = conn.execute(
            "UPDATE event SET false_positive = ?1 WHERE id = ?2",
            params![val, id],
        )?;
        if fp {
            if let Some(ev) = ev_opt {
                let camera = ev["camera"].as_str().unwrap_or_default().to_string();
                let label = ev["label"].as_str().unwrap_or_default().to_string();
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
                    conn.execute(
                        r#"
                        INSERT OR REPLACE INTO false_positive_signature (
                            camera, label, norm_x1, norm_y1, norm_x2, norm_y2,
                            dhash64, dominant_color, hit_count, source_event_id,
                            reason, created_at, last_seen_at
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?11)
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
        } else {
            conn.execute(
                "DELETE FROM false_positive_signature WHERE source_event_id = ?1",
                params![id],
            )?;
        }
        Ok(updated)
    }

    pub fn set_event_ai_verification(&self, id: &str, verification: &Value) -> Result<usize> {
        let Some(mut ev) = self.get_event(id)? else {
            return Ok(0);
        };
        if let Some(data_obj) = ev["data"].as_object_mut() {
            data_obj.insert("ai_verification".to_string(), verification.clone());
        }
        let data_str = serde_json::to_string(&ev["data"])?;
        let conn = self.conn.lock().expect("db mutex poisoned");
        Ok(conn.execute(
            "UPDATE event SET data = ?1 WHERE id = ?2",
            params![data_str, id],
        )?)
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
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
            let new_hits = row.9 + 1;
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
            "#
        } else {
            r#"
            SELECT
                id, camera, label, norm_x1, norm_y1, norm_x2, norm_y2,
                dhash64, dominant_color, hit_count, source_event_id,
                reason, created_at, last_seen_at
            FROM false_positive_signature
            ORDER BY last_seen_at DESC, id DESC
            "#
        };
        let mut stmt = conn.prepare(sql)?;
        let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
            let nx1: f64 = r.get(3)?;
            let ny1: f64 = r.get(4)?;
            let nx2: f64 = r.get(5)?;
            let ny2: f64 = r.get(6)?;
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
                "created_at": r.get::<_, f64>(12)?,
                "last_seen_at": r.get::<_, f64>(13)?,
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let src_ev: Option<String> = conn
            .query_row(
                "SELECT source_event_id FROM false_positive_signature WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let deleted = conn.execute(
            "DELETE FROM false_positive_signature WHERE id = ?1",
            params![id],
        )?;
        if let Some(ev_id) = src_ev {
            let _ = conn.execute(
                "UPDATE event SET false_positive = 0 WHERE id = ?1",
                params![ev_id],
            );
        }
        Ok(deleted)
    }

    pub fn set_event_sub_label(&self, id: &str, sub_label: Option<&str>) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        Ok(conn.execute(
            "UPDATE event SET sub_label = ?1 WHERE id = ?2",
            params![sub_label, id],
        )?)
    }

    pub fn set_event_description(&self, id: &str, description: &str) -> Result<usize> {
        let Some(mut ev) = self.get_event(id)? else {
            return Ok(0);
        };
        if let Some(data_obj) = ev["data"].as_object_mut() {
            data_obj.insert("description".to_string(), json!(description));
        }
        let data_str = serde_json::to_string(&ev["data"])?;
        let conn = self.conn.lock().expect("db mutex poisoned");
        Ok(conn.execute(
            "UPDATE event SET data = ?1 WHERE id = ?2",
            params![data_str, id],
        )?)
    }

    pub fn set_event_notes(&self, id: &str, notes: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        Ok(conn.execute(
            "UPDATE event SET data = json_set(COALESCE(data, '{}'), '$.notes', ?1) WHERE id = ?2",
            params![notes, id],
        )?)
    }

    pub fn set_review_notes(&self, id: &str, notes: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        Ok(conn.execute(
            "UPDATE reviewsegment SET data = json_set(COALESCE(data, '{}'), '$.notes', ?1) WHERE id = ?2",
            params![notes, id],
        )?)
    }

    /// Executes SQLite query-planner optimization and bounded WAL checkpointing (`PRAGMA optimize`
    /// and `PRAGMA wal_checkpoint(TRUNCATE)`).
    pub fn run_maintenance(&self) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Returns total event counts grouped by `(camera, label)` for Prometheus `/api/metrics`.
    pub fn query_event_counts_by_camera_and_label(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let zones_str = serde_json::to_string(&ev.zones)?;
        let box_str = ev
            .box_coords
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let data_str = serde_json::to_string(&ev.data)?;

        conn.execute(
            r#"
            INSERT INTO event (
                id, label, camera, start_time, end_time, top_score, false_positive,
                zones, thumbnail, has_clip, has_snapshot, box, retain_indefinitely,
                sub_label, plus_id, data, area, ratio, score
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
            ON CONFLICT(id) DO UPDATE SET
                end_time = excluded.end_time,
                top_score = excluded.top_score,
                false_positive = CASE
                    WHEN event.false_positive = 1 OR excluded.false_positive = 1 THEN 1
                    ELSE COALESCE(excluded.false_positive, event.false_positive)
                END,
                zones = excluded.zones,
                thumbnail = COALESCE(excluded.thumbnail, event.thumbnail),
                has_clip = excluded.has_clip,
                has_snapshot = excluded.has_snapshot,
                box = excluded.box,
                data = excluded.data,
                area = COALESCE(excluded.area, event.area),
                ratio = COALESCE(excluded.ratio, event.ratio),
                score = COALESCE(excluded.score, event.score)
            "#,
            params![
                ev.id,
                ev.label,
                ev.camera,
                ev.start_time,
                ev.end_time,
                ev.top_score,
                ev.false_positive.map(|b| if b { 1 } else { 0 }),
                zones_str,
                ev.thumbnail,
                if ev.has_clip { 1 } else { 0 },
                if ev.has_snapshot { 1 } else { 0 },
                box_str,
                if ev.retain_indefinitely { 1 } else { 0 },
                ev.sub_label,
                ev.plus_id,
                data_str,
                ev.area,
                ev.ratio.map(|r| r as f64),
                ev.score.map(|s| s as f64)
            ],
        )?;
        Ok(())
    }

    pub fn delete_event(&self, id: &str) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let count = conn.execute("DELETE FROM event WHERE id = ?1", params![id])?;
        Ok(count)
    }

    pub fn delete_events_bulk(&self, ids: &[String]) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut count = 0;
        for id in ids {
            count += conn.execute("DELETE FROM event WHERE id = ?1", params![id])?;
        }
        Ok(count)
    }

    pub fn query_recordings(
        &self,
        camera: &str,
        after: f64,
        before: f64,
    ) -> Result<Vec<RecordingRow>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            r#"
            SELECT
                id, camera, path, start_time, end_time, duration,
                COALESCE(motion, 0), COALESCE(objects, 0), COALESCE(dBFS, 0),
                segment_size, COALESCE(regions, 0), motion_heatmap
            FROM recordings
            WHERE camera = ?1 AND end_time >= ?2 AND start_time <= ?3
            ORDER BY start_time ASC
            "#,
        )?;

        let rows = stmt.query_map(params![camera, after, before], |r| {
            let start_time = value_to_f64(&r.get_ref(3)?).unwrap_or(0.0);
            let end_time = value_to_f64(&r.get_ref(4)?).unwrap_or(0.0);
            let hm_raw: Option<String> = r.get(11).ok();
            let motion_heatmap = hm_raw.and_then(|s| serde_json::from_str(&s).ok());
            Ok(RecordingRow {
                id: r.get(0)?,
                camera: r.get(1)?,
                path: r.get(2)?,
                start_time,
                end_time,
                duration: r.get(5)?,
                motion: r.get(6)?,
                objects: r.get(7)?,
                dbfs: r.get(8)?,
                segment_size: r.get(9)?,
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let cutoff = (Utc::now().timestamp() - 86400 * 60) as f64;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT CAST(start_time / 86400 AS INTEGER) * 86400 FROM recordings WHERE start_time >= ?1 ORDER BY start_time DESC",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| r.get::<_, i64>(0))?;
        let mut map = serde_json::Map::new();
        for r in rows {
            let day_ts = r?;
            if let Some(dt) = Utc.timestamp_opt(day_ts, 0).single() {
                map.insert(dt.format("%Y-%m-%d").to_string(), json!(true));
            }
        }
        Ok(Value::Object(map))
    }

    pub fn query_recordings_storage_usage(&self) -> Result<Value> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let cutoff = (Utc::now().timestamp() - 86400) as f64;
        let mut stmt = conn.prepare(
            r#"
            SELECT
                camera,
                COALESCE(SUM(segment_size), 0.0),
                COALESCE(SUM(duration), 0.0),
                COALESCE(SUM(CASE WHEN start_time >= ?1 THEN segment_size ELSE 0.0 END), 0.0),
                COALESCE(SUM(CASE WHEN start_time >= ?1 THEN duration ELSE 0.0 END), 0.0)
            FROM recordings
            GROUP BY camera
            "#,
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
            ))
        })?;

        let mut out = serde_json::Map::new();
        for r in rows {
            let (cam, total_mb, dur_s, rolling_24h_mb, rolling_24h_dur_s) = r?;
            let bw_mb_hr = if rolling_24h_dur_s > 0.0 {
                (rolling_24h_mb / (rolling_24h_dur_s / 3600.0) * 100.0).round() / 100.0
            } else if dur_s > 0.0 {
                (total_mb / (dur_s / 3600.0) * 100.0).round() / 100.0
            } else {
                0.0
            };
            out.insert(
                cam,
                json!({
                    "usage": (total_mb * 100.0).round() / 100.0,
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut cam_clause = String::new();
        if let Some(cams) = cameras
            && !cams.is_empty()
            && !cams.iter().any(|c| c == "all")
        {
            let list: Vec<String> = cams
                .iter()
                .map(|c| format!("'{}'", c.replace('\'', "")))
                .collect();
            cam_clause = format!(" AND camera IN ({})", list.join(","));
        }
        let sql = format!(
            r#"
            SELECT start_time, COALESCE(motion, 0), camera
            FROM recordings
            WHERE end_time >= {after} AND start_time <= {before} {cam_clause}
            ORDER BY start_time ASC
            LIMIT 2000
            "#
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
                r.get::<_, f64>(1).unwrap_or(0.0),
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
            let (ts, dur, motion, objs, dbfs) = r?;
            if let Some(dt) = Local.timestamp_opt(ts as i64, 0).single() {
                let day = dt.format("%Y-%m-%d").to_string();
                let hour = dt.format("%H").to_string();
                let seg_start = ts as i64;
                let seg_end = (ts + dur).ceil() as i64;
                let h = day_hours.entry(day).or_default().entry(hour).or_default();
                h.duration += dur;
                h.motion += motion;
                h.objects += objs;
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
            if let Some(dt) = Local.timestamp_opt(ts as i64, 0).single() {
                let day = dt.format("%Y-%m-%d").to_string();
                let hour = dt.format("%H").to_string();
                if let Some(hours_map) = day_hours.get_mut(&day)
                    && let Some(h) = hours_map.get_mut(&hour)
                {
                    h.events += 1;
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
                total_events += agg.events;
                total_motion += agg.motion;
                let dur_i64 = agg.duration.round() as i64;
                total_duration += dur_i64;
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let hm_str = rec
            .motion_heatmap
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
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
                rec.start_time,
                rec.end_time,
                rec.duration,
                rec.objects,
                rec.motion,
                rec.segment_size,
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
        let max_cutoff_ts = continuous_before_ts
            .max(motion_before_ts)
            .max(event_before_ts);
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let mut ids = Vec::new();
        let mut paths = Vec::new();
        {
            let mut stmt = conn.prepare(
                r#"
                SELECT r.id, r.path
                FROM recordings r
                WHERE r.camera = ?1
                  AND r.end_time < ?7
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
                    pre_capture_sec,
                    post_capture_sec,
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
            let tx = conn.transaction()?;
            {
                let mut del_stmt = tx.prepare("DELETE FROM recordings WHERE id = ?1")?;
                for id in &ids {
                    del_stmt.execute(params![id])?;
                }
            }
            tx.commit()?;
        }
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
            params![
                camera,
                before_ts,
                pre_capture_sec,
                post_capture_sec,
                limit as i64
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_recording_compacted(&self, id: &str, new_size_mb: f64) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "UPDATE recordings SET segment_size = ?2, stream_tier = 'compacted' WHERE id = ?1",
            params![id, new_size_mb],
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
        let mut conn = self.conn.lock().expect("db mutex poisoned");
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
                    r.get::<_, f64>(2).unwrap_or(0.0),
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
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        let mut orphan_ids = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, path FROM recordings")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for r in rows {
                let (id, path) = r?;
                if !file_exists(&path) {
                    orphan_ids.push(id);
                }
            }
        }
        if !orphan_ids.is_empty() {
            let mut del_stmt = tx.prepare("DELETE FROM recordings WHERE id = ?1")?;
            for id in &orphan_ids {
                del_stmt.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(orphan_ids.len())
    }

    /// Lists up to `limit` recording segments with `start_time < before_ts` whose `path` matches `hot_prefix_like`
    /// so the Hot-to-Cold storage tier migration worker can move them to the cold archive tier.
    pub fn list_recordings_for_tier_migration(
        &self,
        before_ts: f64,
        hot_prefix_like: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            r#"
            SELECT id, path
            FROM recordings
            WHERE start_time < ?1 AND path LIKE ?2
            ORDER BY start_time ASC
            LIMIT ?3
            "#,
        )?;
        let rows = stmt.query_map(params![before_ts, hot_prefix_like, limit as i64], |r| {
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let total_mb: f64 = conn.query_row(
            "SELECT COALESCE(SUM(segment_size), 0.0) FROM recordings WHERE camera = ?1",
            params![camera],
            |r| r.get(0),
        )?;
        if total_mb <= max_size_mb {
            return Ok(Vec::new());
        }
        let excess_mb = total_mb - max_size_mb;
        let tx = conn.transaction()?;
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
                    r.get::<_, f64>(2).unwrap_or(0.0),
                ))
            })?;
            for r in rows {
                let (id, path, sz) = r?;
                to_delete.push((id, path));
                accumulated_mb += sz.max(0.001);
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
        if max_size_mb <= 0.0 {
            return Ok(Vec::new());
        }
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let total_mb: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(segment_size), 0.0) FROM recordings",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0.0);
        if total_mb <= max_size_mb {
            return Ok(Vec::new());
        }
        let excess_mb = total_mb - max_size_mb;
        let tx = conn.transaction()?;
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
                    r.get::<_, f64>(2).unwrap_or(0.0),
                ))
            })?;
            for r in rows {
                let (id, path, sz) = r?;
                to_delete.push((id, path));
                accumulated_mb += sz.max(0.001);
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut sql = String::from(
            "SELECT timestamp, camera, source, source_id, class_type, data FROM timeline WHERE 1=1",
        );
        if let Some(cam) = camera
            && !cam.is_empty()
            && cam != "all"
        {
            sql.push_str(&format!(" AND camera = '{}'", cam.replace('\'', "")));
        }
        if let Some(sid) = source_id
            && !sid.is_empty()
        {
            let ids: Vec<String> = sid
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| format!("'{}'", s.replace('\'', "")))
                .collect();
            if !ids.is_empty() {
                sql.push_str(&format!(" AND source_id IN ({})", ids.join(",")));
            }
        }
        let lim = limit.min(1000);
        sql.push_str(&format!(" ORDER BY timestamp ASC LIMIT {lim}"));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, camera, name, date, video_path, thumb_path, in_progress, export_case_id FROM export ORDER BY date DESC",
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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

    pub fn insert_export(&self, exp: &ExportRow) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        Ok(conn.execute(
            "UPDATE export SET name = ?1 WHERE id = ?2",
            params![new_name, id],
        )?)
    }

    pub fn delete_export(&self, id: &str) -> Result<Option<(String, String)>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let paths: Option<(String, String)> = conn
            .query_row(
                "SELECT video_path, thumb_path FROM export WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if paths.is_some() {
            conn.execute("DELETE FROM export WHERE id = ?1", params![id])?;
        }
        Ok(paths)
    }

    pub fn delete_exports_bulk(&self, ids: &[String]) -> Result<Vec<(String, String)>> {
        let mut deleted_paths = Vec::new();
        for id in ids {
            if let Some(paths) = self.delete_export(id)? {
                deleted_paths.push(paths);
            }
        }
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at, updated_at, COALESCE(event_ids, '[]'), COALESCE(export_ids, '[]') FROM exportcase ORDER BY created_at DESC",
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            r#"
            SELECT camera, path, start_time, end_time, duration
            FROM previews
            WHERE (?1 = 'all' OR camera = ?1) AND end_time >= ?2 AND start_time <= ?3
            ORDER BY start_time ASC
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
                "duration": r.get::<_, f64>(4).unwrap_or(0.0)
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut files_to_remove = Vec::new();

        // 1. Expired review segments
        {
            let mut stmt = conn.prepare(
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
                if !thumb.is_empty() {
                    files_to_remove.push(thumb);
                }
            }
            drop(stmt);
            for id in ids {
                let _ = conn.execute(
                    "DELETE FROM userreviewstatus WHERE review_segment_id = ?1",
                    params![id],
                );
                let _ = conn.execute("DELETE FROM reviewsegment WHERE id = ?1", params![id]);
            }
        }

        // 2. Expired events (excluding retain_indefinitely = 1), respecting alert vs detection cutoffs
        let min_cutoff = alerts_before_ts.min(detections_before_ts);
        {
            let mut stmt = conn.prepare(
                r#"
                SELECT id
                FROM event
                WHERE camera = ?1
                  AND retain_indefinitely = 0
                  AND end_time IS NOT NULL
                  AND (
                    (COALESCE(json_extract(data, '$.max_severity'), 'alert') = 'alert' AND end_time < ?2)
                    OR (COALESCE(json_extract(data, '$.max_severity'), 'alert') != 'alert' AND end_time < ?3)
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
                files_to_remove.push(format!("/media/ninjanvr/clips/{id}.jpg"));
                files_to_remove.push(format!("/media/ninjanvr/clips/{camera}-{id}.webp"));
                let _ = conn.execute("DELETE FROM event WHERE id = ?1", params![id]);
            }
        }

        // 3. Expired timeline & previews
        let _ = conn.execute(
            "DELETE FROM timeline WHERE camera = ?1 AND timestamp < ?2",
            params![camera, min_cutoff],
        );
        {
            let mut stmt = conn.prepare(
                "SELECT id, path FROM previews WHERE camera = ?1 AND end_time < ?2 LIMIT 200",
            )?;
            let rows = stmt.query_map(params![camera, min_cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut pids = Vec::new();
            for r in rows {
                let (pid, ppath) = r?;
                pids.push(pid);
                files_to_remove.push(ppath);
            }
            drop(stmt);
            for pid in pids {
                let _ = conn.execute("DELETE FROM previews WHERE id = ?1", params![pid]);
            }
        }

        Ok(files_to_remove)
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
}
