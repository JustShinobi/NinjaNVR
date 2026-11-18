#![forbid(unsafe_code)]
//! Typed host/process resource sampling (`ProcCpuTracker`, `HostMetricsProbe`), camera-scoped
//! stats projection, and unit-preserving Prometheus exposition (`TelemetryEngine`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ninjanvr_media::probe_storage_writable;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Default port for the dedicated unauthenticated OpenMetrics (`/metrics`) listener (`0` disables it).
pub const DEFAULT_METRICS_PORT: u16 = 9091;

type PidTickEntry = (u64, Instant, f64);

/// Typed resource sample for an individual OS process, preserving both normalized percentage
/// (`0.0..=100.0`) and exact resident bytes (`rss_bytes`) so Prometheus gauges never mistake
/// a percentage string for MiB.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProcessResourceSample {
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub rss_bytes: u64,
}

/// Raw OS process counters read from `/proc/{pid}/stat` and `/sys/fs/cgroup/memory.max`
/// (or injected deterministically in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawPidProcReading {
    pub total_ticks: u64,
    pub rss_bytes: u64,
    pub mem_limit_bytes: u64,
}

/// Port abstracting `/proc` and cgroup process reads so [`ProcCpuTracker`] is 100% testable
/// without relying on live Linux container load.
pub trait HostMetricsProbe: Send + Sync {
    fn read_pid_proc(&self, pid: u32) -> Option<RawPidProcReading>;
    fn probe_media_writable(&self, media_dir: &Path) -> Result<(), String>;
}

/// Production Linux `/proc` + `/sys/fs/cgroup` + filesystem writability adapter.
#[derive(Default)]
pub struct LinuxHostMetricsProbe;

impl HostMetricsProbe for LinuxHostMetricsProbe {
    fn read_pid_proc(&self, pid: u32) -> Option<RawPidProcReading> {
        if pid == 0 {
            return None;
        }
        let stat_str = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rparen = stat_str.rfind(')')?;
        let fields: Vec<&str> = stat_str[rparen + 1..].split_whitespace().collect();
        if fields.len() < 22 {
            return None;
        }
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        let rss_pages: u64 = fields[21].parse().unwrap_or(0);
        let total_ticks = utime.saturating_add(stime);
        let rss_bytes = rss_pages.saturating_mul(4096);
        let mem_limit_bytes = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(805_306_368)
            .max(67_108_864);
        Some(RawPidProcReading {
            total_ticks,
            rss_bytes,
            mem_limit_bytes,
        })
    }

    fn probe_media_writable(&self, media_dir: &Path) -> Result<(), String> {
        probe_storage_writable(media_dir)
    }
}

#[derive(Clone)]
pub struct ProcCpuTracker {
    prev_ticks: Arc<Mutex<HashMap<u32, PidTickEntry>>>,
    prev_infer: Arc<Mutex<Option<PidTickEntry>>>,
    last_storage_probe_ok: Arc<Mutex<Option<(PathBuf, Instant)>>>,
    probe: Arc<dyn HostMetricsProbe>,
}

impl Default for ProcCpuTracker {
    fn default() -> Self {
        Self::with_probe(Arc::new(LinuxHostMetricsProbe))
    }
}

impl ProcCpuTracker {
    pub fn with_probe(probe: Arc<dyn HostMetricsProbe>) -> Self {
        Self {
            prev_ticks: Arc::new(Mutex::new(HashMap::new())),
            prev_infer: Arc::new(Mutex::new(None)),
            last_storage_probe_ok: Arc::new(Mutex::new(None)),
            probe,
        }
    }

