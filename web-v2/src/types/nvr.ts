export interface StationaryConfig {
  interval: number;
  threshold: number;
  classifier: boolean;
}

export interface CameraDetectConfig {
  enabled: boolean;
  detector?: string | null;
  width: number;
  height: number;
  fps: number;
  min_initialized?: number;
  min_initialized_night?: number;
  max_disappeared?: number;
  stationary?: StationaryConfig;
}

export interface CameraMotionConfig {
  enabled: boolean;
  threshold: number;
  lightning_threshold?: number;
  improve_contrast?: boolean;
  contour_area: number;
  delta_alpha?: number;
  frame_alpha?: number;
  frame_height?: number;
  mask: unknown;
}

export interface ObjectFilterConfig {
  min_area: number;
  max_area: number;
  min_ratio: number;
  max_ratio: number;
  threshold: number;
  min_score: number;
  mask?: unknown;
}

export interface ObjectsConfig {
  track: string[];
  attribute_labels?: string[];
  filters?: Record<string, ObjectFilterConfig>;
  mask?: unknown;
}

export interface RetainPolicyConfig {
  days: number;
  mode: "all" | "motion" | "active_objects" | string;
}

export interface RecordEventRetainConfig {
  pre_capture: number;
  post_capture: number;
  retain: RetainPolicyConfig;
}

export interface StorageTiersConfig {
  enabled: boolean;
  hot_dir?: string | null;
  cold_dir?: string | null;
  migrate_after_hours?: number;
}

export interface AlertBackupConfig {
  enabled: boolean;
  target_dir?: string | null;
  severities?: string[];
  include_snapshot?: boolean;
}

export interface ColdCompactionConfig {
  enabled: boolean;
  after_hours?: number;
  crf?: number;
  idle_only?: boolean;
  codec?: "h264" | "hevc" | string;
  preset?: "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" | "medium" | "slow" | string;
  hwaccel?: "none" | "vaapi" | "qsv" | "nvenc" | string;
}

export interface RecordConfig {
  enabled: boolean;
  expire_interval?: number;
  continuous_substream?: boolean;
  mode_tiering?: boolean;
  max_size_gb?: number | null;
  min_free_gb?: number | null;
  storage_tiers?: StorageTiersConfig;
  alert_backup?: AlertBackupConfig;
  cold_compaction?: ColdCompactionConfig;
  continuous?: {
    days: number;
  };
  motion?: {
    days: number;
  };
  alerts?: RecordEventRetainConfig;
  detections?: RecordEventRetainConfig;
  preview?: {
    quality: "very_low" | "low" | "medium" | "high" | "very_high" | string;
  };
}

export interface SnapshotsConfig {
  enabled: boolean;
  timestamp?: boolean;
  bounding_box?: boolean;
  crop?: boolean;
  quality?: number;
  high_res?: boolean;
  from_main_stream?: boolean;
  selection?: "best_overall" | "best_in_zone" | "first_trigger" | "best_attribute";
}

export interface ReviewSeverityConfig {
  enabled: boolean;
  labels?: string[];
  sub_labels?: string[];
  exclude_sub_labels?: string[];
  required_zones: string[];
  cutoff_time: number;
}

export interface FaceRecognitionConfig {
  enabled: boolean;
  model_path?: string;
  similarity_threshold?: number;
  min_face_area?: number;
  unknown_alert?: boolean;
}

export interface EnrolledFaceProfileItem {
  name: string;
  sample_count: number;
  sample_files?: string[];
  updated_at?: number;
}

export interface ReviewConfig {
  alerts: ReviewSeverityConfig;
  detections: ReviewSeverityConfig;
}

export interface AudioConfig {
  enabled: boolean;
  min_volume_dbfs?: number;
  labels?: string[];
}

export interface ZoneConfig {
  coordinates: string;
  objects?: string[];
  inertia?: number;
  loitering_time?: number;
  min_area?: number;
  max_area?: number;
  min_score?: number;
  preset?: string | null;
}

