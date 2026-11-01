use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use hmac::Hmac;
use pbkdf2::pbkdf2;
use rand::Rng;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
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
    pub notification_tokens: Value,
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
    pub box_coords: Option<Value>,
    pub data: Value,
    pub thumbnail: Option<String>,
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
    pub zones: Option<Vec<String>>,
    pub reviewed: Option<bool>,
    pub severity: Option<String>,
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub cameras: Option<Vec<String>>,
    pub labels: Option<Vec<String>>,
    pub zones: Option<Vec<String>>,
    pub sub_labels: Option<Vec<String>>,
    pub favorites: Option<bool>,
    pub before: Option<f64>,
    pub after: Option<f64>,
    pub limit: Option<usize>,
}

/// Hashes a password using Frigate's exact `pbkdf2_sha256$<iterations>$<salt>$<b64>` format.
pub fn hash_password(password: &str, salt: Option<&str>, iterations: u32) -> String {
    let owned_salt: String;
    let salt_str = match salt {
        Some(s) => s,
        None => {
            let mut bytes = [0u8; 16];
            rand::thread_rng().fill(&mut bytes);
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

/// Verifies a plaintext password against Frigate's `pbkdf2_sha256$<iterations>$<salt>$<b64>` hash.
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
            PRAGMA cache_size = -8000;
            PRAGMA mmap_size = 268435456;
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
                "updated_at" DATETIME NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "user" (
                "username" VARCHAR(30) NOT NULL PRIMARY KEY,
                "password_hash" VARCHAR(120) NOT NULL,
                "notification_tokens" JSON NOT NULL,
                "role" VARCHAR(20) NOT NULL DEFAULT 'admin',
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
            CREATE UNIQUE INDEX IF NOT EXISTS "recordings_path" ON "recordings" ("path");
            CREATE INDEX IF NOT EXISTS "review_segment_camera" ON "reviewsegment" ("camera");
            CREATE INDEX IF NOT EXISTS "review_segment_start_time_end_time" ON "reviewsegment" ("start_time" DESC, "end_time" DESC);
            CREATE INDEX IF NOT EXISTS "event_camera" ON "event" ("camera");
            CREATE INDEX IF NOT EXISTS "event_start_time_end_time" ON "event" ("start_time" DESC, "end_time" DESC);
            CREATE UNIQUE INDEX IF NOT EXISTS "userreviewstatus_user_segment" ON "userreviewstatus" ("user_id", "review_segment_id");
            "#,
        )?;
        Ok(())
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
        rand::thread_rng().fill(&mut bytes);
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
                "SELECT username, password_hash, role, notification_tokens FROM user WHERE username = ?1",
                params![username],
                |r| {
                    let tokens_raw: Option<String> = r.get(3).ok();
                    Ok(DbUser {
                        username: r.get(0)?,
                        password_hash: r.get(1)?,
                        role: r.get::<_, Option<String>>(2)?.unwrap_or_else(|| "admin".to_string()),
                        notification_tokens: parse_json_or(tokens_raw, json!([])),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_users(&self) -> Result<Vec<Value>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare("SELECT username, role FROM user ORDER BY username")?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "username": r.get::<_, String>(0)?,
                "role": r.get::<_, Option<String>>(1)?.unwrap_or_else(|| "admin".to_string()),
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
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
            if let Some(ref zones) = filter.zones
                && !zones.is_empty()
                && !zones.iter().any(|z| z == "all")
                && !json_array_matches_any(&item.data["zones"], zones, false)
            {
                continue;
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

    pub fn set_event_retain(&self, id: &str, retain: bool) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let val = if retain { 1 } else { 0 };
        Ok(conn.execute(
            "UPDATE event SET retain_indefinitely = ?1 WHERE id = ?2",
            params![val, id],
        )?)
    }

    pub fn set_event_false_positive(&self, id: &str, fp: bool) -> Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let val = if fp { 1 } else { 0 };
        Ok(conn.execute(
            "UPDATE event SET false_positive = ?1 WHERE id = ?2",
            params![val, id],
        )?)
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
                sub_label, plus_id, data
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(id) DO UPDATE SET
                end_time = excluded.end_time,
                top_score = excluded.top_score,
                zones = excluded.zones,
                thumbnail = COALESCE(excluded.thumbnail, event.thumbnail),
                has_clip = excluded.has_clip,
                has_snapshot = excluded.has_snapshot,
                box = excluded.box,
                data = excluded.data
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
                data_str
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
            SELECT camera, COALESCE(SUM(segment_size), 0.0), COALESCE(SUM(duration), 0.0)
            FROM recordings
            GROUP BY camera
            "#,
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })?;

        let mut out = serde_json::Map::new();
        for r in rows {
            let (cam, total_mb, dur_s) = r?;
            let bw_mb_hr = if dur_s > 0.0 {
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
        let _ = cutoff;
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
            SELECT start_time, duration, COALESCE(motion, 0), COALESCE(objects, 0)
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
            ))
        })?;

        #[derive(Default)]
        struct HourAgg {
            duration: f64,
            motion: i64,
            objects: i64,
            events: i64,
        }
        let mut day_hours: BTreeMap<String, BTreeMap<String, HourAgg>> = BTreeMap::new();

        for r in rows {
            let (ts, dur, motion, objs) = r?;
            if let Some(dt) = Utc.timestamp_opt(ts as i64, 0).single() {
                let day = dt.format("%Y-%m-%d").to_string();
                let hour = dt.format("%H").to_string();
                let h = day_hours.entry(day).or_default().entry(hour).or_default();
                h.duration += dur;
                h.motion += motion;
                h.objects += objs;
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
            if let Some(dt) = Utc.timestamp_opt(ts as i64, 0).single() {
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
            let mut hours_vec = Vec::new();
            for (hour, agg) in hours_map.into_iter().rev() {
                total_events += agg.events;
                hours_vec.push(json!({
                    "hour": hour,
                    "events": agg.events,
                    "motion": agg.motion,
                    "objects": agg.objects,
                    "duration": agg.duration.round() as i64
                }));
            }
            summary.push(json!({
                "day": day,
                "events": total_events,
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
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            r#"
            SELECT id, path
            FROM recordings
            WHERE camera = ?1
              AND (
                (COALESCE(motion, 0) = 0 AND COALESCE(objects, 0) = 0 AND end_time < ?2)
                OR end_time < ?3
              )
            ORDER BY end_time ASC
            LIMIT 500
            "#,
        )?;
        let rows = stmt.query_map(
            params![camera, continuous_before_ts, event_before_ts],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut ids = Vec::new();
        let mut paths = Vec::new();
        for r in rows {
            let (id, path) = r?;
            ids.push(id);
            paths.push(path);
        }
        drop(stmt);

        if !ids.is_empty() {
            let mut del_stmt = conn.prepare("DELETE FROM recordings WHERE id = ?1")?;
            for id in &ids {
                del_stmt.execute(params![id])?;
            }
        }
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

    pub fn query_cases(&self) -> Result<Vec<Value>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, description, created_at, updated_at FROM exportcase ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "description": r.get::<_, Option<String>>(2).ok().flatten(),
                "created_at": value_to_f64(&r.get_ref(3)?).unwrap_or(0.0),
                "updated_at": value_to_f64(&r.get_ref(4)?).unwrap_or(0.0)
            }))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
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
            let src = raw_path
                .strip_prefix("/media/frigate")
                .unwrap_or(&raw_path)
                .to_string();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_password_roundtrip_matches_frigate_format() {
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
            path: "/media/frigate/recordings/2026-11-01/00/front_door/00.00.mp4".to_string(),
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

        let _ = std::fs::remove_file(&tmp);
    }
}
