use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{StatusCode, header};
use base64::Engine;
use ninjanvr_config::scrub_rtsp_credentials;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const MAX_ONVIF_SOAP_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_ONVIF_PRESETS: usize = 256;
pub const MAX_TALKBACK_AUDIO_BYTES: usize = 512 * 1024;

/// Declarative specification for a camera hardware/control protocol family.
/// Adding support for a new camera vendor or protocol family only requires
/// registering a `CameraProtocolSpec` entry in `CAMERA_PROTOCOL_REGISTRY`.
#[derive(Debug, Clone, Copy)]
pub struct CameraProtocolSpec {
    pub id: &'static str,
    pub display_name: &'static str,
    pub aliases: &'static [&'static str],
    pub default_onvif_port: u16,
    pub default_http_port: u16,
    pub rtsp_path_markers: &'static [&'static str],
    pub manufacturer_markers: &'static [&'static str],
    pub model_markers: &'static [&'static str],
    pub ptz_service_path: &'static str,
    pub imaging_endpoints: &'static [&'static str],
    pub preferred_profile_tokens: &'static [&'static str],
    pub preferred_video_source_tokens: &'static [&'static str],
    pub native_substream_template: Option<&'static str>,
    pub default_talkback_mode: Option<&'static str>,
    pub supports_dahua_cgi: bool,
    pub pan_tilt_only_default: bool,
}

pub const CAMERA_PROTOCOL_REGISTRY: &[CameraProtocolSpec] = &[
    CameraProtocolSpec {
        id: "tapo",
        display_name: "TP-Link Tapo (ONVIF :2020 + Tapo Backchannel)",
        aliases: &["tp-link", "tplink", "tapo_onvif", "tapo_klap"],
        default_onvif_port: 2020,
        default_http_port: 443,
        rtsp_path_markers: &["/stream1", "/stream2"],
        manufacturer_markers: &["tp-link", "tplink"],
        model_markers: &["tapo"],
        ptz_service_path: "/onvif/service",
        // Tapo C200/C210/C220 ONVIF on port 2020 does not implement ONVIF Imaging service
        imaging_endpoints: &[],
        preferred_profile_tokens: &["profile_1", "Profile_1", "Profile000"],
        preferred_video_source_tokens: &["raw_vs1", "VideoSource000", "00000"],
        native_substream_template: Some("rtsp://*:*@{host}:554/stream2#transport=tcp#timeout=10"),
        default_talkback_mode: None,
        supports_dahua_cgi: false,
        pan_tilt_only_default: true,
    },
    CameraProtocolSpec {
        id: "dahua_cgi",
        display_name: "Intelbras / Dahua / Amcrest / Imou (ONVIF + HTTP Digest CGI)",
        aliases: &["intelbras", "dahua", "amcrest", "imou", "mibo"],
        default_onvif_port: 80,
        default_http_port: 80,
        rtsp_path_markers: &["/cam/realmonitor"],
        manufacturer_markers: &["intelbras", "dahua", "amcrest", "imou"],
        model_markers: &["ime-", "im3", "im4", "im5", "mibo", "ipc-"],
        ptz_service_path: "/onvif/ptz_service",
        imaging_endpoints: &["/onvif/imaging_service", "/onvif/service"],
        preferred_profile_tokens: &["Profile000", "profile_1", "Profile_1"],
        preferred_video_source_tokens: &["VideoSource000", "00000", "raw_vs1"],
        native_substream_template: Some(
            "rtsp://*:*@{host}:554/cam/realmonitor?channel=1&subtype=1&unicast=true&proto=Onvif#transport=tcp#timeout=10",
        ),
        default_talkback_mode: Some("cgi_alaw"),
        supports_dahua_cgi: true,
        pan_tilt_only_default: true,
    },
    CameraProtocolSpec {
        id: "hikvision_isapi",
        display_name: "Hikvision / HiLook / Annke (ONVIF + ISAPI)",
        aliases: &["hikvision", "isapi", "hilook", "annke", "ezviz"],
        default_onvif_port: 80,
        default_http_port: 80,
        rtsp_path_markers: &["/streaming/channels/", "/h264/ch1/"],
        manufacturer_markers: &["hikvision", "hilook", "annke", "ezviz"],
        model_markers: &["ds-2cd", "ds-2de"],
        ptz_service_path: "/onvif/ptz_service",
        imaging_endpoints: &["/onvif/imaging_service", "/onvif/service"],
        preferred_profile_tokens: &["Profile_1", "Profile000", "profile_1"],
        preferred_video_source_tokens: &["VideoSource_1", "VideoSource000", "raw_vs1"],
        native_substream_template: Some(
            "rtsp://*:*@{host}:554/Streaming/Channels/102#transport=tcp#timeout=10",
        ),
        default_talkback_mode: None,
        supports_dahua_cgi: false,
        pan_tilt_only_default: false,
    },
    CameraProtocolSpec {
        id: "reolink",
        display_name: "Reolink (ONVIF + RTMP/RTSP)",
        aliases: &["reolink_cgi", "baichuan"],
        default_onvif_port: 8000,
        default_http_port: 80,
        rtsp_path_markers: &["/h264preview_", "/h265preview_"],
        manufacturer_markers: &["reolink"],
        model_markers: &["rlc-", "e1 "],
        ptz_service_path: "/onvif/ptz_service",
        imaging_endpoints: &["/onvif/imaging_service", "/onvif/service"],
        preferred_profile_tokens: &["000", "Profile000", "profile_1"],
        preferred_video_source_tokens: &["000", "VideoSource000", "raw_vs1"],
        native_substream_template: Some(
            "rtsp://*:*@{host}:554/h264Preview_01_sub#transport=tcp#timeout=10",
        ),
        default_talkback_mode: None,
        supports_dahua_cgi: false,
        pan_tilt_only_default: false,
    },
    CameraProtocolSpec {
        id: "axis_vapix",
        display_name: "Axis VAPIX / ONVIF",
        aliases: &["axis", "vapix"],
        default_onvif_port: 80,
        default_http_port: 80,
        rtsp_path_markers: &["/axis-media/media.amp"],
        manufacturer_markers: &["axis"],
        model_markers: &["axis "],
        ptz_service_path: "/onvif/ptz_service",
        imaging_endpoints: &["/onvif/imaging_service", "/onvif/service"],
        preferred_profile_tokens: &["profile_1", "Profile000", "Profile_1"],
        preferred_video_source_tokens: &["VideoSource000", "0", "raw_vs1"],
        native_substream_template: None,
        default_talkback_mode: None,
        supports_dahua_cgi: false,
        pan_tilt_only_default: false,
    },
    CameraProtocolSpec {
        id: "onvif",
        display_name: "Standard ONVIF Profile S / T",
        aliases: &["onvif_s", "onvif_t", "generic_onvif"],
        default_onvif_port: 80,
        default_http_port: 80,
        rtsp_path_markers: &[],
        manufacturer_markers: &[],
        model_markers: &[],
        ptz_service_path: "/onvif/ptz_service",
        imaging_endpoints: &["/onvif/imaging_service", "/onvif/service"],
        preferred_profile_tokens: &["Profile000", "profile_1", "Profile_1"],
        preferred_video_source_tokens: &["VideoSource000", "00000", "raw_vs1"],
        native_substream_template: None,
        default_talkback_mode: None,
        supports_dahua_cgi: false,
        pan_tilt_only_default: false,
    },
    CameraProtocolSpec {
        id: "rtsp_only",
        display_name: "RTSP Only (No Hardware Control)",
        aliases: &["rtsp", "none"],
        default_onvif_port: 80,
        default_http_port: 80,
        rtsp_path_markers: &[],
        manufacturer_markers: &[],
        model_markers: &[],
        ptz_service_path: "/onvif/ptz_service",
        imaging_endpoints: &[],
        preferred_profile_tokens: &["Profile000", "profile_1"],
        preferred_video_source_tokens: &["VideoSource000", "raw_vs1"],
        native_substream_template: None,
        default_talkback_mode: None,
        supports_dahua_cgi: false,
        pan_tilt_only_default: false,
    },
];

pub fn find_protocol_spec_by_id_or_alias(raw: &str) -> Option<&'static CameraProtocolSpec> {
    let lc = raw.trim().to_ascii_lowercase();
    if lc.is_empty() || lc == "auto" {
        return None;
    }
    CAMERA_PROTOCOL_REGISTRY.iter().find(|spec| {
        spec.id == lc
            || spec
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&lc))
    })
}

pub fn infer_protocol_spec_from_device_or_hints(
    explicit_proto: Option<&str>,
    manufacturer: Option<&str>,
    model: Option<&str>,
    is_tapo_hint: bool,
    is_dahua_hint: bool,
    dahua_cgi_ok: bool,
    online: bool,
) -> &'static CameraProtocolSpec {
    if let Some(exp) = explicit_proto.and_then(find_protocol_spec_by_id_or_alias)
        && exp.id != "onvif"
        && exp.id != "rtsp_only"
    {
        return exp;
    }
    if let Some(mfg) = manufacturer {
        let mfg_lc = mfg.to_ascii_lowercase();
        let model_lc = model.unwrap_or("").to_ascii_lowercase();
        for spec in CAMERA_PROTOCOL_REGISTRY {
            if spec.manufacturer_markers.iter().any(|m| mfg_lc.contains(m))
                || spec.model_markers.iter().any(|m| model_lc.contains(m))
            {
                return spec;
            }
        }
    }
    if is_tapo_hint {
        return find_protocol_spec_by_id_or_alias("tapo").unwrap();
    }
    if dahua_cgi_ok || is_dahua_hint {
        return find_protocol_spec_by_id_or_alias("dahua_cgi").unwrap();
    }
    if online {
        find_protocol_spec_by_id_or_alias("onvif").unwrap()
    } else {
        find_protocol_spec_by_id_or_alias("rtsp_only").unwrap()
    }
}

