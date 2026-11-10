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
    AppState, filter_stats_snapshot_for_allowed_cameras, has_active_privacy_mask, is_safe_id,
    open_regular_non_symlink_readonly, strip_share_artifact_prefix, unix_now,
};

// ============================================================================
// Part 1 — AuthSessionGate, CallerPrincipal & AccessTarget
// ============================================================================

pub type LoginAttemptKey = (String, String);
pub type LoginAttemptEntry = (u32, Instant);

pub const MAX_LOGIN_FAILURES: u32 = 8;
pub const LOGIN_LOCKOUT_WINDOW: Duration = Duration::from_secs(60);
pub const OIDC_PENDING_STATE_TTL: Duration = Duration::from_secs(600);
pub const MAX_OIDC_PENDING_STATES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcPendingState {
    pub code_verifier: String,
    pub redirect_uri: String,
    pub created_at: Instant,
}

/// `AppState`-owned authentication & session gate replacing process-global singletons.
#[derive(Clone, Debug)]
pub struct AuthSessionGate {
    pub login_attempts: Arc<std::sync::Mutex<HashMap<LoginAttemptKey, LoginAttemptEntry>>>,
    pub pbkdf2_semaphore: Arc<tokio::sync::Semaphore>,
    pub oidc_pending_states: Arc<std::sync::Mutex<HashMap<String, OidcPendingState>>>,
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
            pbkdf2_semaphore: Arc::new(tokio::sync::Semaphore::new(permits.max(1))),
            oidc_pending_states: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub fn pbkdf2_semaphore(&self) -> &tokio::sync::Semaphore {
        &self.pbkdf2_semaphore
    }

    pub fn check_login_rate_limited(&self, scope_key: &str, user_key: &str) -> bool {
        let now = Instant::now();
        let mut guard = self
            .login_attempts
            .lock()
            .expect("login rate limiter poisoned");
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
        let mut guard = self
            .login_attempts
            .lock()
            .expect("login rate limiter poisoned");
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
            .expect("login rate limiter poisoned");
        guard.remove(&(scope_key.to_string(), user_key.to_ascii_lowercase()));
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
            .expect("oidc pending states mutex poisoned");
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
            .expect("oidc pending states mutex poisoned");
        guard.retain(|_, entry| now.duration_since(entry.created_at) < OIDC_PENDING_STATE_TTL);
        guard.remove(state_key)
    }

    #[allow(clippy::result_large_err)]
    pub async fn authenticate_read(
        &self,
        state: &AppState,
        jar: &CookieJar,
        headers: &HeaderMap,
    ) -> Result<CallerPrincipal, Response> {
        authenticate_read(state, jar, headers).await
    }

    #[allow(clippy::result_large_err)]
    pub async fn authorize_admin_read(
        &self,
        state: &AppState,
        jar: &CookieJar,
        headers: &HeaderMap,
    ) -> Result<CallerPrincipal, Response> {
        authorize_admin_read(state, jar, headers).await
    }

    #[allow(clippy::result_large_err)]
    pub async fn authorize_mutation(
        &self,
        state: &AppState,
        jar: &CookieJar,
        uri: &Uri,
        headers: &HeaderMap,
        require_admin: bool,
        target: AccessTarget<'_>,
    ) -> Result<CallerPrincipal, Response> {
        authorize_mutation(state, jar, uri, headers, require_admin, target).await
    }