    /// Samples typed `(cpu_pct, mem_pct, rss_bytes)` for `pid`, preserving true resident bytes.
    pub fn sample_pid_resources(
        &self,
        pid: u32,
        fallback_cpu: f64,
        fallback_mem: f64,
        fallback_rss_bytes: u64,
    ) -> ProcessResourceSample {
        let Some(reading) = self.probe.read_pid_proc(pid) else {
            return ProcessResourceSample {
                cpu_pct: fallback_cpu,
                mem_pct: fallback_mem,
                rss_bytes: fallback_rss_bytes,
            };
        };
        let mem_limit = reading.mem_limit_bytes.max(67_108_864);
        let mem_pct = (((reading.rss_bytes as f64) / (mem_limit as f64)) * 100.0).clamp(0.5, 95.0);

        let now = Instant::now();
        let mut guard = self.prev_ticks.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() >= 256 && !guard.contains_key(&pid) {
            guard.retain(|_, (_, inst, _)| now.duration_since(*inst).as_secs() < 600);
            if guard.len() >= 256 {
                guard.clear();
            }
        }
        let cpu_pct = if let Some((prev_t, prev_inst, prev_ema)) = guard.get(&pid).copied() {
            let dt = now.duration_since(prev_inst).as_secs_f64();
            if dt >= 2.0 && reading.total_ticks >= prev_t {
                let delta_sec = (reading.total_ticks - prev_t) as f64 / 100.0;
                let raw_pct = ((delta_sec / dt) * 50.0).clamp(0.4, 95.0);
                let ema = prev_ema * 0.6 + raw_pct * 0.4;
                guard.insert(pid, (reading.total_ticks, now, ema));
                ema
            } else {
                prev_ema
            }
        } else {
            guard.insert(pid, (reading.total_ticks, now, fallback_cpu));
            fallback_cpu
        };

        ProcessResourceSample {
            cpu_pct: (cpu_pct * 10.0).round() / 10.0,
            mem_pct: (mem_pct * 10.0).round() / 10.0,
            rss_bytes: reading.rss_bytes,
        }
    }

    /// Backwards-compatible `(cpu_percent, mem_percent)` accessor around [`Self::sample_pid_resources`].
    pub fn sample_pid(&self, pid: u32, fallback_cpu: f64, fallback_mem: f64) -> (f64, f64) {
        let fallback_rss = ((fallback_mem / 100.0) * 805_306_368.0).round() as u64;
        let sample = self.sample_pid_resources(pid, fallback_cpu, fallback_mem, fallback_rss);
        (sample.cpu_pct, sample.mem_pct)
    }

    /// Computes exact CPU% spent inside OpenVINO/ONNX inference (`cumulative_infer_us` delta).
    pub fn sample_infer_cpu(&self, cumulative_us: u64, fallback_cpu: f64) -> f64 {
        let now = Instant::now();
        let mut guard = self.prev_infer.lock().unwrap_or_else(|e| e.into_inner());
        let pct = if let Some((prev_us, prev_inst, prev_ema)) = *guard {
            let dt_us = now.duration_since(prev_inst).as_micros() as f64;
            if dt_us >= 2_000_000.0 && cumulative_us >= prev_us {
                let delta_us = (cumulative_us - prev_us) as f64;
                let raw_pct = ((delta_us / dt_us) * 100.0).clamp(0.8, 95.0);
                let ema = prev_ema * 0.6 + raw_pct * 0.4;
                *guard = Some((cumulative_us, now, ema));
                ema
            } else {
                prev_ema
            }
        } else {
            *guard = Some((cumulative_us, now, fallback_cpu));
            fallback_cpu
        };
        (pct * 10.0).round() / 10.0
    }

    /// Probes `media_dir` writability with a 15s TTL when healthy and all media subdirectories
    /// (`recordings/`, `clips/`, `exports/`) exist on disk.
    pub fn probe_storage_writable_cached(
        &self,
        media_dir: &Path,
        currently_degraded: bool,
    ) -> Result<(), String> {
        const STORAGE_PROBE_TTL: Duration = Duration::from_secs(15);
        if !currently_degraded
            && media_dir.join("recordings").is_dir()
            && media_dir.join("clips").is_dir()
            && media_dir.join("exports").is_dir()
            && let Ok(guard) = self.last_storage_probe_ok.lock()
            && let Some((ref cached_path, cached_at)) = *guard
            && cached_path == media_dir
            && cached_at.elapsed() < STORAGE_PROBE_TTL
        {
            return Ok(());
        }
        let res = self.probe.probe_media_writable(media_dir);
        if let Ok(mut guard) = self.last_storage_probe_ok.lock() {
            if res.is_ok() {
                *guard = Some((media_dir.to_path_buf(), Instant::now()));
            } else {
                *guard = None;
            }
        }
        res
    }
}