pub fn format_substream_url_for_protocol(
    spec: &CameraProtocolSpec,
    clean_host: &str,
) -> Option<String> {
    spec.native_substream_template
        .map(|tpl| tpl.replace("{host}", clean_host))
}

/// `AppState`-scoped runtime cache for discovered camera ONVIF tokens, protocol families,
/// and HTTP Digest challenge nonces. Replaces process-global `LazyLock` singletons so each
/// server instance and test harness has isolated, invalidatable state.
#[derive(Debug, Clone, Default)]
pub struct CameraDriverRuntime {
    discovered_profiles: Arc<Mutex<HashMap<(String, String), String>>>,
    discovered_video_sources: Arc<Mutex<HashMap<String, String>>>,
    discovered_protocols: Arc<Mutex<HashMap<String, String>>>,
    digest_challenges: Arc<Mutex<HashMap<String, String>>>,
}

impl CameraDriverRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_profile(&self, camera: &str, endpoint: &str) -> Option<String> {
        self.discovered_profiles
            .lock()
            .ok()
            .and_then(|g| g.get(&(camera.to_string(), endpoint.to_string())).cloned())
    }

    pub fn set_profile(&self, camera: &str, endpoint: &str, profile: &str) {
        if let Ok(mut g) = self.discovered_profiles.lock() {
            let key = (camera.to_string(), endpoint.to_string());
            if g.len() >= 128 && !g.contains_key(&key) {
                g.clear();
            }
            g.insert(key, profile.to_string());
        }
    }

    pub fn get_video_source(&self, camera: &str) -> Option<String> {
        self.discovered_video_sources
            .lock()
            .ok()
            .and_then(|g| g.get(camera).cloned())
    }

    pub fn set_video_source(&self, camera: &str, vs_token: &str) {
        if let Ok(mut g) = self.discovered_video_sources.lock() {
            if g.len() >= 128 && !g.contains_key(camera) {
                g.clear();
            }
            g.insert(camera.to_string(), vs_token.to_string());
        }
    }

    pub fn get_protocol(&self, camera: &str) -> Option<String> {
        self.discovered_protocols
            .lock()
            .ok()
            .and_then(|g| g.get(camera).cloned())
    }

    pub fn set_protocol(&self, camera: &str, protocol: &str) {
        if let Ok(mut g) = self.discovered_protocols.lock() {
            if g.len() >= 128 && !g.contains_key(camera) {
                g.clear();
            }
            g.insert(camera.to_string(), protocol.to_string());
        }
    }

    pub fn get_digest_challenge(&self, authority_key: &str) -> Option<String> {
        self.digest_challenges
            .lock()
            .ok()
            .and_then(|g| g.get(authority_key).cloned())
    }

    pub fn set_digest_challenge(&self, authority_key: &str, www_authenticate: &str) {
        if let Ok(mut g) = self.digest_challenges.lock() {
            if g.len() >= 128 && !g.contains_key(authority_key) {
                g.clear();
            }
            g.insert(authority_key.to_string(), www_authenticate.to_string());
        }
    }

    pub fn invalidate_camera(&self, camera: &str) {
        if let Ok(mut g) = self.discovered_profiles.lock() {
            g.retain(|(cam, _), _| cam != camera);
        }
        if let Ok(mut g) = self.discovered_video_sources.lock() {
            g.remove(camera);
        }
        if let Ok(mut g) = self.discovered_protocols.lock() {
            g.remove(camera);
        }
    }
}

/// Declarative list of Dahua/Intelbras CGI tables queried during capability discovery.
pub const DAHUA_CGI_PROBE_TABLES: &[&str] = &[
    "LeSmartTrack",
    "VideoInOptions",
    "Lighting",
    "LightGlobal",
    "MotionDetect",
    "AudioDetect",
    "AudioOutputVolume",
    "AudioInputVolume",
];

