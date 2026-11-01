export interface CameraDetectConfig {
  enabled: boolean;
  width: number;
  height: number;
  fps: number;
}

export interface CameraMotionConfig {
  enabled: boolean;
  threshold: number;
  contour_area: number;
  mask: unknown;
}

export interface ZoneConfig {
  coordinates: string;
  objects?: string[];
  inertia?: number;
  loitering_time?: number;
}

export interface CameraConfig {
  name: string;
  enabled: boolean;
  detect: CameraDetectConfig;
  motion: CameraMotionConfig;
  zones: Record<string, ZoneConfig>;
  record: {
    enabled: boolean;
  };
  snapshots: {
    enabled: boolean;
  };
}

export interface NinjaConfigResponse {
  version: string;
  safe_mode: boolean;
  cameras: Record<string, CameraConfig>;
  objects?: {
    track?: string[];
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
  ffmpeg_cpu?: string;
  capture_cpu?: string;
  detect_cpu?: string;
}

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
    storage: Record<string, StorageStatItem>;
    last_updated: number;
  };
  processes: Record<string, ProcessStatItem>;
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
  data?: {
    score?: number;
    top_score?: number;
    description?: string;
    box?: [number, number, number, number];
  };
}

export interface RecordingSegment {
  id: string;
  start_time: number;
  end_time: number;
  duration: number;
  motion: number;
  objects: number;
  segment_size: number;
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
  motion: boolean;
  objects: Array<{
    id: string;
    label: string;
    score: number;
    stationary: boolean;
  }>;
}
