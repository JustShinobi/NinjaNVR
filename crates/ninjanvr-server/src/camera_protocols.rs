use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub supports_onvif_system_log: bool,
    pub supports_onvif_pull_events: bool,
    pub pan_tilt_only_default: bool,
}

pub const TAPO_PROTOCOL_SPEC: CameraProtocolSpec = CameraProtocolSpec {
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
    supports_onvif_system_log: false,
    supports_onvif_pull_events: true,
    pan_tilt_only_default: true,
};

pub const DAHUA_CGI_PROTOCOL_SPEC: CameraProtocolSpec = CameraProtocolSpec {
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
    supports_onvif_system_log: false,
    supports_onvif_pull_events: false,
    pan_tilt_only_default: true,
};

pub const HIKVISION_ISAPI_PROTOCOL_SPEC: CameraProtocolSpec = CameraProtocolSpec {
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
    supports_onvif_system_log: true,
    supports_onvif_pull_events: true,
    pan_tilt_only_default: false,
};

pub const REOLINK_PROTOCOL_SPEC: CameraProtocolSpec = CameraProtocolSpec {
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
    supports_onvif_system_log: true,
    supports_onvif_pull_events: true,
    pan_tilt_only_default: false,
};

pub const AXIS_VAPIX_PROTOCOL_SPEC: CameraProtocolSpec = CameraProtocolSpec {
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
    supports_onvif_system_log: true,
    supports_onvif_pull_events: true,
    pan_tilt_only_default: false,
};

pub const ONVIF_PROTOCOL_SPEC: CameraProtocolSpec = CameraProtocolSpec {
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
    supports_onvif_system_log: true,
    supports_onvif_pull_events: true,
    pan_tilt_only_default: false,
};

pub const RTSP_ONLY_PROTOCOL_SPEC: CameraProtocolSpec = CameraProtocolSpec {
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
    supports_onvif_system_log: false,
    supports_onvif_pull_events: false,
    pan_tilt_only_default: false,
};

pub const CAMERA_PROTOCOL_REGISTRY: &[CameraProtocolSpec] = &[
    TAPO_PROTOCOL_SPEC,
    DAHUA_CGI_PROTOCOL_SPEC,
    HIKVISION_ISAPI_PROTOCOL_SPEC,
    REOLINK_PROTOCOL_SPEC,
    AXIS_VAPIX_PROTOCOL_SPEC,
    ONVIF_PROTOCOL_SPEC,
    RTSP_ONLY_PROTOCOL_SPEC,
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
        return &TAPO_PROTOCOL_SPEC;
    }
    if dahua_cgi_ok || is_dahua_hint {
        return &DAHUA_CGI_PROTOCOL_SPEC;
    }
    if online {
        &ONVIF_PROTOCOL_SPEC
    } else {
        &RTSP_ONLY_PROTOCOL_SPEC
    }
}

pub fn format_substream_url_for_protocol(
    spec: &CameraProtocolSpec,
    clean_host: &str,
) -> Option<String> {
    spec.native_substream_template
        .map(|tpl| tpl.replace("{host}", clean_host))
}

#[derive(Debug, Default)]
struct CameraDriverRuntimeInner {
    discovered_profiles: RwLock<HashMap<(String, String), String>>,
    discovered_video_sources: RwLock<HashMap<String, String>>,
    discovered_protocols: RwLock<HashMap<String, String>>,
    digest_challenges: Mutex<HashMap<String, (String, u32)>>,
    siren_cooldown_until: Mutex<HashMap<String, f64>>,
    tapo_streamd_ports: RwLock<HashMap<String, u16>>,
    active_presets: Arc<tokio::sync::RwLock<HashMap<String, Option<String>>>>,
}

/// `AppState`-scoped runtime cache for discovered camera ONVIF tokens, protocol families,
/// active PTZ presets, and HTTP Digest challenge nonces. Replaces process-global `LazyLock`
/// singletons so each server instance and test harness has isolated, invalidatable state.
#[derive(Debug, Clone, Default)]
pub struct CameraDriverRuntime {
    inner: Arc<CameraDriverRuntimeInner>,
}