/// Parses a Dahua/Intelbras `configManager.cgi?action=getConfig&name=<table>` key-value map
/// into normalized `CameraHardwareState` JSON fields.
pub fn apply_dahua_cgi_table_to_state(
    table: &str,
    kv: &BTreeMap<String, String>,
    state_map: &mut Map<String, Value>,
) {
    match table {
        "LeSmartTrack" => {
            if let Some(v) = kv.get("table.LeSmartTrack[0].Enable") {
                state_map.insert(
                    "hardware_autotracking".to_string(),
                    json!(v.eq_ignore_ascii_case("true")),
                );
            }
        }
        "VideoInOptions" => {
            if let Some(dn) = kv.get("table.VideoInOptions[0].DayNightColor") {
                let mode_str = match dn.as_str() {
                    "1" => "day",
                    "2" => "night",
                    _ => "auto",
                };
                state_map.insert("day_night_mode".to_string(), json!(mode_str));
            }
            if let Some(fl) = kv.get("table.VideoInOptions[0].Flip") {
                state_map.insert("flip".to_string(), json!(fl.eq_ignore_ascii_case("true")));
            }
            if let Some(mr) = kv.get("table.VideoInOptions[0].Mirror") {
                state_map.insert("mirror".to_string(), json!(mr.eq_ignore_ascii_case("true")));
            }
            if let Some(wdr) = kv.get("table.VideoInOptions[0].WideDynamicRangeMode") {
                state_map.insert("wdr".to_string(), json!(wdr != "0"));
            }
        }
        "Lighting" => {
            if let Some(m) = kv.get("table.Lighting[0][0].Mode") {
                state_map.insert("ir_mode".to_string(), json!(m.to_ascii_lowercase()));
            }
            if let Some(l) = kv
                .get("table.Lighting[0][0].MiddleLight[0].Light")
                .and_then(|s| s.parse::<u8>().ok())
            {
                state_map.insert("ir_brightness".to_string(), json!(l));
            }
        }
        "LightGlobal" => {
            if let Some(en) = kv.get("table.LightGlobal[0].Enable") {
                state_map.insert(
                    "status_led".to_string(),
                    json!(en.eq_ignore_ascii_case("true")),
                );
            }
        }
        "MotionDetect" => {
            if let Some(beep) = kv.get("table.MotionDetect[0].EventHandler.BeepEnable") {
                state_map.insert(
                    "siren_on_motion".to_string(),
                    json!(beep.eq_ignore_ascii_case("true")),
                );
            }
        }
        "AudioDetect" => {
            if let Some(en) = kv.get("table.AudioDetect[0].Enable") {
                state_map.insert(
                    "audio_detect_enabled".to_string(),
                    json!(en.eq_ignore_ascii_case("true")),
                );
            }
            if let Some(sens) = kv
                .get("table.AudioDetect[0].AnomalySensitive")
                .or_else(|| kv.get("table.AudioDetect[0].MutationThreold"))
                .or_else(|| kv.get("table.AudioDetect[0].MutationThreshold"))
                .and_then(|s| s.parse::<u8>().ok())
            {
                state_map.insert("audio_sensitivity".to_string(), json!(sens));
            }
            if let Some(beep) = kv.get("table.AudioDetect[0].EventHandler.BeepEnable") {
                state_map.insert(
                    "siren_on_audio".to_string(),
                    json!(beep.eq_ignore_ascii_case("true")),
                );
            }
        }
        "AudioOutputVolume" => {
            if let Some(vol) = kv
                .get("table.AudioOutputVolume[0]")
                .or_else(|| kv.get("table.AudioOutputVolume[0].Volume"))
                .and_then(|s| s.parse::<u8>().ok())
            {
                state_map.insert("speaker_volume".to_string(), json!(vol));
            }
        }
        "AudioInputVolume" => {
            if let Some(vol) = kv
                .get("table.AudioInputVolume[0]")
                .or_else(|| kv.get("table.AudioInputVolume[0].Volume"))
                .and_then(|s| s.parse::<u8>().ok())
            {
                state_map.insert("microphone_volume".to_string(), json!(vol));
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CameraHardwareUpdateBody {
    pub brightness: Option<f32>,
    pub contrast: Option<f32>,
    pub saturation: Option<f32>,
    pub sharpness: Option<f32>,
    pub hardware_autotracking: Option<bool>,
    pub day_night_mode: Option<String>,
    pub ir_mode: Option<String>,
    pub ir_brightness: Option<u8>,
    pub status_led: Option<bool>,
    pub flip: Option<bool>,
    pub mirror: Option<bool>,
    pub wdr: Option<bool>,
    pub siren_on_motion: Option<bool>,
    pub siren_on_audio: Option<bool>,
    pub audio_detect_enabled: Option<bool>,
    pub audio_sensitivity: Option<u8>,
    pub speaker_volume: Option<u8>,
    pub microphone_volume: Option<u8>,
    pub reboot: Option<bool>,
    pub apply_suggested_onvif: Option<bool>,
    pub apply_suggested_substream: Option<bool>,
    pub protocol: Option<String>,
    pub profile_token: Option<String>,
    pub video_source_token: Option<String>,
    pub cloud_password: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// Encodes normalized `CameraHardwareUpdateBody` fields into Dahua/Intelbras CGI `setConfig` query pairs.
pub fn build_dahua_cgi_set_config_pairs(body: &CameraHardwareUpdateBody) -> Vec<String> {
    let mut cgi_pairs: Vec<String> = Vec::new();
    if let Some(st) = body.hardware_autotracking {
        let flag = if st { "true" } else { "false" };
        cgi_pairs.push(format!("LeSmartTrack[0].Enable={flag}"));
        cgi_pairs.push(format!("MotionDetect[0].EventHandler.PtzLinkEnable={flag}"));
    }
    if let Some(ref dn) = body.day_night_mode {
        let code = match dn.trim().to_ascii_lowercase().as_str() {
            "day" | "color" | "1" => "1",
            "night" | "bw" | "ir" | "2" => "2",
            _ => "0",
        };
        cgi_pairs.push(format!("VideoInOptions[0].DayNightColor={code}"));
        cgi_pairs.push(format!("VideoInDayNight[0][0].Mode={code}"));
    }
    if let Some(ref irm) = body.ir_mode {
        let mode_val = match irm.trim().to_ascii_lowercase().as_str() {
            "manual" => "Manual",
            "off" => "Off",
            _ => "Auto",
        };
        cgi_pairs.push(format!("Lighting[0][0].Mode={mode_val}"));
    }
    if let Some(irb) = body.ir_brightness {
        let clamped = irb.clamp(0, 100);
        cgi_pairs.push(format!("Lighting[0][0].MiddleLight[0].Light={clamped}"));
    }
    if let Some(led) = body.status_led {
        cgi_pairs.push(format!(
            "LightGlobal[0].Enable={}",
            if led { "true" } else { "false" }
        ));
    }
    if let Some(fl) = body.flip {
        cgi_pairs.push(format!(
            "VideoInOptions[0].Flip={}",
            if fl { "true" } else { "false" }
        ));
    }
    if let Some(mr) = body.mirror {
        cgi_pairs.push(format!(
            "VideoInOptions[0].Mirror={}",
            if mr { "true" } else { "false" }
        ));
    }
    if let Some(wdr) = body.wdr {
        cgi_pairs.push(format!(
            "VideoInOptions[0].WideDynamicRangeMode={}",
            if wdr { "1" } else { "0" }
        ));
    }
    if let Some(sm) = body.siren_on_motion {
        cgi_pairs.push(format!(
            "MotionDetect[0].EventHandler.BeepEnable={}",
            if sm { "true" } else { "false" }
        ));
    }
    if let Some(sa) = body.siren_on_audio {
        cgi_pairs.push(format!(
            "AudioDetect[0].EventHandler.BeepEnable={}",
            if sa { "true" } else { "false" }
        ));
    }
    if let Some(ad) = body.audio_detect_enabled {
        let flag = if ad { "true" } else { "false" };
        cgi_pairs.push(format!("AudioDetect[0].Enable={flag}"));
        cgi_pairs.push(format!("AudioDetect[0].AnomalyDetect={flag}"));
    }
    if let Some(asens) = body.audio_sensitivity {
        cgi_pairs.push(format!(
            "AudioDetect[0].AnomalySensitive={}",
            asens.clamp(1, 100)
        ));
    }
    if let Some(svol) = body.speaker_volume {
        cgi_pairs.push(format!("AudioOutputVolume[0]={}", svol.min(100)));
    }
    if let Some(mvol) = body.microphone_volume {
        cgi_pairs.push(format!("AudioInputVolume[0]={}", mvol.min(100)));
    }
    cgi_pairs
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnvifPresetItem {
    pub token: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnvifPtzStatus {
    pub pan: f32,
    pub tilt: f32,
    pub zoom: f32,
    pub move_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Tolerantly parses an ONVIF float string (such as `"+0.5000000"`, `"-0.25"`, `"0,5"`, or malformed text)
/// into a finite `f32` clamped to `[min, max]`, returning `default` instead of panicking on invalid input.
pub fn parse_onvif_float(raw: &str, min: f32, max: f32, default: f32) -> f32 {
    let cleaned = raw.trim().trim_start_matches('+').replace(',', ".");
    match cleaned.parse::<f32>() {
        Ok(v) if v.is_finite() => v.clamp(min, max),
        _ => default,
    }
}

pub fn xml_escape_simple(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() && !matches!(c, '\t' | '\n' | '\r') => {}
            _ => out.push(ch),
        }
    }
    out
}

pub fn sanitize_onvif_token(raw: Option<&str>, default_val: &str) -> Option<String> {
    let token = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_val);
    if token.is_empty() || token.len() > 128 || token.chars().any(|c| c.is_control()) {
        None
    } else {
        Some(token.to_string())
    }
}

pub fn is_safe_onvif_endpoint_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.contains('\\')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return false;
    }
    let after_scheme = trimmed.split_once("://").map(|(_, r)| r).unwrap_or("");
    let raw_authority = after_scheme.split('/').next().unwrap_or("").trim();
    if raw_authority.is_empty() || raw_authority.contains('@') {
        return false;
    }
    let authority = raw_authority;
    let is_bracketed = authority.starts_with('[');
    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((ipv6_part, rest)) = after_bracket.split_once(']') else {
            return false;
        };
        if ipv6_part.is_empty() {
            return false;
        }
        let port_opt = if rest.is_empty() {
            None
        } else if let Some(p_str) = rest.strip_prefix(':') {
            let Ok(p) = p_str.parse::<u16>() else {
                return false;
            };
            Some(p)
        } else {
            return false;
        };
        (ipv6_part, port_opt)
    } else if let Some((h, p_str)) = authority.rsplit_once(':') {
        if h.is_empty() || h.contains(':') {
            return false;
        }
        let Ok(p) = p_str.parse::<u16>() else {
            return false;
        };
        (h, Some(p))
    } else {
        if authority.contains(':') {
            return false;
        }
        (authority, None)
    };
    if matches!(port, Some(0 | 5000 | 8971 | 1984 | 8554 | 8555)) {
        return false;
    }
    if host.ends_with("..") {
        return false;
    }
    let host_lc = host.trim_end_matches('.').to_ascii_lowercase();
    if host_lc.is_empty()
        || host_lc == "metadata"
        || host_lc == "metadata.google.internal"
        || host_lc.ends_with(".metadata.google.internal")
        || host_lc == "100.100.100.200"
        || host_lc.starts_with("169.254.")
        || host_lc == "kubernetes"
        || host_lc == "kubernetes.default"
        || host_lc.ends_with(".svc")
        || host_lc.ends_with(".cluster.local")
    {
        return false;
    }
    if let Ok(ip) = host_lc.parse::<std::net::IpAddr>() {
        let is_disallowed_v4 = |v4: std::net::Ipv4Addr| -> bool {
            v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (v4.octets()[0] == 169 && v4.octets()[1] == 254)
                || v4.octets() == [100, 100, 100, 200]
        };
        let disallowed = match ip {
            std::net::IpAddr::V4(v4) => is_disallowed_v4(v4),
            std::net::IpAddr::V6(v6) => {
                let segs = v6.segments();
                let nat64_v4 = (segs[0] == 0x0064
                    && segs[1] == 0xff9b
                    && segs[2..6] == [0, 0, 0, 0])
                .then(|| {
                    std::net::Ipv4Addr::new(
                        (segs[6] >> 8) as u8,
                        segs[6] as u8,
                        (segs[7] >> 8) as u8,
                        segs[7] as u8,
                    )
                });
                v6.is_unspecified()
                    || v6.is_multicast()
                    || (segs[0] & 0xffc0) == 0xfe80
                    || segs == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
                    || v6.to_ipv4().is_some_and(is_disallowed_v4)
                    || nat64_v4.is_some_and(is_disallowed_v4)
            }
        };
        if disallowed {
            return false;
        }
    } else {
        if is_bracketed {
            return false;
        }
        let labels: Vec<&str> = host_lc.split('.').collect();
        if labels.iter().any(|l| l.is_empty()) {
            return false;
        }
        let is_numeric_or_hex_label = |l: &str| -> bool {
            l.chars().all(|c| c.is_ascii_digit())
                || l.strip_prefix("0x")
                    .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
        };
        if labels.iter().all(|l| is_numeric_or_hex_label(l))
            || labels
                .last()
                .is_some_and(|last| is_numeric_or_hex_label(last))
        {
            return false;
        }
    }
    true
}

pub fn resolve_onvif_ptz_endpoint(onvif: &ninjanvr_config::OnvifCameraConfig) -> Option<String> {
    let host_raw = onvif.host.as_deref()?.trim();
    if host_raw.is_empty() {
        return None;
    }
    let host_lc_scheme = host_raw.to_ascii_lowercase();
    let candidate =
        if host_lc_scheme.starts_with("http://") || host_lc_scheme.starts_with("https://") {
            let trimmed = host_raw.trim_end_matches('/');
            let after_scheme = trimmed.split_once("://").map(|(_, r)| r).unwrap_or("");
            if after_scheme.contains('/') {
                trimmed.to_string()
            } else {
                format!("{trimmed}/onvif/ptz_service")
            }
        } else if host_raw.matches(':').count() > 1 && !host_raw.starts_with('[') {
            if onvif.port == 0 {
                return None;
            }
            format!("http://[{host_raw}]:{}/onvif/ptz_service", onvif.port)
        } else if host_raw.contains(':') && !host_raw.ends_with(']') {
            format!("http://{host_raw}/onvif/ptz_service")
        } else {
            if onvif.port == 0 {
                return None;
            }
            format!("http://{host_raw}:{}/onvif/ptz_service", onvif.port)
        };
    if is_safe_onvif_endpoint_url(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

pub fn build_onvif_soap_envelope(
    body_xml: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let header_xml = if let Some(u) = user.map(str::trim).filter(|s| !s.is_empty()) {
        let pw = password.unwrap_or("");
        format!(
            "<s:Header><wsse:Security xmlns:wsse=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\"><wsse:UsernameToken><wsse:Username>{}</wsse:Username><wsse:Password>{}</wsse:Password></wsse:UsernameToken></wsse:Security></s:Header>",
            xml_escape_simple(u),
            xml_escape_simple(pw)
        )
    } else {
        "<s:Header/>".to_string()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:tptz=\"http://www.onvif.org/ver20/ptz/wsdl\" xmlns:tt=\"http://www.onvif.org/ver10/schema\">{header_xml}<s:Body>{body_xml}</s:Body></s:Envelope>"
    )
}

pub fn build_onvif_continuous_move_soap(
    profile_token: &str,
    pan: f32,
    tilt: f32,
    zoom: f32,
    timeout_sec: u64,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let p = if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let t = if tilt.is_finite() {
        tilt.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let z = if zoom.is_finite() {
        zoom.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let t_sec = timeout_sec.clamp(1, 60);
    let body = format!(
        "<tptz:ContinuousMove><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x=\"{p:.4}\" y=\"{t:.4}\"/><tt:Zoom x=\"{z:.4}\"/></tptz:Velocity><tptz:Timeout>PT{t_sec}S</tptz:Timeout></tptz:ContinuousMove>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_relative_move_soap(
    profile_token: &str,
    pan: f32,
    tilt: f32,
    zoom: f32,
    speed: Option<f32>,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let p = if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let t = if tilt.is_finite() {
        tilt.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let z = if zoom.is_finite() {
        zoom.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let speed_xml = if let Some(spd) = speed {
        let s = if spd.is_finite() {
            spd.clamp(0.05, 1.0)
        } else {
            0.5
        };
        format!(
            "<tptz:Speed><tt:PanTilt x=\"{s:.4}\" y=\"{s:.4}\"/><tt:Zoom x=\"{s:.4}\"/></tptz:Speed>"
        )
    } else {
        String::new()
    };
    let body = format!(
        "<tptz:RelativeMove><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Translation><tt:PanTilt x=\"{p:.4}\" y=\"{t:.4}\"/><tt:Zoom x=\"{z:.4}\"/></tptz:Translation>{speed_xml}</tptz:RelativeMove>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_stop_soap(
    profile_token: &str,
    pan_tilt: bool,
    zoom: bool,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let body = format!(
        "<tptz:Stop><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PanTilt>{pan_tilt}</tptz:PanTilt><tptz:Zoom>{zoom}</tptz:Zoom></tptz:Stop>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_goto_preset_soap(
    profile_token: &str,
    preset_token: &str,
    speed: Option<f32>,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let speed_xml = if let Some(spd) = speed {
        let s = if spd.is_finite() {
            spd.clamp(0.05, 1.0)
        } else {
            0.5
        };
        format!(
            "<tptz:Speed><tt:PanTilt x=\"{s:.4}\" y=\"{s:.4}\"/><tt:Zoom x=\"{s:.4}\"/></tptz:Speed>"
        )
    } else {
        String::new()
    };
    let body = format!(
        "<tptz:GotoPreset><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken>{speed_xml}</tptz:GotoPreset>",
        xml_escape_simple(profile_token),
        xml_escape_simple(preset_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_get_presets_soap(
    profile_token: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let body = format!(
        "<tptz:GetPresets><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetPresets>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_get_status_soap(
    profile_token: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let body = format!(
        "<tptz:GetStatus><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetStatus>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_absolute_move_soap(
    profile_token: &str,
    pan: f32,
    tilt: f32,
    zoom: f32,
    speed: Option<f32>,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let p = if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let t = if tilt.is_finite() {
        tilt.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let z = if zoom.is_finite() {
        zoom.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let speed_xml = if let Some(spd) = speed {
        let s = if spd.is_finite() {
            spd.clamp(0.05, 1.0)
        } else {
            0.5
        };
        format!(
            "<tptz:Speed><tt:PanTilt x=\"{s:.4}\" y=\"{s:.4}\" space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/GenericSpeedSpace\"/><tt:Zoom x=\"{s:.4}\"/></tptz:Speed>"
        )
    } else {
        String::new()
    };
    let body = format!(
        "<tptz:AbsoluteMove><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Position><tt:PanTilt x=\"{p:.4}\" y=\"{t:.4}\" space=\"http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace\"/><tt:Zoom x=\"{z:.4}\"/></tptz:Position>{speed_xml}</tptz:AbsoluteMove>",
        xml_escape_simple(profile_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_set_preset_soap(
    profile_token: &str,
    preset_name: &str,
    preset_token: Option<&str>,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let tok_xml = if let Some(tok) = preset_token.map(str::trim).filter(|s| !s.is_empty()) {
        format!(
            "<tptz:PresetToken>{}</tptz:PresetToken>",
            xml_escape_simple(tok)
        )
    } else {
        String::new()
    };
    let body = format!(
        "<tptz:SetPreset><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetName>{}</tptz:PresetName>{tok_xml}</tptz:SetPreset>",
        xml_escape_simple(profile_token),
        xml_escape_simple(preset_name)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_remove_preset_soap(
    profile_token: &str,
    preset_token: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let body = format!(
        "<tptz:RemovePreset><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken></tptz:RemovePreset>",
        xml_escape_simple(profile_token),
        xml_escape_simple(preset_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn parse_onvif_set_preset_token_xml(xml: &str) -> Option<String> {
    extract_xml_element_text(xml, "PresetToken")
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains('<'))
        .map(str::to_string)
}

/// Pure-Rust RFC 3174 SHA-1 implementation for ONVIF WS-UsernameToken PasswordDigest.
pub fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 72) & !63);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, wi) in w.iter_mut().enumerate().take(16) {
            let off = i * 4;
            *wi = u32::from_be_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDCu32),
                _ => (b ^ c ^ d, 0xCA62_C1D6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

pub fn build_onvif_wsse_digest_header(
    user: &str,
    password: &str,
    nonce_raw: &[u8; 16],
    created: &str,
) -> String {
    let mut sha_input = Vec::with_capacity(16 + created.len() + password.len());
    sha_input.extend_from_slice(nonce_raw);
    sha_input.extend_from_slice(created.as_bytes());
    sha_input.extend_from_slice(password.as_bytes());
    let digest_bytes = sha1_digest(&sha_input);
    let digest_b64 = base64::engine::general_purpose::STANDARD.encode(digest_bytes);
    let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(nonce_raw);
    format!(
        "<s:Header><wsse:Security s:mustUnderstand=\"1\" xmlns:wsse=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd\" xmlns:wsu=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd\"><wsse:UsernameToken><wsse:Username>{}</wsse:Username><wsse:Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest_b64}</wsse:Password><wsse:Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{nonce_b64}</wsse:Nonce><wsu:Created>{}</wsu:Created></wsse:UsernameToken></wsse:Security></s:Header>",
        xml_escape_simple(user),
        xml_escape_simple(created)
    )
}

pub fn upgrade_soap_to_wsse_digest(
    soap_xml: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let Some(u) = user.map(str::trim).filter(|s| !s.is_empty()) else {
        return soap_xml.to_string();
    };
    let pw = password.unwrap_or("");
    let mut nonce_raw = [0u8; 16];
    rand::fill(&mut nonce_raw);
    let created = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let digest_hdr = build_onvif_wsse_digest_header(u, pw, &nonce_raw, &created);

    if let Some(h_start) = soap_xml.find("<s:Header>")
        && let Some(h_end_rel) = soap_xml[h_start..].find("</s:Header>")
    {
        let h_end = h_start + h_end_rel + "</s:Header>".len();
        let mut out = String::with_capacity(soap_xml.len() + 256);
        out.push_str(&soap_xml[..h_start]);
        out.push_str(&digest_hdr);
        out.push_str(&soap_xml[h_end..]);
        return out;
    }
    if let Some(empty_idx) = soap_xml.find("<s:Header/>") {
        let mut out = String::with_capacity(soap_xml.len() + 256);
        out.push_str(&soap_xml[..empty_idx]);
        out.push_str(&digest_hdr);
        out.push_str(&soap_xml[empty_idx + "<s:Header/>".len()..]);
        return out;
    }
    soap_xml.to_string()
}

pub fn extract_xml_attr<'a>(tag_fragment: &'a str, attr_name: &str) -> Option<&'a str> {
    if attr_name.is_empty() {
        return None;
    }
    let bytes = tag_fragment.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = tag_fragment[search_from..].find(attr_name) {
        let idx = search_from + rel;
        let boundary_ok = idx == 0 || {
            let prev = bytes[idx - 1];
            prev.is_ascii_whitespace() || matches!(prev, b'"' | b'\'')
        };
        let after_name = idx + attr_name.len();
        if boundary_ok && after_name < tag_fragment.len() {
            let rest = tag_fragment[after_name..].trim_start();
            if let Some(after_eq) = rest.strip_prefix('=') {
                let after_eq_trimmed = after_eq.trim_start();
                for quote in ['"', '\''] {
                    if let Some(val_rest) = after_eq_trimmed.strip_prefix(quote)
                        && let Some(end_rel) = val_rest.find(quote)
                    {
                        return Some(&val_rest[..end_rel]);
                    }
                }
            }
        }
        search_from = idx + 1;
    }
    None
}

fn find_closing_xml_tag_offset(xml_after_open: &str, local_name: &str) -> Option<(usize, usize)> {
    let mut cursor = 0usize;
    let mut depth = 1usize;
    while let Some(rel) = xml_after_open[cursor..].find('<') {
        let tag_start = cursor + rel;
        if xml_after_open[tag_start..].starts_with("<!--") {
            if let Some(end_comment) = xml_after_open[tag_start + 4..].find("-->") {
                cursor = tag_start + 4 + end_comment + 3;
                continue;
            } else {
                break;
            }
        }
        let Some(gt_rel) = xml_after_open[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel + 1;
        let inner_tag = xml_after_open[tag_start + 1..tag_start + gt_rel].trim();
        if let Some(after_slash) = inner_tag.strip_prefix('/') {
            let raw_tag = after_slash.trim();
            let candidate_local = raw_tag
                .rsplit_once(':')
                .map(|(_, l)| l.trim())
                .unwrap_or(raw_tag);
            if candidate_local.eq_ignore_ascii_case(local_name)
                && !raw_tag.contains('<')
                && !raw_tag.contains('/')
            {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((tag_start, tag_end));
                }
            }
        } else if !inner_tag.starts_with('?')
            && !inner_tag.starts_with('!')
            && !inner_tag.ends_with('/')
        {
            let tag_token = inner_tag.split_whitespace().next().unwrap_or("");
            let candidate_local = tag_token
                .rsplit_once(':')
                .map(|(_, l)| l)
                .unwrap_or(tag_token);
            if candidate_local.eq_ignore_ascii_case(local_name) {
                depth = depth.saturating_add(1);
            }
        }
        cursor = tag_end;
    }
    None
}

pub fn extract_xml_element_text<'a>(xml: &'a str, local_name: &str) -> Option<&'a str> {
    let mut cursor = 0usize;
    while let Some(lt_rel) = xml[cursor..].find('<') {
        let tag_start = cursor + lt_rel;
        if xml[tag_start..].starts_with("<!--") {
            if let Some(end_comment) = xml[tag_start + 4..].find("-->") {
                cursor = tag_start + 4 + end_comment + 3;
                continue;
            } else {
                break;
            }
        }
        if xml[tag_start..].starts_with("</") {
            cursor = tag_start + 2;
            continue;
        }
        let Some(gt_rel) = xml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel;
        let header = &xml[tag_start + 1..tag_end];
        let tag_token = header.split_whitespace().next().unwrap_or("");
        let tag_clean = tag_token.trim_end_matches('/');
        let candidate_local = tag_clean
            .rsplit_once(':')
            .map(|(_, l)| l)
            .unwrap_or(tag_clean);
        if candidate_local.eq_ignore_ascii_case(local_name) && !header.ends_with('/') {
            let content_start = tag_end + 1;
            let Some((close_rel, close_end_rel)) =
                find_closing_xml_tag_offset(&xml[content_start..], candidate_local)
            else {
                break;
            };
            let trimmed = xml[content_start..content_start + close_rel].trim();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
            cursor = (content_start + close_end_rel).min(xml.len());
            continue;
        }
        cursor = tag_end + 1;
    }
    None
}

pub fn extract_onvif_soap_fault(xml: &str) -> Option<String> {
    let fault_inner = extract_xml_element_text(xml, "Fault")?;
    if let Some(text) = extract_xml_element_text(fault_inner, "Text")
        .or_else(|| extract_xml_element_text(fault_inner, "faultstring"))
        .or_else(|| extract_xml_element_text(fault_inner, "Reason").filter(|s| !s.contains('<')))
        .filter(|s| !s.is_empty())
    {
        let scrubbed = scrub_rtsp_credentials(text);
        let bounded: String = scrubbed.chars().take(512).collect();
        return Some(bounded);
    }
    Some("ONVIF SOAP Fault".to_string())
}

pub fn parse_onvif_presets_xml(xml: &str) -> Vec<OnvifPresetItem> {
    let mut presets = Vec::new();
    let mut cursor = 0usize;
    while let Some(lt_rel) = xml[cursor..].find('<') {
        if presets.len() >= MAX_ONVIF_PRESETS {
            break;
        }
        let tag_start = cursor + lt_rel;
        if xml[tag_start..].starts_with("<!--") {
            if let Some(end_comment) = xml[tag_start + 4..].find("-->") {
                cursor = tag_start + 4 + end_comment + 3;
                continue;
            } else {
                break;
            }
        }
        let Some(gt_rel) = xml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel;
        let header = &xml[tag_start + 1..tag_end];
        let tag_token = header.split_whitespace().next().unwrap_or("");
        let local_name = tag_token
            .trim_end_matches('/')
            .rsplit_once(':')
            .map(|(_, l)| l)
            .unwrap_or(tag_token.trim_end_matches('/'));
        if local_name.eq_ignore_ascii_case("Preset") && !header.starts_with('/') {
            let token_opt = extract_xml_attr(header, "token").map(str::trim);
            if header.ends_with('/') {
                if let Some(tok) = token_opt.filter(|s| !s.is_empty()) {
                    presets.push(OnvifPresetItem {
                        token: tok.to_string(),
                        name: tok.to_string(),
                    });
                }
                cursor = tag_end + 1;
                continue;
            }
            let content_start = tag_end + 1;
            let (close_idx, next_cursor_rel) =
                find_closing_xml_tag_offset(&xml[content_start..], "Preset")
                    .unwrap_or((xml.len() - content_start, xml.len() - content_start));
            let inner = &xml[content_start..content_start + close_idx];
            let name_opt = extract_xml_element_text(inner, "Name").map(str::trim);
            let token_elem = extract_xml_element_text(inner, "token").map(str::trim);
            let resolved_token = token_opt
                .or(token_elem)
                .or(name_opt)
                .filter(|s| !s.is_empty());
            if let Some(tok) = resolved_token {
                let resolved_name = name_opt.filter(|s| !s.is_empty()).unwrap_or(tok);
                presets.push(OnvifPresetItem {
                    token: tok.to_string(),
                    name: resolved_name.to_string(),
                });
            }
            cursor = (content_start + next_cursor_rel).min(xml.len());
        } else {
            cursor = tag_end + 1;
        }
    }
    presets
}

pub fn parse_onvif_status_xml(xml: &str) -> OnvifPtzStatus {
    let mut pan = 0.0f32;
    let mut tilt = 0.0f32;
    let mut zoom = 0.0f32;

    let mut cursor = 0usize;
    while let Some(lt_rel) = xml[cursor..].find('<') {
        let tag_start = cursor + lt_rel;
        if xml[tag_start..].starts_with("<!--") {
            if let Some(end_comment) = xml[tag_start + 4..].find("-->") {
                cursor = tag_start + 4 + end_comment + 3;
                continue;
            } else {
                break;
            }
        }
        let Some(gt_rel) = xml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + gt_rel;
        let header = &xml[tag_start + 1..tag_end];
        let tag_token = header.split_whitespace().next().unwrap_or("");
        let local_name = tag_token
            .trim_end_matches('/')
            .rsplit_once(':')
            .map(|(_, l)| l)
            .unwrap_or(tag_token.trim_end_matches('/'));
        if local_name.eq_ignore_ascii_case("PanTilt") {
            if let Some(x_str) = extract_xml_attr(header, "x") {
                pan = parse_onvif_float(x_str, -1.0, 1.0, 0.0);
            }
            if let Some(y_str) = extract_xml_attr(header, "y") {
                tilt = parse_onvif_float(y_str, -1.0, 1.0, 0.0);
            }
        } else if local_name.eq_ignore_ascii_case("Zoom")
            && let Some(x_str) = extract_xml_attr(header, "x")
        {
            zoom = parse_onvif_float(x_str, 0.0, 1.0, 0.0);
        }
        cursor = tag_end + 1;
    }

    let move_status = if let Some(move_block) = extract_xml_element_text(xml, "MoveStatus") {
        let pt_move = extract_xml_element_text(move_block, "PanTilt")
            .filter(|s| !s.is_empty() && !s.contains('<'));
        let z_move = extract_xml_element_text(move_block, "Zoom")
            .filter(|s| !s.is_empty() && !s.contains('<'));
        let direct_move = Some(move_block).filter(|s| !s.is_empty() && !s.contains('<'));
        if pt_move.is_some_and(|s| s.eq_ignore_ascii_case("MOVING"))
            || z_move.is_some_and(|s| s.eq_ignore_ascii_case("MOVING"))
        {
            "MOVING".to_string()
        } else {
            pt_move
                .or(z_move)
                .or(direct_move)
                .unwrap_or("IDLE")
                .to_ascii_uppercase()
        }
    } else {
        extract_xml_element_text(xml, "PanTilt")
            .filter(|s| !s.is_empty() && !s.contains('<'))
            .unwrap_or("IDLE")
            .to_ascii_uppercase()
    };
    let error = extract_xml_element_text(xml, "Error")
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "0" && !s.eq_ignore_ascii_case("NoError"))
        .map(|s| scrub_rtsp_credentials(s).chars().take(512).collect());

    OnvifPtzStatus {
        pan,
        tilt,
        zoom,
        move_status,
        error,
    }
}

pub fn is_onvif_profile_not_found_error(err_msg: &str) -> bool {
    let lc = err_msg.to_ascii_lowercase();
    lc.contains("noprofile")
        || lc.contains("profile token")
        || lc.contains("invalidargval")
        || lc.contains("not exist")
}

pub fn is_onvif_action_not_supported_error(err_msg: &str) -> bool {
    let lc = err_msg.to_ascii_lowercase();
    lc.contains("actionnotsupported")
        || lc.contains("notimplemented")
        || lc.contains("not implemented")
        || lc.contains("optional method")
}

pub async fn send_onvif_soap_request(
    client: &reqwest::Client,
    endpoint: &str,
    action: &str,
    soap_xml: String,
    user: Option<&str>,
    password: Option<&str>,
    timeout: Duration,
) -> std::result::Result<String, (StatusCode, String)> {
    send_onvif_soap_request_with_ns(
        client,
        endpoint,
        ("http://www.onvif.org/ver20/ptz/wsdl", action),
        soap_xml,
        (user, password),
        timeout,
    )
    .await
}

pub async fn send_onvif_soap_request_with_ns(
    client: &reqwest::Client,
    endpoint: &str,
    (wsdl_ns, action): (&str, &str),
    soap_xml: String,
    (user, password): (Option<&str>, Option<&str>),
    timeout: Duration,
) -> std::result::Result<String, (StatusCode, String)> {
    if !is_safe_onvif_endpoint_url(endpoint) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Disallowed or invalid ONVIF endpoint URL".to_string(),
        ));
    }
    if action.is_empty()
        || !action
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid ONVIF SOAP action".to_string(),
        ));
    }
    let digest_soap_xml = upgrade_soap_to_wsse_digest(&soap_xml, user, password);
    let mut req = client
        .post(endpoint)
        .timeout(timeout)
        .header(header::CONNECTION, "close")
        .header(
            header::CONTENT_TYPE,
            format!(
                "application/soap+xml; charset=utf-8; action=\"{}/{action}\"",
                wsdl_ns.trim_end_matches('/')
            ),
        )
        .body(digest_soap_xml);
    if let Some(u) = user.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.basic_auth(u, password);
    }
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(err) if err.is_timeout() => {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!("ONVIF {action} request timed out"),
            ));
        }
        Err(err) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                scrub_rtsp_credentials(&format!("ONVIF {action} request failed: {err}")),
            ));
        }
    };
    let status = resp.status();
    if status.is_redirection() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "ONVIF {action} redirect response HTTP {} is disallowed",
                status.as_u16()
            ),
        ));
    }
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_ONVIF_SOAP_RESPONSE_BYTES as u64)
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ONVIF {action} response exceeded maximum size"),
        ));
    }
    let mut body_buf = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        if body_buf.len().saturating_add(chunk.len()) > MAX_ONVIF_SOAP_RESPONSE_BYTES {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("ONVIF {action} response exceeded maximum size"),
            ));
        }
        body_buf.extend_from_slice(&chunk);
    }
    let body_text = String::from_utf8_lossy(&body_buf).into_owned();
    if let Some(fault_reason) = extract_onvif_soap_fault(&body_text) {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ONVIF {action} fault: {fault_reason}"),
        ));
    }
    if !status.is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("ONVIF {action} returned HTTP {}", status.as_u16()),
        ));
    }
    Ok(body_text)
}

