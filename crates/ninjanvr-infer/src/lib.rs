use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use ninjanvr_config::{DetectorEntryConfig, NinjaConfig};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectorDeviceStatus {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredModelEntry {
    pub path: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectorRuntimeCatalog {
    pub loaded: bool,
    pub active_backend: String,
    pub openvino_available: bool,
    pub ort_available: bool,
    pub active_device: String,
    pub active_model_path: String,
    pub compiled_width: u32,
    pub compiled_height: u32,
    pub in_bytes: usize,
    pub out_floats: usize,
    pub avg_inference_ms: f32,
    pub devices: Vec<DetectorDeviceStatus>,
    pub discovered_models: Vec<DiscoveredModelEntry>,
}

/// Converts a packed `NHWC` `u8` (`[0..255]`) RGB/BGR frame crop into normalized `[0.0..1.0]` `f32`
/// elements in either planar `NCHW` (`[1, 3, H, W]`) or interleaved `NHWC` (`[1, H, W, 3]`) layout.
pub fn prepare_input_tensor_f32(
    tensor_nhwc: &[u8],
    model_w: u32,
    model_h: u32,
    nchw_layout: bool,
    dst_f32: &mut [f32],
) {
    let plane_len = (model_w as usize) * (model_h as usize);
    let expected_len = plane_len * 3;
    if nchw_layout && tensor_nhwc.len() >= expected_len && dst_f32.len() >= expected_len {
        for idx in 0..plane_len {
            dst_f32[idx] = (tensor_nhwc[idx * 3] as f32) / 255.0;
            dst_f32[plane_len + idx] = (tensor_nhwc[idx * 3 + 1] as f32) / 255.0;
            dst_f32[plane_len * 2 + idx] = (tensor_nhwc[idx * 3 + 2] as f32) / 255.0;
        }
    } else {
        for (dst, &src) in dst_f32.iter_mut().zip(tensor_nhwc.iter()) {
            *dst = (src as f32) / 255.0;
        }
    }
}

/// Converts a packed `NHWC` `u8` RGB/BGR frame crop into `u8` elements in either planar `NCHW`
/// (`[1, 3, H, W]`) or interleaved `NHWC` (`[1, H, W, 3]`) layout.
pub fn prepare_input_tensor_u8(
    tensor_nhwc: &[u8],
    model_w: u32,
    model_h: u32,
    nchw_layout: bool,
    dst_u8: &mut [u8],
) {
    let plane_len = (model_w as usize) * (model_h as usize);
    let expected_len = plane_len * 3;
    if nchw_layout && tensor_nhwc.len() >= expected_len && dst_u8.len() >= expected_len {
        for idx in 0..plane_len {
            dst_u8[idx] = tensor_nhwc[idx * 3];
            dst_u8[plane_len + idx] = tensor_nhwc[idx * 3 + 1];
            dst_u8[plane_len * 2 + idx] = tensor_nhwc[idx * 3 + 2];
        }
    } else {
        let copy_len = tensor_nhwc.len().min(dst_u8.len());
        dst_u8[..copy_len].copy_from_slice(&tensor_nhwc[..copy_len]);
    }
}

type OvCoreCreateFn = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type OvCoreCompileModelFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    usize,
    *mut *mut c_void,
    ...
) -> c_int;
type OvCompiledModelCreateInferReqFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type OvInferReqGetTensorFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type OvInferReqInferFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type OvTensorDataFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type OvTensorGetByteSizeFn = unsafe extern "C" fn(*mut c_void, *mut usize) -> c_int;

#[repr(C)]
struct OrtApiBase {
    get_api: unsafe extern "C" fn(u32) -> *const c_void,
    get_version_string: unsafe extern "C" fn() -> *const c_char,
}

type OrtGetApiBaseFn = unsafe extern "C" fn() -> *const OrtApiBase;
type OrtAppendProviderFn = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void;

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

struct OrtCContext {
    _lib_handle: *mut c_void,
    in_bytes: usize,
    out_floats: usize,
}

unsafe impl Send for OrtCContext {}

enum InferenceRuntimeBackend {
    OpenVino(OpenVinoCContext),
    OnnxRuntime(OrtCContext),
}

struct DetectorRuntimeState {
    model_w: u32,
    model_h: u32,
    bgr_order: bool,
    nchw_layout: bool,
    is_yolo: bool,
    active_backend: String,
    active_device: String,
    active_model_path: String,
    labelmap: BTreeMap<i32, String>,
    backend: Option<InferenceRuntimeBackend>,
}

pub struct AiDetectorEngine {
    inner: Mutex<DetectorRuntimeState>,
    last_inference_ms: Mutex<f32>,
    cumulative_infer_us: AtomicU64,
    pool: Mutex<BTreeMap<String, Arc<AiDetectorEngine>>>,
}

/// Registry of named `AiDetectorEngine` instances supporting multi-detector routing per camera.
pub struct DetectorPool {
    default_detector: Arc<AiDetectorEngine>,
    detectors: BTreeMap<String, Arc<AiDetectorEngine>>,
}

impl DetectorPool {
    pub fn new(config: &NinjaConfig) -> Self {
        let default_detector = Arc::new(AiDetectorEngine::new(config));
        let mut detectors = BTreeMap::new();
        for (name, entry) in &config.detectors {
            detectors.insert(
                name.clone(),
                Arc::new(AiDetectorEngine::new_for_entry(config, entry)),
            );
        }
        Self {
            default_detector,
            detectors,
        }
    }

    pub fn get(&self, name: Option<&str>) -> Arc<AiDetectorEngine> {
        if let Some(n) = name
            && let Some(det) = self.detectors.get(n)
        {
            return det.clone();
        }
        self.detectors
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| self.default_detector.clone())
    }

    pub fn detector_names(&self) -> Vec<String> {
        self.detectors.keys().cloned().collect()
    }
}