impl CameraDriverRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_active_presets(
        active_presets: Arc<tokio::sync::RwLock<HashMap<String, Option<String>>>>,
    ) -> Self {
        Self {
            inner: Arc::new(CameraDriverRuntimeInner {
                active_presets,
                ..Default::default()
            }),
        }
    }

    pub fn active_presets(&self) -> &Arc<tokio::sync::RwLock<HashMap<String, Option<String>>>> {
        &self.inner.active_presets
    }

    pub async fn get_active_preset(
        &self,
        camera: &str,
        fallback_return_preset: Option<&str>,
    ) -> Option<String> {
        self.inner
            .active_presets
            .read()
            .await
            .get(camera)
            .cloned()
            .flatten()
            .or_else(|| {
                fallback_return_preset
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            })
    }

    pub async fn set_active_preset(&self, camera: &str, preset: Option<String>) {
        self.inner
            .active_presets
            .write()
            .await
            .insert(camera.to_string(), preset);
    }

    pub async fn clear_active_preset_if_matching(&self, camera: &str, candidates: &[&str]) {
        let mut guard = self.inner.active_presets.write().await;
        if guard
            .get(camera)
            .and_then(|v| v.as_deref())
            .is_some_and(|active| {
                candidates
                    .iter()
                    .any(|cand| active.eq_ignore_ascii_case(cand))
            })
        {
            guard.insert(camera.to_string(), None);
        }
    }

    pub fn get_profile(&self, camera: &str, endpoint: &str) -> Option<String> {
        self.inner.discovered_profiles.read().ok().and_then(|g| {
            g.iter().find_map(|((cam, ep), prof)| {
                (cam == camera && ep == endpoint).then(|| prof.clone())
            })
        })
    }

    pub fn set_profile(&self, camera: &str, endpoint: &str, profile: &str) {
        if let Ok(mut g) = self.inner.discovered_profiles.write() {
            let key = (camera.to_string(), endpoint.to_string());
            if g.len() >= 128 && !g.contains_key(&key) {
                g.clear();
            }
            g.insert(key, profile.to_string());
        }
    }

    pub fn get_video_source(&self, camera: &str) -> Option<String> {
        self.inner
            .discovered_video_sources
            .read()
            .ok()
            .and_then(|g| g.get(camera).cloned())
    }

    pub fn set_video_source(&self, camera: &str, vs_token: &str) {
        if let Ok(mut g) = self.inner.discovered_video_sources.write() {
            if g.len() >= 128 && !g.contains_key(camera) {
                g.clear();
            }
            g.insert(camera.to_string(), vs_token.to_string());
        }
    }

    pub fn get_protocol(&self, camera: &str) -> Option<String> {
        self.inner
            .discovered_protocols
            .read()
            .ok()
            .and_then(|g| g.get(camera).cloned())
    }

    pub fn set_protocol(&self, camera: &str, protocol: &str) {
        if let Ok(mut g) = self.inner.discovered_protocols.write() {
            if g.len() >= 128 && !g.contains_key(camera) {
                g.clear();
            }
            g.insert(camera.to_string(), protocol.to_string());
        }
    }

    pub fn get_digest_challenge(&self, authority_key: &str) -> Option<String> {
        self.inner
            .digest_challenges
            .lock()
            .ok()
            .and_then(|g| g.get(authority_key).map(|(hdr, _)| hdr.clone()))
    }

    /// Returns the cached `WWW-Authenticate` header for `authority_key` along with the
    /// next monotonically incremented RFC 2617 / RFC 7616 nonce-count (`nc >= 2`).
    pub fn next_digest_challenge(&self, authority_key: &str) -> Option<(String, u32)> {
        self.inner.digest_challenges.lock().ok().and_then(|mut g| {
            let entry = g.get_mut(authority_key)?;
            entry.1 = entry.1.saturating_add(1).max(2);
            Some((entry.0.clone(), entry.1))
        })
    }

    pub fn set_digest_challenge(&self, authority_key: &str, www_authenticate: &str) {
        if let Ok(mut g) = self.inner.digest_challenges.lock() {
            if g.len() >= 128 && !g.contains_key(authority_key) {
                g.clear();
            }
            g.insert(authority_key.to_string(), (www_authenticate.to_string(), 1));
        }
    }

    pub fn try_claim_siren_trigger(&self, camera: &str, now_sec: f64, cooldown_sec: f64) -> bool {
        let Ok(mut g) = self.inner.siren_cooldown_until.lock() else {
            return false;
        };
        if let Some(&until) = g.get(camera)
            && now_sec < until
        {
            return false;
        }
        if g.len() >= 128 && !g.contains_key(camera) {
            g.clear();
        }
        g.insert(camera.to_string(), now_sec + cooldown_sec.max(1.0));
        true
    }

    pub fn get_tapo_streamd_port(&self, camera: &str) -> u16 {
        self.inner
            .tapo_streamd_ports
            .read()
            .ok()
            .and_then(|g| g.get(camera).copied())
            .unwrap_or(8800)
    }

    pub fn set_tapo_streamd_port(&self, camera: &str, port: u16) {
        if let Ok(mut g) = self.inner.tapo_streamd_ports.write() {
            if g.len() >= 128 && !g.contains_key(camera) {
                g.clear();
            }
            g.insert(camera.to_string(), port);
        }
    }

    pub fn invalidate_camera(&self, camera: &str) {
        if let Ok(mut g) = self.inner.discovered_profiles.write() {
            g.retain(|(cam, _), _| cam != camera);
        }
        if let Ok(mut g) = self.inner.discovered_video_sources.write() {
            g.remove(camera);
        }
        if let Ok(mut g) = self.inner.discovered_protocols.write() {
            g.remove(camera);
        }
        if let Ok(mut g) = self.inner.siren_cooldown_until.lock() {
            g.remove(camera);
        }
        if let Ok(mut g) = self.inner.tapo_streamd_ports.write() {
            g.remove(camera);
        }
    }

    pub fn get_any_profile(&self, camera: &str) -> Option<String> {
        self.inner.discovered_profiles.read().ok().and_then(|g| {
            g.iter()
                .find_map(|((cam, _), prof)| (cam == camera).then(|| prof.clone()))
        })
    }

    pub fn resolve_probe_target(
        &self,
        camera: &str,
        cam_cfg: &ninjanvr_config::CameraConfig,
    ) -> CameraProbeTarget {
        resolve_camera_probe_target(self, camera, cam_cfg)
    }

    pub fn resolve_probe_target_with_config(
        &self,
        cfg: &ninjanvr_config::NinjaConfig,
        camera: &str,
        cam_cfg: &ninjanvr_config::CameraConfig,
    ) -> CameraProbeTarget {
        resolve_camera_probe_target_with_config(self, cfg, camera, cam_cfg)
    }

    /// Executes an ONVIF SOAP request via `send_fn(profile_token)` and automatically falls back
    /// through the camera protocol's `preferred_profile_tokens` (plus `["Profile000", "profile_1", "Profile_1"]`)
    /// whenever the camera reports a profile-not-found fault (`ter:NoProfile`, `InvalidArgVal`, etc.).
    /// Caches the working token via `self.set_profile(camera, endpoint, token)` and returns
    /// `(status, body, resolved_profile_token)`.
    pub async fn execute_onvif_with_profile_discovery<F, Fut>(
        &self,
        camera: &str,
        endpoint: &str,
        initial_profile: &str,
        protocol_id: Option<&str>,
        mut send_fn: F,
    ) -> Result<(u16, String, String), String>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<(u16, String), String>>,
    {
        let first_res = send_fn(initial_profile.to_string()).await;
        match &first_res {
            Ok((status, body)) if !is_onvif_profile_not_found_response(*status, body) => {
                if (200..300).contains(status) && extract_onvif_soap_fault(body).is_none() {
                    self.set_profile(camera, endpoint, initial_profile);
                }
                return Ok((*status, body.clone(), initial_profile.to_string()));
            }
            Err(err_msg) if !is_onvif_profile_not_found_error(err_msg) => {
                return Err(err_msg.clone());
            }
            _ => {}
        }

        for candidate in self.candidate_profile_tokens(camera, initial_profile, protocol_id) {
            match send_fn(candidate.to_string()).await {
                Ok((cand_status, cand_body)) => {
                    if !is_onvif_profile_not_found_response(cand_status, &cand_body) {
                        if (200..300).contains(&cand_status)
                            && extract_onvif_soap_fault(&cand_body).is_none()
                        {
                            self.set_profile(camera, endpoint, candidate);
                        }
                        return Ok((cand_status, cand_body, candidate.to_string()));
                    }
                }
                Err(cand_err) => {
                    if !is_onvif_profile_not_found_error(&cand_err) {
                        return Err(cand_err);
                    }
                }
            }
        }

        match first_res {
            Ok((status, body)) => Ok((status, body, initial_profile.to_string())),
            Err(err_msg) => Err(err_msg),
        }
    }

    pub fn candidate_profile_tokens(
        &self,
        camera: &str,
        initial_profile: &str,
        protocol_id: Option<&str>,
    ) -> Vec<&'static str> {
        let effective_proto = protocol_id
            .map(str::to_string)
            .or_else(|| self.get_protocol(camera));
        let spec_opt = effective_proto
            .as_deref()
            .and_then(find_protocol_spec_by_id_or_alias);

        let mut candidates: Vec<&'static str> = Vec::with_capacity(6);
        if let Some(spec) = spec_opt {
            for &tok in spec.preferred_profile_tokens {
                if tok != initial_profile && !candidates.contains(&tok) {
                    candidates.push(tok);
                }
            }
        }
        for tok in ["Profile000", "profile_1", "Profile_1"] {
            if tok != initial_profile && !candidates.contains(&tok) {
                candidates.push(tok);
            }
        }
        candidates
    }

    pub fn candidate_video_source_tokens(
        &self,
        camera: &str,
        initial_vs: &str,
        protocol_id: Option<&str>,
        extra_discovered: &[String],
    ) -> Vec<String> {
        let effective_proto = protocol_id
            .map(str::to_string)
            .or_else(|| self.get_protocol(camera));
        let spec_opt = effective_proto
            .as_deref()
            .and_then(find_protocol_spec_by_id_or_alias);

        let mut candidates: Vec<String> = vec![initial_vs.to_string()];
        if let Some(spec) = spec_opt {
            for &tok in spec.preferred_video_source_tokens {
                if !candidates.iter().any(|c| c == tok) {
                    candidates.push(tok.to_string());
                }
            }
        }
        for tok in ["raw_vs1", "VideoSource000", "00000"] {
            if !candidates.iter().any(|c| c == tok) {
                candidates.push(tok.to_string());
            }
        }
        for tok in extra_discovered {
            if !candidates.iter().any(|c| c == tok) {
                candidates.push(tok.clone());
            }
        }
        candidates
    }

    /// Probes ONVIF Imaging `GetImagingSettings` across `img_endpoints` and candidate video-source
    /// tokens over `transport`, caching the working video-source token in `CameraDriverRuntime`.
    // Grouped ONVIF Imaging probe parameters over CameraWireTransport port
    #[expect(clippy::too_many_arguments)]
    pub async fn probe_imaging_settings_with_transport(
        &self,
        transport: &dyn CameraWireTransport,
        camera: &str,
        img_endpoints: &[String],
        initial_vs: &str,
        protocol_id: Option<&str>,
        extra_discovered_vs: &[String],
        (user, pass): (Option<&str>, Option<&str>),
        timeout: Duration,
    ) -> Option<(String, (Option<f32>, Option<f32>, Option<f32>, Option<f32>))> {
        let vs_candidates = self.candidate_video_source_tokens(
            camera,
            initial_vs,
            protocol_id,
            extra_discovered_vs,
        );
        for img_ep in img_endpoints {
            for cand_vs in &vs_candidates {
                let img_soap = build_onvif_get_imaging_settings_soap(cand_vs, user, pass);
                match transport
                    .send_soap(
                        img_ep,
                        "http://www.onvif.org/ver20/imaging/wsdl",
                        "GetImagingSettings",
                        img_soap,
                        (user, pass),
                        timeout,
                    )
                    .await
                {
                    Ok(img_xml) => {
                        self.set_video_source(camera, cand_vs);
                        let tuple = parse_onvif_imaging_settings_xml(&img_xml);
                        return Some((cand_vs.clone(), tuple));
                    }
                    Err((StatusCode::GATEWAY_TIMEOUT, _)) => return None,
                    Err((_, ref e)) if e.contains("request failed") => return None,
                    Err((
                        StatusCode::NOT_FOUND
                        | StatusCode::METHOD_NOT_ALLOWED
                        | StatusCode::UNAUTHORIZED
                        | StatusCode::FORBIDDEN,
                        _,
                    )) => break,
                    Err(_) => {}
                }
            }
        }
        None
    }

    /// Applies ONVIF Imaging `SetImagingSettings` across `img_endpoints` and candidate video-source
    /// tokens over `transport`, caching the working video-source token in `CameraDriverRuntime`.
    // Grouped ONVIF Imaging update parameters over CameraWireTransport port
    #[expect(clippy::too_many_arguments)]
    pub async fn apply_imaging_settings_with_transport(
        &self,
        transport: &dyn CameraWireTransport,
        camera: &str,
        img_endpoints: &[String],
        initial_vs: &str,
        protocol_id: Option<&str>,
        settings: (Option<f32>, Option<f32>, Option<f32>, Option<f32>),
        (user, pass): (Option<&str>, Option<&str>),
        timeout: Duration,
    ) -> std::result::Result<String, String> {
        let (brightness, contrast, saturation, sharpness) = settings;
        let vs_candidates =
            self.candidate_video_source_tokens(camera, initial_vs, protocol_id, &[]);
        let mut last_img_err = String::new();
        'img_outer: for img_ep in img_endpoints {
            for cand_vs in &vs_candidates {
                let set_img_soap = build_onvif_set_imaging_settings_soap(
                    cand_vs, brightness, contrast, saturation, sharpness, user, pass,
                );
                match transport
                    .send_soap(
                        img_ep,
                        "http://www.onvif.org/ver20/imaging/wsdl",
                        "SetImagingSettings",
                        set_img_soap,
                        (user, pass),
                        timeout,
                    )
                    .await
                {
                    Ok(_) => {
                        self.set_video_source(camera, cand_vs);
                        return Ok(cand_vs.clone());
                    }
                    Err((StatusCode::GATEWAY_TIMEOUT, e)) => {
                        last_img_err = e;
                        break 'img_outer;
                    }
                    Err((_, e)) if e.contains("request failed") => {
                        last_img_err = e;
                        break 'img_outer;
                    }
                    Err((_, e)) => {
                        last_img_err = e;
                    }
                }
            }
        }
        Err(last_img_err)
    }

    /// Executes an ONVIF PTZ SOAP request via `transport` and automatically falls back through
    /// protocol-preferred and standard ONVIF profile tokens when `allow_discovery` is true and
    /// the camera returns a profile-not-found fault (`NoProfile`, `InvalidArgVal`, etc.).
    /// Caches the working profile token via `self.set_profile(camera, endpoint, token)` and returns
    /// `(resolved_profile_token, response_xml)`.
    // Grouped ONVIF PTZ profile discovery over CameraWireTransport port
    #[expect(clippy::too_many_arguments)]
    pub async fn send_ptz_soap_with_profile_discovery<F>(
        &self,
        transport: &dyn CameraWireTransport,
        camera: &str,
        endpoint: &str,
        initial_profile: &str,
        allow_discovery: bool,
        protocol_id: Option<&str>,
        action: &str,
        credentials: (Option<&str>, Option<&str>),
        timeout: Duration,
        mut build_soap: F,
    ) -> std::result::Result<(String, String), (StatusCode, String)>
    where
        F: FnMut(&str) -> String,
    {
        let first_soap = build_soap(initial_profile);
        let first_res = transport
            .send_soap(
                endpoint,
                "http://www.onvif.org/ver20/ptz/wsdl",
                action,
                first_soap,
                credentials,
                timeout,
            )
            .await;

        match first_res {
            Ok(xml) => {
                self.set_profile(camera, endpoint, initial_profile);
                Ok((initial_profile.to_string(), xml))
            }
            Err((first_status, first_err)) => {
                if !allow_discovery
                    || first_status == StatusCode::GATEWAY_TIMEOUT
                    || !is_onvif_profile_not_found_error(&first_err)
                {
                    return Err((first_status, first_err));
                }
                for candidate_prof in
                    self.candidate_profile_tokens(camera, initial_profile, protocol_id)
                {
                    let retry_soap = build_soap(candidate_prof);
                    match transport
                        .send_soap(
                            endpoint,
                            "http://www.onvif.org/ver20/ptz/wsdl",
                            action,
                            retry_soap,
                            credentials,
                            timeout,
                        )
                        .await
                    {
                        Ok(ok_xml) => {
                            self.set_profile(camera, endpoint, candidate_prof);
                            return Ok((candidate_prof.to_string(), ok_xml));
                        }
                        Err((st, e)) if is_onvif_action_not_supported_error(&e) => {
                            return Err((st, e));
                        }
                        Err((StatusCode::GATEWAY_TIMEOUT, e)) => {
                            return Err((StatusCode::GATEWAY_TIMEOUT, e));
                        }
                        Err(_) => {}
                    }
                }
                Err((first_status, first_err))
            }
        }
    }

    /// Queries ONVIF PTZ `GetStatus` via `transport` with automatic profile-token discovery,
    /// returning `(resolved_profile_token, parsed_status, raw_xml)`.
    // Grouped ONVIF PTZ status probe over CameraWireTransport port
    #[expect(clippy::too_many_arguments)]
    pub async fn fetch_ptz_status_with_discovery(
        &self,
        transport: &dyn CameraWireTransport,
        camera: &str,
        endpoint: &str,
        initial_profile: &str,
        allow_discovery: bool,
        protocol_id: Option<&str>,
        credentials: (Option<&str>, Option<&str>),
        timeout: Duration,
    ) -> std::result::Result<(String, OnvifPtzStatus, String), (StatusCode, String)> {
        let (user, pass) = credentials;
        let (resolved_prof, xml) = self
            .send_ptz_soap_with_profile_discovery(
                transport,
                camera,
                endpoint,
                initial_profile,
                allow_discovery,
                protocol_id,
                "GetStatus",
                credentials,
                timeout,
                |prof| build_onvif_get_status_soap(prof, user, pass),
            )
            .await?;
        let parsed = parse_onvif_status_xml(&xml);
        Ok((resolved_prof, parsed, xml))
    }

    /// Discovers ONVIF Media profile tokens and video source tokens across `/onvif/media_service`
    /// and `/onvif/service` via `transport`.
    pub async fn discover_onvif_media_tokens_with_transport(
        &self,
        transport: &dyn CameraWireTransport,
        onvif_base: &str,
        (user, pass): (Option<&str>, Option<&str>),
        timeout: Duration,
    ) -> (Vec<String>, Vec<String>) {
        let mut profiles = Vec::new();
        let mut video_sources = Vec::new();
        for suffix in ["media_service", "service"] {
            let ep = format!("{onvif_base}/onvif/{suffix}");
            let prof_soap = build_onvif_get_profiles_soap(user, pass);
            if let Ok(xml) = transport
                .send_soap(
                    &ep,
                    "http://www.onvif.org/ver10/media/wsdl",
                    "GetProfiles",
                    prof_soap,
                    (user, pass),
                    timeout,
                )
                .await
            {
                let (p, vs) = parse_onvif_profiles_and_video_sources_xml(&xml);
                for tok in p {
                    if !profiles.contains(&tok) {
                        profiles.push(tok);
                    }
                }
                for tok in vs {
                    if !video_sources.contains(&tok) {
                        video_sources.push(tok);
                    }
                }
                if !profiles.is_empty() {
                    break;
                }
            }
        }
        if video_sources.is_empty() {
            for suffix in ["media_service", "service"] {
                let ep = format!("{onvif_base}/onvif/{suffix}");
                let vs_soap = build_onvif_get_video_sources_soap(user, pass);
                if let Ok(xml) = transport
                    .send_soap(
                        &ep,
                        "http://www.onvif.org/ver10/media/wsdl",
                        "GetVideoSources",
                        vs_soap,
                        (user, pass),
                        timeout,
                    )
                    .await
                {
                    let (_, vs) = parse_onvif_profiles_and_video_sources_xml(&xml);
                    for tok in vs {
                        if !video_sources.contains(&tok) {
                            video_sources.push(tok);
                        }
                    }
                    if !video_sources.is_empty() {
                        break;
                    }
                }
            }
        }
        (profiles, video_sources)
    }

    // Direct scalar PTZ pan/tilt/zoom coordinates and step bounds
    #[expect(clippy::too_many_arguments)]
    pub fn compute_soft_preset_delta(
        &self,
        cur_pan: f64,
        cur_tilt: f64,
        cur_zoom: f64,
        target_pan: f64,
        target_tilt: f64,
        target_zoom: f64,
        min_step: f64,
    ) -> Option<(f64, f64, f64)> {
        compute_soft_preset_relative_delta(
            cur_pan,
            cur_tilt,
            cur_zoom,
            target_pan,
            target_tilt,
            target_zoom,
            min_step,
        )
    }

    /// Reboots a camera over `transport` by probing ONVIF `SystemReboot` across both
    /// `/onvif/device_service` and `/onvif/service`, falling back to HTTP Digest CGI
    /// (`/cgi-bin/magicBox.cgi?action=reboot`) when `allow_cgi_fallback` is true.
    pub async fn reboot_camera_at_endpoints(
        &self,
        transport: &dyn CameraWireTransport,
        onvif_base: &str,
        cgi_base: &str,
        allow_cgi_fallback: bool,
        (user, pass): (Option<&str>, Option<&str>),
        timeout: Duration,
    ) -> std::result::Result<&'static str, (StatusCode, String)> {
        let mut last_err: Option<(StatusCode, String)> = None;
        for suffix in ["device_service", "service"] {
            let dev_ep = format!("{onvif_base}/onvif/{suffix}");
            let soap = build_onvif_system_reboot_soap(user, pass);
            match transport
                .send_soap(
                    &dev_ep,
                    "http://www.onvif.org/ver10/device/wsdl",
                    "SystemReboot",
                    soap,
                    (user, pass),
                    timeout,
                )
                .await
            {
                Ok(_) => return Ok("reboot:onvif"),
                Err((st, err_msg)) => {
                    if last_err.is_none() {
                        last_err = Some((st, err_msg));
                    }
                }
            }
        }

        if allow_cgi_fallback {
            let cgi_url = format!("{cgi_base}/cgi-bin/magicBox.cgi?action=reboot");
            if let Ok((st, _)) = transport
                .send_digest_http(
                    self,
                    reqwest::Method::GET,
                    &cgi_url,
                    (user, pass),
                    (None, None),
                    timeout,
                )
                .await
                && st.is_success()
            {
                return Ok("reboot:cgi");
            }
        }

        Err(last_err.unwrap_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "Camera reboot failed across ONVIF and CGI endpoints".to_string(),
            )
        }))
    }

    /// Reboots a camera via ONVIF `SystemReboot` (`/onvif/device_service` or `/onvif/service`)
    /// with automatic fallback to Dahua/Intelbras HTTP Digest CGI (`/cgi-bin/magicBox.cgi?action=reboot`).
    pub async fn reboot_camera(
        &self,
        client: &reqwest::Client,
        camera: &str,
        cam_cfg: &ninjanvr_config::CameraConfig,
    ) -> Result<String, String> {
        let target = self.resolve_probe_target(camera, cam_cfg);
        let Some(ref host) = target.host else {
            return Err(format!(
                "Camera '{camera}' has no reachable ONVIF or HTTP host"
            ));
        };
        let timeout = Duration::from_secs(cam_cfg.onvif.timeout_sec.clamp(1, 10));
        let onvif_base = format_camera_http_base(host, target.onvif_port);
        let cgi_base = format_camera_http_base(host, target.cgi_port);
        let transport = ReqwestCameraWireTransport::new(client.clone());
        self.reboot_camera_at_endpoints(
            &transport,
            &onvif_base,
            &cgi_base,
            !target.is_tapo_hint,
            (target.user.as_deref(), target.password.as_deref()),
            timeout,
        )
        .await
        .map(str::to_string)
        .map_err(|(_, msg)| msg)
    }

    /// Synthesizes a G.711 A-law siren payload for `duration_sec` and dispatches it to `(host, onvif_port)`
    /// using the protocol-appropriate transport (`"tapo_streamd"` on TCP `:8800` or `"dahua_cgi"` `postAudio`).
    // Grouped siren dispatch parameters for resolved host/port and credentials
    #[expect(clippy::too_many_arguments)]
    pub async fn trigger_siren_at_host(
        &self,
        client: &reqwest::Client,
        camera: &str,
        (host, onvif_port): (&str, u16),
        (user, pass): (Option<&str>, Option<&str>),
        cloud_password: Option<&str>,
        is_tapo_proto: bool,
        duration_sec: f32,
    ) -> std::result::Result<&'static str, (StatusCode, String)> {
        let dur_f32 = if duration_sec.is_finite() {
            duration_sec.clamp(0.2, 15.0)
        } else {
            3.5
        };
        let siren_alaw = generate_siren_alaw_bytes(dur_f32);
        let siren_timeout =
            Duration::from_secs((dur_f32.ceil() as u64).saturating_add(6).clamp(5, 25));

        send_camera_talkback_or_siren_alaw(
            client,
            self,
            camera,
            (host, onvif_port),
            (user, pass),
            cloud_password,
            is_tapo_proto,
            siren_alaw,
            true,
            siren_timeout,
        )
        .await
        .map(|_| {
            if is_tapo_proto {
                "tapo_streamd"
            } else {
                "dahua_cgi"
            }
        })
    }

    /// Synthesizes a G.711 A-law siren payload for `duration_sec` and dispatches it to the camera
    /// using the protocol-appropriate transport (`"tapo_streamd"` on TCP `:8800` or `"dahua_cgi"` `postAudio`).
    pub async fn trigger_camera_siren(
        &self,
        client: &reqwest::Client,
        camera: &str,
        cam_cfg: &ninjanvr_config::CameraConfig,
        duration_sec: f64,
    ) -> Result<&'static str, String> {
        let target = self.resolve_probe_target(camera, cam_cfg);
        let Some(ref host) = target.host else {
            return Err(format!(
                "Camera '{camera}' has no reachable ONVIF or HTTP host"
            ));
        };
        let is_tapo_proto =
            target.protocol == "tapo" || (target.is_tapo_hint && !target.is_dahua_hint);
        self.trigger_siren_at_host(
            client,
            camera,
            (host, target.onvif_port),
            (target.user.as_deref(), target.password.as_deref()),
            target.cloud_password.as_deref(),
            is_tapo_proto,
            duration_sec as f32,
        )
        .await
        .map_err(|(_, err_msg)| err_msg)
    }
}