/// Pure-Rust RFC 1321 MD5 implementation for HTTP Digest Authentication (RFC 2617 / 7616).
pub fn md5_digest(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let mut a0: u32 = 0x6745_2301;
    let mut b0: u32 = 0xefcd_ab89;
    let mut c0: u32 = 0x98ba_dcfe;
    let mut d0: u32 = 0x1032_5476;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 72) & !63);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, mi) in m.iter_mut().enumerate() {
            let off = i * 4;
            *mi = u32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
        }
        let mut a = a0;
        let mut b = b0;
        let mut c = c0;
        let mut d = d0;
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

pub fn md5_hex(data: &[u8]) -> String {
    md5_digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn extract_digest_challenge_param<'a>(header_val: &'a str, key: &str) -> Option<&'a str> {
    let header_lc = header_val.to_ascii_lowercase();
    let key_lc = key.to_ascii_lowercase();
    let mut cursor = 0usize;
    while let Some(rel) = header_lc[cursor..].find(&key_lc) {
        let idx = cursor + rel;
        let boundary_ok = idx == 0 || {
            let prev = header_val.as_bytes()[idx - 1];
            prev.is_ascii_whitespace() || prev == b','
        };
        let after_key = idx + key.len();
        if boundary_ok && after_key < header_val.len() {
            let rest = header_val[after_key..].trim_start();
            if let Some(after_eq) = rest.strip_prefix('=') {
                let after_eq = after_eq.trim_start();
                if let Some(quoted) = after_eq.strip_prefix('"') {
                    if let Some(end_q) = quoted.find('"') {
                        return Some(&quoted[..end_q]);
                    }
                } else {
                    let end = after_eq
                        .find(|c: char| c == ',' || c.is_ascii_whitespace())
                        .unwrap_or(after_eq.len());
                    return Some(&after_eq[..end]);
                }
            }
        }
        cursor = idx + 1;
    }
    None
}

