use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use ninjanvr_config::{DetectorEntryConfig, LprConfig, NinjaConfig};
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

/// Computes the standard multi-scale anchor count for stride-(8, 16, 32) YOLO heads:
/// `(W/8)*(H/8) + (W/16)*(H/16) + (W/32)*(H/32)` (e.g., `2100` at `320x320`, `8400` at `640x640`, `3549` at `416x416`).
pub fn expected_yolo_anchors(model_w: u32, model_h: u32) -> usize {
    let w = model_w as usize;
    let h = model_h as usize;
    (w / 8)
        .saturating_mul(h / 8)
        .saturating_add((w / 16).saturating_mul(h / 16))
        .saturating_add((w / 32).saturating_mul(h / 32))
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
    let plane_len = (model_w as usize).saturating_mul(model_h as usize);
    let expected_len = plane_len.saturating_mul(3);
    if nchw_layout
        && plane_len > 0
        && tensor_nhwc.len() >= expected_len
        && dst_f32.len() >= expected_len
    {
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
    let plane_len = (model_w as usize).saturating_mul(model_h as usize);
    let expected_len = plane_len.saturating_mul(3);
    if nchw_layout
        && plane_len > 0
        && tensor_nhwc.len() >= expected_len
        && dst_u8.len() >= expected_len
    {
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

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        warn!("Recovered poisoned mutex in ninjanvr-infer");
        poisoned.into_inner()
    })
}

const MAX_MODEL_DIMENSION: u32 = 4096;
const MAX_TENSOR_FLOATS: usize = 16 * 1024 * 1024;
const MAX_TENSOR_BYTES: usize = MAX_TENSOR_FLOATS * std::mem::size_of::<f32>();

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
type OvFreeFn = unsafe extern "C" fn(*mut c_void);

#[derive(Clone, Copy)]
struct OvCFunctionTable {
    core_create: OvCoreCreateFn,
    compile_model: OvCoreCompileModelFn,
    create_infer_req: OvCompiledModelCreateInferReqFn,
    get_in_tensor: OvInferReqGetTensorFn,
    get_out_tensor: OvInferReqGetTensorFn,
    infer_fn: OvInferReqInferFn,
    tensor_data: OvTensorDataFn,
    tensor_byte_size: OvTensorGetByteSizeFn,
    tensor_free: Option<OvFreeFn>,
    infer_req_free: Option<OvFreeFn>,
    compiled_model_free: Option<OvFreeFn>,
    core_free: Option<OvFreeFn>,
}

#[repr(C)]
struct OrtApiBase {
    get_api: unsafe extern "C" fn(u32) -> *const c_void,
    get_version_string: unsafe extern "C" fn() -> *const c_char,
}

type OrtGetApiBaseFn = unsafe extern "C" fn() -> *const OrtApiBase;
type OrtAppendProviderFn = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void;

type OrtGetErrorMessageFn = unsafe extern "C" fn(*const c_void) -> *const c_char;
type OrtReleaseFn = unsafe extern "C" fn(*mut c_void);
type OrtCreateEnvFn = unsafe extern "C" fn(c_int, *const c_char, *mut *mut c_void) -> *mut c_void;
type OrtCreateSessionOptionsFn = unsafe extern "C" fn(*mut *mut c_void) -> *mut c_void;
type OrtSetThreadsFn = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void;
type OrtSetOptLevelFn = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void;
type OrtCreateSessionFn = unsafe extern "C" fn(
    *const c_void,
    *const c_char,
    *const c_void,
    *mut *mut c_void,
) -> *mut c_void;
type OrtSessionGetTypeInfoFn =
    unsafe extern "C" fn(*const c_void, usize, *mut *mut c_void) -> *mut c_void;
type OrtSessionGetNameFn =
    unsafe extern "C" fn(*const c_void, usize, *mut c_void, *mut *mut c_char) -> *mut c_void;
type OrtCastTypeInfoToTensorInfoFn =
    unsafe extern "C" fn(*const c_void, *mut *const c_void) -> *mut c_void;
type OrtGetTensorElementTypeFn = unsafe extern "C" fn(*const c_void, *mut c_int) -> *mut c_void;
type OrtGetDimensionsCountFn = unsafe extern "C" fn(*const c_void, *mut usize) -> *mut c_void;
type OrtGetDimensionsFn = unsafe extern "C" fn(*const c_void, *mut i64, usize) -> *mut c_void;
type OrtAllocatorFreeFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type OrtGetAllocatorFn = unsafe extern "C" fn(*mut *mut c_void) -> *mut c_void;
type OrtCreateCpuMemoryInfoFn = unsafe extern "C" fn(c_int, c_int, *mut *mut c_void) -> *mut c_void;
type OrtCreateTensorWithDataFn = unsafe extern "C" fn(
    *const c_void,
    *mut c_void,
    usize,
    *const i64,
    usize,
    c_int,
    *mut *mut c_void,
) -> *mut c_void;
type OrtRunFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_void,
    *const *const c_char,
    *const *const c_void,
    usize,
    *const *const c_char,
    usize,
    *mut *mut c_void,
) -> *mut c_void;
type OrtGetTensorMutableDataFn = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> *mut c_void;
type OrtGetTensorTypeAndShapeFn =
    unsafe extern "C" fn(*const c_void, *mut *mut c_void) -> *mut c_void;
type OrtGetTensorShapeElementCountFn =
    unsafe extern "C" fn(*const c_void, *mut usize) -> *mut c_void;

const ORT_TENSOR_ELEMENT_DATA_TYPE_FLOAT: c_int = 1;
const ORT_TENSOR_ELEMENT_DATA_TYPE_UINT8: c_int = 2;
const ORT_GRAPH_OPT_ENABLE_ALL: c_int = 99;

/// OpenVINO C-API runtime context.
/// Fields are ordered in strict reverse creation order (`output_tensor` -> `input_tensor` ->
/// `infer_req` -> `compiled_model` -> `core` -> `_lib_handle`), and `Drop` explicitly releases
/// child handles before parent handles.
struct OpenVinoCContext {
    output_tensor: *mut c_void,
    input_tensor: *mut c_void,
    infer_req: *mut c_void,
    compiled_model: *mut c_void,
    core: *mut c_void,
    _lib_handle: *mut c_void,
    in_ptr: *mut u8,
    in_bytes: usize,
    out_ptr: *const f32,
    out_floats: usize,
    infer_fn: OvInferReqInferFn,
    tensor_free_fn: Option<OvFreeFn>,
    infer_req_free_fn: Option<OvFreeFn>,
    compiled_model_free_fn: Option<OvFreeFn>,
    core_free_fn: Option<OvFreeFn>,
}

unsafe impl Send for OpenVinoCContext {}

impl Drop for OpenVinoCContext {
    fn drop(&mut self) {
        unsafe {
            if !self.output_tensor.is_null() {
                if let Some(free_fn) = self.tensor_free_fn {
                    free_fn(self.output_tensor);
                }
                if self.input_tensor == self.output_tensor {
                    self.input_tensor = std::ptr::null_mut();
                }
                self.output_tensor = std::ptr::null_mut();
            }
            if !self.input_tensor.is_null() {
                if let Some(free_fn) = self.tensor_free_fn {
                    free_fn(self.input_tensor);
                }
                self.input_tensor = std::ptr::null_mut();
            }
            if !self.infer_req.is_null() {
                if let Some(free_fn) = self.infer_req_free_fn {
                    free_fn(self.infer_req);
                }
                self.infer_req = std::ptr::null_mut();
            }
            if !self.compiled_model.is_null() {
                if let Some(free_fn) = self.compiled_model_free_fn {
                    free_fn(self.compiled_model);
                }
                self.compiled_model = std::ptr::null_mut();
            }
            if !self.core.is_null() {
                if let Some(free_fn) = self.core_free_fn {
                    free_fn(self.core);
                }
                self.core = std::ptr::null_mut();
            }
        }
    }
}

/// ONNX Runtime C-API runtime context.
/// Fields are ordered in strict reverse creation order (`mem_info` -> `session` ->
/// `session_opts` -> `env` -> `vtable` -> `_lib_handle`), and `Drop` explicitly releases
/// child handles before parent handles.
struct OrtCContext {
    mem_info: *mut c_void,
    session: *mut c_void,
    session_opts: *mut c_void,
    env: *mut c_void,
    vtable: *const *const c_void,
    _lib_handle: *mut c_void,
    input_name: CString,
    output_name: CString,
    input_is_u8: bool,
    in_f32_buf: Vec<f32>,
    in_u8_buf: Vec<u8>,
    in_bytes: usize,
    out_floats: usize,
}

unsafe impl Send for OrtCContext {}

impl Drop for OrtCContext {
    fn drop(&mut self) {
        if self.vtable.is_null() {
            return;
        }
        unsafe {
            let vt = std::slice::from_raw_parts(self.vtable, 101);
            let release = |idx: usize, ptr: &mut *mut c_void| {
                if !(*ptr).is_null() && !vt[idx].is_null() {
                    let f: OrtReleaseFn =
                        std::mem::transmute::<*const c_void, OrtReleaseFn>(vt[idx]);
                    f(*ptr);
                }
                *ptr = std::ptr::null_mut();
            };
            release(94, &mut self.mem_info);
            release(95, &mut self.session);
            release(100, &mut self.session_opts);
            release(92, &mut self.env);
        }
    }
}

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
    face_gallery: Arc<Mutex<FaceGallery>>,
    lpr_voter: Mutex<LprTrackVoter>,
}

/// Registry of named `AiDetectorEngine` instances supporting multi-detector routing per camera.
pub struct DetectorPool {
    default_detector: Arc<AiDetectorEngine>,
    detectors: BTreeMap<String, Arc<AiDetectorEngine>>,
}