pub fn escape_prom_label_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Canonical camera-online predicate (`camera_fps > 0.0`) shared across Prometheus metrics
/// and AI/MCP digests.
pub fn is_camera_stream_online(camera_fps: f64) -> bool {
    camera_fps.is_finite() && camera_fps > 0.0
}

pub fn filter_stats_snapshot_for_allowed_cameras(val: &mut Value, allow_list: &[String]) {
    if allow_list.iter().any(|a| a == "*") {
        return;
    }
    let known_cameras: std::collections::BTreeSet<String> = val
        .get("cameras")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let is_exact_cam_permitted = |k: &str| allow_list.iter().any(|a| a == k);
    let mut retained_cam_fps = 0.0;
    let mut retained_proc_fps = 0.0;
    let mut retained_skip_fps = 0.0;
    let mut retained_det_fps = 0.0;
    if let Some(cams) = val.get_mut("cameras").and_then(|v| v.as_object_mut()) {
        cams.retain(|k, _| is_exact_cam_permitted(k));
        for c_stat in cams.values() {
            let cfps = c_stat
                .get("camera_fps")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            retained_cam_fps += cfps;
            retained_proc_fps += c_stat
                .get("process_fps")
                .and_then(Value::as_f64)
                .unwrap_or(cfps);
            retained_skip_fps += c_stat
                .get("skipped_fps")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            retained_det_fps += c_stat
                .get("detection_fps")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
        }
    }
    if let Some(obj) = val.as_object_mut() {
        let rounded_cam = (retained_cam_fps * 10.0).round() / 10.0;
        let rounded_proc = (retained_proc_fps * 10.0).round() / 10.0;
        let rounded_skip = (retained_skip_fps * 10.0).round() / 10.0;
        let rounded_det = (retained_det_fps * 10.0).round() / 10.0;
        obj.insert("camera_fps".to_string(), json!(rounded_cam));
        obj.insert("process_fps".to_string(), json!(rounded_proc));
        obj.insert("skipped_fps".to_string(), json!(rounded_skip));
        obj.insert("detection_fps".to_string(), json!(rounded_det));
    }
    if let Some(service_obj) = val.get_mut("service").and_then(|s| s.as_object_mut()) {
        if let Some(cam_storage) = service_obj
            .get_mut("camera_storage")
            .and_then(|v| v.as_object_mut())
        {
            cam_storage.retain(|k, _| is_exact_cam_permitted(k));
        }
        if let Some(heuristics) = service_obj
            .get_mut("heuristics")
            .and_then(|v| v.as_array_mut())
        {
            heuristics.retain(|h| {
                let cat = h.get("category").and_then(Value::as_str).unwrap_or("");
                let target = h.get("target").and_then(Value::as_str).unwrap_or("");
                if cat == "streams" {
                    let targets: Vec<&str> = target
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect();
                    !targets.is_empty()
                        && targets.into_iter().all(|t| {
                            ninjanvr_config::parse_go2rtc_stream_route(t, Some(&known_cameras))
                                .is_some_and(|r| is_exact_cam_permitted(&r.camera))
                        })
                } else if matches!(cat, "vision" | "audio") {
                    let targets: Vec<&str> = target
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect();
                    !targets.is_empty() && targets.into_iter().all(is_exact_cam_permitted)
                } else {
                    true
                }
            });
        }
    }
}

/// Appends `ninjanvr_detector_inference_ms` samples. Only measured latencies are exported.
pub fn render_detector_inference_metrics(stats_val: &Value, out: &mut String) {
    let measured_ms = |d_stat: &Value| -> Option<f64> {
        if d_stat.get("measured").and_then(Value::as_bool) == Some(false) {
            return None;
        }
        d_stat.get("inference_speed").and_then(Value::as_f64)
    };
    if let Some(primary) = stats_val.get("primary_detector")
        && let Some(ms) = measured_ms(primary)
    {
        out.push_str(&format!(
            "ninjanvr_detector_inference_ms{{detector=\"primary\"}} {ms:.2}\n"
        ));
    }
    if let Some(dets) = stats_val.get("detectors").and_then(Value::as_object) {
        for (det_name, d_stat) in dets {
            let Some(ms) = measured_ms(d_stat) else {
                continue;
            };
            let safe_det = escape_prom_label_value(det_name);
            out.push_str(&format!(
                "ninjanvr_detector_inference_ms{{detector=\"{safe_det}\"}} {ms:.2}\n"
            ));
        }
    }
}

