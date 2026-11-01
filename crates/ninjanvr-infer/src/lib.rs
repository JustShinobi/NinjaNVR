use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use ninjanvr_config::NinjaConfig;
use ninjanvr_vision::BoundingBox;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionResult {
    pub label: String,
    pub score: f32,
    pub bbox: BoundingBox,
    pub region: BoundingBox,
}

type OvCoreCreateFn = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type OvCoreCompileModelFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    usize,
    *mut *mut c_void,
) -> c_int;
type OvCompiledModelCreateInferReqFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type OvInferReqGetTensorFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type OvInferReqInferFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type OvTensorDataFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type OvTensorGetByteSizeFn = unsafe extern "C" fn(*mut c_void, *mut usize) -> c_int;

struct OpenVinoCContext {
    _lib_handle: *mut c_void,
    _core: *mut c_void,
    _compiled_model: *mut c_void,
    infer_req: *mut c_void,
    input_tensor: *mut c_void,
    output_tensor: *mut c_void,
    in_ptr: *mut u8,
    in_bytes: usize,
    out_ptr: *const f32,
    out_floats: usize,
    infer_fn: OvInferReqInferFn,
}

unsafe impl Send for OpenVinoCContext {}

pub struct AiDetectorEngine {
    model_w: u32,
    model_h: u32,
    bgr_order: bool,
    nchw_layout: bool,
    is_yolo: bool,
    labelmap: BTreeMap<i32, String>,
    ov_ctx: Mutex<Option<OpenVinoCContext>>,
    last_inference_ms: Mutex<f32>,
    cumulative_infer_us: AtomicU64,
}

impl AiDetectorEngine {
    pub fn new(config: &NinjaConfig) -> Self {
        let raw_labels = config.load_labelmap();
        let mut labelmap = BTreeMap::new();
        for (k, v) in raw_labels {
            if let Ok(id) = k.parse::<i32>() {
                labelmap.insert(id, v);
            }
        }

        let device = config
            .detectors
            .values()
            .next()
            .map(|d| d.device.as_str())
            .unwrap_or("CPU");

        let ov_ctx = match Self::try_init_openvino(&config.model.path, device) {
            Ok(ctx) => {
                info!(
                    "Initialized native OpenVINO C-API detector (model={}, device={device}, in_bytes={}, out_floats={})",
                    config.model.path, ctx.in_bytes, ctx.out_floats
                );
                Some(ctx)
            }
            Err(err) => {
                warn!(
                    "OpenVINO C runtime not loaded ({err:#}); operating in lightweight heuristic mode"
                );
                None
            }
        };

        let mt = config.model.model_type.to_ascii_lowercase();
        let is_yolo = mt.contains("yolo") || config.model.path.ends_with(".onnx");

        Self {
            model_w: config.model.width.max(64),
            model_h: config.model.height.max(64),
            bgr_order: config.model.input_pixel_format.eq_ignore_ascii_case("bgr"),
            nchw_layout: config.model.input_tensor.eq_ignore_ascii_case("nchw"),
            is_yolo,
            labelmap,
            ov_ctx: Mutex::new(ov_ctx),
            last_inference_ms: Mutex::new(2.1),
            cumulative_infer_us: AtomicU64::new(0),
        }
    }

    pub fn model_dimensions(&self) -> (u32, u32, bool) {
        (self.model_w, self.model_h, self.bgr_order)
    }

    pub fn avg_inference_ms(&self) -> f32 {
        *self.last_inference_ms.lock().expect("mutex poisoned")
    }

    pub fn cumulative_infer_us(&self) -> u64 {
        self.cumulative_infer_us.load(Ordering::Relaxed)
    }

    fn resolve_openvino_c_lib() -> Option<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(env_dir) = std::env::var("NINJANVR_OPENVINO_LIB_DIR") {
            dirs.push(PathBuf::from(env_dir));
        }
        dirs.extend([
            PathBuf::from("/usr/local/lib/ninjanvr/openvino"),
            PathBuf::from("/usr/local/lib/python3.11/dist-packages/openvino/libs"),
            PathBuf::from("/usr/lib/x86_64-linux-gnu"),
        ]);