pub type WireSoapFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = std::result::Result<String, (StatusCode, String)>>
            + Send
            + 'a,
    >,
>;

pub type WireHttpFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = std::result::Result<(StatusCode, String), (StatusCode, String)>,
            > + Send
            + 'a,
    >,
>;

/// Port trait abstracting low-level ONVIF SOAP and HTTP Digest wire exchanges for `CameraDriverRuntime`.
pub trait CameraWireTransport: Send + Sync {
    fn send_soap<'a>(
        &'a self,
        endpoint: &'a str,
        wsdl_ns: &'a str,
        action: &'a str,
        soap_xml: String,
        credentials: (Option<&'a str>, Option<&'a str>),
        timeout: Duration,
    ) -> WireSoapFuture<'a>;

    fn send_digest_http<'a>(
        &'a self,
        runtime: &'a CameraDriverRuntime,
        method: reqwest::Method,
        url: &'a str,
        credentials: (Option<&'a str>, Option<&'a str>),
        body: (Option<&'a str>, Option<Vec<u8>>),
        timeout: Duration,
    ) -> WireHttpFuture<'a>;
}

/// Production `reqwest::Client` adapter implementing `CameraWireTransport`.
#[derive(Debug, Clone)]
pub struct ReqwestCameraWireTransport {
    pub client: reqwest::Client,
}

impl ReqwestCameraWireTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl CameraWireTransport for ReqwestCameraWireTransport {
    fn send_soap<'a>(
        &'a self,
        endpoint: &'a str,
        wsdl_ns: &'a str,
        action: &'a str,
        soap_xml: String,
        credentials: (Option<&'a str>, Option<&'a str>),
        timeout: Duration,
    ) -> WireSoapFuture<'a> {
        Box::pin(send_onvif_soap_request_with_ns(
            &self.client,
            endpoint,
            (wsdl_ns, action),
            soap_xml,
            credentials,
            timeout,
        ))
    }

    fn send_digest_http<'a>(
        &'a self,
        runtime: &'a CameraDriverRuntime,
        method: reqwest::Method,
        url: &'a str,
        credentials: (Option<&'a str>, Option<&'a str>),
        body: (Option<&'a str>, Option<Vec<u8>>),
        timeout: Duration,
    ) -> WireHttpFuture<'a> {
        Box::pin(send_http_digest_request(
            &self.client,
            runtime,
            method,
            url,
            credentials,
            body,
            timeout,
        ))
    }
}

/// Resolved hardware probe and control target for a camera, consolidating
/// host, ONVIF/HTTP/Streamd ports, credentials, protocol specification,
/// and preferred/discovered ONVIF tokens from `CAMERA_PROTOCOL_REGISTRY`.
#[derive(Clone, PartialEq, Eq)]
pub struct CameraProbeTarget {
    pub host: Option<String>,
    pub port: u16,
    pub onvif_port: u16,
    pub cgi_port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub cloud_password: Option<String>,
    pub protocol: &'static str,
    pub protocol_spec: &'static CameraProtocolSpec,
    pub profile_token: String,
    pub video_source_token: String,
    pub streamd_port: u16,
    pub is_tapo_hint: bool,
    pub is_dahua_hint: bool,
    pub ptz_endpoint: Option<String>,
    pub onvif_base_url: Option<String>,
    pub cgi_base_url: Option<String>,
    pub onvif_configured: bool,
}

impl std::fmt::Debug for CameraProbeTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let masked_pass = self
            .password
            .as_deref()
            .map(|p| if p.is_empty() { "" } else { "***" });
        let masked_cloud = self
            .cloud_password
            .as_deref()
            .map(|p| if p.is_empty() { "" } else { "***" });
        f.debug_struct("CameraProbeTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("onvif_port", &self.onvif_port)
            .field("cgi_port", &self.cgi_port)
            .field("user", &self.user)
            .field("password", &masked_pass)
            .field("cloud_password", &masked_cloud)
            .field("protocol", &self.protocol)
            .field("profile_token", &self.profile_token)
            .field("video_source_token", &self.video_source_token)
            .field("streamd_port", &self.streamd_port)
            .field("is_tapo_hint", &self.is_tapo_hint)
            .field("is_dahua_hint", &self.is_dahua_hint)
            .field("ptz_endpoint", &self.ptz_endpoint)
            .field("onvif_configured", &self.onvif_configured)
            .finish()
    }
}

impl CameraProbeTarget {
    pub fn host_str(&self) -> &str {
        self.host.as_deref().unwrap_or("")
    }

    pub fn is_reachable_target(&self) -> bool {
        self.host.is_some()
    }
}