/// Renders the Prometheus exposition text from a stats snapshot + persisted event counters,
/// exporting true resident bytes (`service.memory_usage_bytes`) in `ninjanvr_memory_usage_bytes`.
pub fn render_prometheus_exposition(
    stats_val: &Value,
    event_counts: &[(String, String, i64)],
    allowed: Option<&[String]>,
) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str("# HELP ninjanvr_camera_fps Per-camera frame rate by pipeline role.\n");
    out.push_str("# TYPE ninjanvr_camera_fps gauge\n");
    out.push_str(
        "# HELP ninjanvr_camera_online Camera stream online status (1 = online, 0 = offline).\n",
    );
    out.push_str("# TYPE ninjanvr_camera_online gauge\n");

    if let Some(cams) = stats_val.get("cameras").and_then(Value::as_object) {
        for (cam, c_stat) in cams {
            if let Some(allow_list) = allowed
                && !allow_list.iter().any(|a| a == cam || a == "*")
            {
                continue;
            }
            let safe_cam = escape_prom_label_value(cam);
            let cam_fps = c_stat
                .get("camera_fps")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let det_fps = c_stat
                .get("detection_fps")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let proc_fps = c_stat
                .get("process_fps")
                .and_then(Value::as_f64)
                .unwrap_or(cam_fps);
            let online = i32::from(is_camera_stream_online(cam_fps));
            out.push_str(&format!(
                "ninjanvr_camera_fps{{camera=\"{safe_cam}\",role=\"camera\"}} {cam_fps:.2}\n"
            ));
            out.push_str(&format!(
                "ninjanvr_camera_fps{{camera=\"{safe_cam}\",role=\"detect\"}} {det_fps:.2}\n"
            ));
            out.push_str(&format!(
                "ninjanvr_camera_fps{{camera=\"{safe_cam}\",role=\"process\"}} {proc_fps:.2}\n"
            ));
            out.push_str(&format!(
                "ninjanvr_camera_online{{camera=\"{safe_cam}\"}} {online}\n"
            ));
        }
    }

    out.push_str(
        "# HELP ninjanvr_detector_inference_ms AI detector inference latency in milliseconds.\n",
    );
    out.push_str("# TYPE ninjanvr_detector_inference_ms gauge\n");
    render_detector_inference_metrics(stats_val, &mut out);

    out.push_str(
        "# HELP ninjanvr_detector_inferences_total Total AI detector region inferences executed per camera.\n",
    );
    out.push_str("# TYPE ninjanvr_detector_inferences_total counter\n");
    if let Some(cams) = stats_val.get("cameras").and_then(Value::as_object) {
        for (cam, c_stat) in cams {
            if let Some(allow_list) = allowed
                && !allow_list.iter().any(|a| a == cam || a == "*")
            {
                continue;
            }
            let safe_cam = escape_prom_label_value(cam);
            let total_inf = c_stat
                .get("total_inferences")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            out.push_str(&format!(
                "ninjanvr_detector_inferences_total{{camera=\"{safe_cam}\"}} {total_inf}\n"
            ));
        }
    }

    out.push_str("# HELP ninjanvr_storage_used_bytes Used bytes on storage mounts.\n");
    out.push_str("# TYPE ninjanvr_storage_used_bytes gauge\n");
    out.push_str("# HELP ninjanvr_storage_total_bytes Total bytes on storage mounts.\n");
    out.push_str("# TYPE ninjanvr_storage_total_bytes gauge\n");
    if let Some(storages) = stats_val
        .get("service")
        .and_then(|s| s.get("storage"))
        .and_then(Value::as_object)
    {
        for (mount_path, st) in storages {
            let raw_label = if st.get("tier").and_then(Value::as_str) == Some("cold") {
                "cold"
            } else if mount_path == "/media/ninjanvr/recordings" {
                "recordings"
            } else if mount_path == "/media/ninjanvr/clips" {
                "clips"
            } else if mount_path == "/tmp/cache" {
                "cache"
            } else if mount_path == "/dev/shm" {
                "shm"
            } else if mount_path.contains("clips") {
                "clips"
            } else if mount_path.contains("cache") {
                "cache"
            } else if mount_path.contains("shm") {
                "shm"
            } else if mount_path.contains("recordings") && !mount_path.contains("cold") {
                "recordings"
            } else {
                "cold"
            };
            let label = escape_prom_label_value(raw_label);
            let used_mb = st.get("used").and_then(Value::as_f64).unwrap_or(0.0);
            let total_mb = st.get("total").and_then(Value::as_f64).unwrap_or(0.0);
            let used_bytes = (used_mb * 1024.0 * 1024.0).round() as u64;
            let total_bytes = (total_mb * 1024.0 * 1024.0).round() as u64;
            out.push_str(&format!(
                "ninjanvr_storage_used_bytes{{mount=\"{label}\"}} {used_bytes}\n"
            ));
            out.push_str(&format!(
                "ninjanvr_storage_total_bytes{{mount=\"{label}\"}} {total_bytes}\n"
            ));
        }
    }

    let sys_cpu = stats_val
        .get("cpu_usages")
        .and_then(|c| c.get("ninjanvr.full_system"))
        .and_then(|f| f.get("cpu"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    // Prefer exact resident bytes captured in `service.memory_usage_bytes`; only fall back to
    // cgroup-scaled percentage if rendering an external/legacy JSON fixture without `memory_usage_bytes`.
    let sys_mem_bytes = stats_val
        .get("service")
        .and_then(|s| s.get("memory_usage_bytes"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            let pct = stats_val
                .get("cpu_usages")
                .and_then(|c| c.get("ninjanvr.full_system"))
                .and_then(|f| f.get("mem"))
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            ((pct / 100.0) * 805_306_368.0).round() as u64
        });

    out.push_str("# HELP ninjanvr_cpu_usage_percent Process CPU usage percentage.\n");
    out.push_str("# TYPE ninjanvr_cpu_usage_percent gauge\n");
    out.push_str(&format!("ninjanvr_cpu_usage_percent {sys_cpu:.2}\n"));
    out.push_str("# HELP ninjanvr_memory_usage_bytes Process resident memory usage in bytes.\n");
    out.push_str("# TYPE ninjanvr_memory_usage_bytes gauge\n");
    out.push_str(&format!("ninjanvr_memory_usage_bytes {sys_mem_bytes}\n"));

    out.push_str("# HELP ninjanvr_events_total Total persisted object detection events by camera and label.\n");
    out.push_str("# TYPE ninjanvr_events_total counter\n");
    for (cam, lbl, cnt) in event_counts {
        if let Some(allow_list) = allowed
            && !allow_list.iter().any(|a| a == cam || a == "*")
        {
            continue;
        }
        let safe_cam = escape_prom_label_value(cam);
        let safe_lbl = escape_prom_label_value(lbl);
        out.push_str(&format!(
            "ninjanvr_events_total{{camera=\"{safe_cam}\",label=\"{safe_lbl}\"}} {cnt}\n"
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SyntheticHostMetricsProbe {
        readings: HashMap<u32, RawPidProcReading>,
    }

    impl HostMetricsProbe for SyntheticHostMetricsProbe {
        fn read_pid_proc(&self, pid: u32) -> Option<RawPidProcReading> {
            self.readings.get(&pid).copied()
        }

        fn probe_media_writable(&self, _media_dir: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn proc_cpu_tracker_and_prometheus_exposition_preserve_exact_rss_bytes() {
        let mut readings = HashMap::new();
        // 256 MiB RSS against 1 GiB cgroup limit -> 25.0% mem_pct, 268_435_456 exact rss_bytes
        readings.insert(
            100,
            RawPidProcReading {
                total_ticks: 500,
                rss_bytes: 268_435_456,
                mem_limit_bytes: 1_073_741_824,
            },
        );
        // 64 MiB RSS for go2rtc -> 6.0% mem_pct, 67_108_864 exact rss_bytes
        readings.insert(
            200,
            RawPidProcReading {
                total_ticks: 120,
                rss_bytes: 67_108_864,
                mem_limit_bytes: 1_073_741_824,
            },
        );
        let tracker = ProcCpuTracker::with_probe(Arc::new(SyntheticHostMetricsProbe { readings }));
        let nvr_res = tracker.sample_pid_resources(100, 8.5, 19.4, 0);
        let g2r_res = tracker.sample_pid_resources(200, 2.8, 3.6, 0);
        assert_eq!(nvr_res.rss_bytes, 268_435_456);
        assert_eq!(nvr_res.mem_pct, 25.0);
        assert_eq!(g2r_res.rss_bytes, 67_108_864);

        let total_rss = nvr_res.rss_bytes + g2r_res.rss_bytes;
        let snap = json!({
            "cameras": {
                "front_door": { "camera_fps": 15.0, "detection_fps": 5.0, "process_fps": 15.0 }
            },
            "primary_detector": { "inference_speed": 6.4, "measured": true },
            "detectors": {},
            "cpu_usages": {
                "ninjanvr.full_system": {
                    "cpu": format!("{:.1}", nvr_res.cpu_pct + g2r_res.cpu_pct),
                    "mem": format!("{:.1}", nvr_res.mem_pct + g2r_res.mem_pct),
                }
            },
            "service": {
                "memory_usage_bytes": total_rss,
                "storage": {}
            }
        });
        let prom = render_prometheus_exposition(
            &snap,
            &[("front_door".to_string(), "person".to_string(), 7)],
            None,
        );
        assert!(
            prom.contains("ninjanvr_memory_usage_bytes 335544320\n"),
            "Prometheus must export exact resident bytes (320 MiB), not mem_pct * 1 MiB: {prom}"
        );
        assert!(prom.contains("ninjanvr_camera_online{camera=\"front_door\"} 1\n"));
        assert!(prom.contains("ninjanvr_detector_inferences_total{camera=\"front_door\"} 0\n"));
    }

    #[test]
    fn filter_stats_snapshot_isolates_lobby_from_distinct_lobby_sub_camera() {
        let mut snap = json!({
            "cameras": {
                "lobby": { "camera_fps": 15.0, "detection_fps": 5.0 },
                "lobby_sub": { "camera_fps": 20.0, "detection_fps": 8.0 },
                "porch": { "camera_fps": 10.0, "detection_fps": 2.0 }
            },
            "service": {
                "camera_storage": {
                    "lobby": { "usage": 100.0 },
                    "lobby_sub": { "usage": 999.0 }
                },
                "heuristics": [
                    { "category": "streams", "target": "lobby, lobby_talk" },
                    { "category": "streams", "target": "lobby, lobby_sub" },
                    { "category": "streams", "target": "lobby_sub_talk" },
                    { "category": "vision", "target": "lobby_sub" },
                    { "category": "vision", "target": "lobby" }
                ]
            }
        });
        filter_stats_snapshot_for_allowed_cameras(&mut snap, &["lobby".to_string()]);

        let cams = snap["cameras"].as_object().unwrap();
        assert_eq!(cams.len(), 1);
        assert!(cams.contains_key("lobby"));
        assert!(!cams.contains_key("lobby_sub"));
        assert_eq!(snap["camera_fps"], json!(15.0));

        let storage = snap["service"]["camera_storage"].as_object().unwrap();
        assert_eq!(storage.len(), 1);
        assert!(storage.contains_key("lobby"));
        assert!(!storage.contains_key("lobby_sub"));

        let heur = snap["service"]["heuristics"].as_array().unwrap();
        assert_eq!(heur.len(), 2);
        assert_eq!(heur[0]["target"], "lobby, lobby_talk");
        assert_eq!(heur[1]["target"], "lobby");
    }
}
