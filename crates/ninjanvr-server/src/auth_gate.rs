#![forbid(unsafe_code)]
//! Deepened Caller Authorization & Session Gate (`AuthSessionGate`).
//!
//! ## Design-It-Twice Comparison (`/codebase-design`)
//!
//! - **Design A — Unified `AuthSessionGate` + `CallerPrincipal` / `AccessTarget` Seam (Selected):**
//!   Consolidates instance-scoped login rate-limiting (`login_attempts`), PBKDF2 worker
//!   concurrency (`pbkdf2_semaphore`), and OIDC PKCE pending state tracking (`oidc_pending_states`)
//!   inside an `AppState`-owned `AuthSessionGate` struct (eliminating process-global `OnceLock`
//!   singletons). Exposes `CallerPrincipal` (`username`, `role`, `allowed_cameras`, `can_mutate()`,
//!   `is_admin()`, `allows_camera()`, `camera_scope()`) and `AccessTarget<'a>` (`Global`,
//!   `Camera`, `EventId`, `ReviewId`, `DatasetSampleId`) through `authenticate_read(...)` and
//!   `authorize_mutation(...)`, unifying CSRF origin checks, role checks, `"metrics"` role
//!   camera restriction, and target camera resolution in a single call.
//! - **Design B — Process-Global Singletons & Ad-Hoc Per-Handler Checks (Rejected):**
//!   Leaves `login_rate_limiter()` and `auth_pbkdf2_semaphore()` as process-global `OnceLock`
//!   statics (causing cross-instance test pollution) and duplicates 25-line CSRF + role +
//!   `effective_caller_allowed_cameras` + SQLite camera lookup blocks across dozens of handlers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use ninjanvr_config::scrub_rtsp_credentials;
use ninjanvr_db::{CameraScope, verify_password};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::{
    AppState, filter_stats_snapshot_for_allowed_cameras, is_safe_id,
    open_regular_non_symlink_readonly, strip_share_artifact_prefix, unix_now,
};

// ============================================================================
// Part 1 — AuthSessionGate, CallerPrincipal & AccessTarget
// ============================================================================

pub type LoginAttemptKey = (String, String);
pub type LoginAttemptEntry = (u32, Instant);

pub const MAX_LOGIN_FAILURES: u32 = 8;
pub const MAX_IP_LOGIN_FAILURES: u32 = 24;
pub const LOGIN_LOCKOUT_WINDOW: Duration = Duration::from_secs(60);
pub const OIDC_PENDING_STATE_TTL: Duration = Duration::from_secs(600);
pub const MAX_OIDC_PENDING_STATES: usize = 256;
pub const AUTH_CACHE_TTL: Duration = Duration::from_secs(5);
pub const STATIC_MEDIA_CAMERA_CACHE_TTL: Duration = Duration::from_secs(10);
pub const MAX_CACHED_USERS: usize = 1024;
pub const MAX_CACHED_TOKENS: usize = 512;
pub const MAX_CACHED_STATIC_MEDIA: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcPendingState {
    pub code_verifier: String,
    pub redirect_uri: String,
    pub created_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedDbUser {
    pub username: String,
    pub role: String,
    pub allowed_cameras: String,
    pub password_changed_at_epoch: Option<i64>,
}

impl From<&ninjanvr_db::DbUser> for CachedDbUser {
    fn from(u: &ninjanvr_db::DbUser) -> Self {
        Self {
            username: u.username.clone(),
            role: u.role.clone(),
            allowed_cameras: u.allowed_cameras.clone(),
            password_changed_at_epoch: u.password_changed_at_epoch,
        }
    }
}

impl From<ninjanvr_db::DbUser> for CachedDbUser {
    fn from(u: ninjanvr_db::DbUser) -> Self {
        Self {
            username: u.username,
            role: u.role,
            allowed_cameras: u.allowed_cameras,
            password_changed_at_epoch: u.password_changed_at_epoch,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedUserAuth {
    pub db_path: std::path::PathBuf,
    pub user: CachedDbUser,
    pub cached_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthCacheLookupError;

impl std::fmt::Display for AuthCacheLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failed to query user from authentication store")
    }
}

impl std::error::Error for AuthCacheLookupError {}

#[derive(Debug, Clone)]
pub struct CachedTokenVerification {
    pub verified: Option<(String, String)>,
    pub cached_at: Instant,
}

fn hash_api_token_cache_key(raw_token: &str) -> [u8; 32] {
    let digest = Sha256::digest(raw_token.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// `AppState`-owned authentication & session gate replacing process-global singletons.
#[derive(Clone, Debug)]
pub struct AuthSessionGate {
    pub login_attempts: Arc<std::sync::Mutex<HashMap<LoginAttemptKey, LoginAttemptEntry>>>,
    pub ip_login_attempts: Arc<std::sync::Mutex<HashMap<String, LoginAttemptEntry>>>,
    pub pbkdf2_semaphore: Arc<tokio::sync::Semaphore>,
    pub oidc_pending_states: Arc<std::sync::Mutex<HashMap<String, OidcPendingState>>>,
    pub oidc_ip_attempts: Arc<std::sync::Mutex<HashMap<String, (u32, Instant)>>>,
    pub oidc_discovery_cache: Arc<std::sync::RwLock<HashMap<String, (Value, Instant)>>>,
    pub deleted_users: Arc<std::sync::RwLock<HashMap<String, u64>>>,
    pub user_cache: Arc<std::sync::RwLock<HashMap<String, CachedUserAuth>>>,
    pub token_verify_cache: Arc<std::sync::RwLock<HashMap<[u8; 32], CachedTokenVerification>>>,
    pub token_active_cache: Arc<std::sync::RwLock<HashMap<String, (bool, Instant)>>>,
    pub static_media_camera_cache: Arc<std::sync::RwLock<HashMap<String, (String, Instant)>>>,
}

impl Default for AuthSessionGate {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthSessionGate {
    pub fn new() -> Self {
        Self::with_pbkdf2_permits(2)
    }

    pub fn with_pbkdf2_permits(permits: usize) -> Self {
        Self {
            login_attempts: Arc::new(std::sync::Mutex::new(HashMap::new())),
            ip_login_attempts: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pbkdf2_semaphore: Arc::new(tokio::sync::Semaphore::new(permits.max(1))),
            oidc_pending_states: Arc::new(std::sync::Mutex::new(HashMap::new())),
            oidc_ip_attempts: Arc::new(std::sync::Mutex::new(HashMap::new())),
            oidc_discovery_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
            deleted_users: Arc::new(std::sync::RwLock::new(HashMap::new())),
            user_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
            token_verify_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
            token_active_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
            static_media_camera_cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    pub fn pbkdf2_semaphore(&self) -> &tokio::sync::Semaphore {
        &self.pbkdf2_semaphore
    }

    pub fn invalidate_user(&self, username: &str) {
        let key = username.trim().to_ascii_lowercase();
        if let Ok(mut guard) = self.user_cache.write() {
            guard.remove(&key);
        }
    }

    pub fn invalidate_tokens(&self) {
        if let Ok(mut guard) = self.token_verify_cache.write() {
            guard.clear();
        }
        if let Ok(mut guard) = self.token_active_cache.write() {
            guard.clear();
        }
    }

    pub fn invalidate_static_media(&self) {
        if let Ok(mut guard) = self.static_media_camera_cache.write() {
            guard.clear();
        }
    }

    pub fn invalidate_all(&self) {
        if let Ok(mut guard) = self.user_cache.write() {
            guard.clear();
        }
        self.invalidate_tokens();
        self.invalidate_static_media();
    }

    pub fn try_get_cached_user(&self, db_path: &Path, username: &str) -> Option<CachedDbUser> {
        let key = username.trim().to_ascii_lowercase();
        let guard = self.user_cache.read().ok()?;
        let entry = guard.get(&key)?;
        if entry.db_path == db_path && entry.cached_at.elapsed() < AUTH_CACHE_TTL {
            Some(entry.user.clone())
        } else {
            None
        }
    }

    pub fn put_cached_user(&self, db_path: &Path, username: &str, user: CachedDbUser) {
        let key = username.trim().to_ascii_lowercase();
        let now = Instant::now();
        let Ok(mut guard) = self.user_cache.write() else {
            return;
        };
        if !guard.contains_key(&key) && guard.len() >= MAX_CACHED_USERS {
            guard.retain(|_, v| now.duration_since(v.cached_at) < AUTH_CACHE_TTL);
            if guard.len() >= MAX_CACHED_USERS
                && let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, v)| v.cached_at)
                    .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest);
            }
        }
        guard.insert(
            key,
            CachedUserAuth {
                db_path: db_path.to_path_buf(),
                user,
                cached_at: now,
            },
        );
    }

    pub fn get_user_cached_sync(
        &self,
        db: &ninjanvr_db::NinjaDatabase,
        username: &str,
    ) -> Result<Option<CachedDbUser>, AuthCacheLookupError> {
        if let Some(cached) = self.try_get_cached_user(db.path(), username) {
            return Ok(Some(cached));
        }
        match db.get_user(username) {
            Ok(Some(u)) => {
                let mapped = CachedDbUser::from(u);
                self.put_cached_user(db.path(), username, mapped.clone());
                Ok(Some(mapped))
            }
            Ok(None) => Ok(None),
            Err(_) => Err(AuthCacheLookupError),
        }
    }

    pub async fn get_user_cached_async(
        &self,
        db: &ninjanvr_db::NinjaDatabase,
        username: &str,
    ) -> Result<Option<CachedDbUser>, AuthCacheLookupError> {
        if let Some(cached) = self.try_get_cached_user(db.path(), username) {
            return Ok(Some(cached));
        }
        let db_path = db.path().to_path_buf();
        let db_clone = db.clone();
        let user_owned = username.to_string();
        let queried = tokio::task::spawn_blocking(move || db_clone.get_user(&user_owned))
            .await
            .map_err(|_| AuthCacheLookupError)?
            .map_err(|_| AuthCacheLookupError)?;
        if let Some(u) = queried {
            let mapped = CachedDbUser::from(u);
            self.put_cached_user(&db_path, username, mapped.clone());
            Ok(Some(mapped))
        } else {
            Ok(None)
        }
    }

    pub fn try_get_cached_token(&self, raw_token: &str) -> Option<Option<(String, String)>> {
        let key = hash_api_token_cache_key(raw_token);
        let guard = self.token_verify_cache.read().ok()?;
        let entry = guard.get(&key)?;
        if entry.cached_at.elapsed() < AUTH_CACHE_TTL {
            Some(entry.verified.clone())
        } else {
            None
        }
    }

    pub fn put_cached_token(&self, raw_token: &str, verified: Option<(String, String)>) {
        let key = hash_api_token_cache_key(raw_token);
        let now = Instant::now();
        let Ok(mut guard) = self.token_verify_cache.write() else {
            return;
        };
        if !guard.contains_key(&key) && guard.len() >= MAX_CACHED_TOKENS {
            guard.retain(|_, v| now.duration_since(v.cached_at) < AUTH_CACHE_TTL);
            if guard.len() >= MAX_CACHED_TOKENS
                && let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, v)| v.cached_at)
                    .map(|(k, _)| *k)
            {
                guard.remove(&oldest);
            }
        }
        guard.insert(
            key,
            CachedTokenVerification {
                verified,
                cached_at: now,
            },
        );
    }

    pub fn verify_api_token_cached_sync(
        &self,
        db: &ninjanvr_db::NinjaDatabase,
        raw_token: &str,
        now_ts: f64,
    ) -> Option<(String, String)> {
        if let Some(cached) = self.try_get_cached_token(raw_token) {
            return cached;
        }
        let verified = db.verify_api_token(raw_token, now_ts).ok().flatten();
        self.put_cached_token(raw_token, verified.clone());
        verified
    }

    pub async fn verify_api_token_cached_async(
        &self,
        db: &ninjanvr_db::NinjaDatabase,
        raw_token: &str,
        now_ts: f64,
    ) -> Option<(String, String)> {
        if let Some(cached) = self.try_get_cached_token(raw_token) {
            return cached;
        }
        let db_clone = db.clone();
        let tok_owned = raw_token.to_string();
        let verified =
            tokio::task::spawn_blocking(move || db_clone.verify_api_token(&tok_owned, now_ts))
                .await
                .ok()
                .and_then(|r| r.ok())
                .flatten();
        self.put_cached_token(raw_token, verified.clone());
        verified
    }

    pub fn is_api_token_active_cached_sync(
        &self,
        db: &ninjanvr_db::NinjaDatabase,
        name_or_id: &str,
        now_ts: f64,
    ) -> bool {
        let key = name_or_id.trim().to_string();
        if let Ok(guard) = self.token_active_cache.read()
            && let Some((active, at)) = guard.get(&key).copied()
            && at.elapsed() < AUTH_CACHE_TTL
        {
            return active;
        }
        let active = db
            .is_api_token_active_by_name(&key, now_ts)
            .unwrap_or(false);
        if let Ok(mut guard) = self.token_active_cache.write() {
            if !guard.contains_key(&key) && guard.len() >= MAX_CACHED_TOKENS {
                let now = Instant::now();
                guard.retain(|_, (_, at)| now.duration_since(*at) < AUTH_CACHE_TTL);
                if guard.len() >= MAX_CACHED_TOKENS {
                    guard.clear();
                }
            }
            guard.insert(key, (active, Instant::now()));
        }
        active
    }

    pub async fn is_api_token_active_cached_async(
        &self,
        db: &ninjanvr_db::NinjaDatabase,
        name_or_id: &str,
        now_ts: f64,
    ) -> bool {
        let key = name_or_id.trim().to_string();
        if let Ok(guard) = self.token_active_cache.read()
            && let Some((active, at)) = guard.get(&key).copied()
            && at.elapsed() < AUTH_CACHE_TTL
        {
            return active;
        }
        let db_clone = db.clone();
        let key_for_db = key.clone();
        let active = tokio::task::spawn_blocking(move || {
            db_clone
                .is_api_token_active_by_name(&key_for_db, now_ts)
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        if let Ok(mut guard) = self.token_active_cache.write() {
            if !guard.contains_key(&key) && guard.len() >= MAX_CACHED_TOKENS {
                let now = Instant::now();
                guard.retain(|_, (_, at)| now.duration_since(*at) < AUTH_CACHE_TTL);
                if guard.len() >= MAX_CACHED_TOKENS {
                    guard.clear();
                }
            }
            guard.insert(key, (active, Instant::now()));
        }
        active
    }

    pub fn check_login_rate_limited(&self, scope_key: &str, user_key: &str) -> bool {
        let now = Instant::now();
        {
            let mut ip_guard = self
                .ip_login_attempts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((ip_cnt, ip_first)) = ip_guard.get(scope_key).copied() {
                if now.duration_since(ip_first) >= LOGIN_LOCKOUT_WINDOW {
                    ip_guard.remove(scope_key);
                } else if ip_cnt >= MAX_IP_LOGIN_FAILURES {
                    return true;
                }
            }
        }
        let mut guard = self
            .login_attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = (scope_key.to_string(), user_key.to_ascii_lowercase());
        if let Some((count, first_inst)) = guard.get(&key).copied() {
            if now.duration_since(first_inst) >= LOGIN_LOCKOUT_WINDOW {
                guard.remove(&key);
                return false;
            }
            return count >= MAX_LOGIN_FAILURES;
        }
        false
    }

    pub fn record_login_failure(&self, scope_key: &str, user_key: &str) {
        let now = Instant::now();
        {
            let mut ip_guard = self
                .ip_login_attempts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !ip_guard.contains_key(scope_key) && ip_guard.len() >= 1024 {
                ip_guard.retain(|_, (_, inst)| now.duration_since(*inst) < LOGIN_LOCKOUT_WINDOW);
                if ip_guard.len() >= 1024
                    && let Some(unlocked) = ip_guard
                        .iter()
                        .filter(|(_, (cnt, _))| *cnt < MAX_IP_LOGIN_FAILURES)
                        .min_by_key(|(_, (_, inst))| *inst)
                        .map(|(k, _)| k.clone())
                {
                    ip_guard.remove(&unlocked);
                }
            }
            match ip_guard.get_mut(scope_key) {
                Some((cnt, first_inst)) => {
                    if now.duration_since(*first_inst) >= LOGIN_LOCKOUT_WINDOW {
                        *cnt = 1;
                        *first_inst = now;
                    } else {
                        *cnt = cnt.saturating_add(1);
                    }
                }
                None => {
                    if ip_guard.len() < 1024 {
                        ip_guard.insert(scope_key.to_string(), (1, now));
                    }
                }
            }
        }
        let mut guard = self
            .login_attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let key = (scope_key.to_string(), user_key.to_ascii_lowercase());
        if !guard.contains_key(&key) && guard.len() >= 1024 {
            guard.retain(|_, (_, inst)| now.duration_since(*inst) < LOGIN_LOCKOUT_WINDOW);
            if guard.len() >= 1024 {
                // Evict only unlocked single-failure entries so active lockouts cannot be evicted by username floods
                if let Some(unlocked_key) = guard
                    .iter()
                    .filter(|(_, (cnt, _))| *cnt < MAX_LOGIN_FAILURES)
                    .min_by_key(|(_, (_, inst))| *inst)
                    .map(|(k, _)| k.clone())
                {
                    guard.remove(&unlocked_key);
                } else {
                    return;
                }
            }
        }
        match guard.get_mut(&key) {
            Some((count, first_inst)) => {
                if now.duration_since(*first_inst) >= LOGIN_LOCKOUT_WINDOW {
                    *count = 1;
                    *first_inst = now;
                } else {
                    *count = count.saturating_add(1);
                }
            }
            None => {
                guard.insert(key, (1, now));
            }
        }
    }

    pub fn clear_login_failures(&self, scope_key: &str, user_key: &str) {
        let mut guard = self
            .login_attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.remove(&(scope_key.to_string(), user_key.to_ascii_lowercase()));
    }

    pub fn clear_all_login_failures_for_user(&self, user_key: &str) {
        let target = user_key.trim().to_ascii_lowercase();
        let mut guard = self
            .login_attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|(_, u), _| *u != target);
    }

    pub fn store_oidc_pending_state(
        &self,
        state_key: String,
        code_verifier: String,
        redirect_uri: String,
    ) {
        let now = Instant::now();
        let mut guard = self
            .oidc_pending_states
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, entry| now.duration_since(entry.created_at) < OIDC_PENDING_STATE_TTL);
        if guard.len() >= MAX_OIDC_PENDING_STATES
            && !guard.contains_key(&state_key)
            && let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, entry)| entry.created_at)
                .map(|(k, _)| k.clone())
        {
            guard.remove(&oldest_key);
        }
        guard.insert(
            state_key,
            OidcPendingState {
                code_verifier,
                redirect_uri,
                created_at: now,
            },
        );
    }

    pub fn take_oidc_pending_state(&self, state_key: &str) -> Option<OidcPendingState> {
        let now = Instant::now();
        let mut guard = self
            .oidc_pending_states
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, entry| now.duration_since(entry.created_at) < OIDC_PENDING_STATE_TTL);
        guard.remove(state_key)
    }

    pub fn check_and_record_oidc_login_rate(&self, peer_scope: &str) -> bool {
        const OIDC_RATE_WINDOW: Duration = Duration::from_secs(60);
        const MAX_OIDC_STARTS_PER_IP_PER_MIN: u32 = 24;
        let now = Instant::now();
        let mut guard = self
            .oidc_ip_attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !guard.contains_key(peer_scope) && guard.len() >= 1024 {
            guard.retain(|_, (_, at)| now.duration_since(*at) < OIDC_RATE_WINDOW);
            if guard.len() >= 1024 {
                return false;
            }
        }
        match guard.get_mut(peer_scope) {
            Some((cnt, started)) => {
                if now.duration_since(*started) >= OIDC_RATE_WINDOW {
                    *cnt = 1;
                    *started = now;
                    true
                } else if *cnt >= MAX_OIDC_STARTS_PER_IP_PER_MIN {
                    false
                } else {
                    *cnt = cnt.saturating_add(1);
                    true
                }
            }
            None => {
                guard.insert(peer_scope.to_string(), (1, now));
                true
            }
        }
    }

    pub fn get_cached_oidc_discovery(&self, issuer: &str) -> Option<Value> {
        const OIDC_DISCOVERY_TTL: Duration = Duration::from_secs(300);
        let guard = self.oidc_discovery_cache.read().ok()?;
        let (doc, cached_at) = guard.get(issuer)?;
        (cached_at.elapsed() < OIDC_DISCOVERY_TTL).then(|| doc.clone())
    }

    pub fn put_cached_oidc_discovery(&self, issuer: &str, doc: Value) {
        if let Ok(mut guard) = self.oidc_discovery_cache.write() {
            if guard.len() >= 16 && !guard.contains_key(issuer) {
                guard.clear();
            }
            guard.insert(issuer.to_string(), (doc, Instant::now()));
        }
    }

    /// Records the deletion epoch of a local user account so any JWT minted at or before
    /// `deleted_at_epoch` is immediately rejected even if `user_cache` / SQLite returns `Ok(None)`.
    pub fn record_user_deleted(&self, username: &str, deleted_at_epoch: u64) {
        let key = username.trim().to_ascii_lowercase();
        if key.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.deleted_users.write() {
            if guard.len() >= 4096
                && !guard.contains_key(&key)
                && let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, ts)| **ts)
                    .map(|(k, _)| k.clone())
            {
                guard.remove(&oldest);
            }
            guard.insert(key, deleted_at_epoch);
        }
        self.invalidate_user(username);
    }

    /// Returns `true` when `username` was deleted at `deleted_epoch` and `token_iat <= deleted_epoch`.
    pub fn is_token_revoked_by_user_deletion(&self, username: &str, token_iat: u64) -> bool {
        self.deleted_user_epoch(username)
            .is_some_and(|deleted_epoch| token_iat <= deleted_epoch)
    }

    pub fn deleted_user_epoch(&self, username: &str) -> Option<u64> {
        let key = username.trim().to_ascii_lowercase();
        self.deleted_users
            .read()
            .ok()
            .and_then(|g| g.get(&key).copied())
    }

    pub async fn authenticate_read(
        &self,
        state: &AppState,
        jar: &CookieJar,
        headers: &HeaderMap,
    ) -> Result<CallerPrincipal, AuthRejection> {
        authenticate_read(state, jar, headers).await
    }

    pub async fn authorize_admin_read(
        &self,
        state: &AppState,
        jar: &CookieJar,
        headers: &HeaderMap,
    ) -> Result<CallerPrincipal, AuthRejection> {
        authorize_admin_read(state, jar, headers).await
    }

    pub async fn authorize_mutation(
        &self,
        state: &AppState,
        jar: &CookieJar,
        uri: &Uri,
        headers: &HeaderMap,
        require_admin: bool,
        target: AccessTarget<'_>,
    ) -> Result<CallerPrincipal, AuthRejection> {
        authorize_mutation(state, jar, uri, headers, require_admin, target).await
    }

    pub async fn authorize_global_admin_mutation(
        &self,
        state: &AppState,
        jar: &CookieJar,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<CallerPrincipal, AuthRejection> {
        authorize_global_admin_mutation(state, jar, uri, headers).await
    }
}

