//! Deepened dataset curation & Vision AI judgment catalog (`DatasetCatalog`).
//!
//! ## Design-It-Twice Comparison (`/codebase-design`)
//!
//! - **Design A — Unified `VisionAiJudge` Port + `DatasetArtifactStore` (Selected):**
//!   Separates external Vision AI evaluation (`VisionAiJudge` + `VisionAiTransport` port with
//!   `ReqwestVisionAiTransport` and in-memory `FakeVisionAiTransport` adapters) from local media
//!   filesystem persistence (`DatasetArtifactStore`: single-pass privacy redaction + symmetric
//!   3-path artifact resolution and deletion). Both boundaries are narrow, `local-substitutable`,
//!   and testable in milliseconds without network sockets or SQLite scaffolding.
//! - **Design B — Monolithic `DatasetFacade` (Rejected):**
//!   Couples `reqwest::Client`, `NinjaDatabase`, `RwLock<NinjaConfig>`, and WebP encoding inside
//!   one facade struct. Every unit test would require spinning up a full database + TCP server,
//!   creating a shallow pass-through interface with poor locality.

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use image::{Rgb, RgbImage};
use ninjanvr_config::GenAiConfig;
use ninjanvr_db::DatasetAnnotationBox;
use ninjanvr_vision::{PolygonMask, apply_privacy_mask_rgb, encode_image_webp};
use serde_json::{Value, json};

/// Maximum response payload size accepted from a Vision AI provider (2 MiB).
pub const MAX_GENAI_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// Maximum on-disk size for a single dataset sample image (16 MiB).
pub const MAX_DATASET_SAMPLE_FILE_BYTES: u64 = 16 * 1024 * 1024;

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

fn is_disallowed_genai_ipv4(v4: std::net::Ipv4Addr) -> bool {
    let octs = v4.octets();
    v4.is_unspecified()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_multicast()
        || (octs[0] == 169 && octs[1] == 254)
        || octs == [100, 100, 100, 200]
        || octs == [192, 0, 0, 192]
}

fn is_disallowed_genai_ipv6(v6: std::net::Ipv6Addr) -> bool {
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
    v6.is_unspecified()
        || v6.is_multicast()
        || (segs[0] & 0xffc0) == 0xfe80
        || (segs[0] & 0xfe00) == 0xfc00
        || (segs[0] == 0x0064 && segs[1] == 0xff9b)
        || segs[0] == 0x2002
        || (segs[0] == 0x2001 && (segs[1] == 0x0000 || segs[1] == 0x0db8))
        || siit_v4.is_some_and(is_disallowed_genai_ipv4)
        || v6.to_ipv4().is_some_and(is_disallowed_genai_ipv4)
        || v6.to_ipv4_mapped().is_some_and(is_disallowed_genai_ipv4)
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
    }) && clean_host.parse::<std::net::Ipv4Addr>().is_err()
    {
        return Err("Disallowed or invalid GenAI base_url".to_string());
    }
    if let Ok(ip) = clean_host.parse::<std::net::IpAddr>() {
        let disallowed = match ip {
            std::net::IpAddr::V4(v4) => is_disallowed_genai_ipv4(v4),
            std::net::IpAddr::V6(v6) => is_disallowed_genai_ipv6(v6),
        };
        if disallowed {
            return Err("Disallowed or invalid GenAI base_url".to_string());
        }
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
}