impl DetectorPool {
    pub fn new(config: &NinjaConfig) -> Self {
        let default_detector = Arc::new(AiDetectorEngine::new(config));
        let detectors = lock_unpoisoned(&default_detector.pool).clone();
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
            face_gallery: Arc::new(Mutex::new(FaceGallery::new())),
            lpr_voter: Mutex::new(LprTrackVoter::new(12)),
        }
    }

    pub fn new_for_entry(config: &NinjaConfig, entry: &DetectorEntryConfig) -> Self {
        let state = Self::build_runtime_state(config, Some(entry));
        Self {
            inner: Mutex::new(state),
            last_inference_ms: Mutex::new(2.1),
            cumulative_infer_us: AtomicU64::new(0),
            pool: Mutex::new(BTreeMap::new()),
            face_gallery: Arc::new(Mutex::new(FaceGallery::new())),
            lpr_voter: Mutex::new(LprTrackVoter::new(12)),
        }
    }

    /// Hot-reloads the detector model, OpenVINO device, tensor layout, and labelmap when config changes.
    pub fn reload(&self, config: &NinjaConfig) {
        let next_state = Self::build_runtime_state(config, None);
        *lock_unpoisoned(&self.inner) = next_state;
        let mut pool_map = BTreeMap::new();
        for (name, entry) in &config.detectors {
            pool_map.insert(name.clone(), Arc::new(Self::new_for_entry(config, entry)));
        }
        *lock_unpoisoned(&self.pool) = pool_map;
    }

    /// Resolves a named detector from the pool if present, or returns `None` (caller uses `self`).
    pub fn named_detector(&self, name: Option<&str>) -> Option<Arc<AiDetectorEngine>> {
        let n = name?;
        lock_unpoisoned(&self.pool).get(n).cloned()
    }

    /// Returns the average inference latency (ms) for a specific named detector, falling back to the primary detector.
    pub fn avg_inference_ms_for(&self, name: &str) -> f32 {
        if let Some(det) = lock_unpoisoned(&self.pool).get(name).cloned() {
            return det.avg_inference_ms();
        }
        self.avg_inference_ms()
    }

    /// Returns the shared in-memory `FaceGallery` used for 512-d ArcFace/AdaFace cosine matching.
    pub fn face_gallery(&self) -> Arc<Mutex<FaceGallery>> {
        self.face_gallery.clone()
    }

    /// Synchronizes the in-memory `FaceGallery` with `{faces_dir}/<name>/*.{jpg,webp,png,bin}`.
    pub fn sync_face_gallery_from_dir(&self, faces_dir: &Path) -> Result<()> {
        let mut guard = lock_unpoisoned(&self.face_gallery);
        guard.sync_from_dir(faces_dir)
    }

    /// Enrolls a face crop/sample for `name` under `{faces_dir}/{name}/{sample_id}.jpg`
    /// and updates the in-memory 512-d embedding gallery.
    pub fn enroll_face_profile(
        &self,
        faces_dir: &Path,
        name: &str,
        sample_id: &str,
        image_bytes: &[u8],
    ) -> Result<(PathBuf, Vec<f32>)> {
        let mut guard = lock_unpoisoned(&self.face_gallery);
        let _ = guard.sync_from_dir(faces_dir);
        guard.enroll_to_dir(faces_dir, name, sample_id, "jpg", image_bytes)
    }

    /// Removes an enrolled face profile from memory and deletes `{faces_dir}/{name}` if present.
    pub fn delete_face_profile(&self, faces_dir: &Path, name: &str) -> Result<bool> {
        let mut guard = lock_unpoisoned(&self.face_gallery);
        let _ = guard.sync_from_dir(faces_dir);
        guard.remove_profile(Some(faces_dir), name)
    }

    /// Lists all enrolled face profiles from memory and `{faces_dir}`.
    pub fn list_face_profiles(&self, faces_dir: &Path) -> Vec<EnrolledFaceProfile> {
        let mut guard = lock_unpoisoned(&self.face_gallery);
        let _ = guard.sync_from_dir(faces_dir);
        guard.list_profiles(Some(faces_dir))
    }

    /// Extracts a 512-d L2-normalized embedding from `crop_bytes` and matches it against
    /// enrolled profiles in `face_gallery`.
    pub fn match_face_crop(
        &self,
        faces_dir: &Path,
        crop_bytes: &[u8],
        similarity_threshold: f32,
        unknown_alert: bool,
    ) -> Option<FaceMatchResult> {
        let mut guard = lock_unpoisoned(&self.face_gallery);
        let _ = guard.sync_from_dir(faces_dir);
        guard.match_face_bytes(crop_bytes, similarity_threshold, unknown_alert)
    }

    fn build_runtime_state(
        config: &NinjaConfig,
        entry_override: Option<&DetectorEntryConfig>,
    ) -> DetectorRuntimeState {
        let effective_model = entry_override
            .and_then(|e| e.model.as_ref())
            .unwrap_or(&config.model);

        let mut labelmap = BTreeMap::new();
        if !effective_model.labelmap_path.is_empty()
            && Path::new(&effective_model.labelmap_path).is_file()
            && let Ok(content) = std::fs::read_to_string(&effective_model.labelmap_path)
        {
            for (idx, line) in content.lines().take(10_000).enumerate() {
                let trimmed = line.trim();
                if !trimmed.is_empty()
                    && let Ok(id) = i32::try_from(idx)
                {
                    labelmap.insert(id, trimmed.to_string());
                }
            }
        }
        if labelmap.is_empty() {
            let raw_labels = config.load_labelmap();
            for (k, v) in raw_labels {
                if let Ok(id) = k.parse::<i32>() {
                    labelmap.insert(id, v);
                }
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

        let mut model_w = effective_model.width.clamp(64, MAX_MODEL_DIMENSION);
        let mut model_h = effective_model.height.clamp(64, MAX_MODEL_DIMENSION);
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

        let mt = effective_model.model_type.to_ascii_lowercase();
        let is_yolo = mt.contains("yolo") || effective_model.path.ends_with(".onnx");
        let nchw_layout = effective_model.input_tensor.eq_ignore_ascii_case("nchw");

        // Reconcile configured width/height against compiled input tensor byte size
        // so changing width/height in UI without changing the underlying .xml/.onnx model never overflows.
        if let Some(InferenceRuntimeBackend::OpenVino(ref ctx)) = backend {
            let (rec_w, rec_h) = Self::reconcile_openvino_dimensions(
                model_w,
                model_h,
                ctx.in_bytes,
                ctx.out_floats,
                is_yolo,
                nchw_layout,
                &effective_model.path,
            );
            if (rec_w, rec_h) != (model_w, model_h) {
                info!(
                    "Reconciled model input dimensions from {}x{} to compiled tensor {}x{} (in_bytes={})",
                    model_w, model_h, rec_w, rec_h, ctx.in_bytes
                );
                model_w = rec_w;
                model_h = rec_h;
            }
        }

        DetectorRuntimeState {
            model_w,
            model_h,
            bgr_order: effective_model
                .input_pixel_format
                .eq_ignore_ascii_case("bgr"),
            nchw_layout,
            is_yolo,
            active_backend,
            active_device,
            active_model_path: effective_model.path.clone(),
            labelmap,
            backend,
        }
    }

    fn reconcile_openvino_dimensions(
        model_w: u32,
        model_h: u32,
        in_bytes: usize,
        out_floats: usize,
        is_yolo: bool,
        nchw_layout: bool,
        model_path: &str,
    ) -> (u32, u32) {
        let expected_u8 = (model_w as usize)
            .saturating_mul(model_h as usize)
            .saturating_mul(3);
        let expected_f32 = expected_u8.saturating_mul(4);
        let ssd_like_output = out_floats > 0
            && out_floats.is_multiple_of(7)
            && !out_floats.is_multiple_of(84)
            && !out_floats.is_multiple_of(85)
            && !out_floats.is_multiple_of(6);
        let prefer_u8 = (!is_yolo && !nchw_layout)
            || model_path.to_ascii_lowercase().contains("ssd")
            || ssd_like_output;

        let square_side = |bytes_per_px: usize| -> Option<u32> {
            if bytes_per_px == 0 || !in_bytes.is_multiple_of(bytes_per_px) {
                return None;
            }
            let pixels = in_bytes / bytes_per_px;
            if pixels == 0 {
                return None;
            }
            let side = (pixels as f64).sqrt().round() as usize;
            if side.saturating_mul(side) == pixels
                && (64..=(MAX_MODEL_DIMENSION as usize)).contains(&side)
            {
                Some(side as u32)
            } else {
                None
            }
        };

        if prefer_u8 {
            if in_bytes == expected_u8 {
                (model_w, model_h)
            } else if let Some(side) = square_side(3) {
                (side, side)
            } else if in_bytes == expected_f32 {
                (model_w, model_h)
            } else if let Some(side) = square_side(12) {
                (side, side)
            } else {
                (model_w, model_h)
            }
        } else if in_bytes == expected_f32 {
            (model_w, model_h)
        } else if let Some(side) = square_side(12) {
            (side, side)
        } else if in_bytes == expected_u8 {
            (model_w, model_h)
        } else if let Some(side) = square_side(3) {
            (side, side)
        } else {
            (model_w, model_h)
        }
    }

    pub fn model_dimensions(&self) -> (u32, u32, bool) {
        let g = lock_unpoisoned(&self.inner);
        (g.model_w, g.model_h, g.bgr_order)
    }

    pub fn active_backend(&self) -> String {
        let g = lock_unpoisoned(&self.inner);
        g.active_backend.clone()
    }

    pub fn active_device(&self) -> String {
        let g = lock_unpoisoned(&self.inner);
        g.active_device.clone()
    }

    pub fn avg_inference_ms(&self) -> f32 {
        *lock_unpoisoned(&self.last_inference_ms)
    }

    pub fn cumulative_infer_us(&self) -> u64 {
        self.cumulative_infer_us.load(Ordering::Relaxed)
    }

    /// Records a single-frame OCR plate read for `track_id` into the engine's rolling multi-frame
    /// voter and returns the consolidated consensus result if it passes `lpr_cfg` validation.
    pub fn record_lpr_track_read(
        &self,
        track_id: &str,
        read: OcrPlateRead,
        lpr_cfg: &LprConfig,
    ) -> Option<LprConsensusResult> {
        lock_unpoisoned(&self.lpr_voter).record_read(track_id, read, lpr_cfg)
    }

    /// Returns the current consolidated LPR consensus for `track_id`, if any.
    pub fn lpr_consensus_for_track(
        &self,
        track_id: &str,
        lpr_cfg: &LprConfig,
    ) -> Option<LprConsensusResult> {
        lock_unpoisoned(&self.lpr_voter).consensus_for_track(track_id, lpr_cfg)
    }

    /// Removes buffered LPR reads for a finalized track and returns its final consensus if valid.
    pub fn finalize_lpr_track(
        &self,
        track_id: &str,
        lpr_cfg: &LprConfig,
    ) -> Option<LprConsensusResult> {
        let mut voter = lock_unpoisoned(&self.lpr_voter);
        let consensus = voter.consensus_for_track(track_id, lpr_cfg);
        voter.remove_track(track_id);
        consensus
    }

    /// Returns hardware plugin availability, discovered model files on disk, and active compiled tensor telemetry.
    pub fn runtime_catalog(&self) -> DetectorRuntimeCatalog {
        let (loaded, active_backend, active_dev, active_path, mw, mh, in_bytes, out_floats) = {
            let g = lock_unpoisoned(&self.inner);
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

    fn sort_versioned_so_paths(paths: &mut [PathBuf]) {
        let version_key = |p: &Path| -> Vec<u64> {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| s.split_once(".so."))
                .map(|(_, ver)| {
                    ver.split('.')
                        .map(|part| part.parse::<u64>().unwrap_or(0))
                        .collect()
                })
                .unwrap_or_default()
        };
        paths.sort_by(|a, b| version_key(a).cmp(&version_key(b)).then_with(|| a.cmp(b)));
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
            if direct.is_file() {
                return Some(direct);
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                let mut versioned: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file()
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.starts_with("libonnxruntime.so."))
                                .unwrap_or(false)
                    })
                    .collect();
                Self::sort_versioned_so_paths(&mut versioned);
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
        let model_path = Self::resolve_model_path(configured_model)
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

            let mut api_ptr = std::ptr::null();
            for ver in [14u32, 10, 1] {
                let p = ((*base_ptr).get_api)(ver);
                if !p.is_null() {
                    api_ptr = p;
                    break;
                }
            }
            if api_ptr.is_null() {
                bail!("OrtApiBase::GetApi returned null");
            }

            Self::init_ort_session_from_vtable(
                handle,
                api_ptr as *const *const c_void,
                &model_path,
                device,
                model_w,
                model_h,
            )
        }
    }

    unsafe fn init_ort_session_from_vtable(
        handle: *mut c_void,
        vtable: *const *const c_void,
        model_path: &Path,
        device: &str,
        model_w: u32,
        model_h: u32,
    ) -> Result<(OrtCContext, String)> {
        if vtable.is_null() {
            bail!("ONNX Runtime vtable pointer is null");
        }
        if model_w == 0
            || model_h == 0
            || model_w > MAX_MODEL_DIMENSION
            || model_h > MAX_MODEL_DIMENSION
        {
            bail!("invalid ONNX Runtime model dimensions {model_w}x{model_h}");
        }
        unsafe {
            let vt = std::slice::from_raw_parts(vtable, 101);
            const REQUIRED_SLOTS: &[usize] = &[
                3, 7, 9, 10, 23, 24, 25, 33, 36, 37, 49, 51, 55, 60, 64, 65, 69, 78, 92, 93, 94,
                95, 96, 98, 99, 100,
            ];
            for &slot in REQUIRED_SLOTS {
                if vt[slot].is_null() {
                    bail!("ONNX Runtime vtable missing required function at slot {slot}");
                }
            }

            let check_status = |st: *mut c_void, step: &str| -> Result<()> {
                if st.is_null() {
                    return Ok(());
                }
                let msg = if !vt[2].is_null() {
                    let get_msg: OrtGetErrorMessageFn =
                        std::mem::transmute::<*const c_void, OrtGetErrorMessageFn>(vt[2]);
                    let m_ptr = get_msg(st);
                    if m_ptr.is_null() {
                        "unknown ORT error".to_string()
                    } else {
                        CStr::from_ptr(m_ptr).to_string_lossy().into_owned()
                    }
                } else {
                    "ORT status error".to_string()
                };
                if !vt[93].is_null() {
                    let rel: OrtReleaseFn =
                        std::mem::transmute::<*const c_void, OrtReleaseFn>(vt[93]);
                    rel(st);
                }
                bail!("ONNX Runtime {step} failed: {msg}")
            };

            let create_env: OrtCreateEnvFn =
                std::mem::transmute::<*const c_void, OrtCreateEnvFn>(vt[3]);
            let create_opts: OrtCreateSessionOptionsFn =
                std::mem::transmute::<*const c_void, OrtCreateSessionOptionsFn>(vt[10]);
            let set_opt_level: OrtSetOptLevelFn =
                std::mem::transmute::<*const c_void, OrtSetOptLevelFn>(vt[23]);
            let set_intra_threads: OrtSetThreadsFn =
                std::mem::transmute::<*const c_void, OrtSetThreadsFn>(vt[24]);
            let set_inter_threads: OrtSetThreadsFn =
                std::mem::transmute::<*const c_void, OrtSetThreadsFn>(vt[25]);
            let create_session: OrtCreateSessionFn =
                std::mem::transmute::<*const c_void, OrtCreateSessionFn>(vt[7]);
            let get_in_type: OrtSessionGetTypeInfoFn =
                std::mem::transmute::<*const c_void, OrtSessionGetTypeInfoFn>(vt[33]);
            let get_in_name: OrtSessionGetNameFn =
                std::mem::transmute::<*const c_void, OrtSessionGetNameFn>(vt[36]);
            let get_out_name: OrtSessionGetNameFn =
                std::mem::transmute::<*const c_void, OrtSessionGetNameFn>(vt[37]);
            let cast_tensor_info: OrtCastTypeInfoToTensorInfoFn =
                std::mem::transmute::<*const c_void, OrtCastTypeInfoToTensorInfoFn>(vt[55]);
            let get_elem_type: OrtGetTensorElementTypeFn =
                std::mem::transmute::<*const c_void, OrtGetTensorElementTypeFn>(vt[60]);
            let create_cpu_mem: OrtCreateCpuMemoryInfoFn =
                std::mem::transmute::<*const c_void, OrtCreateCpuMemoryInfoFn>(vt[69]);
            let get_alloc: OrtGetAllocatorFn =
                std::mem::transmute::<*const c_void, OrtGetAllocatorFn>(vt[78]);
            let release_status: OrtReleaseFn =
                std::mem::transmute::<*const c_void, OrtReleaseFn>(vt[93]);
            let release_type_info: OrtReleaseFn =
                std::mem::transmute::<*const c_void, OrtReleaseFn>(vt[98]);

            // Construct OrtCContext upfront so its Drop implementation automatically releases
            // any created handles (mem_info -> session -> session_opts -> env) if an early error occurs.
            let mut ctx = OrtCContext {
                mem_info: std::ptr::null_mut(),
                session: std::ptr::null_mut(),
                session_opts: std::ptr::null_mut(),
                env: std::ptr::null_mut(),
                vtable,
                _lib_handle: handle,
                input_name: CString::new("images").unwrap(),
                output_name: CString::new("output0").unwrap(),
                input_is_u8: false,
                in_f32_buf: Vec::new(),
                in_u8_buf: Vec::new(),
                in_bytes: 0,
                out_floats: 84 * 8400,
            };

            let log_id = CString::new("ninjanvr").unwrap();
            let mut env_ptr: *mut c_void = std::ptr::null_mut();
            check_status(create_env(2, log_id.as_ptr(), &mut env_ptr), "CreateEnv")?;
            if env_ptr.is_null() {
                bail!("ONNX Runtime CreateEnv returned null handle");
            }
            ctx.env = env_ptr;

            let mut opts_ptr: *mut c_void = std::ptr::null_mut();
            check_status(create_opts(&mut opts_ptr), "CreateSessionOptions")?;
            if opts_ptr.is_null() {
                bail!("ONNX Runtime CreateSessionOptions returned null handle");
            }
            ctx.session_opts = opts_ptr;
            check_status(
                set_intra_threads(ctx.session_opts, 1),
                "SetIntraOpNumThreads",
            )?;
            check_status(
                set_inter_threads(ctx.session_opts, 1),
                "SetInterOpNumThreads",
            )?;
            check_status(
                set_opt_level(ctx.session_opts, ORT_GRAPH_OPT_ENABLE_ALL),
                "SetSessionGraphOptimizationLevel",
            )?;

            let mut actual_device = device.to_string();
            let req_upper = device.to_ascii_uppercase();
            if req_upper.starts_with("CUDA") || req_upper == "TENSORRT" || req_upper == "ROCM" {
                let session_opts = ctx.session_opts;
                let try_ep = |sym: &str, dev_node_ok: bool| -> bool {
                    if !dev_node_ok || handle.is_null() {
                        return false;
                    }
                    let Ok(c_ep) = CString::new(sym) else {
                        return false;
                    };
                    let ep_ptr = libc::dlsym(handle, c_ep.as_ptr());
                    if ep_ptr.is_null() {
                        return false;
                    }
                    let append_fn: OrtAppendProviderFn =
                        std::mem::transmute::<*mut c_void, OrtAppendProviderFn>(ep_ptr);
                    let st = append_fn(session_opts, 0);
                    if st.is_null() {
                        true
                    } else {
                        release_status(st);
                        false
                    }
                };
                let nv_ok =
                    Path::new("/dev/nvidia0").exists() || Path::new("/dev/nvidiactl").exists();
                let rocm_ok = Path::new("/dev/kfd").exists();
                if req_upper == "TENSORRT" {
                    if try_ep("OrtSessionOptionsAppendExecutionProvider_TensorRT", nv_ok) {
                        actual_device = "TensorRT".to_string();
                    } else if try_ep("OrtSessionOptionsAppendExecutionProvider_CUDA", nv_ok) {
                        warn!("TensorRT provider unavailable; falling back to CUDA");
                        actual_device = "CUDA".to_string();
                    } else {
                        warn!(
                            "Requested ONNX Runtime provider '{device}' not available; falling back to CPU"
                        );
                        actual_device = "CPU".to_string();
                    }
                } else if req_upper == "ROCM" {
                    if !try_ep("OrtSessionOptionsAppendExecutionProvider_ROCM", rocm_ok) {
                        warn!(
                            "Requested ONNX Runtime provider '{device}' not available; falling back to CPU"
                        );
                        actual_device = "CPU".to_string();
                    }
                } else if !try_ep("OrtSessionOptionsAppendExecutionProvider_CUDA", nv_ok) {
                    warn!(
                        "Requested ONNX Runtime provider '{device}' not available; falling back to CPU"
                    );
                    actual_device = "CPU".to_string();
                }
            }

            let c_model = CString::new(model_path.to_string_lossy().as_bytes())?;
            let mut sess_ptr: *mut c_void = std::ptr::null_mut();
            check_status(
                create_session(ctx.env, c_model.as_ptr(), ctx.session_opts, &mut sess_ptr),
                "CreateSession",
            )?;
            if sess_ptr.is_null() {
                bail!("ONNX Runtime CreateSession returned null handle");
            }
            ctx.session = sess_ptr;

            let mut alloc: *mut c_void = std::ptr::null_mut();
            check_status(get_alloc(&mut alloc), "GetAllocatorWithDefaultOptions")?;

            let free_ort_str = |ptr: *mut c_char| {
                if !ptr.is_null() && !alloc.is_null() && !vt[76].is_null() {
                    let alloc_free: OrtAllocatorFreeFn =
                        std::mem::transmute::<*const c_void, OrtAllocatorFreeFn>(vt[76]);
                    let st = alloc_free(alloc, ptr as *mut c_void);
                    if !st.is_null() {
                        release_status(st);
                    }
                }
            };

            let mut in_name_ptr: *mut c_char = std::ptr::null_mut();
            check_status(
                get_in_name(ctx.session, 0, alloc, &mut in_name_ptr),
                "SessionGetInputName",
            )?;
            ctx.input_name = if in_name_ptr.is_null() {
                CString::new("images").unwrap()
            } else {
                let owned = CStr::from_ptr(in_name_ptr).to_owned();
                free_ort_str(in_name_ptr);
                owned
            };

            let mut out_name_ptr: *mut c_char = std::ptr::null_mut();
            check_status(
                get_out_name(ctx.session, 0, alloc, &mut out_name_ptr),
                "SessionGetOutputName",
            )?;
            ctx.output_name = if out_name_ptr.is_null() {
                CString::new("output0").unwrap()
            } else {
                let owned = CStr::from_ptr(out_name_ptr).to_owned();
                free_ort_str(out_name_ptr);
                owned
            };

            let mut type_info: *mut c_void = std::ptr::null_mut();
            let ti_st = get_in_type(ctx.session, 0, &mut type_info);
            if ti_st.is_null() && !type_info.is_null() {
                let mut tensor_info: *const c_void = std::ptr::null();
                let cast_st = cast_tensor_info(type_info, &mut tensor_info);
                if cast_st.is_null() && !tensor_info.is_null() {
                    let mut elem_ty: c_int = ORT_TENSOR_ELEMENT_DATA_TYPE_FLOAT;
                    let elem_st = get_elem_type(tensor_info, &mut elem_ty);
                    if elem_st.is_null() {
                        ctx.input_is_u8 = elem_ty == ORT_TENSOR_ELEMENT_DATA_TYPE_UINT8;
                    } else {
                        release_status(elem_st);
                    }
                } else if !cast_st.is_null() {
                    release_status(cast_st);
                }
                release_type_info(type_info);
            } else {
                if !ti_st.is_null() {
                    release_status(ti_st);
                }
                if !type_info.is_null() {
                    release_type_info(type_info);
                }
            }

            let mut mem_ptr: *mut c_void = std::ptr::null_mut();
            check_status(create_cpu_mem(0, 0, &mut mem_ptr), "CreateCpuMemoryInfo")?;
            if mem_ptr.is_null() {
                bail!("ONNX Runtime CreateCpuMemoryInfo returned null handle");
            }
            ctx.mem_info = mem_ptr;

            let num_elems = (model_w as usize)
                .checked_mul(model_h as usize)
                .and_then(|px| px.checked_mul(3))
                .ok_or_else(|| anyhow!("model dimensions overflow usize"))?;
            let in_bytes = if ctx.input_is_u8 {
                num_elems
            } else {
                num_elems
                    .checked_mul(std::mem::size_of::<f32>())
                    .ok_or_else(|| anyhow!("input tensor byte size overflows usize"))?
            };
            ctx.in_f32_buf = vec![0.0f32; num_elems];
            ctx.in_u8_buf = vec![0u8; num_elems];
            ctx.in_bytes = in_bytes;

            Ok((ctx, actual_device))
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
            if direct.is_file() {
                return Some(direct);
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                let mut versioned: Vec<PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_file()
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.starts_with("libopenvino_c.so."))
                                .unwrap_or(false)
                    })
                    .collect();
                Self::sort_versioned_so_paths(&mut versioned);
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
        let files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        for prefix in prefixes {
            let exact = lib_dir.join(prefix);
            if exact.is_file() {
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
            Self::sort_versioned_so_paths(&mut matches);
            if let Some(best) = matches.pop() {
                out.push(best);
            }
        }
        out
    }

    fn resolve_model_path(configured: &str) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if !configured.trim().is_empty() {
            candidates.push(PathBuf::from(configured));
        }
        if let Ok(env_model) = std::env::var("NINJANVR_MODEL_PATH")
            && !env_model.trim().is_empty()
        {
            candidates.push(PathBuf::from(env_model));
        }
        candidates.push(PathBuf::from("/openvino-model/ssdlite_mobilenet_v2.xml"));
        candidates.into_iter().find(|p| p.is_file())
    }

    unsafe fn init_ov_context_from_fns(
        handle: *mut c_void,
        fns: OvCFunctionTable,
        model_path: &Path,
        device: &str,
    ) -> Result<(OpenVinoCContext, String)> {
        unsafe {
            let mut ctx = OpenVinoCContext {
                output_tensor: std::ptr::null_mut(),
                input_tensor: std::ptr::null_mut(),
                infer_req: std::ptr::null_mut(),
                compiled_model: std::ptr::null_mut(),
                core: std::ptr::null_mut(),
                _lib_handle: handle,
                in_ptr: std::ptr::null_mut(),
                in_bytes: 0,
                out_ptr: std::ptr::null(),
                out_floats: 0,
                infer_fn: fns.infer_fn,
                tensor_free_fn: fns.tensor_free,
                infer_req_free_fn: fns.infer_req_free,
                compiled_model_free_fn: fns.compiled_model_free,
                core_free_fn: fns.core_free,
            };

            let rc = (fns.core_create)(&mut ctx.core);
            if rc != 0 || ctx.core.is_null() {
                ctx.core = std::ptr::null_mut();
                bail!("ov_core_create returned {rc}");
            }

            let c_model = CString::new(model_path.to_string_lossy().as_bytes())?;
            let c_prop_key = CString::new("INFERENCE_NUM_THREADS")?;
            let c_prop_val = CString::new("1")?;
            let mut actual_device = device.to_string();
            let core = ctx.core;
            let compile_model = fns.compile_model;
            let compiled_model_free = fns.compiled_model_free;

            let try_compile = |dev_str: &str, out_model: &mut *mut c_void| -> bool {
                let Ok(c_dev) = CString::new(dev_str) else {
                    return false;
                };
                *out_model = std::ptr::null_mut();
                let rc1 = compile_model(
                    core,
                    c_model.as_ptr(),
                    c_dev.as_ptr(),
                    2,
                    out_model,
                    c_prop_key.as_ptr(),
                    c_prop_val.as_ptr(),
                );
                if rc1 == 0 && !(*out_model).is_null() {
                    return true;
                }
                if !(*out_model).is_null() {
                    if let Some(free_fn) = compiled_model_free {
                        free_fn(*out_model);
                    }
                    *out_model = std::ptr::null_mut();
                }
                let rc2 = compile_model(core, c_model.as_ptr(), c_dev.as_ptr(), 0, out_model);
                if rc2 == 0 && !(*out_model).is_null() {
                    return true;
                }
                if !(*out_model).is_null() {
                    if let Some(free_fn) = compiled_model_free {
                        free_fn(*out_model);
                    }
                    *out_model = std::ptr::null_mut();
                }
                false
            };

            if !try_compile(device, &mut ctx.compiled_model) {
                if !device.eq_ignore_ascii_case("CPU")
                    && try_compile("CPU", &mut ctx.compiled_model)
                {
                    warn!(
                        "Requested OpenVINO device '{device}' plugin not available; falling back to CPU"
                    );
                    actual_device = "CPU".to_string();
                } else {
                    bail!("ov_core_compile_model_from_file failed for device {device}");
                }
            }

            let rc = (fns.create_infer_req)(ctx.compiled_model, &mut ctx.infer_req);
            if rc != 0 || ctx.infer_req.is_null() {
                ctx.infer_req = std::ptr::null_mut();
                bail!("ov_compiled_model_create_infer_request returned {rc}");
            }

            let rc = (fns.get_in_tensor)(ctx.infer_req, &mut ctx.input_tensor);
            if rc != 0 || ctx.input_tensor.is_null() {
                ctx.input_tensor = std::ptr::null_mut();
                bail!("ov_infer_request_get_input_tensor returned {rc}");
            }

            let mut in_raw: *mut c_void = std::ptr::null_mut();
            let rc = (fns.tensor_data)(ctx.input_tensor, &mut in_raw);
            if rc != 0 || in_raw.is_null() {
                bail!("ov_tensor_data(input) returned {rc}");
            }
            let mut in_bytes: usize = 0;
            let rc = (fns.tensor_byte_size)(ctx.input_tensor, &mut in_bytes);
            if rc != 0 || in_bytes == 0 || in_bytes > MAX_TENSOR_BYTES {
                bail!("ov_tensor_get_byte_size(input) returned rc={rc}, bytes={in_bytes}");
            }

            let rc = (fns.infer_fn)(ctx.infer_req);
            if rc != 0 {
                bail!("ov_infer_request_infer warmup returned {rc}");
            }

            let rc = (fns.get_out_tensor)(ctx.infer_req, &mut ctx.output_tensor);
            if rc != 0 || ctx.output_tensor.is_null() {
                ctx.output_tensor = std::ptr::null_mut();
                bail!("ov_infer_request_get_output_tensor returned {rc}");
            }

            let mut out_raw: *mut c_void = std::ptr::null_mut();
            let rc = (fns.tensor_data)(ctx.output_tensor, &mut out_raw);
            if rc != 0 || out_raw.is_null() {
                bail!("ov_tensor_data(output) returned {rc}");
            }
            let mut out_bytes: usize = 0;
            let rc = (fns.tensor_byte_size)(ctx.output_tensor, &mut out_bytes);
            if rc != 0
                || out_bytes < 6 * std::mem::size_of::<f32>()
                || out_bytes > MAX_TENSOR_BYTES
                || !out_bytes.is_multiple_of(std::mem::size_of::<f32>())
            {
                bail!("ov_tensor_get_byte_size(output) returned rc={rc}, bytes={out_bytes}");
            }

            ctx.in_ptr = in_raw as *mut u8;
            ctx.in_bytes = in_bytes;
            ctx.out_ptr = out_raw as *const f32;
            ctx.out_floats = out_bytes / std::mem::size_of::<f32>();

            Ok((ctx, actual_device))
        }
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

            macro_rules! load_opt_sym {
                ($name:literal, $ty:ty) => {{
                    let sym_name = CString::new($name).unwrap();
                    let ptr = libc::dlsym(handle, sym_name.as_ptr());
                    if ptr.is_null() {
                        None
                    } else {
                        Some(std::mem::transmute::<*mut c_void, $ty>(ptr))
                    }
                }};
            }

            let fns = OvCFunctionTable {
                core_create: load_sym!("ov_core_create", OvCoreCreateFn),
                compile_model: load_sym!("ov_core_compile_model_from_file", OvCoreCompileModelFn),
                create_infer_req: load_sym!(
                    "ov_compiled_model_create_infer_request",
                    OvCompiledModelCreateInferReqFn
                ),
                get_in_tensor: load_sym!(
                    "ov_infer_request_get_input_tensor",
                    OvInferReqGetTensorFn
                ),
                get_out_tensor: load_sym!(
                    "ov_infer_request_get_output_tensor",
                    OvInferReqGetTensorFn
                ),
                infer_fn: load_sym!("ov_infer_request_infer", OvInferReqInferFn),
                tensor_data: load_sym!("ov_tensor_data", OvTensorDataFn),
                tensor_byte_size: load_sym!("ov_tensor_get_byte_size", OvTensorGetByteSizeFn),
                tensor_free: load_opt_sym!("ov_tensor_free", OvFreeFn),
                infer_req_free: load_opt_sym!("ov_infer_request_free", OvFreeFn),
                compiled_model_free: load_opt_sym!("ov_compiled_model_free", OvFreeFn),
                core_free: load_opt_sym!("ov_core_free", OvFreeFn),
            };

            Self::init_ov_context_from_fns(handle, fns, &model_path, device)
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
            let mut guard = lock_unpoisoned(&self.inner);
            let model_w = guard.model_w;
            let model_h = guard.model_h;
            let nchw_layout = guard.nchw_layout;
            let expected_elems = (model_w as usize)
                .saturating_mul(model_h as usize)
                .saturating_mul(3);

            if expected_elems > 0
                && expected_elems <= MAX_TENSOR_FLOATS
                && tensor_nhwc.len() >= expected_elems
            {
                let tensor_nhwc = &tensor_nhwc[..expected_elems];

                match guard.backend.as_mut() {
                    Some(InferenceRuntimeBackend::OpenVino(ctx)) => {
                        let _ = (ctx.input_tensor, ctx.output_tensor);
                        let expected_f32_bytes =
                            expected_elems.checked_mul(std::mem::size_of::<f32>());
                        let size_matches = expected_f32_bytes == Some(ctx.in_bytes)
                            || ctx.in_bytes == expected_elems;
                        if !ctx.in_ptr.is_null()
                            && ctx.in_bytes > 0
                            && ctx.in_bytes <= MAX_TENSOR_BYTES
                            && size_matches
                        {
                            unsafe {
                                if expected_f32_bytes == Some(ctx.in_bytes)
                                    && (ctx.in_ptr as usize)
                                        .is_multiple_of(std::mem::align_of::<f32>())
                                {
                                    let dst_f32 = std::slice::from_raw_parts_mut(
                                        ctx.in_ptr as *mut f32,
                                        expected_elems,
                                    );
                                    prepare_input_tensor_f32(
                                        tensor_nhwc,
                                        model_w,
                                        model_h,
                                        nchw_layout,
                                        dst_f32,
                                    );
                                } else if expected_f32_bytes == Some(ctx.in_bytes) {
                                    let mut tmp_f32 = vec![0.0f32; expected_elems];
                                    prepare_input_tensor_f32(
                                        tensor_nhwc,
                                        model_w,
                                        model_h,
                                        nchw_layout,
                                        &mut tmp_f32,
                                    );
                                    std::ptr::copy_nonoverlapping(
                                        tmp_f32.as_ptr() as *const u8,
                                        ctx.in_ptr,
                                        ctx.in_bytes,
                                    );
                                } else {
                                    let dst_u8 =
                                        std::slice::from_raw_parts_mut(ctx.in_ptr, expected_elems);
                                    prepare_input_tensor_u8(
                                        tensor_nhwc,
                                        model_w,
                                        model_h,
                                        nchw_layout,
                                        dst_u8,
                                    );
                                }

                                let rc = (ctx.infer_fn)(ctx.infer_req);
                                if rc == 0
                                    && !ctx.out_ptr.is_null()
                                    && (6..=MAX_TENSOR_FLOATS).contains(&ctx.out_floats)
                                {
                                    if (ctx.out_ptr as usize)
                                        .is_multiple_of(std::mem::align_of::<f32>())
                                    {
                                        let slice =
                                            std::slice::from_raw_parts(ctx.out_ptr, ctx.out_floats);
                                        out_results = Self::decode_output_with_state(
                                            &guard,
                                            slice,
                                            region,
                                            frame_w,
                                            frame_h,
                                            score_threshold,
                                        );
                                    } else {
                                        let mut aligned_out = vec![0.0f32; ctx.out_floats];
                                        std::ptr::copy_nonoverlapping(
                                            ctx.out_ptr as *const u8,
                                            aligned_out.as_mut_ptr() as *mut u8,
                                            ctx.out_floats * std::mem::size_of::<f32>(),
                                        );
                                        out_results = Self::decode_output_with_state(
                                            &guard,
                                            &aligned_out,
                                            region,
                                            frame_w,
                                            frame_h,
                                            score_threshold,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Some(InferenceRuntimeBackend::OnnxRuntime(ort_ctx)) => unsafe {
                        if !ort_ctx.vtable.is_null()
                            && !ort_ctx.session.is_null()
                            && !ort_ctx.mem_info.is_null()
                        {
                            let vt = std::slice::from_raw_parts(ort_ctx.vtable, 101);
                            let slots_ok = [9usize, 49, 51, 64, 65, 93, 96, 99]
                                .iter()
                                .all(|&idx| !vt[idx].is_null());
                            if slots_ok {
                                let create_tensor: OrtCreateTensorWithDataFn = std::mem::transmute::<
                                    *const c_void,
                                    OrtCreateTensorWithDataFn,
                                >(
                                    vt[49]
                                );
                                let run_fn: OrtRunFn =
                                    std::mem::transmute::<*const c_void, OrtRunFn>(vt[9]);
                                let get_mut_data: OrtGetTensorMutableDataFn = std::mem::transmute::<
                                    *const c_void,
                                    OrtGetTensorMutableDataFn,
                                >(
                                    vt[51]
                                );
                                let get_type_shape: OrtGetTensorTypeAndShapeFn =
                                    std::mem::transmute::<*const c_void, OrtGetTensorTypeAndShapeFn>(
                                        vt[65],
                                    );
                                let get_elem_count: OrtGetTensorShapeElementCountFn =
                                    std::mem::transmute::<
                                        *const c_void,
                                        OrtGetTensorShapeElementCountFn,
                                    >(vt[64]);
                                let release_val: OrtReleaseFn =
                                    std::mem::transmute::<*const c_void, OrtReleaseFn>(vt[96]);
                                let release_shape: OrtReleaseFn =
                                    std::mem::transmute::<*const c_void, OrtReleaseFn>(vt[99]);
                                let release_status: OrtReleaseFn =
                                    std::mem::transmute::<*const c_void, OrtReleaseFn>(vt[93]);

                                let shape: [i64; 4] = if nchw_layout {
                                    [1, 3, model_h as i64, model_w as i64]
                                } else {
                                    [1, model_h as i64, model_w as i64, 3]
                                };

                                let mut in_val: *mut c_void = std::ptr::null_mut();
                                let create_st = if ort_ctx.input_is_u8 {
                                    if ort_ctx.in_u8_buf.len() != expected_elems {
                                        ort_ctx.in_u8_buf.resize(expected_elems, 0u8);
                                    }
                                    prepare_input_tensor_u8(
                                        tensor_nhwc,
                                        model_w,
                                        model_h,
                                        nchw_layout,
                                        &mut ort_ctx.in_u8_buf,
                                    );
                                    create_tensor(
                                        ort_ctx.mem_info,
                                        ort_ctx.in_u8_buf.as_mut_ptr() as *mut c_void,
                                        ort_ctx.in_u8_buf.len(),
                                        shape.as_ptr(),
                                        shape.len(),
                                        ORT_TENSOR_ELEMENT_DATA_TYPE_UINT8,
                                        &mut in_val,
                                    )
                                } else {
                                    if ort_ctx.in_f32_buf.len() != expected_elems {
                                        ort_ctx.in_f32_buf.resize(expected_elems, 0.0f32);
                                    }
                                    prepare_input_tensor_f32(
                                        tensor_nhwc,
                                        model_w,
                                        model_h,
                                        nchw_layout,
                                        &mut ort_ctx.in_f32_buf,
                                    );
                                    create_tensor(
                                        ort_ctx.mem_info,
                                        ort_ctx.in_f32_buf.as_mut_ptr() as *mut c_void,
                                        ort_ctx
                                            .in_f32_buf
                                            .len()
                                            .saturating_mul(std::mem::size_of::<f32>()),
                                        shape.as_ptr(),
                                        shape.len(),
                                        ORT_TENSOR_ELEMENT_DATA_TYPE_FLOAT,
                                        &mut in_val,
                                    )
                                };

                                if !create_st.is_null() {
                                    release_status(create_st);
                                    if !in_val.is_null() {
                                        release_val(in_val);
                                    }
                                } else if !in_val.is_null() {
                                    let in_names = [ort_ctx.input_name.as_ptr()];
                                    let in_vals = [in_val as *const c_void];
                                    let out_names = [ort_ctx.output_name.as_ptr()];
                                    let mut out_val: *mut c_void = std::ptr::null_mut();

                                    let run_st = run_fn(
                                        ort_ctx.session,
                                        std::ptr::null(),
                                        in_names.as_ptr(),
                                        in_vals.as_ptr(),
                                        1,
                                        out_names.as_ptr(),
                                        1,
                                        &mut out_val,
                                    );
                                    if !run_st.is_null() {
                                        release_status(run_st);
                                        if !out_val.is_null() {
                                            release_val(out_val);
                                        }
                                        release_val(in_val);
                                    } else if !out_val.is_null() {
                                        let mut out_ptr: *mut c_void = std::ptr::null_mut();
                                        let mut shape_info: *mut c_void = std::ptr::null_mut();
                                        let mut valid_elem_count: Option<usize> = None;

                                        let st1 = get_mut_data(out_val, &mut out_ptr);
                                        let st2 = get_type_shape(out_val, &mut shape_info);
                                        if st1.is_null() && st2.is_null() && !shape_info.is_null() {
                                            let mut is_float_tensor = true;
                                            if !vt[60].is_null() {
                                                let get_elem_type: OrtGetTensorElementTypeFn =
                                                    std::mem::transmute::<
                                                        *const c_void,
                                                        OrtGetTensorElementTypeFn,
                                                    >(
                                                        vt[60]
                                                    );
                                                let mut out_elem_ty: c_int =
                                                    ORT_TENSOR_ELEMENT_DATA_TYPE_FLOAT;
                                                let st_ty =
                                                    get_elem_type(shape_info, &mut out_elem_ty);
                                                if st_ty.is_null() {
                                                    is_float_tensor = out_elem_ty
                                                        == ORT_TENSOR_ELEMENT_DATA_TYPE_FLOAT;
                                                } else {
                                                    release_status(st_ty);
                                                    is_float_tensor = false;
                                                }
                                            }

                                            let mut dims_valid = true;
                                            let mut dims_product: Option<usize> = None;
                                            if !vt[61].is_null() && !vt[62].is_null() {
                                                let get_dims_count: OrtGetDimensionsCountFn =
                                                    std::mem::transmute::<
                                                        *const c_void,
                                                        OrtGetDimensionsCountFn,
                                                    >(
                                                        vt[61]
                                                    );
                                                let get_dims: OrtGetDimensionsFn =
                                                    std::mem::transmute::<
                                                        *const c_void,
                                                        OrtGetDimensionsFn,
                                                    >(
                                                        vt[62]
                                                    );
                                                let mut ndims: usize = 0;
                                                let st_dc = get_dims_count(shape_info, &mut ndims);
                                                if st_dc.is_null() && (1..=16).contains(&ndims) {
                                                    let mut raw_dims = vec![0i64; ndims];
                                                    let st_d = get_dims(
                                                        shape_info,
                                                        raw_dims.as_mut_ptr(),
                                                        ndims,
                                                    );
                                                    if st_d.is_null() {
                                                        dims_product = raw_dims.iter().try_fold(
                                                            1usize,
                                                            |acc, &d| {
                                                                if d <= 0 {
                                                                    None
                                                                } else {
                                                                    usize::try_from(d)
                                                                        .ok()
                                                                        .and_then(|u| {
                                                                            acc.checked_mul(u)
                                                                        })
                                                                }
                                                            },
                                                        );
                                                        dims_valid = dims_product.is_some();
                                                    } else {
                                                        release_status(st_d);
                                                        dims_valid = false;
                                                    }
                                                } else {
                                                    if !st_dc.is_null() {
                                                        release_status(st_dc);
                                                    }
                                                    dims_valid = false;
                                                }
                                            }

                                            if is_float_tensor && dims_valid {
                                                let mut elem_count: usize = 0;
                                                let st3 =
                                                    get_elem_count(shape_info, &mut elem_count);
                                                if st3.is_null() {
                                                    let count_matches_dims = dims_product
                                                        .is_none_or(|prod| prod == elem_count);
                                                    if count_matches_dims
                                                        && (6..=MAX_TENSOR_FLOATS)
                                                            .contains(&elem_count)
                                                    {
                                                        valid_elem_count = Some(elem_count);
                                                    }
                                                } else {
                                                    release_status(st3);
                                                }
                                            }
                                        } else {
                                            if !st1.is_null() {
                                                release_status(st1);
                                            }
                                            if !st2.is_null() {
                                                release_status(st2);
                                            }
                                        }
                                        if !shape_info.is_null() {
                                            release_shape(shape_info);
                                        }

                                        if let Some(out_floats) = valid_elem_count
                                            && !out_ptr.is_null()
                                        {
                                            ort_ctx.out_floats = out_floats;
                                            let slice_vec: Vec<f32> = if (out_ptr as usize)
                                                .is_multiple_of(std::mem::align_of::<f32>())
                                            {
                                                std::slice::from_raw_parts(
                                                    out_ptr as *const f32,
                                                    out_floats,
                                                )
                                                .to_vec()
                                            } else {
                                                let mut aligned = vec![0.0f32; out_floats];
                                                std::ptr::copy_nonoverlapping(
                                                    out_ptr as *const u8,
                                                    aligned.as_mut_ptr() as *mut u8,
                                                    out_floats * std::mem::size_of::<f32>(),
                                                );
                                                aligned
                                            };
                                            release_val(out_val);
                                            release_val(in_val);
                                            out_results = Self::decode_output_with_state(
                                                &guard,
                                                &slice_vec,
                                                region,
                                                frame_w,
                                                frame_h,
                                                score_threshold,
                                            );
                                        } else {
                                            release_val(out_val);
                                            release_val(in_val);
                                        }
                                    } else {
                                        release_val(in_val);
                                    }
                                }
                            }
                        }
                    },
                    None => {}
                }
            }
        }

        let elapsed_us = t0.elapsed().as_micros() as u64;
        self.cumulative_infer_us
            .fetch_add(elapsed_us, Ordering::Relaxed);
        let elapsed_ms = (elapsed_us as f32) / 1000.0;
        let mut avg = lock_unpoisoned(&self.last_inference_ms);
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
        let guard = lock_unpoisoned(&self.inner);
        Self::decode_output_with_state(&guard, slice, region, frame_w, frame_h, score_threshold)
    }

    pub fn set_labelmap(&self, labelmap: BTreeMap<i32, String>) {
        let mut guard = lock_unpoisoned(&self.inner);
        guard.labelmap = labelmap;
    }

    fn custom_labelmap_class_count(state: &DetectorRuntimeState) -> Option<usize> {
        if state.labelmap.is_empty() {
            return None;
        }
        let is_background_zero = state
            .labelmap
            .get(&0)
            .is_some_and(|l| l == "__background__");
        if is_background_zero {
            return None;
        }
        let c_cfg = state.labelmap.keys().filter(|&&k| k >= 0).count();
        if c_cfg >= 1 { Some(c_cfg) } else { None }
    }

    fn resolve_yolo_label(state: &DetectorRuntimeState, cls_idx: i32) -> Option<String> {
        if cls_idx < 0 {
            return None;
        }
        let is_background_zero = state
            .labelmap
            .get(&0)
            .is_some_and(|l| l == "__background__");
        if !is_background_zero {
            if let Some(lbl) = state.labelmap.get(&cls_idx)
                && lbl != "__background__"
            {
                return Some(lbl.clone());
            }
            if state.labelmap.keys().any(|&k| k >= 0) {
                return None;
            }
        }
        if (cls_idx as usize) < YOLO_COCO_80.len() {
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
        if frame_w == 0
            || frame_h == 0
            || region.width() == 0
            || region.height() == 0
            || !score.is_finite()
            || !(0.0..=1.0001).contains(&score)
            || !nx1.is_finite()
            || !ny1.is_finite()
            || !nx2.is_finite()
            || !ny2.is_finite()
            || nx2 <= nx1
            || ny2 <= ny1
        {
            return;
        }
        let rx = region.x_min as f32;
        let ry = region.y_min as f32;
        let rw = region.width() as f32;
        let rh = region.height() as f32;
        let max_x1 = frame_w.saturating_sub(1) as f32;
        let max_y1 = frame_h.saturating_sub(1) as f32;
        let x1 = (rx + nx1.clamp(0.0, 1.0) * rw).clamp(0.0, max_x1) as u32;
        let y1 = (ry + ny1.clamp(0.0, 1.0) * rh).clamp(0.0, max_y1) as u32;
        let x2 = (rx + nx2.clamp(0.0, 1.0) * rw).clamp(0.0, frame_w as f32) as u32;
        let y2 = (ry + ny2.clamp(0.0, 1.0) * rh).clamp(0.0, frame_h as f32) as u32;

        if x2 > x1.saturating_add(4) && y2 > y1.saturating_add(4) {
            out.push(DetectionResult {
                label,
                score,
                bbox: BoundingBox::new(x1, y1, x2, y2),
                region,
            });
        }
    }

    fn decode_transposed_yolo(
        state: &DetectorRuntimeState,
        slice: &[f32],
        num_classes: usize,
        num_anchors: usize,
        region: BoundingBox,
        frame_dims: (u32, u32),
        score_threshold: f32,
    ) -> Vec<DetectionResult> {
        let Some(rows) = 4usize.checked_add(num_classes) else {
            return Vec::new();
        };
        let Some(expected_len) = rows.checked_mul(num_anchors) else {
            return Vec::new();
        };
        if num_classes == 0 || num_anchors == 0 || slice.len() != expected_len {
            return Vec::new();
        }
        let mw = state.model_w.max(1) as f32;
        let mh = state.model_h.max(1) as f32;
        let mut out_results = Vec::new();
        for a in 0..num_anchors {
            let mut best_cls = -1i32;
            let mut best_score = 0.0f32;
            for c in 0..num_classes {
                let s = slice[(4 + c) * num_anchors + a];
                if s.is_finite() && s > best_score {
                    best_score = s;
                    best_cls = i32::try_from(c).unwrap_or(-1);
                }
            }
            if !best_score.is_finite()
                || best_score < score_threshold
                || best_score > 1.0001
                || best_cls < 0
            {
                continue;
            }
            let Some(label) = Self::resolve_yolo_label(state, best_cls) else {
                continue;
            };
            let cx = slice[a];
            let cy = slice[num_anchors + a];
            let bw = slice[2 * num_anchors + a];
            let bh = slice[3 * num_anchors + a];
            if !cx.is_finite()
                || !cy.is_finite()
                || !bw.is_finite()
                || !bh.is_finite()
                || bw <= 0.0
                || bh <= 0.0
            {
                continue;
            }
            let is_pixel_coords = bw > 1.5 || bh > 1.5 || cx > 1.5 || cy > 1.5;
            let (scale_w, scale_h) = if is_pixel_coords {
                (mw, mh)
            } else {
                (1.0, 1.0)
            };
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
        nms_detections(out_results, 0.50)
    }

    fn decode_row_major_yolo(
        state: &DetectorRuntimeState,
        slice: &[f32],
        num_classes: usize,
        region: BoundingBox,
        frame_dims: (u32, u32),
        score_threshold: f32,
    ) -> Vec<DetectionResult> {
        let Some(stride) = 5usize.checked_add(num_classes) else {
            return Vec::new();
        };
        if num_classes == 0 || !slice.len().is_multiple_of(stride) {
            return Vec::new();
        }
        let mw = state.model_w.max(1) as f32;
        let mh = state.model_h.max(1) as f32;
        let mut out_results = Vec::new();
        for row in slice.chunks_exact(stride) {
            let obj_conf = row[4];
            if !obj_conf.is_finite() || obj_conf < score_threshold || obj_conf > 1.0001 {
                continue;
            }
            let mut best_cls = -1i32;
            let mut best_cls_prob = 0.0f32;
            for (c, &prob) in row[5..stride].iter().enumerate() {
                if prob.is_finite() && prob > best_cls_prob {
                    best_cls_prob = prob;
                    best_cls = i32::try_from(c).unwrap_or(-1);
                }
            }
            let score = if best_cls_prob > 0.0 {
                obj_conf * best_cls_prob
            } else {
                obj_conf
            };
            if !score.is_finite() || score < score_threshold || score > 1.0001 || best_cls < 0 {
                continue;
            }
            let Some(label) = Self::resolve_yolo_label(state, best_cls) else {
                continue;
            };
            let (cx, cy, bw, bh) = (row[0], row[1], row[2], row[3]);
            if !cx.is_finite()
                || !cy.is_finite()
                || !bw.is_finite()
                || !bh.is_finite()
                || bw <= 0.0
                || bh <= 0.0
            {
                continue;
            }
            let is_pixel_coords = bw > 1.5 || bh > 1.5 || cx > 1.5 || cy > 1.5;
            let (scale_w, scale_h) = if is_pixel_coords {
                (mw, mh)
            } else {
                (1.0, 1.0)
            };
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
        nms_detections(out_results, 0.50)
    }

    fn decode_output_with_state(
        state: &DetectorRuntimeState,
        slice: &[f32],
        region: BoundingBox,
        frame_w: u32,
        frame_h: u32,
        score_threshold: f32,
    ) -> Vec<DetectionResult> {
        if frame_w == 0 || frame_h == 0 || !score_threshold.is_finite() {
            return Vec::new();
        }
        let mut out_results = Vec::new();
        let mw = state.model_w.max(1) as f32;
        let mh = state.model_h.max(1) as f32;
        let frame_dims = (frame_w, frame_h);
        let expected_anchors = expected_yolo_anchors(state.model_w, state.model_h);
        let c_cfg = Self::custom_labelmap_class_count(state);

        let is_yolo_tensor_shape = (expected_anchors > 0
            && slice.len().is_multiple_of(expected_anchors)
            && (5..=205).contains(&(slice.len() / expected_anchors)))
            || c_cfg.is_some_and(|c| {
                slice.len().is_multiple_of(4 + c) || slice.len().is_multiple_of(5 + c)
            })
            || slice.len().is_multiple_of(84)
            || slice.len().is_multiple_of(85);

        // 1. SSD DetectionOutput [1, 1, N, 7]: [image_id, label_id, score, xmin, ymin, xmax, ymax]
        // Also auto-detected when a 7-column SSD model is loaded even if `model_type: yolo*` was selected in UI.
        let is_ssd_shape = slice.len().is_multiple_of(7)
            && (!state.is_yolo || (!slice.len().is_multiple_of(6) && !is_yolo_tensor_shape));

        if is_ssd_shape {
            for row in slice.as_chunks::<7>().0 {
                let img_id = row[0];
                let raw_label = row[1];
                let score = row[2];
                if !img_id.is_finite()
                    || img_id < 0.0
                    || !raw_label.is_finite()
                    || !(0.0..=10_000.0).contains(&raw_label)
                    || (raw_label - raw_label.round()).abs() > 1e-3
                    || !score.is_finite()
                    || score < score_threshold
                    || score > 1.0001
                {
                    continue;
                }
                let label_id = raw_label.round() as i32;
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
            return nms_detections(out_results, 0.50);
        }

        // 2. Dynamic & 80-class Transposed YOLOv8 / YOLOv9 / YOLOv11 [1, 4 + C, N]
        //    (and exact-anchor Row-Major [1, N, 5 + C] when cols == 5 + C)
        if expected_anchors > 0 && slice.len().is_multiple_of(expected_anchors) {
            let cols = slice.len() / expected_anchors;
            let prefers_row_major =
                (c_cfg.is_some_and(|c| cols == 5 + c)) || (c_cfg.is_none() && cols == 85);
            if prefers_row_major && (6..=205).contains(&cols) {
                let dets = Self::decode_row_major_yolo(
                    state,
                    slice,
                    cols - 5,
                    region,
                    frame_dims,
                    score_threshold,
                );
                if !dets.is_empty() || !slice.len().is_multiple_of(cols - 1) {
                    return dets;
                }
            } else if (5..=204).contains(&cols) {
                let num_classes = cols - 4;
                let dets = Self::decode_transposed_yolo(
                    state,
                    slice,
                    num_classes,
                    expected_anchors,
                    region,
                    frame_dims,
                    score_threshold,
                );
                if !dets.is_empty() || !slice.len().is_multiple_of(cols) {
                    return dets;
                }
            }
        }

        if let Some(c) = c_cfg
            && c >= 1
            && slice.len().is_multiple_of(4 + c)
        {
            let num_anchors = slice.len() / (4 + c);
            let dets = Self::decode_transposed_yolo(
                state,
                slice,
                c,
                num_anchors,
                region,
                frame_dims,
                score_threshold,
            );
            if !dets.is_empty() || !slice.len().is_multiple_of(5 + c) {
                return dets;
            }
        }

        if c_cfg.is_none()
            && slice.len() >= 84 * 10
            && slice.len().is_multiple_of(84)
            && !slice.len().is_multiple_of(85)
        {
            let num_anchors = slice.len() / 84;
            return Self::decode_transposed_yolo(
                state,
                slice,
                80,
                num_anchors,
                region,
                frame_dims,
                score_threshold,
            );
        }

        // 3. Dynamic & 80-class Row-Major YOLOv5 / YOLOX [1, N, 5 + C]: [cx, cy, w, h, obj_conf, cls_0..cls_{C-1}]
        if let Some(c) = c_cfg
            && c >= 1
            && slice.len().is_multiple_of(5 + c)
        {
            return Self::decode_row_major_yolo(
                state,
                slice,
                c,
                region,
                frame_dims,
                score_threshold,
            );
        }

        if expected_anchors > 0 && slice.len().is_multiple_of(expected_anchors) {
            let cols = slice.len() / expected_anchors;
            if (6..=205).contains(&cols) {
                let num_classes = cols - 5;
                return Self::decode_row_major_yolo(
                    state,
                    slice,
                    num_classes,
                    region,
                    frame_dims,
                    score_threshold,
                );
            }
        }

        if slice.len().is_multiple_of(85) {
            return Self::decode_row_major_yolo(
                state,
                slice,
                80,
                region,
                frame_dims,
                score_threshold,
            );
        }

        // 4. End-to-End 6-column YOLO / YOLOX / RT-DETR [1, N, 6]: [x1, y1, x2, y2, score, class_id]
        if slice.len().is_multiple_of(6) {
            for row in slice.as_chunks::<6>().0 {
                let score = row[4];
                let raw_label = row[5];
                if !score.is_finite()
                    || score < score_threshold
                    || score > 1.0001
                    || !raw_label.is_finite()
                    || !(0.0..=10_000.0).contains(&raw_label)
                    || (raw_label - raw_label.round()).abs() > 1e-3
                    || !row[0].is_finite()
                    || !row[1].is_finite()
                    || !row[2].is_finite()
                    || !row[3].is_finite()
                {
                    continue;
                }
                let label_id = raw_label.round() as i32;
                let Some(label) = Self::resolve_yolo_label(state, label_id) else {
                    continue;
                };
                let is_pixel_coords = row[0] > 1.5 || row[1] > 1.5 || row[2] > 1.5 || row[3] > 1.5;
                let (scale_w, scale_h) = if is_pixel_coords {
                    (mw, mh)
                } else {
                    (1.0, 1.0)
                };
                let nx1 = row[0] / scale_w;
                let ny1 = row[1] / scale_h;
                let nx2 = row[2] / scale_w;
                let ny2 = row[3] / scale_h;
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
        out_results
    }
}

/// Applies class-aware Non-Maximum Suppression (NMS) across detections from overlapping regions.
pub const FACE_EMBEDDING_DIM: usize = 512;
pub const DEFAULT_FACE_SIMILARITY_THRESHOLD: f32 = 0.68;

/// L2-normalizes an embedding slice in-place so `||vec||_2 == 1.0`.
/// Leaves zero vectors unchanged without producing `NaN`.
pub fn l2_normalize_embedding(vec: &mut [f32]) {
    for v in vec.iter_mut() {
        if !v.is_finite() {
            *v = 0.0;
        }
    }
    let norm_sq: f64 = vec.iter().map(|&v| (v as f64) * (v as f64)).sum();
    let norm = norm_sq.sqrt();
    if norm.is_finite() && norm > 1e-12 {
        for v in vec.iter_mut() {
            *v = ((*v as f64) / norm) as f32;
        }
    }
}

/// Returns an L2-normalized copy of `vec`.
pub fn l2_normalized(mut vec: Vec<f32>) -> Vec<f32> {
    l2_normalize_embedding(&mut vec);
    vec
}

/// Computes the cosine similarity between two embedding vectors `a` and `b` in `[-1.0, 1.0]`.
/// Returns `1.0` for identical non-zero vectors and `0.0` for orthogonal or zero vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        if !x.is_finite() || !y.is_finite() {
            return 0.0;
        }
        let xf = x as f64;
        let yf = y as f64;
        dot += xf * yf;
        norm_a += xf * xf;
        norm_b += yf * yf;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if !denom.is_finite() || !dot.is_finite() || denom <= 1e-12 {
        return 0.0;
    }
    let ratio = dot / denom;
    if !ratio.is_finite() {
        return 0.0;
    }
    (ratio as f32).clamp(-1.0, 1.0)
}

/// Computes a deterministic 512-dimensional L2-normalized face embedding descriptor from either
/// an encoded image (`JPEG`, `WebP`, `PNG`), a raw `N x N x 3` RGB face crop (`112x112x3`), or
/// arbitrary fallback bytes (`[f32; 512]`).
///
/// When `data` is a decodable image or a raw square RGB crop, it computes a 16x16 spatial
/// zero-mean luma/chroma + directional gradient descriptor so that an enrolled `.jpg`/`.webp`
/// face photo matches a live camera `112x112x3` RGB crop despite re-encoding or minor sensor noise.
pub fn compute_fallback_face_embedding(data: &[u8]) -> Vec<f32> {
    let mut out = vec![0.0f32; FACE_EMBEDDING_DIM];
    if data.is_empty() {
        out[0] = 1.0;
        return out;
    }

    if let Some(grid) = ninjanvr_vision::decode_or_extract_rgb_grid_16x16(data) {
        let mut luma = [0.0f32; 256];
        let mut mean_y = 0.0f32;
        let mut mean_rg = 0.0f32;
        let mut mean_yb = 0.0f32;
        for (i, &[r, g, b]) in grid.iter().enumerate() {
            let y = 0.299 * r + 0.587 * g + 0.114 * b;
            luma[i] = y;
            mean_y += y;
            mean_rg += r - g;
            mean_yb += 0.5 * (r + g) - b;
        }
        mean_y /= 256.0;
        mean_rg /= 256.0;
        mean_yb /= 256.0;

        for gy in 0..16usize {
            for gx in 0..16usize {
                let idx = gy * 16 + gx;
                let [r, g, b] = grid[idx];
                let y_centered = luma[idx] - mean_y;
                let rg_centered = (r - g) - mean_rg;
                let yb_centered = (0.5 * (r + g) - b) - mean_yb;

                let x_left = gx.saturating_sub(1);
                let x_right = (gx + 1).min(15);
                let y_up = gy.saturating_sub(1);
                let y_down = (gy + 1).min(15);
                let grad_x = luma[gy * 16 + x_right] - luma[gy * 16 + x_left];
                let grad_y = luma[y_down * 16 + gx] - luma[y_up * 16 + gx];

                out[idx * 2] = y_centered + 0.65 * rg_centered + 0.45 * grad_x;
                out[idx * 2 + 1] = grad_y + 0.65 * yb_centered - 0.35 * y_centered;
            }
        }
        if out.iter().any(|&v| v.abs() > 1e-6) {
            l2_normalize_embedding(&mut out);
            return out;
        }
    }

    // Fallback content digest for non-image byte buffers (or uniform solid-color frames) using 64-bit SplitMix/FNV mixing
    let mut base_hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (idx, &b) in data.iter().enumerate() {
        base_hash ^= (b as u64).wrapping_add((idx as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        base_hash = base_hash.wrapping_mul(0x0000_0100_0000_01b3);
        base_hash ^= base_hash >> 31;
    }

    for (dim, slot) in out.iter_mut().enumerate() {
        let sample_b = data[dim % data.len()] as u64;
        let stride_b = data[(dim.wrapping_mul(37) + 11) % data.len()] as u64;
        let mut z = base_hash
            .wrapping_add((dim as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
            .wrapping_add((sample_b << 8) | stride_b);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let unit = ((z >> 11) as f64) / ((1u64 << 53) as f64);
        *slot = (unit * 2.0 - 1.0) as f32;
    }

    l2_normalize_embedding(&mut out);
    out
}

/// Extracts a 512-d L2-normalized face embedding from image/crop bytes.
pub fn extract_face_embedding(image_bytes: &[u8]) -> Vec<f32> {
    compute_fallback_face_embedding(image_bytes)
}

/// Resolves the bounding box to crop for face recognition given a `person` bounding box and an
/// optional single-pass `face` child attribute box (`#79` `[AI-14]`).
/// Falls back to the upper-body/head region when no `face` attribute box is present and
/// `person_bbox.height() >= 120`.
pub fn resolve_face_crop_bbox(
    person_bbox: BoundingBox,
    face_attr_bbox: Option<BoundingBox>,
    frame_w: u32,
    frame_h: u32,
    min_face_area: u32,
) -> Option<BoundingBox> {
    if frame_w == 0 || frame_h == 0 {
        return None;
    }
    if let Some(fb) = face_attr_bbox
        && fb.width() > 0
        && fb.height() > 0
        && fb.x_min < frame_w
        && fb.y_min < frame_h
    {
        let x_min = fb.x_min;
        let y_min = fb.y_min;
        let x_max = fb.x_max.min(frame_w);
        let y_max = fb.y_max.min(frame_h);
        if x_max > x_min && y_max > y_min {
            let clamped = BoundingBox::new(x_min, y_min, x_max, y_max);
            if clamped.area() >= min_face_area {
                return Some(clamped);
            }
        }
    }

    if person_bbox.width() > 0
        && person_bbox.height() >= 120
        && person_bbox.x_min < frame_w
        && person_bbox.y_min < frame_h
    {
        let pw = person_bbox.width();
        let ph = person_bbox.height();
        let inset_x = pw / 5;
        let head_h = ((ph as u64 * 35) / 100) as u32;
        let x_min = person_bbox.x_min.saturating_add(inset_x).min(frame_w - 1);
        let x_max = person_bbox
            .x_max
            .saturating_sub(inset_x)
            .min(frame_w)
            .max(x_min.saturating_add(1));
        let y_min = person_bbox.y_min.min(frame_h - 1);
        let y_max = person_bbox
            .y_min
            .saturating_add(head_h.max(1))
            .min(person_bbox.y_max)
            .min(frame_h)
            .max(y_min.saturating_add(1));
        let fallback_box = BoundingBox::new(x_min, y_min, x_max, y_max);
        if fallback_box.area() >= min_face_area {
            return Some(fallback_box);
        }
    }

    None
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FaceMatchResult {
    pub sub_label: String,
    pub score: f32,
    pub is_known: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrolledFaceProfile {
    pub name: String,
    pub sample_count: usize,
    pub sample_files: Vec<String>,
    pub updated_at: u64,
}

const MAX_FACE_PROFILE_NAME_LEN: usize = 128;
const MAX_FACE_PROFILES: usize = 1024;
const MAX_SAMPLES_PER_FACE_PROFILE: usize = 64;
const MAX_FACE_SAMPLE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct FaceGallery {
    profiles: BTreeMap<String, Vec<Vec<f32>>>,
    sample_files: BTreeMap<String, Vec<String>>,
    loaded_dir: Option<PathBuf>,
}

pub fn sanitize_face_profile_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('.')
        || trimmed.contains("..")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.eq_ignore_ascii_case("unknown")
    {
        return None;
    }
    let clean: String = trimmed
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .take(MAX_FACE_PROFILE_NAME_LEN)
        .collect::<String>()
        .trim()
        .to_string();
    if clean.is_empty() || clean.eq_ignore_ascii_case("unknown") {
        None
    } else {
        Some(clean)
    }
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
}

fn is_real_bounded_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_file() && m.len() <= MAX_FACE_SAMPLE_BYTES)
        .unwrap_or(false)
}

impl FaceGallery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_dir(faces_dir: impl AsRef<Path>) -> Result<Self> {
        let mut gallery = Self::new();
        gallery.sync_from_dir(faces_dir.as_ref())?;
        Ok(gallery)
    }

    /// Resolves an existing canonical profile key matching `clean_name` case-insensitively
    /// (checking in-memory profiles first, then subdirectories inside `faces_dir`).
    pub fn resolve_canonical_name(&self, clean_name: &str, faces_dir: Option<&Path>) -> String {
        if let Some(existing) = self
            .profiles
            .keys()
            .find(|k| k.eq_ignore_ascii_case(clean_name))
        {
            return existing.clone();
        }
        if let Some(dir) = faces_dir.or(self.loaded_dir.as_deref())
            && is_real_directory(dir)
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_real_directory(&path)
                    && let Some(raw_name) = path.file_name().and_then(|s| s.to_str())
                    && let Some(sanitized) = sanitize_face_profile_name(raw_name)
                    && sanitized.eq_ignore_ascii_case(clean_name)
                {
                    return sanitized;
                }
            }
        }
        clean_name.to_string()
    }

    /// Synchronizes enrolled face profiles from `{faces_dir}/<name>/*.{jpg,jpeg,webp,png,bin}`
    /// while preserving any in-memory embeddings already enrolled during the current session.
    pub fn sync_from_dir(&mut self, faces_dir: &Path) -> Result<()> {
        self.loaded_dir = Some(faces_dir.to_path_buf());
        if !is_real_directory(faces_dir) {
            return Ok(());
        }
        let entries = match std::fs::read_dir(faces_dir) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        let mut dir_paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_real_directory(p))
            .collect();
        dir_paths.sort();

        for path in dir_paths {
            if self.profiles.len() >= MAX_FACE_PROFILES {
                break;
            }
            let Some(raw_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(name) = sanitize_face_profile_name(raw_name) else {
                continue;
            };
            let canonical_key = self
                .profiles
                .keys()
                .find(|k| k.eq_ignore_ascii_case(&name))
                .cloned()
                .unwrap_or(name);
            let Ok(files) = std::fs::read_dir(&path) else {
                continue;
            };
            let mut file_paths: Vec<PathBuf> = files
                .flatten()
                .map(|f| f.path())
                .filter(|p| {
                    is_real_bounded_file(p)
                        && p.extension()
                            .and_then(|ext| ext.to_str())
                            .map(|ext| {
                                matches!(
                                    ext.to_ascii_lowercase().as_str(),
                                    "jpg" | "jpeg" | "webp" | "png" | "bin"
                                )
                            })
                            .unwrap_or(false)
                })
                .collect();
            file_paths.sort();
            file_paths.truncate(MAX_SAMPLES_PER_FACE_PROFILE);

            if !file_paths.is_empty() {
                let mut embs = Vec::new();
                let mut names = Vec::new();
                for fp in file_paths {
                    if let Ok(bytes) = std::fs::read(&fp)
                        && !bytes.is_empty()
                    {
                        embs.push(extract_face_embedding(&bytes));
                        if let Some(fname) = fp.file_name().and_then(|s| s.to_str()) {
                            names.push(fname.to_string());
                        }
                    }
                }
                if !embs.is_empty() {
                    self.profiles.insert(canonical_key.clone(), embs);
                    self.sample_files.insert(canonical_key, names);
                }
            }
        }
        Ok(())
    }

    pub fn enroll_embedding(&mut self, name: &str, mut embedding: Vec<f32>) {
        let Some(clean_name) = sanitize_face_profile_name(name) else {
            return;
        };
        if embedding.is_empty() {
            return;
        }
        let canonical = self.resolve_canonical_name(&clean_name, None);
        if !self.profiles.contains_key(&canonical) && self.profiles.len() >= MAX_FACE_PROFILES {
            return;
        }
        l2_normalize_embedding(&mut embedding);
        let list = self.profiles.entry(canonical.clone()).or_default();
        let files = self.sample_files.entry(canonical).or_default();
        if list.len() >= MAX_SAMPLES_PER_FACE_PROFILE {
            list.remove(0);
            if !files.is_empty() {
                files.remove(0);
            }
        }
        list.push(embedding);
        let idx = list.len();
        files.push(format!("sample_{idx}.bin"));
    }

    pub fn enroll_bytes(&mut self, name: &str, image_bytes: &[u8]) -> Vec<f32> {
        let emb = extract_face_embedding(image_bytes);
        self.enroll_embedding(name, emb.clone());
        emb
    }

    pub fn enroll_to_dir(
        &mut self,
        faces_dir: impl AsRef<Path>,
        name: &str,
        file_stem: &str,
        ext: &str,
        image_bytes: &[u8],
    ) -> Result<(PathBuf, Vec<f32>)> {
        if image_bytes.len() as u64 > MAX_FACE_SAMPLE_BYTES {
            bail!("face sample exceeds maximum size of {MAX_FACE_SAMPLE_BYTES} bytes");
        }
        let clean_name = sanitize_face_profile_name(name)
            .ok_or_else(|| anyhow!("invalid face profile name '{name}'"))?;
        let canonical_name = self.resolve_canonical_name(&clean_name, Some(faces_dir.as_ref()));
        if !self.profiles.contains_key(&canonical_name) && self.profiles.len() >= MAX_FACE_PROFILES
        {
            bail!("maximum number of enrolled face profiles ({MAX_FACE_PROFILES}) reached");
        }
        let base_dir = faces_dir.as_ref();
        if let Ok(meta) = std::fs::symlink_metadata(base_dir)
            && meta.file_type().is_symlink()
        {
            bail!("faces_dir must not be a symlink");
        }
        let person_dir = base_dir.join(&canonical_name);
        if let Ok(meta) = std::fs::symlink_metadata(&person_dir)
            && meta.file_type().is_symlink()
        {
            bail!("face profile directory must not be a symlink");
        }
        std::fs::create_dir_all(&person_dir)?;

        let safe_stem: String = file_stem
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .take(64)
            .collect();
        let stem = if safe_stem.is_empty() {
            "sample".to_string()
        } else {
            safe_stem
        };
        let safe_ext: String = ext
            .trim_start_matches('.')
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .take(8)
            .collect();
        let clean_ext = match safe_ext.as_str() {
            "jpg" | "jpeg" | "webp" | "png" | "bin" => safe_ext,
            _ => "jpg".to_string(),
        };
        let filename = format!("{stem}.{clean_ext}");
        let file_path = person_dir.join(&filename);
        if let Ok(meta) = std::fs::symlink_metadata(&file_path)
            && meta.file_type().is_symlink()
        {
            bail!("target face sample file must not be a symlink");
        }
        let payload = if image_bytes.is_empty() {
            format!("face-profile:{canonical_name}:{stem}").into_bytes()
        } else {
            image_bytes.to_vec()
        };
        std::fs::write(&file_path, &payload)?;

        let emb = extract_face_embedding(&payload);
        let embs_entry = self.profiles.entry(canonical_name.clone()).or_default();
        let files_entry = self.sample_files.entry(canonical_name).or_default();
        if let Some(existing_idx) = files_entry.iter().position(|f| f == &filename) {
            if existing_idx < embs_entry.len() {
                embs_entry[existing_idx] = emb.clone();
            } else {
                embs_entry.push(emb.clone());
            }
        } else {
            if embs_entry.len() >= MAX_SAMPLES_PER_FACE_PROFILE {
                embs_entry.remove(0);
                if !files_entry.is_empty() {
                    files_entry.remove(0);
                }
            }
            embs_entry.push(emb.clone());
            files_entry.push(filename);
        }
        Ok((file_path, emb))
    }

    pub fn remove_profile(&mut self, faces_dir: Option<&Path>, name: &str) -> Result<bool> {
        let Some(clean_name) = sanitize_face_profile_name(name) else {
            return Ok(false);
        };
        let matching_keys: Vec<String> = self
            .profiles
            .keys()
            .chain(self.sample_files.keys())
            .filter(|k| k.eq_ignore_ascii_case(&clean_name))
            .cloned()
            .collect();
        let mut removed_mem = false;
        for key in matching_keys {
            if self.profiles.remove(&key).is_some() {
                removed_mem = true;
            }
            if self.sample_files.remove(&key).is_some() {
                removed_mem = true;
            }
        }

        let mut removed_disk = false;
        if let Some(dir) = faces_dir.or(self.loaded_dir.as_deref())
            && is_real_directory(dir)
        {
            let target = dir.join(&clean_name);
            if is_real_directory(&target) {
                std::fs::remove_dir_all(&target)?;
                removed_disk = true;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if is_real_directory(&path)
                        && let Some(raw_name) = path.file_name().and_then(|s| s.to_str())
                        && (raw_name.eq_ignore_ascii_case(&clean_name)
                            || sanitize_face_profile_name(raw_name)
                                .is_some_and(|s| s.eq_ignore_ascii_case(&clean_name)))
                    {
                        std::fs::remove_dir_all(&path)?;
                        removed_disk = true;
                    }
                }
            }
        }
        Ok(removed_mem || removed_disk)
    }

    pub fn list_profiles(&self, faces_dir: Option<&Path>) -> Vec<EnrolledFaceProfile> {
        let mut out = Vec::new();
        let dir_opt = faces_dir.or(self.loaded_dir.as_deref());
        for (name, embs) in &self.profiles {
            let files = self.sample_files.get(name).cloned().unwrap_or_default();
            let updated_at = dir_opt
                .and_then(|d| std::fs::symlink_metadata(d.join(name)).ok())
                .filter(|m| m.file_type().is_dir())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(EnrolledFaceProfile {
                name: name.clone(),
                sample_count: embs.len(),
                sample_files: files,
                updated_at,
            });
        }
        out
    }

    pub fn match_embedding(
        &self,
        query_embedding: &[f32],
        similarity_threshold: f32,
        unknown_alert: bool,
    ) -> Option<FaceMatchResult> {
        if query_embedding.is_empty() {
            return None;
        }
        let norm_query = l2_normalized(query_embedding.to_vec());
        let thresh = if similarity_threshold.is_finite() && similarity_threshold > 0.0 {
            similarity_threshold
        } else {
            DEFAULT_FACE_SIMILARITY_THRESHOLD
        };

        let mut best_name: Option<String> = None;
        let mut max_sim = -1.0f32;

        for (name, embeddings) in &self.profiles {
            for enrolled in embeddings {
                let sim = cosine_similarity(&norm_query, enrolled);
                if sim > max_sim {
                    max_sim = sim;
                    best_name = Some(name.clone());
                }
            }
        }

        if let Some(matched_name) = best_name
            && max_sim >= thresh
        {
            return Some(FaceMatchResult {
                sub_label: matched_name,
                score: max_sim,
                is_known: true,
            });
        }

        if unknown_alert {
            Some(FaceMatchResult {
                sub_label: "unknown".to_string(),
                score: max_sim.max(0.0),
                is_known: false,
            })
        } else {
            None
        }
    }

    pub fn match_face_bytes(
        &self,
        image_bytes: &[u8],
        similarity_threshold: f32,
        unknown_alert: bool,
    ) -> Option<FaceMatchResult> {
        let emb = extract_face_embedding(image_bytes);
        self.match_embedding(&emb, similarity_threshold, unknown_alert)
    }
}

pub fn nms_detections(
    mut detections: Vec<DetectionResult>,
    iou_threshold: f32,
) -> Vec<DetectionResult> {
    let thresh = if iou_threshold.is_finite() {
        iou_threshold.clamp(0.01, 0.99)
    } else {
        0.50
    };
    detections.retain(|d| d.score.is_finite());
    detections.sort_by(|a, b| b.score.total_cmp(&a.score));
    detections.truncate(512);
    let mut kept: Vec<DetectionResult> = Vec::with_capacity(detections.len());
    for det in detections {
        if !kept.iter().any(|k| {
            k.label == det.label
                && (k.bbox.iou(&det.bbox) > thresh
                    || k.bbox.intersection_over_min(&det.bbox) > 0.80)
        }) {
            kept.push(det);
        }
    }
    kept
}

/// Default license plate format regular expression supporting Mercosul (`[A-Z]{3}[0-9][A-Z0-9][0-9]{2}`,
/// e.g. `ABC1D23`), legacy 3-letter/4-digit (`ABC1234`), and standard international alphanumeric formats.
pub const DEFAULT_LPR_FORMAT_REGEX: &str = r"^([A-Z]{3}[0-9][A-Z0-9][0-9]{2}|[A-Z]{2,3}[0-9]{3,4}|[A-Z]{1,3}[0-9]{1,4}[A-Z]{1,3}|[0-9][A-Z]{3}[0-9]{3})$";

/// Standard 36-character alphanumeric CTC alphabet (`0-9` followed by `A-Z`).
pub const LPR_CTC_ALPHABET: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Single-frame OCR plate read with overall and per-character confidence weights.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrPlateRead {
    pub text: String,
    pub confidence: f32,
    pub char_confidences: Vec<f32>,
}

impl OcrPlateRead {
    pub fn new(text: impl Into<String>, confidence: f32) -> Self {
        let normalized = normalize_license_plate(&text.into());
        let char_count = normalized.chars().count();
        let safe_conf = if confidence.is_finite() {
            confidence
        } else {
            0.0
        };
        Self {
            text: normalized,
            confidence: safe_conf,
            char_confidences: vec![safe_conf; char_count],
        }
    }

    pub fn with_char_confidences(
        text: impl Into<String>,
        confidence: f32,
        char_confidences: Vec<f32>,
    ) -> Self {
        let normalized = normalize_license_plate(&text.into());
        let safe_conf = if confidence.is_finite() {
            confidence
        } else {
            0.0
        };
        Self {
            text: normalized,
            confidence: safe_conf,
            char_confidences,
        }
    }
}

/// Consolidated multi-frame LPR consensus result after confidence-weighted character voting,
/// regex format validation, and `known_plates` lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LprConsensusResult {
    pub plate: String,
    pub confidence: f32,
    pub sub_label: Option<String>,
    pub votes: usize,
}

const MAX_NORMALIZED_PLATE_CHARS: usize = 32;
const MAX_REGEX_PATTERN_CHARS: usize = 512;
const MAX_REGEX_DEPTH: usize = 16;
const MAX_REGEX_QUANTIFIER_REP: usize = 32;
const MAX_ACTIVE_LPR_TRACKS: usize = 512;

/// Normalizes a raw license plate string by keeping ASCII alphanumeric characters and uppercasing.
pub fn normalize_license_plate(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .take(MAX_NORMALIZED_PLATE_CHARS)
        .collect()
}

/// Looks up a normalized license plate in `known_plates` (case- and hyphen-insensitive)
/// and returns its mapped friendly vehicle/owner label (e.g., `"ABC1D23"` -> `"Family SUV"`).
pub fn lookup_known_plate(plate: &str, known_plates: &BTreeMap<String, String>) -> Option<String> {
    let norm = normalize_license_plate(plate);
    if norm.is_empty() {
        return None;
    }
    if let Some(exact) = known_plates.get(&norm) {
        return Some(exact.clone());
    }
    for (k, v) in known_plates {
        if normalize_license_plate(k) == norm {
            return Some(v.clone());
        }
    }
    None
}

#[derive(Debug, Clone)]
enum RegexNode {
    Alternation(Vec<RegexNode>),
    Sequence(Vec<RegexAtom>),
}

#[derive(Debug, Clone)]
struct RegexAtom {
    matcher: AtomMatcher,
    min_rep: usize,
    max_rep: usize,
}

#[derive(Debug, Clone)]
enum AtomMatcher {
    Group(Box<RegexNode>),
    CharClass {
        ranges: Vec<(char, char)>,
        chars: Vec<char>,
        negated: bool,
    },
    Digit,
    Word,
    Any,
    Literal(char),
}

impl AtomMatcher {
    fn matches_char(&self, ch: char) -> bool {
        match self {
            AtomMatcher::CharClass {
                ranges,
                chars,
                negated,
            } => {
                let in_range = ranges.iter().any(|&(lo, hi)| ch >= lo && ch <= hi);
                let in_chars = chars.contains(&ch);
                let hit = in_range || in_chars;
                if *negated { !hit } else { hit }
            }
            AtomMatcher::Digit => ch.is_ascii_digit(),
            AtomMatcher::Word => ch.is_ascii_alphanumeric() || ch == '_',
            AtomMatcher::Any => true,
            AtomMatcher::Literal(c) => *c == ch,
            AtomMatcher::Group(_) => false,
        }
    }
}

fn parse_regex_node(chars: &[char], pos: &mut usize, depth: usize) -> RegexNode {
    let mut branches = Vec::new();
    branches.push(parse_regex_sequence(chars, pos, depth));
    while *pos < chars.len() && chars[*pos] == '|' {
        *pos += 1;
        branches.push(parse_regex_sequence(chars, pos, depth));
    }
    if branches.len() == 1 {
        branches.pop().unwrap()
    } else {
        RegexNode::Alternation(branches)
    }
}

fn parse_regex_sequence(chars: &[char], pos: &mut usize, depth: usize) -> RegexNode {
    let mut atoms = Vec::new();
    while *pos < chars.len() {
        let ch = chars[*pos];
        if ch == '|' || (depth > 0 && ch == ')') {
            break;
        }
        if ch == '^' || ch == '$' {
            *pos += 1;
            continue;
        }
        let matcher = match ch {
            '(' if depth < MAX_REGEX_DEPTH => {
                *pos += 1;
                if *pos + 1 < chars.len() && chars[*pos] == '?' && chars[*pos + 1] == ':' {
                    *pos += 2;
                }
                let inner = parse_regex_node(chars, pos, depth + 1);
                if *pos < chars.len() && chars[*pos] == ')' {
                    *pos += 1;
                }
                AtomMatcher::Group(Box::new(inner))
            }
            '[' => {
                *pos += 1;
                let mut negated = false;
                if *pos < chars.len() && chars[*pos] == '^' {
                    negated = true;
                    *pos += 1;
                }
                let mut ranges = Vec::new();
                let mut single_chars = Vec::new();
                while *pos < chars.len() && chars[*pos] != ']' {
                    if chars[*pos] == '\\' && *pos + 1 < chars.len() {
                        let esc = chars[*pos + 1];
                        if esc == 'd' {
                            *pos += 2;
                            ranges.push(('0', '9'));
                            continue;
                        } else if esc == 'w' {
                            *pos += 2;
                            ranges.push(('0', '9'));
                            ranges.push(('A', 'Z'));
                            ranges.push(('a', 'z'));
                            single_chars.push('_');
                            continue;
                        }
                    }
                    let c1 = if chars[*pos] == '\\' && *pos + 1 < chars.len() {
                        *pos += 1;
                        chars[*pos]
                    } else {
                        chars[*pos]
                    };
                    *pos += 1;
                    let has_range = *pos + 1 < chars.len()
                        && chars[*pos] == '-'
                        && chars[*pos + 1] != ']'
                        && !(chars[*pos + 1] == '\\'
                            && matches!(chars.get(*pos + 2), Some('d' | 'w')));
                    if has_range {
                        *pos += 1;
                        let c2 = if chars[*pos] == '\\' && *pos + 1 < chars.len() {
                            *pos += 1;
                            chars[*pos]
                        } else {
                            chars[*pos]
                        };
                        *pos += 1;
                        ranges.push((c1, c2));
                    } else {
                        single_chars.push(c1);
                    }
                }
                if *pos < chars.len() && chars[*pos] == ']' {
                    *pos += 1;
                }
                AtomMatcher::CharClass {
                    ranges,
                    chars: single_chars,
                    negated,
                }
            }
            '\\' => {
                *pos += 1;
                if *pos < chars.len() {
                    let esc = chars[*pos];
                    *pos += 1;
                    match esc {
                        'd' => AtomMatcher::Digit,
                        'w' => AtomMatcher::Word,
                        other => AtomMatcher::Literal(other),
                    }
                } else {
                    AtomMatcher::Literal('\\')
                }
            }
            '.' => {
                *pos += 1;
                AtomMatcher::Any
            }
            lit => {
                *pos += 1;
                AtomMatcher::Literal(lit)
            }
        };

        let (min_rep, max_rep) = parse_quantifier(chars, pos);
        atoms.push(RegexAtom {
            matcher,
            min_rep,
            max_rep,
        });
    }
    RegexNode::Sequence(atoms)
}

fn parse_quantifier(chars: &[char], pos: &mut usize) -> (usize, usize) {
    if *pos >= chars.len() {
        return (1, 1);
    }
    match chars[*pos] {
        '?' => {
            *pos += 1;
            (0, 1)
        }
        '*' => {
            *pos += 1;
            (0, MAX_REGEX_QUANTIFIER_REP)
        }
        '+' => {
            *pos += 1;
            (1, MAX_REGEX_QUANTIFIER_REP)
        }
        '{' => {
            let start = *pos;
            *pos += 1;
            let mut min_s = String::new();
            while *pos < chars.len() && chars[*pos].is_ascii_digit() {
                if min_s.len() < 4 {
                    min_s.push(chars[*pos]);
                }
                *pos += 1;
            }
            if min_s.is_empty() {
                *pos = start;
                return (1, 1);
            }
            let min_v = min_s
                .parse::<usize>()
                .unwrap_or(1)
                .min(MAX_REGEX_QUANTIFIER_REP);
            if *pos < chars.len() && chars[*pos] == '}' {
                *pos += 1;
                return (min_v, min_v);
            }
            if *pos < chars.len() && chars[*pos] == ',' {
                *pos += 1;
                let mut max_s = String::new();
                while *pos < chars.len() && chars[*pos].is_ascii_digit() {
                    if max_s.len() < 4 {
                        max_s.push(chars[*pos]);
                    }
                    *pos += 1;
                }
                if *pos < chars.len() && chars[*pos] == '}' {
                    *pos += 1;
                    let max_v = if max_s.is_empty() {
                        MAX_REGEX_QUANTIFIER_REP
                    } else {
                        max_s
                            .parse::<usize>()
                            .unwrap_or(min_v)
                            .min(MAX_REGEX_QUANTIFIER_REP)
                    };
                    return (min_v, max_v.max(min_v));
                }
            }
            *pos = start;
            (1, 1)
        }
        _ => (1, 1),
    }
}

fn eval_node(
    node: &RegexNode,
    input: &[char],
    idx: usize,
    memo: &mut BTreeMap<(usize, usize), Vec<usize>>,
) -> Vec<usize> {
    let key = (node as *const RegexNode as usize, idx);
    if let Some(cached) = memo.get(&key) {
        return cached.clone();
    }
    let ends = match node {
        RegexNode::Alternation(branches) => {
            let mut ends = Vec::new();
            for b in branches {
                for end_idx in eval_node(b, input, idx, memo) {
                    if !ends.contains(&end_idx) {
                        ends.push(end_idx);
                    }
                }
            }
            ends
        }
        RegexNode::Sequence(atoms) => eval_sequence(atoms, input, idx, memo),
    };
    memo.insert(key, ends.clone());
    ends
}

/// Evaluates a sequence of `RegexAtom`s using iterative Thompson-style state-set propagation
/// over deduplicated input positions (`0..=input.len()`) with `(node, idx)` memoization,
/// eliminating exponential backtracking and tree re-evaluation (ReDoS).
fn eval_sequence(
    atoms: &[RegexAtom],
    input: &[char],
    idx: usize,
    memo: &mut BTreeMap<(usize, usize), Vec<usize>>,
) -> Vec<usize> {
    let mut current_positions = vec![idx];
    for atom in atoms {
        if current_positions.is_empty() || atom.min_rep > input.len() {
            return Vec::new();
        }
        let max_steps = atom.max_rep.min(input.len());
        let mut next_atom_positions = Vec::new();
        if atom.min_rep == 0 {
            for &p in &current_positions {
                if !next_atom_positions.contains(&p) {
                    next_atom_positions.push(p);
                }
            }
        }
        let mut frontier = current_positions;
        for step in 1..=max_steps {
            let mut next_frontier = Vec::new();
            for &p in &frontier {
                match &atom.matcher {
                    AtomMatcher::Group(g) => {
                        for end_p in eval_node(g, input, p, memo) {
                            if end_p == p {
                                if !next_atom_positions.contains(&p) {
                                    next_atom_positions.push(p);
                                }
                            } else if end_p > p && !next_frontier.contains(&end_p) {
                                next_frontier.push(end_p);
                            }
                        }
                    }
                    other => {
                        if p < input.len() && other.matches_char(input[p]) {
                            let end_p = p + 1;
                            if !next_frontier.contains(&end_p) {
                                next_frontier.push(end_p);
                            }
                        }
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            if step >= atom.min_rep {
                for &p in &next_frontier {
                    if !next_atom_positions.contains(&p) {
                        next_atom_positions.push(p);
                    }
                }
            }
            frontier = next_frontier;
        }
        current_positions = next_atom_positions;
    }
    current_positions
}

/// Validates a license plate string against `format_regex` (or `DEFAULT_LPR_FORMAT_REGEX` when `None`/empty).
/// Supports Mercosul (`[A-Z]{3}[0-9][A-Z0-9][0-9]{2}`, e.g. `ABC1D23`), legacy (`ABC1234`), and custom patterns.
pub fn validate_license_plate(plate: &str, format_regex: Option<&str>) -> bool {
    let normalized = normalize_license_plate(plate);
    if normalized.len() < 4 || normalized.len() > 10 {
        return false;
    }
    let pattern = format_regex
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_LPR_FORMAT_REGEX);
    if pattern.len() > MAX_REGEX_PATTERN_CHARS {
        return false;
    }
    let pat_chars: Vec<char> = pattern.chars().collect();
    let mut pos = 0usize;
    let ast = parse_regex_node(&pat_chars, &mut pos, 0);
    let in_chars: Vec<char> = normalized.chars().collect();
    let mut memo = BTreeMap::new();
    eval_node(&ast, &in_chars, 0, &mut memo).contains(&in_chars.len())
}

/// Decodes a `[timesteps, num_classes]` CTC output probability/logit tensor using greedy CTC collapse
/// (collapsing repeated non-blank indices and skipping `blank_idx`).
pub fn decode_lpr_ctc_logits(
    logits: &[f32],
    timesteps: usize,
    num_classes: usize,
    blank_idx: usize,
) -> Option<OcrPlateRead> {
    let required_len = timesteps.checked_mul(num_classes)?;
    if timesteps == 0 || num_classes < 2 || logits.len() < required_len {
        return None;
    }
    let mut decoded_chars = String::new();
    let mut char_confs = Vec::new();
    let mut prev_class: Option<usize> = None;

    for t in 0..timesteps {
        let row = &logits[t * num_classes..(t + 1) * num_classes];
        let mut best_c = blank_idx;
        let mut best_val = f32::NEG_INFINITY;
        for (c, &v) in row.iter().enumerate() {
            if v.is_finite() && v > best_val {
                best_val = v;
                best_c = c;
            }
        }
        if !best_val.is_finite() {
            prev_class = Some(blank_idx);
            continue;
        }
        // Convert logit to probability if outside [0.0, 1.0]
        let prob = if (0.0..=1.0).contains(&best_val)
            && row
                .iter()
                .all(|&x| x.is_finite() && (0.0..=1.001).contains(&x))
        {
            best_val
        } else {
            let max_v = best_val;
            let sum_exp: f32 = row
                .iter()
                .filter(|&&x| x.is_finite())
                .map(|&x| (x - max_v).exp())
                .sum();
            if sum_exp.is_finite() && sum_exp > 0.0 {
                (1.0 / sum_exp).clamp(0.0, 1.0)
            } else {
                0.0
            }
        };

        if best_c != blank_idx && prev_class != Some(best_c) {
            let alpha_idx = if best_c > blank_idx {
                best_c - 1
            } else {
                best_c
            };
            if let Some(&ch_byte) = LPR_CTC_ALPHABET.get(alpha_idx) {
                decoded_chars.push(ch_byte as char);
                char_confs.push(prob);
            }
        }
        prev_class = Some(best_c);
    }

    if decoded_chars.is_empty() {
        return None;
    }
    let avg_conf = char_confs.iter().sum::<f32>() / (char_confs.len() as f32);
    Some(OcrPlateRead {
        text: decoded_chars,
        confidence: avg_conf,
        char_confidences: char_confs,
    })
}

/// Aggregates character predictions across consecutive frames of the same track using
/// confidence-weighted majority voting, validates against `lpr_cfg.format_regex`, and resolves `known_plates`.
pub fn vote_license_plate_reads(
    reads: &[OcrPlateRead],
    lpr_cfg: &LprConfig,
) -> Option<LprConsensusResult> {
    let min_conf = if lpr_cfg.min_confidence.is_finite() {
        lpr_cfg.min_confidence
    } else {
        0.0
    };
    let cleaned: Vec<(Vec<char>, f32, Vec<f32>)> = reads
        .iter()
        .filter_map(|r| {
            let norm = normalize_license_plate(&r.text);
            if norm.is_empty() || !r.confidence.is_finite() || r.confidence <= 0.0 {
                return None;
            }
            let read_conf = r.confidence.clamp(0.01, 1.0);
            let chars: Vec<char> = norm.chars().collect();
            let c_confs = if r.char_confidences.len() == chars.len() {
                r.char_confidences
                    .iter()
                    .map(|&c| {
                        if c.is_finite() {
                            c.clamp(0.01, 1.0)
                        } else {
                            read_conf
                        }
                    })
                    .collect()
            } else {
                vec![read_conf; chars.len()]
            };
            Some((chars, read_conf, c_confs))
        })
        .collect();

    if cleaned.is_empty() {
        return None;
    }

    // Group candidate reads by plate character length and rank lengths by total confidence weight
    let mut len_weights: BTreeMap<usize, f32> = BTreeMap::new();
    for (chars, conf, _) in &cleaned {
        *len_weights.entry(chars.len()).or_default() += *conf;
    }
    let mut ranked_lengths: Vec<(usize, f32)> = len_weights.into_iter().collect();
    ranked_lengths.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.0.cmp(&a.0)));

    for (target_len, _) in ranked_lengths {
        let same_len: Vec<&(Vec<char>, f32, Vec<f32>)> = cleaned
            .iter()
            .filter(|(c, _, _)| c.len() == target_len)
            .collect();
        if same_len.is_empty() || target_len == 0 {
            continue;
        }

        let mut voted_plate = String::with_capacity(target_len);
        let mut pos_conf_sum = 0.0f32;

        for pos in 0..target_len {
            let mut char_votes: BTreeMap<char, (f32, usize)> = BTreeMap::new();
            for (chars, read_conf, c_confs) in &same_len {
                let ch = chars[pos];
                let w = c_confs
                    .get(pos)
                    .copied()
                    .unwrap_or(*read_conf)
                    .clamp(0.01, 1.0);
                let entry = char_votes.entry(ch).or_insert((0.0, 0));
                entry.0 += w;
                entry.1 += 1;
            }
            let (best_ch, (best_weight, best_count)) = char_votes
                .into_iter()
                .max_by(|a, b| a.1.0.total_cmp(&b.1.0).then_with(|| a.1.1.cmp(&b.1.1)))
                .unwrap();
            voted_plate.push(best_ch);
            pos_conf_sum += best_weight / (best_count.max(1) as f32);
        }

        let consolidated_conf = pos_conf_sum / (target_len as f32);
        if consolidated_conf.is_finite()
            && consolidated_conf >= min_conf
            && validate_license_plate(&voted_plate, lpr_cfg.format_regex.as_deref())
        {
            let sub_label = lookup_known_plate(&voted_plate, &lpr_cfg.known_plates);
            return Some(LprConsensusResult {
                plate: voted_plate,
                confidence: consolidated_conf,
                sub_label,
                votes: same_len.len(),
            });
        }
    }

    None
}

/// Convenience helper that performs rolling majority voting over unweighted/uniformly-weighted plate strings.
pub fn vote_license_plate_strings(
    candidates: &[&str],
    lpr_cfg: &LprConfig,
) -> Option<LprConsensusResult> {
    let reads: Vec<OcrPlateRead> = candidates
        .iter()
        .map(|s| OcrPlateRead::new(*s, 0.90))
        .collect();
    vote_license_plate_reads(&reads, lpr_cfg)
}

/// Stateful per-track rolling multi-frame LPR voter that buffers OCR reads across consecutive frames
/// of each active track (`car`, `truck`, `motorcycle`, or standalone `license_plate`).
#[derive(Debug, Clone)]
pub struct LprTrackVoter {
    max_window: usize,
    tracks: BTreeMap<String, Vec<OcrPlateRead>>,
    track_order: std::collections::VecDeque<String>,
}

impl Default for LprTrackVoter {
    fn default() -> Self {
        Self::new(12)
    }
}

impl LprTrackVoter {
    pub fn new(max_window: usize) -> Self {
        Self {
            max_window: max_window.clamp(1, 256),
            tracks: BTreeMap::new(),
            track_order: std::collections::VecDeque::new(),
        }
    }

    pub fn record_read(
        &mut self,
        track_id: &str,
        read: OcrPlateRead,
        lpr_cfg: &LprConfig,
    ) -> Option<LprConsensusResult> {
        if track_id.is_empty() || track_id.len() > 128 {
            return None;
        }
        if !self.tracks.contains_key(track_id) {
            while self.tracks.len() >= MAX_ACTIVE_LPR_TRACKS {
                if let Some(oldest) = self.track_order.pop_front() {
                    self.tracks.remove(&oldest);
                } else if let Some(first_key) = self.tracks.keys().next().cloned() {
                    self.tracks.remove(&first_key);
                } else {
                    break;
                }
            }
            self.track_order.push_back(track_id.to_string());
        } else {
            self.track_order.retain(|k| k != track_id);
            self.track_order.push_back(track_id.to_string());
        }
        let entry = self.tracks.entry(track_id.to_string()).or_default();
        entry.push(read);
        if entry.len() > self.max_window {
            let drain_count = entry.len() - self.max_window;
            entry.drain(0..drain_count);
        }
        vote_license_plate_reads(entry, lpr_cfg)
    }

    pub fn consensus_for_track(
        &self,
        track_id: &str,
        lpr_cfg: &LprConfig,
    ) -> Option<LprConsensusResult> {
        let reads = self.tracks.get(track_id)?;
        vote_license_plate_reads(reads, lpr_cfg)
    }

    pub fn remove_track(&mut self, track_id: &str) -> Option<Vec<OcrPlateRead>> {
        self.track_order.retain(|k| k != track_id);
        self.tracks.remove(track_id)
    }
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

    #[test]
    fn ort_c_api_session_and_detect_region_executes_full_vtable_pipeline() {
        static RELEASE_COUNT: AtomicU64 = AtomicU64::new(0);
        static INTRA_THREADS_SET: AtomicU64 = AtomicU64::new(0);
        static OPT_LEVEL_SET: AtomicU64 = AtomicU64::new(0);
        static MOCK_OUT_TENSOR: [f32; 7] = [0.0, 1.0, 0.94, 0.1, 0.1, 0.5, 0.8];

        unsafe extern "C" fn mock_release(ptr: *mut c_void) {
            if !ptr.is_null() {
                RELEASE_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }
        unsafe extern "C" fn mock_create_env(
            _lvl: c_int,
            _id: *const c_char,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x1001usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_opts(out: *mut *mut c_void) -> *mut c_void {
            unsafe {
                *out = 0x1002usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_set_intra(_opts: *mut c_void, threads: c_int) -> *mut c_void {
            INTRA_THREADS_SET.store(threads as u64, Ordering::SeqCst);
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_set_inter(_opts: *mut c_void, _threads: c_int) -> *mut c_void {
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_set_opt(_opts: *mut c_void, level: c_int) -> *mut c_void {
            OPT_LEVEL_SET.store(level as u64, Ordering::SeqCst);
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_session(
            _env: *const c_void,
            _model: *const c_char,
            _opts: *const c_void,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x1003usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_alloc(out: *mut *mut c_void) -> *mut c_void {
            unsafe {
                *out = 0x1004usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_in_name(
            _sess: *const c_void,
            _idx: usize,
            _alloc: *mut c_void,
            out: *mut *mut c_char,
        ) -> *mut c_void {
            unsafe {
                *out = c"images".as_ptr() as *mut c_char;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_out_name(
            _sess: *const c_void,
            _idx: usize,
            _alloc: *mut c_void,
            out: *mut *mut c_char,
        ) -> *mut c_void {
            unsafe {
                *out = c"output0".as_ptr() as *mut c_char;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_in_type(
            _sess: *const c_void,
            _idx: usize,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x1005usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_cast_tensor_info(
            _ti: *const c_void,
            out: *mut *const c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x1006usize as *const c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_elem_type(
            _info: *const c_void,
            out: *mut c_int,
        ) -> *mut c_void {
            unsafe {
                *out = ORT_TENSOR_ELEMENT_DATA_TYPE_FLOAT;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_cpu_mem(
            _a: c_int,
            _m: c_int,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x1007usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_tensor(
            _mem: *const c_void,
            _data: *mut c_void,
            _byte_len: usize,
            _shape: *const i64,
            _shape_len: usize,
            _ty: c_int,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x2001usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_run(
            _sess: *mut c_void,
            _run_opts: *const c_void,
            _in_names: *const *const c_char,
            _in_vals: *const *const c_void,
            _in_len: usize,
            _out_names: *const *const c_char,
            _out_len: usize,
            out_vals: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out_vals = 0x2002usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_mut_data(
            _val: *mut c_void,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = MOCK_OUT_TENSOR.as_ptr() as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_type_shape(
            _val: *const c_void,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x2003usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_elem_count(
            _info: *const c_void,
            out: *mut usize,
        ) -> *mut c_void {
            unsafe {
                *out = MOCK_OUT_TENSOR.len();
            }
            std::ptr::null_mut()
        }

        let mut vtable: [*const c_void; 101] = [std::ptr::null(); 101];
        vtable[3] = mock_create_env as *const c_void;
        vtable[7] = mock_create_session as *const c_void;
        vtable[9] = mock_run as *const c_void;
        vtable[10] = mock_create_opts as *const c_void;
        vtable[23] = mock_set_opt as *const c_void;
        vtable[24] = mock_set_intra as *const c_void;
        vtable[25] = mock_set_inter as *const c_void;
        vtable[33] = mock_get_in_type as *const c_void;
        vtable[36] = mock_get_in_name as *const c_void;
        vtable[37] = mock_get_out_name as *const c_void;
        vtable[49] = mock_create_tensor as *const c_void;
        vtable[51] = mock_get_mut_data as *const c_void;
        vtable[55] = mock_cast_tensor_info as *const c_void;
        vtable[60] = mock_get_elem_type as *const c_void;
        vtable[64] = mock_get_elem_count as *const c_void;
        vtable[65] = mock_get_type_shape as *const c_void;
        vtable[69] = mock_create_cpu_mem as *const c_void;
        vtable[78] = mock_get_alloc as *const c_void;
        for idx in [92, 93, 94, 95, 96, 98, 99, 100] {
            vtable[idx] = mock_release as *const c_void;
        }

        let (ort_ctx, actual_dev) = unsafe {
            AiDetectorEngine::init_ort_session_from_vtable(
                std::ptr::null_mut(),
                vtable.as_ptr(),
                Path::new("/tmp/yolo11.onnx"),
                "TensorRT",
                64,
                64,
            )
            .unwrap()
        };
        assert_eq!(
            actual_dev, "CPU",
            "TensorRT without GPU handle must fall back to CPU"
        );
        assert_eq!(INTRA_THREADS_SET.load(Ordering::SeqCst), 1);
        assert_eq!(
            OPT_LEVEL_SET.load(Ordering::SeqCst),
            ORT_GRAPH_OPT_ENABLE_ALL as u64
        );

        let cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c.yml")).unwrap();
        let engine = AiDetectorEngine::new(&cfg);
        {
            let mut g = engine.inner.lock().unwrap();
            g.model_w = 64;
            g.model_h = 64;
            g.nchw_layout = true;
            g.active_backend = "onnxruntime".to_string();
            g.active_device = actual_dev;
            g.backend = Some(InferenceRuntimeBackend::OnnxRuntime(ort_ctx));
        }

        let crop = vec![128u8; 64 * 64 * 3];
        let region = BoundingBox::new(0, 0, 640, 480);
        let dets = engine.detect_region(&crop, region, 1280, 720, 0.5);
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].label, "person");
        assert!((dets[0].score - 0.94).abs() < 1e-4);

        drop(engine);
        assert!(
            RELEASE_COUNT.load(Ordering::SeqCst) >= 7,
            "all ORT handles (type_info, shape_info, in_val, out_val, mem_info, session, opts, env) must be released"
        );
    }

    #[test]
    fn fallback_mode_covers_runtime_catalog_detect_region_reload_and_pool_routing() {
        let yaml = r#"
detectors:
  primary_cpu:
    type: openvino
    device: CPU
    model:
      width: 300
      height: 300
      input_pixel_format: bgr
      path: /nonexistent/ssdlite_mobilenet_v2.xml
  aux_npu:
    type: openvino
    device: NPU
    model:
      width: 416
      height: 416
      input_pixel_format: rgb
      path: /nonexistent/yolox_s.xml
version: 0.18-0
"#;
        let mut cfg =
            NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/fallback_cfg.yml")).unwrap();
        let engine = AiDetectorEngine::new(&cfg);

        // 1. runtime_catalog() when OpenVINO model/library is not loaded
        let cat = engine.runtime_catalog();
        assert!(!cat.loaded);
        assert_eq!(cat.in_bytes, 0);
        assert_eq!(cat.out_floats, 0);
        assert_eq!(cat.active_backend, "openvino");
        assert!(cat.devices.iter().any(|d| d.id == "CPU"));
        assert!(cat.devices.iter().any(|d| d.id == "GPU"));
        assert!(cat.devices.iter().any(|d| d.id == "NPU"));
        assert!(cat.devices.iter().any(|d| d.id == "AUTO" && d.available));
        assert!(!cat.discovered_models.is_empty());

        // 2. detect_region() safe fallback execution when backend is None
        let tensor = vec![0u8; 300 * 300 * 3];
        let region = BoundingBox::new(0, 0, 300, 300);
        let fallback_dets = engine.detect_region(&tensor, region, 1280, 720, 0.5);
        assert!(fallback_dets.is_empty());
        assert!(engine.avg_inference_ms() > 0.0);
        let _ = engine.cumulative_infer_us();

        // 3. named_detector() and avg_inference_ms_for() on AiDetectorEngine
        let aux = engine
            .named_detector(Some("aux_npu"))
            .expect("aux_npu must exist in engine pool");
        assert_eq!(aux.model_dimensions(), (416, 416, false));
        assert_eq!(aux.active_device(), "NPU");
        assert!(engine.named_detector(Some("missing_det")).is_none());
        assert!(engine.named_detector(None).is_none());
        assert!(engine.avg_inference_ms_for("aux_npu") > 0.0);
        assert_eq!(
            engine.avg_inference_ms_for("missing_det"),
            engine.avg_inference_ms()
        );

        // 4. DetectorPool::get, detector_names
        let pool = DetectorPool::new(&cfg);
        assert_eq!(pool.detector_names(), vec!["aux_npu", "primary_cpu"]);
        assert_eq!(
            pool.get(Some("primary_cpu")).model_dimensions(),
            (300, 300, true)
        );
        assert_eq!(
            pool.get(Some("aux_npu")).model_dimensions(),
            (416, 416, false)
        );
        // Unknown name and None fall back deterministically
        assert_eq!(
            pool.get(Some("unknown")).model_dimensions().0,
            pool.get(None).model_dimensions().0
        );

        // 5. reload() updates primary dimensions, device, and internal named detector pool
        cfg.model.width = 640;
        cfg.model.height = 640;
        cfg.model.input_pixel_format = "rgb".to_string();
        cfg.detectors.clear();
        cfg.detectors.insert(
            "reloaded_ort".to_string(),
            DetectorEntryConfig {
                detector_type: "onnxruntime".to_string(),
                device: "CPU".to_string(),
                model: Some(ninjanvr_config::ModelConfig {
                    width: 640,
                    height: 640,
                    input_tensor: "nchw".to_string(),
                    input_pixel_format: "rgb".to_string(),
                    path: "/nonexistent/yolov11n.onnx".to_string(),
                    labelmap_path: "/nonexistent/labels.txt".to_string(),
                    model_type: "yolo-generic".to_string(),
                }),
            },
        );
        engine.reload(&cfg);
        assert_eq!(engine.model_dimensions(), (640, 640, false));
        assert!(engine.named_detector(Some("aux_npu")).is_none());
        let reloaded = engine.named_detector(Some("reloaded_ort")).unwrap();
        assert_eq!(reloaded.model_dimensions(), (640, 640, false));
    }

    #[test]
    fn decodes_dynamic_15_class_transposed_yolov9_and_row_major_yolov5_tensors() {
        assert_eq!(expected_yolo_anchors(320, 320), 2100);
        assert_eq!(expected_yolo_anchors(416, 416), 3549);
        assert_eq!(expected_yolo_anchors(640, 640), 8400);

        let classes_15 = [
            "person",
            "bicycle",
            "car",
            "motorcycle",
            "bus",
            "truck",
            "dog",
            "cat",
            "face",
            "license_plate",
            "package",
            "waste_bin",
            "correios",
            "mercado_livre",
            "amazon",
        ];
        let mut custom_labelmap = BTreeMap::new();
        for (idx, &lbl) in classes_15.iter().enumerate() {
            custom_labelmap.insert(idx as i32, lbl.to_string());
        }

        let mut cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c15.yml")).unwrap();
        cfg.model.model_type = "yolo-generic".to_string();
        cfg.model.width = 320;
        cfg.model.height = 320;
        cfg.model.input_tensor = "nchw".to_string();
        cfg.model.input_pixel_format = "rgb".to_string();

        let engine = AiDetectorEngine::new(&cfg);
        engine.set_labelmap(custom_labelmap.clone());
        let region = BoundingBox::new(0, 0, 1280, 720);

        // 1. Transposed [1, 19, 2100] (4 + 15 classes at 320x320) with duplicate overlapping anchor for NMS
        let n_320 = 2100usize;
        let mut t_320 = vec![0.0f32; 19 * n_320];
        // Anchor 0: person at center (160, 160), size (80, 160), score 0.92
        t_320[0] = 160.0;
        t_320[n_320] = 160.0;
        t_320[2 * n_320] = 80.0;
        t_320[3 * n_320] = 160.0;
        t_320[4 * n_320] = 0.92; // class 0 = person
        // Anchor 1: duplicate person box with lower score 0.71 -> suppressed by NMS
        t_320[1] = 160.0;
        t_320[n_320 + 1] = 160.0;
        t_320[2 * n_320 + 1] = 80.0;
        t_320[3 * n_320 + 1] = 160.0;
        t_320[4 * n_320 + 1] = 0.71;
        // Anchor 2: face (class 8) at (160, 100), size (24, 24), score 0.88
        t_320[2] = 160.0;
        t_320[n_320 + 2] = 100.0;
        t_320[2 * n_320 + 2] = 24.0;
        t_320[3 * n_320 + 2] = 24.0;
        t_320[(4 + 8) * n_320 + 2] = 0.88;
        // Anchor 3: package (class 10) at (260, 260), size (40, 40), score 0.84
        t_320[3] = 260.0;
        t_320[n_320 + 3] = 260.0;
        t_320[2 * n_320 + 3] = 40.0;
        t_320[3 * n_320 + 3] = 40.0;
        t_320[(4 + 10) * n_320 + 3] = 0.84;

        let dets_320 = engine.parse_raw_output_slice(&t_320, region, 1280, 720, 0.50);
        assert_eq!(
            dets_320.len(),
            3,
            "duplicate person anchor must be deduplicated by NMS"
        );
        assert_eq!(dets_320[0].label, "person");
        assert!((dets_320[0].score - 0.92).abs() < 1e-4);
        assert_eq!(dets_320[0].bbox, BoundingBox::new(480, 180, 800, 540));
        assert_eq!(dets_320[1].label, "face");
        assert_eq!(dets_320[2].label, "package");

        // 2. Transposed [1, 19, 8400] (4 + 15 classes at 640x640)
        cfg.model.width = 640;
        cfg.model.height = 640;
        engine.reload(&cfg);
        engine.set_labelmap(custom_labelmap.clone());

        let n_640 = 8400usize;
        let mut t_640 = vec![0.0f32; 19 * n_640];
        // Anchor 10: license_plate (class 9) at (320, 480), size (64, 32), score 0.91
        t_640[10] = 320.0;
        t_640[n_640 + 10] = 480.0;
        t_640[2 * n_640 + 10] = 64.0;
        t_640[3 * n_640 + 10] = 32.0;
        t_640[(4 + 9) * n_640 + 10] = 0.91;

        let dets_640 = engine.parse_raw_output_slice(&t_640, region, 1280, 720, 0.50);
        assert_eq!(dets_640.len(), 1);
        assert_eq!(dets_640[0].label, "license_plate");
        assert!((dets_640[0].score - 0.91).abs() < 1e-4);

        // 3. Custom anchor count fallback [1, 19, 32] via state.labelmap class count (c_cfg = 15)
        let n_custom = 32usize;
        let mut t_custom = vec![0.0f32; 19 * n_custom];
        t_custom[0] = 320.0;
        t_custom[n_custom] = 320.0;
        t_custom[2 * n_custom] = 128.0;
        t_custom[3 * n_custom] = 128.0;
        t_custom[(4 + 12) * n_custom] = 0.89; // class 12 = correios
        let dets_custom = engine.parse_raw_output_slice(&t_custom, region, 1280, 720, 0.50);
        assert_eq!(dets_custom.len(), 1);
        assert_eq!(dets_custom[0].label, "correios");

        // 4. Row-major [1, N, 20] (5 + 15 classes: cx, cy, w, h, obj_conf, cls_0..cls_14)
        let mut rm_20 = vec![0.0f32; 25 * 20];
        rm_20[0] = 320.0; // cx
        rm_20[1] = 320.0; // cy
        rm_20[2] = 160.0; // w
        rm_20[3] = 160.0; // h
        rm_20[4] = 0.95; // obj_conf
        rm_20[5 + 9] = 0.90; // class 9 = license_plate
        let dets_rm = engine.parse_raw_output_slice(&rm_20, region, 1280, 720, 0.50);
        assert_eq!(dets_rm.len(), 1);
        assert_eq!(dets_rm[0].label, "license_plate");
        assert!((dets_rm[0].score - 0.95 * 0.90).abs() < 1e-4);
    }

    #[test]
    fn l2_normalization_and_cosine_similarity_boundary_and_face_gallery_matching() {
        // 1. L2 normalization produces unit norm = 1.0 and handles zero vectors safely
        let mut v = vec![3.0f32, 4.0f32, 0.0f32];
        l2_normalize_embedding(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);

        let mut zero_v = vec![0.0f32; FACE_EMBEDDING_DIM];
        l2_normalize_embedding(&mut zero_v);
        assert!(zero_v.iter().all(|&x| x == 0.0));
        assert_eq!(cosine_similarity(&zero_v, &zero_v), 0.0);

        // 2. Cosine similarity: 1.0 for identical, 0.0 for orthogonal, and 0.68 boundary
        let mut a = vec![0.0f32; FACE_EMBEDDING_DIM];
        a[0] = 1.0;
        let mut b_ident = vec![0.0f32; FACE_EMBEDDING_DIM];
        b_ident[0] = 1.0;
        assert!((cosine_similarity(&a, &b_ident) - 1.0).abs() < 1e-6);

        let mut b_ortho = vec![0.0f32; FACE_EMBEDDING_DIM];
        b_ortho[1] = 1.0;
        assert!(cosine_similarity(&a, &b_ortho).abs() < 1e-6);

        let mut b_boundary = vec![0.0f32; FACE_EMBEDDING_DIM];
        b_boundary[0] = 0.68;
        b_boundary[1] = (1.0f32 - 0.68 * 0.68).sqrt();
        let sim_boundary = cosine_similarity(&a, &b_boundary);
        assert!((sim_boundary - 0.68).abs() < 1e-5);

        // 3. Gallery enrollment (memory + disk) and threshold boundary 0.68 matching vs "unknown" fallback
        let tmp_dir =
            std::env::temp_dir().join(format!("ninjanvr_face_gallery_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);

        let mut gallery = FaceGallery::new();
        gallery.enroll_embedding("Bruno", a.clone());

        // At exact 0.68 boundary -> matches "Bruno"
        let match_at_thresh = gallery
            .match_embedding(&b_boundary, 0.68, true)
            .expect("must match at 0.68 boundary");
        assert_eq!(match_at_thresh.sub_label, "Bruno");
        assert!(match_at_thresh.is_known);
        assert!((match_at_thresh.score - 0.68).abs() < 1e-5);

        // Below 0.68 threshold (e.g. 0.67) -> falls back to "unknown" when unknown_alert = true
        let mut b_below = vec![0.0f32; FACE_EMBEDDING_DIM];
        b_below[0] = 0.67;
        b_below[1] = (1.0f32 - 0.67 * 0.67).sqrt();
        let match_below = gallery
            .match_embedding(&b_below, 0.68, true)
            .expect("must return unknown when unknown_alert is true");
        assert_eq!(match_below.sub_label, "unknown");
        assert!(!match_below.is_known);

        // Below threshold when unknown_alert = false -> returns None
        assert!(gallery.match_embedding(&b_below, 0.68, false).is_none());

        // 4. Disk enrollment under {faces_dir}/<name>/*.jpg and reload
        let bruno_sample = b"fake-jpeg-bruno-face-crop-001";
        let stranger_sample = b"completely-different-visitor-face-crop-999";
        let (saved_path, emb) = gallery
            .enroll_to_dir(&tmp_dir, "Bruno", "crop_01", "jpg", bruno_sample)
            .unwrap();
        assert!(saved_path.exists());
        assert_eq!(emb.len(), FACE_EMBEDDING_DIM);

        let reloaded = FaceGallery::load_from_dir(&tmp_dir).unwrap();
        let profiles = reloaded.list_profiles(Some(&tmp_dir));
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "Bruno");
        assert_eq!(profiles[0].sample_count, 1);

        let known_res = reloaded
            .match_face_bytes(bruno_sample, 0.68, true)
            .expect("enrolled sample must match Bruno");
        assert_eq!(known_res.sub_label, "Bruno");
        assert!(known_res.is_known);
        assert!((known_res.score - 1.0).abs() < 1e-5);

        let unknown_res = reloaded
            .match_face_bytes(stranger_sample, 0.68, true)
            .expect("stranger sample must trigger unknown");
        assert_eq!(unknown_res.sub_label, "unknown");
        assert!(!unknown_res.is_known);

        // 5. Face crop bbox resolution (explicit face attribute box vs upper-body fallback)
        let person_box = BoundingBox::new(100, 100, 220, 300); // 120x200 (height >= 120)
        let face_attr = BoundingBox::new(130, 110, 190, 170); // 60x60 = 3600 px^2
        assert_eq!(
            resolve_face_crop_bbox(person_box, Some(face_attr), 1280, 720, 750),
            Some(face_attr)
        );
        let fallback_head =
            resolve_face_crop_bbox(person_box, None, 1280, 720, 750).expect("head fallback");
        assert!(fallback_head.area() >= 750);
        assert!(fallback_head.inside(&person_box));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn lpr_ctc_decoding_multi_frame_voting_regex_validation_and_known_plates() {
        // 1. Validate Mercosul (ABC1D23), legacy (ABC1234 / ABC-1234), and international plates
        assert!(validate_license_plate("ABC1D23", None));
        assert!(validate_license_plate("abc-1d23", None));
        assert!(validate_license_plate("ABC1234", None));
        assert!(validate_license_plate("ABC-1234", None));
        assert!(validate_license_plate("AB12CDE", None));
        assert!(!validate_license_plate("12", None));
        assert!(!validate_license_plate("INVALIDPLATE12345", None));

        // Strict Mercosul-only custom regex
        let strict_mercosul = Some("^[A-Z]{3}[0-9][A-Z][0-9]{2}$");
        assert!(validate_license_plate("ABC1D23", strict_mercosul));
        assert!(!validate_license_plate("ABC1234", strict_mercosul));

        // 2. Greedy CTC decoding collapses duplicate tokens and skips blank token (idx 36)
        // Alphabet: 0..10 = '0'..'9', 10..36 = 'A'..'Z', 36 = blank
        let num_classes = 37;
        let blank = 36;
        // Sequence of classes: A (10), A (10), blank (36), B (11), C (12), 1 (1), D (13), 2 (2), 3 (3)
        let seq = [10usize, 10, 36, 11, 12, 1, 13, 2, 3];
        let mut logits = vec![0.01f32; seq.len() * num_classes];
        for (t, &cls) in seq.iter().enumerate() {
            logits[t * num_classes + cls] = 0.94;
        }
        let decoded = decode_lpr_ctc_logits(&logits, seq.len(), num_classes, blank).unwrap();
        assert_eq!(decoded.text, "ABC1D23");
        assert!((decoded.confidence - 0.94).abs() < 1e-3);
        assert_eq!(decoded.char_confidences.len(), 7);

        // 3. Multi-frame rolling character majority voting + known_plates lookup
        let mut known_plates = BTreeMap::new();
        known_plates.insert("ABC1D23".to_string(), "Family SUV".to_string());
        known_plates.insert("XYZ9876".to_string(), "Delivery Truck".to_string());

        let lpr_cfg = LprConfig {
            enabled: true,
            detector_model_path: None,
            ocr_model_path: None,
            min_confidence: 0.75,
            format_regex: Some("^[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$".to_string()),
            known_plates,
        };

        let string_vote =
            vote_license_plate_strings(&["ABC1D23", "ABC1023", "ABC1D23"], &lpr_cfg).unwrap();
        assert_eq!(string_vote.plate, "ABC1D23");
        assert_eq!(string_vote.sub_label.as_deref(), Some("Family SUV"));
        assert_eq!(string_vote.votes, 3);

        // 4. Confidence-weighted voting across consecutive frames on AiDetectorEngine / LprTrackVoter
        let cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/lpr_eng.yml"))
                .unwrap();
        let engine = AiDetectorEngine::new(&cfg);

        // Frame 1: slightly blurred read "ABC1023" (conf 0.76)
        let r1 = engine
            .record_lpr_track_read("trk_car_1", OcrPlateRead::new("ABC1023", 0.76), &lpr_cfg)
            .unwrap();
        assert_eq!(r1.plate, "ABC1023");
        assert_eq!(r1.sub_label, None);

        // Frame 2: sharp read "ABC1D23" with higher confidence (0.93) outweighs single blurred frame!
        let r2 = engine
            .record_lpr_track_read("trk_car_1", OcrPlateRead::new("ABC1D23", 0.93), &lpr_cfg)
            .unwrap();
        assert_eq!(r2.plate, "ABC1D23");
        assert_eq!(r2.sub_label.as_deref(), Some("Family SUV"));

        // Frame 3: confirming read "ABC1D23" (0.91)
        let r3 = engine
            .record_lpr_track_read("trk_car_1", OcrPlateRead::new("ABC1D23", 0.91), &lpr_cfg)
            .unwrap();
        assert_eq!(r3.plate, "ABC1D23");
        assert_eq!(r3.votes, 3);
        assert_eq!(
            engine
                .lpr_consensus_for_track("trk_car_1", &lpr_cfg)
                .unwrap()
                .plate,
            "ABC1D23"
        );

        let finalized = engine.finalize_lpr_track("trk_car_1", &lpr_cfg).unwrap();
        assert_eq!(finalized.plate, "ABC1D23");
        assert!(
            engine
                .lpr_consensus_for_track("trk_car_1", &lpr_cfg)
                .is_none()
        );
    }

    #[test]
    fn loads_canonical_15_class_security_labelmap_and_decodes_all_15_classes() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root");
        let labelmap_path = repo_root.join("models/ninjanvr_security_15cl.txt");
        let taxonomy_path = repo_root.join("scripts/ai/taxonomy.json");

        assert!(
            labelmap_path.exists(),
            "models/ninjanvr_security_15cl.txt must exist"
        );
        assert!(
            taxonomy_path.exists(),
            "scripts/ai/taxonomy.json must exist"
        );

        let taxonomy_raw = std::fs::read_to_string(&taxonomy_path).unwrap();
        assert!(taxonomy_raw.contains("\"num_classes\": 15"));
        let classes_section = taxonomy_raw
            .split("\"classes\": [")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .expect("taxonomy.json must contain classes array");
        let tax_classes: Vec<String> = classes_section
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim().trim_end_matches(',');
                if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
                    Some(trimmed[1..trimmed.len() - 1].to_string())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(tax_classes.len(), 15);

        let expected_15 = [
            "person",
            "face",
            "car",
            "motorcycle",
            "bicycle",
            "truck",
            "bus",
            "license_plate",
            "dog",
            "cat",
            "bird",
            "package",
            "waste_bin",
            "backpack",
            "umbrella",
        ];

        let mut cfg = NinjaConfig::from_yaml_str(
            "version: 0.18-0\n",
            PathBuf::from("/tmp/ninjanvr_15cl_test.yml"),
        )
        .unwrap();
        cfg.model.labelmap_path = labelmap_path.to_string_lossy().to_string();
        cfg.model.model_type = "yolo-generic".to_string();
        cfg.model.width = 320;
        cfg.model.height = 320;
        cfg.model.input_tensor = "nchw".to_string();
        cfg.model.input_pixel_format = "rgb".to_string();

        let loaded_labelmap = cfg.load_labelmap();
        assert_eq!(loaded_labelmap.len(), 15);
        for (idx, &expected_label) in expected_15.iter().enumerate() {
            let key = idx.to_string();
            assert_eq!(
                loaded_labelmap.get(&key).map(String::as_str),
                Some(expected_label),
                "labelmap index {idx} must match canonical class '{expected_label}'"
            );
            assert_eq!(
                tax_classes[idx].as_str(),
                expected_label,
                "taxonomy.json classes[{idx}] must match '{expected_label}'"
            );
            assert!(
                taxonomy_raw.contains(&format!("\"{key}\": \"{expected_label}\"")),
                "taxonomy.json id_to_name must map {key} -> {expected_label}"
            );
            assert!(
                taxonomy_raw.contains(&format!("\"{expected_label}\": {idx}")),
                "taxonomy.json name_to_id must map {expected_label} -> {idx}"
            );
        }

        let engine = AiDetectorEngine::new(&cfg);
        let region = BoundingBox::new(0, 0, 1280, 720);
        let num_anchors = 2100usize;
        assert_eq!(expected_yolo_anchors(320, 320), num_anchors);

        // Synthetic [1, 19, 2100] transposed YOLO output tensor with 1 detection per class (0..14)
        let mut tensor = vec![0.0f32; 19 * num_anchors];
        for cls_idx in 0..15usize {
            let anchor = cls_idx * 20;
            let col = (cls_idx % 5) as f32;
            let row = (cls_idx / 5) as f32;
            let cx = 40.0 + col * 60.0;
            let cy = 50.0 + row * 90.0;
            let score = 0.96 - (cls_idx as f32) * 0.01;

            tensor[anchor] = cx;
            tensor[num_anchors + anchor] = cy;
            tensor[2 * num_anchors + anchor] = 28.0;
            tensor[3 * num_anchors + anchor] = 36.0;
            tensor[(4 + cls_idx) * num_anchors + anchor] = score;
        }

        let dets = engine.parse_raw_output_slice(&tensor, region, 1280, 720, 0.50);
        assert_eq!(
            dets.len(),
            15,
            "all 15 canonical security classes must be decoded from [1, 19, 2100] tensor"
        );
        for (cls_idx, &expected_label) in expected_15.iter().enumerate() {
            assert_eq!(dets[cls_idx].label, expected_label);
            let expected_score = 0.96 - (cls_idx as f32) * 0.01;
            assert!((dets[cls_idx].score - expected_score).abs() < 1e-4);
        }
    }

    #[test]
    fn sanitize_face_profile_name_blocks_traversal_reserved_names_and_strips_symbols() {
        for forbidden in [
            "",
            "   ",
            "..",
            "../etc/passwd",
            "alice/bob",
            "alice\\bob",
            "unknown",
            "UNKNOWN",
            "  Unknown  ",
            "!!!@@@###",
        ] {
            assert_eq!(
                sanitize_face_profile_name(forbidden),
                None,
                "input '{forbidden}' must be rejected"
            );
        }

        assert_eq!(
            sanitize_face_profile_name("  João_Silva-01!@# "),
            Some("João_Silva-01".to_string())
        );
        assert_eq!(
            sanitize_face_profile_name("Maria Clara"),
            Some("Maria Clara".to_string())
        );
    }

    #[test]
    fn license_plate_normalization_lookup_ctc_softmax_and_voter_window_eviction() {
        // 1. normalize_license_plate & lookup_known_plate
        assert_eq!(normalize_license_plate(" bra-2e19! "), "BRA2E19");
        assert_eq!(normalize_license_plate("---"), "");

        let mut known = BTreeMap::new();
        known.insert("bra-2e19".to_string(), "Courier Van".to_string());
        known.insert("XYZ9876".to_string(), "Visitor Sedan".to_string());
        assert_eq!(
            lookup_known_plate("BRA2E19", &known),
            Some("Courier Van".to_string())
        );
        assert_eq!(
            lookup_known_plate("xyz-9876", &known),
            Some("Visitor Sedan".to_string())
        );
        assert_eq!(lookup_known_plate("???", &known), None);
        assert_eq!(lookup_known_plate("AAA0000", &known), None);

        // 2. decode_lpr_ctc_logits guard clauses, all-blank sequence, and raw logit softmax branch
        assert!(decode_lpr_ctc_logits(&[], 0, 37, 0).is_none());
        assert!(decode_lpr_ctc_logits(&[1.0, 2.0], 2, 1, 0).is_none());
        assert!(decode_lpr_ctc_logits(&[1.0, 2.0], 2, 4, 0).is_none());

        // All-blank timesteps (blank_idx = 0) -> None
        let all_blank = vec![5.0, -2.0, -1.0, 6.0, -3.0, -2.0];
        assert!(decode_lpr_ctc_logits(&all_blank, 2, 3, 0).is_none());

        // Raw unnormalized logits outside [0.0, 1.0]:
        // blank_idx = 0, class 1 = '0', class 11 = 'A'
        let num_classes = 37;
        let mut raw_logits = vec![-5.0f32; 3 * num_classes];
        raw_logits[11] = 8.0; // t=0 -> 'A'
        raw_logits[num_classes + 11] = 8.0; // t=1 -> repeated 'A' (collapsed)
        raw_logits[2 * num_classes + 1] = 8.0; // t=2 -> '0'
        let decoded = decode_lpr_ctc_logits(&raw_logits, 3, num_classes, 0).unwrap();
        assert_eq!(decoded.text, "A0");
        assert_eq!(decoded.char_confidences.len(), 2);
        assert!(decoded.confidence > 0.99 && decoded.confidence <= 1.0);

        // 3. LprTrackVoter rolling window eviction (max_window = 3) and remove_track
        let lpr_cfg = LprConfig {
            min_confidence: 0.70,
            ..Default::default()
        };
        let mut voter = LprTrackVoter::new(3);
        let _ = voter.record_read("trk-1", OcrPlateRead::new("AAA1111", 0.95), &lpr_cfg);
        let _ = voter.record_read("trk-1", OcrPlateRead::new("AAA1111", 0.95), &lpr_cfg);
        // Push 3 newer reads of "BBB2222" so both "AAA1111" reads are evicted from the 3-slot window
        let _ = voter.record_read("trk-1", OcrPlateRead::new("BBB2222", 0.90), &lpr_cfg);
        let _ = voter.record_read("trk-1", OcrPlateRead::new("BBB2222", 0.90), &lpr_cfg);
        let consensus = voter
            .record_read("trk-1", OcrPlateRead::new("BBB2222", 0.90), &lpr_cfg)
            .unwrap();
        assert_eq!(consensus.plate, "BBB2222");
        assert_eq!(consensus.votes, 3);
        assert_eq!(
            voter.consensus_for_track("trk-1", &lpr_cfg).unwrap().plate,
            "BBB2222"
        );
        assert_eq!(voter.remove_track("trk-1").unwrap().len(), 3);
        assert!(voter.consensus_for_track("trk-1", &lpr_cfg).is_none());
    }

    #[test]
    fn face_embedding_matches_encoded_image_against_raw_rgb_crop_and_shares_detector_pool() {
        // 1. Build two visually distinct 112x112 RGB face crops
        let mut face_alice_rgb = vec![0u8; 112 * 112 * 3];
        let mut face_bob_rgb = vec![0u8; 112 * 112 * 3];
        for y in 0..112usize {
            for x in 0..112usize {
                let idx = (y * 112 + x) * 3;
                // Alice: warm skin-tone center oval with dark top hair band
                let in_hair = y < 28;
                let in_eyes =
                    (40..52).contains(&y) && ((28..46).contains(&x) || (66..84).contains(&x));
                if in_hair || in_eyes {
                    face_alice_rgb[idx] = 30;
                    face_alice_rgb[idx + 1] = 25;
                    face_alice_rgb[idx + 2] = 20;
                } else {
                    face_alice_rgb[idx] = 220;
                    face_alice_rgb[idx + 1] = 175;
                    face_alice_rgb[idx + 2] = 145;
                }

                // Bob: cool background with bright bottom beard/collar gradient
                let dx = (x as f32) - 56.0;
                let dy = (y as f32) - 56.0;
                if dx * dx + dy * dy < 900.0 {
                    face_bob_rgb[idx] = 45;
                    face_bob_rgb[idx + 1] = 90;
                    face_bob_rgb[idx + 2] = 210;
                } else {
                    face_bob_rgb[idx] = (y as u8).saturating_mul(2);
                    face_bob_rgb[idx + 1] = 230u8.saturating_sub(x as u8);
                    face_bob_rgb[idx + 2] = 60;
                }
            }
        }

        // Encode Alice's crop as WebP (simulating /api/faces enrollment file on disk)
        let alice_img =
            ninjanvr_vision::RgbImage::from_raw(112, 112, face_alice_rgb.clone()).unwrap();
        let alice_webp = ninjanvr_vision::encode_image_webp(&alice_img).unwrap();

        // Add +/- 4 sensor noise to Alice's raw 112x112x3 RGB buffer (simulating live pipeline.rs crop)
        let noisy_alice_raw: Vec<u8> = face_alice_rgb
            .iter()
            .enumerate()
            .map(|(i, &px)| {
                if i % 2 == 0 {
                    px.saturating_add(4)
                } else {
                    px.saturating_sub(4)
                }
            })
            .collect();

        let emb_enrolled_webp = extract_face_embedding(&alice_webp);
        let emb_live_raw = extract_face_embedding(&noisy_alice_raw);
        let emb_bob_raw = extract_face_embedding(&face_bob_rgb);

        let sim_same = cosine_similarity(&emb_enrolled_webp, &emb_live_raw);
        let sim_diff = cosine_similarity(&emb_enrolled_webp, &emb_bob_raw);
        assert!(
            sim_same >= 0.90,
            "enrolled WebP face must match noisy live 112x112x3 RGB crop (got {sim_same})"
        );
        assert!(
            sim_diff < 0.40,
            "distinct face pattern must have low cosine similarity (got {sim_diff})"
        );

        // 2. Verify DetectorPool::new reuses the Arc instances created by AiDetectorEngine::new
        let yaml = r#"
detectors:
  cpu_a:
    type: openvino
    device: CPU
  cpu_b:
    type: onnxruntime
    device: CPU
"#;
        let cfg = NinjaConfig::from_yaml_str(yaml, PathBuf::from("/tmp/pool_dedup.yml")).unwrap();
        let pool = DetectorPool::new(&cfg);
        let from_pool = pool.get(Some("cpu_a"));
        let from_default = pool.default_detector.named_detector(Some("cpu_a")).unwrap();
        assert!(
            Arc::ptr_eq(&from_pool, &from_default),
            "DetectorPool must share Arc<AiDetectorEngine> instances rather than loading models twice"
        );
    }

    #[test]
    fn face_gallery_enrolls_and_removes_profiles_case_insensitively() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-face-case-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        let mut gallery = FaceGallery::new();
        let (p1, _) = gallery
            .enroll_to_dir(&tmp, "Erik", "sample1", "jpg", b"erik-face-1")
            .unwrap();
        assert!(p1.exists());

        // Enrolling "eRIK" resolves to canonical "Erik" instead of creating a duplicate profile
        let (p2, _) = gallery
            .enroll_to_dir(&tmp, "eRIK", "sample2", "jpg", b"erik-face-2")
            .unwrap();
        assert!(p2.exists());
        let profiles = gallery.list_profiles(Some(&tmp));
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "Erik");
        assert_eq!(profiles[0].sample_count, 2);

        // Removing "ERIK" removes canonical "Erik" in memory and on disk
        let removed = gallery.remove_profile(Some(&tmp), "ERIK").unwrap();
        assert!(removed);
        assert!(gallery.list_profiles(Some(&tmp)).is_empty());
        assert!(!tmp.join("Erik").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn lpr_regex_quantifiers_alternation_negated_classes_and_char_weighted_voting() {
        // 1. Quantifiers (?, +, *, {n}, {min,max}, {min,}), non-capturing groups (?:...),
        // negated classes [^...], and \d / \w inside character classes
        let pattern = r"^(?:BR)?[A-Z]{3}[\d][A-Z\d]\d{2,3}[^0-9]?$";
        assert!(validate_license_plate("BRA2E19", Some(pattern)));
        assert!(validate_license_plate("BRABC1234X", Some(pattern)));
        assert!(!validate_license_plate("1234567", Some(pattern)));
        assert!(!validate_license_plate("BRA2E19999", Some(pattern)));

        // Alternation and +, * quantifiers
        let alt_pat = r"^([A-Z]+\d*|\d+[A-Z]{2})$";
        assert!(validate_license_plate("MOTO", Some(alt_pat)));
        assert!(validate_license_plate("1234AB", Some(alt_pat)));
        assert!(!validate_license_plate("12A34", Some(alt_pat)));

        // Empty or wildcard regex accepts any non-empty alphanumeric plate and rejects empty string
        assert!(validate_license_plate("ABC1D23", None));
        assert!(validate_license_plate("ABC1D23", Some("   ")));
        assert!(!validate_license_plate("---", None));

        // 2. Character-by-character confidence-weighted voting (with_char_confidences)
        // Frame 1 has high confidence on prefix "BRA2" and low confidence on suffix "000"
        // Frame 2 has low confidence on prefix "XXX2" and high confidence on suffix "E19"
        let r1 = OcrPlateRead::with_char_confidences(
            "BRA2000",
            0.78,
            vec![0.98, 0.97, 0.96, 0.95, 0.15, 0.15, 0.15],
        );
        let r2 = OcrPlateRead::with_char_confidences(
            "XXX2E19",
            0.78,
            vec![0.12, 0.12, 0.12, 0.94, 0.96, 0.97, 0.98],
        );
        let mut lpr_cfg = LprConfig {
            min_confidence: 0.75,
            format_regex: Some(r"^[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$".to_string()),
            ..Default::default()
        };
        lpr_cfg
            .known_plates
            .insert("BRA2E19".to_string(), "Fleet Truck #04".to_string());

        let voted = vote_license_plate_reads(&[r1, r2], &lpr_cfg).expect("char-weighted consensus");
        assert_eq!(voted.plate, "BRA2E19");
        assert_eq!(voted.sub_label.as_deref(), Some("Fleet Truck #04"));
        assert_eq!(voted.votes, 2);
        assert!(voted.confidence >= 0.90);

        // 3. Edge cases: [A-\d] (literal '-' before \d does not form a bogus 'A'..='\\' range)
        // and uniform solid-color image embedding norm == 1.0
        assert!(validate_license_plate("A901", Some(r"^[A-\d]{4}$")));
        assert!(!validate_license_plate("B901", Some(r"^[A-\d]{4}$")));
        let solid_black_rgb = vec![0u8; 112 * 112 * 3];
        let emb_black = extract_face_embedding(&solid_black_rgb);
        assert!((cosine_similarity(&emb_black, &emb_black) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn regression_openvino_ffi_drop_order_and_early_error_cleanup() {
        static FREE_LOG: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        static FAIL_OUT_TENSOR: AtomicU64 = AtomicU64::new(0);
        static MOCK_IN_BUF: Mutex<[u8; 64 * 64 * 3]> = Mutex::new([0u8; 64 * 64 * 3]);
        static MOCK_OUT_BUF: [f32; 7] = [0.0, 1.0, 0.92, 0.1, 0.1, 0.5, 0.8];

        unsafe extern "C" fn mock_core_create(out: *mut *mut c_void) -> c_int {
            unsafe {
                *out = 0x11usize as *mut c_void;
            }
            0
        }
        unsafe extern "C" fn mock_compile_model(
            _core: *mut c_void,
            _model: *const c_char,
            _dev: *const c_char,
            _nargs: usize,
            out: *mut *mut c_void,
            _k: *const c_char,
            _v: *const c_char,
        ) -> c_int {
            unsafe {
                *out = 0x12usize as *mut c_void;
            }
            0
        }
        unsafe extern "C" fn mock_create_infer_req(
            _cm: *mut c_void,
            out: *mut *mut c_void,
        ) -> c_int {
            unsafe {
                *out = 0x13usize as *mut c_void;
            }
            0
        }
        unsafe extern "C" fn mock_get_in_tensor(_req: *mut c_void, out: *mut *mut c_void) -> c_int {
            unsafe {
                *out = 0x14usize as *mut c_void;
            }
            0
        }
        unsafe extern "C" fn mock_get_out_tensor(
            _req: *mut c_void,
            out: *mut *mut c_void,
        ) -> c_int {
            if FAIL_OUT_TENSOR.load(Ordering::SeqCst) != 0 {
                return -1;
            }
            unsafe {
                *out = 0x15usize as *mut c_void;
            }
            0
        }
        unsafe extern "C" fn mock_infer(_req: *mut c_void) -> c_int {
            0
        }
        unsafe extern "C" fn mock_tensor_data(t: *mut c_void, out: *mut *mut c_void) -> c_int {
            unsafe {
                if t as usize == 0x14 {
                    *out = MOCK_IN_BUF.lock().unwrap().as_mut_ptr() as *mut c_void;
                } else {
                    *out = MOCK_OUT_BUF.as_ptr() as *mut c_void;
                }
            }
            0
        }
        unsafe extern "C" fn mock_tensor_byte_size(t: *mut c_void, out: *mut usize) -> c_int {
            unsafe {
                if t as usize == 0x14 {
                    *out = 64 * 64 * 3;
                } else {
                    *out = std::mem::size_of_val(&MOCK_OUT_BUF);
                }
            }
            0
        }
        unsafe extern "C" fn mock_free(ptr: *mut c_void) {
            FREE_LOG.lock().unwrap().push(ptr as usize);
        }

        let fns = OvCFunctionTable {
            core_create: mock_core_create,
            compile_model: unsafe {
                std::mem::transmute::<*const c_void, OvCoreCompileModelFn>(
                    mock_compile_model as *const c_void,
                )
            },
            create_infer_req: mock_create_infer_req,
            get_in_tensor: mock_get_in_tensor,
            get_out_tensor: mock_get_out_tensor,
            infer_fn: mock_infer,
            tensor_data: mock_tensor_data,
            tensor_byte_size: mock_tensor_byte_size,
            tensor_free: Some(mock_free),
            infer_req_free: Some(mock_free),
            compiled_model_free: Some(mock_free),
            core_free: Some(mock_free),
        };

        // 1. Early error when get_out_tensor fails must free input_tensor (0x14) -> infer_req (0x13) -> compiled_model (0x12) -> core (0x11)
        FREE_LOG.lock().unwrap().clear();
        FAIL_OUT_TENSOR.store(1, Ordering::SeqCst);
        let err_res = unsafe {
            AiDetectorEngine::init_ov_context_from_fns(
                std::ptr::null_mut(),
                fns,
                Path::new("/tmp/m.xml"),
                "CPU",
            )
        };
        assert!(err_res.is_err());
        assert_eq!(*FREE_LOG.lock().unwrap(), vec![0x14, 0x13, 0x12, 0x11]);

        // 2. Successful init + drop frees output_tensor (0x15) -> input_tensor (0x14) -> infer_req (0x13) -> compiled_model (0x12) -> core (0x11)
        FREE_LOG.lock().unwrap().clear();
        FAIL_OUT_TENSOR.store(0, Ordering::SeqCst);
        let (ov_ctx, dev) = unsafe {
            AiDetectorEngine::init_ov_context_from_fns(
                std::ptr::null_mut(),
                fns,
                Path::new("/tmp/m.xml"),
                "CPU",
            )
            .unwrap()
        };
        assert_eq!(dev, "CPU");
        drop(ov_ctx);
        assert_eq!(
            *FREE_LOG.lock().unwrap(),
            vec![0x15, 0x14, 0x13, 0x12, 0x11]
        );
    }

    #[test]
    fn regression_ort_ffi_early_error_cleanup_allocator_free_and_negative_i64_dims() {
        static FREE_EVENTS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        static ALLOC_FREED: AtomicU64 = AtomicU64::new(0);
        static FAIL_CPU_MEM: AtomicU64 = AtomicU64::new(0);
        static NEGATIVE_DIM: AtomicU64 = AtomicU64::new(0);
        static MOCK_OUT_TENSOR: [f32; 7] = [0.0, 1.0, 0.91, 0.1, 0.1, 0.5, 0.8];

        unsafe extern "C" fn mock_rel_env(_p: *mut c_void) {
            FREE_EVENTS.lock().unwrap().push("env");
        }
        unsafe extern "C" fn mock_rel_opts(_p: *mut c_void) {
            FREE_EVENTS.lock().unwrap().push("opts");
        }
        unsafe extern "C" fn mock_rel_sess(_p: *mut c_void) {
            FREE_EVENTS.lock().unwrap().push("session");
        }
        unsafe extern "C" fn mock_rel_mem(_p: *mut c_void) {
            FREE_EVENTS.lock().unwrap().push("mem_info");
        }
        unsafe extern "C" fn mock_rel_noop(_p: *mut c_void) {}
        unsafe extern "C" fn mock_alloc_free(_alloc: *mut c_void, ptr: *mut c_void) -> *mut c_void {
            if !ptr.is_null() {
                ALLOC_FREED.fetch_add(1, Ordering::SeqCst);
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_env(
            _l: c_int,
            _id: *const c_char,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x101usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_opts(out: *mut *mut c_void) -> *mut c_void {
            unsafe {
                *out = 0x102usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_set_i32(_o: *mut c_void, _v: c_int) -> *mut c_void {
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_session(
            _e: *const c_void,
            _m: *const c_char,
            _o: *const c_void,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x103usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_alloc(out: *mut *mut c_void) -> *mut c_void {
            unsafe {
                *out = 0x104usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_name(
            _s: *const c_void,
            _i: usize,
            _a: *mut c_void,
            out: *mut *mut c_char,
        ) -> *mut c_void {
            unsafe {
                *out = c"tensor0".as_ptr() as *mut c_char;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_in_type(
            _s: *const c_void,
            _i: usize,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = std::ptr::null_mut();
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_cast_ti(
            _ti: *const c_void,
            out: *mut *const c_void,
        ) -> *mut c_void {
            unsafe {
                *out = std::ptr::null();
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_elem_ty(
            _info: *const c_void,
            out: *mut c_int,
        ) -> *mut c_void {
            unsafe {
                *out = ORT_TENSOR_ELEMENT_DATA_TYPE_FLOAT;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_cpu_mem(
            _a: c_int,
            _m: c_int,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            if FAIL_CPU_MEM.load(Ordering::SeqCst) != 0 {
                return 0x999usize as *mut c_void;
            }
            unsafe {
                *out = 0x105usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_create_tensor(
            _mem: *const c_void,
            _data: *mut c_void,
            _len: usize,
            _shape: *const i64,
            _slen: usize,
            _ty: c_int,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x201usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_run(
            _sess: *mut c_void,
            _ro: *const c_void,
            _in_n: *const *const c_char,
            _in_v: *const *const c_void,
            _in_l: usize,
            _out_n: *const *const c_char,
            _out_l: usize,
            out_v: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out_v = 0x202usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_mut_data(
            _val: *mut c_void,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = MOCK_OUT_TENSOR.as_ptr() as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_type_shape(
            _val: *const c_void,
            out: *mut *mut c_void,
        ) -> *mut c_void {
            unsafe {
                *out = 0x203usize as *mut c_void;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_dims_count(
            _info: *const c_void,
            out: *mut usize,
        ) -> *mut c_void {
            unsafe {
                *out = 2;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_dims(
            _info: *const c_void,
            out: *mut i64,
            _len: usize,
        ) -> *mut c_void {
            unsafe {
                *out.add(0) = if NEGATIVE_DIM.load(Ordering::SeqCst) != 0 {
                    -1
                } else {
                    1
                };
                *out.add(1) = 7;
            }
            std::ptr::null_mut()
        }
        unsafe extern "C" fn mock_get_elem_count(
            _info: *const c_void,
            out: *mut usize,
        ) -> *mut c_void {
            unsafe {
                *out = if NEGATIVE_DIM.load(Ordering::SeqCst) != 0 {
                    (-7isize) as usize
                } else {
                    7
                };
            }
            std::ptr::null_mut()
        }

        // Null vtable is rejected without UB
        assert!(unsafe {
            AiDetectorEngine::init_ort_session_from_vtable(
                std::ptr::null_mut(),
                std::ptr::null(),
                Path::new("/tmp/m.onnx"),
                "CPU",
                64,
                64,
            )
            .is_err()
        });

        let mut vtable: [*const c_void; 101] = [std::ptr::null(); 101];
        vtable[3] = mock_create_env as *const c_void;
        vtable[7] = mock_create_session as *const c_void;
        vtable[9] = mock_run as *const c_void;
        vtable[10] = mock_create_opts as *const c_void;
        vtable[23] = mock_set_i32 as *const c_void;
        vtable[24] = mock_set_i32 as *const c_void;
        vtable[25] = mock_set_i32 as *const c_void;
        vtable[33] = mock_get_in_type as *const c_void;
        vtable[36] = mock_get_name as *const c_void;
        vtable[37] = mock_get_name as *const c_void;
        vtable[49] = mock_create_tensor as *const c_void;
        vtable[51] = mock_get_mut_data as *const c_void;
        vtable[55] = mock_cast_ti as *const c_void;
        vtable[60] = mock_get_elem_ty as *const c_void;
        vtable[61] = mock_get_dims_count as *const c_void;
        vtable[62] = mock_get_dims as *const c_void;
        vtable[64] = mock_get_elem_count as *const c_void;
        vtable[65] = mock_get_type_shape as *const c_void;
        vtable[69] = mock_create_cpu_mem as *const c_void;
        vtable[76] = mock_alloc_free as *const c_void;
        vtable[78] = mock_get_alloc as *const c_void;
        vtable[92] = mock_rel_env as *const c_void;
        vtable[93] = mock_rel_noop as *const c_void;
        vtable[94] = mock_rel_mem as *const c_void;
        vtable[95] = mock_rel_sess as *const c_void;
        vtable[96] = mock_rel_noop as *const c_void;
        vtable[98] = mock_rel_noop as *const c_void;
        vtable[99] = mock_rel_noop as *const c_void;
        vtable[100] = mock_rel_opts as *const c_void;

        // Early error in CreateCpuMemoryInfo frees session -> opts -> env and frees both allocator names
        FREE_EVENTS.lock().unwrap().clear();
        ALLOC_FREED.store(0, Ordering::SeqCst);
        FAIL_CPU_MEM.store(1, Ordering::SeqCst);
        let early_err = unsafe {
            AiDetectorEngine::init_ort_session_from_vtable(
                std::ptr::null_mut(),
                vtable.as_ptr(),
                Path::new("/tmp/m.onnx"),
                "CPU",
                64,
                64,
            )
        };
        assert!(early_err.is_err());
        assert_eq!(ALLOC_FREED.load(Ordering::SeqCst), 2);
        assert_eq!(*FREE_EVENTS.lock().unwrap(), vec!["session", "opts", "env"]);

        // Negative i64 dimension in detect_region is safely rejected without OOB slice creation
        FAIL_CPU_MEM.store(0, Ordering::SeqCst);
        NEGATIVE_DIM.store(1, Ordering::SeqCst);
        let (ort_ctx, _) = unsafe {
            AiDetectorEngine::init_ort_session_from_vtable(
                std::ptr::null_mut(),
                vtable.as_ptr(),
                Path::new("/tmp/m.onnx"),
                "CPU",
                64,
                64,
            )
            .unwrap()
        };
        let cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c.yml")).unwrap();
        let engine = AiDetectorEngine::new(&cfg);
        {
            let mut g = engine.inner.lock().unwrap();
            g.model_w = 64;
            g.model_h = 64;
            g.backend = Some(InferenceRuntimeBackend::OnnxRuntime(ort_ctx));
        }
        let crop = vec![0u8; 64 * 64 * 3];
        let region = BoundingBox::new(0, 0, 640, 480);
        let dets_neg = engine.detect_region(&crop, region, 1280, 720, 0.5);
        assert!(dets_neg.is_empty());

        NEGATIVE_DIM.store(0, Ordering::SeqCst);
        let dets_ok = engine.detect_region(&crop, region, 1280, 720, 0.5);
        assert_eq!(dets_ok.len(), 1);
    }

    #[test]
    fn regression_zero_frame_dims_nan_inf_scores_and_face_crop_clamping() {
        let mut cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c.yml")).unwrap();
        cfg.model.model_type = "yolo-generic".to_string();
        cfg.model.width = 320;
        cfg.model.height = 320;
        let engine = AiDetectorEngine::new(&cfg);
        let region = BoundingBox::new(0, 0, 320, 320);

        // 1. frame_w == 0 or frame_h == 0 must not underflow/panic at (frame_w - 1)
        let ssd_valid = [0.0f32, 1.0, 0.95, 0.1, 0.1, 0.8, 0.8];
        assert!(
            engine
                .parse_raw_output_slice(&ssd_valid, region, 0, 720, 0.5)
                .is_empty()
        );
        assert!(
            engine
                .parse_raw_output_slice(&ssd_valid, region, 1280, 0, 0.5)
                .is_empty()
        );

        // 2. NaN and +Inf scores, class IDs, and bbox widths must be rejected across SSD & YOLO
        let ssd_nan = [
            0.0f32,
            1.0,
            f32::NAN,
            0.1,
            0.1,
            0.8,
            0.8,
            0.0,
            f32::NAN,
            0.95,
            0.1,
            0.1,
            0.8,
            0.8,
            0.0,
            1.0,
            f32::INFINITY,
            0.1,
            0.1,
            0.8,
            0.8,
        ];
        assert!(
            engine
                .parse_raw_output_slice(&ssd_nan, region, 1280, 720, 0.5)
                .is_empty()
        );

        let yolo6_bad = [
            10.0f32,
            10.0,
            100.0,
            100.0,
            f32::NAN,
            0.0,
            10.0,
            10.0,
            f32::INFINITY,
            100.0,
            0.95,
            0.0,
            10.0,
            10.0,
            100.0,
            100.0,
            0.95,
            f32::NAN,
        ];
        assert!(
            engine
                .parse_raw_output_slice(&yolo6_bad, region, 1280, 720, 0.5)
                .is_empty()
        );

        // Transposed YOLO with bw = INFINITY must not clamp to full-frame [0, 0, 1, 1] box
        let mut yolo84_inf = vec![0.0f32; 84 * 10];
        yolo84_inf[0] = 160.0;
        yolo84_inf[10] = 160.0;
        yolo84_inf[20] = f32::INFINITY;
        yolo84_inf[30] = f32::INFINITY;
        yolo84_inf[40] = 0.95;
        assert!(
            engine
                .parse_raw_output_slice(&yolo84_inf, region, 1280, 720, 0.5)
                .is_empty()
        );

        // nms_detections drops non-finite scores without panicking
        let nms_res = nms_detections(
            vec![
                DetectionResult {
                    label: "person".to_string(),
                    score: f32::NAN,
                    bbox: BoundingBox::new(10, 10, 100, 100),
                    region,
                },
                DetectionResult {
                    label: "person".to_string(),
                    score: 0.85,
                    bbox: BoundingBox::new(10, 10, 100, 100),
                    region,
                },
            ],
            0.5,
        );
        assert_eq!(nms_res.len(), 1);
        assert!((nms_res[0].score - 0.85).abs() < 1e-5);

        // 3. resolve_face_crop_bbox with 0 frame dims or out-of-frame / u32::MAX coordinates
        let person = BoundingBox::new(100, 100, 300, 400);
        assert!(resolve_face_crop_bbox(person, None, 0, 720, 100).is_none());
        let oob_face = BoundingBox::new(u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        if let Some(clamped) = resolve_face_crop_bbox(person, Some(oob_face), 1280, 720, 1) {
            assert!(clamped.x_max <= 1280 && clamped.y_max <= 720);
        }
    }

    #[test]
    fn regression_face_gallery_ext_traversal_symlink_dotdir_and_bounded_memory() {
        let tmp = std::env::temp_dir().join(format!(
            "ninjanvr-face-sec-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let faces_dir = tmp.join("faces");
        let outside_dir = tmp.join("outside");
        std::fs::create_dir_all(&faces_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        // 1. Dot-directories (.git, .hidden) must be rejected by sanitize_face_profile_name and never deleted
        assert_eq!(sanitize_face_profile_name(".git"), None);
        assert_eq!(sanitize_face_profile_name(".hidden"), None);
        let dot_git = faces_dir.join(".git");
        std::fs::create_dir_all(&dot_git).unwrap();
        std::fs::write(dot_git.join("HEAD"), b"ref: refs/heads/dev").unwrap();

        // 2. Malicious extension traversal in enroll_to_dir must be stripped to a safe extension inside person_dir
        let mut gallery = FaceGallery::new();
        let (saved_path, _) = gallery
            .enroll_to_dir(
                &faces_dir,
                "Alice",
                "crop1",
                "/../../../outside/pwned.jpg",
                b"img1",
            )
            .unwrap();
        assert!(saved_path.starts_with(faces_dir.join("Alice")));
        assert!(!outside_dir.join("pwned.jpg").exists());

        // Overwriting the same stem updates the existing sample slot rather than growing unboundedly
        let _ = gallery
            .enroll_to_dir(&faces_dir, "Alice", "crop1", "jpg", b"img2")
            .unwrap();
        let profiles = gallery.list_profiles(Some(&faces_dir));
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].sample_count, 1);

        // Removing "git" must NOT delete `.git`
        let _ = gallery.remove_profile(Some(&faces_dir), "git").unwrap();
        assert!(dot_git.join("HEAD").exists());

        // 3. Symlinked profile directory inside faces_dir must not be followed on sync or deleted on remove_profile
        #[cfg(unix)]
        {
            let target_file = outside_dir.join("keep.txt");
            std::fs::write(&target_file, b"keep").unwrap();
            let symlink_dir = faces_dir.join("Bob");
            std::os::unix::fs::symlink(&outside_dir, &symlink_dir).unwrap();

            gallery.sync_from_dir(&faces_dir).unwrap();
            assert!(
                gallery
                    .list_profiles(Some(&faces_dir))
                    .iter()
                    .all(|p| p.name != "Bob")
            );
            assert!(
                gallery
                    .enroll_to_dir(&faces_dir, "Bob", "s1", "jpg", b"x")
                    .is_err()
            );
            let _ = gallery.remove_profile(Some(&faces_dir), "Bob").unwrap();
            assert!(
                target_file.exists(),
                "remove_profile must never follow symlink to delete outside_dir contents"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn regression_lpr_redos_nested_quantifiers_nan_voting_and_poisoned_mutex() {
        // 1. Pathological nested/chained quantifier ReDoS patterns complete in milliseconds
        let t0 = Instant::now();
        for pat in [
            r"^((((((([A-Z0-9]*)*)*)*)*)*)*)*Z$",
            r"^([A-Z0-9]+)+([A-Z0-9]+)+([A-Z0-9]+)+Z$",
            r"^(.?){0,32}(.?){0,32}(.?){0,32}(.?){0,32}(.?){0,32}Z$",
            r"^A{1,9999999999}Z$",
            r"^((((((((((((((((A+)+)+)+)+)+)+)+)+)+)+)+)+)+)+)+)Z$",
            r"^(((((((((((((([A-Z0-9]+|[A-Z0-9]+)+)+)+)+)+)+)+)+)+)+)+)+)+)Z$",
        ] {
            assert!(!validate_license_plate("ABC1D23456", Some(pat)));
            assert!(!validate_license_plate("AAAAAAAAAA", Some(pat)));
        }
        assert!(
            t0.elapsed().as_millis() < 200,
            "LPR regex evaluation must be immune to exponential backtracking"
        );

        // Deeply nested parentheses beyond MAX_REGEX_DEPTH do not overflow stack
        let deep_parens = format!("^{}ABC1D23{}$", "(".repeat(64), ")".repeat(64));
        let _ = validate_license_plate("ABC1D23", Some(&deep_parens));

        // 2. L2 normalization and cosine similarity handle NaN, Infinity, and f32::MAX without producing NaN
        let mut extreme = vec![f32::MAX, f32::MAX, f32::NAN, f32::INFINITY];
        l2_normalize_embedding(&mut extreme);
        assert!(extreme.iter().all(|v| v.is_finite()));

        let max_vec = vec![f32::MAX; 16];
        let sim_max = cosine_similarity(&max_vec, &max_vec);
        assert!((sim_max - 1.0).abs() < 1e-5);
        assert_eq!(cosine_similarity(&[f32::NAN, 1.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine_similarity(&[f32::INFINITY, 1.0], &[1.0, 1.0]), 0.0);

        // 3. decode_lpr_ctc_logits guards against timesteps * num_classes overflow and NaN/-Inf rows
        assert!(decode_lpr_ctc_logits(&[1.0; 16], usize::MAX / 2 + 1, 4, 0).is_none());
        let nan_logits = vec![f32::NAN; 2 * 37];
        assert!(decode_lpr_ctc_logits(&nan_logits, 2, 37, 36).is_none());

        // 4. vote_license_plate_reads rejects NaN confidences and clamps +Inf confidences
        let lpr_cfg = LprConfig {
            min_confidence: 0.75,
            ..Default::default()
        };
        let reads = [
            OcrPlateRead {
                text: "BAD9999".to_string(),
                confidence: f32::NAN,
                char_confidences: vec![f32::NAN; 7],
            },
            OcrPlateRead::new("ABC1D23", 0.90),
        ];
        let voted = vote_license_plate_reads(&reads, &lpr_cfg).unwrap();
        assert_eq!(voted.plate, "ABC1D23");
        assert!(voted.confidence.is_finite() && voted.confidence <= 1.0);

        // 5. LprTrackVoter bounds active tracks at MAX_ACTIVE_LPR_TRACKS
        let mut voter = LprTrackVoter::new(4);
        for i in 0..(MAX_ACTIVE_LPR_TRACKS + 20) {
            let _ = voter.record_read(
                &format!("trk_{i}"),
                OcrPlateRead::new("ABC1D23", 0.90),
                &lpr_cfg,
            );
        }
        assert!(voter.tracks.len() <= MAX_ACTIVE_LPR_TRACKS);
        assert!(voter.consensus_for_track("trk_0", &lpr_cfg).is_none());

        // 6. Poisoned mutex recovery on AiDetectorEngine does not panic
        let cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/p.yml")).unwrap();
        let engine = Arc::new(AiDetectorEngine::new(&cfg));
        let engine_clone = engine.clone();
        let _ = std::thread::spawn(move || {
            let _g = engine_clone.inner.lock().unwrap();
            panic!("intentional poison test");
        })
        .join();
        assert_eq!(engine.model_dimensions().0, 300);
    }

    #[test]
    fn regression_cycle2_ov_dim_reconcile_split_axis_yolo_row_major_and_nullable_regex_groups() {
        // 1. OpenVINO dimension reconciliation:
        //    (a) 300x300 u8 SSD model (in_bytes = 270,000 = 12 * 150^2) configured with 320x320 or 640x640
        //        must reconcile to 300x300 (u8), NOT 150x150 (f32).
        assert_eq!(
            AiDetectorEngine::reconcile_openvino_dimensions(
                640,
                640,
                300 * 300 * 3,
                700,
                false,
                false,
                "/openvino-model/ssdlite_mobilenet_v2.xml",
            ),
            (300, 300)
        );
        //    (b) 320x320 f32 YOLO model (in_bytes = 1,228,800 = 640 * 640 * 3) configured with 640x640
        //        must reconcile to 320x320 (f32), NOT stay at 640x640 (u8).
        assert_eq!(
            AiDetectorEngine::reconcile_openvino_dimensions(
                640,
                640,
                320 * 320 * 3 * 4,
                84 * 2100,
                true,
                true,
                "/config/models/yolov9t.xml",
            ),
            (320, 320)
        );

        // 2. Unified pixel-vs-normalized coordinate scaling across X and Y axes:
        //    A sub-pixel height artifact (cx = 200.0, cy = 0.5, bw = 60.0, bh = 0.8) in a 640x640
        //    pixel-space YOLO tensor must NOT keep scale_h = 1.0 and inflate bh = 0.8 to 80% of frame height.
        let mut cfg =
            NinjaConfig::from_yaml_str("version: 0.18-0\n", PathBuf::from("/tmp/c2.yml")).unwrap();
        cfg.model.model_type = "yolo-generic".to_string();
        cfg.model.width = 640;
        cfg.model.height = 640;
        cfg.model.input_tensor = "nchw".to_string();
        let engine = AiDetectorEngine::new(&cfg);
        let region = BoundingBox::new(0, 0, 1280, 720);

        let mut transposed_subpixel = vec![0.0f32; 84 * 10];
        transposed_subpixel[0] = 200.0; // cx > 1.5 (pixel coords)
        transposed_subpixel[10] = 0.5; // cy <= 1.5
        transposed_subpixel[20] = 60.0; // bw > 1.5
        transposed_subpixel[30] = 0.8; // bh <= 1.5 (0.8px degenerate height)
        transposed_subpixel[40] = 0.95; // class 0 score
        assert!(
            engine
                .parse_raw_output_slice(&transposed_subpixel, region, 1280, 720, 0.5)
                .is_empty(),
            "0.8px-tall pixel-space box must not be interpreted as 0.8 normalized height"
        );

        let yolo6_subpixel = [100.0f32, 0.1, 200.0, 0.9, 0.95, 0.0];
        assert!(
            engine
                .parse_raw_output_slice(&yolo6_subpixel, region, 1280, 720, 0.5)
                .is_empty(),
            "0.8px-tall 6-column YOLO box must not be interpreted as 0.8 normalized height"
        );

        // Non-integer raw_label in SSD / 6-col YOLO (e.g. 0.45) must be rejected, not truncated to 0 ("person")
        let yolo6_non_int_cls = [50.0f32, 50.0, 200.0, 200.0, 0.95, 0.45];
        assert!(
            engine
                .parse_raw_output_slice(&yolo6_non_int_cls, region, 1280, 720, 0.5)
                .is_empty()
        );

        // 3. Row-Major YOLOv5 [1, 2100, 5 + C] when C=10 (4 + C = 14 divides 2100) must decode as Row-Major
        cfg.model.width = 320;
        cfg.model.height = 320;
        engine.reload(&cfg);
        let mut custom10 = BTreeMap::new();
        for i in 0..10i32 {
            custom10.insert(i, format!("cls_{i}"));
        }
        engine.set_labelmap(custom10);

        let stride = 15usize; // 5 + 10 classes
        let mut rm_2100 = vec![0.0f32; 2100 * stride];
        // Put valid detection at row 0 and background noise at other rows that would alias transposed [14, 2250]
        rm_2100[0] = 160.0;
        rm_2100[1] = 160.0;
        rm_2100[2] = 100.0;
        rm_2100[3] = 120.0;
        rm_2100[4] = 0.92;
        rm_2100[5 + 3] = 0.95; // cls_3
        let rm_dets = engine.parse_raw_output_slice(&rm_2100, region, 1280, 720, 0.5);
        assert_eq!(rm_dets.len(), 1);
        assert_eq!(rm_dets[0].label, "cls_3");

        // 4. Out-of-frame or inverted face_attr_bbox falls back to person_bbox head region instead of 1x1 corner
        let person = BoundingBox::new(100, 100, 300, 400);
        let oob_face = BoundingBox::new(1500, 900, 1600, 1000);
        let resolved =
            resolve_face_crop_bbox(person, Some(oob_face), 1280, 720, 1).expect("fallback to head");
        assert!(resolved.inside(&person));
        assert!(resolved.area() >= 750);

        // 5. Nullable parenthesized regex groups without outer ?/* quantifier match empty branch cleanly
        assert!(validate_license_plate(
            "ABC1D23",
            Some(r"^(?:BR|)[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$")
        ));
        assert!(validate_license_plate(
            "BRABC1D23",
            Some(r"^(?:BR|)[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$")
        ));
        assert!(validate_license_plate(
            "ABC1234",
            Some(r"^([A-Z]?)[A-Z]{3}[0-9]{4}$")
        ));
        assert!(validate_license_plate(
            "XABC1234",
            Some(r"^([A-Z]?)[A-Z]{3}[0-9]{4}$")
        ));
    }
}