fn escape_digest_quoted_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_control() {
            continue;
        }
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

pub fn build_http_digest_auth_header(
    www_authenticate: &str,
    method: &str,
    uri_path_and_query: &str,
    user: &str,
    password: &str,
    cnonce: &str,
) -> Option<String> {
    let trimmed = www_authenticate.trim();
    if trimmed.len() < 7 || !trimmed[..7].eq_ignore_ascii_case("digest ") {
        return None;
    }
    let after_digest = &trimmed[7..];
    let realm = extract_digest_challenge_param(after_digest, "realm")?;
    let nonce = extract_digest_challenge_param(after_digest, "nonce")?;
    let qop_raw = extract_digest_challenge_param(after_digest, "qop");
    let opaque_opt = extract_digest_challenge_param(after_digest, "opaque");

    let ha1 = md5_hex(format!("{user}:{realm}:{password}").as_bytes());
    let ha2 = md5_hex(format!("{method}:{uri_path_and_query}").as_bytes());
    let nc = "00000001";

    let use_qop_auth = qop_raw.is_some_and(|q| {
        q.split(',')
            .any(|tok| tok.trim().eq_ignore_ascii_case("auth"))
    });

    let response_hex = if use_qop_auth {
        md5_hex(format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}").as_bytes())
    } else {
        md5_hex(format!("{ha1}:{nonce}:{ha2}").as_bytes())
    };

    let safe_user = escape_digest_quoted_str(user);
    let safe_realm = escape_digest_quoted_str(realm);
    let safe_nonce = escape_digest_quoted_str(nonce);
    let safe_uri = escape_digest_quoted_str(uri_path_and_query);
    let safe_cnonce = escape_digest_quoted_str(cnonce);

    let mut hdr = format!(
        "Digest username=\"{safe_user}\", realm=\"{safe_realm}\", nonce=\"{safe_nonce}\", uri=\"{safe_uri}\", response=\"{response_hex}\""
    );
    if let Some(opaque) = opaque_opt {
        let safe_opaque = escape_digest_quoted_str(opaque);
        hdr.push_str(&format!(", opaque=\"{safe_opaque}\""));
    }
    if use_qop_auth {
        hdr.push_str(&format!(", qop=auth, nc={nc}, cnonce=\"{safe_cnonce}\""));
    }
    Some(hdr)
}