export interface OnvifCameraConfig {
  host?: string | null;
  port?: number;
  user?: string | null;
  password?: string | null;
  has_password?: boolean;
  clear_password?: boolean;
  autotracking?: boolean;
  return_preset?: string | null;
  timeout_sec?: number;
}

export interface OnvifPresetItem {
  token: string;
  name: string;
}

export interface CameraPtzInfoResponse {
  enabled: boolean;
  online?: boolean;
  camera?: string;
  host?: string | null;
  port?: number;
  autotracking?: boolean;
  return_preset?: string | null;
  active_preset?: string | null;
  features?: string[];
  presets?: OnvifPresetItem[];
  preset_names?: string[];
  status?: {
    pan: number;
    tilt: number;
    zoom: number;
    move_status: string;
    error?: string | null;
  };
  error?: string | null;
}

export interface LineConfig {
  points: [number, number, number, number];
  direction?: "any" | "a_to_b" | "b_to_a";
  objects?: string[];
}

export interface StateRoiConfig {
  crop: [number, number, number, number];
  classes: string[];
  interval_sec?: number;
  hysteresis?: number;
  anomaly_state?: string | null;
  anomaly_timeout_sec?: number;
}

export interface CameraFfmpegInput {
  path: string;
  roles: string[];
  input_args?: unknown;
}

export interface CameraFfmpegConfig {
  inputs: CameraFfmpegInput[];
  output_args?: unknown;
}

export interface CameraLiveConfig {
  streams: Record<string, string>;
  height: number;
  quality: number;
  rotate?: number;
}

export interface CameraMapPlacement {
  x: number;
  y: number;
  angle_deg: number;
  fov_deg: number;
  range: number;
}

export interface FloorplanRoom {
  id: string;
  label: string;
  x: number;
  y: number;
  w: number;
  h: number;
  color?: string | null;
}

export interface FloorplanWall {
  points: [number, number, number, number];
}

export interface FloorplanConfig {
  background_data_url?: string | null;
  opacity: number;
  rooms: FloorplanRoom[];
  walls: FloorplanWall[];
}

export interface UiConfig {
  time_format?: string;
  unit_system?: string;
  floorplan?: FloorplanConfig;
}

export interface CameraUiConfig {
  order?: number;
  dashboard?: boolean;
  review?: boolean;
}

export interface CameraGroupConfig {
  cameras: string[];
  icon?: string | null;
  order?: number;
}

export interface CameraConfig {
  name?: string;
  enabled: boolean;
  indoor?: boolean;
  webui_url?: string;
  notes?: string;
  privacy_mask?: unknown;
  ui?: CameraUiConfig;
  map?: CameraMapPlacement;
  onvif?: OnvifCameraConfig;
  ffmpeg?: CameraFfmpegConfig;
  live?: CameraLiveConfig;
  detect: CameraDetectConfig;
  motion: CameraMotionConfig;
  objects?: ObjectsConfig;
  zones: Record<string, ZoneConfig>;
  lines?: Record<string, LineConfig>;
  state_rois?: Record<string, StateRoiConfig>;
  record: RecordConfig;
  snapshots: SnapshotsConfig;
  review?: ReviewConfig;
  audio?: AudioConfig;
}

export interface DetectorDeviceStatus {
  id: string;
  name: string;
  available: boolean;
  detail: string;
}

export interface DetectorRuntimeCatalog {
  loaded: boolean;
  active_backend: "openvino" | "onnxruntime" | string;
  openvino_available?: boolean;
  ort_available?: boolean;
  active_device: string;
  active_model_path: string;
  compiled_width: number;
  compiled_height: number;
  in_bytes: number;
  out_floats: number;
  avg_inference_ms: number;
  devices: DetectorDeviceStatus[];
  discovered_models?: Array<{ path: string; arch: string }>;
}