/// Lightweight authentication/authorization rejection that defers HTTP [`Response`] construction
/// until converted via [`IntoResponse`], keeping `Result<CallerPrincipal, AuthRejection>` compact on the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRejection {
    status: StatusCode,
    message: std::borrow::Cow<'static, str>,
}

impl AuthRejection {
    pub const fn new(status: StatusCode, message: &'static str) -> Self {
        Self {
            status,
            message: std::borrow::Cow::Borrowed(message),
        }
    }

    pub fn with_message(
        status: StatusCode,
        message: impl Into<std::borrow::Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub const fn status(&self) -> StatusCode {
        self.status
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for AuthRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl std::error::Error for AuthRejection {}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "success": false,
                "message": self.message,
            })),
        )
            .into_response()
    }
}

/// Authenticated caller identity, role, and camera scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerPrincipal {
    pub username: String,
    pub role: String,
    pub allowed_cameras: Option<Vec<String>>,
}

impl CallerPrincipal {
    pub fn new(
        username: impl Into<String>,
        role: impl Into<String>,
        allowed_cameras: Option<Vec<String>>,
    ) -> Self {
        Self {
            username: username.into(),
            role: role.into(),
            allowed_cameras,
        }
    }

    pub fn is_admin(&self) -> bool {
        self.role.trim().eq_ignore_ascii_case("admin")
    }

    pub fn can_mutate(&self) -> bool {
        let role_lc = self.role.trim().to_ascii_lowercase();
        !matches!(
            role_lc.as_str(),
            "viewer" | "read_only" | "metrics" | "unauthorized"
        )
    }

    pub fn allows_camera(&self, cam: &str) -> bool {
        let target = cam.trim();
        if target.is_empty() {
            return false;
        }
        match &self.allowed_cameras {
            None => true,
            Some(list) => list.iter().any(|a| {
                let t = a.trim();
                t == "*" || t == target
            }),
        }
    }

    pub fn camera_scope(&self) -> CameraScope {
        CameraScope::from_option(self.allowed_cameras.clone())
    }
}

/// Target resource for single-call mutation authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessTarget<'a> {
    Global,
    Camera(&'a str),
    EventId(&'a str),
    ReviewId(&'a str),
    DatasetSampleId(&'a str),
}

/// Returns `true` when `ip` belongs to loopback, RFC1918 private, link-local,
/// RFC 6598 CGNAT (`100.64.0.0/10`), or IPv6 Unique Local (`fc00::/7`) address space.
pub fn is_internal_peer_ip(ip: std::net::IpAddr) -> bool {
    match ip.to_canonical() {
        std::net::IpAddr::V4(v4) => {
            let oct = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (oct[0] == 100 && (oct[1] & 0b1100_0000) == 64)
        }
        std::net::IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            v6.is_loopback() || (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
        }
    }
}

/// Returns `true` when an `x-peer-ip` header is present and resolves to a non-internal
/// (public internet or malformed) IP address, requiring explicit authentication even on
/// the internal HTTP router.
pub fn is_external_public_peer_ip(headers: &HeaderMap) -> bool {
    let Some(raw) = headers.get("x-peer-ip").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    match trimmed.parse::<std::net::IpAddr>() {
        Ok(ip) => !is_internal_peer_ip(ip),
        Err(_) => true,
    }
}

/// Returns `true` when the request requires strict credential enforcement (`x-ninjanvr-enforce-auth`
/// injected by `build_router_with_auth(..., true)`, supplied Bearer/cookie/proxy credentials, or
/// external non-internal `x-peer-ip`).
pub fn request_requires_strict_auth(
    headers: &HeaderMap,
    jar: &CookieJar,
    auth_cfg: &ninjanvr_config::AuthConfig,
) -> bool {
    headers.contains_key("x-ninjanvr-enforce-auth")
        || headers.contains_key(header::AUTHORIZATION)
        || jar.get(&auth_cfg.cookie_name).is_some()
        || (auth_cfg.proxy.enabled && extract_trusted_proxy_peer_ip(headers).is_some())
        || is_external_public_peer_ip(headers)
}

/// Pre-warms `AuthSessionGate`'s in-memory user/token cache via `spawn_blocking` on cold cache
/// misses so downstream principal extraction resolves in `O(1)` RAM without blocking Tokio workers.
pub async fn warm_auth_cache_for_request(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    auth_cfg: &ninjanvr_config::AuthConfig,
) {
    if !auth_cfg.enabled {
        return;
    }
    if auth_cfg.proxy.enabled && !headers.contains_key(header::AUTHORIZATION) {
        let trusted_cidrs = if !auth_cfg.proxy.trusted_proxies.is_empty() {
            &auth_cfg.proxy.trusted_proxies
        } else {
            &auth_cfg.trusted_proxies
        };
        if let Some(ip) = extract_trusted_proxy_peer_ip(headers)
            && ip_matches_trusted_proxies(ip, trusted_cidrs)
        {
            let user_hdr_trimmed = auth_cfg.proxy.user_header.trim();
            let user_hdr_name = if user_hdr_trimmed.is_empty() {
                "remote-user".to_string()
            } else {
                user_hdr_trimmed.to_ascii_lowercase()
            };
            if let Some(remote_user) = headers
                .get(user_hdr_name.as_str())
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty() && !is_reserved_token_principal_prefix(s))
            {
                let _ = state
                    .auth_gate
                    .get_user_cached_async(&state.db, remote_user)
                    .await;
                return;
            }
        }
    }

    let token_opt = if let Some(auth_val) = headers.get(header::AUTHORIZATION) {
        auth_val
            .to_str()
            .ok()
            .and_then(parse_authorization_bearer)
            .map(str::to_string)
    } else {
        jar.get(&auth_cfg.cookie_name)
            .map(|c| c.value().to_string())
    };
    let Some(token_str) = token_opt else {
        return;
    };
    if token_str.starts_with("nnvr_") {
        let _ = state
            .auth_gate
            .verify_api_token_cached_async(&state.db, &token_str, unix_now() as f64)
            .await;
        return;
    }
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    if let Ok(decoded) = jsonwebtoken::decode::<JwtClaims>(
        &token_str,
        &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
        &validation,
    ) && !is_reserved_token_principal_prefix(&decoded.claims.sub)
    {
        let _ = state
            .auth_gate
            .get_user_cached_async(&state.db, &decoded.claims.sub)
            .await;
    }
}

/// Authenticates a read caller and resolves their effective camera scope (restricting
/// `"metrics"` tokens to `Some(Vec::new())`).
pub async fn authenticate_read(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Result<CallerPrincipal, AuthRejection> {
    let auth_cfg = state.config.read().await.auth.clone();
    if !auth_cfg.enabled {
        return Ok(CallerPrincipal {
            username: "admin".to_string(),
            role: "admin".to_string(),
            allowed_cameras: None,
        });
    }
    warm_auth_cache_for_request(state, jar, headers, &auth_cfg).await;
    if let Some((user, role)) = extract_authenticated_user_with_cfg(state, jar, headers, &auth_cfg)
    {
        let allowed_cameras = if role.trim().eq_ignore_ascii_case("metrics") {
            Some(Vec::new())
        } else {
            resolve_user_allowed_cameras(state, jar, headers, &auth_cfg, &user, &role)
        };
        return Ok(CallerPrincipal {
            username: user,
            role,
            allowed_cameras,
        });
    }
    if request_requires_strict_auth(headers, jar, &auth_cfg) {
        Err(AuthRejection::new(StatusCode::UNAUTHORIZED, "Unauthorized"))
    } else {
        Ok(CallerPrincipal {
            username: "admin".to_string(),
            role: "admin".to_string(),
            allowed_cameras: None,
        })
    }
}

/// Enforces that a read caller on an admin-only endpoint holds the unrestricted `admin` role.
pub async fn authorize_admin_read(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Result<CallerPrincipal, AuthRejection> {
    let principal = authenticate_read(state, jar, headers).await?;
    if !principal.is_admin() || principal.camera_scope().is_restricted() {
        return Err(AuthRejection::new(
            StatusCode::FORBIDDEN,
            "Admin role required",
        ));
    }
    Ok(principal)
}

/// Verifies CSRF origin, mutation role / admin requirement, and target camera access
/// (`Global`, `Camera`, `EventId`, `ReviewId`, or `DatasetSampleId`) in one call.
pub async fn authorize_mutation(
    state: &AppState,
    jar: &CookieJar,
    uri: &Uri,
    headers: &HeaderMap,
    require_admin: bool,
    target: AccessTarget<'_>,
) -> Result<CallerPrincipal, AuthRejection> {
    if !verify_cookie_csrf_origin(headers, uri) {
        return Err(AuthRejection::new(
            StatusCode::FORBIDDEN,
            "Cross-origin cookie request rejected (Origin/Host mismatch)",
        ));
    }

    let principal = {
        let auth_cfg = state.config.read().await.auth.clone();
        if !auth_cfg.enabled {
            CallerPrincipal {
                username: "admin".to_string(),
                role: "admin".to_string(),
                allowed_cameras: None,
            }
        } else {
            warm_auth_cache_for_request(state, jar, headers, &auth_cfg).await;
            let must_auth = request_requires_strict_auth(headers, jar, &auth_cfg);
            if !must_auth {
                CallerPrincipal {
                    username: "admin".to_string(),
                    role: "admin".to_string(),
                    allowed_cameras: None,
                }
            } else {
                let Some((user, role)) =
                    extract_authenticated_user_with_cfg(state, jar, headers, &auth_cfg)
                else {
                    return Err(AuthRejection::new(StatusCode::UNAUTHORIZED, "Unauthorized"));
                };
                let allowed_cameras = if role.trim().eq_ignore_ascii_case("metrics") {
                    Some(Vec::new())
                } else {
                    resolve_user_allowed_cameras(state, jar, headers, &auth_cfg, &user, &role)
                };
                CallerPrincipal {
                    username: user,
                    role,
                    allowed_cameras,
                }
            }
        }
    };

    if !principal.can_mutate() {
        return Err(AuthRejection::new(
            StatusCode::FORBIDDEN,
            if require_admin {
                "Admin role required"
            } else {
                "Insufficient role for mutation"
            },
        ));
    }

    if require_admin && !principal.is_admin() {
        return Err(AuthRejection::new(
            StatusCode::FORBIDDEN,
            "Admin role required",
        ));
    }

    if principal.camera_scope().is_restricted() {
        match target {
            AccessTarget::Global => {}
            AccessTarget::Camera(cam) => {
                if !principal.allows_camera(cam) {
                    return Err(AuthRejection::with_message(
                        StatusCode::FORBIDDEN,
                        format!("Camera '{cam}' is not permitted for this user"),
                    ));
                }
            }
            AccessTarget::EventId(event_id) => {
                let db_clone = state.db.clone();
                let eid_owned = event_id.to_string();
                let ev_opt = tokio::task::spawn_blocking(move || db_clone.get_event(&eid_owned))
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .flatten();
                let permitted = ev_opt
                    .and_then(|ev| ev["camera"].as_str().map(|c| principal.allows_camera(c)))
                    .unwrap_or(false);
                if !permitted {
                    return Err(AuthRejection::new(
                        StatusCode::FORBIDDEN,
                        "Camera is not permitted for this user",
                    ));
                }
            }
            AccessTarget::ReviewId(review_id) => {
                let db_clone = state.db.clone();
                let rid_owned = review_id.to_string();
                let rev_opt =
                    tokio::task::spawn_blocking(move || db_clone.get_review_by_id(&rid_owned))
                        .await
                        .ok()
                        .and_then(|r| r.ok())
                        .flatten();
                let permitted = rev_opt.is_some_and(|r| principal.allows_camera(&r.camera));
                if !permitted {
                    return Err(AuthRejection::new(
                        StatusCode::FORBIDDEN,
                        "Camera is not permitted for this user",
                    ));
                }
            }
            AccessTarget::DatasetSampleId(sample_id) => {
                let db_clone = state.db.clone();
                let sid_owned = sample_id.to_string();
                let sample_opt =
                    tokio::task::spawn_blocking(move || db_clone.get_dataset_sample(&sid_owned))
                        .await
                        .ok()
                        .and_then(|r| r.ok())
                        .flatten();
                let permitted = sample_opt.is_some_and(|s| principal.allows_camera(&s.camera));
                if !permitted {
                    return Err(AuthRejection::new(
                        StatusCode::FORBIDDEN,
                        "Camera is not permitted for this user",
                    ));
                }
            }
        }
    }

    Ok(principal)
}

/// Verifies CSRF origin and requires an unrestricted (`allowed_cameras` is `None` or `"*"`)
/// `admin` role for system-wide configuration, user, and API token mutations.
pub async fn authorize_global_admin_mutation(
    state: &AppState,
    jar: &CookieJar,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<CallerPrincipal, AuthRejection> {
    let principal =
        authorize_mutation(state, jar, uri, headers, true, AccessTarget::Global).await?;
    if principal.camera_scope().is_restricted() {
        return Err(AuthRejection::new(
            StatusCode::FORBIDDEN,
            "Admin role required",
        ));
    }
    Ok(principal)
}

// ============================================================================
// Part 2 — JWT Secret Management, Path Normalization & Auth Gate Middleware
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    pub sub: String,
    pub role: String,
    #[serde(default)]
    pub allowed_cameras: Option<String>,
    pub exp: u64,
    pub iat: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginBody {
    pub user: String,
    pub password: String,
}

pub fn normalize_jwt_secret_entropy(raw: &str) -> String {
    if raw.len() >= 32 {
        return raw.to_string();
    }
    let mut derived = [0u8; 32];
    let mut mac = Hmac::<Sha256>::new_from_slice(b"ninjanvr-jwt-secret-kdf-v1")
        .expect("HMAC accepts any key");
    mac.update(raw.as_bytes());
    derived.copy_from_slice(&mac.finalize().into_bytes());
    for round in 1..10_000u32 {
        let mut rmac = Hmac::<Sha256>::new_from_slice(b"ninjanvr-jwt-secret-kdf-v1")
            .expect("HMAC accepts any key");
        rmac.update(&derived);
        rmac.update(raw.as_bytes());
        rmac.update(&round.to_be_bytes());
        derived.copy_from_slice(&rmac.finalize().into_bytes());
    }
    derived.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn load_or_create_jwt_secret(config_dir: &Path) -> String {
    if let Ok(env_secret) = std::env::var("NINJANVR_JWT_SECRET") {
        let trimmed = env_secret.trim();
        if !trimmed.is_empty() {
            return normalize_jwt_secret_entropy(trimmed);
        }
    }
    let secret_path = config_dir.join(".jwt_secret");
    if let Ok((mut file, fmeta)) = open_regular_non_symlink_readonly(&secret_path)
        && fmeta.len() > 0
        && fmeta.len() <= 4096
    {
        use std::io::Read;
        let mut raw = Vec::with_capacity(fmeta.len() as usize);
        if (&mut file).take(4097).read_to_end(&mut raw).is_ok()
            && raw.len() <= 4096
            && let Ok(existing) = String::from_utf8(raw)
        {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(err) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
                        warn!(
                            "Failed to enforce 0o600 permissions on {}: {err}",
                            secret_path.display()
                        );
                    }
                }
                return normalize_jwt_secret_entropy(trimmed);
            }
        }
    }
    let mut bytes = [0u8; 64];
    rand::fill(&mut bytes);
    let secret: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let tmp_path = config_dir.join(format!(
        ".jwt_secret.tmp.{}.{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let persist_res = (|| -> std::io::Result<()> {
            if std::fs::symlink_metadata(&secret_path)
                .is_ok_and(|m| m.file_type().is_dir() || m.file_type().is_symlink())
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Refusing to overwrite directory or symlink at .jwt_secret",
                ));
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(ninjanvr_config::O_NOFOLLOW_FLAG)
                .open(&tmp_path)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(secret.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &secret_path)?;
            Ok(())
        })();
        if let Err(err) = persist_res {
            let _ = std::fs::remove_file(&tmp_path);
            warn!(
                "Failed to persist generated JWT secret to {}: {err}",
                secret_path.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let persist_res = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            file.write_all(secret.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &secret_path)?;
            Ok(())
        })();
        if let Err(err) = persist_res {
            let _ = std::fs::remove_file(&tmp_path);
            warn!(
                "Failed to persist generated JWT secret to {}: {err}",
                secret_path.display()
            );
        }
    }
    secret
}

pub fn is_protected_path(path: &str) -> bool {
    if matches!(
        path,
        "/healthz"
            | "/api/healthz"
            | "/readyz"
            | "/api/readyz"
            | "/api/version"
            | "/api/login"
            | "/api/logout"
            | "/api/auth/first_time_login"
            | "/api/auth/providers"
            | "/api/auth/oidc/login"
            | "/api/auth/oidc/callback"
            | "/api/ui/switch"
            | "/api/ui/version"
            | "/logout"
            | "/login"
    ) {
        return false;
    }
    let check_protected = |p: &str| {
        p.starts_with("/api/")
            || p == "/api"
            || p == "/metrics"
            || p.starts_with("/vod/")
            || p == "/vod"
            || p.starts_with("/live/")
            || p == "/live"
            || p == "/ws"
            || p.starts_with("/clips/")
            || p == "/clips"
            || p.starts_with("/recordings/")
            || p == "/recordings"
            || p.starts_with("/exports/")
            || p == "/exports"
    };
    if check_protected(path) {
        return true;
    }
    percent_decode_path(path).is_some_and(|decoded| check_protected(&decoded))
}