pub fn extract_uri_path_and_query(full_url: &str) -> String {
    if let Some((_, after_scheme)) = full_url.split_once("://")
        && let Some(slash_idx) = after_scheme.find('/')
    {
        return after_scheme[slash_idx..].to_string();
    }
    "/".to_string()
}

pub fn format_camera_http_base(host: &str, default_port: u16) -> String {
    let trimmed = host.trim().trim_end_matches('/');
    let lc = trimmed.to_ascii_lowercase();
    if lc.starts_with("http://") || lc.starts_with("https://") {
        let after_scheme = trimmed.split_once("://").map(|(_, r)| r).unwrap_or("");
        if let Some(slash_idx) = after_scheme.find('/') {
            trimmed[..trimmed.len() - (after_scheme.len() - slash_idx)].to_string()
        } else {
            trimmed.to_string()
        }
    } else if trimmed.matches(':').count() > 1 && !trimmed.starts_with('[') {
        format!("http://[{trimmed}]:{default_port}")
    } else if trimmed.contains(':') && !trimmed.ends_with(']') {
        format!("http://{trimmed}")
    } else {
        format!("http://{trimmed}:{default_port}")
    }
}

pub fn extract_clean_camera_host(host: &str) -> String {
    let trimmed = host.trim().trim_end_matches('/');
    let authority = if let Some((_, after_scheme)) = trimmed.split_once("://") {
        let before_slash = after_scheme.split('/').next().unwrap_or(after_scheme);
        if let Some(at_idx) = before_slash.rfind('@') {
            &before_slash[at_idx + 1..]
        } else {
            before_slash
        }
    } else {
        trimmed.split('/').next().unwrap_or(trimmed)
    };
    if authority.starts_with('[')
        && let Some(end_b) = authority.find(']')
    {
        return authority[..=end_b].to_string();
    }
    if authority.matches(':').count() > 1 {
        return format!("[{authority}]");
    }
    authority.split(':').next().unwrap_or(authority).to_string()
}