fn percent_decode_userinfo_str(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push(((hi << 4) | lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct ParsedCandidateUrl {
    host: String,
    user: Option<String>,
    password: Option<String>,
    path: Option<String>,
    is_tapo_scheme: bool,
}

fn parse_rtsp_or_http_candidate_url(raw_url: &str) -> Option<ParsedCandidateUrl> {
    let trimmed = raw_url.trim();
    let (scheme, after_scheme) = trimmed.split_once("://")?;
    let scheme_lc = scheme.trim().to_ascii_lowercase();
    let is_tapo_scheme = scheme_lc == "tapo";

    let slash_idx = after_scheme.find('/');
    let authority = match slash_idx {
        Some(idx) => &after_scheme[..idx],
        None => after_scheme,
    };
    let path = slash_idx.map(|idx| {
        let raw_path = &after_scheme[idx..];
        let hash_idx = raw_path.find('#').unwrap_or(raw_path.len());
        raw_path[..hash_idx].to_string()
    });

    let host_port_part = match authority.rfind('@') {
        Some(at_idx) => &authority[at_idx + 1..],
        None => authority,
    };
    let clean_host = extract_clean_camera_host(host_port_part);
    let host_lc = clean_host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host_lc.is_empty()
        || host_lc == "localhost"
        || host_lc == "127.0.0.1"
        || host_lc.starts_with("127.")
        || host_lc == "::1"
    {
        return None;
    }
    let probe_check_url = format!("{}/onvif/service", format_camera_http_base(&clean_host, 80));
    if !is_safe_onvif_endpoint_url(&probe_check_url) {
        return None;
    }

    let mut user = None;
    let mut password = None;
    if !is_tapo_scheme && let Some(at_idx) = authority.rfind('@') {
        let userinfo = &authority[..at_idx];
        if let Some((u, p)) = userinfo.split_once(':') {
            let u_clean = percent_decode_userinfo_str(u.trim());
            if !u_clean.is_empty() && u_clean != "*" && u_clean != "***" {
                user = Some(u_clean);
            }
            let p_trimmed = p.trim();
            if !p.is_empty() && p_trimmed != "*" && p_trimmed != "***" {
                let p_clean = percent_decode_userinfo_str(p);
                if !p_clean.is_empty() {
                    password = Some(p_clean);
                }
            }
        } else {
            let u_clean = percent_decode_userinfo_str(userinfo.trim());
            if !u_clean.is_empty() && u_clean != "*" && u_clean != "***" {
                user = Some(u_clean);
            }
        }
    }

    Some(ParsedCandidateUrl {
        host: clean_host,
        user,
        password,
        path,
        is_tapo_scheme,
    })
}

/// Enriches a `CameraConfig` with upstream host, credentials, and protocol hints inferred
/// from `NinjaConfig` (including `go2rtc.streams`) when those fields are not already set on `cam_cfg`.
pub fn enrich_camera_config_from_upstream(
    cfg: &ninjanvr_config::NinjaConfig,
    camera: &str,
    cam_cfg: &ninjanvr_config::CameraConfig,
) -> ninjanvr_config::CameraConfig {
    let mut enriched = cam_cfg.clone();
    let Some(inferred) = cfg.infer_camera_upstream(camera) else {
        return enriched;
    };
    if enriched
        .onvif
        .host
        .as_deref()
        .is_none_or(|h| h.trim().is_empty())
    {
        enriched.onvif.host = Some(inferred.host);
        if enriched.onvif.port == 80 && inferred.default_onvif_port != 80 {
            enriched.onvif.port = inferred.default_onvif_port;
        }
    }
    if enriched
        .onvif
        .user
        .as_deref()
        .is_none_or(|u| u.trim().is_empty())
    {
        enriched.onvif.user = inferred.user;
    }
    if enriched
        .onvif
        .password
        .as_deref()
        .is_none_or(|p| p.is_empty() || p.trim() == "*" || p.trim() == "***")
    {
        enriched.onvif.password = inferred.password;
    }
    if enriched
        .hardware
        .protocol
        .as_deref()
        .is_none_or(|p| p.trim().is_empty())
    {
        if inferred.is_tapo_rtsp {
            enriched.hardware.protocol = Some("tapo".to_string());
        } else if inferred.is_dahua_rtsp {
            enriched.hardware.protocol = Some("dahua_cgi".to_string());
        } else if let Some(ref rtsp_path) = inferred.rtsp_path {
            let path_lc = rtsp_path.to_ascii_lowercase();
            if let Some(spec) = CAMERA_PROTOCOL_REGISTRY.iter().find(|spec| {
                spec.rtsp_path_markers
                    .iter()
                    .any(|m| path_lc.contains(&m.to_ascii_lowercase()))
            }) {
                enriched.hardware.protocol = Some(spec.id.to_string());
            }
        }
    }
    enriched
}

pub fn resolve_camera_probe_target_with_config(
    runtime: &CameraDriverRuntime,
    cfg: &ninjanvr_config::NinjaConfig,
    camera: &str,
    cam_cfg: &ninjanvr_config::CameraConfig,
) -> CameraProbeTarget {
    let enriched = enrich_camera_config_from_upstream(cfg, camera, cam_cfg);
    let mut target = resolve_camera_probe_target(runtime, camera, &enriched);
    target.onvif_configured = cam_cfg.onvif.is_configured();
    target
}

/// Consolidates camera host, port, credentials, cloud password, protocol specification,
/// `profile_token`, `video_source_token`, and Tapo `streamd_port` resolution using
/// `CAMERA_PROTOCOL_REGISTRY` and `CameraDriverRuntime`.
pub fn resolve_camera_probe_target(
    runtime: &CameraDriverRuntime,
    camera: &str,
    cam_cfg: &ninjanvr_config::CameraConfig,
) -> CameraProbeTarget {
    let mut inferred_host: Option<String> = None;
    let mut inferred_user: Option<String> = None;
    let mut inferred_pass: Option<String> = None;
    let mut rtsp_matched_spec: Option<&'static CameraProtocolSpec> = None;
    let mut saw_tapo_url = false;

    let candidate_urls = cam_cfg
        .ffmpeg
        .inputs
        .iter()
        .map(|i| i.path.as_str())
        .chain(cam_cfg.webui_url.as_deref());

    for raw_url in candidate_urls {
        if raw_url.trim().is_empty() {
            continue;
        }
        if raw_url.trim().to_ascii_lowercase().starts_with("tapo://") {
            saw_tapo_url = true;
        }
        let Some(parsed) = parse_rtsp_or_http_candidate_url(raw_url) else {
            continue;
        };
        if parsed.is_tapo_scheme {
            saw_tapo_url = true;
        }
        if inferred_host.is_none() {
            inferred_host = Some(parsed.host);
        }
        if inferred_user.is_none() {
            inferred_user = parsed.user;
        }
        if inferred_pass.is_none() {
            inferred_pass = parsed.password;
        }
        if rtsp_matched_spec.is_none()
            && let Some(ref path_str) = parsed.path
        {
            let path_lc = path_str.to_ascii_lowercase();
            rtsp_matched_spec = CAMERA_PROTOCOL_REGISTRY.iter().find(|spec| {
                spec.rtsp_path_markers
                    .iter()
                    .any(|marker| path_lc.contains(&marker.to_ascii_lowercase()))
            });
        }
    }

    let explicit_onvif_host_raw = cam_cfg
        .onvif
        .host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut host = explicit_onvif_host_raw
        .map(extract_clean_camera_host)
        .filter(|h| !h.is_empty())
        .or(inferred_host);

    let user = cam_cfg
        .onvif
        .user
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*" && *s != "***")
        .map(str::to_string)
        .or(inferred_user);

    let password = cam_cfg
        .onvif
        .password
        .as_deref()
        .filter(|p| !p.is_empty() && p.trim() != "***" && p.trim() != "*")
        .map(str::to_string)
        .or(inferred_pass);

    let cloud_password = cam_cfg
        .hardware
        .cloud_password
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "***" && *p != "*")
        .map(str::to_string);

    let explicit_spec = cam_cfg
        .hardware
        .protocol
        .as_deref()
        .and_then(find_protocol_spec_by_id_or_alias);
    let cached_spec = runtime
        .get_protocol(camera)
        .as_deref()
        .and_then(find_protocol_spec_by_id_or_alias);

    let is_tapo_hint = saw_tapo_url
        || explicit_spec.is_some_and(|s| s.id == "tapo")
        || cached_spec.is_some_and(|s| s.id == "tapo")
        || rtsp_matched_spec.is_some_and(|s| s.id == "tapo")
        || cam_cfg.onvif.port == 2020
        || (cloud_password.is_some() && explicit_spec.is_none() && rtsp_matched_spec.is_none());

    let is_dahua_hint = explicit_spec.is_some_and(|s| s.supports_dahua_cgi)
        || cached_spec.is_some_and(|s| s.supports_dahua_cgi)
        || rtsp_matched_spec.is_some_and(|s| s.supports_dahua_cgi);

    let protocol_spec: &'static CameraProtocolSpec = if let Some(exp) =
        explicit_spec.filter(|s| s.id != "onvif" && s.id != "rtsp_only")
    {
        exp
    } else if let Some(cached) = cached_spec.filter(|s| s.id != "onvif" && s.id != "rtsp_only") {
        cached
    } else if let Some(rtsp_spec) = rtsp_matched_spec {
        rtsp_spec
    } else if is_tapo_hint {
        &TAPO_PROTOCOL_SPEC
    } else if is_dahua_hint {
        &DAHUA_CGI_PROTOCOL_SPEC
    } else if let Some(exp) = explicit_spec {
        exp
    } else if let Some(cached) = cached_spec {
        cached
    } else if host.is_some() {
        &ONVIF_PROTOCOL_SPEC
    } else {
        &RTSP_ONLY_PROTOCOL_SPEC
    };

    let onvif_port = if cam_cfg.onvif.port != 80 && cam_cfg.onvif.port != 0 {
        cam_cfg.onvif.port
    } else if cam_cfg.onvif.is_configured() {
        if is_tapo_hint || protocol_spec.id == "tapo" || explicit_spec.is_some() {
            protocol_spec.default_onvif_port
        } else {
            cam_cfg.onvif.port
        }
    } else {
        protocol_spec.default_onvif_port
    };

    let cgi_port = if onvif_port != 2020 { onvif_port } else { 80 };

    let mut ptz_endpoint = None;
    let mut onvif_base_url = None;
    let mut cgi_base_url = None;

    if let Some(ref clean_h) = host {
        let mut adjusted_onvif = cam_cfg.onvif.clone();
        adjusted_onvif.host = Some(
            explicit_onvif_host_raw
                .unwrap_or(clean_h.as_str())
                .to_string(),
        );
        adjusted_onvif.port = onvif_port;
        if let Some(ep) = resolve_onvif_ptz_endpoint(&adjusted_onvif) {
            ptz_endpoint = Some(ep);
            onvif_base_url = Some(format_camera_http_base(clean_h, onvif_port));
            cgi_base_url = Some(format_camera_http_base(clean_h, cgi_port));
        } else {
            host = None;
        }
    }

    let default_prof = protocol_spec
        .preferred_profile_tokens
        .first()
        .copied()
        .unwrap_or("Profile000");
    let profile_token = cam_cfg
        .hardware
        .profile_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|t| sanitize_onvif_token(Some(t), ""))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ptz_endpoint
                .as_deref()
                .and_then(|ep| runtime.get_profile(camera, ep))
        })
        .or_else(|| runtime.get_any_profile(camera))
        .unwrap_or_else(|| default_prof.to_string());

    let default_vs = protocol_spec
        .preferred_video_source_tokens
        .first()
        .copied()
        .unwrap_or("VideoSource000");
    let video_source_token = cam_cfg
        .hardware
        .video_source_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|t| sanitize_onvif_token(Some(t), ""))
        .filter(|s| !s.is_empty())
        .or_else(|| runtime.get_video_source(camera))
        .unwrap_or_else(|| default_vs.to_string());

    let streamd_port = runtime.get_tapo_streamd_port(camera);

    CameraProbeTarget {
        host,
        port: onvif_port,
        onvif_port,
        cgi_port,
        user,
        password,
        cloud_password,
        protocol: protocol_spec.id,
        protocol_spec,
        profile_token,
        video_source_token,
        streamd_port,
        is_tapo_hint: is_tapo_hint || protocol_spec.id == "tapo",
        is_dahua_hint: is_dahua_hint || protocol_spec.supports_dahua_cgi,
        ptz_endpoint,
        onvif_base_url,
        cgi_base_url,
        onvif_configured: cam_cfg.onvif.is_configured(),
    }
}