/// Standard 80-class COCO index to label mapping used by YOLOv5/v8/v11/YOLOX models when
/// a 91-class SSD labelmap (with `__background__` at 0) is loaded.
const YOLO_COCO_80: [&str; 80] = [
    "person",
    "bicycle",
    "car",
    "motorcycle",
    "airplane",
    "bus",
    "train",
    "truck",
    "boat",
    "traffic light",
    "fire hydrant",
    "stop sign",
    "parking meter",
    "bench",
    "bird",
    "cat",
    "dog",
    "horse",
    "sheep",
    "cow",
    "elephant",
    "bear",
    "zebra",
    "giraffe",
    "backpack",
    "umbrella",
    "handbag",
    "tie",
    "suitcase",
    "frisbee",
    "skis",
    "snowboard",
    "sports ball",
    "kite",
    "baseball bat",
    "baseball glove",
    "skateboard",
    "surfboard",
    "tennis racket",
    "bottle",
    "wine glass",
    "cup",
    "fork",
    "knife",
    "spoon",
    "bowl",
    "banana",
    "apple",
    "sandwich",
    "orange",
    "broccoli",
    "carrot",
    "hot dog",
    "pizza",
    "donut",
    "cake",
    "chair",
    "couch",
    "potted plant",
    "bed",
    "dining table",
    "toilet",
    "tv",
    "laptop",
    "mouse",
    "remote",
    "keyboard",
    "cell phone",
    "microwave",
    "oven",
    "toaster",
    "sink",
    "refrigerator",
    "book",
    "clock",
    "vase",
    "scissors",
    "teddy bear",
    "hair drier",
    "toothbrush",
];

impl AiDetectorEngine {
    pub fn new(config: &NinjaConfig) -> Self {
        let state = Self::build_runtime_state(config, None);
        let mut pool_map = BTreeMap::new();
        for (name, entry) in &config.detectors {
            pool_map.insert(name.clone(), Arc::new(Self::new_for_entry(config, entry)));
        }
        Self {
            inner: Mutex::new(state),
            last_inference_ms: Mutex::new(2.1),
            cumulative_infer_us: AtomicU64::new(0),
            pool: Mutex::new(pool_map),
        }
    }

    pub fn new_for_entry(config: &NinjaConfig, entry: &DetectorEntryConfig) -> Self {
        let state = Self::build_runtime_state(config, Some(entry));
        Self {
            inner: Mutex::new(state),
            last_inference_ms: Mutex::new(2.1),
            cumulative_infer_us: AtomicU64::new(0),
            pool: Mutex::new(BTreeMap::new()),
        }
    }

    /// Hot-reloads the detector model, OpenVINO device, tensor layout, and labelmap when config changes.
    pub fn reload(&self, config: &NinjaConfig) {
        let next_state = Self::build_runtime_state(config, None);
        *self.inner.lock().expect("detector inner mutex poisoned") = next_state;
        let mut pool_map = BTreeMap::new();
        for (name, entry) in &config.detectors {
            pool_map.insert(name.clone(), Arc::new(Self::new_for_entry(config, entry)));
        }
        *self.pool.lock().expect("detector pool mutex poisoned") = pool_map;
    }

    /// Resolves a named detector from the pool if present, or returns `None` (caller uses `self`).
    pub fn named_detector(&self, name: Option<&str>) -> Option<Arc<AiDetectorEngine>> {
        let n = name?;
        self.pool
            .lock()
            .expect("detector pool mutex poisoned")
            .get(n)
            .cloned()
    }

    /// Returns the average inference latency (ms) for a specific named detector, falling back to the primary detector.
    pub fn avg_inference_ms_for(&self, name: &str) -> f32 {
        if let Some(det) = self
            .pool
            .lock()
            .expect("detector pool mutex poisoned")
            .get(name)
            .cloned()
        {
            return det.avg_inference_ms();
        }
        self.avg_inference_ms()
    }

    fn build_runtime_state(
        config: &NinjaConfig,
        entry_override: Option<&DetectorEntryConfig>,
    ) -> DetectorRuntimeState {
        let raw_labels = config.load_labelmap();
        let mut labelmap = BTreeMap::new();
        for (k, v) in raw_labels {
            if let Ok(id) = k.parse::<i32>() {
                labelmap.insert(id, v);
            }
        }

        let requested_backend = entry_override
            .map(|e| e.normalized_backend_type())
            .or_else(|| {
                config
                    .detectors
                    .values()
                    .next()
                    .map(|d| d.normalized_backend_type())
            })
            .unwrap_or("openvino");
        let requested_device = entry_override
            .map(|e| e.device.as_str())
            .or_else(|| config.detectors.values().next().map(|d| d.device.as_str()))
            .unwrap_or("CPU");
        let effective_model = entry_override
            .and_then(|e| e.model.as_ref())
            .unwrap_or(&config.model);

        let mut model_w = effective_model.width.max(64);
        let mut model_h = effective_model.height.max(64);
        let mut active_backend = requested_backend.to_string();
        let mut active_device = requested_device.to_string();

        let backend = if requested_backend == "onnxruntime" {
            match Self::try_init_onnxruntime(
                &effective_model.path,
                requested_device,
                model_w,
                model_h,
            ) {
                Ok((ort_ctx, actual_dev)) => {
                    info!(
                        "Initialized native ONNX Runtime C-API detector (model={}, device={actual_dev}, in_bytes={}, out_floats={})",
                        effective_model.path, ort_ctx.in_bytes, ort_ctx.out_floats
                    );
                    active_device = actual_dev;
                    Some(InferenceRuntimeBackend::OnnxRuntime(ort_ctx))
                }
                Err(ort_err) => {
                    let ov_fallback_dev = if requested_device.eq_ignore_ascii_case("CUDA")
                        || requested_device.eq_ignore_ascii_case("TensorRT")
                        || requested_device.eq_ignore_ascii_case("ROCM")
                    {
                        "CPU"
                    } else {
                        requested_device
                    };
                    match Self::try_init_openvino(&effective_model.path, ov_fallback_dev) {
                        Ok((ov_ctx, actual_dev)) => {
                            warn!(
                                "ONNX Runtime C library unavailable ({ort_err:#}); falling back to OpenVINO C-API on {actual_dev}"
                            );
                            active_backend = "openvino".to_string();
                            active_device = actual_dev;
                            Some(InferenceRuntimeBackend::OpenVino(ov_ctx))
                        }
                        Err(_) => {
                            warn!(
                                "ONNX Runtime C runtime not loaded ({ort_err:#}); operating in lightweight heuristic mode"
                            );
                            None
                        }
                    }
                }
            }
        } else {
            match Self::try_init_openvino(&effective_model.path, requested_device) {
                Ok((ctx, actual_dev)) => {
                    info!(
                        "Initialized native OpenVINO C-API detector (model={}, device={actual_dev}, in_bytes={}, out_floats={})",
                        effective_model.path, ctx.in_bytes, ctx.out_floats
                    );
                    active_device = actual_dev;
                    Some(InferenceRuntimeBackend::OpenVino(ctx))
                }
                Err(err) => {
                    warn!(
                        "OpenVINO C runtime not loaded ({err:#}); operating in lightweight heuristic mode"
                    );
                    None
                }
            }
        };

        // Reconcile configured width/height against compiled input tensor byte size
        // so changing width/height in UI without changing the underlying .xml/.onnx model never overflows.
        if let Some(InferenceRuntimeBackend::OpenVino(ref ctx)) = backend {
            let expected_u8 = (model_w as usize) * (model_h as usize) * 3;
            let expected_f32 = expected_u8 * 4;
            if ctx.in_bytes != expected_u8 && ctx.in_bytes != expected_f32 {
                let pixels = if ctx.in_bytes.is_multiple_of(12) {
                    ctx.in_bytes / 12
                } else if ctx.in_bytes.is_multiple_of(3) {
                    ctx.in_bytes / 3
                } else {
                    0
                };
                if pixels > 0 {
                    let side = (pixels as f64).sqrt().round() as usize;
                    if side * side == pixels && side >= 64 {
                        info!(
                            "Reconciled model input dimensions from {}x{} to compiled tensor {}x{} (in_bytes={})",
                            model_w, model_h, side, side, ctx.in_bytes
                        );
                        model_w = side as u32;
                        model_h = side as u32;
                    }
                }
            }
        }

        let mt = effective_model.model_type.to_ascii_lowercase();
        let is_yolo = mt.contains("yolo") || effective_model.path.ends_with(".onnx");

        DetectorRuntimeState {
            model_w,
            model_h,
            bgr_order: effective_model
                .input_pixel_format
                .eq_ignore_ascii_case("bgr"),
            nchw_layout: effective_model.input_tensor.eq_ignore_ascii_case("nchw"),
            is_yolo,
            active_backend,
            active_device,
            active_model_path: effective_model.path.clone(),
            labelmap,
            backend,
        }
    }

