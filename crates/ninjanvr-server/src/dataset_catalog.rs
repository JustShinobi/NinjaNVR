#![forbid(unsafe_code)]
//! Deepened dataset curation & Vision AI judgment catalog (`DatasetCatalog`).
//!
//! ## Design-It-Twice Comparison (`/codebase-design`)
//!
//! - **Design A — Unified `VisionAiJudge` Port + `DatasetArtifactStore` + `DatasetCatalog` (Selected):**
//!   Separates external Vision AI evaluation (`VisionAiJudge` + `VisionAiTransport` port with
//!   `ReqwestVisionAiTransport` and in-memory `FakeVisionAiTransport` adapters) from local media
//!   filesystem persistence (`DatasetArtifactStore`: single-pass privacy redaction + symmetric
//!   3-path artifact resolution and deletion) and instance-scoped concurrency (`DatasetCatalog`
//!   owning `batch_auto_annotate_semaphore` and `export_zip_semaphore`).
//! - **Design B — Monolithic `DatasetFacade` or Process-Global Statics (Rejected):**
//!   Couples `reqwest::Client`, `NinjaDatabase`, `RwLock<NinjaConfig>`, and `static LazyLock<Semaphore>`
//!   singletons inside `lib.rs`, leaking concurrency state across test instances.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Json, Response};
use axum_extra::extract::cookie::CookieJar;
use base64::Engine;
use image::{Rgb, RgbImage};
use ninjanvr_config::{GenAiConfig, NinjaConfig};
use ninjanvr_db::{DatasetAnnotationBox, DatasetSampleDto, NinjaDatabase};
use ninjanvr_vision::{PolygonMask, apply_privacy_mask_rgb, encode_image_webp};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{RwLock, Semaphore};
use tracing::warn;

use crate::{
    AppState, atomic_write_within_media_subdir, caller_can_mutate, camera_privacy_mask_value,
    decode_bounded_snapshot_rgb, effective_caller_allowed_cameras, ensure_clips_artifact_path,
    ensure_dataset_sample_path, has_active_privacy_mask, is_safe_id,
    read_bounded_non_symlink_bytes_sync, redact_image_bytes_with_privacy_mask,
    redact_image_bytes_with_privacy_mask_async, verify_cookie_csrf_origin,
};

async fn read_bounded_non_symlink_bytes(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_bounded_non_symlink_bytes_sync(&p, max_bytes))
        .await
        .ok()
        .flatten()
}

/// Maximum response payload size accepted from a Vision AI provider (2 MiB).
pub const MAX_GENAI_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Maximum on-disk size for a single dataset sample image (16 MiB).
pub const MAX_DATASET_SAMPLE_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum total size of an exported YOLO dataset ZIP archive (64 MiB).
pub const MAX_DATASET_EXPORT_ZIP_BYTES: usize = 64 * 1024 * 1024;

/// Maximum number of unverified samples annotated per batch auto-annotate call.
pub const MAX_BATCH_AUTO_ANNOTATE_SAMPLES: usize = 50;

/// Maximum entry count accepted when parsing a Stored ZIP archive.
pub const MAX_ZIP_ARCHIVE_ENTRIES: usize = 4096;

/// Maximum cumulative extracted payload size accepted when parsing a Stored ZIP archive (64 MiB).
pub const MAX_ZIP_EXTRACTED_BYTES: usize = 64 * 1024 * 1024;

// ============================================================================
// Part 1 — VisionAiJudge & Transport Seam (Ports & Adapters)
// ============================================================================

/// Provider-agnostic HTTP request envelope produced by [`build_vision_ai_request`].
#[derive(Debug, Clone, PartialEq)]
pub struct VisionAiProviderRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    pub provider: String,
    pub model: String,
}

/// Port for executing Vision AI requests against either a live HTTP backend or an in-memory fake.
pub trait VisionAiTransport: Send + Sync {
    fn send_json(
        &self,
        req: &VisionAiProviderRequest,
        timeout: Duration,
    ) -> impl Future<Output = Result<Value, String>> + Send;
}

/// Production HTTP adapter backed by `reqwest` with redirect suppression and bounded body reads.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReqwestVisionAiTransport;

impl ReqwestVisionAiTransport {
    pub const fn new() -> Self {
        Self
    }
}

impl VisionAiTransport for ReqwestVisionAiTransport {
    async fn send_json(
        &self,
        req: &VisionAiProviderRequest,
        timeout: Duration,
    ) -> Result<Value, String> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("failed to build Vision AI HTTP client: {e}"))?;

        let mut builder = client.post(&req.url).json(&req.body);
        for (name, value) in &req.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }

        let provider_label = match req.provider.as_str() {
            "openai" => "OpenAI Vision API request failed",
            "ollama" => "Ollama Vision API request failed",
            _ => "Gemini Vision API request failed",
        };

        let mut resp = builder
            .send()
            .await
            .map_err(|e| format!("{provider_label}: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Vision AI HTTP {}", resp.status()));
        }

        if let Some(len) = resp.content_length()
            && len > MAX_GENAI_RESPONSE_BYTES as u64
        {
            return Err(format!(
                "Vision AI response body too large ({len} bytes > {MAX_GENAI_RESPONSE_BYTES} bytes)"
            ));
        }

        let mut buf = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("failed to read Vision AI response body: {e}"))?
        {
            if buf.len().saturating_add(chunk.len()) > MAX_GENAI_RESPONSE_BYTES {
                return Err(format!(
                    "Vision AI response body exceeded maximum limit of {MAX_GENAI_RESPONSE_BYTES} bytes"
                ));
            }
            buf.extend_from_slice(&chunk);
        }

        serde_json::from_slice(&buf)
            .map_err(|e| format!("failed to decode Vision AI JSON response: {e}"))
    }
}

/// In-memory `VisionAiTransport` adapter for deterministic, zero-network unit tests.
#[derive(Debug, Default)]
pub struct FakeVisionAiTransport {
    responses: Mutex<VecDeque<Result<Value, String>>>,
    recorded_requests: Mutex<Vec<VisionAiProviderRequest>>,
    recorded_timeouts: Mutex<Vec<Duration>>,
}

impl FakeVisionAiTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_response(response: Value) -> Self {
        Self::with_responses([Ok(response)])
    }

    pub fn with_error(err: impl Into<String>) -> Self {
        Self::with_responses([Err(err.into())])
    }

    pub fn with_responses(responses: impl IntoIterator<Item = Result<Value, String>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            recorded_requests: Mutex::new(Vec::new()),
            recorded_timeouts: Mutex::new(Vec::new()),
        }
    }

    pub fn push_response(&self, response: Result<Value, String>) {
        if let Ok(mut guard) = self.responses.lock() {
            guard.push_back(response);
        }
    }

    pub fn recorded_requests(&self) -> Vec<VisionAiProviderRequest> {
        self.recorded_requests
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn recorded_timeouts(&self) -> Vec<Duration> {
        self.recorded_timeouts
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

impl VisionAiTransport for FakeVisionAiTransport {
    async fn send_json(
        &self,
        req: &VisionAiProviderRequest,
        timeout: Duration,
    ) -> Result<Value, String> {
        if let Ok(mut reqs) = self.recorded_requests.lock() {
            reqs.push(req.clone());
        }
        if let Ok(mut timeouts) = self.recorded_timeouts.lock() {
            timeouts.push(timeout);
        }
        let next = self
            .responses
            .lock()
            .map_err(|_| "FakeVisionAiTransport mutex poisoned".to_string())?
            .pop_front();
        next.unwrap_or_else(|| {
            Err("FakeVisionAiTransport has no queued response remaining".to_string())
        })
    }
}

/// Executes `req` through `transport` and extracts the normalized response text across
/// Gemini, OpenAI, and Ollama payload schemas.
pub async fn execute_vision_ai_prompt<T: VisionAiTransport>(
    transport: &T,
    req: &VisionAiProviderRequest,
    timeout: Duration,
) -> Result<String, String> {
    let body_json = transport.send_json(req, timeout).await?;
    extract_vision_ai_response_text(&body_json).ok_or_else(|| {
        format!(
            "unrecognized Vision AI response payload from provider '{}': {body_json}",
            req.provider
        )
    })
}

/// Computes the bounded timeout (`[5s, 60s]`, defaulting to `30s` when `<= 5s`) for a `GenAiConfig`.
pub fn effective_vision_ai_timeout(genai_cfg: &GenAiConfig) -> Duration {
    let ms = if genai_cfg.timeout_ms <= 5_000 {
        30_000
    } else {
        genai_cfg.timeout_ms.clamp(5_000, 60_000)
    };
    Duration::from_millis(ms)
}

pub fn is_allowed_genai_api_key_env(name: &str) -> bool {
    let trimmed = name.trim();
    matches!(
        trimmed,
        "GEMINI_API_KEY"
            | "OPENAI_API_KEY"
            | "OLLAMA_API_KEY"
            | "GOOGLE_API_KEY"
            | "ANTHROPIC_API_KEY"
    ) || trimmed.starts_with("NINJANVR_GENAI_")
        || trimmed.starts_with("NINJANVR_TEST_GENAI_")
}

pub fn is_env_var_identifier(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.starts_with('$') {
        return true;
    }
    is_allowed_genai_api_key_env(trimmed)
        || (!trimmed.is_empty()
            && trimmed.contains('_')
            && trimmed
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
}

pub fn resolve_safe_genai_api_key(genai: &GenAiConfig) -> String {
    if let Some(ref key) = genai.api_key {
        let trimmed = key.trim();
        if !trimmed.is_empty() && trimmed != "***" && trimmed != "*" {
            if let Some(after_dollar) = trimmed.strip_prefix('$') {
                let env_name = after_dollar
                    .trim()
                    .strip_prefix('{')
                    .and_then(|s| s.strip_suffix('}'))
                    .unwrap_or(after_dollar.trim())
                    .trim();
                if is_allowed_genai_api_key_env(env_name)
                    && let Ok(val) = std::env::var(env_name)
                    && !val.trim().is_empty()
                {
                    return val.trim().to_string();
                }
                return String::new();
            }
            if is_env_var_identifier(trimmed) || std::env::var(trimmed).is_ok() {
                if is_allowed_genai_api_key_env(trimmed)
                    && let Ok(val) = std::env::var(trimmed)
                    && !val.trim().is_empty()
                {
                    return val.trim().to_string();
                }
                return String::new();
            }
            return trimmed.to_string();
        }
    }
    let env_var = genai.api_key_env.trim();
    if is_allowed_genai_api_key_env(env_var)
        && let Ok(val) = std::env::var(env_var)
        && !val.trim().is_empty()
    {
        return val.trim().to_string();
    }
    String::new()
}

pub fn is_safe_genai_model_name(raw: &str) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 128
        && !trimmed.contains("..")
        && !trimmed.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '?' | '#' | '@')
        })
}

pub fn is_disallowed_genai_ipv4(v4: std::net::Ipv4Addr) -> bool {
    let octs = v4.octets();
    v4.is_unspecified()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || (octs[0] == 169 && octs[1] == 254)
        || octs == [100, 100, 100, 200]
        || octs == [192, 0, 0, 192]
}

pub fn is_disallowed_ssrf_ipv6(v6: std::net::Ipv6Addr, block_private_v4: bool) -> bool {
    let segs = v6.segments();
    let siit_v4 = if segs[0..6] == [0, 0, 0, 0, 0xffff, 0] {
        Some(std::net::Ipv4Addr::new(
            (segs[6] >> 8) as u8,
            segs[6] as u8,
            (segs[7] >> 8) as u8,
            segs[7] as u8,
        ))
    } else {
        None
    };
    let check_v4 = |v4: std::net::Ipv4Addr| -> bool {
        is_disallowed_genai_ipv4(v4) || v4.is_loopback() || (block_private_v4 && v4.is_private())
    };
    v6.is_unspecified()
        || v6.is_multicast()
        || (segs[0] & 0xffc0) == 0xfe80
        || (segs[0] & 0xfe00) == 0xfc00
        || (segs[0] == 0x0064 && segs[1] == 0xff9b)
        || segs[0] == 0x2002
        || (segs[0] == 0x2001 && (segs[1] == 0x0000 || segs[1] == 0x0db8))
        || siit_v4.is_some_and(check_v4)
        || v6.to_ipv4().is_some_and(check_v4)
        || v6.to_ipv4_mapped().is_some_and(check_v4)
}

pub fn validate_genai_base_url(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    if (!lower.starts_with("http://") && !lower.starts_with("https://"))
        || trimmed.contains('@')
        || trimmed.contains('\\')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.contains("..")
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("Disallowed or invalid GenAI base_url".to_string());
    }
    let after_scheme = trimmed.split_once("://").map(|(_, r)| r).unwrap_or("");
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if authority.is_empty() {
        return Err("Disallowed or invalid GenAI base_url".to_string());
    }
    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((ipv6_part, rest)) = after_bracket.split_once(']') else {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        };
        if ipv6_part.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        }
        let p = if rest.is_empty() {
            None
        } else if let Some(port_str) = rest.strip_prefix(':') {
            let Ok(parsed_p) = port_str.parse::<u16>() else {
                return Err("Disallowed or invalid GenAI base_url".to_string());
            };
            Some(parsed_p)
        } else {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        };
        (ipv6_part, p)
    } else if let Some((h, p_str)) = authority.rsplit_once(':') {
        if h.contains(':') {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        }
        let Ok(parsed_p) = p_str.parse::<u16>() else {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        };
        (h, Some(parsed_p))
    } else {
        (authority, None)
    };
    if matches!(port, Some(0 | 5000 | 8971 | 1984 | 8554)) {
        return Err("Disallowed or invalid GenAI base_url".to_string());
    }
    let host_lc = host.to_ascii_lowercase();
    let clean_host = host_lc.trim_end_matches('.');
    if clean_host.is_empty()
        || clean_host.contains('%')
        || clean_host == "metadata.google.internal"
        || clean_host.ends_with(".metadata.google.internal")
        || clean_host == "metadata"
        || clean_host == "metadata.azure.com"
        || clean_host.ends_with(".metadata.azure.com")
        || clean_host == "instance-data"
        || clean_host.ends_with(".nip.io")
        || clean_host.ends_with(".sslip.io")
        || clean_host.ends_with(".xip.io")
        || clean_host == "localtest.me"
        || clean_host.ends_with(".localtest.me")
        || clean_host == "lvh.me"
        || clean_host.ends_with(".lvh.me")
        || clean_host.starts_with("169.254.")
    {
        return Err("Disallowed or invalid GenAI base_url".to_string());
    }
    if clean_host.split('.').all(|part| {
        !part.is_empty()
            && (part.chars().all(|c| c.is_ascii_digit())
                || part
                    .strip_prefix("0x")
                    .or_else(|| part.strip_prefix("0X"))
                    .is_some_and(|hex| {
                        !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())
                    }))
    }) {
        let Ok(v4) = clean_host.parse::<std::net::Ipv4Addr>() else {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        };
        if clean_host != v4.to_string() || is_disallowed_genai_ipv4(v4) {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        }
    }
    if let Ok(ip) = clean_host.parse::<std::net::IpAddr>() {
        let disallowed = match ip {
            std::net::IpAddr::V4(v4) => {
                clean_host != v4.to_string() || is_disallowed_genai_ipv4(v4)
            }
            std::net::IpAddr::V6(v6) => is_disallowed_ssrf_ipv6(v6, true),
        };
        if disallowed {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        }
    }
    let Ok(parsed_url) = reqwest::Url::parse(trimmed) else {
        return Err("Disallowed or invalid GenAI base_url".to_string());
    };
    if parsed_url.query().is_some()
        || parsed_url.fragment().is_some()
        || matches!(parsed_url.port(), Some(0 | 5000 | 8971 | 1984 | 8554))
    {
        return Err("Disallowed or invalid GenAI base_url".to_string());
    }
    if let Some(url_host) = parsed_url.host_str() {
        let url_host_clean = url_host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if let Ok(v4) = url_host_clean.parse::<std::net::Ipv4Addr>()
            && (clean_host != v4.to_string() || is_disallowed_genai_ipv4(v4))
        {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        }
    } else {
        return Err("Disallowed or invalid GenAI base_url".to_string());
    }
    Ok(())
}