pub fn percent_decode_segment(seg: &str) -> Option<String> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                let decoded = ((hi << 4) | lo) as u8;
                if decoded == 0 || decoded == b'/' || decoded == b'\\' {
                    return None;
                }
                out.push(decoded);
                i += 3;
            }
            0 | b'/' | b'\\' => return None,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

pub fn percent_decode_path(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return Some(String::new());
    }
    let mut decoded_segments = Vec::new();
    for seg in raw.split('/') {
        decoded_segments.push(percent_decode_segment(seg)?);
    }
    Some(decoded_segments.join("/"))
}

pub fn normalize_http_path_for_auth(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.chars().any(|c| c.is_control() || c == '\\') {
        return None;
    }
    let decoded = percent_decode_path(raw)?;
    if decoded.chars().any(|c| c.is_control() || c == '\\') {
        return None;
    }
    let mut segments = Vec::new();
    for seg in decoded.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == "." || seg == ".." {
            return None;
        }
        segments.push(seg);
    }
    if segments.is_empty() {
        Some("/".to_string())
    } else {
        Some(format!("/{}", segments.join("/")))
    }
}

pub fn is_safe_go2rtc_stream_name(name: &str) -> bool {
    ninjanvr_config::is_safe_go2rtc_stream_name(name)
}

pub fn query_targets_talkback_variant(
    query: Option<&str>,
    known_cameras: Option<&std::collections::BTreeSet<String>>,
) -> bool {
    let Some(q) = query.filter(|s| !s.trim().is_empty()) else {
        return false;
    };
    for kv in q.split('&') {
        let (raw_k, raw_v) = kv.split_once('=').unwrap_or((kv, ""));
        let Some(k) = percent_decode_segment(raw_k) else {
            continue;
        };
        let Some(v) = percent_decode_segment(raw_v) else {
            continue;
        };
        if (k.eq_ignore_ascii_case("src") || k.eq_ignore_ascii_case("dst"))
            && matches!(
                ninjanvr_config::parse_go2rtc_stream_route(&v, known_cameras).map(|r| r.variant),
                Some(
                    ninjanvr_config::Go2RtcStreamVariant::Talkback
                        | ninjanvr_config::Go2RtcStreamVariant::SubTalkback
                )
            )
        {
            return true;
        }
    }
    false
}

pub fn validate_go2rtc_query_streams(
    query: Option<&str>,
    allowed_set: Option<&[String]>,
) -> Result<Vec<String>, &'static str> {
    validate_go2rtc_query_streams_with_known(query, allowed_set, None)
}

pub fn validate_go2rtc_query_streams_with_known(
    query: Option<&str>,
    allowed_set: Option<&[String]>,
    known_cameras: Option<&std::collections::BTreeSet<String>>,
) -> Result<Vec<String>, &'static str> {
    let Some(q) = query.filter(|s| !s.trim().is_empty()) else {
        return Err("Missing stream src query parameter");
    };
    let mut query_cams = Vec::new();
    for kv in q.split('&') {
        if kv.is_empty() {
            continue;
        }
        let (raw_k, raw_v) = kv.split_once('=').unwrap_or((kv, ""));
        let Some(k) = percent_decode_segment(raw_k) else {
            return Err("Invalid query encoding");
        };
        let Some(v) = percent_decode_segment(raw_v) else {
            return Err("Invalid query encoding");
        };
        if k.chars().any(|c| c.is_control()) || v.chars().any(|c| c.is_control()) {
            return Err("Invalid query encoding");
        }
        if k.eq_ignore_ascii_case("src") || k.eq_ignore_ascii_case("dst") {
            let Some(route) = ninjanvr_config::parse_go2rtc_stream_route(&v, known_cameras) else {
                return Err("Invalid stream identifier");
            };
            if let Some(allowed) = allowed_set
                && !allowed.iter().any(|a| a == &route.camera || a == "*")
            {
                return Err("Stream camera is not permitted for this user");
            }
            query_cams.push(route.camera);
        }
    }
    if query_cams.is_empty() {
        return Err("Missing stream src query parameter");
    }
    Ok(query_cams)
}

fn strip_static_media_prefix(p: &str) -> Option<&str> {
    p.strip_prefix("/exports/")
        .or_else(|| p.strip_prefix("/clips/"))
        .or_else(|| p.strip_prefix("/recordings/"))
}

pub fn is_hidden_static_media_path(path: &str) -> bool {
    let Some(decoded) = percent_decode_path(path) else {
        return true;
    };
    if let Some(rest) = strip_static_media_prefix(path)
        && rest.split('/').filter(|s| !s.is_empty()).any(|seg| {
            seg.starts_with('.')
                || seg.contains("..")
                || seg.starts_with("%2e")
                || seg.starts_with("%2E")
        })
    {
        return true;
    }
    if let Some(rest) = strip_static_media_prefix(&decoded) {
        return rest
            .split('/')
            .filter(|s| !s.is_empty())
            .any(|seg| seg.starts_with('.') || seg.contains(".."));
    }
    false
}

fn lookup_export_camera_by_filename(
    db: &ninjanvr_db::NinjaDatabase,
    fname: &str,
) -> Option<String> {
    if fname.is_empty() {
        return None;
    }
    let mut escaped = String::with_capacity(fname.len());
    for ch in fname.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    let suffix_pattern = format!("%/{escaped}");
    db.with_read_conn(|conn| {
        let mut stmt = conn.prepare_cached(
            "SELECT camera FROM export WHERE video_path = ?1 OR video_path LIKE ?2 ESCAPE '\\' LIMIT 1",
        )?;
        let mut rows = stmt.query([fname, suffix_pattern.as_str()])?;
        if let Some(row) = rows.next()? {
            let cam: String = row.get(0)?;
            if !cam.trim().is_empty() {
                return Ok(Some(cam));
            }
        }
        Ok(None)
    })
    .ok()
    .flatten()
}

fn match_longest_camera_key_prefix<'a>(stem: &str, camera_keys: &'a [String]) -> Option<&'a str> {
    let mut matched_cam: Option<&'a str> = None;
    for cam in camera_keys {
        let prefix = format!("{cam}-");
        if stem.starts_with(&prefix) && matched_cam.is_none_or(|prev| cam.len() > prev.len()) {
            matched_cam = Some(cam.as_str());
        }
    }
    matched_cam
}

fn resolve_fast_static_media_camera_no_db(normalized_path: &str) -> Option<String> {
    if let Some(rest) = normalized_path.strip_prefix("/recordings/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 3 && parts[0].len() == 10 && parts[0].chars().nth(4) == Some('-') {
            return Some(parts[2].to_string());
        }
        if !parts.is_empty() {
            return Some(parts[0].to_string());
        }
        return None;
    }
    if let Some(rest) = normalized_path.strip_prefix("/clips/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if (parts.first() == Some(&"previews") || parts.first() == Some(&"preview_frames"))
            && parts.len() >= 2
        {
            return Some(parts[1].to_string());
        }
        if parts.len() == 1 {
            let raw_stem = parts[0].rsplit_once('.').map_or(parts[0], |(s, _)| s);
            let (is_share_artifact, stem) = strip_share_artifact_prefix(raw_stem);
            if !is_share_artifact
                && let Some(latest_cam) = stem.strip_prefix("latest_").filter(|s| !s.is_empty())
            {
                return Some(latest_cam.to_string());
            }
        }
    }
    None
}

pub fn resolve_static_media_camera(state: &AppState, path: &str) -> Option<String> {
    let camera_keys: Vec<String> = state
        .config
        .try_read()
        .map(|cfg| cfg.cameras.keys().cloned().collect())
        .unwrap_or_default();
    resolve_static_media_camera_with_keys(state, path, &camera_keys)
}

pub async fn resolve_static_media_camera_async(state: &AppState, path: &str) -> Option<String> {
    let camera_keys: Vec<String> = {
        let cfg = state.config.read().await;
        cfg.cameras.keys().cloned().collect()
    };
    resolve_static_media_camera_async_with_keys(state, path, &camera_keys).await
}

pub async fn resolve_static_media_camera_async_with_keys(
    state: &AppState,
    path: &str,
    camera_keys: &[String],
) -> Option<String> {
    if is_hidden_static_media_path(path) {
        return None;
    }
    let normalized = normalize_http_path_for_auth(path)?;
    if is_hidden_static_media_path(&normalized) {
        return None;
    }
    if let Some(fast_cam) = resolve_fast_static_media_camera_no_db(&normalized) {
        return Some(fast_cam);
    }
    if let Ok(guard) = state.auth_gate.static_media_camera_cache.read()
        && let Some((cam, at)) = guard.get(&normalized)
        && at.elapsed() < STATIC_MEDIA_CAMERA_CACHE_TTL
    {
        return Some(cam.clone());
    }
    let state_clone = state.clone();
    let norm_owned = normalized;
    let keys_owned = camera_keys.to_vec();
    tokio::task::spawn_blocking(move || {
        resolve_static_media_camera_with_keys(&state_clone, &norm_owned, &keys_owned)
    })
    .await
    .ok()
    .flatten()
}

fn cache_db_confirmed_static_media_camera(state: &AppState, normalized_path: &str, camera: &str) {
    let Ok(mut guard) = state.auth_gate.static_media_camera_cache.write() else {
        return;
    };
    if !guard.contains_key(normalized_path) && guard.len() >= MAX_CACHED_STATIC_MEDIA {
        let now = Instant::now();
        guard.retain(|_, (_, at)| now.duration_since(*at) < STATIC_MEDIA_CAMERA_CACHE_TTL);
        if guard.len() >= MAX_CACHED_STATIC_MEDIA {
            guard.clear();
        }
    }
    guard.insert(
        normalized_path.to_string(),
        (camera.to_string(), Instant::now()),
    );
}

pub fn resolve_static_media_camera_with_keys(
    state: &AppState,
    path: &str,
    camera_keys: &[String],
) -> Option<String> {
    if is_hidden_static_media_path(path) {
        return None;
    }
    let normalized = normalize_http_path_for_auth(path)?;
    if is_hidden_static_media_path(&normalized) {
        return None;
    }
    if let Some(fast_cam) = resolve_fast_static_media_camera_no_db(&normalized) {
        return Some(fast_cam);
    }
    if let Ok(guard) = state.auth_gate.static_media_camera_cache.read()
        && let Some((cam, at)) = guard.get(&normalized)
        && at.elapsed() < STATIC_MEDIA_CAMERA_CACHE_TTL
    {
        return Some(cam.clone());
    }
    let path = normalized.as_str();
    if let Some(rest) = path.strip_prefix("/exports/") {
        let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if segs.len() != 1 {
            return None;
        }
        let fname = segs[0];
        let raw_id = fname.strip_suffix(".mp4").unwrap_or(fname);
        let (is_share_artifact, exp_id) = strip_share_artifact_prefix(raw_id);
        if let Ok(Some(exp)) = state.db.get_export_by_id(exp_id)
            && let Some(cam) = exp["camera"].as_str()
        {
            cache_db_confirmed_static_media_camera(state, path, cam);
            return Some(cam.to_string());
        }
        if is_share_artifact {
            if let Ok(Some(ev)) = state.db.get_event(exp_id)
                && let Some(cam) = ev["camera"].as_str()
            {
                cache_db_confirmed_static_media_camera(state, path, cam);
                return Some(cam.to_string());
            }
            if let Ok(Some(rev)) = state.db.get_review_by_id(exp_id) {
                cache_db_confirmed_static_media_camera(state, path, &rev.camera);
                return Some(rev.camera);
            }
            return None;
        }
        if let Some(cam) = lookup_export_camera_by_filename(&state.db, fname) {
            cache_db_confirmed_static_media_camera(state, path, &cam);
            return Some(cam);
        }
        return exp_id
            .rsplitn(3, '_')
            .nth(2)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
    }
    if let Some(rest) = path.strip_prefix("/clips/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if parts.first() == Some(&"dataset") && parts.len() == 2 {
            let sample_id = parts[1].rsplit_once('.').map_or(parts[1], |(s, _)| s);
            if let Ok(Some(sample)) = state.db.get_dataset_sample(sample_id)
                && !sample.camera.trim().is_empty()
            {
                cache_db_confirmed_static_media_camera(state, path, &sample.camera);
                return Some(sample.camera);
            }
            return None;
        }
        if parts.first() == Some(&"faces") && parts.len() == 3 {
            let sample_id = parts[2].rsplit_once('.').map_or(parts[2], |(s, _)| s);
            if let Ok(Some(ev)) = state.db.get_event(sample_id)
                && let Some(cam) = ev["camera"].as_str().filter(|s| !s.trim().is_empty())
            {
                cache_db_confirmed_static_media_camera(state, path, cam);
                return Some(cam.to_string());
            }
            return None;
        }
        if parts.first() == Some(&"review") && parts.len() >= 2 {
            if let Some(raw_after_thumb) = parts[1].strip_prefix("thumb-") {
                let after_thumb = raw_after_thumb
                    .strip_suffix(".webp")
                    .or_else(|| raw_after_thumb.strip_suffix(".jpg"))
                    .unwrap_or(raw_after_thumb);
                if let Ok(Some(rev)) = state.db.get_review_by_id(after_thumb) {
                    cache_db_confirmed_static_media_camera(state, path, &rev.camera);
                    return Some(rev.camera);
                }
                let matched_cam = match_longest_camera_key_prefix(after_thumb, camera_keys);
                if let Some(cam_prefix) = matched_cam
                    && let Some(exact_rev_id) = after_thumb
                        .strip_prefix(&format!("{cam_prefix}-"))
                        .filter(|s| !s.is_empty())
                    && let Ok(Some(rev)) = state.db.get_review_by_id(exact_rev_id)
                {
                    cache_db_confirmed_static_media_camera(state, path, &rev.camera);
                    return Some(rev.camera);
                }
                for (idx, _) in after_thumb.match_indices('-').take(6) {
                    let candidate_rev_id = &after_thumb[idx + 1..];
                    if !candidate_rev_id.is_empty()
                        && let Ok(Some(rev)) = state.db.get_review_by_id(candidate_rev_id)
                    {
                        cache_db_confirmed_static_media_camera(state, path, &rev.camera);
                        return Some(rev.camera);
                    }
                }
                if let Some(cam) = matched_cam {
                    return Some(cam.to_string());
                }
                if let Some(cam) = after_thumb
                    .rsplitn(3, '-')
                    .nth(2)
                    .or_else(|| after_thumb.split_once('-').map(|(c, _)| c))
                    .filter(|s| !s.is_empty())
                {
                    return Some(cam.to_string());
                }
            }
            return None;
        }
        if parts.len() == 1 {
            let raw_stem = parts[0].rsplit_once('.').map_or(parts[0], |(s, _)| s);
            let (is_share_artifact, stem) = strip_share_artifact_prefix(raw_stem);
            let clean_stem = stem.strip_suffix("-clean").unwrap_or(stem);
            for candidate in [clean_stem, stem] {
                if let Ok(Some(ev)) = state.db.get_event(candidate)
                    && let Some(cam) = ev["camera"].as_str()
                {
                    cache_db_confirmed_static_media_camera(state, path, cam);
                    return Some(cam.to_string());
                }
                if let Ok(Some(exp)) = state.db.get_export_by_id(candidate)
                    && let Some(cam) = exp["camera"].as_str()
                {
                    cache_db_confirmed_static_media_camera(state, path, cam);
                    return Some(cam.to_string());
                }
                if let Ok(Some(rev)) = state.db.get_review_by_id(candidate) {
                    cache_db_confirmed_static_media_camera(state, path, &rev.camera);
                    return Some(rev.camera);
                }
            }
            if is_share_artifact {
                return None;
            }
            let matched_cam = match_longest_camera_key_prefix(clean_stem, camera_keys);
            if let Some(cam_prefix) = matched_cam
                && let Some(exact_ev_id) = clean_stem
                    .strip_prefix(&format!("{cam_prefix}-"))
                    .filter(|s| !s.is_empty())
            {
                if let Ok(Some(ev)) = state.db.get_event(exact_ev_id)
                    && let Some(ev_cam) = ev["camera"].as_str()
                {
                    cache_db_confirmed_static_media_camera(state, path, ev_cam);
                    return Some(ev_cam.to_string());
                }
                if let Ok(Some(exp)) = state.db.get_export_by_id(exact_ev_id)
                    && let Some(exp_cam) = exp["camera"].as_str()
                {
                    cache_db_confirmed_static_media_camera(state, path, exp_cam);
                    return Some(exp_cam.to_string());
                }
                if let Ok(Some(rev)) = state.db.get_review_by_id(exact_ev_id) {
                    cache_db_confirmed_static_media_camera(state, path, &rev.camera);
                    return Some(rev.camera);
                }
            }
            for (idx, _) in clean_stem.match_indices('-').take(6) {
                let ev_part = &clean_stem[idx + 1..];
                if !ev_part.is_empty() {
                    if let Ok(Some(ev)) = state.db.get_event(ev_part)
                        && let Some(ev_cam) = ev["camera"].as_str()
                    {
                        cache_db_confirmed_static_media_camera(state, path, ev_cam);
                        return Some(ev_cam.to_string());
                    }
                    if let Ok(Some(exp)) = state.db.get_export_by_id(ev_part)
                        && let Some(exp_cam) = exp["camera"].as_str()
                    {
                        cache_db_confirmed_static_media_camera(state, path, exp_cam);
                        return Some(exp_cam.to_string());
                    }
                    if let Ok(Some(rev)) = state.db.get_review_by_id(ev_part) {
                        cache_db_confirmed_static_media_camera(state, path, &rev.camera);
                        return Some(rev.camera);
                    }
                }
            }
            if let Some(cam) = matched_cam {
                return Some(cam.to_string());
            }
        }
    }
    None
}