    #[allow(clippy::result_large_err)]
    pub async fn authorize_global_admin_mutation(
        &self,
        state: &AppState,
        jar: &CookieJar,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Result<CallerPrincipal, Response> {
        authorize_global_admin_mutation(state, jar, uri, headers).await
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

/// Authenticates a read caller and resolves their effective camera scope (restricting
/// `"metrics"` tokens to `Some(Vec::new())`).
#[allow(clippy::result_large_err)]
pub async fn authenticate_read(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Result<CallerPrincipal, Response> {
    let cfg = state.config.read().await;
    if !cfg.auth.enabled {
        return Ok(CallerPrincipal {
            username: "admin".to_string(),
            role: "admin".to_string(),
            allowed_cameras: None,
        });
    }
    if let Some((user, role)) = extract_authenticated_user_with_cfg(state, jar, headers, &cfg.auth)
    {
        let allowed_cameras = if role.trim().eq_ignore_ascii_case("metrics") {
            Some(Vec::new())
        } else {
            resolve_user_allowed_cameras(state, jar, headers, &cfg.auth, &user, &role)
        };
        return Ok(CallerPrincipal {
            username: user,
            role,
            allowed_cameras,
        });
    }
    let has_supplied_credentials = headers.contains_key(header::AUTHORIZATION)
        || jar.get(&cfg.auth.cookie_name).is_some()
        || (cfg.auth.proxy.enabled && extract_trusted_proxy_peer_ip(headers).is_some())
        || is_external_public_peer_ip(headers);
    if has_supplied_credentials {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "success": false, "message": "Unauthorized" })),
        )
            .into_response())
    } else {
        Ok(CallerPrincipal {
            username: "admin".to_string(),
            role: "admin".to_string(),
            allowed_cameras: None,
        })
    }
}

/// Enforces that a read caller on an admin-only endpoint holds the unrestricted `admin` role.
#[allow(clippy::result_large_err)]
pub async fn authorize_admin_read(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Result<CallerPrincipal, Response> {
    let principal = authenticate_read(state, jar, headers).await?;
    if !principal.is_admin() || principal.camera_scope().is_restricted() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Admin role required"
            })),
        )
            .into_response());
    }
    Ok(principal)
}

/// Verifies CSRF origin, mutation role / admin requirement, and target camera access
/// (`Global`, `Camera`, `EventId`, `ReviewId`, or `DatasetSampleId`) in one call.
#[allow(clippy::result_large_err)]
pub async fn authorize_mutation(
    state: &AppState,
    jar: &CookieJar,
    uri: &Uri,
    headers: &HeaderMap,
    require_admin: bool,
    target: AccessTarget<'_>,
) -> Result<CallerPrincipal, Response> {
    if !verify_cookie_csrf_origin(headers, uri) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response());
    }

    let principal = {
        let cfg = state.config.read().await;
        if !cfg.auth.enabled {
            CallerPrincipal {
                username: "admin".to_string(),
                role: "admin".to_string(),
                allowed_cameras: None,
            }
        } else {
            let has_creds = headers.contains_key(header::AUTHORIZATION)
                || jar.get(&cfg.auth.cookie_name).is_some()
                || (cfg.auth.proxy.enabled && extract_trusted_proxy_peer_ip(headers).is_some())
                || is_external_public_peer_ip(headers);
            if !has_creds {
                CallerPrincipal {
                    username: "admin".to_string(),
                    role: "admin".to_string(),
                    allowed_cameras: None,
                }
            } else {
                let Some((user, role)) =
                    extract_authenticated_user_with_cfg(state, jar, headers, &cfg.auth)
                else {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "success": false,
                            "message": if require_admin {
                                "Admin role required"
                            } else {
                                "Insufficient role for mutation"
                            }
                        })),
                    )
                        .into_response());
                };
                let allowed_cameras = if role.trim().eq_ignore_ascii_case("metrics") {
                    Some(Vec::new())
                } else {
                    resolve_user_allowed_cameras(state, jar, headers, &cfg.auth, &user, &role)
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
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": if require_admin {
                    "Admin role required"
                } else {
                    "Insufficient role for mutation"
                }
            })),
        )
            .into_response());
    }

    if require_admin && !principal.is_admin() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Admin role required" })),
        )
            .into_response());
    }

    if principal.camera_scope().is_restricted() {
        match target {
            AccessTarget::Global => {}
            AccessTarget::Camera(cam) => {
                if !principal.allows_camera(cam) {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "success": false,
                            "message": format!("Camera '{cam}' is not permitted for this user")
                        })),
                    )
                        .into_response());
                }
            }
            AccessTarget::EventId(event_id) => {
                let permitted = state
                    .db
                    .get_event(event_id)
                    .ok()
                    .flatten()
                    .and_then(|ev| ev["camera"].as_str().map(|c| principal.allows_camera(c)))
                    .unwrap_or(false);
                if !permitted {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "success": false,
                            "message": "Camera is not permitted for this user"
                        })),
                    )
                        .into_response());
                }
            }
            AccessTarget::ReviewId(review_id) => {
                let permitted = state
                    .db
                    .get_review_by_id(review_id)
                    .ok()
                    .flatten()
                    .is_some_and(|r| principal.allows_camera(&r.camera));
                if !permitted {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "success": false,
                            "message": "Camera is not permitted for this user"
                        })),
                    )
                        .into_response());
                }
            }
            AccessTarget::DatasetSampleId(sample_id) => {
                let permitted = state
                    .db
                    .get_dataset_sample(sample_id)
                    .ok()
                    .flatten()
                    .is_some_and(|s| principal.allows_camera(&s.camera));
                if !permitted {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "success": false,
                            "message": "Camera is not permitted for this user"
                        })),
                    )
                        .into_response());
                }
            }
        }
    }

    Ok(principal)
}