export interface DatasetAnnotationBox {
  class_id: number;
  label: string;
  cx: number;
  cy: number;
  w: number;
  h: number;
  score?: number;
  source?: "event" | "manual" | "ai_teacher" | string;
}

export interface DatasetSampleItem {
  id: string;
  event_id?: string | null;
  camera: string;
  split: "train" | "val" | string;
  sample_kind: "true_positive" | "hard_negative" | "ai_annotated" | string;
  image_path: string;
  width: number;
  height: number;
  annotations: DatasetAnnotationBox[];
  verified: boolean;
  night_mode: boolean;
  created_at: number;
  image_url?: string;
}

export interface DatasetSummaryResponse {
  total_samples: number;
  true_positive_count: number;
  hard_negative_count: number;
  ai_annotated_count: number;
  positive_samples?: number;
  train_count: number;
  val_count: number;
  verified_count: number;
  unverified_count: number;
  night_mode_count: number;
  total_annotations: number;
  positive_ratio: number;
  hard_negative_ratio: number;
  target_positive_ratio?: number;
  target_hard_negative_ratio?: number;
  by_camera: Record<string, number>;
  by_class: Record<string, number>;
  classes: string[];
  ready_for_export: boolean;
}

export interface ModelUploadPayload {
  filename: string;
  model_base64: string;
  weights_filename?: string;
  weights_base64?: string;
  labelmap_filename?: string;
  labelmap?: string;
  model_type?: "yolo-generic" | "yolox" | "ssd" | string;
  width?: number;
  height?: number;
  input_tensor?: "nchw" | "nhwc" | string;
  input_pixel_format?: "rgb" | "bgr" | string;
  activate?: boolean;
}

export interface DetectorEntryConfig {
  type: "openvino" | "onnxruntime" | string;
  device: "CPU" | "GPU" | "NPU" | "AUTO" | "CUDA" | "TensorRT" | "ROCM" | string;
  model?: ModelConfig;
}

export interface ModelConfig {
  path: string;
  labelmap_path: string;
  width: number;
  height: number;
  input_tensor: string;
  input_pixel_format: string;
  model_type: string;
  labelmap?: Record<string, string>;
}

export interface MqttConfig {
  enabled: boolean;
  host: string;
  port: number;
  topic_prefix: string;
  client_id: string;
  user?: string | null;
  password?: string | null;
  has_password?: boolean;
  clear_password?: boolean;
  stats_interval: number;
}

export interface OidcAuthConfig {
  enabled: boolean;
  provider_name?: string;
  issuer_url?: string;
  client_id?: string;
  client_secret_env?: string;
  scopes?: string[];
  auto_redirect?: boolean;
  username_claim?: string;
  groups_claim?: string;
  default_role?: string;
}

export interface ProxyAuthConfig {
  enabled: boolean;
  trusted_proxies?: string[];
  user_header?: string;
  groups_header?: string;
  logout_url?: string;
}

export interface AuthConfig {
  enabled: boolean;
  local_login_enabled?: boolean;
  reset_admin_password: boolean;
  cookie_name: string;
  cookie_secure: boolean;
  session_length: number;
  refresh_time: number;
  hash_iterations: number;
  trusted_proxies: string[];
  admin_first_time_login?: boolean;
  oidc?: OidcAuthConfig;
  proxy?: ProxyAuthConfig;
}

export interface PeerNodeConfig {
  enabled: boolean;
  url: string;
  token_env?: string;
}

export interface GenAiConfig {
  enabled: boolean;
  provider: "gemini" | "openai" | "ollama" | string;
  base_url?: string | null;
  api_key_env: string;
  model: string;
  verify_ambiguous_alerts: boolean;
  ambiguous_score_min: number;
  ambiguous_score_max: number;
  auto_suppress_false_positives: boolean;
  timeout_ms?: number;
}