/// Builds a provider-specific [`VisionAiProviderRequest`] for `"openai"`, `"ollama"`, or `"gemini"` (default).
///
/// Consolidates the previously duplicated request builders in `verify_event_with_genai_components`
/// and `auto_annotate_dataset_sample`.
pub fn build_vision_ai_request(
    genai_cfg: &GenAiConfig,
    prompt: &str,
    image_b64: &str,
    mime_type: &str,
    temperature: f64,
    max_tokens: u32,
) -> Result<VisionAiProviderRequest, String> {
    let model = genai_cfg.model.trim();
    if model.is_empty() {
        return Err(
            "No Vision AI model selected. Validate API and choose a model in Settings.".to_string(),
        );
    }
    if !is_safe_genai_model_name(model) {
        return Err("Disallowed or invalid GenAI model identifier".to_string());
    }
    if let Some(base) = genai_cfg
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        validate_genai_base_url(base)?;
    }

    let raw_provider = genai_cfg.provider.trim().to_ascii_lowercase();
    let provider = match raw_provider.as_str() {
        "openai" => "openai",
        "ollama" => "ollama",
        _ => "gemini",
    };
    let api_key = resolve_safe_genai_api_key(genai_cfg);
    let safe_mime = if mime_type.trim().is_empty() {
        "image/webp"
    } else {
        mime_type.trim()
    };
    let safe_temp = if temperature.is_finite() {
        temperature.clamp(0.0, 2.0)
    } else {
        0.1
    };
    let safe_max_tokens = max_tokens.max(1);

    match provider {
        "openai" => {
            let base = genai_cfg
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/');
            let url = if base.ends_with("/chat/completions") {
                base.to_string()
            } else if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            };
            let mut headers = Vec::new();
            if !api_key.is_empty() {
                headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
            }
            let body = json!({
                "model": model,
                "temperature": safe_temp,
                "max_tokens": safe_max_tokens,
                "response_format": { "type": "json_object" },
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": prompt },
                        {
                            "type": "image_url",
                            "image_url": { "url": format!("data:{safe_mime};base64,{image_b64}") }
                        }
                    ]
                }]
            });
            Ok(VisionAiProviderRequest {
                url,
                headers,
                body,
                provider: provider.to_string(),
                model: model.to_string(),
            })
        }
        "ollama" => {
            let base = genai_cfg
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("http://127.0.0.1:11434")
                .trim_end_matches('/');
            let url = if base.ends_with("/api/generate") || base.ends_with("/api/chat") {
                base.to_string()
            } else {
                format!("{base}/api/generate")
            };
            let mut headers = Vec::new();
            if !api_key.is_empty() {
                headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
            }
            let body = json!({
                "model": model,
                "prompt": prompt,
                "images": [image_b64],
                "stream": false,
                "format": "json",
                "options": {
                    "temperature": safe_temp,
                    "num_predict": safe_max_tokens
                }
            });
            Ok(VisionAiProviderRequest {
                url,
                headers,
                body,
                provider: provider.to_string(),
                model: model.to_string(),
            })
        }
        _ => {
            let base = genai_cfg
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("https://generativelanguage.googleapis.com")
                .trim_end_matches('/');
            let url = if api_key.is_empty() {
                format!("{base}/v1beta/models/{model}:generateContent")
            } else {
                format!("{base}/v1beta/models/{model}:generateContent?key={api_key}")
            };
            let mut headers = Vec::new();
            if !api_key.is_empty() {
                headers.push(("x-goog-api-key".to_string(), api_key));
            }
            let body = json!({
                "contents": [{
                    "parts": [
                        { "text": prompt },
                        {
                            "inline_data": {
                                "mime_type": safe_mime,
                                "data": image_b64
                            }
                        }
                    ]
                }],
                "generationConfig": {
                    "responseMimeType": "application/json",
                    "temperature": safe_temp,
                    "maxOutputTokens": safe_max_tokens
                }
            });
            Ok(VisionAiProviderRequest {
                url,
                headers,
                body,
                provider: "gemini".to_string(),
                model: model.to_string(),
            })
        }
    }
}

/// Strips optional ` ```json ... ``` ` or ` ``` ... ``` ` markdown code fences around JSON output.
pub fn strip_markdown_json_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(after_open) = trimmed.strip_prefix("```") {
        let after_lang = after_open
            .strip_prefix("json")
            .or_else(|| after_open.strip_prefix("JSON"))
            .unwrap_or(after_open)
            .trim_start();
        if let Some(body) = after_lang.strip_suffix("```") {
            return body.trim();
        }
    }
    trimmed
}

/// Extracts the primary non-thought candidate text from a Gemini `generateContent` payload.
pub fn extract_gemini_candidate_text(resp_val: &Value) -> Option<&str> {
    if let Some(parts) = resp_val
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())
    {
        if let Some(text) = parts
            .iter()
            .rev()
            .filter(|p| p.get("thought").and_then(|t| t.as_bool()) != Some(true))
            .find_map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .filter(|s| !s.trim().is_empty())
            })
        {
            return Some(text);
        }
        if let Some(text) = parts.iter().rev().find_map(|p| {
            p.get("text")
                .and_then(|t| t.as_str())
                .filter(|s| !s.trim().is_empty())
        }) {
            return Some(text);
        }
    }
    resp_val
        .pointer("/candidates/0/content/parts/0/text")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
}

/// Consolidates response text extraction across:
/// - Gemini (`candidates[0].content.parts`, skipping `"thought": true` parts)
/// - OpenAI (`choices[0].message.content` string or text part array)
/// - Ollama (`response` string/object or `message.content`)
/// - Direct JSON verdict/annotation objects or arrays
pub fn extract_vision_ai_response_text(body_json: &Value) -> Option<String> {
    if let Some(text) = extract_gemini_candidate_text(body_json) {
        return Some(strip_markdown_json_fence(text).to_string());
    }
    if let Some(text) = body_json
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        return Some(strip_markdown_json_fence(text).to_string());
    }
    if let Some(parts) = body_json
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_array())
        && let Some(text) = parts.iter().rev().find_map(|p| {
            p.get("text")
                .and_then(|t| t.as_str())
                .filter(|s| !s.trim().is_empty())
        })
    {
        return Some(strip_markdown_json_fence(text).to_string());
    }
    if let Some(resp_field) = body_json.get("response") {
        if let Some(text) = resp_field.as_str() {
            if !text.trim().is_empty() {
                return Some(strip_markdown_json_fence(text).to_string());
            }
        } else if resp_field.is_object() || resp_field.is_array() {
            return serde_json::to_string(resp_field).ok();
        }
    }
    if let Some(text) = body_json
        .pointer("/message/content")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        return Some(strip_markdown_json_fence(text).to_string());
    }
    if body_json.is_array()
        || body_json.get("is_true_positive").is_some()
        || body_json.get("annotations").is_some()
        || body_json.get("detections").is_some()
        || body_json.get("boxes").is_some()
        || body_json.get("objects").is_some()
    {
        return serde_json::to_string(body_json).ok();
    }
    None
}

fn round6(v: f64) -> f64 {
    (v.clamp(0.0, 1.0) * 1_000_000.0).round() / 1_000_000.0
}

pub fn xyxy_to_yolo_normalized(x1: f64, y1: f64, x2: f64, y2: f64) -> (f64, f64, f64, f64) {
    let xmin = x1.min(x2).clamp(0.0, 1.0);
    let xmax = x1.max(x2).clamp(0.0, 1.0);
    let ymin = y1.min(y2).clamp(0.0, 1.0);
    let ymax = y1.max(y2).clamp(0.0, 1.0);
    let w = (xmax - xmin).clamp(0.001, 1.0);
    let h = (ymax - ymin).clamp(0.001, 1.0);
    let cx = (xmin + xmax) / 2.0;
    let cy = (ymin + ymax) / 2.0;
    (round6(cx), round6(cy), round6(w), round6(h))
}

/// Parses a Vision AI false-positive judge verdict `(is_true_positive, confidence, reason)`.
pub fn parse_ai_judge_json_verdict(resp_val: &Value) -> Result<(bool, f64, String), String> {
    let candidate: Value = if resp_val.get("is_true_positive").is_some() {
        resp_val.clone()
    } else if let Some(text) = extract_vision_ai_response_text(resp_val) {
        serde_json::from_str(strip_markdown_json_fence(&text))
            .map_err(|e| format!("invalid JSON in Vision AI response text: {e}"))?
    } else {
        return Err(format!(
            "unrecognized AI Judge response payload: {resp_val}"
        ));
    };

    let is_true_positive = candidate
        .get("is_true_positive")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let confidence = candidate
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.9)
        .clamp(0.0, 1.0);
    let reason = candidate
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("Verified by Vision AI Judge")
        .to_string();
    Ok((is_true_positive, confidence, reason))
}