/// Computes the clamped `(dx, dy, dz)` relative PTZ translation delta toward a soft preset target,
/// returning `None` when all three axes are already within `min_step` (deadband) or if any coordinate is non-finite.
pub fn compute_soft_preset_relative_delta(
    cur_pan: f64,
    cur_tilt: f64,
    cur_zoom: f64,
    target_pan: f64,
    target_tilt: f64,
    target_zoom: f64,
    min_step: f64,
) -> Option<(f64, f64, f64)> {
    if !cur_pan.is_finite()
        || !cur_tilt.is_finite()
        || !cur_zoom.is_finite()
        || !target_pan.is_finite()
        || !target_tilt.is_finite()
        || !target_zoom.is_finite()
    {
        return None;
    }
    let threshold = if min_step.is_finite() && min_step > 0.0 {
        min_step
    } else {
        0.0
    };
    let dx = (target_pan - cur_pan).clamp(-1.0, 1.0);
    let dy = (target_tilt - cur_tilt).clamp(-1.0, 1.0);
    let dz = (target_zoom - cur_zoom).clamp(-1.0, 1.0);
    if dx.abs() <= threshold && dy.abs() <= threshold && dz.abs() <= threshold {
        None
    } else {
        Some((dx, dy, dz))
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
    "Syslog",
    "AutoMaintain",
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
        "Syslog" => {
            if let Some(en) = kv.get("table.Syslog.Enable") {
                state_map.insert(
                    "syslog_enabled".to_string(),
                    json!(en.eq_ignore_ascii_case("true")),
                );
            }
            if let Some(srv) = kv.get("table.Syslog.Server") {
                state_map.insert("syslog_server".to_string(), json!(srv));
            }
            if let Some(port) = kv
                .get("table.Syslog.Port")
                .and_then(|s| s.parse::<u16>().ok())
            {
                state_map.insert("syslog_port".to_string(), json!(port));
            }
        }
        "AutoMaintain" => {
            if let Some(en) = kv.get("table.AutoMaintain.AutoRebootEnable") {
                state_map.insert(
                    "auto_reboot_enabled".to_string(),
                    json!(en.eq_ignore_ascii_case("true")),
                );
            }
            if let Some(day) = kv
                .get("table.AutoMaintain.AutoRebootDay")
                .and_then(|s| s.parse::<i8>().ok())
            {
                state_map.insert("auto_reboot_day".to_string(), json!(day));
            }
            if let Some(hr) = kv
                .get("table.AutoMaintain.AutoRebootHour")
                .and_then(|s| s.parse::<u8>().ok())
            {
                state_map.insert("auto_reboot_hour".to_string(), json!(hr));
            }
            if let Some(min) = kv
                .get("table.AutoMaintain.AutoRebootMinute")
                .and_then(|s| s.parse::<u8>().ok())
            {
                state_map.insert("auto_reboot_minute".to_string(), json!(min));
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
    pub syslog_enabled: Option<bool>,
    pub syslog_server: Option<String>,
    pub syslog_port: Option<u16>,
    pub auto_reboot_enabled: Option<bool>,
    pub auto_reboot_day: Option<i8>,
    pub auto_reboot_hour: Option<u8>,
    pub auto_reboot_minute: Option<u8>,
    pub reboot: Option<bool>,
    pub apply_suggested_onvif: Option<bool>,
    pub apply_suggested_substream: Option<bool>,
    pub protocol: Option<String>,
    pub profile_token: Option<String>,
    pub video_source_token: Option<String>,
    pub cloud_password: Option<String>,
    pub trigger_siren: Option<bool>,
    pub siren_duration_sec: Option<f32>,
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
    if let Some(sys_en) = body.syslog_enabled {
        cgi_pairs.push(format!(
            "Syslog.Enable={}",
            if sys_en { "true" } else { "false" }
        ));
    }
    if let Some(ref sys_srv) = body.syslog_server {
        let clean: String = sys_srv
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
            .take(128)
            .collect();
        if !clean.is_empty() {
            cgi_pairs.push(format!("Syslog.Server={clean}"));
        }
    }
    if let Some(sys_port) = body.syslog_port {
        cgi_pairs.push(format!("Syslog.Port={}", sys_port.max(1)));
    }
    if let Some(ar_en) = body.auto_reboot_enabled {
        cgi_pairs.push(format!(
            "AutoMaintain.AutoRebootEnable={}",
            if ar_en { "true" } else { "false" }
        ));
    }
    if let Some(ar_day) = body.auto_reboot_day {
        cgi_pairs.push(format!(
            "AutoMaintain.AutoRebootDay={}",
            ar_day.clamp(-1, 7)
        ));
    }
    if let Some(ar_hr) = body.auto_reboot_hour {
        cgi_pairs.push(format!("AutoMaintain.AutoRebootHour={}", ar_hr.min(23)));
    }
    if let Some(ar_min) = body.auto_reboot_minute {
        cgi_pairs.push(format!("AutoMaintain.AutoRebootMinute={}", ar_min.min(59)));
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
    let after_scheme = trimmed.split_once("://").map_or("", |(_, r)| r);
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
            let after_scheme = trimmed.split_once("://").map_or("", |(_, r)| r);
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
            let candidate_local = raw_tag.rsplit_once(':').map_or(raw_tag, |(_, l)| l.trim());
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
            let candidate_local = tag_token.rsplit_once(':').map_or(tag_token, |(_, l)| l);
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
        let candidate_local = tag_clean.rsplit_once(':').map_or(tag_clean, |(_, l)| l);
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
            .map_or(tag_token.trim_end_matches('/'), |(_, l)| l);
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
            .map_or(tag_token.trim_end_matches('/'), |(_, l)| l);
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

pub fn is_onvif_profile_not_found_response(status: u16, body: &str) -> bool {
    if is_onvif_profile_not_found_error(body) {
        return true;
    }
    if let Some(fault) = extract_onvif_soap_fault(body)
        && is_onvif_profile_not_found_error(&fault)
    {
        return true;
    }
    status >= 400 && is_onvif_profile_not_found_error(body)
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
    build_http_digest_auth_header_with_nc(
        www_authenticate,
        method,
        uri_path_and_query,
        user,
        password,
        cnonce,
        1,
    )
}

pub fn build_http_digest_auth_header_with_nc(
    www_authenticate: &str,
    method: &str,
    uri_path_and_query: &str,
    user: &str,
    password: &str,
    cnonce: &str,
    nonce_count: u32,
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
    let nc = format!("{:08x}", nonce_count.max(1));

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
        let after_scheme = trimmed.split_once("://").map_or("", |(_, r)| r);
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
    let authority_key = url.split_once("://").map_or_else(
        || base_without_query.to_string(),
        |(sch, rest)| {
            let auth = rest.split('/').next().unwrap_or(rest);
            format!("{sch}://{auth}")
        },
    );

    let build_req = |auth_header: Option<String>, include_body: bool| {
        let mut r = client.request(method.clone(), url).timeout(timeout);
        if is_audio_post {
            r = r.header(header::CONNECTION, "close");
        }
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
            .next_digest_challenge(&authority_key)
            .and_then(|(www_auth, nc)| {
                let uri_pq = extract_uri_path_and_query(url);
                let cnonce = format!("{:016x}", rand::random::<u64>());
                build_http_digest_auth_header_with_nc(
                    &www_auth,
                    method.as_str(),
                    &uri_pq,
                    u,
                    pw,
                    &cnonce,
                    nc,
                )
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

pub const TAPO_STREAMD_BOUNDARY: &[u8] = b"----client-stream-boundary--";
pub const TAPO_AUDIO_RATE: usize = 8000;
pub const TAPO_AUDIO_CHUNK_BYTES: usize = 160; // 20 ms at 8 kHz G.711 A-law
pub const TAPO_ALAW_SILENCE: u8 = 0xD5;
const TAPO_STREAM_TYPE_PCMA: u8 = 0x90;
const TAPO_PCR_PID: u16 = 0x1FFF;
const TAPO_PES_PID: u16 = 0x0100;
const TAPO_PMT_PID: u16 = 0x1000;

/// Synthesizes a sweeping two-tone tactical siren (`750 Hz – 1450 Hz` at `8000 Hz` mono)
/// encoded as ITU-T G.711 A-law (`PCMA`) bytes for immediate playback on camera speakers.
pub fn generate_siren_alaw_bytes(duration_sec: f32) -> Vec<u8> {
    let clamped_sec = if duration_sec.is_finite() {
        duration_sec.clamp(0.2, 15.0)
    } else {
        3.5
    };
    let total_samples = ((TAPO_AUDIO_RATE as f32) * clamped_sec).round() as usize;
    let mut out = Vec::with_capacity(total_samples);
    let mut phase: f64 = 0.0;
    let two_pi = 2.0 * std::f64::consts::PI;
    for i in 0..total_samples {
        let t = (i as f64) / (TAPO_AUDIO_RATE as f64);
        let sweep = 0.5 * (1.0 + (two_pi * 2.2 * t).sin());
        let freq = 750.0 + 700.0 * sweep;
        phase += two_pi * freq / (TAPO_AUDIO_RATE as f64);
        let sample = (26_000.0 * phase.sin()).round() as i16;
        out.push(linear_pcm16_to_alaw(sample));
    }
    out
}

/// Computes ISO/IEC 13818-1 MPEG-2 CRC-32 (`poly = 0x04C11DB7`, `init = 0xFFFFFFFF`) for PAT/PMT tables.
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04C1_1DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn build_mpegts_packet(pid: u16, payload: &[u8], continuity_counter: u8, start: bool) -> [u8; 188] {
    let mut pkt = [0xFFu8; 188];
    let payload_len = payload.len().min(184);
    pkt[0] = 0x47;
    pkt[1] = (if start { 0x40 } else { 0x00 }) | (((pid >> 8) as u8) & 0x1F);
    pkt[2] = (pid & 0xFF) as u8;
    if payload_len == 184 {
        pkt[3] = 0x10 | (continuity_counter & 0x0F);
        pkt[4..188].copy_from_slice(&payload[..184]);
    } else {
        let fill = 184 - payload_len - 1;
        pkt[3] = 0x30 | (continuity_counter & 0x0F);
        pkt[4] = fill as u8;
        if fill > 0 {
            pkt[5] = 0x00;
            // pkt[6..4 + 1 + fill] already filled with 0xFF stuffing bytes
        }
        let payload_offset = 4 + 1 + fill;
        pkt[payload_offset..188].copy_from_slice(&payload[..payload_len]);
    }
    pkt
}

fn build_mpegts_psi_section(table_id: u8, body: &[u8]) -> Vec<u8> {
    let section_len = 5 + body.len() + 4;
    let mut sec = Vec::with_capacity(3 + section_len);
    sec.push(table_id);
    sec.push(0xB0 | (((section_len >> 8) as u8) & 0x0F));
    sec.push((section_len & 0xFF) as u8);
    sec.extend_from_slice(&[0x00, 0x01, 0xC1, 0x00, 0x00]);
    sec.extend_from_slice(body);
    let crc = crc32_mpeg2(&sec);
    sec.extend_from_slice(&crc.to_be_bytes());
    sec
}

/// Builds the 2-packet (376-byte) MPEG-TS PAT + PMT header declaring an 8 kHz G.711 A-law (`0x90`)
/// audio stream on PID `0x0100` for Tapo `Streamd` (`:8800`) two-way audio / siren playback.
pub fn build_tapo_mpegts_audio_header_packets() -> Vec<u8> {
    let mut pat_body = Vec::with_capacity(4);
    pat_body.extend_from_slice(&1u16.to_be_bytes());
    pat_body.extend_from_slice(&(0xE000u16 | TAPO_PMT_PID).to_be_bytes());
    let pat_sec = build_mpegts_psi_section(0x00, &pat_body);
    let mut pat_payload = Vec::with_capacity(1 + pat_sec.len());
    pat_payload.push(0x00);
    pat_payload.extend_from_slice(&pat_sec);

    let mut pmt_body = Vec::with_capacity(9);
    pmt_body.extend_from_slice(&(0xE000u16 | TAPO_PCR_PID).to_be_bytes());
    pmt_body.extend_from_slice(&0xF000u16.to_be_bytes());
    pmt_body.push(TAPO_STREAM_TYPE_PCMA);
    pmt_body.extend_from_slice(&(0xE000u16 | TAPO_PES_PID).to_be_bytes());
    pmt_body.extend_from_slice(&0xF000u16.to_be_bytes());
    let pmt_sec = build_mpegts_psi_section(0x02, &pmt_body);
    let mut pmt_payload = Vec::with_capacity(1 + pmt_sec.len());
    pmt_payload.push(0x00);
    pmt_payload.extend_from_slice(&pmt_sec);

    let mut out = Vec::with_capacity(376);
    out.extend_from_slice(&build_mpegts_packet(0x0000, &pat_payload, 0, true));
    out.extend_from_slice(&build_mpegts_packet(TAPO_PMT_PID, &pmt_payload, 0, true));
    out
}

fn encode_mpegts_pts_33bit(pts: u64) -> [u8; 5] {
    let v = pts & 0x1_FFFF_FFFF;
    [
        0x21 | ((((v >> 30) & 0x07) as u8) << 1),
        ((v >> 22) & 0xFF) as u8,
        ((((v >> 15) & 0x7F) as u8) << 1) | 0x01,
        ((v >> 7) & 0xFF) as u8,
        (((v & 0x7F) as u8) << 1) | 0x01,
    ]
}

/// Wraps a raw G.711 A-law frame into a PES packet (`00 00 01 C0`) and slices it into 188-byte MPEG-TS packets.
pub fn wrap_alaw_chunk_in_mpegts_pes(
    alaw_chunk: &[u8],
    pts_90khz: u64,
    continuity_counter: &mut u8,
) -> Vec<u8> {
    let pes_payload_len = (3 + 5 + alaw_chunk.len()).min(u16::MAX as usize) as u16;
    let mut pes = Vec::with_capacity(6 + 3 + 5 + alaw_chunk.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xC0]);
    pes.extend_from_slice(&pes_payload_len.to_be_bytes());
    pes.extend_from_slice(&[0x80, 0x80, 0x05]);
    pes.extend_from_slice(&encode_mpegts_pts_33bit(pts_90khz));
    pes.extend_from_slice(alaw_chunk);

    let mut out = Vec::with_capacity(pes.len().div_ceil(184) * 188);
    let mut first = true;
    for slice in pes.chunks(184) {
        let pkt = build_mpegts_packet(TAPO_PES_PID, slice, *continuity_counter, first);
        out.extend_from_slice(&pkt);
        first = false;
        *continuity_counter = (*continuity_counter + 1) & 0x0F;
    }
    out
}

pub fn sha256_hex_upper(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02X}"));
    }
    out
}

async fn read_http_headers_bounded(
    stream: &mut tokio::net::TcpStream,
    timeout: Duration,
) -> std::result::Result<(String, Vec<u8>), (StatusCode, String)> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if buf.len() > 16 * 1024 {
            return Err((
                StatusCode::BAD_GATEWAY,
                "Tapo Streamd HTTP header exceeded 16 KiB".to_string(),
            ));
        }
        let n = match tokio::time::timeout(timeout, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    format!("Tapo Streamd socket read error: {e}"),
                ));
            }
            Err(_) => {
                return Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "Tapo Streamd header read timed out".to_string(),
                ));
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let hdr_str = String::from_utf8_lossy(&buf[..pos]).into_owned();
            let rest = buf[pos + 4..].to_vec();
            return Ok((hdr_str, rest));
        }
    }
    Err((
        StatusCode::BAD_GATEWAY,
        "Tapo Streamd closed connection before completing HTTP headers".to_string(),
    ))
}