export interface FalsePositiveSignatureItem {
  id: number;
  camera: string;
  label: string;
  norm_bbox: [number, number, number, number];
  norm_x1?: number;
  norm_y1?: number;
  norm_x2?: number;
  norm_y2?: number;
  dhash64?: number | null;
  dominant_color?: string | null;
  hit_count: number;
  source_event_id?: string | null;
  reason?: string | null;
  created_at: number;
  last_seen_at: number;
}

export interface AiVerificationResult {
  is_true_positive: boolean;
  confidence: number;
  reason: string;
  provider?: string;
  model?: string;
  verified_at?: number;
}

export interface LprConfig {
  enabled: boolean;
  detector_model_path?: string | null;
  ocr_model_path?: string | null;
  min_confidence?: number;
  format_regex?: string | null;
  known_plates?: Record<string, string>;
}

export interface NinjaConfigResponse {
  version: string;
  safe_mode: boolean;
  mqtt?: MqttConfig;
  auth?: AuthConfig;
  database?: {
    path: string;
  };
  tls?: {
    enabled: boolean;
  };
  detectors?: Record<string, DetectorEntryConfig>;
  model?: ModelConfig;
  genai?: GenAiConfig;
  face_recognition?: FaceRecognitionConfig;
  detect?: CameraDetectConfig;
  motion?: CameraMotionConfig;
  objects?: ObjectsConfig;
  lpr?: LprConfig;
  record?: RecordConfig;
  snapshots?: SnapshotsConfig;
  review?: ReviewConfig;
  audio?: AudioConfig;
  go2rtc?: {
    streams: Record<string, unknown>;
  };
  peers?: Record<string, PeerNodeConfig>;
  camera_groups?: Record<string, CameraGroupConfig>;
  ui?: UiConfig;
  cameras: Record<string, CameraConfig>;
}

export interface UserAccountItem {
  username: string;
  role: "admin" | "operator" | "viewer" | string;
  allowed_cameras?: string;
  password_changed_at?: string | null;
}

export interface FfprobeStreamEntry {
  codec_long_name?: string;
  width?: number;
  height?: number;
  avg_frame_rate?: string;
}

export interface FfprobeItem {
  return_code: number;
  stderr: string;
  stdout?: {
    streams?: FfprobeStreamEntry[];
  };
}

export interface CameraRuntimeStats {
  camera_fps: number;
  process_fps: number;
  skipped_fps: number;
  detection_fps: number;
  detection_enabled: boolean;
  pid: number;
  capture_pid: number;
  ffmpeg_pid: number;
  last_frame_ts?: number;
  night_mode?: boolean;
  states?: Record<string, string>;
  ffmpeg_cpu?: string;
  capture_cpu?: string;
  detect_cpu?: string;
}

export type CameraStats = CameraRuntimeStats;

export interface DetectorStats {
  inference_speed: number;
  detection_start: number;
  pid: number;
  cpu: string;
  mem: string;
}

export interface StorageStatItem {
  total: number;
  used: number;
  free: number;
  mount_type: string;
}

export interface ProcessStatItem {
  pid: number;
  cpu: string;
  mem: string;
}

export interface HeuristicAlertItem {
  id: string;
  severity: "critical" | "warning" | "optimization" | string;
  category: "streams" | "vision" | "storage" | "audio" | "config" | string;
  target: string;
  title: string;
  detail: string;
  recommendation: string;
}

export interface NinjaStatsResponse {
  cameras: Record<string, CameraRuntimeStats>;
  detectors: Record<string, DetectorStats>;
  camera_fps: number;
  process_fps: number;
  skipped_fps: number;
  detection_fps: number;
  cpu_usages: Record<string, { cpu: string; mem: string; cmdline?: string }>;
  service: {
    uptime: number;
    version: string;
    latest_version: string;
    storage_degraded?: boolean;
    storage_error?: string | null;
    estimated_retention_days?: number;
    camera_storage?: Record<
      string,
      {
        usage: number;
        bandwidth: number;
        estimated_retention_days?: number;
      }
    >;
    storage: Record<string, StorageStatItem>;
    heuristics?: HeuristicAlertItem[];
    last_updated: number;
  };
  processes: Record<string, ProcessStatItem>;
}