/// Parses Teacher Vision AI auto-annotation boxes into normalized YOLO [`DatasetAnnotationBox`] items.
pub fn parse_ai_auto_annotations(
    resp_val: &Value,
    sample_w: u32,
    sample_h: u32,
) -> Result<Vec<DatasetAnnotationBox>, String> {
    let candidate: Value = if resp_val.is_array()
        || resp_val.get("annotations").is_some()
        || resp_val.get("detections").is_some()
        || resp_val.get("boxes").is_some()
        || resp_val.get("objects").is_some()
    {
        resp_val.clone()
    } else if let Some(text) = extract_vision_ai_response_text(resp_val) {
        serde_json::from_str(strip_markdown_json_fence(&text))
            .map_err(|e| format!("invalid JSON in Vision AI auto-annotate response: {e}"))?
    } else {
        return Err(format!(
            "unrecognized Vision AI auto-annotate payload: {resp_val}"
        ));
    };

    let items_slice: &[Value] = if let Some(arr) = candidate.as_array() {
        arr.as_slice()
    } else if let Some(arr) = candidate
        .get("annotations")
        .or_else(|| candidate.get("detections"))
        .or_else(|| candidate.get("boxes"))
        .or_else(|| candidate.get("objects"))
        .or_else(|| candidate.get("items"))
        .and_then(|v| v.as_array())
    {
        arr.as_slice()
    } else {
        &[]
    };

    let fw = sample_w.max(1) as f64;
    let fh = sample_h.max(1) as f64;
    let mut out = Vec::new();

    for item in items_slice {
        let explicit_cid = item
            .get("class_id")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .filter(|&cid| cid < ninjanvr_db::NINJANVR_SECURITY_CLASSES.len());
        let raw_label = item
            .get("label")
            .or_else(|| item.get("class"))
            .or_else(|| item.get("name"))
            .or_else(|| item.get("category"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let class_id = match explicit_cid.or_else(|| ninjanvr_db::security_class_id(raw_label)) {
            Some(cid) => cid,
            None => continue,
        };
        let label = ninjanvr_db::security_class_label(class_id)
            .unwrap_or("person")
            .to_string();

        let coords_opt: Option<(f64, f64, f64, f64)> =
            if let (Some(cx), Some(cy), Some(w), Some(h)) = (
                item.get("cx").and_then(|v| v.as_f64()),
                item.get("cy").and_then(|v| v.as_f64()),
                item.get("w").and_then(|v| v.as_f64()),
                item.get("h").and_then(|v| v.as_f64()),
            ) {
                let scale = if cx > 1.0 || cy > 1.0 || w > 1.0 || h > 1.0 {
                    1000.0
                } else {
                    1.0
                };
                Some((
                    round6(cx / scale),
                    round6(cy / scale),
                    round6(w / scale),
                    round6(h / scale),
                ))
            } else if let Some(b2d) = item.get("box_2d").and_then(|v| v.as_array())
                && b2d.len() >= 4
            {
                let v0 = b2d[0].as_f64().unwrap_or(0.0);
                let v1 = b2d[1].as_f64().unwrap_or(0.0);
                let v2 = b2d[2].as_f64().unwrap_or(0.0);
                let v3 = b2d[3].as_f64().unwrap_or(0.0);
                let (ymin, xmin, ymax, xmax) = if v0 > 1.0 || v1 > 1.0 || v2 > 1.0 || v3 > 1.0 {
                    (v0 / 1000.0, v1 / 1000.0, v2 / 1000.0, v3 / 1000.0)
                } else {
                    (v0, v1, v2, v3)
                };
                Some(xyxy_to_yolo_normalized(xmin, ymin, xmax, ymax))
            } else if let Some(arr) = item
                .get("bbox")
                .or_else(|| item.get("box"))
                .or_else(|| item.get("xyxy"))
                .and_then(|v| v.as_array())
                && arr.len() >= 4
            {
                let v0 = arr[0].as_f64().unwrap_or(0.0);
                let v1 = arr[1].as_f64().unwrap_or(0.0);
                let v2 = arr[2].as_f64().unwrap_or(0.0);
                let v3 = arr[3].as_f64().unwrap_or(0.0);
                let (xmin, ymin, xmax, ymax) = if v0 > 1.0 || v1 > 1.0 || v2 > 1.0 || v3 > 1.0 {
                    if v2 <= 1000.0 && v3 <= 1000.0 {
                        (v0 / 1000.0, v1 / 1000.0, v2 / 1000.0, v3 / 1000.0)
                    } else {
                        (v0 / fw, v1 / fh, v2 / fw, v3 / fh)
                    }
                } else {
                    (v0, v1, v2, v3)
                };
                Some(xyxy_to_yolo_normalized(xmin, ymin, xmax, ymax))
            } else if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
                item.get("x_min")
                    .or_else(|| item.get("xmin"))
                    .or_else(|| item.get("x1"))
                    .and_then(|v| v.as_f64()),
                item.get("y_min")
                    .or_else(|| item.get("ymin"))
                    .or_else(|| item.get("y1"))
                    .and_then(|v| v.as_f64()),
                item.get("x_max")
                    .or_else(|| item.get("xmax"))
                    .or_else(|| item.get("x2"))
                    .and_then(|v| v.as_f64()),
                item.get("y_max")
                    .or_else(|| item.get("ymax"))
                    .or_else(|| item.get("y2"))
                    .and_then(|v| v.as_f64()),
            ) {
                let (xmin, ymin, xmax, ymax) = if x1 > 1.0 || y1 > 1.0 || x2 > 1.0 || y2 > 1.0 {
                    (x1 / 1000.0, y1 / 1000.0, x2 / 1000.0, y2 / 1000.0)
                } else {
                    (x1, y1, x2, y2)
                };
                Some(xyxy_to_yolo_normalized(xmin, ymin, xmax, ymax))
            } else {
                None
            };

        let Some((cx, cy, w, h)) = coords_opt else {
            continue;
        };
        let score = item
            .get("score")
            .or_else(|| item.get("confidence"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.95)
            .clamp(0.0, 1.0);
        out.push(DatasetAnnotationBox {
            class_id,
            label,
            cx,
            cy,
            w,
            h,
            score,
            source: "ai_teacher".to_string(),
        });
    }

    Ok(out)
}

/// High-level Vision AI Judge service parameterized over [`VisionAiTransport`].
#[derive(Debug, Clone)]
pub struct VisionAiJudge<T = ReqwestVisionAiTransport> {
    transport: T,
}

impl Default for VisionAiJudge<ReqwestVisionAiTransport> {
    fn default() -> Self {
        Self {
            transport: ReqwestVisionAiTransport,
        }
    }
}

impl<T: VisionAiTransport> VisionAiJudge<T> {
    pub const fn new(transport: T) -> Self {
        Self { transport }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub async fn execute_prompt(
        &self,
        genai_cfg: &GenAiConfig,
        prompt: &str,
        image_b64: &str,
        mime_type: &str,
        temperature: f64,
        max_tokens: u32,
    ) -> Result<String, String> {
        let req = build_vision_ai_request(
            genai_cfg,
            prompt,
            image_b64,
            mime_type,
            temperature,
            max_tokens,
        )?;
        let timeout = effective_vision_ai_timeout(genai_cfg);
        execute_vision_ai_prompt(&self.transport, &req, timeout).await
    }
}

// ============================================================================
// Part 2 — DatasetArtifactStore (Local-Substitutable)
// ============================================================================

/// Validates that `sample_id` is a safe, non-traversal identifier.
pub fn is_safe_sample_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && !id.starts_with('.')
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn is_safe_relative_subpath(rel: &Path) -> bool {
    if rel.as_os_str().is_empty() {
        return false;
    }
    let mut comps = rel.components();
    let Some(first) = comps.next() else {
        return false;
    };
    let first_str = first.as_os_str().to_str().unwrap_or("");
    if !matches!(first_str, "dataset" | "clips") {
        return false;
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                let Some(s) = part.to_str() else {
                    return false;
                };
                if s.is_empty() || s.starts_with('.') || s.contains("..") {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn resolve_safe_recorded_artifact_path(media_dir: &Path, raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(trimmed);
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    if std::fs::symlink_metadata(media_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }

    let resolved = if candidate.is_absolute() {
        if let Ok(rel) = candidate.strip_prefix(media_dir) {
            if !is_safe_relative_subpath(rel) {
                return None;
            }
            media_dir.join(rel)
        } else if let (Ok(canon_media), Ok(canon_cand)) = (
            std::fs::canonicalize(media_dir),
            std::fs::canonicalize(&candidate),
        ) {
            let rel = canon_cand.strip_prefix(&canon_media).ok()?;
            if !is_safe_relative_subpath(rel) {
                return None;
            }
            canon_cand
        } else {
            return None;
        }
    } else {
        if !is_safe_relative_subpath(&candidate) {
            return None;
        }
        media_dir.join(&candidate)
    };

    if let Ok(rel) = resolved.strip_prefix(media_dir) {
        let mut cur = media_dir.to_path_buf();
        for comp in rel.components() {
            if let Component::Normal(part) = comp {
                cur.push(part);
                if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
                    return None;
                }
            } else {
                return None;
            }
        }
    } else if std::fs::symlink_metadata(&resolved).is_ok_and(|m| m.file_type().is_symlink()) {
        return None;
    }

    Some(resolved)
}

fn paths_refer_to_same_artifact(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(ca), Ok(cb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return ca == cb;
    }
    false
}

/// Returns the deduplicated canonical set of artifact paths for a dataset sample:
/// 1. `recorded_image_path` (when present and safely contained inside `media_dir/dataset` or `media_dir/clips`)
/// 2. `media_dir/dataset/{sample_id}.webp`
/// 3. `media_dir/clips/dataset/{sample_id}.webp`
pub fn sample_artifact_paths(
    media_dir: &Path,
    sample_id: &str,
    recorded_image_path: Option<&str>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(3);

    if let Some(rec) = recorded_image_path
        && let Some(safe_rec) = resolve_safe_recorded_artifact_path(media_dir, rec)
    {
        out.push(safe_rec);
    }

    if is_safe_sample_id(sample_id) {
        let fname = format!("{sample_id}.webp");
        let primary = media_dir.join("dataset").join(&fname);
        let mirror = media_dir.join("clips").join("dataset").join(&fname);
        for cand in [primary, mirror] {
            if let Some(cand_str) = cand.to_str()
                && let Some(safe_cand) = resolve_safe_recorded_artifact_path(media_dir, cand_str)
                && !out
                    .iter()
                    .any(|existing| paths_refer_to_same_artifact(existing, &safe_cand))
            {
                out.push(safe_cand);
            }
        }
    }

    out
}

fn build_mask_config_value(masks: &[[[f64; 2]; 4]]) -> Option<Value> {
    if masks.is_empty() {
        return None;
    }
    let mut poly_strings = Vec::with_capacity(masks.len());
    for quad in masks {
        if quad
            .iter()
            .any(|pt| !pt[0].is_finite() || !pt[1].is_finite())
        {
            return None;
        }
        let coords = quad
            .iter()
            .map(|pt| format!("{:.6},{:.6}", pt[0], pt[1]))
            .collect::<Vec<_>>()
            .join(",");
        poly_strings.push(Value::String(coords));
    }
    Some(Value::Array(poly_strings))
}

fn redact_sample_bytes_once(
    raw_image_bytes: &[u8],
    masks: &[[[f64; 2]; 4]],
) -> Result<Vec<u8>, String> {
    if raw_image_bytes.len() as u64 > MAX_DATASET_SAMPLE_FILE_BYTES {
        return Err(format!(
            "Dataset sample image exceeds maximum size ({MAX_DATASET_SAMPLE_FILE_BYTES} bytes)"
        ));
    }
    if masks.is_empty() {
        if raw_image_bytes.is_empty() {
            let fallback = RgbImage::from_pixel(64, 36, Rgb([24, 24, 24]));
            return encode_image_webp(&fallback).map_err(|e| e.to_string());
        }
        if raw_image_bytes.starts_with(b"RIFF") {
            return Ok(raw_image_bytes.to_vec());
        }
        if let Ok(decoded) = image::load_from_memory(raw_image_bytes) {
            let rgb = decoded.to_rgb8();
            if let Ok(webp) = encode_image_webp(&rgb) {
                return Ok(webp);
            }
        }
        return Ok(raw_image_bytes.to_vec());
    }

    // Active privacy masks: decode to RGB (or fail closed to matte tactical dark) and redact once.
    let mut rgb = image::load_from_memory(raw_image_bytes)
        .map(|img| img.to_rgb8())
        .unwrap_or_else(|_| RgbImage::from_pixel(64, 36, Rgb([12, 14, 18])));

    match build_mask_config_value(masks) {
        Some(mask_val) => {
            let poly_mask = PolygonMask::from_config_value(&mask_val, rgb.width(), rgb.height());
            if poly_mask.is_empty() {
                rgb = RgbImage::from_pixel(
                    rgb.width().max(1),
                    rgb.height().max(1),
                    Rgb([12, 14, 18]),
                );
            } else {
                apply_privacy_mask_rgb(&mut rgb, &poly_mask);
            }
        }
        None => {
            // Non-finite mask coordinates fail closed so unredacted pixels never leak.
            rgb = RgbImage::from_pixel(rgb.width().max(1), rgb.height().max(1), Rgb([12, 14, 18]));
        }
    }

    encode_image_webp(&rgb).map_err(|e| format!("Failed to encode redacted WebP sample: {e}"))
}

fn ensure_contained_regular_file(media_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    if candidate.as_os_str().is_empty()
        || candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    let media_canon = std::fs::canonicalize(media_dir).ok()?;
    let lmeta = std::fs::symlink_metadata(candidate).ok()?;
    if lmeta.file_type().is_symlink() || !lmeta.file_type().is_file() {
        return None;
    }
    let candidate_canon = std::fs::canonicalize(candidate).ok()?;
    let cmeta = std::fs::symlink_metadata(&candidate_canon).ok()?;
    if cmeta.file_type().is_symlink() || !cmeta.file_type().is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if lmeta.dev() != cmeta.dev() || lmeta.ino() != cmeta.ino() {
            return None;
        }
    }
    let rel = candidate_canon.strip_prefix(&media_canon).ok()?;
    if rel.as_os_str().is_empty()
        || rel
            .components()
            .any(|c| c.as_os_str().to_str().is_none_or(|s| s.starts_with('.')))
    {
        return None;
    }
    Some(candidate_canon)
}

fn write_contained_dataset_file(
    media_dir: &Path,
    target_dir: &Path,
    filename: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind, Write};
    if filename.is_empty()
        || filename.starts_with('.')
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.contains('\0')
    {
        return Err(Error::new(ErrorKind::InvalidInput, "unsafe media filename"));
    }
    if std::fs::symlink_metadata(media_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "refusing to write through symlinked media_dir",
        ));
    }
    std::fs::create_dir_all(media_dir)?;
    let Ok(rel) = target_dir.strip_prefix(media_dir) else {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "target directory must reside inside media_dir",
        ));
    };
    let mut cur = media_dir.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                cur.push(part);
                if let Ok(meta) = std::fs::symlink_metadata(&cur)
                    && (meta.file_type().is_symlink() || !meta.file_type().is_dir())
                {
                    return Err(Error::new(
                        ErrorKind::PermissionDenied,
                        "refusing to traverse symlinked or non-directory media subdirectory",
                    ));
                }
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "invalid path component in target media directory",
                ));
            }
        }
    }
    std::fs::create_dir_all(target_dir)?;
    let canon_media = std::fs::canonicalize(media_dir)?;
    let canon_target = std::fs::canonicalize(target_dir)?;
    if !canon_target.starts_with(&canon_media) {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "target directory escapes media_dir",
        ));
    }
    let dst_path = target_dir.join(filename);
    if let Ok(dst_meta) = std::fs::symlink_metadata(&dst_path)
        && (dst_meta.file_type().is_symlink() || !dst_meta.file_type().is_file())
    {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "refusing to overwrite symlink or non-regular file in media_dir",
        ));
    }
    let nonce: u64 = rand::random();
    let tmp_name = format!(".{filename}.{nonce:016x}.tmp");
    let tmp_path = target_dir.join(tmp_name);
    if std::fs::symlink_metadata(&tmp_path).is_ok() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(nix::libc::O_NOFOLLOW);
        }
        let mut tmp_file = opts.open(&tmp_path)?;
        if let Err(err) = tmp_file.write_all(bytes) {
            drop(tmp_file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
    }
    if let Err(err) = std::fs::rename(&tmp_path, &dst_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(dst_path)
}

/// Applies privacy mask redaction **once** (when `!masks.is_empty()`) and writes both
/// `media_dir/dataset/{sample_id}.webp` and `media_dir/clips/dataset/{sample_id}.webp`,
/// returning the canonical primary dataset path.
pub fn write_redacted_sample_webp(
    media_dir: &Path,
    sample_id: &str,
    raw_image_bytes: &[u8],
    masks: &[[[f64; 2]; 4]],
) -> Result<PathBuf, String> {
    if !is_safe_sample_id(sample_id) {
        return Err(format!("Invalid dataset sample_id '{sample_id}'"));
    }

    let final_bytes = redact_sample_bytes_once(raw_image_bytes, masks)?;
    let fname = format!("{sample_id}.webp");
    let dataset_dir = media_dir.join("dataset");
    let clips_dataset_dir = media_dir.join("clips").join("dataset");

    let primary_path = write_contained_dataset_file(media_dir, &dataset_dir, &fname, &final_bytes)
        .map_err(|e| format!("Failed to write primary dataset sample '{fname}': {e}"))?;

    if let Err(err) =
        write_contained_dataset_file(media_dir, &clips_dataset_dir, &fname, &final_bytes)
    {
        let _ = std::fs::remove_file(&primary_path);
        return Err(format!(
            "Failed to write mirror dataset sample 'clips/dataset/{fname}': {err}"
        ));
    }

    Ok(primary_path)
}

/// Removes all canonical artifact files (`recorded_image_path`, `media_dir/dataset/{sample_id}.webp`,
/// `media_dir/clips/dataset/{sample_id}.webp`) symmetrically and returns the number of deleted files.
pub fn delete_sample_artifacts(
    media_dir: &Path,
    sample_id: &str,
    recorded_image_path: Option<&str>,
) -> usize {
    let candidates = sample_artifact_paths(media_dir, sample_id, recorded_image_path);
    let mut removed = 0usize;

    for path in candidates {
        let Some(safe_path) = ensure_contained_regular_file(media_dir, &path) else {
            continue;
        };
        if std::fs::remove_file(&safe_path).is_ok() {
            removed += 1;
        }
    }

    removed
}

/// Local-substitutable store handle encapsulating dataset sample artifact persistence under `media_dir`.
#[derive(Debug, Clone)]
pub struct DatasetArtifactStore {
    media_dir: PathBuf,
}

impl DatasetArtifactStore {
    pub fn new(media_dir: impl Into<PathBuf>) -> Self {
        Self {
            media_dir: media_dir.into(),
        }
    }

    pub fn media_dir(&self) -> &Path {
        &self.media_dir
    }

    pub fn artifact_paths(
        &self,
        sample_id: &str,
        recorded_image_path: Option<&str>,
    ) -> Vec<PathBuf> {
        sample_artifact_paths(&self.media_dir, sample_id, recorded_image_path)
    }

    pub fn write_redacted_webp(
        &self,
        sample_id: &str,
        raw_image_bytes: &[u8],
        masks: &[[[f64; 2]; 4]],
    ) -> Result<PathBuf, String> {
        write_redacted_sample_webp(&self.media_dir, sample_id, raw_image_bytes, masks)
    }

    pub fn delete_artifacts(&self, sample_id: &str, recorded_image_path: Option<&str>) -> usize {
        delete_sample_artifacts(&self.media_dir, sample_id, recorded_image_path)
    }
}

// ============================================================================
// Part 3 — DatasetCatalog, GenAI Discovery & Event Verification
// ============================================================================

/// Instance-owned dataset curation and Vision AI catalog stored on `AppState`.
///
/// Replaces process-global `BATCH_AUTO_ANNOTATE_SEMAPHORE` and `DATASET_EXPORT_ZIP_SEMAPHORE`
/// `LazyLock` singletons with per-instance semaphores.
#[derive(Debug, Clone)]
pub struct DatasetCatalog {
    pub batch_auto_annotate_semaphore: Arc<Semaphore>,
    pub export_zip_semaphore: Arc<Semaphore>,
    pub vision_judge: VisionAiJudge<ReqwestVisionAiTransport>,
}

impl Default for DatasetCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl DatasetCatalog {
    pub fn new() -> Self {
        Self {
            batch_auto_annotate_semaphore: Arc::new(Semaphore::new(1)),
            export_zip_semaphore: Arc::new(Semaphore::new(2)),
            vision_judge: VisionAiJudge::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GenAiValidateRequest {
    pub provider: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub clear_api_key: Option<bool>,
    pub validate: Option<bool>,
}

async fn read_bounded_reqwest_json(
    mut resp: reqwest::Response,
    max_bytes: usize,
    context_msg: &'static str,
) -> Result<Value> {
    if let Some(len) = resp.content_length()
        && len > max_bytes as u64
    {
        anyhow::bail!("{context_msg}: response body too large ({len} bytes > {max_bytes} bytes)");
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.context(context_msg)? {
        if buf.len().saturating_add(chunk.len()) > max_bytes {
            anyhow::bail!(
                "{context_msg}: response body exceeded maximum limit of {max_bytes} bytes"
            );
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).context(context_msg)
}

pub fn build_effective_genai_config(
    saved: &GenAiConfig,
    req: &GenAiValidateRequest,
) -> GenAiConfig {
    let mut cfg = saved.clone();
    let mut provider_changed = false;
    if let Some(ref p) = req.provider
        && !p.trim().is_empty()
    {
        if !p.trim().eq_ignore_ascii_case(saved.provider.trim()) {
            provider_changed = true;
        }
        cfg.provider = p.trim().to_string();
    }
    let mut custom_base_url_changed = false;
    if let Some(ref b) = req.base_url {
        let trimmed = b.trim();
        if trimmed.is_empty() {
            cfg.base_url = None;
        } else {
            if Some(trimmed) != saved.base_url.as_deref().map(str::trim) {
                custom_base_url_changed = true;
            }
            cfg.base_url = Some(trimmed.to_string());
        }
    }
    let base_url_changed = custom_base_url_changed
        || (provider_changed
            && (cfg.base_url.is_some() || cfg.provider.eq_ignore_ascii_case("ollama")));
    if base_url_changed {
        cfg.api_key = None;
        cfg.api_key_env = String::new();
    } else if cfg.base_url.is_some() {
        cfg.api_key_env = String::new();
        if cfg
            .api_key
            .as_deref()
            .is_some_and(|k| is_env_var_identifier(k) || std::env::var(k.trim()).is_ok())
        {
            cfg.api_key = None;
        }
    }
    if let Some(ref env_name) = req.api_key_env
        && !env_name.trim().is_empty()
    {
        let env_trimmed = env_name.trim();
        if is_allowed_genai_api_key_env(env_trimmed) && !base_url_changed && cfg.base_url.is_none()
        {
            cfg.api_key_env = env_trimmed.to_string();
        }
    }
    if req.clear_api_key == Some(true) {
        cfg.api_key = None;
    } else if let Some(ref key) = req.api_key {
        let trimmed = key.trim();
        if !trimmed.is_empty() && trimmed != "***" && trimmed != "*" {
            if let Some(after_dollar) = trimmed.strip_prefix('$') {
                let env_var = after_dollar
                    .trim()
                    .strip_prefix('{')
                    .and_then(|s| s.strip_suffix('}'))
                    .unwrap_or(after_dollar.trim())
                    .trim();
                if !base_url_changed
                    && cfg.base_url.is_none()
                    && is_allowed_genai_api_key_env(env_var)
                {
                    cfg.api_key = Some(trimmed.to_string());
                } else {
                    cfg.api_key = None;
                }
            } else if is_env_var_identifier(trimmed) || std::env::var(trimmed).is_ok() {
                if !base_url_changed
                    && cfg.base_url.is_none()
                    && is_allowed_genai_api_key_env(trimmed)
                {
                    cfg.api_key = Some(trimmed.to_string());
                } else {
                    cfg.api_key = None;
                }
            } else {
                cfg.api_key = Some(trimmed.to_string());
            }
        }
    }
    cfg
}

pub fn extract_models_from_provider_payload(provider: &str, val: &Value) -> Vec<String> {
    let mut models = Vec::new();
    fn push_unique(models: &mut Vec<String>, name: &str) {
        let cleaned = name.trim().strip_prefix("models/").unwrap_or(name.trim());
        if !cleaned.is_empty() && !models.iter().any(|m| m == cleaned) {
            models.push(cleaned.to_string());
        }
    }

    if provider == "gemini"
        && let Some(arr) = val.get("models").and_then(|v| v.as_array())
    {
        for item in arr {
            if let Some(methods) = item
                .get("supportedGenerationMethods")
                .and_then(|v| v.as_array())
                && !methods.is_empty()
                && !methods
                    .iter()
                    .any(|m| m.as_str() == Some("generateContent"))
            {
                continue;
            }
            if let Some(raw_name) = item
                .get("name")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .or_else(|| item.as_str())
            {
                let short = raw_name
                    .trim()
                    .strip_prefix("models/")
                    .unwrap_or(raw_name.trim());
                let lower = short.to_ascii_lowercase();
                if lower.starts_with("embedding-")
                    || lower.starts_with("text-embedding-")
                    || lower == "aqa"
                    || lower.starts_with("imagen-")
                    || lower.starts_with("veo-")
                {
                    continue;
                }
                push_unique(&mut models, short);
            }
        }
    }

    if models.is_empty() {
        for key in ["models", "data"] {
            if let Some(arr) = val.get(key).and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        push_unique(&mut models, s);
                    } else if let Some(s) = item
                        .get("id")
                        .or_else(|| item.get("name"))
                        .or_else(|| item.get("model"))
                        .and_then(|v| v.as_str())
                    {
                        push_unique(&mut models, s);
                    }
                }
            }
        }
    }

    if models.is_empty()
        && let Some(arr) = val.as_array()
    {
        for item in arr {
            if let Some(s) = item.as_str() {
                push_unique(&mut models, s);
            } else if let Some(s) = item
                .get("id")
                .or_else(|| item.get("name"))
                .or_else(|| item.get("model"))
                .and_then(|v| v.as_str())
            {
                push_unique(&mut models, s);
            }
        }
    }

    if provider == "openai" && models.len() > 1 {
        let filtered: Vec<String> = models
            .iter()
            .filter(|m| {
                let l = m.to_ascii_lowercase();
                !l.starts_with("whisper-")
                    && !l.starts_with("tts-")
                    && !l.starts_with("dall-e-")
                    && !l.starts_with("text-embedding-")
                    && !l.starts_with("omni-moderation-")
                    && !l.starts_with("babbage-")
                    && !l.starts_with("davinci-")
            })
            .cloned()
            .collect();
        if !filtered.is_empty() {
            return filtered;
        }
    }

    models
}

pub async fn discover_genai_models(genai: &GenAiConfig) -> Result<Vec<String>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let provider = genai.provider.trim().to_ascii_lowercase();
    let api_key = resolve_safe_genai_api_key(genai);
    let custom_base = genai
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(b) = custom_base {
        validate_genai_base_url(b).map_err(|e| anyhow::anyhow!(e))?;
    }
    let timeout_ms = genai.timeout_ms.clamp(2000, 30_000);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let resp = match provider.as_str() {
        "openai" => {
            if api_key.is_empty() && custom_base.is_none() {
                anyhow::bail!(
                    "Missing OpenAI API key. Enter an API key, set '{}', or provide a custom LLM proxy Base URL.",
                    genai.api_key_env
                );
            }
            let base = custom_base
                .unwrap_or("https://api.openai.com/v1")
                .trim_end_matches('/');
            let url = if base.ends_with("/models") {
                base.to_string()
            } else if let Some(prefix) = base.strip_suffix("/chat/completions") {
                format!("{}/models", prefix.trim_end_matches('/'))
            } else if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            let mut req = client.get(&url);
            if !api_key.is_empty() {
                req = req.bearer_auth(&api_key);
            }
            req.send()
                .await
                .context("OpenAI /models validation request failed")?
        }
        "ollama" => {
            let base = custom_base
                .unwrap_or("http://127.0.0.1:11434")
                .trim_end_matches('/');
            let url = if base.ends_with("/api/tags") || base.ends_with("/models") {
                base.to_string()
            } else if let Some(prefix) = base
                .strip_suffix("/api/generate")
                .or_else(|| base.strip_suffix("/api/chat"))
            {
                format!("{}/api/tags", prefix.trim_end_matches('/'))
            } else if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/api/tags")
            };
            let mut req = client.get(&url);
            if !api_key.is_empty() {
                req = req.bearer_auth(&api_key);
            }
            req.send()
                .await
                .context("Ollama /api/tags validation request failed")?
        }
        _ => {
            if api_key.is_empty() && custom_base.is_none() {
                anyhow::bail!(
                    "Missing Gemini API key. Enter an API key or set '{}'.",
                    genai.api_key_env
                );
            }
            let base = custom_base
                .unwrap_or("https://generativelanguage.googleapis.com")
                .trim_end_matches('/');
            let url = if base.ends_with("/models") {
                if api_key.is_empty() || !base.contains("/v1beta") {
                    base.to_string()
                } else {
                    format!("{base}?pageSize=100&key={api_key}")
                }
            } else if base.ends_with("/v1") {
                format!("{base}/models")
            } else if api_key.is_empty() {
                format!("{base}/v1beta/models?pageSize=100")
            } else {
                format!("{base}/v1beta/models?pageSize=100&key={api_key}")
            };
            let mut req = client.get(&url);
            if !api_key.is_empty() {
                req = req.header("x-goog-api-key", &api_key);
                if base.ends_with("/v1") || base.ends_with("/v1/models") {
                    req = req.bearer_auth(&api_key);
                }
            }
            req.send()
                .await
                .context("Gemini /v1beta/models validation request failed")?
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("API validation HTTP {status}");
    }

    let resp_json: Value = read_bounded_reqwest_json(
        resp,
        MAX_GENAI_RESPONSE_BYTES,
        "failed to decode model catalog JSON from AI provider",
    )
    .await?;
    let models = extract_models_from_provider_payload(&provider, &resp_json);
    if models.is_empty() {
        anyhow::bail!(
            "API endpoint responded, but no compatible models were returned for provider '{provider}'"
        );
    }
    Ok(models)
}

pub async fn api_genai_models_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(query): Query<GenAiValidateRequest>,
) -> Response {
    if let Err(err_resp) = crate::authorize_admin_read(&state, &jar, &headers).await {
        return err_resp.into_response();
    }
    let _ = query.validate;
    Json(json!([])).into_response()
}

pub async fn api_genai_models_post(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !verify_cookie_csrf_origin(&headers, &uri) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "valid": false,
                "models": [],
                "message": "Cross-origin cookie request rejected (Origin/Host mismatch)"
            })),
        )
            .into_response();
    }
    if !caller_can_mutate(&state, &jar, &headers, true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "valid": false,
                "models": [],
                "message": "Admin role required"
            })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "valid": false,
                "models": [],
                "message": "Camera-restricted users cannot validate or modify global GenAI settings"
            })),
        )
            .into_response();
    }
    let req: GenAiValidateRequest = if body.is_empty() {
        GenAiValidateRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "valid": false,
                        "models": [],
                        "message": format!("Invalid JSON request: {err}")
                    })),
                )
                    .into_response();
            }
        }
    };
    let saved = state.config.read().await.genai.clone();
    let provider_matches_saved = match req
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => true,
        Some(p) => p.eq_ignore_ascii_case(saved.provider.trim()),
    };
    let base_url_matches_saved = match req.base_url.as_deref().map(str::trim) {
        None => true,
        Some("") => saved
            .base_url
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty(),
        Some(b) => Some(b) == saved.base_url.as_deref().map(str::trim),
    };
    let effective = build_effective_genai_config(&saved, &req);
    match discover_genai_models(&effective).await {
        Ok(models) => {
            if provider_matches_saved
                && base_url_matches_saved
                && let Some(ref k) = effective.api_key
                && !is_env_var_identifier(k)
            {
                state.config.write().await.genai.api_key = Some(k.clone());
            }
            Json(json!({
                "valid": true,
                "provider": effective.provider,
                "models": models,
                "message": format!("Validated {} API ({} models loaded)", effective.provider.to_uppercase(), models.len())
            }))
            .into_response()
        }
        Err(err) => {
            let api_key = resolve_safe_genai_api_key(&effective);
            let msg = scrub_genai_secret_in_error(format!("{err:#}"), &api_key);
            let status = if msg.contains("Missing ") || msg.contains("Disallowed or invalid") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            (
                status,
                Json(json!({
                    "valid": false,
                    "provider": effective.provider,
                    "models": [],
                    "message": msg
                })),
            )
                .into_response()
        }
    }
}