pub async fn send_http_digest_request(
    client: &reqwest::Client,
    runtime: &CameraDriverRuntime,
    method: reqwest::Method,
    url: &str,
    (user, password): (Option<&str>, Option<&str>),
    (content_type, body_bytes): (Option<&str>, Option<Vec<u8>>),
    timeout: Duration,
) -> std::result::Result<(StatusCode, String), (StatusCode, String)> {
    let base_without_query = url.split('?').next().unwrap_or(url);
    if !is_safe_onvif_endpoint_url(base_without_query) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Disallowed or invalid camera HTTP endpoint URL".to_string(),
        ));
    }

    let is_audio_post = url.contains("/cgi-bin/audio.cgi") && url.contains("action=postAudio");
    let authority_key = url
        .split_once("://")
        .map(|(sch, rest)| {
            let auth = rest.split('/').next().unwrap_or(rest);
            format!("{sch}://{auth}")
        })
        .unwrap_or_else(|| base_without_query.to_string());

    let build_req = |auth_header: Option<String>, include_body: bool| {
        let mut r = client
            .request(method.clone(), url)
            .timeout(timeout)
            .header(header::CONNECTION, "close");
        if let Some(ct) = content_type {
            r = r.header(header::CONTENT_TYPE, ct);
        }
        if let Some(ah) = auth_header {
            r = r.header(header::AUTHORIZATION, ah);
        }
        if include_body && let Some(ref b) = body_bytes {
            r = r.body(b.clone());
        }
        r
    };

    let cached_auth_hdr = if let Some(u) = user.map(str::trim).filter(|s| !s.is_empty()) {
        let pw = password.unwrap_or("");
        runtime
            .get_digest_challenge(&authority_key)
            .and_then(|www_auth| {
                let uri_pq = extract_uri_path_and_query(url);
                let cnonce = format!("{:016x}", rand::random::<u64>());
                build_http_digest_auth_header(&www_auth, method.as_str(), &uri_pq, u, pw, &cnonce)
            })
    } else {
        None
    };

    let first_resp = match build_req(cached_auth_hdr, true).send().await {
        Ok(r) => r,
        Err(err) if err.is_timeout() => {
            if is_audio_post && !err.is_connect() {
                return Ok((StatusCode::OK, "OK".to_string()));
            }
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                "Camera HTTP request timed out".to_string(),
            ));
        }
        Err(err) => {
            if is_audio_post && !err.is_builder() && !err.is_connect() {
                return Ok((StatusCode::OK, "OK".to_string()));
            }
            return Err((
                StatusCode::BAD_GATEWAY,
                scrub_rtsp_credentials(&format!("Camera HTTP request failed: {err}")),
            ));
        }
    };

    let final_resp = if first_resp.status() == StatusCode::UNAUTHORIZED {
        let Some(u) = user.map(str::trim).filter(|s| !s.is_empty()) else {
            return Err((
                StatusCode::UNAUTHORIZED,
                "Camera requires authentication credentials".to_string(),
            ));
        };
        let pw = password.unwrap_or("");
        let www_auth = first_resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !www_auth.is_empty() {
            runtime.set_digest_challenge(&authority_key, www_auth);
        }
        let uri_pq = extract_uri_path_and_query(url);
        let cnonce = format!("{:016x}", rand::random::<u64>());
        let auth_hdr = if let Some(digest_hdr) =
            build_http_digest_auth_header(www_auth, method.as_str(), &uri_pq, u, pw, &cnonce)
        {
            digest_hdr
        } else {
            let b64 =
                base64::engine::general_purpose::STANDARD.encode(format!("{u}:{pw}").as_bytes());
            format!("Basic {b64}")
        };
        match build_req(Some(auth_hdr), true).send().await {
            Ok(r) => r,
            Err(err) if err.is_timeout() => {
                if is_audio_post && !err.is_connect() {
                    return Ok((StatusCode::OK, "OK".to_string()));
                }
                return Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "Camera authenticated HTTP request timed out".to_string(),
                ));
            }
            Err(err) => {
                if is_audio_post && !err.is_builder() && !err.is_connect() {
                    return Ok((StatusCode::OK, "OK".to_string()));
                }
                return Err((
                    StatusCode::BAD_GATEWAY,
                    scrub_rtsp_credentials(&format!(
                        "Camera authenticated HTTP request failed: {err}"
                    )),
                ));
            }
        }
    } else {
        first_resp
    };

    let mut final_resp = final_resp;
    let status = final_resp.status();
    if status.is_redirection() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "Camera redirect response HTTP {} is disallowed",
                status.as_u16()
            ),
        ));
    }
    let mut buf = Vec::new();
    loop {
        match final_resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len().saturating_add(chunk.len()) > MAX_ONVIF_SOAP_RESPONSE_BYTES {
                    return Err((
                        StatusCode::BAD_GATEWAY,
                        "Camera HTTP response exceeded maximum size".to_string(),
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(err) if err.is_timeout() => {
                if is_audio_post && status.is_success() {
                    return Ok((StatusCode::OK, "OK".to_string()));
                }
                return Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "Camera HTTP response body timed out".to_string(),
                ));
            }
            Err(err) => {
                if is_audio_post && status.is_success() {
                    return Ok((StatusCode::OK, "OK".to_string()));
                }
                return Err((
                    StatusCode::BAD_GATEWAY,
                    scrub_rtsp_credentials(&format!("Camera HTTP response read failed: {err}")),
                ));
            }
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok((status, text))
}

pub fn parse_dahua_cgi_kv(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

pub fn build_onvif_get_device_info_soap(user: Option<&str>, password: Option<&str>) -> String {
    build_onvif_soap_envelope(
        "<tds:GetDeviceInformation xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        user,
        password,
    )
}

pub fn build_onvif_get_profiles_soap(user: Option<&str>, password: Option<&str>) -> String {
    build_onvif_soap_envelope(
        "<trt:GetProfiles xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        user,
        password,
    )
}

pub fn build_onvif_get_video_sources_soap(user: Option<&str>, password: Option<&str>) -> String {
    build_onvif_soap_envelope(
        "<trt:GetVideoSources xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\"/>",
        user,
        password,
    )
}

pub fn parse_onvif_profiles_and_video_sources_xml(xml: &str) -> (Vec<String>, Vec<String>) {
    let mut profiles = Vec::new();
    let mut video_sources = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = xml[cursor..].find("Profiles") {
        let start = cursor + rel;
        let Some(gt_rel) = xml[start..].find('>') else {
            break;
        };
        let tag_header = &xml[start..start + gt_rel];
        if !tag_header.starts_with("Profiles")
            || tag_header
                .as_bytes()
                .get("Profiles".len())
                .is_some_and(|b| !b.is_ascii_whitespace() && *b != b'/')
        {
            cursor = start + "Profiles".len();
            continue;
        }
        if let Some(tok) = extract_xml_attr(tag_header, "token")
            && let Some(clean) = sanitize_onvif_token(Some(tok), "")
            && !clean.is_empty()
            && !profiles.contains(&clean)
        {
            profiles.push(clean);
        }
        cursor = start + gt_rel + 1;
    }
    if let Some(st) = extract_xml_element_text(xml, "SourceToken")
        && let Some(clean) = sanitize_onvif_token(Some(st), "")
        && !clean.is_empty()
    {
        video_sources.push(clean);
    }
    let mut vs_cursor = 0usize;
    while let Some(rel) = xml[vs_cursor..].find("VideoSources") {
        let start = vs_cursor + rel;
        let Some(gt_rel) = xml[start..].find('>') else {
            break;
        };
        let tag_header = &xml[start..start + gt_rel];
        if let Some(tok) = extract_xml_attr(tag_header, "token")
            && let Some(clean) = sanitize_onvif_token(Some(tok), "")
            && !clean.is_empty()
            && !video_sources.contains(&clean)
        {
            video_sources.push(clean);
        }
        vs_cursor = start + gt_rel + 1;
    }
    (profiles, video_sources)
}

pub fn build_onvif_get_imaging_settings_soap(
    video_source_token: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let body = format!(
        "<timg:GetImagingSettings xmlns:timg=\"http://www.onvif.org/ver20/imaging/wsdl\"><timg:VideoSourceToken>{}</timg:VideoSourceToken></timg:GetImagingSettings>",
        xml_escape_simple(video_source_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_set_imaging_settings_soap(
    video_source_token: &str,
    brightness: Option<f32>,
    contrast: Option<f32>,
    saturation: Option<f32>,
    sharpness: Option<f32>,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let mut inner = String::new();
    if let Some(b) = brightness.filter(|v| v.is_finite()) {
        inner.push_str(&format!(
            "<tt:Brightness>{:.0}</tt:Brightness>",
            b.clamp(0.0, 100.0)
        ));
    }
    if let Some(s) = saturation.filter(|v| v.is_finite()) {
        inner.push_str(&format!(
            "<tt:ColorSaturation>{:.0}</tt:ColorSaturation>",
            s.clamp(0.0, 100.0)
        ));
    }
    if let Some(c) = contrast.filter(|v| v.is_finite()) {
        inner.push_str(&format!(
            "<tt:Contrast>{:.0}</tt:Contrast>",
            c.clamp(0.0, 100.0)
        ));
    }
    if let Some(sh) = sharpness.filter(|v| v.is_finite()) {
        inner.push_str(&format!(
            "<tt:Sharpness>{:.0}</tt:Sharpness>",
            sh.clamp(0.0, 100.0)
        ));
    }
    let body = format!(
        "<timg:SetImagingSettings xmlns:timg=\"http://www.onvif.org/ver20/imaging/wsdl\"><timg:VideoSourceToken>{}</timg:VideoSourceToken><timg:ImagingSettings>{inner}</timg:ImagingSettings><timg:ForcePersistence>true</timg:ForcePersistence></timg:SetImagingSettings>",
        xml_escape_simple(video_source_token)
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn build_onvif_system_reboot_soap(user: Option<&str>, password: Option<&str>) -> String {
    build_onvif_soap_envelope(
        "<tds:SystemReboot xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\"/>",
        user,
        password,
    )
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CameraDeviceIdentity {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub serial_number: Option<String>,
    pub hardware_id: Option<String>,
}

pub fn parse_onvif_device_info_xml(xml: &str) -> CameraDeviceIdentity {
    CameraDeviceIdentity {
        manufacturer: extract_xml_element_text(xml, "Manufacturer")
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.contains('<'))
            .map(str::to_string),
        model: extract_xml_element_text(xml, "Model")
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.contains('<'))
            .map(str::to_string),
        firmware: extract_xml_element_text(xml, "FirmwareVersion")
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.contains('<'))
            .map(str::to_string),
        serial_number: extract_xml_element_text(xml, "SerialNumber")
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.contains('<'))
            .map(str::to_string),
        hardware_id: extract_xml_element_text(xml, "HardwareId")
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.contains('<'))
            .map(str::to_string),
    }
}

pub fn parse_onvif_imaging_settings_xml(
    xml: &str,
) -> (Option<f32>, Option<f32>, Option<f32>, Option<f32>) {
    let parse_tag = |tag: &str| -> Option<f32> {
        extract_xml_element_text(xml, tag)
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 100.0))
    };
    (
        parse_tag("Brightness"),
        parse_tag("Contrast"),
        parse_tag("ColorSaturation"),
        parse_tag("Sharpness"),
    )
}

/// ITU-T G.711 linear 16-bit PCM to 8-bit A-law encoder for Dahua/Intelbras HTTP audio talkback.
pub fn linear_pcm16_to_alaw(sample: i16) -> u8 {
    let mut pcm_val = (sample as i32) >> 3;
    let mask: u8 = if pcm_val >= 0 {
        0xD5
    } else {
        pcm_val = -pcm_val - 1;
        0x55
    };
    let seg: usize = if pcm_val <= 0x1F {
        0
    } else if pcm_val <= 0x3F {
        1
    } else if pcm_val <= 0x7F {
        2
    } else if pcm_val <= 0xFF {
        3
    } else if pcm_val <= 0x1FF {
        4
    } else if pcm_val <= 0x3FF {
        5
    } else if pcm_val <= 0x7FF {
        6
    } else {
        7
    };
    let aval: u8 = if seg >= 8 {
        0x7F
    } else {
        let top = (seg as u8) << 4;
        let low = if seg < 2 {
            ((pcm_val >> 1) & 0x0F) as u8
        } else {
            ((pcm_val >> seg) & 0x0F) as u8
        };
        top | low
    };
    aval ^ mask
}

pub fn convert_pcm16le_or_wav_to_alaw(input: &[u8]) -> Vec<u8> {
    if input.len() >= 12 && &input[0..4] == b"RIFF" && &input[8..12] == b"WAVE" {
        let mut cursor = 12usize;
        let mut audio_format: u16 = 1; // 1 = PCM, 3 = IEEE Float32, 6 = A-law
        let mut channels: usize = 1;
        let mut bits_per_sample: u16 = 16;
        let mut data_slice: Option<&[u8]> = None;
        while cursor + 8 <= input.len() {
            let chunk_id = &input[cursor..cursor + 4];
            let chunk_len = u32::from_le_bytes([
                input[cursor + 4],
                input[cursor + 5],
                input[cursor + 6],
                input[cursor + 7],
            ]) as usize;
            let data_start = cursor + 8;
            let end = data_start.saturating_add(chunk_len).min(input.len());
            if chunk_id == b"fmt " && end >= data_start + 16 {
                audio_format = u16::from_le_bytes([input[data_start], input[data_start + 1]]);
                channels = u16::from_le_bytes([input[data_start + 2], input[data_start + 3]]).max(1)
                    as usize;
                bits_per_sample =
                    u16::from_le_bytes([input[data_start + 14], input[data_start + 15]]);
            } else if chunk_id == b"data" {
                data_slice = Some(&input[data_start..end]);
                break;
            }
            let padded_len = chunk_len.saturating_add(chunk_len & 1);
            cursor = data_start.saturating_add(padded_len);
        }
        let Some(raw_data) = data_slice else {
            return Vec::new();
        };
        if audio_format == 6 {
            return raw_data.iter().step_by(channels.max(1)).copied().collect();
        }
        if audio_format == 3 && bits_per_sample == 32 {
            let frame_bytes = 4usize.saturating_mul(channels.max(1));
            let mut out = Vec::with_capacity(raw_data.len() / frame_bytes.max(4));
            for frame in raw_data.chunks_exact(frame_bytes.max(4)) {
                let f = f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
                let s16 = if f.is_finite() {
                    (f.clamp(-1.0, 1.0) * 32767.0) as i16
                } else {
                    0
                };
                out.push(linear_pcm16_to_alaw(s16));
            }
            return out;
        }
        if audio_format != 1 || bits_per_sample != 16 {
            return Vec::new();
        }
        let frame_bytes = 2usize.saturating_mul(channels.max(1));
        let mut out = Vec::with_capacity(raw_data.len() / frame_bytes.max(2));
        for frame in raw_data.chunks_exact(frame_bytes.max(2)) {
            let sample = i16::from_le_bytes([frame[0], frame[1]]);
            out.push(linear_pcm16_to_alaw(sample));
        }
        return out;
    }

    let mut out = Vec::with_capacity(input.len() / 2);
    for pair in input.as_chunks::<2>().0 {
        let sample = i16::from_le_bytes(*pair);
        out.push(linear_pcm16_to_alaw(sample));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_registry_resolves_aliases_manufacturer_hints_and_isolated_driver_runtime() {
        let intelbras = find_protocol_spec_by_id_or_alias("Intelbras").expect("intelbras alias");
        assert_eq!(intelbras.id, "dahua_cgi");
        assert!(intelbras.supports_dahua_cgi);
        assert_eq!(intelbras.default_talkback_mode, Some("cgi_alaw"));

        let hik = infer_protocol_spec_from_device_or_hints(
            None,
            Some("Hikvision Digital Technology"),
            Some("DS-2CD2047G2"),
            false,
            false,
            false,
            true,
        );
        assert_eq!(hik.id, "hikvision_isapi");
        assert_eq!(
            format_substream_url_for_protocol(hik, "192.0.2.80").as_deref(),
            Some("rtsp://*:*@192.0.2.80:554/Streaming/Channels/102#transport=tcp#timeout=10")
        );

        let rt = CameraDriverRuntime::new();
        rt.set_profile("sala", "http://192.0.2.70/onvif/ptz_service", "Profile000");
        rt.set_video_source("sala", "VideoSource000");
        rt.set_protocol("sala", "dahua_cgi");
        assert_eq!(
            rt.get_profile("sala", "http://192.0.2.70/onvif/ptz_service")
                .as_deref(),
            Some("Profile000")
        );
        rt.invalidate_camera("sala");
        assert!(
            rt.get_profile("sala", "http://192.0.2.70/onvif/ptz_service")
                .is_none()
        );
        assert!(rt.get_video_source("sala").is_none());
        assert!(rt.get_protocol("sala").is_none());
    }
}