export interface TriggerProvenance {
  trigger_ts: number;
  bbox: [number, number, number, number];
  norm_bbox: [number, number, number, number];
  area_px: number;
  area_pct: number;
  aspect_ratio: number;
  score: number;
  top_score: number;
  min_score_cfg: number;
  threshold_cfg: number;
  zones: string[];
  frames_tracked: number;
  stationary: boolean;
}

export interface DetectedAttribute {
  label: string;
  score: number;
  bbox?:
    | { x_min: number; y_min: number; x_max: number; y_max: number }
    | [number, number, number, number];
  norm_bbox: [number, number, number, number];
  area_px: number;
}

export interface ReviewItem {
  id: string;
  camera: string;
  start_time: number;
  end_time: number | null;
  severity: "alert" | "detection";
  thumb_path: string;
  has_been_reviewed: boolean;
  data: {
    objects?: string[];
    sub_labels?: string[];
    sub_label_score?: number | null;
    zones?: string[];
    detections?: string[];
    audio?: string[];
    dbfs?: number;
    notes?: string;
    provenance?: TriggerProvenance | null;
    detected_attributes?: DetectedAttribute[];
    recognized_license_plates?: string[];
    metadata?: {
      potential_threat_level?: number;
    };
  };
}

export interface EventItem {
  id: string;
  camera: string;
  label: string;
  sub_label: string | null;
  sub_label_score?: number | null;
  zones: string[];
  start_time: number;
  end_time: number | null;
  top_score: number | null;
  score: number | null;
  has_clip: boolean;
  has_snapshot: boolean;
  retain_indefinitely: boolean;
  false_positive: boolean | null;
  box: [number, number, number, number] | null;
  area?: number | null;
  ratio?: number | null;
  stationary?: boolean;
  data?: {
    score?: number;
    top_score?: number;
    sub_label?: string | null;
    sub_label_score?: number | null;
    description?: string;
    notes?: string;
    attributes?: string[];
    detected_attributes?: DetectedAttribute[];
    face_box?: [number, number, number, number] | null;
    license_plate_box?: [number, number, number, number] | null;
    recognized_license_plate?: string | null;
    recognized_license_plate_confidence?: number | null;
    box?: [number, number, number, number];
    dhash64?: number | null;
    ai_verification?: AiVerificationResult | null;
    provenance?: TriggerProvenance | null;
  };
}

export interface RecordingSegment {
  id: string;
  start_time: number;
  end_time: number;
  duration: number;
  motion: number;
  objects: number;
  dbfs?: number;
  segment_size: number;
}

export interface RecordingHourSummary {
  hour: string;
  events: number;
  motion: number;
  objects: number;
  dbfs?: number;
  duration: number;
  start_ts?: number;
  end_ts?: number;
}

export interface RecordingDaySummary {
  day: string;
  events: number;
  motion?: number;
  duration?: number;
  start_ts?: number;
  end_ts?: number;
  hours: RecordingHourSummary[];
}

export interface ExportItem {
  id: string;
  camera: string;
  name: string;
  date: number;
  video_path: string;
  thumb_path: string;
  in_progress: boolean;
}

export interface CameraLiveActivity {
  enabled?: boolean;
  night_mode?: boolean;
  tamper?: boolean;
  motion: boolean;
  max_Area?: number;
  states?: Record<string, string>;
  objects: Array<{
    id: string;
    label: string;
    score: number;
    stationary: boolean;
    area?: number;
    ratio?: number;
  }>;
  config?: {
    record?: boolean;
    detect?: boolean;
    enabled?: boolean;
    snapshots?: boolean;
    alerts?: boolean;
    detections?: boolean;
  };
}