pub async fn api_event_verify_ai(
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
    if !caller_can_mutate(&state, &jar, &headers, false).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Read-only role cannot verify events" })),
        )
            .into_response();
    }
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*")
    {
        let permitted = state
            .db
            .get_event(&id)
            .ok()
            .flatten()
            .and_then(|ev| {
                ev["camera"]
                    .as_str()
                    .map(|c| allowed.iter().any(|a| a == c))
            })
            .unwrap_or(false);
        if !permitted {
            return (
                StatusCode::FORBIDDEN,
                Json(
                    json!({ "success": false, "message": "Camera is not permitted for this user" }),
                ),
            )
                .into_response();
        }
    }
    match verify_event_with_genai(&state, &id).await {
        Ok(res) => Json(res).into_response(),
        Err(err) => {
            let genai_cfg = state.config.read().await.genai.clone();
            let api_key = resolve_safe_genai_api_key(&genai_cfg);
            let msg = scrub_genai_secret_in_error(format!("{err:#}"), &api_key);
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            };
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
    }
}

fn urlencoding_simple(s: &str) -> String {
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

pub fn scrub_genai_secret_in_error(raw: String, api_key: &str) -> String {
    let trimmed = api_key.trim();
    if !trimmed.is_empty() && trimmed != "***" && trimmed != "*" {
        let mut out = raw.replace(trimmed, "***");
        let url_encoded = urlencoding_simple(trimmed);
        if !url_encoded.is_empty() && url_encoded != trimmed {
            out = out.replace(&url_encoded, "***");
        }
        out
    } else {
        raw
    }
}

pub async fn verify_event_with_genai(state: &AppState, event_id: &str) -> Result<Value> {
    verify_event_with_genai_components(
        state.config.clone(),
        state.db.clone(),
        state.media_dir.clone(),
        event_id,
    )
    .await
}

pub async fn verify_event_with_genai_components(
    config: Arc<RwLock<NinjaConfig>>,
    db: NinjaDatabase,
    media_dir: PathBuf,
    event_id: &str,
) -> Result<Value> {
    if !is_safe_id(event_id) {
        anyhow::bail!("Invalid event_id '{event_id}'");
    }
    let Some(ev) = db.get_event(event_id)? else {
        anyhow::bail!("Event '{event_id}' not found");
    };
    let camera = ev["camera"].as_str().unwrap_or("camera");
    if !is_safe_id(camera) {
        anyhow::bail!("Invalid camera identifier '{camera}'");
    }
    let label = ev["label"].as_str().unwrap_or("object");
    let score = ev["top_score"]
        .as_f64()
        .or_else(|| ev["data"]["top_score"].as_f64())
        .unwrap_or(0.5);

    let priv_mask = camera_privacy_mask_value(&config, camera).await;
    let clips_dir = media_dir.join("clips");
    let candidates = [
        (
            clips_dir.join(format!("{event_id}-clean.jpg")),
            "image/jpeg",
        ),
        (
            clips_dir.join(format!("{camera}-{event_id}-clean.webp")),
            "image/webp",
        ),
        (
            clips_dir.join(format!("{camera}-{event_id}.webp")),
            "image/webp",
        ),
        (clips_dir.join(format!("{event_id}.jpg")), "image/jpeg"),
    ];

    let mut raw_img_bytes: Option<(Vec<u8>, &'static str)> = None;
    for (path, mime) in candidates {
        if let Some(safe_path) = ensure_clips_artifact_path(&media_dir, &path)
            && let Some(bytes) =
                read_bounded_non_symlink_bytes(&safe_path, MAX_DATASET_SAMPLE_FILE_BYTES).await
            && !bytes.is_empty()
        {
            raw_img_bytes = Some((bytes, mime));
            break;
        }
    }
    if raw_img_bytes.is_none()
        && let Some(thumb) = ev["thumbnail"].as_str()
        && !thumb.is_empty()
        && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(thumb.trim())
        && !decoded.is_empty()
    {
        raw_img_bytes = Some((decoded, "image/webp"));
    }

    let (final_bytes, mime_type) = match raw_img_bytes {
        Some((bytes, mime)) => {
            redact_image_bytes_with_privacy_mask_async(
                bytes,
                priv_mask.clone(),
                mime == "image/jpeg",
            )
            .await
        }
        None => {
            if let Some(thumb) = ev["thumbnail"].as_str()
                && !thumb.is_empty()
                && !has_active_privacy_mask(priv_mask.as_ref())
            {
                (
                    base64::engine::general_purpose::STANDARD
                        .decode(thumb.trim())
                        .unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec()),
                    "image/webp",
                )
            } else {
                (b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec(), "image/webp")
            }
        }
    };
    let img_b64 = base64::engine::general_purpose::STANDARD.encode(&final_bytes);

    let genai = config.read().await.genai.clone();
    let prompt = format!(
        "You are a security camera false-positive verification judge for camera '{camera}'. \
         An edge object detector reported a '{label}' with confidence {score:.2}. \
         Inspect the image and respond ONLY with a JSON object: \
         {{\"is_true_positive\": boolean, \"confidence\": number, \"reason\": \"short explanation\"}}."
    );
    let req = build_vision_ai_request(&genai, &prompt, &img_b64, mime_type, 0.1, 512)
        .map_err(|e| anyhow::anyhow!(e))?;
    let timeout = effective_vision_ai_timeout(&genai);
    let resp_json = ReqwestVisionAiTransport::new()
        .send_json(&req, timeout)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let (is_true_positive, confidence, reason) =
        parse_ai_judge_json_verdict(&resp_json).map_err(|e| anyhow::anyhow!(e))?;
    let verified_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let verification = json!({
        "is_true_positive": is_true_positive,
        "confidence": confidence,
        "reason": reason,
        "provider": genai.provider,
        "model": genai.model,
        "verified_at": verified_at
    });
    db.set_event_ai_verification(event_id, &verification)?;

    let auto_suppressed = !is_true_positive && genai.auto_suppress_false_positives;
    if auto_suppressed {
        let _ = db.set_event_false_positive(event_id, true)?;
        let _ = create_dataset_sample_from_event_with_config(
            Some(&config),
            &db,
            &media_dir,
            event_id,
            "hard_negative",
            None,
        )
        .await;
    }

    Ok(json!({
        "success": true,
        "event_id": event_id,
        "is_true_positive": is_true_positive,
        "confidence": confidence,
        "reason": reason,
        "provider": genai.provider,
        "model": genai.model,
        "verified_at": verified_at,
        "false_positive": auto_suppressed,
        "ai_verification": verification
    }))
}

pub fn xywh_to_yolo_normalized(b: [f64; 4]) -> (f64, f64, f64, f64) {
    if b.iter().all(|&v| v == 0.0) {
        return (0.5, 0.5, 0.25, 0.4);
    }
    let xmin = b[0].clamp(0.0, 1.0);
    let ymin = b[1].clamp(0.0, 1.0);
    let w = b[2].clamp(0.001, (1.0 - xmin).max(0.001));
    let h = b[3].clamp(0.001, (1.0 - ymin).max(0.001));
    (
        round6((xmin + w / 2.0).clamp(0.0, 1.0)),
        round6((ymin + h / 2.0).clamp(0.0, 1.0)),
        round6(w),
        round6(h),
    )
}

pub fn event_box_to_yolo_normalized(b: [f64; 4]) -> (f64, f64, f64, f64) {
    if b.iter().all(|&v| v == 0.0) {
        return (0.5, 0.5, 0.25, 0.4);
    }
    if b[2] > b[0] && b[3] > b[1] {
        xyxy_to_yolo_normalized(b[0], b[1], b[2], b[3])
    } else {
        xywh_to_yolo_normalized(b)
    }
}

pub fn event_uses_xywh_norm_boxes(ev: &Value) -> bool {
    ev["data"].get("provenance").is_some_and(|p| p.is_object())
        || ev["data"].get("region").is_some_and(|r| r.is_array())
}

pub fn is_rgb_image_monochrome_ir(rgb: &RgbImage) -> bool {
    let w = rgb.width();
    let h = rgb.height();
    if w < 8 || h < 8 {
        return false;
    }
    let mut diff_sum = 0.0f64;
    let mut luma_sum = 0.0f64;
    let mut luma_sq_sum = 0.0f64;
    let mut samples = 0u32;
    for gy in 0..16u32 {
        let y = ((gy * h) / 16).min(h - 1);
        for gx in 0..16u32 {
            let x = ((gx * w) / 16).min(w - 1);
            let px = rgb.get_pixel(x, y);
            let r = px[0] as f64;
            let g = px[1] as f64;
            let b = px[2] as f64;
            diff_sum += (r - g).abs() + (g - b).abs() + (r - b).abs();
            let y_val = 0.299 * r + 0.587 * g + 0.114 * b;
            luma_sum += y_val;
            luma_sq_sum += y_val * y_val;
            samples += 1;
        }
    }
    if samples == 0 {
        return false;
    }
    let n = samples as f64;
    let mean_diff = diff_sum / (n * 3.0);
    let mean_luma = luma_sum / n;
    let luma_var = (luma_sq_sum / n) - (mean_luma * mean_luma);
    mean_diff <= 4.0 && luma_var > 4.0
}

pub fn extract_event_box_array(ev: &Value) -> [f64; 4] {
    let parse4 = |v: &Value| -> Option<[f64; 4]> {
        let arr = v.as_array()?;
        if arr.len() < 4 {
            return None;
        }
        Some([
            arr[0].as_f64()?,
            arr[1].as_f64()?,
            arr[2].as_f64()?,
            arr[3].as_f64()?,
        ])
    };
    parse4(&ev["box"])
        .or_else(|| parse4(&ev["data"]["box"]))
        .or_else(|| parse4(&ev["data"]["provenance"]["norm_bbox"]))
        .unwrap_or([0.0, 0.0, 0.0, 0.0])
}

pub async fn redact_dataset_sample_file_if_masked(
    config: &Arc<RwLock<NinjaConfig>>,
    media_dir: &Path,
    sample: &DatasetSampleDto,
) {
    let priv_mask = camera_privacy_mask_value(config, &sample.camera).await;
    if !has_active_privacy_mask(priv_mask.as_ref()) {
        return;
    }
    let sample_path = PathBuf::from(&sample.image_path);
    let resolved_path = ensure_dataset_sample_path(media_dir, &sample_path).or_else(|| {
        if is_safe_id(&sample.id) {
            ensure_dataset_sample_path(
                media_dir,
                &media_dir
                    .join("dataset")
                    .join(format!("{}.webp", sample.id)),
            )
        } else {
            None
        }
    });
    if let Some(safe_path) = resolved_path
        && let Some(raw_bytes) =
            read_bounded_non_symlink_bytes(&safe_path, MAX_DATASET_SAMPLE_FILE_BYTES).await
    {
        let (redacted, _) =
            redact_image_bytes_with_privacy_mask_async(raw_bytes, priv_mask, false).await;
        if let Some(fname) = safe_path.file_name().and_then(|s| s.to_str()) {
            if let Err(err) =
                atomic_write_within_media_subdir(media_dir, "dataset", fname, &redacted).await
            {
                warn!("Failed to persist redacted dataset sample '{fname}': {err:#}");
                let _ = tokio::fs::remove_file(&safe_path).await;
                let clips_mirror = media_dir.join("clips").join("dataset").join(fname);
                if let Some(safe_mirror) = ensure_clips_artifact_path(media_dir, &clips_mirror) {
                    let _ = tokio::fs::remove_file(&safe_mirror).await;
                }
            } else {
                let clips_dataset_dir = media_dir.join("clips").join("dataset");
                if let Err(err) = crate::pipeline::write_contained_media_file(
                    media_dir,
                    &clips_dataset_dir,
                    fname,
                    &redacted,
                ) {
                    warn!(
                        "Failed to persist redacted dataset mirror sample 'clips/dataset/{fname}': {err:#}"
                    );
                    let clips_mirror = clips_dataset_dir.join(fname);
                    if let Some(safe_mirror) = ensure_clips_artifact_path(media_dir, &clips_mirror)
                    {
                        let _ = tokio::fs::remove_file(&safe_mirror).await;
                    }
                }
            }
        }
    }
}

pub async fn resolve_event_clean_snapshot_bytes(
    media_dir: &Path,
    ev: &Value,
    event_id: &str,
) -> (Vec<u8>, u32, u32) {
    let (bytes, w, h, _) = resolve_event_clean_snapshot_with_ir(media_dir, ev, event_id).await;
    (bytes, w, h)
}

pub async fn resolve_event_clean_snapshot_with_ir(
    media_dir: &Path,
    ev: &Value,
    event_id: &str,
) -> (Vec<u8>, u32, u32, bool) {
    let camera = ev["camera"].as_str().unwrap_or("camera");
    let clips_dir = media_dir.join("clips");
    let mut candidates = Vec::with_capacity(4);
    if is_safe_id(camera) && is_safe_id(event_id) {
        candidates.push(clips_dir.join(format!("{camera}-{event_id}-clean.webp")));
        candidates.push(clips_dir.join(format!("{camera}-{event_id}.webp")));
    }
    if is_safe_id(event_id) {
        candidates.push(clips_dir.join(format!("{event_id}-clean.jpg")));
        candidates.push(clips_dir.join(format!("{event_id}.jpg")));
    }
    let mut found_bytes: Option<Vec<u8>> = None;
    for path in candidates {
        if let Some(safe_path) = ensure_clips_artifact_path(media_dir, &path)
            && let Some(bytes) =
                read_bounded_non_symlink_bytes(&safe_path, MAX_DATASET_SAMPLE_FILE_BYTES).await
            && !bytes.is_empty()
        {
            found_bytes = Some(bytes);
            break;
        }
    }
    if found_bytes.is_none()
        && let Some(b64) = ev["thumbnail"].as_str()
        && !b64.is_empty()
        && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
        && !bytes.is_empty()
    {
        found_bytes = Some(bytes);
    }
    let bytes = found_bytes.unwrap_or_else(|| {
        let img = RgbImage::from_pixel(64, 36, Rgb([24, 24, 24]));
        encode_image_webp(&img).unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec())
    });
    tokio::task::spawn_blocking(move || {
        let mut out_bytes = bytes;
        let mut w = 1280u32;
        let mut h = 720u32;
        let mut is_ir = false;
        if let Some(rgb) = decode_bounded_snapshot_rgb(&out_bytes) {
            w = rgb.width();
            h = rgb.height();
            is_ir = is_rgb_image_monochrome_ir(&rgb);
            if out_bytes.starts_with(&[0xFF, 0xD8])
                && let Ok(webp) = encode_image_webp(&rgb)
            {
                out_bytes = webp;
            }
        }
        (out_bytes, w, h, is_ir)
    })
    .await
    .unwrap_or_else(|_| {
        let img = RgbImage::from_pixel(64, 36, Rgb([24, 24, 24]));
        let fallback =
            encode_image_webp(&img).unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec());
        (fallback, 1280, 720, false)
    })
}

