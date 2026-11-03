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
  required_zones: string[];
  cutoff_time: number;
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
}

export interface LineConfig {
  points: [number, number, number, number];
  direction?: "any" | "a_to_b" | "b_to_a";
  objects?: string[];
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
  ffmpeg?: CameraFfmpegConfig;
  live?: CameraLiveConfig;
  detect: CameraDetectConfig;
  motion: CameraMotionConfig;
  objects?: ObjectsConfig;
  zones: Record<string, ZoneConfig>;
  lines?: Record<string, LineConfig>;
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
  detect?: CameraDetectConfig;
  motion?: CameraMotionConfig;
  objects?: ObjectsConfig;
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
    zones?: string[];
    detections?: string[];
    audio?: string[];
    dbfs?: number;
    notes?: string;
    provenance?: TriggerProvenance | null;
    detected_attributes?: DetectedAttribute[];
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
    description?: string;
    notes?: string;
    attributes?: string[];
    detected_attributes?: DetectedAttribute[];
    face_box?: [number, number, number, number] | null;
    license_plate_box?: [number, number, number, number] | null;
    box?: [number, number, number, number];
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