/// Streams 8 kHz G.711 A-law (`PCMA`) audio or siren payload to a TP-Link Tapo camera's
/// `Streamd` service on TCP port `8800` (`POST /stream`, Digest auth with `SHA256(cloud_password)`
/// uppercase for `encrypt_type="3"` or `MD5(cloud_password)` uppercase, `talk` session negotiation,
/// and MPEG-TS `audio/mp2t` multipart framing).
pub async fn send_tapo_streamd_alaw_audio(
    host: &str,
    port: u16,
    cloud_password: &str,
    alaw_bytes: &[u8],
    pace_realtime: bool,
    timeout: Duration,
) -> std::result::Result<usize, (StatusCode, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let clean_host = extract_clean_camera_host(host);
    if clean_host.is_empty()
        || clean_host
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid camera host for Tapo Streamd".to_string(),
        ));
    }
    let trimmed_cp = cloud_password.trim();
    if trimmed_cp.is_empty() || trimmed_cp == "***" || trimmed_cp == "*" {
        return Err((
            StatusCode::BAD_REQUEST,
            "Camera requires tapo_cloud_password to stream audio or siren on port 8800".to_string(),
        ));
    }
    if alaw_bytes.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Audio payload is empty".to_string(),
        ));
    }

    let addr = if clean_host.contains(':') && !clean_host.starts_with('[') {
        format!("[{clean_host}]:{port}")
    } else {
        format!("{clean_host}:{port}")
    };

    // Step 1: Unauthenticated POST /stream to obtain WWW-Authenticate Digest challenge
    let mut sock1 = match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("Failed to connect to Tapo Streamd at {addr}: {e}"),
            ));
        }
        Err(_) => {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!("Timed out connecting to Tapo Streamd at {addr}"),
            ));
        }
    };
    let _ = sock1.set_nodelay(true);
    let post1 = format!(
        "POST /stream HTTP/1.1\r\nHost: {addr}\r\nContent-Type: multipart/mixed; boundary=--client-stream-boundary--\r\nContent-Length: 0\r\n\r\n"
    );
    if let Err(e) = tokio::time::timeout(timeout, sock1.write_all(post1.as_bytes())).await {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("Failed to write initial Tapo Streamd challenge request: {e}"),
        ));
    }
    let (challenge_hdrs, _) = read_http_headers_bounded(&mut sock1, timeout).await?;
    drop(sock1);

    let www_auth = challenge_hdrs
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("www-authenticate") {
                Some(v.trim())
            } else {
                None
            }
        })
        .ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "Tapo Streamd did not return a WWW-Authenticate Digest challenge".to_string(),
            )
        })?;

    let after_digest = if www_auth.len() >= 7 && www_auth[..7].eq_ignore_ascii_case("digest ") {
        &www_auth[7..]
    } else {
        www_auth
    };
    let encrypt_type = extract_digest_challenge_param(after_digest, "encrypt_type").unwrap_or("3");
    let hashed_pwd = if encrypt_type == "3" {
        sha256_hex_upper(trimmed_cp.as_bytes())
    } else {
        md5_hex(trimmed_cp.as_bytes()).to_ascii_uppercase()
    };
    let cnonce = format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    );
    let auth_hdr =
        build_http_digest_auth_header(www_auth, "POST", "/stream", "admin", &hashed_pwd, &cnonce)
            .ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "Failed to construct Digest Authorization header for Tapo Streamd".to_string(),
            )
        })?;

    // Step 2: Open authenticated POST /stream connection
    let mut sock2 = match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("Failed to reconnect to Tapo Streamd at {addr}: {e}"),
            ));
        }
        Err(_) => {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!("Timed out reconnecting to Tapo Streamd at {addr}"),
            ));
        }
    };
    let _ = sock2.set_nodelay(true);
    let post2 = format!(
        "POST /stream HTTP/1.1\r\nHost: {addr}\r\nContent-Type: multipart/mixed; boundary=--client-stream-boundary--\r\nAuthorization: {auth_hdr}, algorithm=MD5\r\nContent-Length: 0\r\n\r\n"
    );
    if tokio::time::timeout(timeout, sock2.write_all(post2.as_bytes()))
        .await
        .is_err()
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Failed to send authenticated Tapo Streamd request".to_string(),
        ));
    }
    let (auth_resp_hdrs, mut body_buf) = read_http_headers_bounded(&mut sock2, timeout).await?;
    let status_line = auth_resp_hdrs.lines().next().unwrap_or("");
    if !status_line.contains(" 200") {
        let code = if status_line.contains(" 401") {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::BAD_GATEWAY
        };
        return Err((
            code,
            format!("Tapo Streamd authentication rejected ({status_line}); verify cloud_password"),
        ));
    }

    // Step 3: Request two-way audio talk session ({"talk":{"mode":"aec"},"method":"get"})
    let talk_req_json =
        r#"{"type":"request","seq":3,"params":{"talk":{"mode":"aec"},"method":"get"}}"#;
    let talk_part = format!(
        "----client-stream-boundary--\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}\r\n",
        talk_req_json.len(),
        talk_req_json
    );
    if tokio::time::timeout(timeout, sock2.write_all(talk_part.as_bytes()))
        .await
        .is_err()
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Failed to write Tapo talk session request".to_string(),
        ));
    }

    let mut session_id: Option<String> = None;
    let mut read_tmp = [0u8; 1024];
    while session_id.is_none() {
        if let Some(brace_idx) = body_buf.iter().position(|&b| b == b'{') {
            let json_slice = &body_buf[brace_idx..];
            let mut de = serde_json::Deserializer::from_slice(json_slice).into_iter::<Value>();
            if let Some(Ok(val)) = de.next() {
                let err_code = val["params"]["error_code"].as_i64().unwrap_or(0);
                if err_code != 0 {
                    return Err((
                        StatusCode::BAD_GATEWAY,
                        format!("Tapo camera refused talk session (error_code={err_code})"),
                    ));
                }
                if let Some(sid) = val["params"]["session_id"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| val["params"]["session_id"].as_i64().map(|n| n.to_string()))
                {
                    session_id = Some(sid);
                    break;
                }
            }
        }
        if body_buf.len() > 64 * 1024 {
            return Err((
                StatusCode::BAD_GATEWAY,
                "Tapo talk session response exceeded 64 KiB without session_id".to_string(),
            ));
        }
        match tokio::time::timeout(timeout, sock2.read(&mut read_tmp)).await {
            Ok(Ok(0)) => {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    "Tapo camera closed connection before starting talk session".to_string(),
                ));
            }
            Ok(Ok(n)) => body_buf.extend_from_slice(&read_tmp[..n]),
            Ok(Err(e)) => {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    format!("Tapo talk session read error: {e}"),
                ));
            }
            Err(_) => {
                return Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "Timed out waiting for Tapo talk session_id".to_string(),
                ));
            }
        }
    }
    let sid = session_id.unwrap_or_else(|| "1".to_string());

    // Step 4: Stream MPEG-TS PAT+PMT header and 20ms G.711 A-law PES chunks
    let ts_header = build_tapo_mpegts_audio_header_packets();
    let mut init_part = Vec::with_capacity(128 + ts_header.len());
    init_part.extend_from_slice(TAPO_STREAMD_BOUNDARY);
    init_part.extend_from_slice(
        format!(
            "\r\nContent-Type: audio/mp2t\r\nX-If-Encrypt: 0\r\nX-Session-Id: {sid}\r\nContent-Length: {}\r\n\r\n",
            ts_header.len()
        )
        .as_bytes(),
    );
    init_part.extend_from_slice(&ts_header);
    if tokio::time::timeout(timeout, sock2.write_all(&init_part))
        .await
        .is_err()
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "Failed to write MPEG-TS PAT/PMT header to Tapo Streamd".to_string(),
        ));
    }

    let mut continuity_counter: u8 = 0;
    let mut pts_90khz: u64 = 0;
    let start_instant = tokio::time::Instant::now();
    for (chunk_idx, raw_chunk) in alaw_bytes.chunks(TAPO_AUDIO_CHUNK_BYTES).enumerate() {
        let mut padded = [TAPO_ALAW_SILENCE; TAPO_AUDIO_CHUNK_BYTES];
        padded[..raw_chunk.len()].copy_from_slice(raw_chunk);
        let ts_pes = wrap_alaw_chunk_in_mpegts_pes(&padded, pts_90khz, &mut continuity_counter);
        pts_90khz = pts_90khz
            .saturating_add(((TAPO_AUDIO_CHUNK_BYTES as u64) * 90_000) / (TAPO_AUDIO_RATE as u64));
        let mut part = Vec::with_capacity(128 + ts_pes.len());
        part.extend_from_slice(TAPO_STREAMD_BOUNDARY);
        part.extend_from_slice(
            format!(
                "\r\nContent-Type: audio/mp2t\r\nX-If-Encrypt: 0\r\nX-Session-Id: {sid}\r\nContent-Length: {}\r\n\r\n",
                ts_pes.len()
            )
            .as_bytes(),
        );
        part.extend_from_slice(&ts_pes);
        if tokio::time::timeout(timeout, sock2.write_all(&part))
            .await
            .is_err()
        {
            return Err((
                StatusCode::BAD_GATEWAY,
                "Tapo camera closed speaker stream during audio transmission".to_string(),
            ));
        }
        if pace_realtime {
            let target_elapsed = Duration::from_millis(((chunk_idx + 1) as u64) * 20);
            let now_elapsed = start_instant.elapsed();
            if target_elapsed > now_elapsed {
                tokio::time::sleep(target_elapsed - now_elapsed).await;
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), sock2.flush()).await;
    if pace_realtime {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(alaw_bytes.len())
}

/// Unified protocol-driven audio talkback and siren dispatcher.
/// Routes 8 kHz G.711 A-law (`PCMA`) payloads to either:
/// - **TP-Link Tapo (`tapo`)**: TCP port `8800` `Streamd` (`POST /stream` + MPEG-TS `audio/mp2t`) using `cloud_password`
/// - **Intelbras / Dahua (`dahua_cgi`)**: HTTP Digest `POST /cgi-bin/audio.cgi?action=postAudio&httptype=singlepart&channel=1`
// Protocol-agnostic audio backchannel dispatch accepting resolved connection and payload parameters
#[expect(clippy::too_many_arguments)]
pub async fn send_camera_talkback_or_siren_alaw(
    client: &reqwest::Client,
    runtime: &CameraDriverRuntime,
    camera: &str,
    (host, onvif_port): (&str, u16),
    (user, pass): (Option<&str>, Option<&str>),
    cloud_password: Option<&str>,
    is_tapo_protocol: bool,
    alaw_payload: Vec<u8>,
    pace_realtime: bool,
    timeout: Duration,
) -> std::result::Result<usize, (StatusCode, String)> {
    if is_tapo_protocol {
        let cp = cloud_password.unwrap_or("");
        let streamd_port = runtime.get_tapo_streamd_port(camera);
        return send_tapo_streamd_alaw_audio(
            host,
            streamd_port,
            cp,
            &alaw_payload,
            pace_realtime,
            timeout,
        )
        .await;
    }

    let port = if onvif_port != 2020 { onvif_port } else { 80 };
    let cgi_base = format_camera_http_base(host, port);
    let talk_url =
        format!("{cgi_base}/cgi-bin/audio.cgi?action=postAudio&httptype=singlepart&channel=1");
    let total_len = alaw_payload.len();
    match send_http_digest_request(
        client,
        runtime,
        reqwest::Method::POST,
        &talk_url,
        (user, pass),
        (Some("Audio/G.711A"), Some(alaw_payload)),
        timeout,
    )
    .await
    {
        Ok((st, _)) if st.is_success() => Ok(total_len),
        Ok((st, txt)) => {
            let scrubbed_snippet: String = scrub_rtsp_credentials(txt.trim())
                .chars()
                .take(256)
                .collect();
            Err((
                StatusCode::BAD_GATEWAY,
                format!(
                    "Camera talkback returned HTTP {} ({scrubbed_snippet})",
                    st.as_u16()
                ),
            ))
        }
        Err((st, err_msg)) => Err((st, err_msg)),
    }
}

pub fn build_onvif_get_system_log_soap(
    log_type: &str,
    user: Option<&str>,
    password: Option<&str>,
) -> String {
    let safe_type = if log_type.eq_ignore_ascii_case("access") {
        "Access"
    } else {
        "System"
    };
    let body = format!(
        "<tds:GetSystemLog xmlns:tds=\"http://www.onvif.org/ver10/device/wsdl\"><tds:LogType>{safe_type}</tds:LogType></tds:GetSystemLog>"
    );
    build_onvif_soap_envelope(&body, user, password)
}

pub fn parse_onvif_system_log_xml(xml: &str) -> Vec<String> {
    let Some(raw) = extract_xml_element_text(xml, "String") else {
        return Vec::new();
    };
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(200)
        .map(|l| scrub_rtsp_credentials(l).chars().take(512).collect())
        .collect()
}

pub fn build_onvif_create_pull_point_soap(user: Option<&str>, password: Option<&str>) -> String {
    build_onvif_soap_envelope(
        "<tev:CreatePullPointSubscription xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\"><tev:InitialTerminationTime>PT30S</tev:InitialTerminationTime></tev:CreatePullPointSubscription>",
        user,
        password,
    )
}

pub fn build_onvif_pull_messages_soap(user: Option<&str>, password: Option<&str>) -> String {
    build_onvif_soap_envelope(
        "<tev:PullMessages xmlns:tev=\"http://www.onvif.org/ver10/events/wsdl\"><tev:Timeout>PT1S</tev:Timeout><tev:MessageLimit>16</tev:MessageLimit></tev:PullMessages>",
        user,
        password,
    )
}

pub fn build_onvif_unsubscribe_soap(user: Option<&str>, password: Option<&str>) -> String {
    build_onvif_soap_envelope(
        "<wsnt:Unsubscribe xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\"/>",
        user,
        password,
    )
}

pub fn parse_onvif_pull_messages_xml(xml: &str) -> (Option<String>, Vec<String>) {
    let cur_time = extract_xml_element_text(xml, "CurrentTime")
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains('<'))
        .map(str::to_string);
    let mut events = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = xml[cursor..].find("NotificationMessage") {
        let start = cursor + rel;
        let Some((close_rel, close_end_rel)) =
            find_closing_xml_tag_offset(&xml[start..], "NotificationMessage")
        else {
            break;
        };
        let block = &xml[start..start + close_rel];
        let topic = extract_xml_element_text(block, "Topic").map_or("Event", str::trim);
        let mut kv_parts = Vec::new();
        let mut sc = 0usize;
        while let Some(s_rel) = block[sc..].find("SimpleItem") {
            let s_idx = sc + s_rel;
            let Some(gt) = block[s_idx..].find('>') else {
                break;
            };
            let tag = &block[s_idx..s_idx + gt];
            if let (Some(n), Some(v)) = (
                extract_xml_attr(tag, "Name"),
                extract_xml_attr(tag, "Value"),
            ) {
                kv_parts.push(format!("{n}={v}"));
            }
            sc = s_idx + gt + 1;
        }
        let summary = if kv_parts.is_empty() {
            format!("ONVIF Event: {topic}")
        } else {
            format!("ONVIF Event: {topic} ({})", kv_parts.join(", "))
        };
        events.push(summary);
        if events.len() >= 32 {
            break;
        }
        cursor = (start + close_end_rel).min(xml.len());
    }
    (cur_time, events)
}

#[derive(Debug, Clone, Serialize)]
pub struct CameraDiagnosticReport {
    pub camera: String,
    pub online: bool,
    pub protocol_family: String,
    pub sources_queried: Vec<String>,
    pub lines: Vec<String>,
}