pub async fn create_dataset_sample_from_event(
    db: &NinjaDatabase,
    media_dir: &Path,
    event_id: &str,
    sample_kind: &str,
    override_annotations: Option<Vec<DatasetAnnotationBox>>,
) -> Result<DatasetSampleDto> {
    create_dataset_sample_from_event_with_config(
        None,
        db,
        media_dir,
        event_id,
        sample_kind,
        override_annotations,
    )
    .await
}

pub async fn create_dataset_sample_from_event_with_config(
    config: Option<&Arc<RwLock<NinjaConfig>>>,
    db: &NinjaDatabase,
    media_dir: &Path,
    event_id: &str,
    sample_kind: &str,
    override_annotations: Option<Vec<DatasetAnnotationBox>>,
) -> Result<DatasetSampleDto> {
    if !is_safe_id(event_id) {
        anyhow::bail!("Invalid event_id '{event_id}'");
    }
    let Some(ev) = db.get_event(event_id)? else {
        anyhow::bail!("Event '{event_id}' not found");
    };
    let camera = ev["camera"].as_str().unwrap_or("camera").to_string();
    let label = ev["label"].as_str().unwrap_or("person").to_string();
    let score = ev["top_score"]
        .as_f64()
        .or_else(|| ev["data"]["top_score"].as_f64())
        .or_else(|| ev["score"].as_f64())
        .unwrap_or(0.9);

    let (raw_img_bytes, width, height, inferred_night_ir) =
        resolve_event_clean_snapshot_with_ir(media_dir, &ev, event_id).await;
    let priv_mask = if let Some(cfg_lock) = config {
        camera_privacy_mask_value(cfg_lock, &camera).await
    } else if let Some(m) = ev["data"].get("privacy_mask") {
        Some(m.clone())
    } else {
        db.path()
            .parent()
            .and_then(|dir| NinjaConfig::load(dir.join("config.yml")).ok())
            .and_then(|loaded| loaded.cameras.get(&camera).map(|c| c.privacy_mask.clone()))
    };
    let (img_bytes, _) =
        redact_image_bytes_with_privacy_mask_async(raw_img_bytes, priv_mask, false).await;
    let night_mode = ev["data"]["night_mode"]
        .as_bool()
        .unwrap_or(inferred_night_ir);

    let dataset_dir = media_dir.join("dataset");
    let sample_id = if event_id.starts_with("ds_") {
        event_id.to_string()
    } else {
        format!("ds_{event_id}")
    };
    if !is_safe_id(&sample_id) {
        anyhow::bail!("Invalid dataset sample_id '{sample_id}'");
    }
    let sample_fname = format!("{sample_id}.webp");
    atomic_write_within_media_subdir(media_dir, "dataset", &sample_fname, &img_bytes).await?;
    let clips_dataset_dir = media_dir.join("clips").join("dataset");
    if crate::pipeline::write_contained_media_file(
        media_dir,
        &clips_dataset_dir,
        &sample_fname,
        &img_bytes,
    )
    .is_err()
    {
        let clips_mirror = clips_dataset_dir.join(&sample_fname);
        if let Some(safe_mirror) = ensure_clips_artifact_path(media_dir, &clips_mirror) {
            let _ = tokio::fs::remove_file(&safe_mirror).await;
        }
    }
    let img_path = dataset_dir.join(&sample_fname);

    let use_xywh = event_uses_xywh_norm_boxes(&ev);
    let to_yolo = |b: [f64; 4]| -> (f64, f64, f64, f64) {
        if use_xywh {
            xywh_to_yolo_normalized(b)
        } else {
            event_box_to_yolo_normalized(b)
        }
    };

    let annotations = if sample_kind == "hard_negative" {
        Vec::new()
    } else if let Some(explicit) = override_annotations {
        explicit
    } else {
        let mut anns = Vec::new();
        let raw_box = extract_event_box_array(&ev);
        let (cx, cy, w, h) = to_yolo(raw_box);
        let class_id = ninjanvr_db::security_class_id(&label).unwrap_or(0);
        let canonical_label = ninjanvr_db::security_class_label(class_id)
            .unwrap_or("person")
            .to_string();
        anns.push(DatasetAnnotationBox {
            class_id,
            label: canonical_label,
            cx,
            cy,
            w,
            h,
            score,
            source: "event".to_string(),
        });
        if let Some(attrs) = ev["data"]["detected_attributes"].as_array() {
            for attr in attrs {
                let Some(attr_lbl) = attr["label"].as_str() else {
                    continue;
                };
                let Some(attr_cid) = ninjanvr_db::security_class_id(attr_lbl) else {
                    continue;
                };
                if let Some(nb) = attr["norm_bbox"].as_array()
                    && nb.len() >= 4
                {
                    let arr = [
                        nb[0].as_f64().unwrap_or(0.0),
                        nb[1].as_f64().unwrap_or(0.0),
                        nb[2].as_f64().unwrap_or(0.0),
                        nb[3].as_f64().unwrap_or(0.0),
                    ];
                    let (acx, acy, aw, ah) = to_yolo(arr);
                    anns.push(DatasetAnnotationBox {
                        class_id: attr_cid,
                        label: ninjanvr_db::security_class_label(attr_cid)
                            .unwrap_or(attr_lbl)
                            .to_string(),
                        cx: acx,
                        cy: acy,
                        w: aw,
                        h: ah,
                        score: attr["score"].as_f64().unwrap_or(0.9),
                        source: "event".to_string(),
                    });
                }
            }
        }
        for (box_key, extra_cid, extra_lbl) in [
            ("face_box", 1usize, "face"),
            ("license_plate_box", 7usize, "license_plate"),
        ] {
            if !anns.iter().any(|a| a.class_id == extra_cid)
                && let Some(nb) = ev["data"][box_key].as_array()
                && nb.len() >= 4
            {
                let arr = [
                    nb[0].as_f64().unwrap_or(0.0),
                    nb[1].as_f64().unwrap_or(0.0),
                    nb[2].as_f64().unwrap_or(0.0),
                    nb[3].as_f64().unwrap_or(0.0),
                ];
                if arr.iter().any(|&v| v > 0.0) {
                    let (acx, acy, aw, ah) = to_yolo(arr);
                    anns.push(DatasetAnnotationBox {
                        class_id: extra_cid,
                        label: extra_lbl.to_string(),
                        cx: acx,
                        cy: acy,
                        w: aw,
                        h: ah,
                        score,
                        source: "event".to_string(),
                    });
                }
            }
        }
        anns
    };

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let dto = DatasetSampleDto {
        id: sample_id.clone(),
        event_id: Some(event_id.to_string()),
        camera,
        split: ninjanvr_db::deterministic_dataset_split(&sample_id).to_string(),
        sample_kind: sample_kind.to_string(),
        image_path: img_path.to_string_lossy().to_string(),
        width,
        height,
        annotations,
        verified: sample_kind == "hard_negative",
        night_mode,
        created_at,
        verified_by: (sample_kind == "hard_negative")
            .then(|| ninjanvr_db::DATASET_VERIFIED_BY_MANUAL.to_string()),
    };
    db.upsert_dataset_sample(&dto)
}