/// Verifies CSRF origin and requires an unrestricted (`allowed_cameras` is `None` or `"*"`)
/// `admin` role for system-wide configuration, user, and API token mutations.
#[allow(clippy::result_large_err)]
pub async fn authorize_global_admin_mutation(
    state: &AppState,
    jar: &CookieJar,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<CallerPrincipal, Response> {
    let principal =
        authorize_mutation(state, jar, uri, headers, true, AccessTarget::Global).await?;
    if principal.camera_scope().is_restricted() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "message": "Admin role required"
            })),
        )
            .into_response());
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
        #[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
        const O_NOFOLLOW_FLAG: i32 = 0o100000;
        #[cfg(all(
            target_os = "linux",
            not(any(target_arch = "arm", target_arch = "aarch64"))
        ))]
        const O_NOFOLLOW_FLAG: i32 = 0o400000;
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd"
        ))]
        const O_NOFOLLOW_FLAG: i32 = 0x0100;
        #[cfg(all(
            unix,
            not(any(
                target_os = "linux",
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd"
            ))
        ))]
        const O_NOFOLLOW_FLAG: i32 = 0;

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
                .custom_flags(O_NOFOLLOW_FLAG)
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
        "/api/version"
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
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn validate_go2rtc_query_streams(
    query: Option<&str>,
    allowed_set: Option<&[String]>,
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
            if !is_safe_go2rtc_stream_name(&v) {
                return Err("Invalid stream identifier");
            }
            let base_cam = v.strip_suffix("_sub").unwrap_or(&v).trim();
            if base_cam.is_empty() {
                return Err("Invalid stream identifier");
            }
            if let Some(allowed) = allowed_set
                && !allowed.iter().any(|a| a == base_cam || a == "*")
            {
                return Err("Stream camera is not permitted for this user");
            }
            query_cams.push(base_cam.to_string());
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
    resolve_static_media_camera_with_keys(state, path, &camera_keys)
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
            return Some(cam.to_string());
        }
        if is_share_artifact {
            if let Ok(Some(ev)) = state.db.get_event(exp_id)
                && let Some(cam) = ev["camera"].as_str()
            {
                return Some(cam.to_string());
            }
            if let Ok(Some(rev)) = state.db.get_review_by_id(exp_id) {
                return Some(rev.camera);
            }
            return None;
        }
        if !fname.is_empty()
            && let Ok(exports) = state.db.query_exports()
        {
            let suffix = format!("/{fname}");
            for exp in exports {
                if exp["video_path"]
                    .as_str()
                    .is_some_and(|vp| vp == fname || vp.ends_with(&suffix))
                    && let Some(cam) = exp["camera"].as_str()
                    && !cam.is_empty()
                {
                    return Some(cam.to_string());
                }
            }
        }
        return exp_id
            .rsplitn(3, '_')
            .nth(2)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
    }
    if let Some(rest) = path.strip_prefix("/recordings/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 3 && parts[0].len() == 10 && parts[0].chars().nth(4) == Some('-') {
            return Some(parts[2].to_string());
        }
        if !parts.is_empty() {
            return Some(parts[0].to_string());
        }
        return None;
    }
    if let Some(rest) = path.strip_prefix("/clips/") {
        let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        if (parts.first() == Some(&"previews") || parts.first() == Some(&"preview_frames"))
            && parts.len() >= 2
        {
            return Some(parts[1].to_string());
        }
        if parts.first() == Some(&"dataset") && parts.len() == 2 {
            let sample_id = parts[1]
                .rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or(parts[1]);
            if let Ok(Some(sample)) = state.db.get_dataset_sample(sample_id)
                && !sample.camera.trim().is_empty()
            {
                return Some(sample.camera);
            }
            return None;
        }
        if parts.first() == Some(&"faces") && parts.len() == 3 {
            let sample_id = parts[2]
                .rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or(parts[2]);
            if let Ok(Some(ev)) = state.db.get_event(sample_id)
                && let Some(cam) = ev["camera"].as_str().filter(|s| !s.trim().is_empty())
            {
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
                    return Some(rev.camera);
                }
                for (idx, _) in after_thumb.match_indices('-') {
                    let candidate_rev_id = &after_thumb[idx + 1..];
                    if !candidate_rev_id.is_empty()
                        && let Ok(Some(rev)) = state.db.get_review_by_id(candidate_rev_id)
                    {
                        return Some(rev.camera);
                    }
                }
                let mut matched_cam: Option<&str> = None;
                for cam in camera_keys {
                    let prefix = format!("{cam}-");
                    if after_thumb.starts_with(&prefix)
                        && matched_cam.is_none_or(|prev| cam.len() > prev.len())
                    {
                        matched_cam = Some(cam.as_str());
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
            let raw_stem = parts[0]
                .rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or(parts[0]);
            let (is_share_artifact, stem) = strip_share_artifact_prefix(raw_stem);
            if !is_share_artifact
                && let Some(latest_cam) = stem.strip_prefix("latest_").filter(|s| !s.is_empty())
            {
                return Some(latest_cam.to_string());
            }
            let clean_stem = stem.strip_suffix("-clean").unwrap_or(stem);
            for candidate in [clean_stem, stem] {
                if let Ok(Some(ev)) = state.db.get_event(candidate)
                    && let Some(cam) = ev["camera"].as_str()
                {
                    return Some(cam.to_string());
                }
                if let Ok(Some(exp)) = state.db.get_export_by_id(candidate)
                    && let Some(cam) = exp["camera"].as_str()
                {
                    return Some(cam.to_string());
                }
                if let Ok(Some(rev)) = state.db.get_review_by_id(candidate) {
                    return Some(rev.camera);
                }
            }
            if is_share_artifact {
                return None;
            }
            for (idx, _) in clean_stem.match_indices('-') {
                let ev_part = &clean_stem[idx + 1..];
                if !ev_part.is_empty() {
                    if let Ok(Some(ev)) = state.db.get_event(ev_part)
                        && let Some(ev_cam) = ev["camera"].as_str()
                    {
                        return Some(ev_cam.to_string());
                    }
                    if let Ok(Some(exp)) = state.db.get_export_by_id(ev_part)
                        && let Some(exp_cam) = exp["camera"].as_str()
                    {
                        return Some(exp_cam.to_string());
                    }
                    if let Ok(Some(rev)) = state.db.get_review_by_id(ev_part) {
                        return Some(rev.camera);
                    }
                }
            }
            let mut matched_cam: Option<&str> = None;
            for cam in camera_keys {
                let prefix = format!("{cam}-");
                if clean_stem.starts_with(&prefix)
                    && matched_cam.is_none_or(|prev| cam.len() > prev.len())
                {
                    matched_cam = Some(cam.as_str());
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
    let (auth_cfg, camera_keys, active_masks) = {
        let cfg = state.config.read().await;
        let keys: Vec<String> = cfg.cameras.keys().cloned().collect();
        let masks: std::collections::BTreeMap<String, Value> = cfg
            .cameras
            .iter()
            .filter(|(_, c)| crate::has_active_privacy_mask(Some(&c.privacy_mask)))
            .map(|(k, c)| {
                (
                    k.clone(),
                    crate::normalize_privacy_mask_value_for_resolution(
                        &c.privacy_mask,
                        c.detect.width.max(1),
                        c.detect.height.max(1),
                    ),
                )
            })
            .collect();
        (cfg.auth.clone(), keys, masks)
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
                && (path == "/api/stats" || path == "/api/stats/history" || path == "/api/metrics");
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
                || !(path.starts_with("/api/") || path == "/api"))
        {
            state.log_hub.push_nginx(&method, &path, 403);
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "message": "Token scope 'read_only' only permits GET /api/*" })),
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
        if (path.starts_with("/live/") || path == "/live" || path == "/api/go2rtc/webrtc")
            && let Err(msg) =
                validate_go2rtc_query_streams(req.uri().query(), allowed_set_opt.as_deref())
        {
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
            if !is_safe_go2rtc_stream_name(&decoded_seg) {
                state.log_hub.push_nginx(&method, &path, 400);
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "message": "Invalid stream identifier" })),
                )
                    .into_response();
            }
            let base_cam = decoded_seg
                .strip_suffix("_sub")
                .unwrap_or(&decoded_seg)
                .trim();
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
                resolve_static_media_camera_with_keys(&state, &path, &camera_keys)
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
                        if dk == "src" || dk == "dst" {
                            Some(dv.strip_suffix("_sub").unwrap_or(&dv).to_string())
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
                ) && !(first == "recordings" && after_first == "summary");
                if is_camera_subroute {
                    Some(first)
                } else if first == "preview" {
                    if after_first.contains("/start/") {
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

    let mut active_clip_mask: Option<Value> = None;
    if !active_masks.is_empty()
        && (path == "/recordings"
            || path.starts_with("/recordings/")
            || path.starts_with("/exports/share_clip_")
            || path == "/clips"
            || path.starts_with("/clips/"))
    {
        match resolve_static_media_camera_with_keys(&state, &path, &camera_keys) {
            Some(cam) => {
                if let Some(mask) = active_masks.get(&cam) {
                    let path_lc = path.to_ascii_lowercase();
                    let is_clip_image = path.starts_with("/clips/")
                        && !path.starts_with("/clips/faces/")
                        && (path_lc.ends_with(".jpg")
                            || path_lc.ends_with(".jpeg")
                            || path_lc.ends_with(".webp")
                            || path_lc.ends_with(".png"));
                    if !is_clip_image {
                        state.log_hub.push_nginx(&method, &path, 404);
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    active_clip_mask = Some(mask.clone());
                }
            }
            None => {
                state.log_hub.push_nginx(&method, &path, 404);
                return StatusCode::NOT_FOUND.into_response();
            }
        }
    }

    if active_clip_mask.is_some() {
        req.headers_mut().remove(header::RANGE);
        req.headers_mut().remove(header::IF_RANGE);
        req.headers_mut().remove(header::IF_NONE_MATCH);
        req.headers_mut().remove(header::IF_MODIFIED_SINCE);
    }

    let mut resp = next.run(req).await;
    if let Some(mask) = active_clip_mask {
        if method == "HEAD" {
            resp.headers_mut().remove(header::ETAG);
            resp.headers_mut().remove(header::LAST_MODIFIED);
        } else if resp.status() == StatusCode::OK || resp.status() == StatusCode::PARTIAL_CONTENT {
            let prefer_jpeg = {
                let p_lc = path.to_ascii_lowercase();
                p_lc.ends_with(".jpg") || p_lc.ends_with(".jpeg")
            };
            if let Ok(raw_bytes) = axum::body::to_bytes(resp.into_body(), 32 * 1024 * 1024).await {
                let (redacted_bytes, mime) = crate::redact_image_bytes_with_privacy_mask_async(
                    raw_bytes.to_vec(),
                    Some(mask),
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
    let cfg = state.config.read().await;
    if let Some((user, role)) =
        extract_authenticated_user_with_cfg(&state, &jar, &headers, &cfg.auth)
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

pub async fn api_auth_providers(State(state): State<AppState>) -> Json<Value> {
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
    let x_forwarded_host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty());
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
    let raw_proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .filter(|s| !s.is_empty())
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
    let cookie_secure = cfg.auth.cookie_secure;
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

    let (state_str, verifier, challenge) = generate_pkce_pair();
    let base_url = resolve_external_base_url(&headers, &uri);
    let redirect_uri = format!("{base_url}/api/auth/oidc/callback");

    state.auth_gate.store_oidc_pending_state(
        state_str.clone(),
        verifier.clone(),
        redirect_uri.clone(),
    );

    // Discover authorization_endpoint or fall back to standard /authorize
    let mut auth_endpoint = format!("{issuer}/authorize");
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
        && let Some(ep) = disc.get("authorization_endpoint").and_then(|v| v.as_str())
        && is_valid_oidc_discovered_endpoint(ep, &issuer)
    {
        auth_endpoint = ep.trim().to_string();
    }

    let scope_str = if oidc.scopes.is_empty() {
        "openid profile email groups".to_string()
    } else {
        oidc.scopes.join(" ")
    };

    let redirect_target = format!(
        "{auth_endpoint}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={state_str}&code_challenge={challenge}&code_challenge_method=S256",
        urlencoding_simple(&client_id),
        urlencoding_simple(&redirect_uri),
        urlencoding_simple(&scope_str),
    );

    let mut pkce_cookie = Cookie::new(
        "ninjanvr_oidc_pkce",
        format!("{state_str}:{verifier}:{redirect_uri}"),
    );
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
    let Some((expected_state, rest_cookie)) = pkce_cookie.value().split_once(':') else {
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

    let (oidc_cfg, cookie_name, cookie_secure, session_len) = {
        let cfg = state.config.read().await;
        (
            cfg.auth.oidc.clone(),
            cfg.auth.cookie_name.clone(),
            cfg.auth.cookie_secure,
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
    let client_secret = std::env::var(&oidc_cfg.client_secret_env).unwrap_or_default();

    let expected_callback_uri = {
        let base_url = resolve_external_base_url(&headers, &uri);
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
    let mut disc_loaded = false;
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
                disc_loaded = true;
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
                let mut merged_claims = json!({});
                if let Some(id_token) = tok_json.get("id_token").and_then(|v| v.as_str()) {
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
                            if let Some(ref req_kid) = jwt_hdr.kid
                                && let Some(ref key_kid) = jwk.common.key_id
                                && req_kid != key_kid
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
                    } else if disc_loaded {
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
                    merged_claims = claims_json;
                }
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
                    let (canonical_username, role, allowed_cameras) =
                        match state.db.get_user(&username) {
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
                    let session_token = jsonwebtoken::encode(
                        &Header::new(Algorithm::HS256),
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
            cfg.auth.cookie_secure,
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

    let rate_scope = state.jwt_secret.clone();
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

    let Ok(_pbkdf2_permit) = state.auth_gate.pbkdf2_semaphore.acquire().await else {
        return (
            jar,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "message": "Authentication worker pool unavailable" })),
            )
                .into_response(),
        );
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

    if db_user.role == "admin" {
        state.config.write().await.auth.admin_first_time_login = false;
    }

    let now = unix_now();
    let clamped_session_len = session_len.clamp(60, 86_400 * 365);
    let claims = JwtClaims {
        sub: db_user.username.clone(),
        role: db_user.role.clone(),
        allowed_cameras: Some(db_user.allowed_cameras.clone()),
        iat: now,
        exp: now.saturating_add(clamped_session_len),
    };

    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
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
    if method != axum::http::Method::GET
        && method != axum::http::Method::HEAD
        && !verify_cookie_csrf_origin(&headers, &uri)
    {
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
                if let Ok(Some(db_u)) = state.db.get_user(remote_user) {
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
        if let Ok(Some((token_name, scope))) =
            state.db.verify_api_token(&token_str, unix_now() as f64)
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
    if is_reserved_token_principal_prefix(&decoded.claims.sub) {
        return None;
    }

    let (canonical_sub, role) = match state.db.get_user(&decoded.claims.sub) {
        Ok(Some(db_u)) => (
            db_u.username,
            cap_external_role_by_db(&decoded.claims.role, &db_u.role),
        ),
        Ok(None) => {
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
    match state.db.get_user(user) {
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
    let cfg = state.config.read().await;
    if let Some((user, role)) = extract_authenticated_user_with_cfg(state, jar, headers, &cfg.auth)
    {
        if cfg.auth.enabled && role.trim().eq_ignore_ascii_case("metrics") {
            return Some(Vec::new());
        }
        return resolve_user_allowed_cameras(state, jar, headers, &cfg.auth, &user, &role);
    }
    let has_supplied_credentials = headers.contains_key(header::AUTHORIZATION)
        || jar.get(&cfg.auth.cookie_name).is_some()
        || (cfg.auth.proxy.enabled && extract_trusted_proxy_peer_ip(headers).is_some())
        || is_external_public_peer_ip(headers);
    if cfg.auth.enabled && has_supplied_credentials {
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
    let cfg = state.config.read().await;
    if !cfg.auth.enabled {
        return true;
    }
    let has_creds = headers.contains_key(header::AUTHORIZATION)
        || jar.get(&cfg.auth.cookie_name).is_some()
        || (cfg.auth.proxy.enabled && extract_trusted_proxy_peer_ip(headers).is_some())
        || is_external_public_peer_ip(headers);
    if !has_creds {
        return true;
    }
    let Some((_user, role)) = extract_authenticated_user_with_cfg(state, jar, headers, &cfg.auth)
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
    let Ok(Some(item)) = state.db.get_review_by_id(id) else {
        return Err(ReviewMediaDenied::NotFound);
    };
    if let Some(allowed) = effective_caller_allowed_cameras(state, jar, headers).await
        && !allowed.iter().any(|a| a == &item.camera)
    {
        return Err(ReviewMediaDenied::Forbidden);
    }
    let privacy_mask_active = {
        let cfg = state.config.read().await;
        cfg.cameras
            .get(&item.camera)
            .is_some_and(|c| has_active_privacy_mask(Some(&c.privacy_mask)))
    };
    if privacy_mask_active {
        return Err(ReviewMediaDenied::NotFound);
    }
    Ok(item)
}

pub async fn verify_caller_can_mutate(
    state: &AppState,
    jar: &CookieJar,
    headers: &HeaderMap,
) -> Option<Response> {
    let auth_cfg = state.config.read().await.auth.clone();
    if auth_cfg.enabled
        && let Some((_, role)) = extract_authenticated_user_with_cfg(state, jar, headers, &auth_cfg)
    {
        let role_lc = role.trim().to_ascii_lowercase();
        if matches!(role_lc.as_str(), "viewer" | "read_only" | "metrics") {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "success": false,
                        "message": "Insufficient role for mutation"
                    })),
                )
                    .into_response(),
            );
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
) -> Option<Response> {
    if !verify_cookie_csrf_origin(headers, uri) {
        return Some(
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
    let auth_cfg = state.config.read().await.auth.clone();
    if auth_cfg.enabled
        && let Some((user, role)) =
            extract_authenticated_user_with_cfg(state, jar, headers, &auth_cfg)
    {
        let role_lc = role.to_ascii_lowercase();
        if matches!(role_lc.as_str(), "viewer" | "read_only" | "metrics") {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "success": false,
                        "message": "Insufficient role for camera mutation"
                    })),
                )
                    .into_response(),
            );
        }
        if role_lc != "admin"
            && let Some(allowed) =
                resolve_user_allowed_cameras(state, jar, headers, &auth_cfg, &user, &role)
            && !allowed.iter().any(|a| a == camera || a == "*")
        {
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "success": false,
                        "message": format!("Camera '{camera}' is not permitted for this user")
                    })),
                )
                    .into_response(),
            );
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
        .map(|(_, hp)| hp.trim())
        .unwrap_or(raw_authority);
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
    if clean_topic == "events" || clean_topic == "reviews" {
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

pub fn reverify_ws_caller_access(
    state: &AppState,
    caller_user: Option<&str>,
    had_db_user_at_connect: bool,
    fallback_role: &str,
    fallback_allowed: &Option<Vec<String>>,
) -> Option<(String, Option<Vec<String>>)> {
    let Some(user) = caller_user else {
        return Some((fallback_role.to_string(), fallback_allowed.clone()));
    };
    if let Some(token_name) = user.strip_prefix("token:") {
        if !state
            .db
            .is_api_token_active_by_name(token_name.trim(), unix_now() as f64)
            .unwrap_or(false)
        {
            return None;
        }
        return Some((fallback_role.to_string(), fallback_allowed.clone()));
    }
    match state.db.get_user(user) {
        Ok(Some(db_u)) => {
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