pub fn path_matches_segment_prefix(path: &str, prefix: &str) -> bool {
    let clean_prefix = prefix.trim_end_matches('/');
    path == clean_prefix
        || path
            .strip_prefix(clean_prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

pub async fn auth_gate_middleware(
    State((state, enforce_auth)): State<(AppState, bool)>,
    jar: CookieJar,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().as_str().to_string();
    let raw_path = req.uri().path().to_string();
    if is_hidden_static_media_path(&raw_path) {
        state.log_hub.push_nginx(&method, &raw_path, 403);
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Hidden or internal static media artifacts are forbidden" })),
        )
            .into_response();
    }
    let Some(path) = normalize_http_path_for_auth(&raw_path) else {
        state.log_hub.push_nginx(&method, &raw_path, 403);
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Invalid or forbidden request path" })),
        )
            .into_response();
    };
    if is_hidden_static_media_path(&path) {
        state.log_hub.push_nginx(&method, &path, 403);
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Hidden or internal static media artifacts are forbidden" })),
        )
            .into_response();
    }
    if enforce_auth {
        req.headers_mut().insert(
            axum::http::HeaderName::from_static("x-ninjanvr-enforce-auth"),
            axum::http::HeaderValue::from_static("1"),
        );
    }
    let (auth_cfg, camera_keys, known_cameras, active_policies) = {
        let cfg = state.config.read().await;
        let keys: Vec<String> = cfg.cameras.keys().cloned().collect();
        let known: std::collections::BTreeSet<String> = keys.iter().cloned().collect();
        let policies = crate::media_redaction::MediaDeliveryGate::configured_camera_policies(&cfg);
        (cfg.auth.clone(), keys, known, policies)
    };
    let is_static_media_route = path == "/exports"
        || path.starts_with("/exports/")
        || path == "/recordings"
        || path.starts_with("/recordings/")
        || path == "/clips"
        || path.starts_with("/clips/");
    let has_supplied_credentials = req.headers().contains_key(header::AUTHORIZATION)
        || jar.get(&auth_cfg.cookie_name).is_some()
        || (auth_cfg.proxy.enabled && extract_trusted_proxy_peer_ip(req.headers()).is_some());
    if auth_cfg.enabled
        && (enforce_auth
            || is_external_public_peer_ip(req.headers())
            || (is_static_media_route && has_supplied_credentials))
        && is_protected_path(&path)
    {
        warm_auth_cache_for_request(&state, &jar, req.headers(), &auth_cfg).await;
        let Some((user, role)) =
            extract_authenticated_user_with_cfg(&state, &jar, req.headers(), &auth_cfg)
        else {
            state.log_hub.push_nginx(&method, &path, 401);
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "Unauthorized" })),
            )
                .into_response();
        };

        if method != "GET"
            && method != "HEAD"
            && method != "OPTIONS"
            && !verify_cookie_csrf_origin(req.headers(), req.uri())
        {
            state.log_hub.push_nginx(&method, &path, 403);
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "success": false,
                    "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
                })),
            )
                .into_response();
        }

        let role_lc = role.to_ascii_lowercase();
        if role_lc == "metrics" {
            let is_metrics_route = (method == "GET" || method == "HEAD")
                && (path == "/api/stats"
                    || path == "/api/stats/history"
                    || path == "/api/metrics"
                    || path == "/metrics");
            if !is_metrics_route {
                state.log_hub.push_nginx(&method, &path, 403);
                return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "message": "Token scope 'metrics' only permits GET /api/stats and /api/metrics" })),
                    )
                        .into_response();
            }
        }

        let is_admin_only = path_matches_segment_prefix(&path, "/api/config/save")
            || path_matches_segment_prefix(&path, "/api/config/structured")
            || path_matches_segment_prefix(&path, "/api/config/set")
            || path_matches_segment_prefix(&path, "/api/config/raw")
            || path_matches_segment_prefix(&path, "/api/restart")
            || path_matches_segment_prefix(&path, "/api/tokens")
            || path_matches_segment_prefix(&path, "/api/users")
            || path_matches_segment_prefix(&path, "/api/camera-proxy")
            || path_matches_segment_prefix(&path, "/api/recordings/sync")
            || path_matches_segment_prefix(&path, "/api/logs")
            || (path.starts_with("/api/") && path.ends_with("/logs"))
            || path_matches_segment_prefix(&path, "/api/genai/models")
            || path_matches_segment_prefix(&path, "/api/genai/validate")
            || path_matches_segment_prefix(&path, "/api/detectors/models/upload")
            || path_matches_segment_prefix(&path, "/api/streams/probe")
            || path_matches_segment_prefix(&path, "/api/federation")
            || path_matches_segment_prefix(&path, "/api/debug")
            || path_matches_segment_prefix(&path, "/api/notifications/test")
            || path == "/api/exports/delete"
            || path == "/api/reviews/delete"
            || path == "/api/events/delete"
            || (method != "GET"
                && method != "HEAD"
                && (path_matches_segment_prefix(&path, "/api/faces")
                    || path_matches_segment_prefix(&path, "/api/dataset")
                    || path_matches_segment_prefix(&path, "/api/false_positives")))
            || (method == "DELETE"
                && !path.ends_with("/retain")
                && !path.ends_with("/viewed")
                && (path.ends_with("/recordings")
                    || path_matches_segment_prefix(&path, "/api/events")
                    || path_matches_segment_prefix(&path, "/api/review")
                    || path_matches_segment_prefix(&path, "/api/export")));
        if is_admin_only && role_lc != "admin" {
            state.log_hub.push_nginx(&method, &path, 403);
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Admin role required" })),
            )
                .into_response();
        }

        if role_lc == "read_only"
            && !path_matches_segment_prefix(&path, "/api/mcp")
            && ((method != "GET" && method != "HEAD")
                || !(path.starts_with("/api/") || path == "/api" || path == "/metrics"))
        {
            state.log_hub.push_nginx(&method, &path, 403);
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Token scope 'read_only' only permits GET /api/* and /metrics" })),
            )
                .into_response();
        }

        if role_lc == "viewer"
            && method != "GET"
            && method != "HEAD"
            && !path_matches_segment_prefix(&path, "/api/logout")
            && !path_matches_segment_prefix(&path, "/api/mcp")
        {
            state.log_hub.push_nginx(&method, &path, 403);
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Viewer role is read-only" })),
            )
                .into_response();
        }

        // Enforce go2rtc stream syntax and camera scoping unconditionally for all callers
        let allowed_set_opt = if role_lc != "admin" {
            resolve_user_allowed_cameras(&state, &jar, req.headers(), &auth_cfg, &user, &role)
        } else {
            None
        };
        if path.starts_with("/live/") || path == "/live" || path == "/api/go2rtc/webrtc" {
            if let Err(msg) = validate_go2rtc_query_streams_with_known(
                req.uri().query(),
                allowed_set_opt.as_deref(),
                Some(&known_cameras),
            ) {
                let status = if msg == "Stream camera is not permitted for this user"
                    || (msg == "Missing stream src query parameter" && allowed_set_opt.is_some())
                {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::BAD_REQUEST
                };
                state.log_hub.push_nginx(&method, &path, status.as_u16());
                return (status, Json(json!({ "message": msg }))).into_response();
            }
            if matches!(role_lc.as_str(), "viewer" | "read_only" | "metrics")
                && query_targets_talkback_variant(req.uri().query(), Some(&known_cameras))
            {
                state.log_hub.push_nginx(&method, &path, 403);
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "message": "Read-only role is not permitted to use audio talkback" })),
                )
                    .into_response();
            }
        }
        if let Some(after_streams) = path.strip_prefix("/api/go2rtc/streams/") {
            let raw_seg = after_streams
                .strip_suffix("/prewarm")
                .unwrap_or(after_streams)
                .trim_matches('/');
            let Some(decoded_seg) = percent_decode_segment(raw_seg) else {
                state.log_hub.push_nginx(&method, &path, 400);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": "Invalid stream identifier" })),
                )
                    .into_response();
            };
            let Some(route) =
                ninjanvr_config::parse_go2rtc_stream_route(&decoded_seg, Some(&known_cameras))
            else {
                state.log_hub.push_nginx(&method, &path, 400);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": "Invalid stream identifier" })),
                )
                    .into_response();
            };
            let base_cam = route.camera.as_str();
            if let Some(ref allowed_set) = allowed_set_opt
                && (base_cam.is_empty() || !allowed_set.iter().any(|a| a == base_cam || a == "*"))
            {
                state.log_hub.push_nginx(&method, &path, 403);
                return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "message": format!("Camera '{base_cam}' is not permitted for this user") })),
                    )
                        .into_response();
            }
        }

        // Enforce per-camera scoping when user is restricted to specific cameras
        if let Some(allowed_set) = allowed_set_opt {
            let is_static_media = path.starts_with("/exports/")
                || path == "/exports"
                || path.starts_with("/recordings/")
                || path == "/recordings"
                || path.starts_with("/clips/")
                || path == "/clips";
            let static_media_cam = if is_static_media {
                resolve_static_media_camera_async_with_keys(&state, &path, &camera_keys).await
            } else {
                None
            };
            if is_static_media
                && static_media_cam.is_none()
                && !allowed_set.iter().any(|a| a == "*")
            {
                state.log_hub.push_nginx(&method, &path, 403);
                return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "message": "Static media resource is not permitted for this user" })),
                    )
                        .into_response();
            }
            let query_cam = if path.starts_with("/api/go2rtc") {
                req.uri().query().and_then(|q| {
                    q.split('&').find_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        let dk = percent_decode_segment(k)?;
                        let dv = percent_decode_segment(v)?;
                        if dk.eq_ignore_ascii_case("src") || dk.eq_ignore_ascii_case("dst") {
                            ninjanvr_config::parse_go2rtc_stream_route(&dv, Some(&known_cameras))
                                .map(|r| r.camera)
                        } else {
                            None
                        }
                    })
                })
            } else {
                None
            };
            let target_cam = if let Some(ref sc) = static_media_cam {
                Some(sc.as_str())
            } else if let Some(ref qc) = query_cam {
                Some(qc.as_str())
            } else if path == "/vod" || path.starts_with("/vod/") {
                let rest = path.strip_prefix("/vod/").unwrap_or("");
                let vod_rest = rest
                    .strip_prefix("clip/")
                    .unwrap_or_else(|| if rest == "clip" { "" } else { rest });
                let vod_cam = vod_rest.split('/').find(|s| !s.is_empty()).unwrap_or("");
                if vod_cam.is_empty() && !allowed_set.iter().any(|a| a == "*") {
                    state.log_hub.push_nginx(&method, &path, 403);
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "message": "VOD camera is not permitted for this user" })),
                    )
                        .into_response();
                }
                Some(vod_cam)
            } else if let Some(rest) = path.strip_prefix("/api/") {
                let first = rest.split('/').next().unwrap_or("");
                let after_first = rest
                    .strip_prefix(first)
                    .unwrap_or("")
                    .trim_start_matches('/');
                let is_camera_subroute = matches!(
                    after_first,
                    "recordings"
                        | "recordings/summary"
                        | "ptz/info"
                        | "ptz/move"
                        | "ptz/stop"
                        | "ptz/preset"
                        | "hardware"
                        | "logs"
                        | "audio/talkback"
                        | "state"
                        | "latest.webp"
                        | "latest.jpg"
                        | "snapshot.jpg"
                        | "frame"
                        | "frame.jpg"
                ) && !(first == "recordings" && after_first == "summary");
                if is_camera_subroute {
                    Some(first)
                } else if first == "preview" {
                    if after_first.contains("/start/")
                        || after_first.contains("/frame/")
                        || after_first.contains("/at/")
                    {
                        let p_cam = after_first.split('/').next().unwrap_or("");
                        if p_cam.is_empty() || p_cam == "all" {
                            None
                        } else {
                            Some(p_cam)
                        }
                    } else {
                        None
                    }
                } else if matches!(
                    first,
                    "version"
                        | "ui"
                        | "auth"
                        | "login"
                        | "logout"
                        | "profile"
                        | "profiles"
                        | "users"
                        | "config"
                        | "detectors"
                        | "dataset"
                        | "ffmpeg"
                        | "ffprobe"
                        | "streams"
                        | "keyframe_analysis"
                        | "restart"
                        | "logs"
                        | "stats"
                        | "review"
                        | "review_ids"
                        | "reviews"
                        | "events"
                        | "event_ids"
                        | "false_positives"
                        | "timeline"
                        | "recordings"
                        | "exports"
                        | "jobs"
                        | "cases"
                        | "share"
                        | "tokens"
                        | "metrics"
                        | "ai"
                        | "mcp"
                        | "debug"
                        | "notifications"
                        | "federation"
                        | "camera-proxy"
                        | "go2rtc"
                        | "ws"
                        | "labels"
                        | "sub_labels"
                        | "audio_labels"
                        | "recognized_license_plates"
                        | "classification"
                        | "genai"
                        | "plus"
                        | "faces"
                ) {
                    None
                } else if first == "vod" {
                    let after_clip = after_first.strip_prefix("clip/").unwrap_or_else(|| {
                        if after_first == "clip" {
                            ""
                        } else {
                            after_first
                        }
                    });
                    let vod_cam = after_clip.split('/').find(|s| !s.is_empty()).unwrap_or("");
                    if vod_cam.is_empty() && !allowed_set.iter().any(|a| a == "*") {
                        state.log_hub.push_nginx(&method, &path, 403);
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({ "message": "VOD camera is not permitted for this user" })),
                        )
                            .into_response();
                    }
                    Some(vod_cam)
                } else if first == "export" {
                    let after_clip = after_first.strip_prefix("clip/").unwrap_or(after_first);
                    if after_clip.contains("/start/") {
                        after_clip.split('/').next()
                    } else {
                        None
                    }
                } else {
                    Some(first)
                }
            } else {
                None
            };
            if let Some(cam) = target_cam
                && !cam.is_empty()
                && !allowed_set.iter().any(|a| a == cam || a == "*")
            {
                state.log_hub.push_nginx(&method, &path, 403);
                return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "message": format!("Camera '{cam}' is not permitted for this user") })),
                    )
                        .into_response();
            }
        }
    }

    let mut active_clip_policy: Option<ninjanvr_vision::PrivacyMaskPolicy> = None;
    let any_invalid_policy = active_policies.values().any(|p| p.is_invalid());
    if !active_policies.is_empty()
        && (path == "/recordings"
            || path.starts_with("/recordings/")
            || path == "/exports"
            || path.starts_with("/exports/")
            || path == "/clips"
            || path.starts_with("/clips/"))
    {
        let is_non_share_clip_export = (path == "/exports" || path.starts_with("/exports/"))
            && !path.starts_with("/exports/share_clip_");
        match resolve_static_media_camera_async_with_keys(&state, &path, &camera_keys).await {
            Some(cam) => {
                let cam_trimmed = cam.trim();
                if let Some(policy) = active_policies
                    .get(&cam)
                    .or_else(|| active_policies.get(cam_trimmed))
                {
                    if is_non_share_clip_export {
                        if policy.is_invalid() {
                            state.log_hub.push_nginx(&method, &path, 404);
                            return StatusCode::NOT_FOUND.into_response();
                        }
                    } else {
                        let path_lc = path.to_ascii_lowercase();
                        let is_clip_image = path.starts_with("/clips/")
                            && !path.starts_with("/clips/faces/")
                            && (path_lc.ends_with(".jpg")
                                || path_lc.ends_with(".jpeg")
                                || path_lc.ends_with(".webp")
                                || path_lc.ends_with(".png"));
                        if !is_clip_image || policy.is_invalid() {
                            state.log_hub.push_nginx(&method, &path, 404);
                            return StatusCode::NOT_FOUND.into_response();
                        }
                        active_clip_policy = Some(policy.clone());
                    }
                }
            }
            None => {
                if !is_non_share_clip_export || any_invalid_policy {
                    state.log_hub.push_nginx(&method, &path, 404);
                    return StatusCode::NOT_FOUND.into_response();
                }
            }
        }
    }

    if active_clip_policy.is_some() {
        req.headers_mut().remove(header::RANGE);
        req.headers_mut().remove(header::IF_RANGE);
        req.headers_mut().remove(header::IF_NONE_MATCH);
        req.headers_mut().remove(header::IF_MODIFIED_SINCE);
    }

    let mut resp = next.run(req).await;
    if let Some(policy) = active_clip_policy {
        if method == "HEAD" {
            resp.headers_mut().remove(header::ETAG);
            resp.headers_mut().remove(header::LAST_MODIFIED);
        } else if resp.status() == StatusCode::OK || resp.status() == StatusCode::PARTIAL_CONTENT {
            let prefer_jpeg = {
                let p_lc = path.to_ascii_lowercase();
                p_lc.ends_with(".jpg") || p_lc.ends_with(".jpeg")
            };
            if let Ok(raw_bytes) = axum::body::to_bytes(resp.into_body(), 32 * 1024 * 1024).await {
                let (redacted_bytes, mime) =
                    crate::media_redaction::MediaDeliveryGate::redact_image_bytes_with_policy_async(
                        raw_bytes.to_vec(),
                        policy,
                        prefer_jpeg,
                    )
                    .await;
                resp = ([(header::CONTENT_TYPE, mime)], redacted_bytes).into_response();
            } else {
                resp = StatusCode::NOT_FOUND.into_response();
            }
        }
    }
    if !path.starts_with("/api/logs") && !path.starts_with("/assets/") {
        state
            .log_hub
            .push_nginx(&method, &path, resp.status().as_u16());
    }
    let resp_headers = resp.headers_mut();
    if !resp_headers.contains_key(header::X_CONTENT_TYPE_OPTIONS) {
        resp_headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        );
    }
    if !resp_headers.contains_key(header::X_FRAME_OPTIONS)
        && !path_matches_segment_prefix(&path, "/api/camera-proxy")
    {
        resp_headers.insert(
            header::X_FRAME_OPTIONS,
            axum::http::HeaderValue::from_static("DENY"),
        );
    }
    if !resp_headers.contains_key(header::REFERRER_POLICY) {
        resp_headers.insert(
            header::REFERRER_POLICY,
            axum::http::HeaderValue::from_static("strict-origin-when-cross-origin"),
        );
    }
    let corp_name = header::HeaderName::from_static("cross-origin-resource-policy");
    if !resp_headers.contains_key(&corp_name) {
        resp_headers.insert(
            corp_name,
            axum::http::HeaderValue::from_static("same-origin"),
        );
    }
    resp
}

// ============================================================================
// Part 3 — Auth Providers, Trusted Proxy CIDR, OIDC SSO & Login/Logout
// ============================================================================

pub async fn api_auth_verify(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    let auth_cfg = state.config.read().await.auth.clone();
    warm_auth_cache_for_request(&state, &jar, &headers, &auth_cfg).await;
    if let Some((user, role)) =
        extract_authenticated_user_with_cfg(&state, &jar, &headers, &auth_cfg)
    {
        if role.eq_ignore_ascii_case("metrics") {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "message": "Token scope 'metrics' only permits GET /api/stats and /api/metrics"
                })),
            )
                .into_response();
        }
        let safe_user: String = user.chars().filter(|c| !c.is_control()).collect();
        let safe_role: String = role.chars().filter(|c| !c.is_control()).collect();
        (
            StatusCode::ACCEPTED,
            [("remote-user", safe_user), ("remote-role", safe_role)],
            "",
        )
            .into_response()
    } else {
        (StatusCode::UNAUTHORIZED, [(header::LOCATION, "/login")], "").into_response()
    }
}

pub async fn api_first_time_login(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await;
    Json(json!({
        "admin_first_time_login": cfg.auth.admin_first_time_login
    }))
}

pub async fn api_auth_providers(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    let authenticated = authenticate_read(&state, &jar, &headers).await.is_ok();
    let cfg = state.config.read().await;
    let oidc = &cfg.auth.oidc;
    let issuer = if !oidc.issuer_url.trim().is_empty() {
        oidc.issuer_url.trim().to_string()
    } else {
        std::env::var("NINJANVR_OIDC_ISSUER_URL").unwrap_or_default()
    };
    let client_id = if !oidc.client_id.trim().is_empty() {
        oidc.client_id.trim().to_string()
    } else {
        std::env::var("NINJANVR_OIDC_CLIENT_ID").unwrap_or_default()
    };
    let oidc_ready = oidc.enabled && !issuer.is_empty() && !client_id.is_empty();
    Json(json!({
        "enabled": cfg.auth.enabled,
        "authenticated": authenticated,
        "admin_first_time_login": cfg.auth.admin_first_time_login,
        "initial_password_file": cfg.auth.admin_first_time_login.then_some("initial_admin_password"),
        "local_login_enabled": cfg.auth.local_login_enabled,
        "oidc": {
            "enabled": oidc_ready,
            "provider_name": oidc.provider_name,
            "auto_redirect": oidc.auto_redirect
        },
        "proxy": {
            "enabled": cfg.auth.proxy.enabled,
            "logout_url": scrub_rtsp_credentials(&cfg.auth.proxy.logout_url)
        }
    }))
}

/// Checks whether `ip_str` matches a single CIDR (`"10.42.0.0/16"`, `"127.0.0.1/32"`) or bare IP (`"127.0.0.1"`).
pub fn ip_matches_cidr(ip_str: &str, cidr_str: &str) -> bool {
    use std::net::IpAddr;
    let Ok(raw_ip) = ip_str.trim().parse::<IpAddr>() else {
        return false;
    };
    let ip = raw_ip.to_canonical();
    let cidr = cidr_str.trim();
    if let Some((net_s, prefix_s)) = cidr.split_once('/') {
        let Ok(raw_net_ip) = net_s.trim().parse::<IpAddr>() else {
            return false;
        };
        let Ok(prefix_len) = prefix_s.trim().parse::<u32>() else {
            return false;
        };
        match (raw_ip, raw_net_ip) {
            (IpAddr::V6(a), IpAddr::V6(b)) if prefix_len <= 128 => {
                if prefix_len == 0 {
                    return true;
                }
                let mask = !0u128 << (128 - prefix_len);
                if (u128::from(a) & mask) == (u128::from(b) & mask) {
                    return true;
                }
            }
            _ => {}
        }
        let net_ip = raw_net_ip.to_canonical();
        match (ip, net_ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) if prefix_len <= 32 => {
                if prefix_len == 0 {
                    return true;
                }
                let mask = !0u32 << (32 - prefix_len);
                (u32::from(a) & mask) == (u32::from(b) & mask)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) if prefix_len <= 128 => {
                if prefix_len == 0 {
                    return true;
                }
                let mask = !0u128 << (128 - prefix_len);
                (u128::from(a) & mask) == (u128::from(b) & mask)
            }
            _ => false,
        }
    } else if let Ok(raw_net_ip) = cidr.parse::<IpAddr>() {
        raw_ip == raw_net_ip || ip == raw_net_ip.to_canonical()
    } else {
        false
    }
}

pub fn ip_matches_trusted_proxies(ip_str: &str, cidrs: &[String]) -> bool {
    cidrs.iter().any(|c| ip_matches_cidr(ip_str, c))
}

pub fn is_valid_oidc_discovered_endpoint(endpoint: &str, issuer: &str) -> bool {
    let ep = endpoint.trim();
    if ep.is_empty()
        || ep.contains(['@', '\\', '#'])
        || ep.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return false;
    }
    let Some((ep_scheme, ep_host)) = extract_url_scheme_and_host(ep) else {
        return false;
    };
    let Some((iss_scheme, iss_host)) = extract_url_scheme_and_host(issuer) else {
        return false;
    };
    ep_scheme.eq_ignore_ascii_case(iss_scheme)
        && strip_default_port_for_scheme(ep_scheme, ep_host)
            .eq_ignore_ascii_case(strip_default_port_for_scheme(iss_scheme, iss_host))
}

pub async fn read_upstream_body_bounded(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> std::result::Result<Vec<u8>, StatusCode> {
    if let Some(len) = resp.content_length()
        && len > max_bytes as u64
    {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| StatusCode::BAD_GATEWAY)? {
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            return Err(StatusCode::BAD_GATEWAY);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Resolves a NinjaNVR role (`"admin"`, `"operator"`, or `"viewer"`) from OIDC ID-token / UserInfo claims,
/// supporting both Authelia array claims (`"groups": ["admins"]`) and Zitadel project role map claims
/// (`"urn:zitadel:iam:org:project:roles": { "ninjanvr-admin": { ... } }`).
pub fn resolve_oidc_role(claims: &Value, oidc_cfg: &ninjanvr_config::OidcAuthConfig) -> String {
    let mut candidate_groups: Vec<String> = Vec::new();
    let mut collect_from_val = |val: &Value| match val {
        Value::Array(arr) => {
            for item in arr {
                if let Some(s) = item.as_str() {
                    candidate_groups.push(s.trim().to_string());
                }
            }
        }
        Value::String(s) => {
            for part in s.split([',', ' ']) {
                let t = part.trim();
                if !t.is_empty() {
                    candidate_groups.push(t.to_string());
                }
            }
        }
        Value::Object(map) => {
            for key in map.keys() {
                candidate_groups.push(key.trim().to_string());
            }
        }
        _ => {}
    };

    if let Some(v) = claims.get(&oidc_cfg.groups_claim) {
        collect_from_val(v);
    }
    if oidc_cfg.groups_claim != "groups"
        && let Some(v) = claims.get("groups")
    {
        collect_from_val(v);
    }
    if let Some(v) = claims.get("urn:zitadel:iam:org:project:roles") {
        collect_from_val(v);
    }
    if let Some(v) = claims.get("roles") {
        collect_from_val(v);
    }

    let matches_any = |targets: &[String]| -> bool {
        targets.iter().any(|t| {
            candidate_groups
                .iter()
                .any(|cg| cg.eq_ignore_ascii_case(t.trim()))
        })
    };

    if matches_any(&oidc_cfg.role_mapping.admin) {
        "admin".to_string()
    } else if matches_any(&oidc_cfg.role_mapping.operator) {
        "operator".to_string()
    } else if matches_any(&oidc_cfg.role_mapping.viewer) {
        "viewer".to_string()
    } else {
        let def = oidc_cfg.default_role.trim().to_ascii_lowercase();
        if def == "admin" || def == "operator" {
            def
        } else {
            "viewer".to_string()
        }
    }
}

fn role_privilege_rank(role: &str) -> u8 {
    match role.trim().to_ascii_lowercase().as_str() {
        "admin" => 2,
        "operator" => 1,
        _ => 0,
    }
}

pub fn cap_external_role_by_db(external_role: &str, db_role: &str) -> String {
    if role_privilege_rank(db_role) < role_privilege_rank(external_role) {
        db_role.trim().to_ascii_lowercase()
    } else {
        external_role.trim().to_ascii_lowercase()
    }
}

pub fn is_reserved_token_principal_prefix(username: &str) -> bool {
    let trimmed = username.trim();
    trimmed
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("token:"))
        || trimmed
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("ext:"))
}

pub fn is_allowed_oidc_client_secret_env(name: &str) -> bool {
    let trimmed = name.trim();
    ninjanvr_config::is_safe_secret_env_var_name(trimmed)
        && (trimmed.starts_with("NINJANVR_OIDC_") || trimmed.starts_with("OIDC_"))
}

pub fn sign_oidc_pkce_cookie_payload(jwt_secret: &str, payload: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(jwt_secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(b"ninjanvr-oidc-pkce-v1:");
    mac.update(payload.as_bytes());
    let sig_hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("v1.{sig_hex}:{payload}")
}

pub fn format_signed_oidc_pkce_cookie(
    jwt_secret: &str,
    state_str: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> String {
    let payload = format!("{state_str}:{code_verifier}:{redirect_uri}");
    sign_oidc_pkce_cookie_payload(jwt_secret, &payload)
}

pub fn verify_signed_oidc_pkce_cookie<'a>(
    jwt_secret: &str,
    raw_cookie_val: &'a str,
) -> Option<&'a str> {
    let rest = raw_cookie_val.strip_prefix("v1.")?;
    let (sig_hex, payload) = rest.split_once(':')?;
    if sig_hex.len() != 64 || !sig_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut sig_bytes = [0u8; 32];
    for (i, chunk) in sig_hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        sig_bytes[i] = ((hi << 4) | lo) as u8;
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(jwt_secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(b"ninjanvr-oidc-pkce-v1:");
    mac.update(payload.as_bytes());
    mac.verify_slice(&sig_bytes).ok()?;
    Some(payload)
}

/// Determines whether session/OIDC cookies must carry the `Secure` attribute:
/// enabled whenever `auth_cfg.cookie_secure` is `true`, the request arrived over the `:8971` TLS listener,
/// the URI scheme is `https`, or a trusted proxy supplied `X-Forwarded-Proto: https`.
pub fn should_set_secure_cookie(
    headers: &HeaderMap,
    uri: &Uri,
    auth_cfg: &ninjanvr_config::AuthConfig,
) -> bool {
    if auth_cfg.cookie_secure
        || headers.contains_key("x-ninjanvr-https-listener")
        || uri
            .scheme_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("https"))
    {
        return true;
    }
    let is_xfp_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    if !is_xfp_https {
        return false;
    }
    let Some(peer_ip) = headers
        .get("x-peer-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return true;
    };
    let default_loopback = ["127.0.0.1/32".to_string(), "::1/128".to_string()];
    let trusted_cidrs: &[String] = if !auth_cfg.proxy.trusted_proxies.is_empty() {
        &auth_cfg.proxy.trusted_proxies
    } else if !auth_cfg.trusted_proxies.is_empty() {
        &auth_cfg.trusted_proxies
    } else {
        &default_loopback
    };
    ip_matches_trusted_proxies(peer_ip, trusted_cidrs)
}

pub fn extract_login_peer_scope(headers: &HeaderMap) -> String {
    headers
        .get("x-peer-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        .map_or_else(|| "local".to_string(), |ip| ip.to_canonical().to_string())
}

pub fn generate_pkce_pair() -> (String, String, String) {
    let mut verifier_bytes = [0u8; 32];
    rand::fill(&mut verifier_bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge_hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge_hash);
    let mut state_bytes = [0u8; 16];
    rand::fill(&mut state_bytes);
    let state_str: String = state_bytes.iter().map(|b| format!("{b:02x}")).collect();
    (state_str, verifier, challenge)
}

pub fn resolve_external_base_url(headers: &HeaderMap, uri: &Uri) -> String {
    resolve_external_base_url_with_auth(headers, uri, None)
}

pub fn resolve_external_base_url_with_auth(
    headers: &HeaderMap,
    uri: &Uri,
    auth_cfg: Option<&ninjanvr_config::AuthConfig>,
) -> String {
    let default_loopback = ["127.0.0.1/32".to_string(), "::1/128".to_string()];
    let peer_is_trusted_proxy = headers
        .get("x-peer-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none_or(|peer_ip| {
            let trusted_cidrs: &[String] = match auth_cfg {
                Some(cfg) if !cfg.proxy.trusted_proxies.is_empty() => &cfg.proxy.trusted_proxies,
                Some(cfg) if !cfg.trusted_proxies.is_empty() => &cfg.trusted_proxies,
                _ => &default_loopback,
            };
            ip_matches_trusted_proxies(peer_ip, trusted_cidrs)
        });

    let x_forwarded_host = peer_is_trusted_proxy
        .then(|| {
            headers
                .get("x-forwarded-host")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(',').next().unwrap_or(s).trim())
                .filter(|s| !s.is_empty())
        })
        .flatten();
    let host_header = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty());
    let uri_authority = uri
        .authority()
        .map(|a| a.as_str().trim())
        .filter(|s| !s.is_empty());

    let primary_host = host_header.or(uri_authority);
    let allow_xfh = match (x_forwarded_host, primary_host) {
        (Some(_), None) => true,
        (Some(xfh), Some(pri)) => {
            is_internal_backend_host(pri)
                || bare_hostname(xfh).eq_ignore_ascii_case(bare_hostname(pri))
        }
        (None, _) => false,
    };

    let raw_host = if allow_xfh {
        x_forwarded_host.or(primary_host)
    } else {
        primary_host
    }
    .unwrap_or("localhost:5000");

    let is_valid_host = !raw_host.is_empty()
        && !raw_host.contains(['/', '\\', '?', '#', '@'])
        && !raw_host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control());
    let host = if is_valid_host {
        raw_host
    } else {
        "localhost:5000"
    };

    let default_proto = if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        "http"
    } else {
        "https"
    };
    let forwarded_proto = peer_is_trusted_proxy
        .then(|| {
            headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(',').next().unwrap_or(s).trim())
                .filter(|s| !s.is_empty())
        })
        .flatten();
    let raw_proto = forwarded_proto
        .or_else(|| uri.scheme_str())
        .unwrap_or(default_proto);
    let proto = if raw_proto.eq_ignore_ascii_case("http") {
        "http"
    } else if raw_proto.eq_ignore_ascii_case("https") {
        "https"
    } else {
        default_proto
    };

    format!("{proto}://{host}")
}

pub async fn api_auth_oidc_login(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
) -> Response {
    let cfg = state.config.read().await;
    let oidc = &cfg.auth.oidc;
    let cookie_secure = should_set_secure_cookie(&headers, &uri, &cfg.auth);
    let issuer = if !oidc.issuer_url.trim().is_empty() {
        oidc.issuer_url.trim().trim_end_matches('/').to_string()
    } else {
        std::env::var("NINJANVR_OIDC_ISSUER_URL")
            .unwrap_or_default()
            .trim()
            .trim_end_matches('/')
            .to_string()
    };
    let client_id = if !oidc.client_id.trim().is_empty() {
        oidc.client_id.trim().to_string()
    } else {
        std::env::var("NINJANVR_OIDC_CLIENT_ID").unwrap_or_default()
    };
    if !oidc.enabled || issuer.is_empty() || client_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "OIDC SSO is not enabled or missing issuer_url/client_id" })),
        )
            .into_response();
    }

    let peer_scope = extract_login_peer_scope(&headers);
    if !state
        .auth_gate
        .check_and_record_oidc_login_rate(&peer_scope)
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "message": "Too many OIDC login requests; try again shortly" })),
        )
            .into_response();
    }

    let (state_str, verifier, challenge) = generate_pkce_pair();
    let base_url = resolve_external_base_url_with_auth(&headers, &uri, Some(&cfg.auth));
    let redirect_uri = format!("{base_url}/api/auth/oidc/callback");
    drop(cfg);

    state.auth_gate.store_oidc_pending_state(
        state_str.clone(),
        verifier.clone(),
        redirect_uri.clone(),
    );

    // Discover authorization_endpoint (with 300s TTL cache) or fall back to standard /authorize
    let mut auth_endpoint = format!("{issuer}/authorize");
    let cached_disc = state.auth_gate.get_cached_oidc_discovery(&issuer);
    let disc_opt = if let Some(doc) = cached_disc {
        Some(doc)
    } else {
        let _ = rustls::crypto::ring::default_provider().install_default();
        if let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            && let Ok(resp) = client
                .get(format!("{issuer}/.well-known/openid-configuration"))
                .send()
                .await
            && resp.status().is_success()
            && let Ok(disc_bytes) = read_upstream_body_bounded(resp, 256 * 1024).await
            && let Ok(disc) = serde_json::from_slice::<Value>(&disc_bytes)
            && disc
                .get("issuer")
                .and_then(|v| v.as_str())
                .is_none_or(|s| s.trim().trim_end_matches('/') == issuer.as_str())
        {
            state
                .auth_gate
                .put_cached_oidc_discovery(&issuer, disc.clone());
            Some(disc)
        } else {
            None
        }
    };
    if let Some(disc) = disc_opt
        && let Some(ep) = disc.get("authorization_endpoint").and_then(|v| v.as_str())
        && is_valid_oidc_discovered_endpoint(ep, &issuer)
    {
        auth_endpoint = ep.trim().to_string();
    }

    let scopes = state.config.read().await.auth.oidc.scopes.clone();
    let scope_str = if scopes.is_empty() {
        "openid profile email groups".to_string()
    } else {
        scopes.join(" ")
    };

    let redirect_target = format!(
        "{auth_endpoint}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={state_str}&code_challenge={challenge}&code_challenge_method=S256",
        urlencoding_simple(&client_id),
        urlencoding_simple(&redirect_uri),
        urlencoding_simple(&scope_str),
    );

    let signed_pkce_val =
        format_signed_oidc_pkce_cookie(&state.jwt_secret, &state_str, &verifier, &redirect_uri);
    let mut pkce_cookie = Cookie::new("ninjanvr_oidc_pkce", signed_pkce_val);
    pkce_cookie.set_path("/");
    pkce_cookie.set_http_only(true);
    pkce_cookie.set_secure(cookie_secure);
    pkce_cookie.set_same_site(SameSite::Lax);

    (jar.add(pkce_cookie), Redirect::to(&redirect_target)).into_response()
}

pub fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Debug, Deserialize, Default)]
pub struct OidcCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
}

pub async fn api_auth_oidc_callback(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
    Query(q): Query<OidcCallbackQuery>,
) -> Response {
    let Some(code) = q.code.as_deref().filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Missing authorization code" })),
        )
            .into_response();
    };
    let Some(returned_state) = q.state.as_deref().filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Missing OIDC state parameter" })),
        )
            .into_response();
    };
    let pending_from_gate = state.auth_gate.take_oidc_pending_state(returned_state);
    let Some(pkce_cookie) = jar.get("ninjanvr_oidc_pkce") else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Missing OIDC PKCE state cookie" })),
        )
            .into_response();
    };
    let raw_pkce_val = pkce_cookie.value();
    let verified_payload = if raw_pkce_val.starts_with("v1.") {
        let Some(p) = verify_signed_oidc_pkce_cookie(&state.jwt_secret, raw_pkce_val) else {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Invalid OIDC PKCE cookie signature" })),
            )
                .into_response();
        };
        p
    } else if pending_from_gate.is_some() {
        raw_pkce_val
    } else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Unsigned OIDC PKCE cookie rejected" })),
        )
            .into_response();
    };
    let Some((expected_state, rest_cookie)) = verified_payload.split_once(':') else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Malformed OIDC PKCE cookie" })),
        )
            .into_response();
    };
    let (code_verifier, saved_redirect_uri) = match rest_cookie.split_once(':') {
        Some((v, r)) => (v, Some(r.to_string())),
        None => (rest_cookie, None),
    };
    if expected_state.is_empty() || code_verifier.is_empty() || expected_state != returned_state {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "OIDC state mismatch" })),
        )
            .into_response();
    }
    if let Some(ref pending) = pending_from_gate
        && (pending.code_verifier != code_verifier
            || saved_redirect_uri
                .as_deref()
                .is_some_and(|r| !r.eq_ignore_ascii_case(&pending.redirect_uri)))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "OIDC state mismatch" })),
        )
            .into_response();
    }

    let (auth_full_cfg, oidc_cfg, cookie_name, cookie_secure, session_len) = {
        let cfg = state.config.read().await;
        (
            cfg.auth.clone(),
            cfg.auth.oidc.clone(),
            cfg.auth.cookie_name.clone(),
            should_set_secure_cookie(&headers, &uri, &cfg.auth),
            cfg.auth.session_length,
        )
    };

    let issuer = if !oidc_cfg.issuer_url.trim().is_empty() {
        oidc_cfg.issuer_url.trim().trim_end_matches('/').to_string()
    } else {
        std::env::var("NINJANVR_OIDC_ISSUER_URL")
            .unwrap_or_default()
            .trim()
            .trim_end_matches('/')
            .to_string()
    };
    let client_id = if !oidc_cfg.client_id.trim().is_empty() {
        oidc_cfg.client_id.trim().to_string()
    } else {
        std::env::var("NINJANVR_OIDC_CLIENT_ID").unwrap_or_default()
    };
    if !oidc_cfg.enabled || issuer.is_empty() || client_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "OIDC SSO is not enabled or missing issuer_url/client_id" })),
        )
            .into_response();
    }
    let secret_env_trimmed = oidc_cfg.client_secret_env.trim();
    if !secret_env_trimmed.is_empty() && !is_allowed_oidc_client_secret_env(secret_env_trimmed) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "message": "Disallowed OIDC client_secret_env variable name" })),
        )
            .into_response();
    }
    let client_secret = if secret_env_trimmed.is_empty() {
        String::new()
    } else {
        std::env::var(secret_env_trimmed).unwrap_or_default()
    };

    let expected_callback_uri = {
        let base_url = resolve_external_base_url_with_auth(&headers, &uri, Some(&auth_full_cfg));
        format!("{base_url}/api/auth/oidc/callback")
    };
    let redirect_uri = match saved_redirect_uri {
        Some(ref r) => {
            if !(r.starts_with("http://") || r.starts_with("https://"))
                || !r.ends_with("/api/auth/oidc/callback")
                || r.contains("..")
                || r.contains(['?', '#', '@', '\\'])
                || r.chars().any(|c| c.is_whitespace() || c.is_control())
                || !r.eq_ignore_ascii_case(&expected_callback_uri)
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "message": "OIDC PKCE redirect_uri mismatch" })),
                )
                    .into_response();
            }
            r.clone()
        }
        None => expected_callback_uri,
    };

    let mut token_endpoint = format!("{issuer}/oauth/v2/token");
    let mut userinfo_endpoint = format!("{issuer}/oidc/v1/userinfo");
    let mut jwks_endpoint: Option<String> = None;
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        if let Ok(resp) = client
            .get(format!("{issuer}/.well-known/openid-configuration"))
            .send()
            .await
        {
            if resp.status() != StatusCode::NOT_FOUND && !resp.status().is_success() {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "message": "OIDC discovery endpoint returned error status" })),
                )
                    .into_response();
            }
            if resp.status().is_success() {
                let Ok(disc_bytes) = read_upstream_body_bounded(resp, 256 * 1024).await else {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "message": "Invalid OIDC discovery payload" })),
                    )
                        .into_response();
                };
                let Ok(disc) = serde_json::from_slice::<Value>(&disc_bytes) else {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "message": "Malformed OIDC discovery JSON" })),
                    )
                        .into_response();
                };
                if let Some(disc_iss) = disc.get("issuer")
                    && !disc_iss
                        .as_str()
                        .is_some_and(|s| s.trim().trim_end_matches('/') == issuer.as_str())
                {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "message": "OIDC discovery issuer mismatch" })),
                    )
                        .into_response();
                }
                if let Some(ep_val) = disc.get("token_endpoint") {
                    let Some(ep) = ep_val
                        .as_str()
                        .filter(|ep| is_valid_oidc_discovered_endpoint(ep, &issuer))
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Invalid or disallowed OIDC token_endpoint" })),
                        )
                            .into_response();
                    };
                    token_endpoint = ep.trim().to_string();
                }
                if let Some(uep_val) = disc.get("userinfo_endpoint") {
                    let Some(uep) = uep_val
                        .as_str()
                        .filter(|uep| is_valid_oidc_discovered_endpoint(uep, &issuer))
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Invalid or disallowed OIDC userinfo_endpoint" })),
                        )
                            .into_response();
                    };
                    userinfo_endpoint = uep.trim().to_string();
                }
                if let Some(jep_val) = disc.get("jwks_uri") {
                    let Some(jep) = jep_val
                        .as_str()
                        .filter(|jep| is_valid_oidc_discovered_endpoint(jep, &issuer))
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Invalid or disallowed OIDC jwks_uri" })),
                        )
                            .into_response();
                    };
                    jwks_endpoint = Some(jep.trim().to_string());
                }
            }
        }

        let mut form_body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            urlencoding_simple(code),
            urlencoding_simple(&redirect_uri),
            urlencoding_simple(&client_id),
            urlencoding_simple(code_verifier)
        );
        if !client_secret.is_empty() {
            form_body.push_str(&format!(
                "&client_secret={}",
                urlencoding_simple(&client_secret)
            ));
        }
        if let Ok(tok_resp) = client
            .post(&token_endpoint)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form_body)
            .send()
            .await
        {
            if !tok_resp.status().is_success() {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "message": "OIDC token endpoint rejected authorization code" })),
                )
                    .into_response();
            }
            if let Ok(tok_bytes) = read_upstream_body_bounded(tok_resp, 256 * 1024).await
                && let Ok(tok_json) = serde_json::from_slice::<Value>(&tok_bytes)
            {
                if tok_json.get("error").is_some() {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "message": "OIDC token response contained an error" })),
                    )
                        .into_response();
                }
                let Some(id_token) = tok_json
                    .get("id_token")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                else {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "message": "OIDC token response missing required id_token" })),
                    )
                        .into_response();
                };
                let mut merged_claims = {
                    let parts: Vec<&str> = id_token.split('.').collect();
                    if parts.len() != 3
                        || parts[2].trim().is_empty()
                        || base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(parts[2].trim())
                            .is_err()
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Malformed or unsigned OIDC ID token" })),
                        )
                            .into_response();
                    }
                    let Ok(hdr_bytes) =
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[0])
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Malformed OIDC ID token header" })),
                        )
                            .into_response();
                    };
                    let Ok(hdr_json) = serde_json::from_slice::<Value>(&hdr_bytes) else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Malformed OIDC ID token header JSON" })),
                        )
                            .into_response();
                    };
                    let Some(alg_str) = hdr_json
                        .get("alg")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|alg| !alg.is_empty() && !alg.eq_ignore_ascii_case("none"))
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Malformed or unsigned OIDC ID token" })),
                        )
                            .into_response();
                    };
                    let Ok(jwt_hdr) = jsonwebtoken::decode_header(id_token) else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Unsupported OIDC ID token algorithm" })),
                        )
                            .into_response();
                    };
                    if let Some(ref jwks_url) = jwks_endpoint {
                        let Ok(jwks_resp) = client.get(jwks_url).send().await else {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "message": "Failed to fetch OIDC JWKS" })),
                            )
                                .into_response();
                        };
                        if !jwks_resp.status().is_success() {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "message": "OIDC JWKS endpoint returned error status" })),
                            )
                                .into_response();
                        }
                        let Ok(jwks_bytes) =
                            read_upstream_body_bounded(jwks_resp, 256 * 1024).await
                        else {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "message": "Invalid OIDC JWKS payload" })),
                            )
                                .into_response();
                        };
                        let Ok(jwk_set) =
                            serde_json::from_slice::<jsonwebtoken::jwk::JwkSet>(&jwks_bytes)
                        else {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "message": "Malformed OIDC JWKS JSON" })),
                            )
                                .into_response();
                        };
                        let mut sig_verified = false;
                        for jwk in &jwk_set.keys {
                            if matches!(
                                jwk.common.public_key_use,
                                Some(jsonwebtoken::jwk::PublicKeyUse::Encryption)
                            ) {
                                continue;
                            }
                            if let Some(ref req_kid) = jwt_hdr.kid
                                && let Some(ref key_kid) = jwk.common.key_id
                                && req_kid != key_kid
                            {
                                continue;
                            }
                            let kty_matches_alg = match &jwk.algorithm {
                                jsonwebtoken::jwk::AlgorithmParameters::RSA(_) => matches!(
                                    jwt_hdr.alg,
                                    Algorithm::RS256
                                        | Algorithm::RS384
                                        | Algorithm::RS512
                                        | Algorithm::PS256
                                        | Algorithm::PS384
                                        | Algorithm::PS512
                                ),
                                jsonwebtoken::jwk::AlgorithmParameters::EllipticCurve(_) => {
                                    matches!(jwt_hdr.alg, Algorithm::ES256 | Algorithm::ES384)
                                }
                                jsonwebtoken::jwk::AlgorithmParameters::OctetKeyPair(_) => {
                                    matches!(jwt_hdr.alg, Algorithm::EdDSA)
                                }
                                jsonwebtoken::jwk::AlgorithmParameters::OctetKey(_) => matches!(
                                    jwt_hdr.alg,
                                    Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
                                ),
                                _ => false,
                            };
                            if !kty_matches_alg {
                                continue;
                            }
                            if let Some(key_alg) = jwk.common.key_algorithm
                                && format!("{key_alg:?}") != format!("{:?}", jwt_hdr.alg)
                            {
                                continue;
                            }
                            if let Ok(dec_key) = DecodingKey::from_jwk(jwk) {
                                let mut val = Validation::new(jwt_hdr.alg);
                                val.validate_exp = false;
                                val.validate_aud = false;
                                val.required_spec_claims.clear();
                                if jsonwebtoken::decode::<Value>(id_token, &dec_key, &val).is_ok() {
                                    sig_verified = true;
                                    break;
                                }
                            }
                        }
                        if !sig_verified {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "message": "OIDC ID token JWKS signature verification failed" })),
                            )
                                .into_response();
                        }
                    } else if matches!(alg_str, "HS256" | "HS384" | "HS512") {
                        if client_secret.is_empty() {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "message": "HMAC OIDC ID token requires client_secret" })),
                            )
                                .into_response();
                        }
                        let hmac_alg = match alg_str {
                            "HS384" => Algorithm::HS384,
                            "HS512" => Algorithm::HS512,
                            _ => Algorithm::HS256,
                        };
                        let mut val = Validation::new(hmac_alg);
                        val.validate_exp = false;
                        val.validate_aud = false;
                        val.required_spec_claims.clear();
                        if jsonwebtoken::decode::<Value>(
                            id_token,
                            &DecodingKey::from_secret(client_secret.as_bytes()),
                            &val,
                        )
                        .is_err()
                        {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({ "message": "OIDC ID token HMAC signature verification failed" })),
                            )
                                .into_response();
                        }
                    } else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC discovery document missing required jwks_uri for asymmetric ID token" })),
                        )
                            .into_response();
                    }
                    let Ok(payload_bytes) =
                        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1])
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Malformed OIDC ID token payload" })),
                        )
                            .into_response();
                    };
                    let Ok(claims_json) = serde_json::from_slice::<Value>(&payload_bytes) else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Malformed OIDC ID token claims JSON" })),
                        )
                            .into_response();
                    };
                    let has_sub = claims_json
                        .get("sub")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.trim().is_empty());
                    if !has_sub {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC ID token missing required sub claim" })),
                        )
                            .into_response();
                    }
                    let now_ts = unix_now();
                    let Some(exp) = claims_json.get("exp").and_then(|v| v.as_u64()) else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC ID token missing required exp claim" })),
                        )
                            .into_response();
                    };
                    if exp <= now_ts {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC ID token has expired" })),
                        )
                            .into_response();
                    }
                    if claims_json
                        .get("nbf")
                        .and_then(|v| v.as_u64())
                        .is_some_and(|nbf| nbf > now_ts.saturating_add(60))
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC ID token is not yet valid (nbf)" })),
                        )
                            .into_response();
                    }
                    let iss_ok = claims_json
                        .get("iss")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s.trim().trim_end_matches('/') == issuer.as_str());
                    if !iss_ok {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC ID token issuer mismatch" })),
                        )
                            .into_response();
                    }
                    let aud_ok = match claims_json.get("aud") {
                        Some(Value::String(s)) => s.trim() == client_id.as_str(),
                        Some(Value::Array(arr)) => arr
                            .iter()
                            .any(|v| v.as_str().is_some_and(|s| s.trim() == client_id.as_str())),
                        _ => false,
                    };
                    if !aud_ok {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC ID token audience mismatch" })),
                        )
                            .into_response();
                    }
                    claims_json
                };
                if let Some(access_token) = tok_json.get("access_token").and_then(|v| v.as_str()) {
                    let Ok(ui_resp) = client
                        .get(&userinfo_endpoint)
                        .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
                        .send()
                        .await
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC UserInfo request failed" })),
                        )
                            .into_response();
                    };
                    if !ui_resp.status().is_success() {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC UserInfo endpoint rejected access token" })),
                        )
                            .into_response();
                    }
                    let Ok(ui_bytes) = read_upstream_body_bounded(ui_resp, 256 * 1024).await else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Invalid OIDC UserInfo response payload" })),
                        )
                            .into_response();
                    };
                    let Ok(Value::Object(ui_map)) = serde_json::from_slice::<Value>(&ui_bytes)
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Invalid OIDC UserInfo response payload" })),
                        )
                            .into_response();
                    };
                    if ui_map.contains_key("error") {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC UserInfo response contained an error" })),
                        )
                            .into_response();
                    }
                    let Some(ui_sub) = ui_map
                        .get("sub")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC UserInfo missing required sub claim" })),
                        )
                            .into_response();
                    };
                    if let Some(id_sub) = merged_claims
                        .get("sub")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        && ui_sub != id_sub
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC subject mismatch between ID token and UserInfo" })),
                        )
                            .into_response();
                    }
                    if let Some(obj) = merged_claims.as_object_mut() {
                        for (k, v) in ui_map {
                            if matches!(k.as_str(), "iss" | "aud" | "exp" | "iat" | "nbf" | "sub")
                                && obj.contains_key(&k)
                            {
                                continue;
                            }
                            obj.insert(k, v);
                        }
                    }
                }
                if merged_claims.as_object().is_some_and(|m| !m.is_empty()) {
                    let identity_opt = merged_claims
                        .get(&oidc_cfg.username_claim)
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            merged_claims
                                .get("preferred_username")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                        })
                        .or_else(|| {
                            merged_claims
                                .get("email")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                        })
                        .or_else(|| {
                            merged_claims
                                .get("sub")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                        });
                    let Some(username_str) =
                        identity_opt.filter(|u| !is_reserved_token_principal_prefix(u))
                    else {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "OIDC claims missing valid subject/username identity" })),
                        )
                            .into_response();
                    };
                    let username = username_str.to_string();
                    let oidc_role = resolve_oidc_role(&merged_claims, &oidc_cfg);
                    let (canonical_username, role, allowed_cameras) = match state
                        .auth_gate
                        .get_user_cached_async(&state.db, &username)
                        .await
                    {
                        Ok(Some(db_u)) => (
                            db_u.username,
                            cap_external_role_by_db(&oidc_role, &db_u.role),
                            Some(db_u.allowed_cameras),
                        ),
                        _ => (username, oidc_role, Some("*".to_string())),
                    };
                    let now = unix_now();
                    let clamped_session_len = session_len.clamp(60, 86_400 * 365);
                    let jwt_claims = JwtClaims {
                        sub: canonical_username,
                        role,
                        allowed_cameras,
                        iat: now,
                        exp: now.saturating_add(clamped_session_len),
                    };
                    let mut oidc_hdr = Header::new(Algorithm::HS256);
                    oidc_hdr.kid = Some("oidc".to_string());
                    let session_token = jsonwebtoken::encode(
                        &oidc_hdr,
                        &jwt_claims,
                        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
                    )
                    .unwrap_or_default();
                    let mut session_cookie = Cookie::new(cookie_name, session_token);
                    session_cookie.set_path("/");
                    session_cookie.set_http_only(true);
                    session_cookie.set_secure(cookie_secure);
                    session_cookie.set_same_site(SameSite::Lax);

                    let mut clear_pkce = Cookie::from("ninjanvr_oidc_pkce");
                    clear_pkce.set_path("/");
                    return (
                        jar.remove(clear_pkce).add(session_cookie),
                        Redirect::to("/"),
                    )
                        .into_response();
                } else {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "message": "OIDC token response contained no valid identity claims" })),
                    )
                        .into_response();
                }
            }
        }
    }

    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "message": "OIDC token exchange failed" })),
    )
        .into_response()
}

pub async fn api_login(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<LoginBody>,
) -> impl IntoResponse {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            jar,
            (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "success": false,
                    "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
                })),
            )
                .into_response(),
        );
    }

    let (
        cookie_name,
        cookie_secure,
        session_len,
        auth_enabled,
        local_login_enabled,
        hash_iterations,
    ) = {
        let cfg = state.config.read().await;
        (
            cfg.auth.cookie_name.clone(),
            should_set_secure_cookie(&headers, &uri, &cfg.auth),
            cfg.auth.session_length,
            cfg.auth.enabled,
            cfg.auth.local_login_enabled,
            cfg.auth.hash_iterations,
        )
    };

    if !auth_enabled {
        return (
            jar,
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "message": "Authentication is disabled" })),
            )
                .into_response(),
        );
    }

    if !local_login_enabled {
        return (
            jar,
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Local password login is disabled; use OIDC SSO" })),
            )
                .into_response(),
        );
    }

    let trimmed_user = body.user.trim();
    if trimmed_user.is_empty() || trimmed_user.len() > 128 || body.password.len() > 1024 {
        return (
            jar,
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "message": "Invalid username or password length" })),
            )
                .into_response(),
        );
    }

    let rate_scope = extract_login_peer_scope(&headers);
    if state
        .auth_gate
        .check_login_rate_limited(&rate_scope, trimmed_user)
    {
        return (
            jar,
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "message": "Too many failed login attempts; try again later" })),
            )
                .into_response(),
        );
    }

    let _pbkdf2_permit = match tokio::time::timeout(
        Duration::from_secs(2),
        state.auth_gate.pbkdf2_semaphore.acquire(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            return (
                jar,
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "message": "Authentication worker pool unavailable" })),
                )
                    .into_response(),
            );
        }
        Err(_) => {
            return (
                jar,
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "message": "Authentication busy; try again shortly" })),
                )
                    .into_response(),
            );
        }
    };

    let db_clone = state.db.clone();
    let user_owned = trimmed_user.to_string();
    let password_owned = body.password.clone();
    let auth_res = tokio::task::spawn_blocking(move || match db_clone.get_user(&user_owned) {
        Ok(Some(db_user)) => {
            if verify_password(&password_owned, &db_user.password_hash) {
                Some(db_user)
            } else {
                None
            }
        }
        _ => {
            // Equalize PBKDF2 timing when user does not exist to prevent username enumeration timing oracle
            let dummy_hash = format!(
                "pbkdf2_sha256${}$dummysalt0000000$0000000000000000000000000000000000000000000=",
                hash_iterations.clamp(1_000, 600_000)
            );
            let _ = verify_password(&password_owned, &dummy_hash);
            None
        }
    })
    .await
    .ok()
    .flatten();
    drop(_pbkdf2_permit);

    let Some(db_user) = auth_res else {
        state
            .auth_gate
            .record_login_failure(&rate_scope, trimmed_user);
        return (
            jar,
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "message": "Login failed" })),
            )
                .into_response(),
        );
    };

    state
        .auth_gate
        .clear_login_failures(&rate_scope, trimmed_user);
    state.auth_gate.put_cached_user(
        state.db.path(),
        &db_user.username,
        CachedDbUser::from(&db_user),
    );

    if db_user.role == "admin" {
        state.config.write().await.auth.admin_first_time_login = false;
    }

    let now = unix_now();
    let issued_at = match state.auth_gate.deleted_user_epoch(&db_user.username) {
        Some(prev_deleted) => now.max(prev_deleted.saturating_add(1)),
        None => now,
    };
    let clamped_session_len = session_len.clamp(60, 86_400 * 365);
    let claims = JwtClaims {
        sub: db_user.username.clone(),
        role: db_user.role.clone(),
        allowed_cameras: Some(db_user.allowed_cameras.clone()),
        iat: issued_at,
        exp: issued_at.saturating_add(clamped_session_len),
    };

    let mut local_hdr = Header::new(Algorithm::HS256);
    local_hdr.kid = Some("local".to_string());
    let token = jsonwebtoken::encode(
        &local_hdr,
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .unwrap_or_default();

    let mut cookie = Cookie::new(cookie_name, token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_secure(cookie_secure);
    cookie.set_same_site(SameSite::Lax);

    (jar.add(cookie), StatusCode::OK.into_response())
}

pub async fn api_logout(
    State(state): State<AppState>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
) -> Response {
    if method == axum::http::Method::GET || method == axum::http::Method::HEAD {
        return Redirect::to("/login").into_response();
    }
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
    let cookie_name = state.config.read().await.auth.cookie_name.clone();
    let mut removal = Cookie::from(cookie_name);
    removal.set_path("/");
    (jar.remove(removal), Redirect::to("/login")).into_response()
}

// ============================================================================
// Part 4 — Principal Extraction, Camera Scoping, CSRF & WebSocket Filtering
// ============================================================================

pub fn parse_authorization_bearer(raw: &str) -> Option<&str> {
    let trimmed = raw.trim_start();
    if trimmed
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("bearer "))
        && let Some(rest) = trimmed.get(7..)
    {
        let tok = rest.trim();
        if !tok.is_empty() {
            return Some(tok);
        }
    }
    None
}

pub fn is_loopback_ip_str(ip_str: &str) -> bool {
    let trimmed = ip_str.trim();
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback(),
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
        };
    }
    false
}

pub fn extract_trusted_proxy_peer_ip(headers: &HeaderMap) -> Option<&str> {
    if let Some(peer_hdr) = headers.get("x-peer-ip") {
        return peer_hdr
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|s| !s.is_empty());
    }
    let forwarded_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|xff| xff.split(',').next_back())
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    if is_loopback_ip_str(forwarded_ip) {
        return None;
    }
    Some(forwarded_ip)
}

pub fn extract_authenticated_user_with_cfg(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    auth_cfg: &ninjanvr_config::AuthConfig,
) -> Option<(String, String)> {
    if !auth_cfg.enabled {
        return Some(("admin".to_string(), "admin".to_string()));
    }

    // Hardened Trusted-Proxy ForwardAuth: only trust Remote-User / Remote-Groups headers
    // when proxy auth is explicitly enabled, no explicit Authorization header is present,
    // AND the peer IP matches trusted_proxies CIDRs.
    if auth_cfg.proxy.enabled && !headers.contains_key(header::AUTHORIZATION) {
        let peer_ip = extract_trusted_proxy_peer_ip(headers);

        let trusted_cidrs = if !auth_cfg.proxy.trusted_proxies.is_empty() {
            &auth_cfg.proxy.trusted_proxies
        } else {
            &auth_cfg.trusted_proxies
        };

        if let Some(ip) = peer_ip
            && ip_matches_trusted_proxies(ip, trusted_cidrs)
        {
            let user_hdr_trimmed = auth_cfg.proxy.user_header.trim();
            let user_hdr_name = if user_hdr_trimmed.is_empty() {
                "remote-user".to_string()
            } else {
                user_hdr_trimmed.to_ascii_lowercase()
            };
            if let Some(remote_user) = headers
                .get(user_hdr_name.as_str())
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && !is_reserved_token_principal_prefix(s))
            {
                let groups_hdr_trimmed = auth_cfg.proxy.groups_header.trim();
                let groups_hdr_name = if groups_hdr_trimmed.is_empty() {
                    "remote-groups".to_string()
                } else {
                    groups_hdr_trimmed.to_ascii_lowercase()
                };
                let proxy_role = if let Some(groups_str) = headers
                    .get(groups_hdr_name.as_str())
                    .and_then(|v| v.to_str().ok())
                {
                    resolve_oidc_role(&json!({ "groups": groups_str }), &auth_cfg.oidc)
                } else {
                    "viewer".to_string()
                };
                if let Ok(Some(db_u)) = state.auth_gate.get_user_cached_sync(&state.db, remote_user)
                {
                    return Some((
                        db_u.username,
                        cap_external_role_by_db(&proxy_role, &db_u.role),
                    ));
                }
                return Some((remote_user.to_string(), proxy_role));
            }
        }
    }

    let token_str = if let Some(auth_val) = headers.get(header::AUTHORIZATION) {
        let raw = auth_val.to_str().ok()?;
        parse_authorization_bearer(raw)?.to_string()
    } else {
        jar.get(&auth_cfg.cookie_name)
            .map(|c| c.value().to_string())?
    };

    if token_str.starts_with("nnvr_") {
        if let Some((token_name, scope)) =
            state
                .auth_gate
                .verify_api_token_cached_sync(&state.db, &token_str, unix_now() as f64)
        {
            let mapped_role = if scope == "full" {
                "admin".to_string()
            } else {
                scope
            };
            return Some((format!("token:{token_name}"), mapped_role));
        }
        return None;
    }

    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    let decoded = jsonwebtoken::decode::<JwtClaims>(
        &token_str,
        &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
        &validation,
    )
    .ok()?;
    if is_reserved_token_principal_prefix(&decoded.claims.sub)
        || state
            .auth_gate
            .is_token_revoked_by_user_deletion(&decoded.claims.sub, decoded.claims.iat)
    {
        return None;
    }

    let is_local_session_token = decoded
        .header
        .kid
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case("local"));

    let (canonical_sub, role) = match state
        .auth_gate
        .get_user_cached_sync(&state.db, &decoded.claims.sub)
    {
        Ok(Some(db_u)) => {
            if let Some(changed_epoch) = db_u.password_changed_at_epoch
                && (decoded.claims.iat as i64) < changed_epoch
            {
                return None;
            }
            (
                db_u.username,
                cap_external_role_by_db(&decoded.claims.role, &db_u.role),
            )
        }
        Ok(None) => {
            if is_local_session_token {
                return None;
            }
            if auth_cfg.enabled
                && (decoded.claims.role.eq_ignore_ascii_case("admin")
                    || decoded.claims.sub.eq_ignore_ascii_case("admin"))
            {
                let oidc_active = auth_cfg.oidc.enabled
                    && (!auth_cfg.oidc.issuer_url.trim().is_empty()
                        || !std::env::var("NINJANVR_OIDC_ISSUER_URL")
                            .unwrap_or_default()
                            .trim()
                            .is_empty());
                if !oidc_active {
                    return None;
                }
            }
            (decoded.claims.sub, decoded.claims.role)
        }
        Err(_) => return None,
    };

    Some((canonical_sub, role))
}

pub fn extract_jwt_iat(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    cookie_name: &str,
) -> Option<i64> {
    let token_str = if let Some(auth_val) = headers.get(header::AUTHORIZATION) {
        let raw = auth_val.to_str().ok()?;
        parse_authorization_bearer(raw)?.to_string()
    } else {
        jar.get(cookie_name).map(|c| c.value().to_string())?
    };
    if token_str.starts_with("nnvr_") {
        return None;
    }
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    let decoded = jsonwebtoken::decode::<JwtClaims>(
        &token_str,
        &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
        &validation,
    )
    .ok()?;
    Some(decoded.claims.iat as i64)
}

pub fn extract_jwt_allowed_cameras(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    cookie_name: &str,
) -> Option<String> {
    let token_str = if let Some(auth_val) = headers.get(header::AUTHORIZATION) {
        let raw = auth_val.to_str().ok()?;
        parse_authorization_bearer(raw)?.to_string()
    } else {
        jar.get(cookie_name).map(|c| c.value().to_string())?
    };
    if token_str.starts_with("nnvr_") {
        return None;
    }
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;
    let decoded = jsonwebtoken::decode::<JwtClaims>(
        &token_str,
        &DecodingKey::from_secret(state.jwt_secret.as_bytes()),
        &validation,
    )
    .ok()?;
    decoded.claims.allowed_cameras
}

pub fn resolve_user_allowed_cameras(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    auth_cfg: &ninjanvr_config::AuthConfig,
    user: &str,
    role: &str,
) -> Option<Vec<String>> {
    if auth_cfg.enabled && role.trim().eq_ignore_ascii_case("metrics") {
        return Some(Vec::new());
    }
    match state.auth_gate.get_user_cached_sync(&state.db, user) {
        Ok(Some(db_u)) => {
            if db_u.role.eq_ignore_ascii_case("admin") {
                if !role.eq_ignore_ascii_case("admin")
                    && let Some(jwt_raw) =
                        extract_jwt_allowed_cameras(state, jar, headers, &auth_cfg.cookie_name)
                    && !jwt_raw.trim().is_empty()
                    && jwt_raw.trim() != "*"
                {
                    let jwt_list: Vec<String> = jwt_raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !jwt_list.is_empty() && !jwt_list.iter().any(|c| c == "*") {
                        return Some(jwt_list);
                    }
                }
                return None;
            }
            let raw = db_u.allowed_cameras.trim();
            if raw.is_empty() || raw == "*" {
                if let Some(jwt_raw) =
                    extract_jwt_allowed_cameras(state, jar, headers, &auth_cfg.cookie_name)
                    && !jwt_raw.trim().is_empty()
                    && jwt_raw.trim() != "*"
                {
                    let jwt_list: Vec<String> = jwt_raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !jwt_list.is_empty() && !jwt_list.iter().any(|c| c == "*") {
                        return Some(jwt_list);
                    }
                }
                return None;
            }
            let list: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if list.is_empty() || list.iter().any(|c| c == "*") {
                None
            } else {
                Some(list)
            }
        }
        Ok(None) => {
            let has_non_api_jwt =
                extract_jwt_allowed_cameras(state, jar, headers, &auth_cfg.cookie_name).is_some();
            let is_token_principal = user
                .trim()
                .get(..6)
                .is_some_and(|p| p.eq_ignore_ascii_case("token:"));
            if (is_token_principal && !has_non_api_jwt)
                || (!auth_cfg.enabled
                    && user.eq_ignore_ascii_case("admin")
                    && role.eq_ignore_ascii_case("admin"))
            {
                return None;
            }
            let oidc_active = auth_cfg.oidc.enabled
                && (!auth_cfg.oidc.issuer_url.trim().is_empty()
                    || !std::env::var("NINJANVR_OIDC_ISSUER_URL")
                        .unwrap_or_default()
                        .trim()
                        .is_empty());
            let trusted_cidrs = if !auth_cfg.proxy.trusted_proxies.is_empty() {
                &auth_cfg.proxy.trusted_proxies
            } else {
                &auth_cfg.trusted_proxies
            };
            let verified_proxy_peer = extract_trusted_proxy_peer_ip(headers)
                .is_some_and(|ip| ip_matches_trusted_proxies(ip, trusted_cidrs));
            let is_proxy_header_auth = auth_cfg.proxy.enabled
                && !headers.contains_key(header::AUTHORIZATION)
                && (jar.get(&auth_cfg.cookie_name).is_none() || verified_proxy_peer)
                && {
                    let user_hdr_trimmed = auth_cfg.proxy.user_header.trim();
                    let user_hdr_name = if user_hdr_trimmed.is_empty() {
                        "remote-user".to_string()
                    } else {
                        user_hdr_trimmed.to_ascii_lowercase()
                    };
                    headers
                        .get(user_hdr_name.as_str())
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|u| u.trim().eq_ignore_ascii_case(user))
                }
                && {
                    let has_any_ip_hdr = headers.contains_key("x-peer-ip")
                        || headers.contains_key("x-real-ip")
                        || headers.contains_key("x-forwarded-for");
                    !has_any_ip_hdr || verified_proxy_peer
                };
            if !oidc_active && !is_proxy_header_auth {
                return Some(Vec::new());
            }
            if role.eq_ignore_ascii_case("admin") || is_proxy_header_auth {
                return None;
            }
            match extract_jwt_allowed_cameras(state, jar, headers, &auth_cfg.cookie_name) {
                Some(raw) if !raw.trim().is_empty() && raw.trim() != "*" => {
                    let list: Vec<String> = raw
                        .trim()
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if list.iter().any(|c| c == "*") {
                        None
                    } else {
                        Some(list)
                    }
                }
                Some(raw) if raw.trim() == "*" => None,
                _ => Some(Vec::new()),
            }
        }
        Err(_) => Some(Vec::new()),
    }
}

pub async fn caller_allowed_cameras(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Option<Vec<String>> {
    let auth_cfg = state.config.read().await.auth.clone();
    warm_auth_cache_for_request(state, jar, headers, &auth_cfg).await;
    if let Some((user, role)) = extract_authenticated_user_with_cfg(state, jar, headers, &auth_cfg)
    {
        if auth_cfg.enabled && role.trim().eq_ignore_ascii_case("metrics") {
            return Some(Vec::new());
        }
        return resolve_user_allowed_cameras(state, jar, headers, &auth_cfg, &user, &role);
    }
    if auth_cfg.enabled && request_requires_strict_auth(headers, jar, &auth_cfg) {
        Some(Vec::new())
    } else {
        None
    }
}

pub fn scope_cameras_filter(
    requested: Option<Vec<String>>,
    allowed: Option<Vec<String>>,
) -> Option<Vec<String>> {
    match (requested, allowed) {
        (Some(req_list), None) => {
            if req_list.iter().any(|c| c == "all" || c == "*") {
                None
            } else {
                Some(req_list)
            }
        }
        (None, None) => None,
        (None, Some(allow_list)) => {
            if allow_list.iter().any(|a| a == "*") {
                return None;
            }
            let safe_allow: Vec<String> = allow_list.into_iter().filter(|a| a != "all").collect();
            Some(if safe_allow.is_empty() {
                vec!["__none__".to_string()]
            } else {
                safe_allow
            })
        }
        (Some(req_list), Some(allow_list)) => {
            if allow_list.iter().any(|a| a == "*") {
                if req_list.iter().any(|c| c == "all" || c == "*") {
                    return None;
                }
                return Some(req_list);
            }
            let safe_allow: Vec<String> = allow_list.into_iter().filter(|a| a != "all").collect();
            if req_list.iter().any(|c| c == "all" || c == "*") {
                return Some(if safe_allow.is_empty() {
                    vec!["__none__".to_string()]
                } else {
                    safe_allow
                });
            }
            let filtered: Vec<String> = req_list
                .into_iter()
                .filter(|c| c != "all" && safe_allow.contains(c))
                .collect();
            Some(if filtered.is_empty() {
                vec!["__none__".to_string()]
            } else {
                filtered
            })
        }
    }
}

pub async fn effective_caller_allowed_cameras(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Option<Vec<String>> {
    caller_allowed_cameras(state, jar, headers).await
}

pub async fn caller_can_mutate(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    require_admin: bool,
) -> bool {
    let auth_cfg = state.config.read().await.auth.clone();
    if !auth_cfg.enabled {
        return true;
    }
    warm_auth_cache_for_request(state, jar, headers, &auth_cfg).await;
    if !request_requires_strict_auth(headers, jar, &auth_cfg) {
        return true;
    }
    let Some((_user, role)) = extract_authenticated_user_with_cfg(state, jar, headers, &auth_cfg)
    else {
        return false;
    };
    let role_lc = role.trim().to_ascii_lowercase();
    if matches!(role_lc.as_str(), "viewer" | "read_only" | "metrics") {
        return false;
    }
    if require_admin && role_lc != "admin" {
        return false;
    }
    true
}

/// Why review media was refused (kept small so `Result` stays cheap to move).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMediaDenied {
    BadRequest,
    NotFound,
    Forbidden,
}

impl ReviewMediaDenied {
    pub fn into_response(self) -> Response {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Camera is not permitted for this user" })),
            )
                .into_response(),
        }
    }
}

/// Shared gate for review media: the review must exist, the caller must be allowed
/// to see its camera, and cameras with an active privacy mask never expose media.
pub async fn authorize_review_media(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
    id: &str,
) -> Result<ninjanvr_db::ReviewItemDto, ReviewMediaDenied> {
    if !is_safe_id(id) {
        return Err(ReviewMediaDenied::BadRequest);
    }
    let db_for_rev = state.db.clone();
    let id_owned = id.to_string();
    let Ok(Some(item)) =
        tokio::task::spawn_blocking(move || db_for_rev.get_review_by_id(&id_owned))
            .await
            .ok()
            .unwrap_or(Ok(None))
    else {
        return Err(ReviewMediaDenied::NotFound);
    };
    if let Some(allowed) = effective_caller_allowed_cameras(state, jar, headers).await
        && !allowed.iter().any(|a| a == &item.camera)
    {
        return Err(ReviewMediaDenied::Forbidden);
    }
    if state
        .media_delivery_gate
        .blocks_untranscoded_video(&item.camera)
        .await
    {
        return Err(ReviewMediaDenied::NotFound);
    }
    Ok(item)
}

pub async fn verify_caller_can_mutate(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Option<AuthRejection> {
    let auth_cfg = state.config.read().await.auth.clone();
    if auth_cfg.enabled {
        warm_auth_cache_for_request(state, jar, headers, &auth_cfg).await;
        match extract_authenticated_user_with_cfg(state, jar, headers, &auth_cfg) {
            Some((_, role)) => {
                let role_lc = role.trim().to_ascii_lowercase();
                if matches!(role_lc.as_str(), "viewer" | "read_only" | "metrics") {
                    return Some(AuthRejection::new(
                        StatusCode::FORBIDDEN,
                        "Insufficient role for mutation",
                    ));
                }
            }
            None if request_requires_strict_auth(headers, jar, &auth_cfg) => {
                return Some(AuthRejection::new(StatusCode::UNAUTHORIZED, "Unauthorized"));
            }
            None => {}
        }
    }
    None
}

pub async fn verify_camera_mutation_access(
    state: &AppState,
    jar: &CookieJar,
    uri: &Uri,
    headers: &HeaderMap,
    camera: &str,
) -> Option<AuthRejection> {
    if !verify_cookie_csrf_origin(headers, uri) {
        return Some(AuthRejection::new(
            StatusCode::FORBIDDEN,
            "Cross-origin cookie request rejected (Origin/Host mismatch)",
        ));
    }
    let auth_cfg = state.config.read().await.auth.clone();
    if auth_cfg.enabled {
        warm_auth_cache_for_request(state, jar, headers, &auth_cfg).await;
        match extract_authenticated_user_with_cfg(state, jar, headers, &auth_cfg) {
            Some((user, role)) => {
                let role_lc = role.to_ascii_lowercase();
                if matches!(role_lc.as_str(), "viewer" | "read_only" | "metrics") {
                    return Some(AuthRejection::new(
                        StatusCode::FORBIDDEN,
                        "Insufficient role for camera mutation",
                    ));
                }
                if role_lc != "admin"
                    && let Some(allowed) =
                        resolve_user_allowed_cameras(state, jar, headers, &auth_cfg, &user, &role)
                    && !allowed.iter().any(|a| a == camera || a == "*")
                {
                    return Some(AuthRejection::with_message(
                        StatusCode::FORBIDDEN,
                        format!("Camera '{camera}' is not permitted for this user"),
                    ));
                }
            }
            None if request_requires_strict_auth(headers, jar, &auth_cfg) => {
                return Some(AuthRejection::new(StatusCode::UNAUTHORIZED, "Unauthorized"));
            }
            None => {}
        }
    }
    None
}

fn is_valid_host_header_token(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && !trimmed.contains(['/', '?', '#', '\\', '@'])
        && !trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub(crate) fn extract_url_scheme_and_host(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        return None;
    }
    let (scheme_raw, after_scheme) = trimmed.split_once("://")?;
    let scheme = scheme_raw.trim();
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let raw_authority = after_scheme
        .split(['/', '?', '#', '\\'])
        .next()
        .unwrap_or("")
        .trim();
    let host = raw_authority
        .rsplit_once('@')
        .map_or(raw_authority, |(_, hp)| hp.trim());
    if !is_valid_host_header_token(host) {
        return None;
    }
    Some((scheme, host))
}

fn strip_default_port_for_scheme<'a>(scheme: &str, host: &'a str) -> &'a str {
    if scheme.eq_ignore_ascii_case("https") {
        host.strip_suffix(":443").unwrap_or(host)
    } else if scheme.eq_ignore_ascii_case("http") {
        host.strip_suffix(":80").unwrap_or(host)
    } else {
        host
    }
}

pub(crate) fn bare_hostname(host_port: &str) -> &str {
    let trimmed = host_port.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some((ipv6, _)) = rest.split_once(']')
    {
        return ipv6;
    }
    if let Some((h, p)) = trimmed.rsplit_once(':')
        && !h.contains(':')
        && p.chars().all(|c| c.is_ascii_digit())
    {
        return h;
    }
    trimmed
}

pub(crate) fn is_internal_backend_host(host_port: &str) -> bool {
    let host_raw = bare_hostname(host_port).trim().trim_end_matches('.');
    let host_lc = host_raw.to_ascii_lowercase();
    if host_lc == "localhost"
        || host_lc == "ninjanvr"
        || host_lc.ends_with(".svc")
        || host_lc.ends_with(".cluster.local")
        || host_lc.ends_with(".internal")
    {
        return true;
    }
    if let Ok(ip) = host_raw.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_loopback(),
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
        };
    }
    false
}

fn request_host_matches(headers: &HeaderMap, uri: &Uri, scheme: &str, source_host: &str) -> bool {
    let normalized_source = strip_default_port_for_scheme(scheme, source_host);
    let x_forwarded_host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| is_valid_host_header_token(s));
    let host_header = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| is_valid_host_header_token(s));
    let uri_authority = uri
        .authority()
        .map(|a| a.as_str().trim())
        .filter(|s| is_valid_host_header_token(s));
    let is_same_origin_fetch = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("same-origin"));

    let primary_host = host_header.or(uri_authority);
    let allow_xfh = match (x_forwarded_host, primary_host) {
        (Some(_), None) => true,
        (Some(_), Some(_)) if is_same_origin_fetch => true,
        (Some(xfh), Some(pri)) => {
            let norm_xfh = strip_default_port_for_scheme(scheme, xfh);
            let norm_pri = strip_default_port_for_scheme(scheme, pri);
            is_internal_backend_host(pri)
                || (bare_hostname(xfh).eq_ignore_ascii_case(bare_hostname(pri))
                    && (!norm_xfh.contains(':') || norm_xfh.eq_ignore_ascii_case(norm_pri)))
        }
        (None, _) => false,
    };

    let trusted_xfh = if allow_xfh { x_forwarded_host } else { None };

    [trusted_xfh, host_header, uri_authority]
        .into_iter()
        .flatten()
        .any(|candidate| {
            let normalized_candidate = strip_default_port_for_scheme(scheme, candidate);
            normalized_source.eq_ignore_ascii_case(normalized_candidate)
        })
}

pub fn verify_cookie_csrf_origin(headers: &HeaderMap, uri: &Uri) -> bool {
    let has_bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_authorization_bearer)
        .is_some();
    if has_bearer {
        return true;
    }
    let mut is_same_origin_sfs = false;
    if let Some(sfs) = headers.get("sec-fetch-site") {
        let Ok(sfs_str) = sfs.to_str() else {
            return false;
        };
        let sfs_trimmed = sfs_str.trim();
        if !sfs_trimmed.eq_ignore_ascii_case("same-origin")
            && !sfs_trimmed.eq_ignore_ascii_case("none")
        {
            return false;
        }
        is_same_origin_sfs = sfs_trimmed.eq_ignore_ascii_case("same-origin");
    }
    if let Some(origin_val) = headers.get(header::ORIGIN) {
        let Some((scheme, origin_host)) = origin_val
            .to_str()
            .ok()
            .and_then(extract_url_scheme_and_host)
        else {
            return false;
        };
        return request_host_matches(headers, uri, scheme, origin_host);
    }
    if let Some(referer_val) = headers.get(header::REFERER) {
        let Some((scheme, referer_host)) = referer_val
            .to_str()
            .ok()
            .and_then(extract_url_scheme_and_host)
        else {
            return false;
        };
        return request_host_matches(headers, uri, scheme, referer_host);
    }
    if headers.contains_key(header::COOKIE) {
        return is_same_origin_sfs
            && (headers.contains_key(header::HOST) || uri.authority().is_some());
    }
    true
}

pub fn filter_ws_message_for_allowed_cameras(
    msg: &str,
    allowed: Option<&[String]>,
) -> Option<String> {
    let Some(allow_list) = allowed else {
        return Some(msg.to_string());
    };
    if allow_list.iter().any(|a| a == "*") {
        return Some(msg.to_string());
    }
    let parsed: Value = serde_json::from_str(msg).ok()?;
    let topic = parsed.get("topic").and_then(|v| v.as_str())?;
    let clean_topic = topic.strip_prefix("ninjanvr/").unwrap_or(topic);
    if clean_topic == "profile" || clean_topic.starts_with("profile/") {
        return Some(msg.to_string());
    }
    if clean_topic == "config" {
        let mut payload_val: Value = match parsed.get("payload") {
            Some(Value::String(s)) => serde_json::from_str(s).ok()?,
            Some(v @ Value::Object(_)) => v.clone(),
            _ => {
                return Some(
                    json!({ "topic": topic, "payload": { "type": "updated" } }).to_string(),
                );
            }
        };
        if let Some(arr) = payload_val
            .get_mut("invalidated_cameras")
            .and_then(Value::as_array_mut)
        {
            arr.retain(|v| {
                v.as_str()
                    .is_some_and(|c| allow_list.iter().any(|a| a == c.trim()))
            });
        }
        let filtered_payload = if parsed.get("payload").is_some_and(Value::is_string) {
            Value::String(serde_json::to_string(&payload_val).unwrap_or_else(|_| "{}".to_string()))
        } else {
            payload_val
        };
        return Some(
            json!({
                "topic": topic,
                "payload": filtered_payload
            })
            .to_string(),
        );
    }
    if clean_topic == "stats" {
        let mut payload_val: Value = match parsed.get("payload") {
            Some(Value::String(s)) => serde_json::from_str(s).ok()?,
            Some(v @ Value::Object(_)) => v.clone(),
            _ => return None,
        };
        filter_stats_snapshot_for_allowed_cameras(&mut payload_val, allow_list);
        let filtered_payload = if parsed.get("payload").is_some_and(Value::is_string) {
            Value::String(serde_json::to_string(&payload_val).unwrap_or_else(|_| "{}".to_string()))
        } else {
            payload_val
        };
        return Some(
            json!({
                "topic": topic,
                "payload": filtered_payload
            })
            .to_string(),
        );
    }
    if clean_topic == "camera_activity" {
        let payload_val: Value = match parsed.get("payload") {
            Some(Value::String(s)) => serde_json::from_str(s).ok()?,
            Some(v @ Value::Object(_)) => v.clone(),
            _ => return None,
        };
        let mut map = payload_val.as_object()?.clone();
        map.retain(|cam, _| allow_list.iter().any(|a| a == cam.trim()));
        let filtered_payload =
            serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| "{}".to_string());
        return Some(
            json!({
                "topic": topic,
                "payload": filtered_payload
            })
            .to_string(),
        );
    }
    if clean_topic == "events" || clean_topic == "reviews" || clean_topic == "exports" {
        let payload_val: Value = match parsed.get("payload") {
            Some(Value::String(s)) => serde_json::from_str(s).ok()?,
            Some(v @ Value::Object(_)) => v.clone(),
            _ => return None,
        };
        let after_cam = payload_val
            .pointer("/after/camera")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let before_cam = payload_val
            .pointer("/before/camera")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let top_cam = payload_val
            .get("camera")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(bc) = before_cam
            && !allow_list.iter().any(|a| a == bc)
        {
            return None;
        }
        if let Some(ac) = after_cam
            && !allow_list.iter().any(|a| a == ac)
        {
            return None;
        }
        if let Some(tc) = top_cam
            && !allow_list.iter().any(|a| a == tc)
        {
            return None;
        }
        let cam = after_cam.or(before_cam).or(top_cam)?;
        return if allow_list.iter().any(|a| a == cam) {
            Some(msg.to_string())
        } else {
            None
        };
    }
    let first_seg = clean_topic.split('/').next().unwrap_or("").trim();
    if !first_seg.is_empty() && allow_list.iter().any(|a| a == first_seg) {
        Some(msg.to_string())
    } else {
        None
    }
}

fn map_cached_ws_user_access(
    db_u: &CachedDbUser,
    session_iat: Option<i64>,
) -> Option<(String, Option<Vec<String>>)> {
    if let (Some(iat), Some(changed_epoch)) = (session_iat, db_u.password_changed_at_epoch)
        && iat < changed_epoch
    {
        return None;
    }
    let role = db_u.role.trim().to_string();
    if role.eq_ignore_ascii_case("admin") {
        return Some((role, None));
    }
    let raw = db_u.allowed_cameras.trim();
    if raw.is_empty() || raw == "*" {
        return Some((role, None));
    }
    let list: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if list.is_empty() || list.iter().any(|c| c == "*") {
        Some((role, None))
    } else {
        Some((role, Some(list)))
    }
}

pub fn reverify_ws_caller_access(
    state: &AppState,
    caller_user: Option<&str>,
    had_db_user_at_connect: bool,
    fallback_role: &str,
    fallback_allowed: &Option<Vec<String>>,
) -> Option<(String, Option<Vec<String>>)> {
    reverify_ws_caller_access_with_iat(
        state,
        caller_user,
        had_db_user_at_connect,
        fallback_role,
        fallback_allowed,
        None,
    )
}

pub fn reverify_ws_caller_access_with_iat(
    state: &AppState,
    caller_user: Option<&str>,
    had_db_user_at_connect: bool,
    fallback_role: &str,
    fallback_allowed: &Option<Vec<String>>,
    session_iat: Option<i64>,
) -> Option<(String, Option<Vec<String>>)> {
    let Some(user) = caller_user else {
        return Some((fallback_role.to_string(), fallback_allowed.clone()));
    };
    if !user.starts_with("token:")
        && state
            .auth_gate
            .is_token_revoked_by_user_deletion(user, session_iat.unwrap_or(0).max(0) as u64)
    {
        return None;
    }
    if let Some(token_name) = user.strip_prefix("token:") {
        if !state.auth_gate.is_api_token_active_cached_sync(
            &state.db,
            token_name.trim(),
            unix_now() as f64,
        ) {
            return None;
        }
        return Some((fallback_role.to_string(), fallback_allowed.clone()));
    }
    match state.auth_gate.get_user_cached_sync(&state.db, user) {
        Ok(Some(db_u)) => map_cached_ws_user_access(&db_u, session_iat),
        Ok(None) if had_db_user_at_connect => None,
        Ok(None) => Some((fallback_role.to_string(), fallback_allowed.clone())),
        Err(_) => None,
    }
}

pub async fn reverify_ws_caller_access_async(
    state: &AppState,
    caller_user: Option<&str>,
    had_db_user_at_connect: bool,
    fallback_role: &str,
    fallback_allowed: &Option<Vec<String>>,
    session_iat: Option<i64>,
) -> Option<(String, Option<Vec<String>>)> {
    let Some(user) = caller_user else {
        return Some((fallback_role.to_string(), fallback_allowed.clone()));
    };
    if !user.starts_with("token:")
        && state
            .auth_gate
            .is_token_revoked_by_user_deletion(user, session_iat.unwrap_or(0).max(0) as u64)
    {
        return None;
    }
    if let Some(token_name) = user.strip_prefix("token:") {
        if !state
            .auth_gate
            .is_api_token_active_cached_async(&state.db, token_name.trim(), unix_now() as f64)
            .await
        {
            return None;
        }
        return Some((fallback_role.to_string(), fallback_allowed.clone()));
    }
    match state.auth_gate.get_user_cached_async(&state.db, user).await {
        Ok(Some(db_u)) => map_cached_ws_user_access(&db_u, session_iat),
        Ok(None) if had_db_user_at_connect => None,
        Ok(None) => Some((fallback_role.to_string(), fallback_allowed.clone())),
        Err(_) => None,
    }
}

// ============================================================================
// Part 5 — Unit Tests (Instance Isolation & AuthSessionGate Verification)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_app_state;
    use std::path::PathBuf;

    async fn setup_test_auth_state() -> (PathBuf, AppState) {
        let tmp_dir = std::env::temp_dir().join(format!(
            "ninjanvr-auth-gate-test-{}-{}",
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
        std::fs::create_dir_all(media_dir.join("clips/review")).unwrap();
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

        let state = new_app_state(&cfg_path, web_dir, media_dir, false)
            .await
            .unwrap();
        let _ = state.db.ensure_default_admin(1000).unwrap();
        (tmp_dir, state)
    }

    fn mint_jwt(secret: &str, sub: &str, role: &str, allowed_cameras: Option<&str>) -> String {
        let now = unix_now();
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &JwtClaims {
                sub: sub.to_string(),
                role: role.to_string(),
                allowed_cameras: allowed_cameras.map(str::to_string),
                iat: now,
                exp: now + 3600,
            },
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    #[test]
    fn auth_session_gate_login_rate_limiting_and_oidc_pending_states_are_instance_isolated() {
        let gate_a = AuthSessionGate::new();
        let gate_b = AuthSessionGate::new();
        let scope = "shared-scope-key";

        for _ in 0..MAX_LOGIN_FAILURES {
            assert!(!gate_a.check_login_rate_limited(scope, "OperatorOne"));
            gate_a.record_login_failure(scope, "OperatorOne");
        }
        assert!(
            gate_a.check_login_rate_limited(scope, "operatorone"),
            "gate_a must lock out user after MAX_LOGIN_FAILURES case-insensitively"
        );
        assert!(
            !gate_b.check_login_rate_limited(scope, "OperatorOne"),
            "gate_b must be completely isolated from gate_a lockout state"
        );

        gate_a.clear_login_failures(scope, "OPERATORONE");
        assert!(
            !gate_a.check_login_rate_limited(scope, "OperatorOne"),
            "clearing login failures on gate_a must lift lockout"
        );

        gate_a.store_oidc_pending_state(
            "st_1".to_string(),
            "verifier_a".to_string(),
            "https://nvr.local/api/auth/oidc/callback".to_string(),
        );
        assert!(
            gate_b.take_oidc_pending_state("st_1").is_none(),
            "OIDC pending state in gate_a must not be visible in gate_b"
        );
        let taken = gate_a.take_oidc_pending_state("st_1").unwrap();
        assert_eq!(taken.code_verifier, "verifier_a");
        assert!(
            gate_a.take_oidc_pending_state("st_1").is_none(),
            "OIDC pending state must be single-use"
        );
    }

    #[tokio::test]
    async fn path_normalization_protected_routes_and_static_media_camera_resolution_work() {
        assert_eq!(
            percent_decode_path("/api/%66ront_door/state"),
            Some("/api/front_door/state".to_string())
        );
        assert_eq!(percent_decode_path("/api/front%2fdoor/state"), None);
        assert_eq!(percent_decode_path("/api/front%00door/state"), None);

        assert_eq!(
            normalize_http_path_for_auth("//api///%66ront_door/state"),
            Some("/api/front_door/state".to_string())
        );
        assert_eq!(normalize_http_path_for_auth("/api/../login"), None);
        assert_eq!(normalize_http_path_for_auth("/api/%2e%2e/login"), None);
        assert_eq!(normalize_http_path_for_auth("/api/back\\slash"), None);

        assert!(!is_protected_path("/api/login"));
        assert!(!is_protected_path("/api/auth/providers"));
        assert!(is_protected_path("/api/config"));
        assert!(is_protected_path("/%61pi/config"));
        assert!(is_protected_path("/clips/front_door-ev1.webp"));
        assert!(is_protected_path(
            "/recordings/2026-11-08/12/front_door/00.00.mp4"
        ));
        assert!(is_protected_path("/exports/exp_1.mp4"));

        let (tmp_dir, state) = setup_test_auth_state().await;
        state
            .db
            .insert_export(&ninjanvr_db::ExportRow {
                id: "exp_custom_1".to_string(),
                camera: "backyard".to_string(),
                name: "Backyard Clip".to_string(),
                date: 1_700_000_000.0,
                video_path: "/media/ninjanvr/exports/exp_custom_1.mp4".to_string(),
                thumb_path: "/media/ninjanvr/clips/exp_custom_1.webp".to_string(),
                in_progress: false,
            })
            .unwrap();

        assert_eq!(
            resolve_static_media_camera(&state, "/exports/exp_custom_1.mp4"),
            Some("backyard".to_string())
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/exports/nested/exp_custom_1.mp4"),
            None
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/exports/.hidden.mp4"),
            None
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/recordings/2026-11-08/14/front_door/10.20.mp4"),
            Some("front_door".to_string())
        );
        assert_eq!(
            resolve_static_media_camera(&state, "/clips/previews/front_door/100.webp"),
            Some("front_door".to_string())
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn trusted_proxy_cidr_matching_and_oidc_role_capping_work() {
        assert!(ip_matches_cidr("10.42.1.99", "10.42.0.0/16"));
        assert!(ip_matches_cidr("::ffff:10.42.1.99", "10.42.0.0/16"));
        assert!(!ip_matches_cidr("192.168.1.10", "10.42.0.0/16"));
        assert!(ip_matches_cidr("fd00::1234", "fd00::/64"));
        assert!(!ip_matches_cidr("fd01::1234", "fd00::/64"));

        let cidrs = vec!["127.0.0.1/32".to_string(), "10.42.0.0/16".to_string()];
        assert!(ip_matches_trusted_proxies("10.42.5.6", &cidrs));
        assert!(!ip_matches_trusted_proxies("172.16.0.1", &cidrs));

        let mut oidc_cfg = ninjanvr_config::OidcAuthConfig::default();
        oidc_cfg.role_mapping.admin = vec!["nvr-admins".to_string()];
        oidc_cfg.role_mapping.operator = vec!["nvr-operators".to_string()];
        oidc_cfg.role_mapping.viewer = vec!["nvr-viewers".to_string()];

        assert_eq!(
            resolve_oidc_role(&json!({ "groups": ["nvr-operators"] }), &oidc_cfg),
            "operator"
        );
        assert_eq!(
            resolve_oidc_role(
                &json!({ "urn:zitadel:iam:org:project:roles": { "nvr-admins": { "org": "1" } } }),
                &oidc_cfg
            ),
            "admin"
        );
        assert_eq!(
            resolve_oidc_role(&json!({ "groups": ["unmapped"] }), &oidc_cfg),
            "viewer"
        );

        assert_eq!(cap_external_role_by_db("admin", "viewer"), "viewer");
        assert_eq!(cap_external_role_by_db("admin", "operator"), "operator");
        assert_eq!(cap_external_role_by_db("viewer", "admin"), "viewer");
        assert_eq!(cap_external_role_by_db("operator", "admin"), "operator");
    }

    #[tokio::test]
    async fn csrf_cookie_origin_and_caller_principal_metrics_and_target_scoping_work() {
        let uri: Uri = "/api/events/ev1/retain".parse().unwrap();

        // Bearer token bypasses cookie CSRF origin check
        let mut bearer_hdrs = HeaderMap::new();
        bearer_hdrs.insert(header::AUTHORIZATION, "Bearer tok123".parse().unwrap());
        bearer_hdrs.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(verify_cookie_csrf_origin(&bearer_hdrs, &uri));

        // Matching Origin and Host succeeds
        let mut same_origin_hdrs = HeaderMap::new();
        same_origin_hdrs.insert(header::HOST, "nvr.local:5000".parse().unwrap());
        same_origin_hdrs.insert(header::ORIGIN, "http://nvr.local:5000".parse().unwrap());
        assert!(verify_cookie_csrf_origin(&same_origin_hdrs, &uri));

        // Cross-origin mismatches fail
        let mut cross_origin_hdrs = HeaderMap::new();
        cross_origin_hdrs.insert(header::HOST, "nvr.local:5000".parse().unwrap());
        cross_origin_hdrs.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!verify_cookie_csrf_origin(&cross_origin_hdrs, &uri));

        let (tmp_dir, state) = setup_test_auth_state().await;
        state
            .db
            .create_user_with_cameras("scoped_op", "pass1234", "operator", "front_door", 1000)
            .unwrap();
        let (_, metrics_tok) = state
            .db
            .create_api_token("prom_metrics", "metrics", None)
            .unwrap();

        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_front".to_string(),
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
        state
            .db
            .upsert_event(&ninjanvr_db::EventDto {
                id: "ev_back".to_string(),
                camera: "backyard".to_string(),
                label: "person".to_string(),
                zones: json!([]),
                start_time: 2000.0,
                end_time: Some(2005.0),
                has_clip: false,
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

        let jar = CookieJar::new();
        let mut metrics_hdrs = HeaderMap::new();
        metrics_hdrs.insert(
            header::AUTHORIZATION,
            format!("Bearer {metrics_tok}").parse().unwrap(),
        );

        // Both caller_allowed_cameras and effective_caller_allowed_cameras restrict metrics role to Some(vec![])
        assert_eq!(
            caller_allowed_cameras(&state, &jar, &metrics_hdrs).await,
            Some(Vec::new())
        );
        assert_eq!(
            effective_caller_allowed_cameras(&state, &jar, &metrics_hdrs).await,
            Some(Vec::new())
        );

        let metrics_principal = state
            .auth_gate
            .authenticate_read(&state, &jar, &metrics_hdrs)
            .await
            .unwrap();
        assert!(!metrics_principal.can_mutate());
        assert!(!metrics_principal.is_admin());
        assert!(!metrics_principal.allows_camera("front_door"));
        assert_eq!(
            metrics_principal.camera_scope(),
            CameraScope::Allowed(Vec::new())
        );

        let mut scoped_hdrs = HeaderMap::new();
        scoped_hdrs.insert(
            header::AUTHORIZATION,
            format!(
                "Bearer {}",
                mint_jwt(
                    &state.jwt_secret,
                    "scoped_op",
                    "operator",
                    Some("front_door")
                )
            )
            .parse()
            .unwrap(),
        );

        let ok_front = state
            .auth_gate
            .authorize_mutation(
                &state,
                &jar,
                &uri,
                &scoped_hdrs,
                false,
                AccessTarget::EventId("ev_front"),
            )
            .await
            .expect("scoped operator must be allowed to mutate front_door event");
        assert!(ok_front.allows_camera("front_door"));
        assert!(!ok_front.allows_camera("backyard"));

        let err_back = state
            .auth_gate
            .authorize_mutation(
                &state,
                &jar,
                &uri,
                &scoped_hdrs,
                false,
                AccessTarget::EventId("ev_back"),
            )
            .await
            .unwrap_err();
        assert_eq!(err_back.status(), StatusCode::FORBIDDEN);

        let err_admin = state
            .auth_gate
            .authorize_mutation(
                &state,
                &jar,
                &uri,
                &scoped_hdrs,
                true,
                AccessTarget::EventId("ev_front"),
            )
            .await
            .unwrap_err();
        assert_eq!(err_admin.status(), StatusCode::FORBIDDEN);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn ws_message_camera_filtering_enforces_allowed_camera_scope() {
        let allowed = vec!["front_door".to_string()];

        let ev_front = json!({
            "topic": "events",
            "payload": { "after": { "id": "ev1", "camera": "front_door" } }
        })
        .to_string();
        assert!(filter_ws_message_for_allowed_cameras(&ev_front, Some(&allowed)).is_some());

        let ev_back = json!({
            "topic": "events",
            "payload": { "after": { "id": "ev2", "camera": "backyard" } }
        })
        .to_string();
        assert!(filter_ws_message_for_allowed_cameras(&ev_back, Some(&allowed)).is_none());

        let activity_msg = json!({
            "topic": "camera_activity",
            "payload": {
                "front_door": { "motion": true },
                "backyard": { "motion": true }
            }
        })
        .to_string();
        let filtered_act =
            filter_ws_message_for_allowed_cameras(&activity_msg, Some(&allowed)).unwrap();
        let act_json: Value = serde_json::from_str(&filtered_act).unwrap();
        let act_payload: Value =
            serde_json::from_str(act_json["payload"].as_str().unwrap()).unwrap();
        assert!(act_payload.get("front_door").is_some());
        assert!(act_payload.get("backyard").is_none());

        let stats_msg = json!({
            "topic": "stats",
            "payload": {
                "cameras": {
                    "front_door": { "camera_fps": 15.0, "detection_fps": 5.0 },
                    "backyard": { "camera_fps": 20.0, "detection_fps": 4.0 }
                }
            }
        })
        .to_string();
        let filtered_stats =
            filter_ws_message_for_allowed_cameras(&stats_msg, Some(&allowed)).unwrap();
        let stats_json: Value = serde_json::from_str(&filtered_stats).unwrap();
        assert!(stats_json["payload"]["cameras"].get("front_door").is_some());
        assert!(stats_json["payload"]["cameras"].get("backyard").is_none());
        assert_eq!(stats_json["payload"]["camera_fps"], json!(15.0));
    }
}