pub fn sample_dto_to_response_json(sample: &DatasetSampleDto) -> Value {
    let mut base = serde_json::to_value(sample).unwrap_or_else(|_| json!({}));
    if let Some(obj) = base.as_object_mut() {
        obj.insert("success".to_string(), json!(true));
        obj.insert(
            "image_url".to_string(),
            json!(format!("/api/dataset/samples/{}/image.webp", sample.id)),
        );
        obj.insert(
            "sample".to_string(),
            serde_json::to_value(sample).unwrap_or_else(|_| json!({})),
        );
    }
    base
}

pub async fn api_dataset_summary(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Json<Value> {
    if let Some(allow_list) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allow_list.iter().any(|a| a == "*")
    {
        let samples: Vec<_> = state
            .db
            .list_dataset_samples(None, None, None)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| allow_list.iter().any(|a| a == &s.camera))
            .collect();
        return Json(ninjanvr_db::summarize_dataset_samples(&samples));
    }
    let summary = state
        .db
        .get_dataset_summary()
        .unwrap_or_else(|_| json!({ "total_samples": 0 }));
    Json(summary)
}

#[derive(Debug, Deserialize, Default)]
pub struct DatasetSamplesQuery {
    pub camera: Option<String>,
    pub sample_kind: Option<String>,
    pub verified: Option<bool>,
}

pub async fn api_dataset_samples(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(params): Query<DatasetSamplesQuery>,
) -> Json<Value> {
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let normalized_cam = params
        .camera
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("all") && *c != "*");
    if let Some(ref allow_list) = allowed
        && !allow_list.iter().any(|a| a == "*")
        && let Some(req_cam) = normalized_cam
        && !allow_list.iter().any(|a| a == req_cam)
    {
        return Json(json!([]));
    }
    let mut items = state
        .db
        .list_dataset_samples(
            normalized_cam,
            params.sample_kind.as_deref(),
            params.verified,
        )
        .unwrap_or_default();
    if let Some(allow_list) = allowed
        && !allow_list.iter().any(|a| a == "*")
    {
        items.retain(|s| allow_list.iter().any(|a| a == &s.camera));
    }
    Json(json!(items))
}

#[derive(Debug, Deserialize, Default)]
pub struct CreateSampleFromEventBody {
    pub sample_kind: Option<String>,
    pub annotations: Option<Vec<DatasetAnnotationBox>>,
    #[serde(default, alias = "autoAnnotate")]
    pub auto_annotate: Option<bool>,
}

pub async fn api_dataset_sample_from_event(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(event_id): AxumPath<String>,
    body: Option<Json<CreateSampleFromEventBody>>,
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
    if !is_safe_id(&event_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "success": false, "message": "Invalid event ID" })),
        )
            .into_response();
    }
    if let Err(err_resp) = state
        .auth_gate
        .authorize_mutation(
            &state,
            &jar,
            &uri,
            &headers,
            true,
            crate::AccessTarget::EventId(&event_id),
        )
        .await
    {
        return err_resp.into_response();
    }
    let (kind, override_anns, auto_annotate) = match body {
        Some(Json(b)) => (
            b.sample_kind.unwrap_or_else(|| "true_positive".to_string()),
            b.annotations,
            b.auto_annotate.unwrap_or(false),
        ),
        None => ("true_positive".to_string(), None, false),
    };
    match create_dataset_sample_from_event_with_config(
        Some(&state.config),
        &state.db,
        &state.media_dir,
        &event_id,
        &kind,
        override_anns,
    )
    .await
    {
        Ok(sample) => {
            let genai_enabled = {
                let cfg = state.config.read().await;
                cfg.genai.enabled && !cfg.genai.model.trim().is_empty()
            };
            let final_sample =
                if auto_annotate && genai_enabled && sample.sample_kind == "true_positive" {
                    auto_annotate_dataset_sample(&state, &sample.id)
                        .await
                        .unwrap_or(sample)
                } else {
                    sample
                };
            Json(sample_dto_to_response_json(&final_sample)).into_response()
        }
        Err(err) => {
            let msg = err.to_string();
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
    }
}

pub async fn api_dataset_sample_get(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match state.db.get_dataset_sample(&id) {
        Ok(Some(sample)) => {
            if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
                && !allowed.iter().any(|a| a == "*" || a == &sample.camera)
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
                )
                    .into_response();
            }
            Json(sample_dto_to_response_json(&sample)).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Dataset sample not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateDatasetSampleBody {
    pub annotations: Option<Vec<DatasetAnnotationBox>>,
    pub sample_kind: Option<String>,
    pub verified: Option<bool>,
    /// `"ai"` or `"manual"`; anything else (or absent) is attributed to an operator.
    pub verified_by: Option<String>,
    pub split: Option<String>,
}

pub async fn api_dataset_sample_update(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<UpdateDatasetSampleBody>,
) -> Response {
    if let Err(err_resp) = state
        .auth_gate
        .authorize_mutation(
            &state,
            &jar,
            &uri,
            &headers,
            true,
            crate::AccessTarget::DatasetSampleId(&id),
        )
        .await
    {
        return err_resp.into_response();
    }
    match state.db.update_dataset_sample_annotations(
        &id,
        body.annotations.as_deref(),
        body.sample_kind.as_deref(),
        body.verified,
        body.verified_by.as_deref(),
        body.split.as_deref(),
    ) {
        Ok(Some(updated)) => Json(sample_dto_to_response_json(&updated)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Dataset sample not found" })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "success": false, "message": err.to_string() })),
        )
            .into_response(),
    }
}

pub async fn api_dataset_sample_delete(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(err_resp) = state
        .auth_gate
        .authorize_mutation(
            &state,
            &jar,
            &uri,
            &headers,
            true,
            crate::AccessTarget::DatasetSampleId(&id),
        )
        .await
    {
        return err_resp.into_response();
    }
    let sample_opt = state.db.get_dataset_sample(&id).ok().flatten();
    if let Some(sample) = sample_opt {
        delete_sample_artifacts(&state.media_dir, &sample.id, Some(&sample.image_path));
    }
    let deleted = state.db.delete_dataset_sample(&id).unwrap_or(0);
    if deleted == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "success": false, "message": "Dataset sample not found" })),
        )
            .into_response();
    }
    Json(json!({ "success": true, "deleted": deleted, "id": id })).into_response()
}

fn read_bounded_dataset_file_sync(path: &Path) -> Option<Vec<u8>> {
    let bytes = read_bounded_non_symlink_bytes_sync(path, MAX_DATASET_SAMPLE_FILE_BYTES)?;
    if bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

async fn read_bounded_dataset_file(path: &Path) -> Option<Vec<u8>> {
    let path_owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_bounded_dataset_file_sync(&path_owned))
        .await
        .ok()
        .flatten()
}

pub fn load_dataset_sample_image_bytes_sync(
    state: &AppState,
    sample: &DatasetSampleDto,
) -> Vec<u8> {
    let direct_path = PathBuf::from(&sample.image_path);
    if let Some(safe_direct) = ensure_dataset_sample_path(&state.media_dir, &direct_path)
        && let Some(bytes) = read_bounded_dataset_file_sync(&safe_direct)
    {
        return bytes;
    }
    if is_safe_id(&sample.id) {
        let sample_fname = format!("{}.webp", sample.id);
        let dataset_fallback = state.media_dir.join("dataset").join(&sample_fname);
        if let Some(safe_fallback) = ensure_dataset_sample_path(&state.media_dir, &dataset_fallback)
            && let Some(bytes) = read_bounded_dataset_file_sync(&safe_fallback)
        {
            return bytes;
        }
        let clips_mirror = state
            .media_dir
            .join("clips")
            .join("dataset")
            .join(&sample_fname);
        if let Some(safe_mirror) = ensure_clips_artifact_path(&state.media_dir, &clips_mirror)
            && let Some(bytes) = read_bounded_dataset_file_sync(&safe_mirror)
        {
            let dataset_dir = state.media_dir.join("dataset");
            let _ = crate::pipeline::write_contained_media_file(
                &state.media_dir,
                &dataset_dir,
                &sample_fname,
                &bytes,
            );
            return bytes;
        }
    }
    if let Some(ref ev_id) = sample.event_id
        && is_safe_id(ev_id)
        && let Ok(Some(ev)) = state.db.get_event(ev_id)
        && ev["camera"].as_str() == Some(sample.camera.as_str())
    {
        let clips_dir = state.media_dir.join("clips");
        for cand in [
            clips_dir.join(format!("{}-{ev_id}-clean.webp", sample.camera)),
            clips_dir.join(format!("{}-{ev_id}.webp", sample.camera)),
            clips_dir.join(format!("{ev_id}-clean.jpg")),
            clips_dir.join(format!("{ev_id}.jpg")),
        ] {
            if let Some(safe_cand) = ensure_clips_artifact_path(&state.media_dir, &cand)
                && let Some(bytes) = read_bounded_dataset_file_sync(&safe_cand)
            {
                return bytes;
            }
        }
        if let Some(b64) = ev["thumbnail"].as_str()
            && !b64.is_empty()
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
            && !bytes.is_empty()
            && (bytes.len() as u64) <= MAX_DATASET_SAMPLE_FILE_BYTES
        {
            return bytes;
        }
    }
    let img = RgbImage::from_pixel(64, 36, Rgb([24, 24, 24]));
    encode_image_webp(&img).unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec())
}

pub async fn load_dataset_sample_image_bytes(
    state: &AppState,
    sample: &DatasetSampleDto,
) -> Vec<u8> {
    let direct_path = PathBuf::from(&sample.image_path);
    if let Some(safe_direct) = ensure_dataset_sample_path(&state.media_dir, &direct_path)
        && let Some(bytes) = read_bounded_dataset_file(&safe_direct).await
    {
        return bytes;
    }
    if is_safe_id(&sample.id) {
        let sample_fname = format!("{}.webp", sample.id);
        let dataset_fallback = state.media_dir.join("dataset").join(&sample_fname);
        if let Some(safe_fallback) = ensure_dataset_sample_path(&state.media_dir, &dataset_fallback)
            && let Some(bytes) = read_bounded_dataset_file(&safe_fallback).await
        {
            return bytes;
        }
        let clips_mirror = state
            .media_dir
            .join("clips")
            .join("dataset")
            .join(&sample_fname);
        if let Some(safe_mirror) = ensure_clips_artifact_path(&state.media_dir, &clips_mirror)
            && let Some(bytes) = read_bounded_dataset_file(&safe_mirror).await
        {
            let _ = atomic_write_within_media_subdir(
                &state.media_dir,
                "dataset",
                &sample_fname,
                &bytes,
            )
            .await;
            return bytes;
        }
    }
    if let Some(ref ev_id) = sample.event_id
        && is_safe_id(ev_id)
        && let Ok(Some(ev)) = state.db.get_event(ev_id)
        && ev["camera"].as_str() == Some(sample.camera.as_str())
    {
        let (raw_bytes, _, _) =
            resolve_event_clean_snapshot_bytes(&state.media_dir, &ev, ev_id).await;
        let priv_mask = camera_privacy_mask_value(&state.config, &sample.camera).await;
        let (redacted_bytes, _) =
            redact_image_bytes_with_privacy_mask_async(raw_bytes, priv_mask, false).await;
        if is_safe_id(&sample.id) {
            let sample_fname = format!("{}.webp", sample.id);
            let _ = atomic_write_within_media_subdir(
                &state.media_dir,
                "dataset",
                &sample_fname,
                &redacted_bytes,
            )
            .await;
            let media_dir_owned = state.media_dir.clone();
            let fname_owned = sample_fname.clone();
            let bytes_for_mirror = redacted_bytes.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let clips_dataset_dir = media_dir_owned.join("clips").join("dataset");
                if crate::pipeline::write_contained_media_file(
                    &media_dir_owned,
                    &clips_dataset_dir,
                    &fname_owned,
                    &bytes_for_mirror,
                )
                .is_err()
                {
                    let mirror_p = clips_dataset_dir.join(&fname_owned);
                    if let Some(safe_m) = ensure_clips_artifact_path(&media_dir_owned, &mirror_p) {
                        let _ = std::fs::remove_file(safe_m);
                    }
                }
            })
            .await;
        }
        return redacted_bytes;
    }
    let img = RgbImage::from_pixel(64, 36, Rgb([24, 24, 24]));
    encode_image_webp(&img).unwrap_or_else(|_| b"RIFF\x1a\x00\x00\x00WEBPVP8 ".to_vec())
}

pub async fn api_dataset_sample_image_webp(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !is_safe_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(Some(sample)) = state.db.get_dataset_sample(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(allowed) = effective_caller_allowed_cameras(&state, &jar, &headers).await
        && !allowed.iter().any(|a| a == "*" || a == &sample.camera)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Camera is not permitted for this user" })),
        )
            .into_response();
    }
    let priv_mask = camera_privacy_mask_value(&state.config, &sample.camera).await;
    let raw_bytes = load_dataset_sample_image_bytes(&state, &sample).await;
    let (bytes, _) = redact_image_bytes_with_privacy_mask_async(raw_bytes, priv_mask, false).await;
    ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response()
}

pub async fn auto_annotate_dataset_sample(
    state: &AppState,
    sample_id: &str,
) -> Result<DatasetSampleDto> {
    if !is_safe_id(sample_id) {
        anyhow::bail!("Invalid dataset sample_id '{sample_id}'");
    }
    let Some(sample) = state.db.get_dataset_sample(sample_id)? else {
        anyhow::bail!("Dataset sample '{sample_id}' not found");
    };
    let raw_img_bytes = load_dataset_sample_image_bytes(state, &sample).await;
    let priv_mask = camera_privacy_mask_value(&state.config, &sample.camera).await;
    let (img_bytes, mime_type) =
        redact_image_bytes_with_privacy_mask_async(raw_img_bytes, priv_mask, false).await;
    let img_b64 = base64::engine::general_purpose::STANDARD.encode(&img_bytes);

    let genai = state.config.read().await.genai.clone();
    let classes_list = ninjanvr_db::NINJANVR_SECURITY_CLASSES.join(", ");
    let prompt = format!(
        "You are a Teacher Vision AI auto-annotator for security camera '{}'. \
         Detect all visible objects belonging ONLY to these 15 canonical classes: [{}]. \
         Whenever a person is visible, annotate both 'person' (full body) and 'face' (head/face region if visible), \
         plus any visible backpack, package, dog, cat, car, license_plate, etc. \
         Respond ONLY with JSON containing {{\"detections\": [{{\"label\": \"class_name\", \"x_min\": 0.10, \"y_min\": 0.20, \"x_max\": 0.35, \"y_max\": 0.85, \"bbox\": [x_min, y_min, x_max, y_max], \"score\": 0.95}}]}} \
         where coordinates are normalized in [0.0, 1.0].",
        sample.camera, classes_list
    );
    let req = build_vision_ai_request(&genai, &prompt, &img_b64, mime_type, 0.1, 1024)
        .map_err(|e| anyhow::anyhow!(e))?;
    let timeout = effective_vision_ai_timeout(&genai);
    let resp_json = state
        .dataset_catalog
        .vision_judge
        .transport()
        .send_json(&req, timeout)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    let annotations = parse_ai_auto_annotations(&resp_json, sample.width, sample.height)
        .map_err(|e| anyhow::anyhow!(e))?;
    let updated = state
        .db
        .update_dataset_sample_annotations(
            sample_id,
            Some(&annotations),
            Some("ai_annotated"),
            Some(true),
            Some(ninjanvr_db::DATASET_VERIFIED_BY_AI),
            None,
        )?
        .context("dataset sample disappeared during auto-annotation")?;
    Ok(updated)
}