/// Queries supported camera log and event diagnostic channels (`ONVIF GetDeviceInformation`,
/// `ONVIF GetSystemLog`, `ONVIF PullPointSubscription`, and `Dahua CGI` diagnostics/syslog status)
/// according to the camera's `CameraProtocolSpec`.
pub async fn collect_camera_diagnostic_logs(
    client: &reqwest::Client,
    runtime: &CameraDriverRuntime,
    camera: &str,
    (host, onvif_port): (&str, u16),
    (user, pass): (Option<&str>, Option<&str>),
    spec: &CameraProtocolSpec,
    timeout: Duration,
) -> CameraDiagnosticReport {
    let mut lines: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    let mut online = false;

    let onvif_base = format_camera_http_base(host, onvif_port);
    let dev_ep = format!("{onvif_base}/onvif/device_service");

    // 1. ONVIF Device Identity
    let dev_soap = build_onvif_get_device_info_soap(user, pass);
    if let Ok(xml) = send_onvif_soap_request_with_ns(
        client,
        &dev_ep,
        (
            "http://www.onvif.org/ver10/device/wsdl",
            "GetDeviceInformation",
        ),
        dev_soap,
        (user, pass),
        timeout,
    )
    .await
    {
        online = true;
        sources.push("onvif:GetDeviceInformation".to_string());
        let info = parse_onvif_device_info_xml(&xml);
        lines.push(format!(
            "[onvif/device] Manufacturer={} Model={} Firmware={} HardwareId={}",
            info.manufacturer.as_deref().unwrap_or("unknown"),
            info.model.as_deref().unwrap_or("unknown"),
            info.firmware.as_deref().unwrap_or("unknown"),
            info.hardware_id.as_deref().unwrap_or("unknown"),
        ));
    }

    // 2. ONVIF GetSystemLog (for protocols that support it: Hikvision ISAPI, Axis, Reolink, Standard ONVIF)
    if spec.supports_onvif_system_log {
        let log_soap = build_onvif_get_system_log_soap("System", user, pass);
        if let Ok(xml) = send_onvif_soap_request_with_ns(
            client,
            &dev_ep,
            ("http://www.onvif.org/ver10/device/wsdl", "GetSystemLog"),
            log_soap,
            (user, pass),
            timeout,
        )
        .await
        {
            online = true;
            sources.push("onvif:GetSystemLog".to_string());
            for l in parse_onvif_system_log_xml(&xml) {
                lines.push(format!("[onvif/syslog] {l}"));
            }
        }
    }

    // 3. ONVIF PullPoint Event Subscription (Tapo C200 :2020/onvif/service and ONVIF Profile S/T)
    if spec.supports_onvif_pull_events {
        let ev_ep = if spec.id == "tapo" {
            format!("{onvif_base}/onvif/service")
        } else {
            format!("{onvif_base}/onvif/event_service")
        };
        let sub_soap = build_onvif_create_pull_point_soap(user, pass);
        if let Ok(sub_xml) = send_onvif_soap_request_with_ns(
            client,
            &ev_ep,
            (
                "http://www.onvif.org/ver10/events/wsdl/EventPortType",
                "CreatePullPointSubscriptionRequest",
            ),
            sub_soap,
            (user, pass),
            timeout,
        )
        .await
        {
            online = true;
            sources.push("onvif:PullPointEvents".to_string());
            if let Some(sub_addr) = extract_xml_element_text(&sub_xml, "Address")
                .map(str::trim)
                .filter(|u| is_safe_onvif_endpoint_url(u))
            {
                let pull_soap = build_onvif_pull_messages_soap(user, pass);
                if let Ok(pull_xml) = send_onvif_soap_request_with_ns(
                    client,
                    sub_addr,
                    (
                        "http://www.onvif.org/ver10/events/wsdl/PullPointSubscription",
                        "PullMessagesRequest",
                    ),
                    pull_soap,
                    (user, pass),
                    timeout,
                )
                .await
                {
                    let (cam_clock, ev_list) = parse_onvif_pull_messages_xml(&pull_xml);
                    if let Some(clk) = cam_clock {
                        lines.push(format!(
                            "[onvif/pullpoint] Subscription active (camera clock={clk}, pending_events={})",
                            ev_list.len()
                        ));
                    }
                    for ev in ev_list {
                        lines.push(format!("[onvif/event] {ev}"));
                    }
                }
                // Always Unsubscribe immediately so no subscription slot is leaked on the camera
                let unsub_soap = build_onvif_unsubscribe_soap(user, pass);
                let _ = send_onvif_soap_request_with_ns(
                    client,
                    sub_addr,
                    (
                        "http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager",
                        "UnsubscribeRequest",
                    ),
                    unsub_soap,
                    (user, pass),
                    Duration::from_secs(2),
                )
                .await;
            }
        }
    }

    // 4. Dahua / Intelbras HTTP Digest CGI diagnostics & event/syslog state
    if spec.supports_dahua_cgi {
        let cgi_port = if onvif_port != 2020 { onvif_port } else { 80 };
        let cgi_base = format_camera_http_base(host, cgi_port);
        for (label, path) in [
            ("cgi/system", "/cgi-bin/magicBox.cgi?action=getSystemInfo"),
            (
                "cgi/version",
                "/cgi-bin/magicBox.cgi?action=getSoftwareVersion",
            ),
            ("cgi/clock", "/cgi-bin/global.cgi?action=getCurrentTime"),
            (
                "cgi/syslog",
                "/cgi-bin/configManager.cgi?action=getConfig&name=Syslog",
            ),
            (
                "cgi/automaintain",
                "/cgi-bin/configManager.cgi?action=getConfig&name=AutoMaintain",
            ),
            (
                "cgi/events",
                "/cgi-bin/eventManager.cgi?action=getEventIndexes&code=VideoMotion",
            ),
        ] {
            let url = format!("{cgi_base}{path}");
            if let Ok((StatusCode::OK, txt)) = send_http_digest_request(
                client,
                runtime,
                reqwest::Method::GET,
                &url,
                (user, pass),
                (None, None),
                timeout,
            )
            .await
            {
                online = true;
                if !sources.contains(&"dahua_cgi".to_string()) {
                    sources.push("dahua_cgi".to_string());
                }
                let compact: Vec<String> = txt
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("Error"))
                    .map(scrub_rtsp_credentials)
                    .collect();
                if !compact.is_empty() {
                    lines.push(format!("[{label}] {}", compact.join(" | ")));
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(format!(
            "[diagnostic] Camera '{camera}' ({}) did not return any log or event lines (online={online})",
            spec.id
        ));
    }

    CameraDiagnosticReport {
        camera: camera.to_string(),
        online,
        protocol_family: spec.id.to_string(),
        sources_queried: sources,
        lines,
    }
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

        // Test ONVIF GetSystemLog & PullPoint XML parsers and Dahua CGI Syslog/AutoMaintain encoders
        let syslog_xml = r#"<s:Envelope><s:Body><tds:GetSystemLogResponse><tds:SystemLog><tt:String>2026-11-08 00:01:02 [rtsp] client connected rtsp://admin:secret@192.0.2.10:554/stream1
2026-11-08 00:02:00 [ptz] move completed</tt:String></tds:SystemLog></tds:GetSystemLogResponse></s:Body></s:Envelope>"#;
        let parsed_logs = parse_onvif_system_log_xml(syslog_xml);
        assert_eq!(parsed_logs.len(), 2);
        assert!(parsed_logs[0].contains("rtsp://*:*@192.0.2.10:554/stream1"));

        let pull_xml = r#"<SOAP-ENV:Envelope><SOAP-ENV:Body><tev:PullMessagesResponse><tev:CurrentTime>2026-11-08T03:12:10Z</tev:CurrentTime><wsnt:NotificationMessage><wsnt:Topic>tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic><wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage></tev:PullMessagesResponse></SOAP-ENV:Body></SOAP-ENV:Envelope>"#;
        let (clk, evs) = parse_onvif_pull_messages_xml(pull_xml);
        assert_eq!(clk.as_deref(), Some("2026-11-08T03:12:10Z"));
        assert_eq!(
            evs,
            vec!["ONVIF Event: tns1:RuleEngine/CellMotionDetector/Motion (IsMotion=true)"]
        );

        let mut st_map = Map::new();
        let syslog_kv = parse_dahua_cgi_kv(
            "table.Syslog.Enable=true\r\ntable.Syslog.Server=192.0.2.1\r\ntable.Syslog.Port=514\r\n",
        );
        apply_dahua_cgi_table_to_state("Syslog", &syslog_kv, &mut st_map);
        assert_eq!(st_map["syslog_enabled"], json!(true));
        assert_eq!(st_map["syslog_server"], json!("192.0.2.1"));
        assert_eq!(st_map["syslog_port"], json!(514));

        let pairs = build_dahua_cgi_set_config_pairs(&CameraHardwareUpdateBody {
            syslog_enabled: Some(true),
            syslog_server: Some("192.0.2.99".to_string()),
            syslog_port: Some(5514),
            auto_reboot_enabled: Some(true),
            auto_reboot_day: Some(2),
            auto_reboot_hour: Some(4),
            auto_reboot_minute: Some(30),
            ..Default::default()
        });
        assert!(pairs.contains(&"Syslog.Enable=true".to_string()));
        assert!(pairs.contains(&"Syslog.Server=192.0.2.99".to_string()));
        assert!(pairs.contains(&"Syslog.Port=5514".to_string()));
        assert!(pairs.contains(&"AutoMaintain.AutoRebootEnable=true".to_string()));
        assert!(pairs.contains(&"AutoMaintain.AutoRebootHour=4".to_string()));
    }

    #[tokio::test]
    async fn tapo_streamd_8800_siren_and_mpegts_audio_talkback_handshake_works() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let siren_bytes = generate_siren_alaw_bytes(0.4);
        assert_eq!(siren_bytes.len(), 3200);
        let ts_hdr = build_tapo_mpegts_audio_header_packets();
        assert_eq!(ts_hdr.len(), 376);
        assert_eq!(ts_hdr[0], 0x47);
        assert_eq!(ts_hdr[188], 0x47);

        let mut cc = 0u8;
        let ts_pes = wrap_alaw_chunk_in_mpegts_pes(&siren_bytes[..160], 0, &mut cc);
        assert_eq!(ts_pes.len() % 188, 0);
        assert_eq!(ts_pes[0], 0x47);

        let rt = CameraDriverRuntime::new();
        assert!(rt.try_claim_siren_trigger("escritorio", 100.0, 15.0));
        assert!(
            !rt.try_claim_siren_trigger("escritorio", 105.0, 15.0),
            "cooldown must suppress duplicate siren trigger within 15s"
        );
        assert!(rt.try_claim_siren_trigger("escritorio", 116.0, 15.0));

        // Spawn mock Tapo Streamd server (:8800 equivalent) verifying Digest auth + talk session + audio/mp2t
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received_ts_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rec_clone = received_ts_bytes.clone();

        let srv = tokio::spawn(async move {
            // Connection 1: unauthenticated -> return 401 Digest challenge with encrypt_type="3"
            let (mut s1, _) = listener.accept().await.unwrap();
            let mut b1 = [0u8; 1024];
            let _ = s1.read(&mut b1).await.unwrap();
            let resp_401 = "HTTP/1.0 401 Unauthorized\r\nServer: Streamd\r\nWWW-Authenticate: Digest realm=\"TP-Link IP-Camera\",algorithm=\"MD5\",encrypt_type=\"3\",qop=\"auth\",nonce=\"deadbeef12345678\",opaque=\"6494321465\"\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
            s1.write_all(resp_401.as_bytes()).await.unwrap();
            drop(s1);

            // Connection 2: authenticated -> verify Digest Authorization -> 200 OK -> talk session -> audio/mp2t
            let (mut s2, _) = listener.accept().await.unwrap();
            let mut b2 = vec![0u8; 4096];
            let n2 = s2.read(&mut b2).await.unwrap();
            let req2_str = String::from_utf8_lossy(&b2[..n2]);
            assert!(req2_str.contains("Authorization: Digest username=\"admin\""));
            let resp_200 = "HTTP/1.0 200 OK\r\nServer: Streamd\r\nContent-Type: multipart/mixed;boundary=--device-stream-boundary--\r\nKey-Exchange: cipher=\"AES_128_CBC\" username=\"admin\" padding=\"PKCS7_16\" algorithm=\"MD5\" encrypt_type=\"3\" nonce=\"11223344\"\r\nConnection: keep-alive\r\n\r\n";
            s2.write_all(resp_200.as_bytes()).await.unwrap();

            // Read talk request part if not already in b2
            let mut stream_buf = b2[..n2].to_vec();
            while !String::from_utf8_lossy(&stream_buf).contains("\"talk\"") {
                let mut tmp = [0u8; 1024];
                let m = s2.read(&mut tmp).await.unwrap();
                if m == 0 {
                    break;
                }
                stream_buf.extend_from_slice(&tmp[..m]);
            }
            let talk_reply_json =
                r#"{"type":"response", "seq":3, "params":{"error_code":0, "session_id":"42"}}"#;
            let talk_reply_part = format!(
                "----device-stream-boundary--\r\nContent-Type: application/json\r\nX-If-Encrypt: 0\r\nContent-Length: {}\r\n\r\n{}\r\n",
                talk_reply_json.len(),
                talk_reply_json
            );
            s2.write_all(talk_reply_part.as_bytes()).await.unwrap();

            // Read incoming audio/mp2t parts
            let mut total_read = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match s2.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(k) => total_read.extend_from_slice(&tmp[..k]),
                }
            }
            let body_str = String::from_utf8_lossy(&total_read);
            if body_str.contains("Content-Type: audio/mp2t")
                && body_str.contains("X-Session-Id: 42")
            {
                rec_clone.store(total_read.len(), std::sync::atomic::Ordering::SeqCst);
            }
        });

        let sent = send_tapo_streamd_alaw_audio(
            "127.0.0.1",
            port,
            "tp_cloud_secret_pw",
            &siren_bytes[..320],
            false,
            Duration::from_secs(5),
        )
        .await
        .expect("Tapo Streamd audio transmission should succeed");
        assert_eq!(sent, 320);
        let _ = tokio::time::timeout(Duration::from_secs(2), srv).await;
        assert!(
            received_ts_bytes.load(std::sync::atomic::Ordering::SeqCst) > 376,
            "mock Tapo Streamd server must receive PAT+PMT+PES audio/mp2t bytes"
        );
    }

    #[test]
    fn resolve_probe_target_consolidates_tapo_and_dahua_defaults_and_rtsp_inference() {
        let rt = CameraDriverRuntime::new();

        // 1. Tapo camera inferred from RTSP /stream1 URL
        let mut tapo_cfg = ninjanvr_config::CameraConfig::default();
        tapo_cfg.hardware.cloud_password = Some("tapo_cloud_pw_123".to_string());
        tapo_cfg
            .ffmpeg
            .inputs
            .push(ninjanvr_config::CameraFfmpegInput {
                path: "rtsp://tapo_admin:secret%401@192.0.2.44:554/stream1#transport=tcp"
                    .to_string(),
                roles: vec!["record".to_string()],
                input_args: serde_json::Value::Null,
            });

        let tapo_target = rt.resolve_probe_target("escritorio", &tapo_cfg);
        assert_eq!(tapo_target.host.as_deref(), Some("192.0.2.44"));
        assert_eq!(tapo_target.port, 2020);
        assert_eq!(tapo_target.onvif_port, 2020);
        assert_eq!(tapo_target.user.as_deref(), Some("tapo_admin"));
        assert_eq!(tapo_target.password.as_deref(), Some("secret@1"));
        assert_eq!(
            tapo_target.cloud_password.as_deref(),
            Some("tapo_cloud_pw_123")
        );
        assert_eq!(tapo_target.protocol, "tapo");
        assert_eq!(tapo_target.profile_token, "profile_1");
        assert_eq!(tapo_target.video_source_token, "raw_vs1");
        assert_eq!(tapo_target.streamd_port, 8800);
        assert!(tapo_target.is_tapo_hint);
        assert!(!tapo_target.is_dahua_hint);

        // 2. Dahua / Intelbras camera inferred from RTSP /cam/realmonitor URL
        let mut dahua_cfg = ninjanvr_config::CameraConfig::default();
        dahua_cfg.ffmpeg.inputs.push(ninjanvr_config::CameraFfmpegInput {
            path:
                "rtsp://admin:intelbras_pw@192.0.2.77:554/cam/realmonitor?channel=1&subtype=0&unicast=true"
                    .to_string(),
            roles: vec!["record".to_string()],
            input_args: serde_json::Value::Null,
        });

        let dahua_target = rt.resolve_probe_target("sala", &dahua_cfg);
        assert_eq!(dahua_target.host.as_deref(), Some("192.0.2.77"));
        assert_eq!(dahua_target.port, 80);
        assert_eq!(dahua_target.onvif_port, 80);
        assert_eq!(dahua_target.user.as_deref(), Some("admin"));
        assert_eq!(dahua_target.password.as_deref(), Some("intelbras_pw"));
        assert_eq!(dahua_target.protocol, "dahua_cgi");
        assert_eq!(dahua_target.profile_token, "Profile000");
        assert_eq!(dahua_target.video_source_token, "VideoSource000");
        assert!(!dahua_target.is_tapo_hint);
        assert!(dahua_target.is_dahua_hint);
    }

    #[tokio::test]
    async fn execute_onvif_with_profile_discovery_retries_noprofile_and_caches_working_token() {
        let rt = CameraDriverRuntime::new();
        let camera = "sala";
        let endpoint = "http://192.0.2.77:80/onvif/ptz_service";
        let attempted = Arc::new(Mutex::new(Vec::<String>::new()));
        let attempted_clone = attempted.clone();

        let (status, body, resolved_prof) = rt
            .execute_onvif_with_profile_discovery(
                camera,
                endpoint,
                "Profile_1",
                Some("dahua_cgi"),
                move |prof| {
                    let attempted_inner = attempted_clone.clone();
                    async move {
                        attempted_inner.lock().unwrap().push(prof.clone());
                        if prof == "Profile_1" {
                            Ok((
                                400,
                                "<s:Envelope><s:Body><s:Fault><s:Code><s:Subcode><s:Value>ter:NoProfile</s:Value></s:Subcode></s:Code><s:Reason><s:Text>The requested profile token Profile_1 does not exist</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>".to_string(),
                            ))
                        } else if prof == "Profile000" {
                            Ok((
                                200,
                                "<s:Envelope><s:Body><tptz:GetStatusResponse/></s:Body></s:Envelope>"
                                    .to_string(),
                            ))
                        } else {
                            Err("unexpected profile token".to_string())
                        }
                    }
                },
            )
            .await
            .expect("profile discovery should succeed on Profile000");

        assert_eq!(status, 200);
        assert!(body.contains("GetStatusResponse"));
        assert_eq!(resolved_prof, "Profile000");
        assert_eq!(
            *attempted.lock().unwrap(),
            vec!["Profile_1".to_string(), "Profile000".to_string()]
        );
        assert_eq!(
            rt.get_profile(camera, endpoint).as_deref(),
            Some("Profile000"),
            "working profile token must be cached in CameraDriverRuntime"
        );
    }

    #[test]
    fn compute_soft_preset_relative_delta_enforces_deadband_and_clamping() {
        // Within deadband on all axes -> returns None
        assert_eq!(
            compute_soft_preset_relative_delta(0.25, -0.40, 0.10, 0.254, -0.395, 0.108, 0.01),
            None
        );
        // Exact boundary equal to min_step -> returns None
        assert_eq!(
            compute_soft_preset_relative_delta(0.0, 0.0, 0.0, 0.01, -0.01, 0.01, 0.01),
            None
        );
        // Exceeding min_step -> returns clamped delta tuple
        let (dx, dy, dz) =
            compute_soft_preset_relative_delta(0.10, -0.20, 0.0, 0.60, 0.30, 0.50, 0.01)
                .expect("delta exceeding min_step must return Some");
        assert!((dx - 0.50).abs() < 1e-9);
        assert!((dy - 0.50).abs() < 1e-9);
        assert!((dz - 0.50).abs() < 1e-9);

        // Extreme delta clamps to [-1.0, 1.0]
        let (cx, cy, cz) =
            compute_soft_preset_relative_delta(-0.90, 0.85, 0.90, 0.95, -0.95, -0.50, 0.01)
                .expect("large delta must clamp to [-1.0, 1.0]");
        assert_eq!(cx, 1.0);
        assert_eq!(cy, -1.0);
        assert!((cz - (-1.0)).abs() < 0.5);

        // Non-finite input returns None
        assert_eq!(
            compute_soft_preset_relative_delta(f64::NAN, 0.0, 0.0, 0.5, 0.5, 0.0, 0.01),
            None
        );
    }

    #[tokio::test]
    async fn reboot_camera_and_trigger_camera_siren_dispatch_protocol_seams() {
        use axum::{Router, routing::get, routing::post};

        let mock_app = Router::new()
            .route(
                "/onvif/device_service",
                post(|| async { (StatusCode::BAD_REQUEST, "SOAP fault") }),
            )
            .route(
                "/onvif/service",
                post(|| async { (StatusCode::BAD_REQUEST, "SOAP fault") }),
            )
            .route(
                "/cgi-bin/magicBox.cgi",
                get(|| async { (StatusCode::OK, "OK\r\n") }),
            )
            .route(
                "/cgi-bin/audio.cgi",
                post(|| async { (StatusCode::OK, "OK\r\n") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, mock_app).await;
        });

        let rt = CameraDriverRuntime::new();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();

        let mut cam_cfg = ninjanvr_config::CameraConfig::default();
        cam_cfg.onvif.host = Some("127.0.0.1".to_string());
        cam_cfg.onvif.port = addr.port();
        cam_cfg.onvif.user = Some("admin".to_string());
        cam_cfg.onvif.password = Some("secret".to_string());
        cam_cfg.hardware.protocol = Some("dahua_cgi".to_string());

        let reboot_res = rt
            .reboot_camera(&client, "sala", &cam_cfg)
            .await
            .expect("reboot_camera should fall back to Dahua CGI reboot");
        assert_eq!(reboot_res, "reboot:cgi");

        let siren_res = rt
            .trigger_camera_siren(&client, "sala", &cam_cfg, 0.2)
            .await
            .expect("trigger_camera_siren should succeed over Dahua CGI postAudio");
        assert_eq!(siren_res, "dahua_cgi");

        srv.abort();
    }

    struct FakeCameraWireTransport {
        soap_calls: Mutex<Vec<(String, String, String)>>,
        http_calls: Mutex<Vec<String>>,
    }

    impl FakeCameraWireTransport {
        fn new() -> Self {
            Self {
                soap_calls: Mutex::new(Vec::new()),
                http_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl CameraWireTransport for FakeCameraWireTransport {
        fn send_soap<'a>(
            &'a self,
            endpoint: &'a str,
            _wsdl_ns: &'a str,
            action: &'a str,
            soap_xml: String,
            _credentials: (Option<&'a str>, Option<&'a str>),
            _timeout: Duration,
        ) -> WireSoapFuture<'a> {
            self.soap_calls.lock().unwrap().push((
                endpoint.to_string(),
                action.to_string(),
                soap_xml.clone(),
            ));
            let ep = endpoint.to_string();
            let act = action.to_string();
            Box::pin(async move {
                if act == "GetStatus" {
                    if soap_xml.contains("ProfileToken>profile_1<") {
                        Ok(r#"<s:Envelope><s:Body><tptz:GetStatusResponse><tptz:PTZStatus><tt:Position><tt:PanTilt x="0.35" y="-0.15"/><tt:Zoom x="0.50"/></tt:Position><tt:MoveStatus><tt:PanTilt>IDLE</tt:PanTilt></tt:MoveStatus></tptz:PTZStatus></tptz:GetStatusResponse></s:Body></s:Envelope>"#.to_string())
                    } else {
                        Err((
                            StatusCode::BAD_REQUEST,
                            "ONVIF SOAP Fault: ter:NoProfile".to_string(),
                        ))
                    }
                } else if act == "GetProfiles" && ep.ends_with("/onvif/service") {
                    Ok(r#"<s:Envelope><s:Body><trt:GetProfilesResponse><trt:Profiles token="profile_1"><tt:VideoSourceConfiguration><tt:SourceToken>raw_vs1</tt:SourceToken></tt:VideoSourceConfiguration></trt:Profiles></trt:GetProfilesResponse></s:Body></s:Envelope>"#.to_string())
                } else if act == "SystemReboot" && ep.ends_with("/onvif/service") {
                    Ok(
                        "<s:Envelope><s:Body><tds:SystemRebootResponse/></s:Body></s:Envelope>"
                            .to_string(),
                    )
                } else {
                    Err((
                        StatusCode::BAD_REQUEST,
                        "ONVIF SOAP Fault: ActionNotSupported".to_string(),
                    ))
                }
            })
        }

        fn send_digest_http<'a>(
            &'a self,
            _runtime: &'a CameraDriverRuntime,
            _method: reqwest::Method,
            url: &'a str,
            _credentials: (Option<&'a str>, Option<&'a str>),
            _body: (Option<&'a str>, Option<Vec<u8>>),
            _timeout: Duration,
        ) -> WireHttpFuture<'a> {
            self.http_calls.lock().unwrap().push(url.to_string());
            Box::pin(async move { Ok((StatusCode::OK, "OK\r\n".to_string())) })
        }
    }

    #[tokio::test]
    async fn fake_wire_transport_exercises_profile_discovery_media_tokens_reboot_and_presets() {
        let rt = CameraDriverRuntime::new();
        let fake_transport = FakeCameraWireTransport::new();
        let endpoint = "http://192.0.2.44:2020/onvif/service";

        // 1. Tapo protocol prefers profile_1 when initial Profile_1 fails with NoProfile
        let (resolved_prof, status, _raw) = rt
            .fetch_ptz_status_with_discovery(
                &fake_transport,
                "escritorio",
                endpoint,
                "Profile_1",
                true,
                Some("tapo"),
                (Some("admin"), Some("secret")),
                Duration::from_secs(2),
            )
            .await
            .expect("status discovery over FakeCameraWireTransport must succeed");
        assert_eq!(resolved_prof, "profile_1");
        assert!((status.pan - 0.35).abs() < 1e-4);
        assert!((status.tilt - (-0.15)).abs() < 1e-4);
        assert!((status.zoom - 0.50).abs() < 1e-4);
        assert_eq!(
            rt.get_profile("escritorio", endpoint).as_deref(),
            Some("profile_1")
        );

        // 2. Discover ONVIF media tokens falls back from /onvif/media_service to /onvif/service
        let (profs, vs) = rt
            .discover_onvif_media_tokens_with_transport(
                &fake_transport,
                "http://192.0.2.44:2020",
                (Some("admin"), Some("secret")),
                Duration::from_secs(2),
            )
            .await;
        assert_eq!(profs, vec!["profile_1".to_string()]);
        assert_eq!(vs, vec!["raw_vs1".to_string()]);

        // 3. Reboot probes both /onvif/device_service and /onvif/service
        let reboot_tag = rt
            .reboot_camera_at_endpoints(
                &fake_transport,
                "http://192.0.2.44:2020",
                "http://192.0.2.44:80",
                false,
                (Some("admin"), Some("secret")),
                Duration::from_secs(2),
            )
            .await
            .expect("reboot should succeed on /onvif/service");
        assert_eq!(reboot_tag, "reboot:onvif");

        // 4. Active preset tracking inside CameraDriverRuntime
        assert_eq!(
            rt.get_active_preset("escritorio", Some("home"))
                .await
                .as_deref(),
            Some("home")
        );
        rt.set_active_preset("escritorio", Some("desk_view".to_string()))
            .await;
        assert_eq!(
            rt.get_active_preset("escritorio", Some("home"))
                .await
                .as_deref(),
            Some("desk_view")
        );
        rt.clear_active_preset_if_matching("escritorio", &["other", "DESK_VIEW"])
            .await;
        assert_eq!(rt.get_active_preset("escritorio", None).await, None);
    }
}