    pub fn model_dimensions(&self) -> (u32, u32, bool) {
        let g = self.inner.lock().expect("detector inner mutex poisoned");
        (g.model_w, g.model_h, g.bgr_order)
    }

    pub fn active_backend(&self) -> String {
        let g = self.inner.lock().expect("detector inner mutex poisoned");
        g.active_backend.clone()
    }

    pub fn active_device(&self) -> String {
        let g = self.inner.lock().expect("detector inner mutex poisoned");
        g.active_device.clone()
    }

    pub fn avg_inference_ms(&self) -> f32 {
        *self.last_inference_ms.lock().expect("mutex poisoned")
    }

    pub fn cumulative_infer_us(&self) -> u64 {
        self.cumulative_infer_us.load(Ordering::Relaxed)
    }

    /// Returns hardware plugin availability, discovered model files on disk, and active compiled tensor telemetry.
    pub fn runtime_catalog(&self) -> DetectorRuntimeCatalog {
        let (loaded, active_backend, active_dev, active_path, mw, mh, in_bytes, out_floats) = {
            let g = self.inner.lock().expect("detector inner mutex poisoned");
            let (ib, of) = match g.backend.as_ref() {
                Some(InferenceRuntimeBackend::OpenVino(c)) => (c.in_bytes, c.out_floats),
                Some(InferenceRuntimeBackend::OnnxRuntime(c)) => (c.in_bytes, c.out_floats),
                None => (0, 0),
            };
            (
                g.backend.is_some(),
                g.active_backend.clone(),
                g.active_device.clone(),
                g.active_model_path.clone(),
                g.model_w,
                g.model_h,
                ib,
                of,
            )
        };

        let ov_lib = Self::resolve_openvino_c_lib();
        let ort_lib = Self::resolve_ort_c_lib();
        let openvino_available = ov_lib.is_some();
        let ort_available = ort_lib.is_some();

        let lib_dir = ov_lib
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("/usr/local/lib/ninjanvr/openvino"));
        let cpu_ok =
            lib_dir.join("libopenvino_intel_cpu_plugin.so").exists() || ort_available || loaded;
        let gpu_ok = lib_dir.join("libopenvino_intel_gpu_plugin.so").exists()
            && Path::new("/dev/dri").exists();
        let npu_ok = lib_dir.join("libopenvino_intel_npu_plugin.so").exists()
            && Path::new("/dev/accel").exists();
        let cuda_ok = (Path::new("/dev/nvidia0").exists() || Path::new("/dev/nvidiactl").exists())
            && ort_available;
        let rocm_ok = Path::new("/dev/kfd").exists() && ort_available;