pub async fn api_dataset_sample_auto_annotate(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if let Err(err_resp) = state
        .auth_gate
        .authorize_mutation(
            &state,
            &jar,
            &uri,
            &headers,
            true,
            crate::AccessTarget::DatasetSampleId(&id),
        )
        .await
    {
        return err_resp.into_response();
    }
    match auto_annotate_dataset_sample(&state, &id).await {
        Ok(updated) => Json(sample_dto_to_response_json(&updated)).into_response(),
        Err(err) => {
            let genai_cfg = state.config.read().await.genai.clone();
            let api_key = resolve_safe_genai_api_key(&genai_cfg);
            let msg = scrub_genai_secret_in_error(format!("{err:#}"), &api_key);
            let status = if msg.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_GATEWAY
            };
            (status, Json(json!({ "success": false, "message": msg }))).into_response()
        }
    }
}

pub async fn api_dataset_batch_auto_annotate(
    State(state): State<AppState>,
    jar: CookieJar,
    uri: Uri,
    headers: HeaderMap,
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
    if !caller_can_mutate(&state, &jar, &headers, true).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "success": false, "message": "Admin role required" })),
        )
            .into_response();
    }
    let Ok(_permit) = state
        .dataset_catalog
        .batch_auto_annotate_semaphore
        .try_acquire()
    else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "success": false,
                "message": "A batch auto-annotation job is already in progress"
            })),
        )
            .into_response();
    };
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let mut candidates = state
        .db
        .list_dataset_samples(None, None, Some(false))
        .unwrap_or_default();
    if let Some(ref allow_list) = allowed
        && !allow_list.iter().any(|a| a == "*")
    {
        candidates.retain(|s| allow_list.iter().any(|a| a == &s.camera));
    }
    let mut updated_samples = Vec::new();
    for s in candidates
        .into_iter()
        .filter(|s| s.sample_kind != "hard_negative")
        .take(MAX_BATCH_AUTO_ANNOTATE_SAMPLES)
    {
        if let Ok(updated) = auto_annotate_dataset_sample(&state, &s.id).await {
            updated_samples.push(updated);
        }
    }
    Json(json!({
        "success": true,
        "annotated_count": updated_samples.len(),
        "samples": updated_samples
    }))
    .into_response()
}

// ============================================================================
// Part 4 — Hardened Stored ZIP Archive Builder & Parser
// ============================================================================

pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

pub fn is_safe_zip_entry_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 1024
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(|c| c.is_control())
    {
        return false;
    }
    name.split('/').all(|seg| {
        !seg.is_empty() && !seg.starts_with('.') && seg != "." && seg != ".." && !seg.contains("..")
    })
}

pub struct ZipArchiveBuilder {
    buf: Vec<u8>,
    central_records: Vec<(String, u32, u32, u32)>,
    central_dir_bytes: usize,
}

impl Default for ZipArchiveBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ZipArchiveBuilder {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            central_records: Vec::new(),
            central_dir_bytes: 0,
        }
    }

    pub fn can_fit_entry(&self, name: &str, data_len: usize) -> bool {
        if !is_safe_zip_entry_name(name)
            || self.central_records.len() >= u16::MAX as usize
            || u32::try_from(data_len).is_err()
            || u32::try_from(self.buf.len()).is_err()
        {
            return false;
        }
        let name_bytes = name.as_bytes();
        if u16::try_from(name_bytes.len()).is_err() {
            return false;
        }
        let Some(local_entry_bytes) = 30usize
            .checked_add(name_bytes.len())
            .and_then(|v| v.checked_add(data_len))
        else {
            return false;
        };
        let Some(cd_record_bytes) = 46usize.checked_add(name_bytes.len()) else {
            return false;
        };
        let Some(next_cd_bytes) = self.central_dir_bytes.checked_add(cd_record_bytes) else {
            return false;
        };
        self.buf
            .len()
            .checked_add(local_entry_bytes)
            .and_then(|v| v.checked_add(next_cd_bytes))
            .and_then(|v| v.checked_add(22))
            .is_some_and(|total| total <= u32::MAX as usize)
    }

    pub fn push_entry(&mut self, name: &str, data: &[u8]) -> bool {
        if !self.can_fit_entry(name, data.len()) {
            return false;
        }
        let Ok(size) = u32::try_from(data.len()) else {
            return false;
        };
        let Ok(offset) = u32::try_from(self.buf.len()) else {
            return false;
        };
        let name_bytes = name.as_bytes();
        let Ok(name_len_u16) = u16::try_from(name_bytes.len()) else {
            return false;
        };
        let crc = crc32_ieee(data);

        self.buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        self.buf.extend_from_slice(&20u16.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&0x5421u16.to_le_bytes());
        self.buf.extend_from_slice(&crc.to_le_bytes());
        self.buf.extend_from_slice(&size.to_le_bytes());
        self.buf.extend_from_slice(&size.to_le_bytes());
        self.buf.extend_from_slice(&name_len_u16.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(name_bytes);
        self.buf.extend_from_slice(data);

        self.central_dir_bytes = self.central_dir_bytes.saturating_add(46 + name_bytes.len());
        self.central_records
            .push((name.to_string(), crc, size, offset));
        true
    }

    pub fn finish(mut self) -> Vec<u8> {
        let central_dir_start = self.buf.len();
        let Ok(central_dir_offset) = u32::try_from(central_dir_start) else {
            return Vec::new();
        };
        for (name, crc, size, local_offset) in &self.central_records {
            let name_bytes = name.as_bytes();
            let Ok(name_len_u16) = u16::try_from(name_bytes.len()) else {
                continue;
            };
            self.buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            self.buf.extend_from_slice(&20u16.to_le_bytes());
            self.buf.extend_from_slice(&20u16.to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes());
            self.buf.extend_from_slice(&0x5421u16.to_le_bytes());
            self.buf.extend_from_slice(&crc.to_le_bytes());
            self.buf.extend_from_slice(&size.to_le_bytes());
            self.buf.extend_from_slice(&size.to_le_bytes());
            self.buf.extend_from_slice(&name_len_u16.to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes());
            self.buf.extend_from_slice(&0u16.to_le_bytes());
            self.buf.extend_from_slice(&0u32.to_le_bytes());
            self.buf.extend_from_slice(&local_offset.to_le_bytes());
            self.buf.extend_from_slice(name_bytes);
        }
        let central_dir_len = self.buf.len().saturating_sub(central_dir_start);
        let Ok(central_dir_size) = u32::try_from(central_dir_len) else {
            return Vec::new();
        };
        let entry_count_u16 = u16::try_from(self.central_records.len()).unwrap_or(u16::MAX);

        self.buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());
        self.buf.extend_from_slice(&entry_count_u16.to_le_bytes());
        self.buf.extend_from_slice(&entry_count_u16.to_le_bytes());
        self.buf.extend_from_slice(&central_dir_size.to_le_bytes());
        self.buf
            .extend_from_slice(&central_dir_offset.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes());

        self.buf
    }
}

pub fn build_zip_archive(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut builder = ZipArchiveBuilder::new();
    for (name, data) in entries {
        let _ = builder.push_entry(name, data);
    }
    builder.finish()
}

pub fn parse_and_validate_zip_archive(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    if bytes.len() < 22 {
        anyhow::bail!("ZIP archive too small");
    }
    let eocd_pos = bytes
        .windows(4)
        .rposition(|w| w == 0x0605_4b50u32.to_le_bytes())
        .context("missing ZIP End of Central Directory signature")?;
    if eocd_pos.checked_add(22).is_none_or(|end| end > bytes.len()) {
        anyhow::bail!("truncated ZIP EOCD header");
    }
    let disk_num = u16::from_le_bytes([bytes[eocd_pos + 4], bytes[eocd_pos + 5]]);
    let cd_disk_num = u16::from_le_bytes([bytes[eocd_pos + 6], bytes[eocd_pos + 7]]);
    let disk_entries = u16::from_le_bytes([bytes[eocd_pos + 8], bytes[eocd_pos + 9]]) as usize;
    let total_entries = u16::from_le_bytes([bytes[eocd_pos + 10], bytes[eocd_pos + 11]]) as usize;
    if disk_num != 0 || cd_disk_num != 0 || disk_entries != total_entries {
        anyhow::bail!("multi-disk or inconsistent ZIP EOCD header is not allowed");
    }
    if total_entries > MAX_ZIP_ARCHIVE_ENTRIES {
        anyhow::bail!("ZIP archive entry count ({total_entries}) exceeds maximum limit");
    }
    let cd_size = u32::from_le_bytes([
        bytes[eocd_pos + 12],
        bytes[eocd_pos + 13],
        bytes[eocd_pos + 14],
        bytes[eocd_pos + 15],
    ]) as usize;
    let cd_offset = u32::from_le_bytes([
        bytes[eocd_pos + 16],
        bytes[eocd_pos + 17],
        bytes[eocd_pos + 18],
        bytes[eocd_pos + 19],
    ]) as usize;
    let eocd_comment_len =
        u16::from_le_bytes([bytes[eocd_pos + 20], bytes[eocd_pos + 21]]) as usize;
    if eocd_pos
        .checked_add(22)
        .and_then(|v| v.checked_add(eocd_comment_len))
        != Some(bytes.len())
    {
        anyhow::bail!("invalid ZIP EOCD comment length or trailing archive data");
    }
    if cd_offset > eocd_pos || cd_offset.checked_add(cd_size) != Some(eocd_pos) {
        anyhow::bail!("invalid ZIP Central Directory offset or size");
    }

    let mut out = BTreeMap::new();
    let mut cursor = cd_offset;
    let mut cumulative_extracted: usize = 0;
    let mut occupied_ranges: Vec<(usize, usize)> = Vec::with_capacity(total_entries.min(256));
    for _ in 0..total_entries {
        let Some(hdr_end) = cursor.checked_add(46) else {
            anyhow::bail!("integer overflow in Central Directory cursor");
        };
        if hdr_end > eocd_pos || hdr_end > bytes.len() {
            anyhow::bail!("truncated Central Directory record");
        }
        let sig = u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]);
        if sig != 0x0201_4b50 {
            anyhow::bail!("invalid Central Directory signature");
        }
        let cd_flags = u16::from_le_bytes([bytes[cursor + 8], bytes[cursor + 9]]);
        if (cd_flags & 0x0009) != 0 {
            anyhow::bail!("encrypted or streaming data-descriptor ZIP entries are not allowed");
        }
        let cd_method = u16::from_le_bytes([bytes[cursor + 10], bytes[cursor + 11]]);
        if cd_method != 0 {
            anyhow::bail!("unsupported ZIP compression method {cd_method} (only Stored=0 allowed)");
        }
        let expected_crc = u32::from_le_bytes([
            bytes[cursor + 16],
            bytes[cursor + 17],
            bytes[cursor + 18],
            bytes[cursor + 19],
        ]);
        let size = u32::from_le_bytes([
            bytes[cursor + 20],
            bytes[cursor + 21],
            bytes[cursor + 22],
            bytes[cursor + 23],
        ]) as usize;
        let uncompressed_size = u32::from_le_bytes([
            bytes[cursor + 24],
            bytes[cursor + 25],
            bytes[cursor + 26],
            bytes[cursor + 27],
        ]) as usize;
        if size != uncompressed_size {
            anyhow::bail!("mismatched compressed and uncompressed size in Stored ZIP entry");
        }
        let name_len = u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[cursor + 30], bytes[cursor + 31]]) as usize;
        let comment_len = u16::from_le_bytes([bytes[cursor + 32], bytes[cursor + 33]]) as usize;
        let local_offset = u32::from_le_bytes([
            bytes[cursor + 42],
            bytes[cursor + 43],
            bytes[cursor + 44],
            bytes[cursor + 45],
        ]) as usize;

        let Some(name_end) = hdr_end.checked_add(name_len) else {
            anyhow::bail!("integer overflow in Central Directory entry name");
        };
        let Some(next_cursor) = name_end
            .checked_add(extra_len)
            .and_then(|v| v.checked_add(comment_len))
        else {
            anyhow::bail!("integer overflow in Central Directory record length");
        };
        if name_end > bytes.len() || next_cursor > eocd_pos || next_cursor > bytes.len() {
            anyhow::bail!("truncated Central Directory entry metadata");
        }
        let name = String::from_utf8(bytes[hdr_end..name_end].to_vec())?;
        if !is_safe_zip_entry_name(&name) {
            anyhow::bail!("unsafe or traversal ZIP entry path '{name}'");
        }
        if out.contains_key(&name) {
            anyhow::bail!("duplicate ZIP entry path '{name}'");
        }
        cursor = next_cursor;

        let Some(local_hdr_end) = local_offset.checked_add(30) else {
            anyhow::bail!("invalid local header offset for {name}");
        };
        if local_hdr_end > cd_offset || local_hdr_end > bytes.len() {
            anyhow::bail!("invalid local header offset for {name}");
        }
        let local_sig = u32::from_le_bytes([
            bytes[local_offset],
            bytes[local_offset + 1],
            bytes[local_offset + 2],
            bytes[local_offset + 3],
        ]);
        if local_sig != 0x0403_4b50 {
            anyhow::bail!("invalid local file header signature for {name}");
        }
        let local_flags = u16::from_le_bytes([bytes[local_offset + 6], bytes[local_offset + 7]]);
        if (local_flags & 0x0009) != 0 {
            anyhow::bail!("encrypted or streaming data-descriptor local header is not allowed");
        }
        let local_method = u16::from_le_bytes([bytes[local_offset + 8], bytes[local_offset + 9]]);
        if local_method != 0 {
            anyhow::bail!("unsupported local header compression method {local_method} for {name}");
        }
        let l_crc = u32::from_le_bytes([
            bytes[local_offset + 14],
            bytes[local_offset + 15],
            bytes[local_offset + 16],
            bytes[local_offset + 17],
        ]);
        let l_comp_size = u32::from_le_bytes([
            bytes[local_offset + 18],
            bytes[local_offset + 19],
            bytes[local_offset + 20],
            bytes[local_offset + 21],
        ]) as usize;
        let l_uncomp_size = u32::from_le_bytes([
            bytes[local_offset + 22],
            bytes[local_offset + 23],
            bytes[local_offset + 24],
            bytes[local_offset + 25],
        ]) as usize;
        if l_crc != expected_crc || l_comp_size != size || l_uncomp_size != uncompressed_size {
            anyhow::bail!(
                "local file header metadata mismatch against Central Directory for {name}"
            );
        }
        let l_name_len =
            u16::from_le_bytes([bytes[local_offset + 26], bytes[local_offset + 27]]) as usize;
        let l_extra_len =
            u16::from_le_bytes([bytes[local_offset + 28], bytes[local_offset + 29]]) as usize;
        let Some(l_name_end) = local_hdr_end.checked_add(l_name_len) else {
            anyhow::bail!("truncated data payload for {name}");
        };
        let Some(data_start) = l_name_end.checked_add(l_extra_len) else {
            anyhow::bail!("truncated data payload for {name}");
        };
        let Some(data_end) = data_start.checked_add(size) else {
            anyhow::bail!("truncated data payload for {name}");
        };
        if data_end > cd_offset || data_end > bytes.len() {
            anyhow::bail!("truncated data payload for {name}");
        }
        if &bytes[local_hdr_end..l_name_end] != name.as_bytes() {
            anyhow::bail!("local header filename mismatch for {name}");
        }
        if occupied_ranges
            .iter()
            .any(|&(s, e)| local_offset < e && data_end > s)
        {
            anyhow::bail!("overlapping ZIP local file entry range for {name}");
        }
        occupied_ranges.push((local_offset, data_end));
        cumulative_extracted = cumulative_extracted
            .checked_add(size)
            .context("ZIP archive extracted size overflow")?;
        if cumulative_extracted > bytes.len() || cumulative_extracted > MAX_ZIP_EXTRACTED_BYTES {
            anyhow::bail!("ZIP archive extracted payload exceeds maximum allowed size");
        }
        let payload = bytes[data_start..data_end].to_vec();
        let actual_crc = crc32_ieee(&payload);
        if actual_crc != expected_crc {
            anyhow::bail!("CRC32 mismatch for {name}");
        }
        out.insert(name, payload);
    }
    if cursor != eocd_pos {
        anyhow::bail!("unparsed trailing bytes in ZIP Central Directory");
    }
    Ok(out)
}