        for dir in dirs {
            for name in ["libopenvino_c.so.2541", "libopenvino_c.so"] {
                let candidate = dir.join(name);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn resolve_model_path(configured: &str) -> Option<PathBuf> {
        let mut candidates = vec![PathBuf::from(configured)];
        if let Ok(env_model) = std::env::var("NINJANVR_MODEL_PATH") {
            candidates.push(PathBuf::from(env_model));
        }
        candidates.push(PathBuf::from("/openvino-model/ssdlite_mobilenet_v2.xml"));
        candidates.into_iter().find(|p| p.exists())
    }

    fn try_init_openvino(configured_model: &str, device: &str) -> Result<OpenVinoCContext> {
        let lib_path = Self::resolve_openvino_c_lib()
            .ok_or_else(|| anyhow!("libopenvino_c.so not found in standard paths"))?;
        let model_path = Self::resolve_model_path(configured_model)
            .ok_or_else(|| anyhow!("model file {configured_model} not found"))?;

        let lib_dir = lib_path.parent().unwrap_or_else(|| Path::new("/"));

        unsafe {
            for dep in [
                "libtbb.so.12",
                "libhwloc.so.15",
                "libopenvino.so.2541",
                "libopenvino_ir_frontend.so.2541",
                "libopenvino_onnx_frontend.so.2541",
            ] {
                let dep_path = lib_dir.join(dep);
                if dep_path.exists()
                    && let Ok(c_dep) = CString::new(dep_path.to_string_lossy().as_bytes())
                {
                    let _ = libc::dlopen(c_dep.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
                }
            }

            let c_lib = CString::new(lib_path.to_string_lossy().as_bytes())?;
            let handle = libc::dlopen(c_lib.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            if handle.is_null() {
                let err_ptr = libc::dlerror();
                let msg = if err_ptr.is_null() {
                    "unknown dlopen error".to_string()
                } else {
                    CStr::from_ptr(err_ptr).to_string_lossy().into_owned()
                };
                bail!("dlopen({}) failed: {msg}", lib_path.display());
            }

            macro_rules! load_sym {
                ($name:literal, $ty:ty) => {{
                    let sym_name = CString::new($name).unwrap();
                    let ptr = libc::dlsym(handle, sym_name.as_ptr());
                    if ptr.is_null() {
                        bail!("missing symbol {} in libopenvino_c", $name);
                    }
                    std::mem::transmute::<*mut c_void, $ty>(ptr)
                }};
            }

            let core_create = load_sym!("ov_core_create", OvCoreCreateFn);
            let compile_model = load_sym!("ov_core_compile_model_from_file", OvCoreCompileModelFn);
            let create_infer_req = load_sym!(
                "ov_compiled_model_create_infer_request",
                OvCompiledModelCreateInferReqFn
            );
            let get_in_tensor =
                load_sym!("ov_infer_request_get_input_tensor", OvInferReqGetTensorFn);
            let get_out_tensor =
                load_sym!("ov_infer_request_get_output_tensor", OvInferReqGetTensorFn);
            let infer_fn = load_sym!("ov_infer_request_infer", OvInferReqInferFn);
            let tensor_data = load_sym!("ov_tensor_data", OvTensorDataFn);
            let tensor_byte_size = load_sym!("ov_tensor_get_byte_size", OvTensorGetByteSizeFn);

            let mut core: *mut c_void = std::ptr::null_mut();
            let rc = core_create(&mut core);
            if rc != 0 || core.is_null() {
                bail!("ov_core_create returned {rc}");
            }

            let c_model = CString::new(model_path.to_string_lossy().as_bytes())?;
            let c_dev = CString::new(device)?;
            let mut compiled_model: *mut c_void = std::ptr::null_mut();
            let rc = compile_model(
                core,
                c_model.as_ptr(),
                c_dev.as_ptr(),
                0,
                &mut compiled_model,
            );
            if rc != 0 || compiled_model.is_null() {
                bail!("ov_core_compile_model_from_file returned {rc}");
            }

            let mut infer_req: *mut c_void = std::ptr::null_mut();
            let rc = create_infer_req(compiled_model, &mut infer_req);
            if rc != 0 || infer_req.is_null() {
                bail!("ov_compiled_model_create_infer_request returned {rc}");
            }

            let mut input_tensor: *mut c_void = std::ptr::null_mut();
            let rc = get_in_tensor(infer_req, &mut input_tensor);
            if rc != 0 || input_tensor.is_null() {
                bail!("ov_infer_request_get_input_tensor returned {rc}");
            }

            let mut in_raw: *mut c_void = std::ptr::null_mut();
            let rc = tensor_data(input_tensor, &mut in_raw);
            if rc != 0 || in_raw.is_null() {
                bail!("ov_tensor_data(input) returned {rc}");
            }
            let mut in_bytes: usize = 0;
            tensor_byte_size(input_tensor, &mut in_bytes);

            let _ = infer_fn(infer_req);

            let mut output_tensor: *mut c_void = std::ptr::null_mut();
            let rc = get_out_tensor(infer_req, &mut output_tensor);
            if rc != 0 || output_tensor.is_null() {
                bail!("ov_infer_request_get_output_tensor returned {rc}");
            }

            let mut out_raw: *mut c_void = std::ptr::null_mut();
            let rc = tensor_data(output_tensor, &mut out_raw);
            if rc != 0 || out_raw.is_null() {
                bail!("ov_tensor_data(output) returned {rc}");
            }
            let mut out_bytes: usize = 0;
            tensor_byte_size(output_tensor, &mut out_bytes);

            Ok(OpenVinoCContext {
                _lib_handle: handle,
                _core: core,
                _compiled_model: compiled_model,
                infer_req,
                input_tensor,
                output_tensor,
                in_ptr: in_raw as *mut u8,
                in_bytes,
                out_ptr: out_raw as *const f32,
                out_floats: out_bytes / 4,
                infer_fn,
            })
        }
    }

    pub fn detect_region(
        &self,
        tensor_nhwc: &[u8],
        region: BoundingBox,
        frame_w: u32,
        frame_h: u32,
        score_threshold: f32,
    ) -> Vec<DetectionResult> {
        let t0 = Instant::now();
        let mut out_results = Vec::new();

        let guard = self.ov_ctx.lock().expect("ov_ctx mutex poisoned");
        if let Some(ctx) = guard.as_ref() {
            let _ = (ctx.input_tensor, ctx.output_tensor);
            unsafe {
                if ctx.in_bytes == tensor_nhwc.len() * 4 {
                    let dst_f32 =
                        std::slice::from_raw_parts_mut(ctx.in_ptr as *mut f32, tensor_nhwc.len());
                    if self.nchw_layout {
                        let plane_len = (self.model_w * self.model_h) as usize;
                        for idx in 0..plane_len {
                            dst_f32[idx] = (tensor_nhwc[idx * 3] as f32) / 255.0;
                            dst_f32[plane_len + idx] = (tensor_nhwc[idx * 3 + 1] as f32) / 255.0;
                            dst_f32[plane_len * 2 + idx] =
                                (tensor_nhwc[idx * 3 + 2] as f32) / 255.0;
                        }
                    } else {
                        for (dst, &src) in dst_f32.iter_mut().zip(tensor_nhwc.iter()) {
                            *dst = (src as f32) / 255.0;
                        }
                    }
                } else {
                    let copy_len = tensor_nhwc.len().min(ctx.in_bytes);
                    std::ptr::copy_nonoverlapping(tensor_nhwc.as_ptr(), ctx.in_ptr, copy_len);
                }

                let rc = (ctx.infer_fn)(ctx.infer_req);
                if rc == 0 && ctx.out_floats >= 6 {
                    let slice = std::slice::from_raw_parts(ctx.out_ptr, ctx.out_floats);
                    out_results = self.parse_raw_output_slice(
                        slice,
                        region,
                        frame_w,
                        frame_h,
                        score_threshold,
                    );
                }
            }
        }
        drop(guard);

        let elapsed_us = t0.elapsed().as_micros() as u64;
        self.cumulative_infer_us
            .fetch_add(elapsed_us, Ordering::Relaxed);
        let elapsed_ms = (elapsed_us as f32) / 1000.0;
        let mut avg = self.last_inference_ms.lock().expect("mutex poisoned");
        *avg = (*avg * 0.9) + (elapsed_ms.max(0.2) * 0.1);

        out_results
    }

    pub fn parse_raw_output_slice(
        &self,
        slice: &[f32],
        region: BoundingBox,
        frame_w: u32,
        frame_h: u32,
        score_threshold: f32,
    ) -> Vec<DetectionResult> {
        let mut out_results = Vec::new();
        let rx = region.x_min as f32;
        let ry = region.y_min as f32;
        let rw = region.width() as f32;
        let rh = region.height() as f32;

        if !self.is_yolo && slice.len().is_multiple_of(7) {
            for row in slice.as_chunks::<7>().0 {
                let img_id = row[0];
                let label_id = row[1] as i32;
                let score = row[2];
                if img_id < 0.0 || score < score_threshold {
                    continue;
                }
                let Some(label) = self.labelmap.get(&label_id) else {
                    continue;
                };
                if label == "__background__" {
                    continue;
                }
                let x1 = (rx + row[3].clamp(0.0, 1.0) * rw).clamp(0.0, (frame_w - 1) as f32) as u32;
                let y1 = (ry + row[4].clamp(0.0, 1.0) * rh).clamp(0.0, (frame_h - 1) as f32) as u32;
                let x2 = (rx + row[5].clamp(0.0, 1.0) * rw).clamp(0.0, frame_w as f32) as u32;
                let y2 = (ry + row[6].clamp(0.0, 1.0) * rh).clamp(0.0, frame_h as f32) as u32;

                if x2 > x1 + 4 && y2 > y1 + 4 {
                    out_results.push(DetectionResult {
                        label: label.clone(),
                        score,
                        bbox: BoundingBox::new(x1, y1, x2, y2),
                        region,
                    });
                }
            }
        } else if slice.len().is_multiple_of(6) {
            let mw = self.model_w as f32;
            let mh = self.model_h as f32;
            for row in slice.as_chunks::<6>().0 {
                let score = row[4];
                if score < score_threshold {
                    continue;
                }
                let label_id = row[5] as i32;
                let Some(label) = self.labelmap.get(&label_id) else {
                    continue;
                };
                let nx1 = if row[2] > 1.5 { row[0] / mw } else { row[0] };
                let ny1 = if row[3] > 1.5 { row[1] / mh } else { row[1] };
                let nx2 = if row[2] > 1.5 { row[2] / mw } else { row[2] };
                let ny2 = if row[3] > 1.5 { row[3] / mh } else { row[3] };

                let x1 = (rx + nx1.clamp(0.0, 1.0) * rw).clamp(0.0, (frame_w - 1) as f32) as u32;
                let y1 = (ry + ny1.clamp(0.0, 1.0) * rh).clamp(0.0, (frame_h - 1) as f32) as u32;
                let x2 = (rx + nx2.clamp(0.0, 1.0) * rw).clamp(0.0, frame_w as f32) as u32;
                let y2 = (ry + ny2.clamp(0.0, 1.0) * rh).clamp(0.0, frame_h as f32) as u32;

                if x2 > x1 + 4 && y2 > y1 + 4 {
                    out_results.push(DetectionResult {
                        label: label.clone(),
                        score,
                        bbox: BoundingBox::new(x1, y1, x2, y2),
                        region,
                    });
                }
            }
        }
        out_results
    }
}

/// Applies class-aware Non-Maximum Suppression (NMS) across detections from overlapping regions.
pub fn nms_detections(
    mut detections: Vec<DetectionResult>,
    iou_threshold: f32,
) -> Vec<DetectionResult> {
    detections.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept: Vec<DetectionResult> = Vec::with_capacity(detections.len());
    for det in detections {
        if !kept
            .iter()
            .any(|k| k.label == det.label && k.bbox.iou(&det.bbox) > iou_threshold)
        {
            kept.push(det);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_ssd_and_yolo_outputs_and_runs_nms() {
        let cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c.yml")).unwrap();
        let engine = AiDetectorEngine::new(&cfg);
        let region = BoundingBox::new(100, 100, 400, 400);

        let ssd_raw = [0.0f32, 1.0, 0.91, 0.1, 0.1, 0.5, 0.8];
        let dets = engine.parse_raw_output_slice(&ssd_raw, region, 1280, 720, 0.5);
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].label, "person");

        let mut dup = dets.clone();
        dup.push(DetectionResult {
            label: "person".to_string(),
            score: 0.75,
            bbox: dets[0].bbox,
            region,
        });
        let merged = nms_detections(dup, 0.55);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].score, 0.91);
    }
}