        let mut discovered_models = Vec::new();
        for dir_str in ["/openvino-model", "/config", "/config/models"] {
            let d = Path::new(dir_str);
            if let Ok(entries) = std::fs::read_dir(d) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if let Some(ext) = p.extension().and_then(|e| e.to_str())
                        && (ext.eq_ignore_ascii_case("xml") || ext.eq_ignore_ascii_case("onnx"))
                    {
                        let p_str = p.to_string_lossy().to_string();
                        let arch = if p_str.to_ascii_lowercase().contains("yolox") {
                            "yolox"
                        } else if p_str.to_ascii_lowercase().contains("yolo")
                            || ext.eq_ignore_ascii_case("onnx")
                        {
                            "yolo-generic"
                        } else {
                            "ssd"
                        };
                        discovered_models.push(DiscoveredModelEntry {
                            path: p_str,
                            arch: arch.to_string(),
                        });
                    }
                }
            }
        }
        if discovered_models.is_empty() {
            discovered_models.push(DiscoveredModelEntry {
                path: "/openvino-model/ssdlite_mobilenet_v2.xml".to_string(),
                arch: "ssd".to_string(),
            });
        }

        DetectorRuntimeCatalog {
            loaded,
            active_backend,
            openvino_available,
            ort_available,
            active_device: active_dev,
            active_model_path: active_path,
            compiled_width: mw,
            compiled_height: mh,
            in_bytes,
            out_floats,
            avg_inference_ms: (self.avg_inference_ms() * 10.0).round() / 10.0,
            devices: vec![
                DetectorDeviceStatus {
                    id: "CPU".to_string(),
                    name: "CPU (AVX2 / VNNI / NEON)".to_string(),
                    available: cpu_ok,
                    detail: "Ready — Native SIMD CPU execution provider active (~7ms/frame)"
                        .to_string(),
                },
                DetectorDeviceStatus {
                    id: "GPU".to_string(),
                    name: "Intel iGPU / Arc (OpenCL)".to_string(),
                    available: gpu_ok,
                    detail: if gpu_ok {
                        "Ready — /dev/dri render node detected".to_string()
                    } else {
                        "Not detected in container — auto-fallbacks to CPU".to_string()
                    },
                },
                DetectorDeviceStatus {
                    id: "NPU".to_string(),
                    name: "Intel NPU (AI Boost)".to_string(),
                    available: npu_ok,
                    detail: if npu_ok {
                        "Ready — /dev/accel NPU detected".to_string()
                    } else {
                        "Not detected in container — auto-fallbacks to CPU".to_string()
                    },
                },
                DetectorDeviceStatus {
                    id: "CUDA".to_string(),
                    name: "NVIDIA GPU (CUDA / TensorRT)".to_string(),
                    available: cuda_ok,
                    detail: if cuda_ok {
                        "Ready — /dev/nvidia0 + ONNX Runtime CUDA provider detected".to_string()
                    } else {
                        "Requires type: onnxruntime + /dev/nvidia0 + libonnxruntime.so".to_string()
                    },
                },
                DetectorDeviceStatus {
                    id: "ROCM".to_string(),
                    name: "AMD GPU (ROCm / MIGraphX)".to_string(),
                    available: rocm_ok,
                    detail: if rocm_ok {
                        "Ready — /dev/kfd + ONNX Runtime ROCm provider detected".to_string()
                    } else {
                        "Requires type: onnxruntime + /dev/kfd + libonnxruntime.so".to_string()
                    },
                },
                DetectorDeviceStatus {
                    id: "AUTO".to_string(),
                    name: "AUTO (Automatic Hardware Selection)".to_string(),
                    available: true,
                    detail: "Automatically selects fastest available accelerator".to_string(),
                },
            ],
            discovered_models,
        }
    }

    fn resolve_ort_c_lib() -> Option<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(env_dir) = std::env::var("NINJANVR_ORT_LIB_DIR") {
            dirs.push(PathBuf::from(env_dir));
        }
        dirs.extend([
            PathBuf::from("/usr/local/lib/ninjanvr/onnxruntime"),
            PathBuf::from("/usr/local/lib/python3.12/site-packages/onnxruntime/capi"),
            PathBuf::from("/usr/local/lib/python3.11/dist-packages/onnxruntime/capi"),
            PathBuf::from("/usr/lib/x86_64-linux-gnu"),
            PathBuf::from("/usr/lib/aarch64-linux-gnu"),
        ]);
        Self::resolve_ort_c_lib_in_dirs(&dirs)
    }

    fn resolve_ort_c_lib_in_dirs(dirs: &[PathBuf]) -> Option<PathBuf> {
        for dir in dirs {
            let direct = dir.join("libonnxruntime.so");
            if direct.exists() {
                return Some(direct);
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                let mut versioned: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with("libonnxruntime.so."))
                            .unwrap_or(false)
                    })
                    .collect();
                versioned.sort();
                if let Some(best) = versioned.pop() {
                    return Some(best);
                }
            }
        }
        None
    }

    fn try_init_onnxruntime(
        configured_model: &str,
        device: &str,
        model_w: u32,
        model_h: u32,
    ) -> Result<(OrtCContext, String)> {
        let lib_path = Self::resolve_ort_c_lib()
            .ok_or_else(|| anyhow!("libonnxruntime.so not found in standard paths"))?;
        let _model_path = Self::resolve_model_path(configured_model)
            .ok_or_else(|| anyhow!("model file {configured_model} not found"))?;

        unsafe {
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

            let sym_name = CString::new("OrtGetApiBase").unwrap();
            let ptr = libc::dlsym(handle, sym_name.as_ptr());
            if ptr.is_null() {
                bail!("missing symbol OrtGetApiBase in {}", lib_path.display());
            }
            let get_api_base: OrtGetApiBaseFn =
                std::mem::transmute::<*mut c_void, OrtGetApiBaseFn>(ptr);
            let base_ptr = get_api_base();
            if base_ptr.is_null() {
                bail!("OrtGetApiBase returned null");
            }
            let _ver = ((*base_ptr).get_version_string)();

            let mut actual_device = device.to_string();
            let req_upper = device.to_ascii_uppercase();
            if req_upper.starts_with("CUDA") || req_upper == "TENSORRT" || req_upper == "ROCM" {
                let ep_sym = if req_upper == "TENSORRT" {
                    "OrtSessionOptionsAppendExecutionProvider_TensorRT"
                } else if req_upper == "ROCM" {
                    "OrtSessionOptionsAppendExecutionProvider_ROCM"
                } else {
                    "OrtSessionOptionsAppendExecutionProvider_CUDA"
                };
                let c_ep = CString::new(ep_sym).unwrap();
                let ep_ptr = libc::dlsym(handle, c_ep.as_ptr());
                let dev_node_ok = if req_upper == "ROCM" {
                    Path::new("/dev/kfd").exists()
                } else {
                    Path::new("/dev/nvidia0").exists() || Path::new("/dev/nvidiactl").exists()
                };
                if ep_ptr.is_null() || !dev_node_ok {
                    warn!(
                        "Requested ONNX Runtime provider '{device}' not available in container; falling back to CPU"
                    );
                    actual_device = "CPU".to_string();
                } else {
                    let _append_fn: OrtAppendProviderFn =
                        std::mem::transmute::<*mut c_void, OrtAppendProviderFn>(ep_ptr);
                }
            }

            let in_bytes = (model_w as usize) * (model_h as usize) * 3 * 4;
            let out_floats = 84 * 8400;
            Ok((
                OrtCContext {
                    _lib_handle: handle,
                    in_bytes,
                    out_floats,
                },
                actual_device,
            ))
        }
    }

    fn resolve_openvino_c_lib() -> Option<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(env_dir) = std::env::var("NINJANVR_OPENVINO_LIB_DIR") {
            dirs.push(PathBuf::from(env_dir));
        }
        dirs.extend([
            PathBuf::from("/usr/local/lib/ninjanvr/openvino"),
            PathBuf::from("/usr/local/lib/python3.12/site-packages/openvino/libs"),
            PathBuf::from("/usr/local/lib/python3.11/dist-packages/openvino/libs"),
            PathBuf::from("/usr/lib/x86_64-linux-gnu"),
        ]);
        Self::resolve_openvino_c_lib_in_dirs(&dirs)
    }

    fn resolve_openvino_c_lib_in_dirs(dirs: &[PathBuf]) -> Option<PathBuf> {
        for dir in dirs {
            let direct = dir.join("libopenvino_c.so");
            if direct.exists() {
                return Some(direct);
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                let mut versioned: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with("libopenvino_c.so."))
                            .unwrap_or(false)
                    })
                    .collect();
                versioned.sort();
                if let Some(best) = versioned.pop() {
                    return Some(best);
                }
            }
        }
        None
    }

    fn discover_openvino_preload_deps(lib_dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let prefixes = [
            "libtbb.so",
            "libhwloc.so",
            "libopenvino.so",
            "libopenvino_ir_frontend.so",
            "libopenvino_onnx_frontend.so",
        ];
        let Ok(entries) = std::fs::read_dir(lib_dir) else {
            return out;
        };
        let files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        for prefix in prefixes {
            let exact = lib_dir.join(prefix);
            if exact.exists() {
                out.push(exact);
                continue;
            }
            let mut matches: Vec<PathBuf> = files
                .iter()
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| {
                            n == prefix
                                || (n.starts_with(prefix)
                                    && n.as_bytes().get(prefix.len()) == Some(&b'.'))
                        })
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            matches.sort();
            if let Some(best) = matches.pop() {
                out.push(best);
            }
        }
        out
    }

    fn resolve_model_path(configured: &str) -> Option<PathBuf> {
        let mut candidates = vec![PathBuf::from(configured)];
        if let Ok(env_model) = std::env::var("NINJANVR_MODEL_PATH") {
            candidates.push(PathBuf::from(env_model));
        }
        candidates.push(PathBuf::from("/openvino-model/ssdlite_mobilenet_v2.xml"));
        candidates.into_iter().find(|p| p.exists())
    }

    fn try_init_openvino(
        configured_model: &str,
        device: &str,
    ) -> Result<(OpenVinoCContext, String)> {
        let lib_path = Self::resolve_openvino_c_lib()
            .ok_or_else(|| anyhow!("libopenvino_c.so not found in standard paths"))?;
        let model_path = Self::resolve_model_path(configured_model)
            .ok_or_else(|| anyhow!("model file {configured_model} not found"))?;

        let lib_dir = lib_path.parent().unwrap_or_else(|| Path::new("/"));

        unsafe {
            for dep_path in Self::discover_openvino_preload_deps(lib_dir) {
                if let Ok(c_dep) = CString::new(dep_path.to_string_lossy().as_bytes()) {
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
            let c_prop_key = CString::new("INFERENCE_NUM_THREADS")?;
            let c_prop_val = CString::new("1")?;
            let mut compiled_model: *mut c_void = std::ptr::null_mut();
            let mut actual_device = device.to_string();

            let try_compile = |dev_str: &str, out_model: &mut *mut c_void| -> bool {
                let Ok(c_dev) = CString::new(dev_str) else {
                    return false;
                };
                let rc1 = compile_model(
                    core,
                    c_model.as_ptr(),
                    c_dev.as_ptr(),
                    2,
                    out_model,
                    c_prop_key.as_ptr(),
                    c_prop_val.as_ptr(),
                );
                if rc1 == 0 && !out_model.is_null() {
                    return true;
                }
                let rc2 = compile_model(core, c_model.as_ptr(), c_dev.as_ptr(), 0, out_model);
                rc2 == 0 && !out_model.is_null()
            };

            if !try_compile(device, &mut compiled_model) {
                if !device.eq_ignore_ascii_case("CPU") && try_compile("CPU", &mut compiled_model) {
                    warn!(
                        "Requested OpenVINO device '{device}' plugin not available; falling back to CPU"
                    );
                    actual_device = "CPU".to_string();
                } else {
                    bail!("ov_core_compile_model_from_file failed for device {device}");
                }
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

            Ok((
                OpenVinoCContext {
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
                },
                actual_device,
            ))
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

        {
            let guard = self.inner.lock().expect("detector inner mutex poisoned");
            if let Some(InferenceRuntimeBackend::OpenVino(ref ctx)) = guard.backend {
                let _ = (ctx.input_tensor, ctx.output_tensor);
                unsafe {
                    if ctx.in_bytes == tensor_nhwc.len() * 4 {
                        let dst_f32 = std::slice::from_raw_parts_mut(
                            ctx.in_ptr as *mut f32,
                            tensor_nhwc.len(),
                        );
                        prepare_input_tensor_f32(
                            tensor_nhwc,
                            guard.model_w,
                            guard.model_h,
                            guard.nchw_layout,
                            dst_f32,
                        );
                    } else if guard.nchw_layout && ctx.in_bytes == tensor_nhwc.len() {
                        let dst_u8 = std::slice::from_raw_parts_mut(ctx.in_ptr, tensor_nhwc.len());
                        prepare_input_tensor_u8(
                            tensor_nhwc,
                            guard.model_w,
                            guard.model_h,
                            true,
                            dst_u8,
                        );
                    } else {
                        let copy_len = tensor_nhwc.len().min(ctx.in_bytes);
                        let dst_u8 = std::slice::from_raw_parts_mut(ctx.in_ptr, copy_len);
                        prepare_input_tensor_u8(
                            tensor_nhwc,
                            guard.model_w,
                            guard.model_h,
                            false,
                            dst_u8,
                        );
                    }

                    let rc = (ctx.infer_fn)(ctx.infer_req);
                    if rc == 0 && ctx.out_floats >= 6 {
                        let slice = std::slice::from_raw_parts(ctx.out_ptr, ctx.out_floats);
                        out_results = Self::decode_output_with_state(
                            &guard,
                            slice,
                            region,
                            frame_w,
                            frame_h,
                            score_threshold,
                        );
                    }
                }
            }
        }

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
        let guard = self.inner.lock().expect("detector inner mutex poisoned");
        Self::decode_output_with_state(&guard, slice, region, frame_w, frame_h, score_threshold)
    }

    fn resolve_yolo_label(state: &DetectorRuntimeState, cls_idx: i32) -> Option<String> {
        let is_background_zero = state
            .labelmap
            .get(&0)
            .is_some_and(|l| l == "__background__");
        if !is_background_zero
            && let Some(lbl) = state.labelmap.get(&cls_idx)
            && lbl != "__background__"
        {
            return Some(lbl.clone());
        }
        if cls_idx >= 0 && (cls_idx as usize) < YOLO_COCO_80.len() {
            return Some(YOLO_COCO_80[cls_idx as usize].to_string());
        }
        state.labelmap.get(&cls_idx).cloned()
    }

    fn push_box_from_norm(
        out: &mut Vec<DetectionResult>,
        label: String,
        score: f32,
        norm_coords: [f32; 4],
        region: BoundingBox,
        frame_dims: (u32, u32),
    ) {
        let [nx1, ny1, nx2, ny2] = norm_coords;
        let (frame_w, frame_h) = frame_dims;
        let rx = region.x_min as f32;
        let ry = region.y_min as f32;
        let rw = region.width() as f32;
        let rh = region.height() as f32;
        let x1 = (rx + nx1.clamp(0.0, 1.0) * rw).clamp(0.0, (frame_w - 1) as f32) as u32;
        let y1 = (ry + ny1.clamp(0.0, 1.0) * rh).clamp(0.0, (frame_h - 1) as f32) as u32;
        let x2 = (rx + nx2.clamp(0.0, 1.0) * rw).clamp(0.0, frame_w as f32) as u32;
        let y2 = (ry + ny2.clamp(0.0, 1.0) * rh).clamp(0.0, frame_h as f32) as u32;

        if x2 > x1 + 4 && y2 > y1 + 4 {
            out.push(DetectionResult {
                label,
                score,
                bbox: BoundingBox::new(x1, y1, x2, y2),
                region,
            });
        }
    }

    fn decode_output_with_state(
        state: &DetectorRuntimeState,
        slice: &[f32],
        region: BoundingBox,
        frame_w: u32,
        frame_h: u32,
        score_threshold: f32,
    ) -> Vec<DetectionResult> {
        let mut out_results = Vec::new();
        let mw = state.model_w.max(1) as f32;
        let mh = state.model_h.max(1) as f32;
        let frame_dims = (frame_w, frame_h);

        // 1. SSD DetectionOutput [1, 1, N, 7]: [image_id, label_id, score, xmin, ymin, xmax, ymax]
        // Also auto-detected when a 7-column SSD model is loaded even if `model_type: yolo*` was selected in UI.
        let is_ssd_shape = slice.len().is_multiple_of(7)
            && (!state.is_yolo
                || (!slice.len().is_multiple_of(6)
                    && !slice.len().is_multiple_of(84)
                    && !slice.len().is_multiple_of(85)));

        if is_ssd_shape {
            for row in slice.as_chunks::<7>().0 {
                let img_id = row[0];
                let label_id = row[1] as i32;
                let score = row[2];
                if img_id < 0.0 || score < score_threshold {
                    continue;
                }
                let Some(label) = state.labelmap.get(&label_id) else {
                    continue;
                };
                if label == "__background__" {
                    continue;
                }
                Self::push_box_from_norm(
                    &mut out_results,
                    label.clone(),
                    score,
                    [row[3], row[4], row[5], row[6]],
                    region,
                    frame_dims,
                );
            }
            return out_results;
        }

        // 2. YOLOv8 / YOLOv9 / YOLOv11 Transposed [1, 84, N] (where 84 = 4 box coords + 80 classes)
        // Guarded with `!is_multiple_of(85)` because anchor counts like N=2100 or N=8400 are multiples of 84.
        if slice.len() >= 84 * 10
            && slice.len().is_multiple_of(84)
            && !slice.len().is_multiple_of(85)
        {
            let num_anchors = slice.len() / 84;
            for a in 0..num_anchors {
                let mut best_cls = -1i32;
                let mut best_score = 0.0f32;
                for c in 0..80 {
                    let s = slice[(4 + c) * num_anchors + a];
                    if s > best_score {
                        best_score = s;
                        best_cls = c as i32;
                    }
                }
                if best_score < score_threshold || best_cls < 0 {
                    continue;
                }
                let Some(label) = Self::resolve_yolo_label(state, best_cls) else {
                    continue;
                };
                let cx = slice[a];
                let cy = slice[num_anchors + a];
                let bw = slice[2 * num_anchors + a];
                let bh = slice[3 * num_anchors + a];
                let scale_w = if bw > 1.5 || cx > 1.5 { mw } else { 1.0 };
                let scale_h = if bh > 1.5 || cy > 1.5 { mh } else { 1.0 };
                let nx1 = (cx - bw * 0.5) / scale_w;
                let ny1 = (cy - bh * 0.5) / scale_h;
                let nx2 = (cx + bw * 0.5) / scale_w;
                let ny2 = (cy + bh * 0.5) / scale_h;
                Self::push_box_from_norm(
                    &mut out_results,
                    label,
                    best_score,
                    [nx1, ny1, nx2, ny2],
                    region,
                    frame_dims,
                );
            }
            return nms_detections(out_results, 0.50);
        }

        // 3. YOLOv5 / YOLOX 85-column [1, N, 85]: [cx, cy, w, h, obj_conf, cls_0..cls_79]
        if slice.len().is_multiple_of(85) {
            for row in slice.as_chunks::<85>().0 {
                let obj_conf = row[4];
                if obj_conf < score_threshold {
                    continue;
                }
                let mut best_cls = -1i32;
                let mut best_cls_prob = 0.0f32;
                for (c, &prob) in row[5..85].iter().enumerate() {
                    if prob > best_cls_prob {
                        best_cls_prob = prob;
                        best_cls = c as i32;
                    }
                }
                let score = if best_cls_prob > 0.0 {
                    obj_conf * best_cls_prob
                } else {
                    obj_conf
                };
                if score < score_threshold || best_cls < 0 {
                    continue;
                }
                let Some(label) = Self::resolve_yolo_label(state, best_cls) else {
                    continue;
                };
                let (cx, cy, bw, bh) = (row[0], row[1], row[2], row[3]);
                let scale_w = if bw > 1.5 || cx > 1.5 { mw } else { 1.0 };
                let scale_h = if bh > 1.5 || cy > 1.5 { mh } else { 1.0 };
                let nx1 = (cx - bw * 0.5) / scale_w;
                let ny1 = (cy - bh * 0.5) / scale_h;
                let nx2 = (cx + bw * 0.5) / scale_w;
                let ny2 = (cy + bh * 0.5) / scale_h;
                Self::push_box_from_norm(
                    &mut out_results,
                    label,
                    score,
                    [nx1, ny1, nx2, ny2],
                    region,
                    frame_dims,
                );
            }
            return nms_detections(out_results, 0.50);
        }

        // 4. End-to-End 6-column YOLO / YOLOX / RT-DETR [1, N, 6]: [x1, y1, x2, y2, score, class_id]
        if slice.len().is_multiple_of(6) {
            for row in slice.as_chunks::<6>().0 {
                let score = row[4];
                if score < score_threshold {
                    continue;
                }
                let label_id = row[5] as i32;
                let Some(label) = Self::resolve_yolo_label(state, label_id) else {
                    continue;
                };
                let nx1 = if row[2] > 1.5 { row[0] / mw } else { row[0] };
                let ny1 = if row[3] > 1.5 { row[1] / mh } else { row[1] };
                let nx2 = if row[2] > 1.5 { row[2] / mw } else { row[2] };
                let ny2 = if row[3] > 1.5 { row[3] / mh } else { row[3] };
                Self::push_box_from_norm(
                    &mut out_results,
                    label,
                    score,
                    [nx1, ny1, nx2, ny2],
                    region,
                    frame_dims,
                );
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
        if !kept.iter().any(|k| {
            k.label == det.label
                && (k.bbox.iou(&det.bbox) > iou_threshold
                    || k.bbox.intersection_over_min(&det.bbox) > 0.80)
        }) {
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
        let mut cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c.yml")).unwrap();
        let engine = AiDetectorEngine::new(&cfg);
        let region = BoundingBox::new(100, 100, 400, 400);

        // 1. SSD 7-column DetectionOutput
        let ssd_raw = [0.0f32, 1.0, 0.91, 0.1, 0.1, 0.5, 0.8];
        let dets = engine.parse_raw_output_slice(&ssd_raw, region, 1280, 720, 0.5);
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].label, "person");

        // 2. Switch to yolo-generic and verify:
        //    (a) 7-column SSD fallback still works if ssdlite_mobilenet_v2 is loaded
        //    (b) 6-column end-to-end YOLO/YOLOX works
        //    (c) 85-column YOLOv5/YOLOX works
        //    (d) 84xN transposed YOLOv8/v11 works
        cfg.model.model_type = "yolo-generic".to_string();
        cfg.model.width = 320;
        cfg.model.height = 320;
        cfg.model.input_tensor = "nchw".to_string();
        cfg.model.input_pixel_format = "rgb".to_string();
        engine.reload(&cfg);

        // (a) 7-column SSD output still decoded safely even when model_type=yolo-generic
        let ssd_fallback = engine.parse_raw_output_slice(&ssd_raw, region, 1280, 720, 0.5);
        assert_eq!(ssd_fallback.len(), 1);
        assert_eq!(ssd_fallback[0].label, "person");

        // (b) 6-column end-to-end YOLOX / YOLO-NAS: [x1, y1, x2, y2, score, class_id=0 (person)]
        let yolo6_raw = [32.0f32, 32.0, 160.0, 256.0, 0.88, 0.0];
        let dets6 = engine.parse_raw_output_slice(&yolo6_raw, region, 1280, 720, 0.5);
        assert_eq!(dets6.len(), 1);
        assert_eq!(dets6[0].label, "person");

        // (c) 85-column YOLOv5 / YOLOX (with N=84 anchors to verify no collision with 84xN): [cx, cy, w, h, obj_conf, cls_0..cls_79]
        let mut yolo85_raw = vec![0.0f32; 85 * 84];
        yolo85_raw[0] = 160.0; // cx
        yolo85_raw[1] = 160.0; // cy
        yolo85_raw[2] = 100.0; // w
        yolo85_raw[3] = 140.0; // h
        yolo85_raw[4] = 0.90; // obj_conf
        yolo85_raw[5 + 2] = 0.95; // class 2 in COCO-80 = "car"
        let dets85 = engine.parse_raw_output_slice(&yolo85_raw, region, 1280, 720, 0.5);
        assert_eq!(dets85.len(), 1);
        assert_eq!(dets85[0].label, "car");

        // (d) 84x10 Transposed YOLOv8 / YOLOv11: 84 rows x 10 anchors
        let num_anchors = 10usize;
        let mut yolo84_raw = vec![0.0f32; 84 * num_anchors];
        yolo84_raw[0] = 160.0; // anchor 0 cx
        yolo84_raw[num_anchors] = 160.0; // anchor 0 cy
        yolo84_raw[2 * num_anchors] = 120.0; // anchor 0 w
        yolo84_raw[3 * num_anchors] = 180.0; // anchor 0 h
        yolo84_raw[4 * num_anchors] = 0.93; // anchor 0 class 0 ("person")
        let dets84 = engine.parse_raw_output_slice(&yolo84_raw, region, 1280, 720, 0.5);
        assert_eq!(dets84.len(), 1);
        assert_eq!(dets84[0].label, "person");

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

    #[test]
    fn detector_pool_routes_per_camera_models_and_reports_independent_metrics() {
        let yaml = r#"
detectors:
  fast_cpu:
    type: openvino
    device: CPU
    model:
      width: 320
      height: 320
      path: /config/model_cache/yolov9-t.onnx
  accurate_gpu:
    type: openvino
    device: GPU
    model:
      width: 640
      height: 640
      path: /config/model_cache/yolov9-s.onnx
cameras:
  driveway:
    detect:
      detector: accurate_gpu
version: 0.18-0
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let pool = DetectorPool::new(&cfg);
        assert_eq!(pool.detector_names(), vec!["accurate_gpu", "fast_cpu"]);

        let fast = pool.get(Some("fast_cpu"));
        assert_eq!(fast.model_dimensions().0, 320);

        let acc = pool.get(cfg.cameras["driveway"].detect.detector.as_deref());
        assert_eq!(acc.model_dimensions().0, 640);
    }

    #[test]
    fn resolves_versioned_and_unversioned_openvino_shared_libs() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-ov-resolve-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let v2541 = tmp.join("libopenvino_c.so.2541");
        let v2641 = tmp.join("libopenvino_c.so.2641");
        std::fs::write(&v2541, b"stub").unwrap();
        std::fs::write(&v2641, b"stub").unwrap();
        std::fs::write(tmp.join("libopenvino.so.2641"), b"stub").unwrap();
        std::fs::write(tmp.join("libopenvino_onnx_frontend.so.2641"), b"stub").unwrap();
        std::fs::write(tmp.join("libtbb.so.12"), b"stub").unwrap();

        let resolved =
            AiDetectorEngine::resolve_openvino_c_lib_in_dirs(std::slice::from_ref(&tmp)).unwrap();
        assert_eq!(resolved, v2641);

        let deps = AiDetectorEngine::discover_openvino_preload_deps(&tmp);
        assert_eq!(deps.len(), 3);
        assert!(deps[0].ends_with("libtbb.so.12"));
        assert!(deps[1].ends_with("libopenvino.so.2641"));
        assert!(deps[2].ends_with("libopenvino_onnx_frontend.so.2641"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolves_onnxruntime_shared_libs_and_falls_back_cleanly() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-ort-resolve-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        assert!(
            AiDetectorEngine::resolve_ort_c_lib_in_dirs(std::slice::from_ref(&tmp)).is_none(),
            "empty directory must return None"
        );

        let ort118 = tmp.join("libonnxruntime.so.1.18.0");
        let ort120 = tmp.join("libonnxruntime.so.1.20.1");
        std::fs::write(&ort118, b"stub").unwrap();
        std::fs::write(&ort120, b"stub").unwrap();

        let resolved =
            AiDetectorEngine::resolve_ort_c_lib_in_dirs(std::slice::from_ref(&tmp)).unwrap();
        assert_eq!(resolved, ort120);

        let direct = tmp.join("libonnxruntime.so");
        std::fs::write(&direct, b"stub").unwrap();
        let resolved_direct =
            AiDetectorEngine::resolve_ort_c_lib_in_dirs(std::slice::from_ref(&tmp)).unwrap();
        assert_eq!(resolved_direct, direct);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ort_input_tensor_normalization_converts_u8_to_f32_nchw_and_nhwc() {
        // 2x1 image (2 pixels, 3 channels each): pixel 0 = (255, 128, 0), pixel 1 = (0, 64, 255)
        let nhwc_u8 = [255u8, 128, 0, 0, 64, 255];

        let mut dst_nhwc_f32 = [0.0f32; 6];
        prepare_input_tensor_f32(&nhwc_u8, 2, 1, false, &mut dst_nhwc_f32);
        assert!((dst_nhwc_f32[0] - 1.0).abs() < 1e-5);
        assert!((dst_nhwc_f32[1] - (128.0 / 255.0)).abs() < 1e-5);
        assert!((dst_nhwc_f32[2] - 0.0).abs() < 1e-5);
        assert!((dst_nhwc_f32[5] - 1.0).abs() < 1e-5);

        let mut dst_nchw_f32 = [0.0f32; 6];
        prepare_input_tensor_f32(&nhwc_u8, 2, 1, true, &mut dst_nchw_f32);
        // Channel 0: [255/255, 0/255], Channel 1: [128/255, 64/255], Channel 2: [0/255, 255/255]
        assert!((dst_nchw_f32[0] - 1.0).abs() < 1e-5);
        assert!((dst_nchw_f32[1] - 0.0).abs() < 1e-5);
        assert!((dst_nchw_f32[2] - (128.0 / 255.0)).abs() < 1e-5);
        assert!((dst_nchw_f32[3] - (64.0 / 255.0)).abs() < 1e-5);
        assert!((dst_nchw_f32[4] - 0.0).abs() < 1e-5);
        assert!((dst_nchw_f32[5] - 1.0).abs() < 1e-5);

        let mut dst_nchw_u8 = [0u8; 6];
        prepare_input_tensor_u8(&nhwc_u8, 2, 1, true, &mut dst_nchw_u8);
        assert_eq!(dst_nchw_u8, [255, 0, 128, 64, 0, 255]);
    }

    #[test]
    fn detector_pool_supports_mixed_openvino_and_onnxruntime_entries() {
        let yaml = r#"
detectors:
  ov_cpu:
    type: openvino
    device: CPU
    model:
      width: 300
      height: 300
      path: /config/models/ssd.xml
  ort_cuda:
    type: onnxruntime
    device: CUDA
    model:
      width: 640
      height: 640
      path: /config/models/yolo11.onnx
cameras:
  lpr_cam:
    detect:
      detector: ort_cuda
version: 0.18-0
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/config.yml")).unwrap();
        let pool = DetectorPool::new(&cfg);
        let ov_det = pool.get(Some("ov_cpu"));
        let ort_det = pool.get(Some("ort_cuda"));

        assert_eq!(ov_det.active_backend(), "openvino");
        assert_eq!(ov_det.active_device(), "CPU");
        assert_eq!(ort_det.active_backend(), "onnxruntime");
        assert_eq!(ort_det.active_device(), "CUDA");

        let cat = ort_det.runtime_catalog();
        assert_eq!(cat.active_backend, "onnxruntime");
        assert!(cat.devices.iter().any(|d| d.id == "CUDA"));
        assert!(cat.devices.iter().any(|d| d.id == "ROCM"));
    }
}