pub fn parse_zip_archive(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    parse_and_validate_zip_archive(bytes)
}

pub async fn api_dataset_export_zip(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Response {
    let Ok(_export_permit) = state.dataset_catalog.export_zip_semaphore.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "success": false,
                "message": "Too many dataset export requests in progress"
            })),
        )
            .into_response();
    };
    let allowed = effective_caller_allowed_cameras(&state, &jar, &headers).await;
    let privacy_masks: HashMap<String, Value> = {
        let cfg = state.config.read().await;
        cfg.cameras
            .iter()
            .filter(|(_, v)| has_active_privacy_mask(Some(&v.privacy_mask)))
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::normalize_privacy_mask_value_for_resolution(
                        &v.privacy_mask,
                        v.detect.width.max(1),
                        v.detect.height.max(1),
                    ),
                )
            })
            .collect()
    };
    let state_for_zip = state.clone();
    let Ok(zip_bytes) = tokio::task::spawn_blocking(move || {
        let mut samples = state_for_zip
            .db
            .list_dataset_samples(None, None, None)
            .unwrap_or_default();
        if let Some(ref allow_list) = allowed
            && !allow_list.iter().any(|a| a == "*")
        {
            samples.retain(|s| allow_list.iter().any(|a| a == &s.camera));
        }
        samples.sort_by(|a, b| a.id.cmp(&b.id));

        let mut yaml = String::from(
            "# NinjaNVR Active Learning YOLO Dataset\npath: .\ntrain: images/train\nval: images/val\nnc: 15\nnames:\n",
        );
        for (idx, cls_name) in ninjanvr_db::NINJANVR_SECURITY_CLASSES.iter().enumerate() {
            yaml.push_str(&format!("  {idx}: {cls_name}\n"));
        }

        let mut zip_builder = ZipArchiveBuilder::new();
        let yaml_bytes = yaml.into_bytes();
        let mut cumulative_bytes = yaml_bytes.len();
        let _ = zip_builder.push_entry("data.yaml", &yaml_bytes);
        drop(yaml_bytes);

        for sample in samples {
            if !is_safe_id(&sample.id) {
                continue;
            }
            let split = if sample.split == "val" {
                "val"
            } else {
                "train"
            };
            let raw_img_bytes = load_dataset_sample_image_bytes_sync(&state_for_zip, &sample);
            let priv_mask = privacy_masks.get(&sample.camera);
            let (img_bytes, _) =
                redact_image_bytes_with_privacy_mask(&raw_img_bytes, priv_mask, false);
            drop(raw_img_bytes);

            let label_txt = if sample.sample_kind == "hard_negative" || sample.annotations.is_empty()
            {
                String::new()
            } else {
                let mut lines = Vec::with_capacity(sample.annotations.len());
                for ann in &sample.annotations {
                    lines.push(format!(
                        "{} {:.6} {:.6} {:.6} {:.6}",
                        ann.class_id, ann.cx, ann.cy, ann.w, ann.h
                    ));
                }
                format!("{}\n", lines.join("\n"))
            };
            let label_bytes = label_txt.into_bytes();
            let Some(next_total) = cumulative_bytes
                .checked_add(img_bytes.len())
                .and_then(|v| v.checked_add(label_bytes.len()))
            else {
                break;
            };
            if next_total > MAX_DATASET_EXPORT_ZIP_BYTES {
                break;
            }
            let img_entry_name = format!("images/{split}/{}.webp", sample.id);
            let lbl_entry_name = format!("labels/{split}/{}.txt", sample.id);
            if !zip_builder.push_entry(&img_entry_name, &img_bytes) {
                break;
            }
            drop(img_bytes);
            if !zip_builder.push_entry(&lbl_entry_name, &label_bytes) {
                break;
            }
            drop(label_bytes);
            cumulative_bytes = next_total;
        }

        zip_builder.finish()
    })
    .await
    else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, "application/zip"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"ninjanvr-yolo-dataset.zip\"",
            ),
        ],
        zip_bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_genai_config(
        provider: &str,
        model: &str,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> GenAiConfig {
        GenAiConfig {
            enabled: true,
            provider: provider.to_string(),
            base_url: base_url.map(str::to_string),
            api_key: api_key.map(str::to_string),
            api_key_env: "GEMINI_API_KEY".to_string(),
            model: model.to_string(),
            verify_ambiguous_alerts: true,
            ambiguous_score_min: 0.45,
            ambiguous_score_max: 0.78,
            auto_suppress_false_positives: true,
            timeout_ms: 15_000,
        }
    }

    #[tokio::test]
    async fn build_vision_ai_request_and_extract_response_across_gemini_openai_and_ollama() {
        // 1. Gemini request builder + thinking-model response extraction via FakeVisionAiTransport
        let gemini_cfg = test_genai_config(
            "gemini",
            "gemini-2.5-flash",
            Some("https://generativelanguage.googleapis.com"),
            Some("AIzaTestKey123"),
        );
        let gemini_req = build_vision_ai_request(
            &gemini_cfg,
            "Inspect office camera",
            "UklGRg==",
            "image/webp",
            0.1,
            512,
        )
        .expect("gemini request should build");
        assert_eq!(gemini_req.provider, "gemini");
        assert_eq!(gemini_req.model, "gemini-2.5-flash");
        assert_eq!(
            gemini_req.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent?key=AIzaTestKey123"
        );
        assert_eq!(
            gemini_req.headers,
            vec![("x-goog-api-key".to_string(), "AIzaTestKey123".to_string())]
        );
        assert_eq!(
            gemini_req.body["contents"][0]["parts"][0]["text"],
            "Inspect office camera"
        );
        assert_eq!(
            gemini_req.body["contents"][0]["parts"][1]["inline_data"]["data"],
            "UklGRg=="
        );
        assert_eq!(
            gemini_req.body["generationConfig"]["responseMimeType"],
            "application/json"
        );

        let fake_gemini = FakeVisionAiTransport::with_response(json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "thought": true, "text": "Reasoning about shadows..." },
                        { "text": "```json\n{\"is_true_positive\": false, \"confidence\": 0.96, \"reason\": \"Tree shadow\"}\n```" }
                    ]
                }
            }]
        }));
        let gemini_text =
            execute_vision_ai_prompt(&fake_gemini, &gemini_req, Duration::from_secs(15))
                .await
                .expect("fake gemini transport should succeed");
        assert_eq!(
            gemini_text,
            "{\"is_true_positive\": false, \"confidence\": 0.96, \"reason\": \"Tree shadow\"}"
        );
        assert_eq!(fake_gemini.recorded_requests().len(), 1);
        let (is_tp, conf, reason) =
            parse_ai_judge_json_verdict(&serde_json::from_str(&gemini_text).unwrap()).unwrap();
        assert!(!is_tp);
        assert!((conf - 0.96).abs() < 1e-6);
        assert_eq!(reason, "Tree shadow");

        // 2. OpenAI request builder + response extraction via FakeVisionAiTransport
        let openai_cfg = test_genai_config(
            "openai",
            "gpt-4o-mini",
            Some("https://api.openai.com/v1/"),
            Some("sk-test-openai-key"),
        );
        let openai_req = build_vision_ai_request(
            &openai_cfg,
            "Detect security classes",
            "UklGRg==",
            "image/webp",
            0.2,
            1024,
        )
        .expect("openai request should build");
        assert_eq!(openai_req.provider, "openai");
        assert_eq!(openai_req.url, "https://api.openai.com/v1/chat/completions");
        assert_eq!(
            openai_req.headers,
            vec![(
                "Authorization".to_string(),
                "Bearer sk-test-openai-key".to_string()
            )]
        );
        assert_eq!(
            openai_req.body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/webp;base64,UklGRg=="
        );

        let fake_openai = FakeVisionAiTransport::with_response(json!({
            "choices": [{
                "message": {
                    "content": "```JSON\n{\"detections\": [{\"label\": \"person\", \"bbox\": [0.1, 0.2, 0.5, 0.8], \"score\": 0.94}]}\n```"
                }
            }]
        }));
        let openai_text =
            execute_vision_ai_prompt(&fake_openai, &openai_req, Duration::from_secs(15))
                .await
                .expect("fake openai transport should succeed");
        let parsed_anns =
            parse_ai_auto_annotations(&serde_json::from_str(&openai_text).unwrap(), 1920, 1080)
                .expect("annotations should parse");
        assert_eq!(parsed_anns.len(), 1);
        assert_eq!(parsed_anns[0].label, "person");
        assert!((parsed_anns[0].cx - 0.3).abs() < 1e-5);
        assert!((parsed_anns[0].cy - 0.5).abs() < 1e-5);

        // 3. Ollama request builder + both /api/generate and /api/chat response extraction
        let ollama_cfg = test_genai_config(
            "ollama",
            "qwen2.5vl:7b",
            Some("http://127.0.0.1:11434"),
            None,
        );
        let ollama_req = build_vision_ai_request(
            &ollama_cfg,
            "Verify alert",
            "UklGRg==",
            "image/webp",
            0.0,
            256,
        )
        .expect("ollama request should build");
        assert_eq!(ollama_req.provider, "ollama");
        assert_eq!(ollama_req.url, "http://127.0.0.1:11434/api/generate");
        assert!(ollama_req.headers.is_empty());
        assert_eq!(ollama_req.body["images"][0], "UklGRg==");
        assert_eq!(ollama_req.body["stream"], false);

        let fake_ollama = FakeVisionAiTransport::with_responses([
            Ok(json!({
                "response": "{\"is_true_positive\": true, \"confidence\": 0.91, \"reason\": \"Person at gate\"}"
            })),
            Ok(json!({
                "message": {
                    "content": "```json\n{\"is_true_positive\": false, \"confidence\": 0.88, \"reason\": \"Spider web\"}\n```"
                }
            })),
        ]);
        let ollama_gen_text =
            execute_vision_ai_prompt(&fake_ollama, &ollama_req, Duration::from_secs(10))
                .await
                .unwrap();
        assert!(ollama_gen_text.contains("Person at gate"));
        let ollama_chat_text =
            execute_vision_ai_prompt(&fake_ollama, &ollama_req, Duration::from_secs(10))
                .await
                .unwrap();
        assert_eq!(
            ollama_chat_text,
            "{\"is_true_positive\": false, \"confidence\": 0.88, \"reason\": \"Spider web\"}"
        );
        assert_eq!(fake_ollama.recorded_requests().len(), 2);

        // 4. Rejects empty/unsafe models and SSRF metadata base_urls
        let bad_model_cfg = test_genai_config("gemini", "../evil", None, None);
        assert!(
            build_vision_ai_request(&bad_model_cfg, "p", "img", "image/webp", 0.1, 100).is_err()
        );
        let ssrf_cfg = test_genai_config(
            "openai",
            "gpt-4o-mini",
            Some("http://169.254.169.254/latest/meta-data"),
            None,
        );
        assert!(build_vision_ai_request(&ssrf_cfg, "p", "img", "image/webp", 0.1, 100).is_err());
    }

    #[test]
    fn dataset_artifact_store_writes_single_pass_redacted_mirrors_and_deletes_all_three_paths() {
        let nonce: u64 = rand::random();
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_dataset_catalog_test_{nonce:016x}"));
        std::fs::create_dir_all(&tmp_dir).unwrap();

        // Build a bright white 64x64 source WebP image
        let bright_img = RgbImage::from_pixel(64, 64, Rgb([240, 240, 240]));
        let raw_webp = encode_image_webp(&bright_img).expect("webp encode should succeed");

        // Define a full-frame quad privacy mask
        let masks = [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]];

        // 1. Write sample in a single pass with privacy redaction
        let primary_path = write_redacted_sample_webp(&tmp_dir, "ds_ev_101", &raw_webp, &masks)
            .expect("write_redacted_sample_webp should succeed");
        let mirror_path = tmp_dir.join("clips").join("dataset").join("ds_ev_101.webp");
        assert!(primary_path.is_file(), "primary dataset webp must exist");
        assert!(
            mirror_path.is_file(),
            "clips/dataset mirror webp must exist"
        );

        // Verify both files were redacted in the single write pass (center pixel redacted to matte dark)
        for path in [&primary_path, &mirror_path] {
            let bytes = std::fs::read(path).unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap().to_rgb8();
            let center = decoded.get_pixel(32, 32);
            assert!(
                center[0] < 30 && center[1] < 30 && center[2] < 30,
                "expected redacted dark center pixel in {}, got {center:?}",
                path.display()
            );
        }

        // 2. When recorded_image_path equals the primary path, sample_artifact_paths deduplicates to 2 paths
        let dedup_paths = sample_artifact_paths(&tmp_dir, "ds_ev_101", primary_path.to_str());
        assert_eq!(dedup_paths.len(), 2);

        // 3. Create a distinct legacy recorded_image_path so all 3 canonical artifact locations exist
        let custom_recorded = tmp_dir.join("dataset").join("custom_recorded_ev_101.webp");
        std::fs::write(&custom_recorded, &raw_webp).unwrap();
        let all_three_paths =
            sample_artifact_paths(&tmp_dir, "ds_ev_101", custom_recorded.to_str());
        assert_eq!(all_three_paths.len(), 3);

        // 4. delete_sample_artifacts symmetrically removes all 3 paths
        let removed_count =
            delete_sample_artifacts(&tmp_dir, "ds_ev_101", custom_recorded.to_str());
        assert_eq!(removed_count, 3);
        assert!(!custom_recorded.exists());
        assert!(!primary_path.exists());
        assert!(!mirror_path.exists());

        // 5. Rejects traversal in sample_id and recorded_image_path
        assert!(write_redacted_sample_webp(&tmp_dir, "../escape", &raw_webp, &[]).is_err());
        assert!(
            sample_artifact_paths(&tmp_dir, "../escape", Some("../../outside.webp")).is_empty()
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn dataset_catalog_instance_semaphores_and_zip_archive_roundtrip() {
        let cat_a = DatasetCatalog::new();
        let cat_b = DatasetCatalog::new();

        // Acquiring batch_auto_annotate_semaphore on instance A does not block instance B
        let _permit_a = cat_a
            .batch_auto_annotate_semaphore
            .try_acquire()
            .expect("instance A should acquire batch semaphore");
        assert!(
            cat_a.batch_auto_annotate_semaphore.try_acquire().is_err(),
            "instance A second acquire must fail while held"
        );
        assert!(
            cat_b.batch_auto_annotate_semaphore.try_acquire().is_ok(),
            "instance B batch semaphore must be independent of instance A"
        );

        // Stored ZIP builder + parser round-trip and traversal rejection
        let zip_bytes = build_zip_archive(&[
            ("data.yaml".to_string(), b"nc: 15\n".to_vec()),
            ("images/train/ds_1.webp".to_string(), b"RIFFWEBP".to_vec()),
        ]);
        let parsed = parse_and_validate_zip_archive(&zip_bytes).expect("valid zip should parse");
        assert_eq!(parsed.get("data.yaml").unwrap(), b"nc: 15\n");
        assert_eq!(parsed.get("images/train/ds_1.webp").unwrap(), b"RIFFWEBP");
    }
}
