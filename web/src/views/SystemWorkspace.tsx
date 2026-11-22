import React from "react";
import { formatMeasured, parseMeasured, resolvePrimaryInferenceMs, sparklineCeiling } from "../utils/detectorTelemetry";
import { AlertTriangle, CheckCircle2, Copy, Pause, Play, RefreshCw, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import {
  CameraGranularStorageDto,
  CameraRuntimeStats,
  HeuristicAlertItem,
  NinjaConfigResponse,
  NinjaStatsResponse,
  StorageBreakdownResponse,
  StorageCleanupResponse,
  StorageCleanupTarget,
} from "../types/nvr";
import { formatUptime, resolveEffectiveTheme } from "../utils/format";
import {
  resolveCanvasThemePalette,
  subscribePrefersLightScheme,
} from "../utils/themeTokens";
import { CanvasSparkline } from "../components/VideoPlayers";
import { useTwoStepConfirm } from "./settings/vision/useTwoStepConfirm";

export type HealthStatusFilter = "all" | "offline" | "active_ai";
export type HealthSortColumn =
  | "channel"
  | "camera_fps"
  | "detection_fps"
  | "ffmpeg_cpu";
export type HealthSortDirection = "asc" | "desc";

export function toFiniteNumber(val: unknown, fallback: number = 0): number {
  if (typeof val === "number") {
    return Number.isFinite(val) ? val : fallback;
  }
  if (typeof val === "string") {
    const parsed = parseFloat(val);
    return Number.isFinite(parsed) ? parsed : fallback;
  }
  return fallback;
}

export function formatCameraTelemetryValue(
  val: unknown,
  digits: number = 1,
  unit: string = ""
): string {
  if (val === null || val === undefined) {
    return "—";
  }
  if (typeof val === "number") {
    return Number.isFinite(val) ? `${val.toFixed(digits)}${unit}` : "—";
  }
  if (typeof val === "string") {
    const trimmed = val.trim();
    if (!trimmed) return "—";
    const parsed = parseFloat(trimmed);
    return Number.isFinite(parsed) ? `${parsed.toFixed(digits)}${unit}` : "—";
  }
  return "—";
}

export function formatStorageBytes(rawBytes: unknown): string {
  const bytes = Math.max(0, toFiniteNumber(rawBytes, 0));
  if (bytes < 1024) {
    return `${Math.round(bytes)} B`;
  }
  const kb = bytes / 1024;
  if (kb < 1024) {
    return `${kb >= 100 ? Math.round(kb) : kb.toFixed(1)} KB`;
  }
  const mb = kb / 1024;
  if (mb < 1024) {
    return `${mb >= 100 ? mb.toFixed(0) : mb.toFixed(1)} MB`;
  }
  const gb = mb / 1024;
  if (gb < 1024) {
    return `${gb.toFixed(2)} GB`;
  }
  const tb = gb / 1024;
  return `${tb.toFixed(2)} TB`;
}

export function formatStorageShortBytes(rawBytes: unknown): string {
  const bytes = Math.max(0, toFiniteNumber(rawBytes, 0));
  if (bytes <= 0) return "0 B";
  const mb = bytes / (1024 * 1024);
  if (mb < 1) {
    return `${Math.max(1, Math.round(bytes / 1024))}K`;
  }
  if (mb < 1024) {
    return `${mb >= 10 ? Math.round(mb) : mb.toFixed(1)}M`;
  }
  const gb = mb / 1024;
  return `${gb >= 10 ? gb.toFixed(1) : gb.toFixed(2)}G`;
}

export function formatRetentionSpan(
  oldestTs: unknown,
  nowSec: number = Date.now() / 1000
): string | null {
  const ts = toFiniteNumber(oldestTs, 0);
  if (ts <= 0 || !Number.isFinite(nowSec) || nowSec <= ts) {
    return null;
  }
  const ageSec = nowSec - ts;
  const hours = ageSec / 3600;
  if (hours < 24) {
    return `${Math.max(0.1, hours).toFixed(1)}h`;
  }
  const days = hours / 24;
  return `${days.toFixed(1)}d`;
}

export interface StorageStackSegment {
  key: string;
  label: string;
  bytes: number;
  pct: number;
  color: string;
}

export function computeStorageStackSegments(
  breakdown: StorageBreakdownResponse | null,
  isPt: boolean
): StorageStackSegment[] {
  if (!breakdown) return [];
  const c = breakdown.categories;
  const contBytes = Math.max(0, toFiniteNumber(c?.recordings_continuous?.bytes, 0));
  const motBytes = Math.max(0, toFiniteNumber(c?.recordings_motion?.bytes, 0));
  const evtSegBytes = Math.max(0, toFiniteNumber(c?.recordings_events?.bytes, 0));
  const evRevClipBytes =
    Math.max(0, toFiniteNumber(c?.events_media?.bytes, 0)) +
    Math.max(0, toFiniteNumber(c?.reviews_media?.bytes, 0));
  const prevExpBytes =
    Math.max(0, toFiniteNumber(c?.previews_media?.bytes, 0)) +
    Math.max(0, toFiniteNumber(c?.exports_media?.bytes, 0));
  const sysBytes =
    Math.max(0, toFiniteNumber(c?.dataset_and_models?.bytes, 0)) +
    Math.max(0, toFiniteNumber(c?.database?.bytes, 0));

  const raw: Array<{ key: string; label: string; bytes: number; color: string }> = [
    {
      key: "continuous",
      label: isPt ? "MP4 Contínuo" : "Continuous MP4",
      bytes: contBytes,
      color: "var(--signal-live)",
    },
    {
      key: "motion",
      label: isPt ? "MP4 Movimento" : "Motion MP4",
      bytes: motBytes,
      color: "var(--signal-motion)",
    },
    {
      key: "event_mp4",
      label: isPt ? "MP4 Eventos" : "Event MP4",
      bytes: evtSegBytes,
      color: "var(--signal-alert)",
    },
    {
      key: "clips_media",
      label: isPt ? "Snapshots & Revisões" : "Snapshots & Reviews",
      bytes: evRevClipBytes,
      color: "var(--signal-accent)",
    },
    {
      key: "previews_exports",
      label: isPt ? "Previews & Exports" : "Previews & Exports",
      bytes: prevExpBytes,
      color: "var(--text-secondary)",
    },
    {
      key: "sys_db",
      label: isPt ? "Dataset / Modelos / DB" : "Dataset / Models / DB",
      bytes: sysBytes,
      color: "var(--border-strong)",
    },
  ];

  const sum = raw.reduce((acc, item) => acc + item.bytes, 0);
  if (sum <= 0) return [];
  return raw
    .filter((item) => item.bytes > 0)
    .map((item) => ({
      ...item,
      pct: Math.min(100, (item.bytes / sum) * 100),
    }));
}

export function buildCameraGranularRows(
  stats: NinjaStatsResponse | null,
  breakdown: StorageBreakdownResponse | null
): CameraGranularStorageDto[] {
  if (breakdown && breakdown.cameras && Object.keys(breakdown.cameras).length > 0) {
    return Object.values(breakdown.cameras).sort((a, b) =>
      a.camera.localeCompare(b.camera, undefined, { numeric: true })
    );
  }
  const camStorage = stats?.service?.camera_storage;
  if (!camStorage) return [];
  const fallbackRunway = toFiniteNumber(stats?.service?.estimated_retention_days, 14);
  return Object.entries(camStorage)
    .sort(([a], [b]) => a.localeCompare(b, undefined, { numeric: true }))
    .map(([cam, cs]) => {
      const recBytes = Math.max(0, toFiniteNumber(cs.usage, 0)) * 1024 * 1024;
      const contBytes = Math.max(0, toFiniteNumber(cs.continuous_mb, 0)) * 1024 * 1024;
      const motBytes = Math.max(0, toFiniteNumber(cs.motion_mb, 0)) * 1024 * 1024;
      const evtBytes = Math.max(0, toFiniteNumber(cs.event_segments_mb, 0)) * 1024 * 1024;
      const compBytes = Math.max(0, toFiniteNumber(cs.compacted_mb, 0)) * 1024 * 1024;
      return {
        camera: cam,
        configured: true,
        total_bytes: recBytes,
        recordings_bytes: recBytes,
        continuous_bytes: contBytes,
        motion_bytes: motBytes,
        event_segments_bytes: evtBytes,
        compacted_bytes: compBytes,
        retained_past_continuous_bytes: 0,
        retained_past_continuous_count: 0,
        segments_count: Math.max(0, Math.round(toFiniteNumber(cs.segments_count, 0))),
        oldest_segment_ts: cs.oldest_ts ?? null,
        newest_segment_ts: cs.newest_ts ?? null,
        events_count: Math.max(0, Math.round(toFiniteNumber(cs.events_count, 0))),
        pinned_events_count: Math.max(0, Math.round(toFiniteNumber(cs.pinned_events_count, 0))),
        events_media_bytes: 0,
        reviews_count: Math.max(0, Math.round(toFiniteNumber(cs.reviews_count, 0))),
        reviews_media_bytes: 0,
        previews_bytes: 0,
        previews_files_count: 0,
        exports_bytes: 0,
        exports_count: 0,
        bandwidth_mb_hr: Math.max(0, toFiniteNumber(cs.bandwidth, 0)),
        estimated_retention_days: toFiniteNumber(
          cs.estimated_retention_days ?? fallbackRunway,
          fallbackRunway
        ),
        continuous_days: 7,
        event_retention_days: 7,
      };
    });
}

const SENSITIVE_RTSP_QUERY_KEYS = new Set([
  "password",
  "pass",
  "passwd",
  "pwd",
  "token",
  "access_token",
  "auth_token",
  "api_key",
  "apikey",
  "secret",
  "auth",
  "key",
  "sig",
  "signature",
]);

function scrubRtspPathAndQuery(pathAndQuery: string): string {
  const qIdx = pathAndQuery.indexOf("?");
  if (qIdx < 0) return pathAndQuery;
  const pathPart = pathAndQuery.slice(0, qIdx + 1);
  const queryPart = pathAndQuery.slice(qIdx + 1);
  const scrubbedPairs = queryPart.split("&").map((pair) => {
    const eqIdx = pair.indexOf("=");
    if (eqIdx < 0) return pair;
    const rawKey = pair.slice(0, eqIdx);
    const keyLower = rawKey.trim().toLowerCase();
    if (SENSITIVE_RTSP_QUERY_KEYS.has(keyLower)) {
      return `${rawKey}=***`;
    }
    return pair;
  });
  return `${pathPart}${scrubbedPairs.join("&")}`;
}

export function normalizeRtspProducerEndpoint(rawUrl: string): string | null {
  const clean = (rawUrl || "")
    .split("#")[0]
    .trim()
    .replace(/^(?:ffmpeg:)+/i, "");
  if (!clean) return null;
  const schemeMatch = /^(rtsps?):\/\/(.*)$/i.exec(clean);
  if (!schemeMatch) return null;
  const scheme = schemeMatch[1].toLowerCase();
  const remainder = schemeMatch[2];
  const slashIdx = remainder.indexOf("/");
  const authority = slashIdx >= 0 ? remainder.slice(0, slashIdx) : remainder;
  const pathAndQuery = scrubRtspPathAndQuery(
    slashIdx >= 0 ? remainder.slice(slashIdx) : "/"
  );
  const atIdx = authority.lastIndexOf("@");
  const hostPort = (atIdx >= 0 ? authority.slice(atIdx + 1) : authority).trim();
  if (!hostPort) return null;

  let hostOnly = hostPort.toLowerCase();
  if (hostOnly.startsWith("[")) {
    const closeBracket = hostOnly.indexOf("]");
    if (closeBracket < 0) return null;
    hostOnly = hostOnly.slice(1, closeBracket);
  } else {
    const colonIdx = hostOnly.indexOf(":");
    if (colonIdx >= 0) {
      hostOnly = hostOnly.slice(0, colonIdx);
    }
  }
  hostOnly = hostOnly
    .replace(/\.+$/, "")
    .replace(/^(?:::ffff:|0:0:0:0:0:ffff:)/, "");
  if (
    !hostOnly ||
    hostOnly === "localhost" ||
    hostOnly.endsWith(".localhost") ||
    hostOnly === "0.0.0.0" ||
    hostOnly === "0:0" ||
    hostOnly === "::" ||
    hostOnly === "::1" ||
    hostOnly === "0:0:0:0:0:0:0:0" ||
    hostOnly === "0:0:0:0:0:0:0:1" ||
    /^7f[0-9a-f]{2}:[0-9a-f]{1,4}$/.test(hostOnly) ||
    /^127(?:\.\d{1,3}){3}$/.test(hostOnly)
  ) {
    return null;
  }
  return `${scheme}://${hostPort.toLowerCase()}${pathAndQuery}`;
}

export function filterAndSortCameraHealth(
  cameraNames: string[],
  statsMap: Record<string, CameraRuntimeStats> | undefined,
  options: {
    searchQuery?: string;
    statusFilter?: HealthStatusFilter;
    sortColumn?: HealthSortColumn;
    sortDirection?: HealthSortDirection;
  } = {}
): string[] {
  const {
    searchQuery = "",
    statusFilter = "all",
    sortColumn = "channel",
    sortDirection = "asc",
  } = options;
  const q = searchQuery.trim().toLowerCase();

  const filtered = cameraNames.filter((cam) => {
    if (q && !cam.toLowerCase().includes(q)) {
      return false;
    }
    const cs = statsMap?.[cam];
    if (statusFilter === "offline") {
      return toFiniteNumber(cs?.camera_fps, 0) <= 0;
    }
    if (statusFilter === "active_ai") {
      return toFiniteNumber(cs?.detection_fps, 0) > 0;
    }
    return true;
  });

  const dir = sortDirection === "desc" ? -1 : 1;
  return [...filtered].sort((a, b) => {
    const csA = statsMap?.[a];
    const csB = statsMap?.[b];
    let diff = 0;
    if (sortColumn === "channel") {
      diff = a.localeCompare(b, undefined, { numeric: true });
    } else if (sortColumn === "camera_fps") {
      diff =
        toFiniteNumber(csA?.camera_fps, 0) - toFiniteNumber(csB?.camera_fps, 0);
    } else if (sortColumn === "detection_fps") {
      diff =
        toFiniteNumber(csA?.detection_fps, 0) -
        toFiniteNumber(csB?.detection_fps, 0);
    } else if (sortColumn === "ffmpeg_cpu") {
      diff =
        toFiniteNumber(csA?.ffmpeg_cpu, 0) - toFiniteNumber(csB?.ffmpeg_cpu, 0);
    }
    if (diff !== 0 && Number.isFinite(diff)) {
      return diff * dir;
    }
    return a.localeCompare(b, undefined, { numeric: true });
  });
}

export function cameraUsesSharedMainLoopback(
  config: NinjaConfigResponse | null,
  camName: string
): boolean {
  const trimmed = camName.trim();
  if (!trimmed || !config) return false;
  const camCfg = config.cameras?.[trimmed];
  const stream1 = camCfg?.live?.streams?.["Stream 1"]?.trim();
  if (stream1 && stream1 === trimmed) {
    return true;
  }
  const loop127 = `rtsp://127.0.0.1:8554/${trimmed}`;
  const loopLocal = `rtsp://localhost:8554/${trimmed}`;
  const streamsMap = config.go2rtc?.streams;
  if (streamsMap && Object.keys(streamsMap).length > 0) {
    const subKey = `${trimmed}_sub`;
    const subVal = streamsMap[subKey];
    if (subVal === undefined) {
      if (streamsMap[trimmed] !== undefined || camCfg !== undefined) {
        return true;
      }
    } else {
      const firstUrl = Array.isArray(subVal)
        ? typeof subVal[0] === "string"
          ? subVal[0]
          : ""
        : typeof subVal === "string"
        ? subVal
        : "";
      const cleanSub = firstUrl.split("#")[0]?.trim() || "";
      if (cleanSub === loop127 || cleanSub === loopLocal) {
        return true;
      }
    }
  }
  const detectInput = camCfg?.ffmpeg?.inputs?.find((inp) =>
    inp.roles?.includes("detect")
  );
  if (detectInput?.path) {
    const cleanDet = detectInput.path.split("#")[0]?.trim() || "";
    if (cleanDet === loop127 || cleanDet === loopLocal) {
      return true;
    }
  }
  return false;
}

export function buildClientFallbackHeuristics(
  config: NinjaConfigResponse | null,
  stats: NinjaStatsResponse | null
): HeuristicAlertItem[] {
  if (stats?.service?.heuristics && stats.service.heuristics.length > 0) {
    return stats.service.heuristics.filter((alert) => {
      if (alert.id.startsWith("detect_ffmpeg_cpu_")) {
        const camName = alert.target.trim();
        if (cameraUsesSharedMainLoopback(config, camName)) {
          const ffVal = toFiniteNumber(
            stats?.cameras?.[camName]?.ffmpeg_cpu,
            0
          );
          const allCpus = Object.values(stats?.cameras || {})
            .map((c) => toFiniteNumber(c?.ffmpeg_cpu, 0))
            .filter((v) => v > 0);
          const minFf =
            allCpus.length > 0 ? Math.max(0.3, Math.min(...allCpus)) : 0.3;
          return ffVal >= 3.5 && ffVal >= minFf * 3.5;
        }
      }
      return true;
    });
  }
  const out: HeuristicAlertItem[] = [];
  const cams = config?.cameras || {};
  const enabledEntries = Object.entries(cams).filter(([, c]) => c.enabled);

  // 1. Duplicate external RTSP URLs in go2rtc.streams
  const urlMap: Record<string, string[]> = {};
  for (const [sName, sVal] of Object.entries(config?.go2rtc?.streams || {})) {
    const raw = Array.isArray(sVal)
      ? typeof sVal[0] === "string"
        ? sVal[0]
        : ""
      : typeof sVal === "string"
      ? sVal
      : "";
    const normalizedEndpoint = normalizeRtspProducerEndpoint(raw);
    if (normalizedEndpoint) {
      if (!urlMap[normalizedEndpoint]) urlMap[normalizedEndpoint] = [];
      urlMap[normalizedEndpoint].push(sName);
    }
  }
  for (const names of Object.values(urlMap)) {
    if (names.length >= 2) {
      out.push({
        id: `duplicate_rtsp_producer_${names[0]}`,
        severity: "warning",
        category: "streams",
        target: names.join(", "),
        title: "Duplicate upstream RTSP connections to physical camera",
        detail: `Streams [${names.join(
          ", "
        )}] pull the identical external RTSP endpoint directly from the camera, opening ${
          names.length
        } concurrent RTSP sessions and duplicating network traffic.`,
        recommendation: `Point '${names[1]}' in go2rtc.streams to 'rtsp://127.0.0.1:8554/${names[0]}' so go2rtc pulls from the camera once and multiplexes locally in RAM.`,
      });
    }
  }

  // 2. Detect aspect ratio & FFmpeg downscale CPU outlier
  const ffCpus: Array<[string, number]> = [];
  for (const [cName, cCfg] of enabledEntries) {
    const rawW = toFiniteNumber(cCfg.detect?.width, 1280);
    const rawH = toFiniteNumber(cCfg.detect?.height, 720);
    const w = rawW > 0 ? rawW : 1280;
    const h = rawH > 0 ? rawH : 720;
    const ratio = w / Math.max(1, h);
    if (Math.abs(ratio - 16 / 9) > 0.15) {
      out.push({
        id: `detect_aspect_mismatch_${cName}`,
        severity: "warning",
        category: "vision",
        target: cName,
        title: `Non-16:9 detect resolution (${w}x${h})`,
        detail: `Camera '${cName}' runs detect/preview at ${w}x${h} (aspect ${ratio.toFixed(
          2
        )}:1) while main recording is 16:9, causing lower preview sharpness and bounding-box projection distortion.`,
        recommendation:
          "Set detect.width: 1280 and detect.height: 720 (16:9) and ensure the go2rtc substream uses a 16:9 VBR profile.",
      });
    }
    const ffVal = toFiniteNumber(stats?.cameras?.[cName]?.ffmpeg_cpu, 0);
    if (ffVal > 0) {
      ffCpus.push([cName, ffVal]);
    }
  }
  if (ffCpus.length >= 2) {
    const minFf = Math.max(0.3, Math.min(...ffCpus.map(([, v]) => v)));
    for (const [cName, ffVal] of ffCpus) {
      const isSharedLoopback = cameraUsesSharedMainLoopback(config, cName);
      const minAbsCpu = isSharedLoopback ? 3.5 : 1.4;
      const minRatio = isSharedLoopback ? 3.5 : 1.8;
      if (ffVal >= minAbsCpu && ffVal >= minFf * minRatio) {
        out.push({
          id: `detect_ffmpeg_cpu_${cName}`,
          severity: "optimization",
          category: "vision",
          target: cName,
          title: "Elevated FFmpeg decode/scale CPU on detect pipeline",
          detail: `Camera '${cName}' detect decoder uses ${ffVal.toFixed(
            1
          )}% CPU (vs ${minFf.toFixed(
            1
          )}% peer baseline), caused by software downscaling a 1080p/20fps stream or CBR substream packet flood.`,
          recommendation:
            "Share a single go2rtc loopback producer (rtsp://127.0.0.1:8554/<cam>) or lower the camera's encoder FPS to 15 FPS.",
        });
      }
    }
  }

  // 3. Storage capacity & retention runway
  const recSt = stats?.service?.storage?.["/media/ninjanvr/recordings"];
  const estDays = toFiniteNumber(stats?.service?.estimated_retention_days, 14);
  const recTotal = toFiniteNumber(recSt?.total, 0);
  const recUsed = Math.max(0, toFiniteNumber(recSt?.used, 0));
  const recFree = Math.max(
    0,
    toFiniteNumber(recSt?.free, Math.max(0, recTotal - recUsed))
  );
  if (recSt && recTotal > 0) {
    const usedPct = Math.min(100, (recUsed / recTotal) * 100);
    const freeGb = recFree / 1024;
    if (
      usedPct >= 85 &&
      !config?.record?.min_free_gb &&
      !config?.record?.max_size_gb
    ) {
      out.push({
        id: "storage_capacity_unbounded",
        severity: usedPct >= 95 ? "critical" : "warning",
        category: "storage",
        target: "/media/ninjanvr/recordings",
        title: `Recording volume at ${usedPct.toFixed(1)}% capacity without min_free_gb guard`,
        detail: `/media/ninjanvr/recordings has only ${freeGb.toFixed(
          1
        )} GB free (${usedPct.toFixed(
          1
        )}% used, ~${estDays.toFixed(
          1
        )}d runway), and neither record.min_free_gb nor record.max_size_gb is configured.`,
        recommendation:
          "Configure record.min_free_gb (e.g. 100) or record.max_size_gb in config.yml so retention cleanup prunes segments before disk exhaustion.",
      });
    }
  }

  const daysList = enabledEntries
    .filter(([, c]) => c.record?.enabled !== false)
    .map(([, c]) => toFiniteNumber(c.record?.continuous?.days, 14));
  if (daysList.length > 0) {
    const minD = Math.min(...daysList);
    const maxD = Math.max(...daysList);
    const hasAsymmetry = daysList.length >= 2 && maxD - minD >= 3;
    const exceedsRunway = maxD > estDays + 1;
    if (hasAsymmetry) {
      out.push({
        id: "retention_runway_asymmetry",
        severity: "warning",
        category: "storage",
        target: "record.continuous.days",
        title: exceedsRunway
          ? "Retention policy exceeds disk runway & camera asymmetry"
          : "Camera continuous retention asymmetry detected",
        detail: `Configured continuous retention varies from ${minD}d to ${maxD}d across cameras, while free storage supports ~${estDays.toFixed(
          1
        )}d at current write rate.`,
        recommendation:
          "Standardize cameras.*.record.continuous.days across cameras or enable continuous_substream / mode_tiering.",
      });
    } else if (exceedsRunway) {
      out.push({
        id: "retention_runway_shortfall",
        severity: "warning",
        category: "storage",
        target: "record.continuous.days",
        title: "Retention policy exceeds available disk runway",
        detail: `Configured continuous retention is ${maxD}d, while free storage currently supports ~${estDays.toFixed(
          1
        )}d at the measured write rate.`,
        recommendation:
          "Lower record.continuous.days to match disk capacity, expand the recordings volume, or enable continuous_substream / mode_tiering.",
      });
    }
  }

  // 4. Audio role active while audio detection disabled
  const unusedAudio = enabledEntries
    .filter(
      ([, c]) =>
        !c.audio?.enabled &&
        !config?.audio?.enabled &&
        (c.ffmpeg?.inputs || []).some((inp) => (inp.roles || []).includes("audio"))
    )
    .map(([n]) => n);
  if (unusedAudio.length > 0) {
    out.push({
      id: "audio_role_disabled_mismatch",
      severity: "optimization",
      category: "audio",
      target: unusedAudio.join(", "),
      title: "Continuous AAC audio transcoding while acoustic detection is disabled",
      detail: `Cameras [${unusedAudio.join(
        ", "
      )}] include the 'audio' FFmpeg input role and transcode audio to AAC (-c:a aac) on every segment while audio.enabled is false.`,
      recommendation:
        "Enable audio.enabled if acoustic event detection is desired, or remove the 'audio' role on mute channels to save FFmpeg CPU cycles.",
    });
  }

  // 5. Snapshot / preview quality drift across cameras
  const snapQuals = new Set(
    enabledEntries.map(([, c]) => c.snapshots?.quality ?? 100)
  );
  const prevQuals = new Set(
    enabledEntries.map(([, c]) => c.record?.preview?.quality || "medium")
  );
  if (snapQuals.size > 1 || prevQuals.size > 1) {
    out.push({
      id: "camera_quality_drift",
      severity: "optimization",
      category: "config",
      target: "snapshots / preview",
      title: "Inconsistent snapshot or preview quality across cameras",
      detail:
        "Enabled cameras have mixed snapshots.quality (e.g. 95 vs 100) or record.preview.quality (very_high vs medium) settings.",
      recommendation:
        "Standardize snapshots.quality: 100 and record.preview.quality: off across all cameras.",
    });
  }

  return out;
}

export interface CleanupStatusNotice {
  kind: "running" | "success" | "error";
  text: string;
}

export interface ArmedCleanupMeta {
  key: string;
  target: StorageCleanupTarget;
  cameraOverride?: string | null;
  summaryPt: string;
  summaryEn: string;
}

export async function runStorageCleanupFlow(params: {
  target: StorageCleanupTarget;
  effectiveCam: string | null;
  olderThanDays: number;
  keepPinnedEvents: boolean;
  pruneRetainedSegments: boolean;
  isPt: boolean;
  fetchImpl?: typeof fetch;
  isMounted?: () => boolean;
  onNotice: (notice: CleanupStatusNotice) => void;
  notify: (msg: string) => void;
  refreshStatsAndBreakdown: () => Promise<void>;
  scheduleDelayedRefresh: () => void;
}): Promise<void> {
  const {
    target,
    effectiveCam,
    olderThanDays,
    keepPinnedEvents,
    pruneRetainedSegments,
    isPt,
    fetchImpl = fetch,
    isMounted = () => true,
    onNotice,
    notify,
    refreshStatsAndBreakdown,
    scheduleDelayedRefresh,
  } = params;
  const scopeLabel = effectiveCam ? ` [${effectiveCam.toUpperCase()}]` : "";
  onNotice({
    kind: "running",
    text: isPt
      ? `Executando limpeza${scopeLabel} no SQLite e disco — aguarde...`
      : `Running storage cleanup${scopeLabel} across SQLite & disk — please wait...`,
  });
  try {
    const res = await fetchImpl("/api/storage/cleanup", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        target,
        camera: effectiveCam,
        older_than_days: olderThanDays > 0 ? olderThanDays : null,
        include_pinned: !keepPinnedEvents,
        prune_retained_segments: pruneRetainedSegments,
      }),
    });
    if (!isMounted()) return;
    if (res.ok) {
      const data: StorageCleanupResponse = await res.json();
      if (!isMounted()) return;
      const freedText = formatStorageBytes(data.freed_bytes);
      const okMsg = isPt
        ? `Limpeza concluída${scopeLabel}: ${data.deleted_items ?? 0} item(ns)${
            data.deleted_segments > 0
              ? ` + ${data.deleted_segments} seg(s) MP4`
              : ""
          } removidos · ${freedText} liberados`
        : `Cleanup completed${scopeLabel}: removed ${data.deleted_items ?? 0} item(s)${
            data.deleted_segments > 0
              ? ` + ${data.deleted_segments} MP4 seg(s)`
              : ""
          } · freed ${freedText}`;
      onNotice({ kind: "success", text: okMsg });
      notify(okMsg);
      await refreshStatsAndBreakdown();
    } else {
      const errBody = await res.json().catch(() => null);
      if (!isMounted()) return;
      const isGatewayTimeout =
        res.status === 502 || res.status === 504 || res.status === 524;
      const msg =
        errBody && typeof errBody.message === "string"
          ? errBody.message
          : `HTTP ${res.status}`;
      const errText = isGatewayTimeout
        ? isPt
          ? `Tempo limite do proxy (${msg}) durante limpeza extensa${scopeLabel} — a operação em segundo plano pode ainda estar concluindo; atualizando métricas...`
          : `Proxy timeout (${msg}) during large cleanup${scopeLabel} — background cleanup may still be finishing; refreshing disk usage...`
        : isPt
        ? `Falha na limpeza de armazenamento${scopeLabel} (${msg})`
        : `Storage cleanup failed${scopeLabel} (${msg})`;
      onNotice({ kind: "error", text: errText });
      notify(errText);
      await refreshStatsAndBreakdown();
      if (isGatewayTimeout) {
        scheduleDelayedRefresh();
      }
    }
  } catch {
    if (isMounted()) {
      const netMsg = isPt
        ? `Conexão interrompida durante a limpeza${scopeLabel} — verificando uso atualizado de disco...`
        : `Connection interrupted during cleanup${scopeLabel} — refreshing current disk usage...`;
      onNotice({ kind: "error", text: netMsg });
      notify(netMsg);
      await refreshStatsAndBreakdown();
      scheduleDelayedRefresh();
    }
  }
}

interface SystemWorkspaceProps {
  cameraNames: string[];
  logService: "ninjanvr" | "go2rtc" | "nginx" | "cameras";
  setLogService: (svc: "ninjanvr" | "go2rtc" | "nginx" | "cameras") => void;
  logLines: string[];
  logSearch: string;
  setLogSearch: (q: string) => void;
  logPaused: boolean;
  setLogPaused: React.Dispatch<React.SetStateAction<boolean>>;
  notify: (msg: string) => void;
  initialRightMode?: "alerts" | "logs";
  initialBreakdown?: StorageBreakdownResponse | null;
  initialArmedCleanup?: ArmedCleanupMeta | null;
  initialCleanupNotice?: CleanupStatusNotice | null;
  onStorageMutated?: () => void;
}

export const SystemWorkspace: React.FC<SystemWorkspaceProps> = ({
  cameraNames,
  logService,
  setLogService,
  logLines,
  logSearch,
  setLogSearch,
  logPaused,
  setLogPaused,
  notify,
  initialRightMode = "alerts",
  initialBreakdown = null,
  initialArmedCleanup = null,
  initialCleanupNotice = null,
  onStorageMutated,
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const config = useNvrStore((s) => s.config);
  const stats = useNvrStore((s) => s.stats);
  const setStats = useNvrStore((s) => s.setStats);
  const statsHistory = useNvrStore((s) => s.statsHistory);
  const themeMode = useNvrStore((s) => s.themeMode);
  const highContrast = useNvrStore((s) => s.highContrast);
  const [systemThemeTick, setSystemThemeTick] = React.useState(0);
  React.useEffect(() => {
    if (themeMode !== "system") return;
    return subscribePrefersLightScheme(() =>
      setSystemThemeTick((tick) => tick + 1)
    );
  }, [themeMode]);
  const effectiveTheme = resolveEffectiveTheme(themeMode);
  const canvasPalette = React.useMemo(
    () => resolveCanvasThemePalette(effectiveTheme, highContrast),
    [effectiveTheme, highContrast, systemThemeTick]
  );

  const sysCpu = formatMeasured(parseMeasured(stats?.cpu_usages?.["ninjanvr.full_system"]?.cpu), 1, "%");
  const sysMem = formatMeasured(parseMeasured(stats?.cpu_usages?.["ninjanvr.full_system"]?.mem), 1, "%");
  const inferMs = formatMeasured(resolvePrimaryInferenceMs(stats), 1, " ms");
  const inferHistory = statsHistory.map((s) => resolvePrimaryInferenceMs(s));
  const [aiBriefText, setAiBriefText] = React.useState<string | null>(null);
  const [aiBriefBusy, setAiBriefBusy] = React.useState(false);
  const [syncStorageBusy, setSyncStorageBusy] = React.useState(false);
  const [storageBreakdown, setStorageBreakdown] =
    React.useState<StorageBreakdownResponse | null>(initialBreakdown);
  const [breakdownBusy, setBreakdownBusy] = React.useState(false);
  const [breakdownError, setBreakdownError] = React.useState(false);
  const [cleanupBusyKey, setCleanupBusyKey] = React.useState<string | null>(null);
  const [cleanupNotice, setCleanupNotice] =
    React.useState<CleanupStatusNotice | null>(initialCleanupNotice);
  const [armedMeta, setArmedMeta] = React.useState<ArmedCleanupMeta | null>(
    initialArmedCleanup
  );
  const [cleanupCameraScope, setCleanupCameraScope] = React.useState<string>("all");
  const [cleanupOlderDays, setCleanupOlderDays] = React.useState<number>(0);
  const [keepPinnedEvents, setKeepPinnedEvents] = React.useState<boolean>(true);
  const [pruneRetainedSegments, setPruneRetainedSegments] = React.useState<boolean>(true);
  const { armedKey, isArmed, confirmOrArm, clearArmed } = useTwoStepConfirm(8000);
  const isMountedRef = React.useRef(true);
  const followUpTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(null);

  const isKeyArmed = React.useCallback(
    (key: string) =>
      isArmed(key) || (armedKey === null && initialArmedCleanup?.key === key && armedMeta?.key === key),
    [isArmed, armedKey, initialArmedCleanup, armedMeta]
  );

  const activeArmedMeta =
    armedMeta && isKeyArmed(armedMeta.key) ? armedMeta : null;

  const cancelArmedCleanup = React.useCallback(() => {
    clearArmed();
    setArmedMeta(null);
  }, [clearArmed]);

  React.useEffect(() => {
    if ((!armedKey && !activeArmedMeta) || typeof window === "undefined") return;
    const onKeyDown = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") {
        cancelArmedCleanup();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [armedKey, activeArmedMeta, cancelArmedCleanup]);

  const fetchStorageBreakdown = React.useCallback(
    async (forceRefresh: boolean = false) => {
      if (typeof fetch !== "function") return;
      if (isMountedRef.current) {
        setBreakdownBusy(true);
      }
      try {
        const url = forceRefresh
          ? "/api/storage/breakdown?refresh=1"
          : "/api/storage/breakdown";
        const res = await fetch(url, { credentials: "include" });
        if (!isMountedRef.current) return;
        if (res.ok) {
          const data: StorageBreakdownResponse = await res.json();
          if (isMountedRef.current && data && typeof data === "object") {
            setStorageBreakdown(data);
            setBreakdownError(false);
          }
        } else if (isMountedRef.current) {
          setBreakdownError(true);
        }
      } catch {
        if (isMountedRef.current) {
          setBreakdownError(true);
        }
      } finally {
        if (isMountedRef.current) {
          setBreakdownBusy(false);
        }
      }
    },
    []
  );

  React.useEffect(() => {
    isMountedRef.current = true;
    void fetchStorageBreakdown(false);
    return () => {
      isMountedRef.current = false;
      if (followUpTimerRef.current !== null) {
        clearTimeout(followUpTimerRef.current);
        followUpTimerRef.current = null;
      }
    };
  }, [fetchStorageBreakdown]);

  const refreshStatsAndBreakdown = React.useCallback(async () => {
    await fetchStorageBreakdown(true);
    try {
      const sRes = await fetch("/api/stats", { credentials: "include" });
      if (sRes.ok && isMountedRef.current) {
        const freshStats: NinjaStatsResponse = await sRes.json();
        if (isMountedRef.current && freshStats && typeof freshStats === "object") {
          setStats(freshStats);
        }
      }
    } catch {
      // Ignore transient refresh failure
    }
    onStorageMutated?.();
  }, [fetchStorageBreakdown, onStorageMutated, setStats]);

  const scheduleDelayedRefresh = React.useCallback(() => {
    if (followUpTimerRef.current !== null) {
      clearTimeout(followUpTimerRef.current);
    }
    followUpTimerRef.current = setTimeout(() => {
      followUpTimerRef.current = null;
      if (isMountedRef.current) {
        void refreshStatsAndBreakdown();
      }
    }, 5000);
  }, [refreshStatsAndBreakdown]);

  const cameraGranularRows = React.useMemo(
    () => buildCameraGranularRows(stats, storageBreakdown),
    [stats, storageBreakdown]
  );
  const effectiveCameraScope = React.useMemo(() => {
    if (cleanupCameraScope === "all") return "all";
    return cameraGranularRows.some((r) => r.camera === cleanupCameraScope)
      ? cleanupCameraScope
      : "all";
  }, [cleanupCameraScope, cameraGranularRows]);

  React.useEffect(() => {
    if (cleanupCameraScope !== "all" && effectiveCameraScope === "all") {
      setCleanupCameraScope("all");
    }
  }, [cleanupCameraScope, effectiveCameraScope]);

  const stackSegments = React.useMemo(
    () => computeStorageStackSegments(storageBreakdown, isPt),
    [storageBreakdown, isPt]
  );
  const scopedCamDto =
    effectiveCameraScope !== "all"
      ? storageBreakdown?.cameras?.[effectiveCameraScope] ??
        cameraGranularRows.find((r) => r.camera === effectiveCameraScope) ??
        null
      : null;

  const executeStorageCleanup = React.useCallback(
    async (
      busyKey: string,
      target: StorageCleanupTarget,
      cameraOverride?: string | null
    ) => {
      if (cleanupBusyKey) return;
      cancelArmedCleanup();
      if (followUpTimerRef.current !== null) {
        clearTimeout(followUpTimerRef.current);
        followUpTimerRef.current = null;
      }
      setCleanupBusyKey(busyKey);
      const effectiveCam =
        cameraOverride !== undefined
          ? cameraOverride
          : effectiveCameraScope === "all"
          ? null
          : effectiveCameraScope;
      try {
        await runStorageCleanupFlow({
          target,
          effectiveCam,
          olderThanDays: cleanupOlderDays,
          keepPinnedEvents,
          pruneRetainedSegments,
          isPt,
          isMounted: () => isMountedRef.current,
          onNotice: (notice) => {
            if (isMountedRef.current) {
              setCleanupNotice(notice);
            }
          },
          notify,
          refreshStatsAndBreakdown,
          scheduleDelayedRefresh,
        });
      } finally {
        if (isMountedRef.current) {
          setCleanupBusyKey(null);
        }
      }
    },
    [
      cleanupBusyKey,
      cancelArmedCleanup,
      effectiveCameraScope,
      cleanupOlderDays,
      keepPinnedEvents,
      pruneRetainedSegments,
      isPt,
      notify,
      refreshStatsAndBreakdown,
      scheduleDelayedRefresh,
    ]
  );

  const triggerTwoStepCleanup = React.useCallback(
    (
      key: string,
      target: StorageCleanupTarget,
      summaryPt: string,
      summaryEn: string,
      cameraOverride?: string | null
    ) => {
      if (cleanupBusyKey) return;
      const confirmed = confirmOrArm(key, () => {
        setArmedMeta(null);
        void executeStorageCleanup(key, target, cameraOverride);
      });
      if (!confirmed) {
        setArmedMeta({
          key,
          target,
          cameraOverride,
          summaryPt,
          summaryEn,
        });
      }
    },
    [cleanupBusyKey, confirmOrArm, executeStorageCleanup]
  );

  const safeCopyText = async (text: string, successMsg: string) => {
    if (
      typeof navigator !== "undefined" &&
      navigator.clipboard &&
      typeof navigator.clipboard.writeText === "function"
    ) {
      try {
        await navigator.clipboard.writeText(text);
        if (isMountedRef.current) {
          notify(successMsg);
        }
        return;
      } catch {
        // Fall through to failure notice
      }
    }
    if (isMountedRef.current) {
      notify("Clipboard access unavailable in this browser context");
    }
  };
  const renderStableCleanupLabel = (idleText: string, liveText: string) => (
    <span className="nvr-cleanup-label-stack">
      {liveText !== idleText && (
        <span className="nvr-cleanup-label-sizer" aria-hidden="true">
          {idleText}
        </span>
      )}
      <span className="nvr-cleanup-label-live">{liveText}</span>
    </span>
  );

  const [rightMode, setRightMode] = React.useState<"alerts" | "logs">(
    initialRightMode
  );
  const [healthSearch, setHealthSearch] = React.useState("");
  const [healthStatusFilter, setHealthStatusFilter] =
    React.useState<HealthStatusFilter>("all");
  const [healthSortCol, setHealthSortCol] =
    React.useState<HealthSortColumn>("channel");
  const [healthSortDir, setHealthSortDir] =
    React.useState<HealthSortDirection>("asc");

  const filteredHealthCameras = React.useMemo(
    () =>
      filterAndSortCameraHealth(cameraNames, stats?.cameras, {
        searchQuery: healthSearch,
        statusFilter: healthStatusFilter,
        sortColumn: healthSortCol,
        sortDirection: healthSortDir,
      }),
    [
      cameraNames,
      stats?.cameras,
      healthSearch,
      healthStatusFilter,
      healthSortCol,
      healthSortDir,
    ]
  );

  const toggleHealthSort = (col: HealthSortColumn) => {
    if (healthSortCol === col) {
      setHealthSortDir((d) => (d === "asc" ? "desc" : "asc"));
    } else {
      setHealthSortCol(col);
      setHealthSortDir(col === "channel" ? "asc" : "desc");
    }
  };

  const heuristicAlerts = React.useMemo(
    () => buildClientFallbackHeuristics(config, stats),
    [config, stats]
  );
  const filteredAlerts = React.useMemo(
    () =>
      heuristicAlerts.filter((a) => {
        if (!logSearch.trim()) return true;
        const q = logSearch.toLowerCase();
        return (
          a.title.toLowerCase().includes(q) ||
          a.detail.toLowerCase().includes(q) ||
          a.target.toLowerCase().includes(q) ||
          a.category.toLowerCase().includes(q) ||
          a.recommendation.toLowerCase().includes(q)
        );
      }),
    [heuristicAlerts, logSearch]
  );
  const critCount = heuristicAlerts.filter((a) => a.severity === "critical").length;
  const warnCount = heuristicAlerts.filter((a) => a.severity === "warning").length;
  const optCount = heuristicAlerts.filter((a) => a.severity === "optimization").length;

  return (
    <div className="nvr-workbench nvr-system-workbench">
      <div className="sr-only" role="status" aria-live="polite">
        {armedKey
          ? isPt
            ? "Ação de limpeza armada. Pressione novamente em até 8 segundos (ou confirme na barra de armazenamento) ou Escape para cancelar."
            : "Cleanup action armed. Activate again within 8 seconds (or confirm in the storage banner), or press Escape to cancel."
          : ""}
      </div>
      {/* Left Telemetry Column */}
      <div className="nvr-panel">
        <div className="nvr-panel-header">
          <span>
            {isPt
              ? `TELEMETRIA DE HARDWARE & PIPELINE (UPTIME ${formatUptime(
                  Math.max(0, toFiniteNumber(stats?.service?.uptime, 0))
                )})`
              : `HARDWARE & PIPELINE TELEMETRY (UPTIME ${formatUptime(
                  Math.max(0, toFiniteNumber(stats?.service?.uptime, 0))
                )})`}
          </span>
          <span className="mono">{stats?.service?.version || "—"}</span>
        </div>
        <div
          className="nvr-scroll-y"
          style={{ padding: 12, display: "flex", flexDirection: "column", gap: 12 }}
        >
          {/* Native AI Context Summary & MCP Ecosystem Inspector */}
          <div
            style={{
              background: "var(--bg-canvas)",
              border: "1px solid var(--signal-accent)",
              borderRadius: 3,
            }}
          >
            <div
              className="nvr-panel-header"
              style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: 6 }}
            >
              <span>
                {isPt
                  ? "RESUMO SITUACIONAL DE IA & SERVIDOR MCP (/api/ai/context · /api/mcp)"
                  : "AI SITUATION BRIEF & MCP SERVER (/api/ai/context · /api/mcp)"}
              </span>
              <div style={{ display: "flex", gap: 6 }}>
                <button
                  type="button"
                  className="nvr-btn mono"
                  style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() => {
                    const url = `${window.location.origin}/api/mcp`;
                    void safeCopyText(
                      url,
                      `Copied MCP JSON-RPC endpoint: ${url}`
                    );
                  }}
                >
                  {isPt ? "Copiar URL MCP" : "Copy MCP URL"}
                </button>
                <button
                  type="button"
                  className="nvr-btn primary mono"
                  style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  disabled={aiBriefBusy}
                  onClick={async () => {
                    if (aiBriefBusy) return;
                    setAiBriefBusy(true);
                    try {
                      const res = await fetch("/api/ai/context?hours=24", {
                        credentials: "include",
                      });
                      if (!isMountedRef.current) return;
                      if (res.ok) {
                        const data = await res.json();
                        if (!isMountedRef.current) return;
                        setAiBriefText(
                          data.markdown_brief || JSON.stringify(data, null, 2)
                        );
                        notify(
                          isPt
                            ? "Resumo situacional de IA (24h) gerado"
                            : "Generated 24h AI Situation Brief"
                        );
                      } else {
                        notify(`Failed to fetch AI context (${res.status})`);
                      }
                    } catch {
                      if (isMountedRef.current) {
                        notify("Failed to reach /api/ai/context");
                      }
                    } finally {
                      if (isMountedRef.current) {
                        setAiBriefBusy(false);
                      }
                    }
                  }}
                >
                  {aiBriefBusy
                    ? isPt
                      ? "Gerando..."
                      : "Generating..."
                    : isPt
                    ? "Gerar Resumo IA 24h"
                    : "Generate 24h AI Brief"}
                </button>
              </div>
            </div>
            {aiBriefText && (
              <pre
                className="mono"
                style={{
                  margin: 0,
                  padding: 10,
                  fontSize: "var(--text-xs)",
                  color: "var(--text-primary)",
                  whiteSpace: "pre-wrap",
                  maxHeight: 220,
                  overflowY: "auto",
                  borderTop: "1px solid var(--border-hairline)",
                }}
              >
                {aiBriefText}
              </pre>
            )}
          </div>

          {/* 3 Canvas Sparklines */}
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
              gap: 10,
            }}
          >
            <div
              style={{
                background: "var(--bg-subcard)",
                border: "1px solid var(--border-hairline)",
                borderRadius: 3,
                padding: 10,
              }}
            >
              <div
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  gap: 6,
                  fontSize: "var(--text-xs)",
                  marginBottom: 6,
                  whiteSpace: "nowrap",
                }}
              >
                <span style={{ color: "var(--text-secondary)" }}>
                  {stats?.primary_detector?.device
                    ? `${isPt ? "INFERÊNCIA" : "INFERENCE"} · ${stats.primary_detector.device}`
                    : isPt
                    ? "INFERÊNCIA"
                    : "INFERENCE"}
                </span>
                <span
                  className="mono"
                  style={{ color: "var(--signal-accent-text)", fontWeight: 700 }}
                >
                  {inferMs}
                </span>
              </div>
              <CanvasSparkline
                values={inferHistory.map((v) => v ?? 0)}
                color={canvasPalette.sparklineAccentStroke}
                maxVal={sparklineCeiling(inferHistory)}
              />
            </div>

            <div
              style={{
                background: "var(--bg-subcard)",
                border: "1px solid var(--border-hairline)",
                borderRadius: 3,
                padding: 10,
              }}
            >
              <div
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  fontSize: "var(--text-xs)",
                  marginBottom: 6,
                }}
              >
                <span style={{ color: "var(--text-secondary)" }}>
                  {isPt ? "CPU DO SISTEMA" : "SYSTEM CPU"}
                </span>
                <span
                  className="mono"
                  style={{ color: "var(--signal-live-text)", fontWeight: 700 }}
                >
                  {sysCpu}
                </span>
              </div>
              <CanvasSparkline
                values={statsHistory.map((s) =>
                  toFiniteNumber(s.cpu_usages?.["ninjanvr.full_system"]?.cpu, 0)
                )}
                color={canvasPalette.sparklineLiveStroke}
                maxVal={100}
              />
            </div>

            <div
              style={{
                background: "var(--bg-subcard)",
                border: "1px solid var(--border-hairline)",
                borderRadius: 3,
                padding: 10,
              }}
            >
              <div
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  fontSize: "var(--text-xs)",
                  marginBottom: 6,
                }}
              >
                <span style={{ color: "var(--text-secondary)" }}>
                  {isPt ? "MEMÓRIA RSS" : "MEMORY RSS"}
                </span>
                <span
                  className="mono"
                  style={{ color: "var(--signal-motion-text)", fontWeight: 700 }}
                >
                  {sysMem}
                </span>
              </div>
              <CanvasSparkline
                values={statsHistory.map((s) =>
                  toFiniteNumber(s.cpu_usages?.["ninjanvr.full_system"]?.mem, 0)
                )}
                color={canvasPalette.sparklineMotionStroke}
                maxVal={100}
              />
            </div>
          </div>

          {/* Per-Camera Pipeline Table */}
          <div className="nvr-system-card">
            <div
              className="nvr-panel-header"
              style={{
                height: "auto",
                minHeight: 36,
                padding: "6px 12px",
                flexWrap: "wrap",
              }}
            >
              <span>
                {t.system.cameraHealth} ({filteredHealthCameras.length}/
                {cameraNames.length})
              </span>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: 6,
                  flexWrap: "wrap",
                }}
              >
                <input
                  type="text"
                  className="nvr-input mono nvr-system-filter-input"
                  style={{ width: 136 }}
                  placeholder={isPt ? "Buscar câmera..." : "Search camera..."}
                  aria-label="Search camera health"
                  value={healthSearch}
                  onChange={(e) => setHealthSearch(e.target.value)}
                />
                {(
                  [
                    ["all", isPt ? "Todas" : "All"],
                    ["offline", isPt ? "Offline / 0 FPS" : "Offline / 0 FPS"],
                    ["active_ai", isPt ? "IA Ativa" : "Active AI"],
                  ] as Array<[HealthStatusFilter, string]>
                ).map(([flt, label]) => (
                  <button
                    key={flt}
                    type="button"
                    className={`nvr-btn mono nvr-system-chip-btn ${
                      healthStatusFilter === flt ? "active" : ""
                    }`}
                    onClick={() => setHealthStatusFilter(flt)}
                  >
                    {label}
                  </button>
                ))}
              </div>
            </div>
            <div
              className="nvr-table-region"
              role="region"
              tabIndex={0}
              aria-label={
                isPt ? "Telemetria de pipeline por câmera" : "Per-camera pipeline telemetry"
              }
            >
              <table className="nvr-table mono">
              <thead>
                <tr>
                  <th
                    scope="col"
                    aria-sort={
                      healthSortCol === "channel"
                        ? healthSortDir === "asc"
                          ? "ascending"
                          : "descending"
                        : "none"
                    }
                  >
                    <button
                      type="button"
                      className="nvr-table-sort-btn mono"
                      onClick={() => toggleHealthSort("channel")}
                    >
                      <span>{isPt ? "Canal" : "Channel"}</span>
                      <span aria-hidden="true">
                        {healthSortCol === "channel"
                          ? healthSortDir === "asc"
                            ? "▲"
                            : "▼"
                          : ""}
                      </span>
                    </button>
                  </th>
                  <th
                    scope="col"
                    aria-sort={
                      healthSortCol === "camera_fps"
                        ? healthSortDir === "asc"
                          ? "ascending"
                          : "descending"
                        : "none"
                    }
                  >
                    <button
                      type="button"
                      className="nvr-table-sort-btn mono"
                      onClick={() => toggleHealthSort("camera_fps")}
                    >
                      <span>Cam FPS</span>
                      <span aria-hidden="true">
                        {healthSortCol === "camera_fps"
                          ? healthSortDir === "asc"
                            ? "▲"
                            : "▼"
                          : ""}
                      </span>
                    </button>
                  </th>
                  <th scope="col">Proc FPS</th>
                  <th
                    scope="col"
                    aria-sort={
                      healthSortCol === "detection_fps"
                        ? healthSortDir === "asc"
                          ? "ascending"
                          : "descending"
                        : "none"
                    }
                  >
                    <button
                      type="button"
                      className="nvr-table-sort-btn mono"
                      onClick={() => toggleHealthSort("detection_fps")}
                    >
                      <span>{isPt ? "FPS IA" : "AI Detect FPS"}</span>
                      <span aria-hidden="true">
                        {healthSortCol === "detection_fps"
                          ? healthSortDir === "asc"
                            ? "▲"
                            : "▼"
                          : ""}
                      </span>
                    </button>
                  </th>
                  <th
                    scope="col"
                    aria-sort={
                      healthSortCol === "ffmpeg_cpu"
                        ? healthSortDir === "asc"
                          ? "ascending"
                          : "descending"
                        : "none"
                    }
                  >
                    <button
                      type="button"
                      className="nvr-table-sort-btn mono"
                      onClick={() => toggleHealthSort("ffmpeg_cpu")}
                    >
                      <span>FFmpeg CPU</span>
                      <span aria-hidden="true">
                        {healthSortCol === "ffmpeg_cpu"
                          ? healthSortDir === "asc"
                            ? "▲"
                            : "▼"
                          : ""}
                      </span>
                    </button>
                  </th>
                  <th scope="col">{isPt ? "CPU Captura" : "Capture CPU"}</th>
                </tr>
              </thead>
              <tbody>
                {filteredHealthCameras.length === 0 ? (
                  <tr>
                    <td
                      colSpan={6}
                      style={{
                        textAlign: "center",
                        padding: "16px 12px",
                        color: "var(--text-muted)",
                        whiteSpace: "normal",
                      }}
                    >
                      {isPt
                        ? "Nenhuma câmera corresponde aos filtros selecionados."
                        : "No cameras match the active health filters."}
                    </td>
                  </tr>
                ) : (
                  filteredHealthCameras.map((cam) => {
                    const cs = stats?.cameras?.[cam];
                    return (
                      <tr key={cam}>
                        <td style={{ fontWeight: 700, textTransform: "uppercase" }}>{cam}</td>
                        <td>{formatCameraTelemetryValue(cs?.camera_fps, 1)}</td>
                        <td>{formatCameraTelemetryValue(cs?.process_fps, 1)}</td>
                        <td style={{ color: "var(--signal-accent-text)" }}>
                          {formatCameraTelemetryValue(cs?.detection_fps, 1)}
                        </td>
                        <td>{formatCameraTelemetryValue(cs?.ffmpeg_cpu, 1, "%")}</td>
                        <td>{formatCameraTelemetryValue(cs?.capture_cpu, 1, "%")}</td>
                      </tr>
                    );
                  })
                )}
              </tbody>
            </table>
            </div>
          </div>

          {/* Storage Volumes & Granular Cleanup */}
          <div className="nvr-system-card">
            <div
              className="nvr-panel-header"
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                flexWrap: "wrap",
                gap: 6,
                height: "auto",
                minHeight: 34,
                padding: "6px 12px",
              }}
            >
              <span>{t.system.storageVolumes}</span>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <button
                  type="button"
                  className="nvr-btn xs mono"
                  aria-busy={breakdownBusy}
                  title={
                    isPt
                      ? "Recalcular uso detalhado de disco em todas as categorias"
                      : "Refresh granular on-disk storage breakdown"
                  }
                  disabled={breakdownBusy || cleanupBusyKey !== null}
                  onClick={() => {
                    void refreshStatsAndBreakdown();
                  }}
                >
                  <RefreshCw size={11} />
                  <span>
                    {breakdownBusy
                      ? isPt
                        ? "Lendo..."
                        : "Scanning..."
                      : isPt
                      ? "Atualizar Uso"
                      : "Refresh Usage"}
                  </span>
                </button>
                <button
                  type="button"
                  className={`nvr-btn xs mono nvr-cleanup-btn ${
                    isKeyArmed("hdr:retention_pass") ? "danger-armed" : ""
                  }`}
                  aria-busy={cleanupBusyKey === "hdr:retention_pass"}
                  aria-label={
                    cleanupBusyKey === "hdr:retention_pass"
                      ? isPt
                        ? "Aplicando... — Aplicar Retenção"
                        : "Running... — Run Retention"
                      : isKeyArmed("hdr:retention_pass")
                      ? isPt
                        ? "Confirmar Retenção? — Aplicar Retenção"
                        : "Confirm Retention? — Run Retention"
                      : isPt
                      ? "Aplicar Retenção"
                      : "Run Retention"
                  }
                  title={
                    isPt
                      ? "Executar imediatamente o ciclo completo de retenção, cotas e checkpoint WAL"
                      : "Execute full retention, quota eviction, and WAL checkpoint pass immediately"
                  }
                  disabled={cleanupBusyKey !== null || Boolean(storageBreakdown?.scoped)}
                  onClick={() => {
                    triggerTwoStepCleanup(
                      "hdr:retention_pass",
                      "retention_pass",
                      "Ciclo completo de retenção, cotas e checkpoint WAL",
                      "Full retention, quota eviction & WAL checkpoint pass",
                      null
                    );
                  }}
                >
                  {renderStableCleanupLabel(
                    isPt ? "Aplicar Retenção" : "Run Retention",
                    cleanupBusyKey === "hdr:retention_pass"
                      ? isPt
                        ? "Aplicando..."
                        : "Running..."
                      : isKeyArmed("hdr:retention_pass")
                      ? isPt
                        ? "Confirmar Retenção?"
                        : "Confirm Retention?"
                      : isPt
                      ? "Aplicar Retenção"
                      : "Run Retention"
                  )}
                </button>
                <button
                  type="button"
                  className="nvr-btn xs mono"
                  aria-busy={syncStorageBusy}
                  title={
                    isPt
                      ? "Reconciliar tabela SQLite de gravações contra os arquivos MP4 no disco"
                      : "Reconcile SQLite recordings table against on-disk MP4 files"
                  }
                  disabled={syncStorageBusy || cleanupBusyKey !== null}
                  onClick={async () => {
                    if (syncStorageBusy) return;
                    setSyncStorageBusy(true);
                    try {
                      const res = await fetch("/api/recordings/sync", {
                        method: "POST",
                        credentials: "include",
                      });
                      if (!isMountedRef.current) return;
                      if (res.ok) {
                        const data = await res.json();
                        if (!isMountedRef.current) return;
                        notify(
                          isPt
                            ? `DB sincronizado: ${data.removed_orphans ?? 0} segmento(s) órfão(s) removido(s) em ${data.duration_ms ?? 0}ms`
                            : `Synced Storage DB: removed ${data.removed_orphans ?? 0} orphan segment(s) in ${data.duration_ms ?? 0}ms`
                        );
                        await refreshStatsAndBreakdown();
                      } else {
                        notify(
                          isPt
                            ? `Falha ao sincronizar DB (${res.status})`
                            : `Storage DB sync failed (${res.status})`
                        );
                      }
                    } catch {
                      if (isMountedRef.current) {
                        notify(
                          isPt
                            ? "Falha de rede ao sincronizar DB"
                            : "Storage DB sync request failed"
                        );
                      }
                    } finally {
                      if (isMountedRef.current) {
                        setSyncStorageBusy(false);
                      }
                    }
                  }}
                >
                  {syncStorageBusy
                    ? isPt
                      ? "Sincronizando..."
                      : "Syncing..."
                    : isPt
                    ? "Sincronizar DB"
                    : "Sync Storage DB"}
                </button>
              </div>
            </div>
            <div
              className="nvr-table-region"
              role="region"
              tabIndex={0}
              aria-label={isPt ? "Volumes de armazenamento" : "Storage mounts"}
            >
              <table className="nvr-table mono">
                <thead>
                  <tr>
                    <th scope="col">{isPt ? "Ponto de Montagem" : "Mount Path"}</th>
                    <th scope="col">FS</th>
                    <th scope="col">{isPt ? "Usado" : "Used"}</th>
                    <th scope="col">{isPt ? "Total" : "Total"}</th>
                    <th scope="col">{isPt ? "Utilização" : "Utilization"}</th>
                  </tr>
                </thead>
                <tbody>
                  {Object.entries(stats?.service?.storage || {}).map(([mPath, st]) => {
                    const safeUsed = Math.max(0, toFiniteNumber(st?.used, 0));
                    const safeTotal = Math.max(1, toFiniteNumber(st?.total, 1));
                    const pct = Math.max(
                      0,
                      Math.min(100, Math.round((safeUsed / safeTotal) * 100))
                    );
                    const isCrit = pct >= 90;
                    const isWarn = pct >= 80 && pct < 90;
                    const barColor = isCrit
                      ? "var(--signal-alert)"
                      : isWarn
                      ? "var(--signal-motion)"
                      : "var(--signal-live)";
                    const textColor = isCrit
                      ? "var(--signal-alert-text)"
                      : isWarn
                      ? "var(--signal-motion-text)"
                      : "var(--text-primary)";
                    return (
                      <tr key={mPath}>
                        <td>
                          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                            <span>{mPath}</span>
                            {st.mount_type === "cold_tier" || mPath.includes("cold") ? (
                              <span className="nvr-badge mono">
                                {isPt ? "ARQUIVO FRIO" : "COLD ARCHIVE TIER"}
                              </span>
                            ) : mPath.includes("recordings") ? (
                              <span className="nvr-badge live mono">
                                {isPt ? "TIER QUENTE" : "HOT TIER"}
                              </span>
                            ) : null}
                          </div>
                        </td>
                        <td>{st.mount_type}</td>
                        <td>{(safeUsed / 1024).toFixed(1)} GB</td>
                        <td>{(toFiniteNumber(st?.total, 0) / 1024).toFixed(1)} GB</td>
                        <td>
                          <div className="nvr-util-cell">
                            <div className="nvr-util-bar" aria-hidden="true">
                              <div
                                className="nvr-util-fill"
                                style={{
                                  width: `${pct}%`,
                                  background: barColor,
                                }}
                              />
                            </div>
                            <span
                              style={{
                                color: textColor,
                                fontWeight: isCrit || isWarn ? 700 : 500,
                              }}
                            >
                              {pct}%
                            </span>
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>

            {/* Granular Storage Composition & Cleanup Matrix */}
            {storageBreakdown && stackSegments.length > 0 && (
              <div className="nvr-storage-stack-wrap">
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 8,
                    flexWrap: "wrap",
                    fontSize: "var(--text-xs)",
                  }}
                >
                  <span
                    className="mono"
                    style={{
                      fontWeight: 700,
                      color: "var(--text-primary)",
                      textTransform: "uppercase",
                    }}
                  >
                    {isPt
                      ? "Uso Granular do NVR por Categoria"
                      : "Granular NVR Footprint by Category"}
                  </span>
                  <span className="mono" style={{ color: "var(--text-secondary)" }}>
                    {isPt ? "Rastreado pelo NVR: " : "NVR Tracked: "}
                    <strong style={{ color: "var(--text-primary)" }}>
                      {formatStorageBytes(storageBreakdown.total_tracked_bytes)}
                    </strong>
                  </span>
                </div>
                <div className="nvr-storage-stack-bar" aria-hidden="true">
                  {stackSegments.map((seg) => {
                    const pctLabel =
                      seg.pct < 0.1 ? "<0.1%" : `${seg.pct.toFixed(1)}%`;
                    return (
                      <div
                        key={seg.key}
                        className="nvr-storage-stack-seg"
                        title={`${seg.label}: ${formatStorageBytes(seg.bytes)} (${pctLabel})`}
                        style={{
                          width: `${seg.pct}%`,
                          background: seg.color,
                        }}
                      />
                    );
                  })}
                </div>
                <div className="nvr-storage-legend mono">
                  {stackSegments.map((seg) => (
                    <span key={seg.key} className="nvr-storage-legend-item">
                      <span
                        className="nvr-storage-dot"
                        style={{ background: seg.color }}
                        aria-hidden="true"
                      />
                      <span>{seg.label}:</span>
                      <strong style={{ color: "var(--text-primary)" }}>
                        {formatStorageBytes(seg.bytes)}
                      </strong>
                    </span>
                  ))}
                </div>
              </div>
            )}

            {(storageBreakdown ||
              cameraGranularRows.length > 0 ||
              breakdownBusy ||
              breakdownError ||
              activeArmedMeta ||
              cleanupNotice) &&
              (() => {
                if (cleanupBusyKey !== null || cleanupNotice?.kind === "running") {
                  return (
                    <div
                      className="nvr-cleanup-banner is-running mono"
                      role="status"
                      data-testid="storage-cleanup-status-banner"
                    >
                      <span>
                        {cleanupNotice?.text ||
                          (isPt
                            ? "Executando limpeza no SQLite e disco — aguarde..."
                            : "Running storage cleanup across SQLite & disk — please wait...")}
                      </span>
                      <span className="nvr-badge mono" style={{ minBlockSize: 24, display: "inline-flex", alignItems: "center" }}>
                        {isPt ? "EM CURSO" : "ACTIVE"}
                      </span>
                    </div>
                  );
                }
                if (activeArmedMeta) {
                  return (
                    <div
                      className="nvr-cleanup-banner is-armed mono"
                      role="region"
                      data-testid="storage-cleanup-confirm-banner"
                      aria-label={
                        isPt
                          ? "Confirmação de limpeza de armazenamento"
                          : "Storage cleanup confirmation"
                      }
                    >
                      <div style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                        <AlertTriangle size={13} aria-hidden="true" />
                        <span>
                          {isPt
                            ? `Confirmar execução: ${activeArmedMeta.summaryPt}?`
                            : `Confirm cleanup: ${activeArmedMeta.summaryEn}?`}
                        </span>
                      </div>
                      <div className="nvr-cleanup-banner-actions">
                        <button
                          type="button"
                          className="nvr-btn xs mono danger"
                          data-testid="storage-cleanup-confirm-now"
                          onClick={() => {
                            const pending = activeArmedMeta;
                            cancelArmedCleanup();
                            void executeStorageCleanup(
                              pending.key,
                              pending.target,
                              pending.cameraOverride
                            );
                          }}
                        >
                          <Trash2 size={11} aria-hidden="true" />
                          <span>{isPt ? "Confirmar Limpeza" : "Confirm Clear"}</span>
                        </button>
                        <button
                          type="button"
                          className="nvr-btn xs mono"
                          data-testid="storage-cleanup-cancel"
                          onClick={() => {
                            cancelArmedCleanup();
                          }}
                        >
                          <span>{isPt ? "Cancelar" : "Cancel"}</span>
                        </button>
                      </div>
                    </div>
                  );
                }
                if (cleanupNotice) {
                  return (
                    <div
                      className={`nvr-cleanup-banner is-${cleanupNotice.kind} mono`}
                      role={cleanupNotice.kind === "error" ? "alert" : "status"}
                      data-testid="storage-cleanup-status-banner"
                    >
                      <span>{cleanupNotice.text}</span>
                      <button
                        type="button"
                        className="nvr-btn xs mono"
                        onClick={() => setCleanupNotice(null)}
                      >
                        <span>{isPt ? "Fechar" : "Dismiss"}</span>
                      </button>
                    </div>
                  );
                }
                if (!storageBreakdown && breakdownBusy) {
                  return (
                    <div className="nvr-cleanup-banner is-running mono" role="status">
                      <span>
                        {isPt
                          ? "Calculando detalhamento granular de categorias no disco..."
                          : "Scanning granular on-disk storage categories..."}
                      </span>
                      <span className="nvr-badge mono" style={{ minBlockSize: 24, display: "inline-flex", alignItems: "center" }}>
                        {isPt ? "LENDO" : "SCAN"}
                      </span>
                    </div>
                  );
                }
                if (!storageBreakdown && breakdownError) {
                  return (
                    <div className="nvr-cleanup-banner is-error mono" role="alert">
                      <span>
                        {isPt
                          ? "Não foi possível carregar o detalhamento por categoria agora."
                          : "Could not load granular category breakdown right now."}
                      </span>
                      <button
                        type="button"
                        className="nvr-btn xs mono"
                        onClick={() => {
                          void fetchStorageBreakdown(true);
                        }}
                      >
                        <RefreshCw size={11} aria-hidden="true" />
                        <span>{isPt ? "Tentar Novamente" : "Retry Scan"}</span>
                      </button>
                    </div>
                  );
                }
                return (
                  <div className="nvr-cleanup-banner is-idle mono">
                    <span>
                      {isPt
                        ? "Clique 2x em qualquer ação de limpeza (ou confirme aqui) para executar · Eventos fixados (★) protegidos por padrão"
                        : "Click any cleanup action twice (or confirm here) to execute · Pinned (★) events protected by default"}
                    </span>
                    <span
                      className="nvr-badge mono"
                      style={{
                        minBlockSize: 24,
                        display: "inline-flex",
                        alignItems: "center",
                      }}
                    >
                      {isPt ? "2 ETAPAS" : "2-STEP"}
                    </span>
                  </div>
                );
              })()}

            {(storageBreakdown || cameraGranularRows.length > 0) && (
              <div className="nvr-cleanup-toolbar mono">
                <div className="nvr-cleanup-controls">
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: 5,
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                    }}
                  >
                    <span>{isPt ? "Escopo:" : "Scope:"}</span>
                    <select
                      className="nvr-select mono nvr-cleanup-select"
                      aria-label={isPt ? "Escopo de câmera para limpeza" : "Cleanup camera scope"}
                      value={effectiveCameraScope}
                      onChange={(e) => {
                        cancelArmedCleanup();
                        setCleanupCameraScope(e.target.value);
                      }}
                    >
                      <option value="all">
                        {isPt ? "Todas as Câmeras" : "All Cameras"}
                      </option>
                      {cameraGranularRows.map((row) => (
                        <option key={row.camera} value={row.camera}>
                          {row.camera.toUpperCase()}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: 5,
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                    }}
                  >
                    <span>{isPt ? "Idade:" : "Age:"}</span>
                    <select
                      className="nvr-select mono nvr-cleanup-select"
                      aria-label={isPt ? "Filtro de idade para limpeza" : "Cleanup age filter"}
                      value={String(cleanupOlderDays)}
                      onChange={(e) => {
                        cancelArmedCleanup();
                        setCleanupOlderDays(Number(e.target.value) || 0);
                      }}
                    >
                      <option value="0">
                        {isPt ? "Tudo (Qualquer Idade)" : "All Ages (Everything)"}
                      </option>
                      <option value="1">
                        {isPt ? "Mais antigos que 24h" : "Older than 24h"}
                      </option>
                      <option value="3">
                        {isPt ? "Mais antigos que 3d" : "Older than 3d"}
                      </option>
                      <option value="7">
                        {isPt ? "Mais antigos que 7d" : "Older than 7d"}
                      </option>
                      <option value="14">
                        {isPt ? "Mais antigos que 14d" : "Older than 14d"}
                      </option>
                      <option value="30">
                        {isPt ? "Mais antigos que 30d" : "Older than 30d"}
                      </option>
                    </select>
                  </label>
                </div>
                <div className="nvr-cleanup-controls">
                  <label
                    className="nvr-cleanup-check"
                    title={
                      isPt
                        ? "Preserva eventos marcados para retenção indefinida e seus vídeos"
                        : "Preserves pinned (retain indefinitely) events and their recordings"
                    }
                  >
                    <input
                      type="checkbox"
                      checked={keepPinnedEvents}
                      onChange={(e) => {
                        cancelArmedCleanup();
                        setKeepPinnedEvents(e.target.checked);
                      }}
                    />
                    <span>{isPt ? "Manter fixados (★)" : "Keep pinned (★)"}</span>
                  </label>
                  <label
                    className="nvr-cleanup-check"
                    title={
                      isPt
                        ? "Ao limpar eventos, remove também revisões correspondentes e segmentos MP4 acima da janela contínua (preservando eventos fixados ★)"
                        : "When clearing events, also removes matching reviews and MP4 segments older than continuous retention (preserving pinned ★ events)"
                    }
                  >
                    <input
                      type="checkbox"
                      checked={pruneRetainedSegments}
                      onChange={(e) => {
                        cancelArmedCleanup();
                        setPruneRetainedSegments(e.target.checked);
                      }}
                    />
                    <span>
                      {isPt ? "Liberar MP4 retido (> cont.)" : "Free event-held MP4 (> cont.)"}
                    </span>
                  </label>
                </div>
              </div>
            )}

            {storageBreakdown && (
              <>
                <div
                  className="nvr-table-region"
                  role="region"
                  tabIndex={0}
                  aria-label={
                    isPt ? "Detalhamento de categorias e limpeza" : "Category breakdown and cleanup"
                  }
                >
                  <table className="nvr-table mono">
                    <thead>
                      <tr>
                        <th scope="col">{isPt ? "Categoria" : "Category"}</th>
                        <th scope="col">{isPt ? "Volume de Itens" : "Items"}</th>
                        <th scope="col">{isPt ? "Uso em Disco" : "Disk Usage"}</th>
                        <th scope="col" style={{ textAlign: "right" }}>
                          {isPt ? "Ação de Limpeza" : "Cleanup Action"}
                        </th>
                      </tr>
                    </thead>
                    <tbody>
                      {(() => {
                        const c = storageBreakdown.categories;
                        const evCount = scopedCamDto
                          ? scopedCamDto.events_count
                          : c.events_media.count;
                        const pinnedCount = scopedCamDto
                          ? scopedCamDto.pinned_events_count
                          : c.pinned_events_count;
                        const evBytes = scopedCamDto
                          ? scopedCamDto.events_media_bytes
                          : c.events_media.bytes;
                        const excessBytes = scopedCamDto
                          ? scopedCamDto.retained_past_continuous_bytes
                          : c.recordings_retained_excess.bytes;
                        const excessCount = scopedCamDto
                          ? scopedCamDto.retained_past_continuous_count
                          : c.recordings_retained_excess.count;
                        const revCount = scopedCamDto
                          ? scopedCamDto.reviews_count
                          : c.reviews_media.count;
                        const revBytes = scopedCamDto
                          ? scopedCamDto.reviews_media_bytes
                          : c.reviews_media.bytes;
                        const recBytes = scopedCamDto
                          ? scopedCamDto.recordings_bytes
                          : c.recordings_continuous.bytes +
                            c.recordings_motion.bytes +
                            c.recordings_events.bytes;
                        const recCount = scopedCamDto
                          ? scopedCamDto.segments_count
                          : c.recordings_continuous.count;
                        const contBytes = scopedCamDto
                          ? scopedCamDto.continuous_bytes
                          : c.recordings_continuous.bytes;
                        const motBytes = scopedCamDto
                          ? scopedCamDto.motion_bytes
                          : c.recordings_motion.bytes;
                        const evtSegBytes = scopedCamDto
                          ? scopedCamDto.event_segments_bytes
                          : c.recordings_events.bytes;
                        const prevCount = scopedCamDto
                          ? scopedCamDto.previews_files_count
                          : c.previews_media.count;
                        const prevBytes = scopedCamDto
                          ? scopedCamDto.previews_bytes
                          : c.previews_media.bytes;
                        const expCount = scopedCamDto
                          ? scopedCamDto.exports_count
                          : c.exports_media.count;
                        const expBytes = scopedCamDto
                          ? scopedCamDto.exports_bytes
                          : c.exports_media.bytes;

                        const ageBadge =
                          cleanupOlderDays > 0 ? ` (> ${cleanupOlderDays}d)` : "";
                        const scopeCtxLabel =
                          effectiveCameraScope !== "all"
                            ? ` [${effectiveCameraScope.toUpperCase()}]`
                            : "";

                        const renderCleanupBtn = (
                          key: string,
                          target: StorageCleanupTarget,
                          labelPt: string,
                          labelEn: string
                        ) => {
                          const armed = isKeyArmed(key);
                          const busy = cleanupBusyKey === key;
                          const baseLabel = `${isPt ? labelPt : labelEn}${ageBadge}`;
                          const armedText = isPt ? "Confirmar?" : "Confirm Clear?";
                          const busyText = isPt ? "Limpando..." : "Cleaning...";
                          const ariaText = busy
                            ? `${busyText} ${baseLabel}${scopeCtxLabel}`
                            : armed
                            ? `${armedText} — ${baseLabel}${scopeCtxLabel}`
                            : `${baseLabel}${scopeCtxLabel}`;
                          return (
                            <button
                              type="button"
                              className={`nvr-btn xs mono nvr-cleanup-btn ${
                                armed ? "danger-armed" : "danger-subtle"
                              }`}
                              aria-busy={busy}
                              aria-label={ariaText}
                              disabled={cleanupBusyKey !== null}
                              onClick={() => {
                                triggerTwoStepCleanup(
                                  key,
                                  target,
                                  `${labelPt}${ageBadge}${scopeCtxLabel}`,
                                  `${labelEn}${ageBadge}${scopeCtxLabel}`
                                );
                              }}
                            >
                              <Trash2 size={11} aria-hidden="true" />
                              {renderStableCleanupLabel(
                                baseLabel,
                                busy ? busyText : armed ? armedText : baseLabel
                              )}
                            </button>
                          );
                        };

                        return (
                          <>
                            <tr>
                              <td>
                                <div style={{ fontWeight: 700, color: "var(--text-primary)" }}>
                                  {isPt ? "Eventos & Snapshots" : "Events & Snapshots"}
                                </div>
                                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                                  {isPt
                                    ? "Catálogo IA + miniaturas/clips em /clips"
                                    : "AI event catalog + snapshots/clips in /clips"}
                                </div>
                              </td>
                              <td>
                                <span>
                                  {evCount} {isPt ? "eventos" : "events"}
                                </span>
                                {pinnedCount > 0 && (
                                  <span
                                    className="nvr-badge mono"
                                    style={{ marginLeft: 6 }}
                                    title={
                                      isPt
                                        ? "Eventos fixados (reter indefinidamente)"
                                        : "Pinned events (retain indefinitely)"
                                    }
                                  >
                                    {pinnedCount} ★
                                  </span>
                                )}
                              </td>
                              <td>
                                <div>{formatStorageBytes(evBytes)}</div>
                                {excessBytes > 0 && pruneRetainedSegments && (
                                  <div
                                    style={{
                                      fontSize: "var(--text-xs)",
                                      color: "var(--signal-motion-text)",
                                    }}
                                  >
                                    {isPt ? "até " : "up to "}
                                    {formatStorageBytes(excessBytes)}{" "}
                                    {isPt ? "MP4 > cont." : "MP4 > cont."}
                                  </div>
                                )}
                              </td>
                              <td style={{ textAlign: "right" }}>
                                {renderCleanupBtn(
                                  "cat:events",
                                  "events",
                                  "Limpar Eventos",
                                  "Clear Events"
                                )}
                              </td>
                            </tr>

                            <tr>
                              <td>
                                <div style={{ fontWeight: 700, color: "var(--text-primary)" }}>
                                  {isPt ? "Revisões / Triagem" : "Review Triage"}
                                </div>
                                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                                  {isPt
                                    ? "Segmentos de triagem + miniaturas em /clips/review"
                                    : "Review segments + thumbnails in /clips/review"}
                                </div>
                              </td>
                              <td>
                                {revCount} {isPt ? "revisões" : "reviews"}
                              </td>
                              <td>{formatStorageBytes(revBytes)}</td>
                              <td style={{ textAlign: "right" }}>
                                {renderCleanupBtn(
                                  "cat:reviews",
                                  "reviews",
                                  "Limpar Revisões",
                                  "Clear Reviews"
                                )}
                              </td>
                            </tr>

                            <tr>
                              <td>
                                <div style={{ fontWeight: 700, color: "var(--text-primary)" }}>
                                  {isPt
                                    ? "Excedente MP4 (> Janela Contínua)"
                                    : "Excess MP4 (> Continuous Window)"}
                                </div>
                                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                                  {isPt
                                    ? "Segmentos antigos mantidos apenas por eventos/movimento"
                                    : "Older MP4 segments held past continuous days by events/motion"}
                                </div>
                              </td>
                              <td>
                                {excessCount} {isPt ? "segmentos" : "segments"}
                              </td>
                              <td
                                style={{
                                  color:
                                    excessBytes > 0
                                      ? "var(--signal-motion-text)"
                                      : "var(--text-secondary)",
                                  fontWeight: excessBytes > 0 ? 700 : 400,
                                }}
                              >
                                {formatStorageBytes(excessBytes)}
                              </td>
                              <td style={{ textAlign: "right" }}>
                                {renderCleanupBtn(
                                  "cat:excess_recordings",
                                  "excess_recordings",
                                  "Podar Excedente",
                                  "Prune Excess"
                                )}
                              </td>
                            </tr>

                            <tr>
                              <td>
                                <div style={{ fontWeight: 700, color: "var(--text-primary)" }}>
                                  {isPt ? "Gravações MP4 (Total)" : "MP4 Recordings (Total)"}
                                </div>
                                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                                  Cont {formatStorageShortBytes(contBytes)} ·{" "}
                                  {isPt ? "Mov " : "Mot "}
                                  {formatStorageShortBytes(motBytes)} · Evt{" "}
                                  {formatStorageShortBytes(evtSegBytes)}
                                </div>
                              </td>
                              <td>
                                {recCount} {isPt ? "segmentos" : "segments"}
                              </td>
                              <td style={{ fontWeight: 700 }}>{formatStorageBytes(recBytes)}</td>
                              <td style={{ textAlign: "right" }}>
                                {renderCleanupBtn(
                                  "cat:recordings",
                                  "recordings",
                                  cleanupOlderDays > 0
                                    ? "Limpar Gravações"
                                    : "Limpar Tudo MP4",
                                  cleanupOlderDays > 0
                                    ? "Clear Recordings"
                                    : "Clear All MP4"
                                )}
                              </td>
                            </tr>

                            <tr>
                              <td>
                                <div style={{ fontWeight: 700, color: "var(--text-primary)" }}>
                                  {isPt ? "Previews & Scrubbing" : "Previews & Scrubbing"}
                                </div>
                                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                                  {isPt
                                    ? "MP4s horários e quadros rápidos da timeline"
                                    : "Hourly timeline MP4s & scrubbing JPEG frames"}
                                </div>
                              </td>
                              <td>
                                {prevCount} {isPt ? "arquivos" : "files"}
                              </td>
                              <td>{formatStorageBytes(prevBytes)}</td>
                              <td style={{ textAlign: "right" }}>
                                {renderCleanupBtn(
                                  "cat:previews",
                                  "previews",
                                  "Limpar Previews",
                                  "Clear Previews"
                                )}
                              </td>
                            </tr>

                            <tr>
                              <td>
                                <div style={{ fontWeight: 700, color: "var(--text-primary)" }}>
                                  {isPt ? "Exports & Shares" : "Exports & Share Clips"}
                                </div>
                                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                                  {isPt
                                    ? "Clipes exportados e cache de links compartilhados"
                                    : "Exported MP4 clips & guest share artifacts"}
                                </div>
                              </td>
                              <td>
                                {expCount} {isPt ? "exportações" : "exports"}
                                {!scopedCamDto && c.share_artifacts_count > 0
                                  ? ` · ${c.share_artifacts_count} ${
                                      isPt ? "compartilhamentos" : "shares"
                                    }`
                                  : ""}
                              </td>
                              <td>{formatStorageBytes(expBytes)}</td>
                              <td style={{ textAlign: "right" }}>
                                {renderCleanupBtn(
                                  "cat:exports",
                                  "exports",
                                  "Limpar Exports",
                                  "Clear Exports"
                                )}
                              </td>
                            </tr>

                            {!storageBreakdown.scoped && (
                              <>
                                <tr>
                                  <td>
                                    <div
                                      style={{
                                        display: "flex",
                                        alignItems: "center",
                                        gap: 6,
                                        flexWrap: "wrap",
                                      }}
                                    >
                                      <span
                                        style={{ fontWeight: 700, color: "var(--text-primary)" }}
                                      >
                                        {isPt
                                          ? "Dataset IA, Faces & Modelos"
                                          : "AI Dataset, Faces & Models"}
                                      </span>
                                      {effectiveCameraScope !== "all" && (
                                        <span className="nvr-badge mono">GLOBAL</span>
                                      )}
                                    </div>
                                    <div
                                      style={{
                                        fontSize: "var(--text-xs)",
                                        color: "var(--text-muted)",
                                      }}
                                    >
                                      {isPt
                                        ? "Amostras anotadas, perfis faciais e pesos ONNX/OpenVINO"
                                        : "Annotated samples, face profiles & ONNX/OpenVINO weights"}
                                    </div>
                                  </td>
                                  <td>
                                    {c.dataset_samples_count}{" "}
                                    {isPt ? "amostras" : "samples"} · {c.models_files_count}{" "}
                                    {isPt ? "modelos" : "models"}
                                  </td>
                                  <td>{formatStorageBytes(c.dataset_and_models.bytes)}</td>
                                  <td
                                    style={{
                                      textAlign: "right",
                                      color: "var(--text-muted)",
                                      fontSize: "var(--text-xs)",
                                    }}
                                  >
                                    {isPt ? "Protegido (Studio)" : "Protected (Studio)"}
                                  </td>
                                </tr>

                                <tr>
                                  <td>
                                    <div
                                      style={{
                                        display: "flex",
                                        alignItems: "center",
                                        gap: 6,
                                        flexWrap: "wrap",
                                      }}
                                    >
                                      <span
                                        style={{ fontWeight: 700, color: "var(--text-primary)" }}
                                      >
                                        {isPt ? "Banco de Dados SQLite" : "SQLite Database"}
                                      </span>
                                      {effectiveCameraScope !== "all" && (
                                        <span className="nvr-badge mono">GLOBAL</span>
                                      )}
                                    </div>
                                    <div
                                      style={{
                                        fontSize: "var(--text-xs)",
                                        color: "var(--text-muted)",
                                      }}
                                    >
                                      DB {formatStorageShortBytes(c.database_main_bytes)} · WAL{" "}
                                      {formatStorageShortBytes(c.database_wal_bytes)} ·{" "}
                                      {c.timeline_rows_count}{" "}
                                      {isPt ? "eventos timeline" : "timeline rows"}
                                    </div>
                                  </td>
                                  <td>
                                    {c.database.count} {isPt ? "arquivos" : "files"}
                                  </td>
                                  <td>{formatStorageBytes(c.database.bytes)}</td>
                                  <td style={{ textAlign: "right" }}>
                                    <button
                                      type="button"
                                      className={`nvr-btn xs mono nvr-cleanup-btn ${
                                        isKeyArmed("cat:wal_checkpoint") ? "danger-armed" : ""
                                      }`}
                                      aria-busy={cleanupBusyKey === "cat:wal_checkpoint"}
                                      aria-label={
                                        cleanupBusyKey === "cat:wal_checkpoint"
                                          ? isPt
                                            ? "Compactando... WAL SQLite"
                                            : "Checkpointing... WAL SQLite"
                                          : isKeyArmed("cat:wal_checkpoint")
                                          ? isPt
                                            ? "Confirmar WAL? — Compactar WAL SQLite"
                                            : "Confirm WAL? — Checkpoint WAL SQLite"
                                          : isPt
                                          ? "Compactar WAL SQLite"
                                          : "Checkpoint WAL SQLite"
                                      }
                                      disabled={cleanupBusyKey !== null}
                                      onClick={() => {
                                        triggerTwoStepCleanup(
                                          "cat:wal_checkpoint",
                                          "wal_checkpoint",
                                          "Compactar WAL SQLite",
                                          "Checkpoint SQLite WAL",
                                          null
                                        );
                                      }}
                                    >
                                      {renderStableCleanupLabel(
                                        isPt ? "Compactar WAL" : "Checkpoint WAL",
                                        cleanupBusyKey === "cat:wal_checkpoint"
                                          ? isPt
                                            ? "Compactando..."
                                            : "Checkpointing..."
                                          : isKeyArmed("cat:wal_checkpoint")
                                          ? isPt
                                            ? "Confirmar WAL?"
                                            : "Confirm WAL?"
                                          : isPt
                                          ? "Compactar WAL"
                                          : "Checkpoint WAL"
                                      )}
                                    </button>
                                  </td>
                                </tr>
                              </>
                            )}
                          </>
                        );
                      })()}
                    </tbody>
                  </table>
                </div>
              </>
            )}

            {cameraGranularRows.length > 0 && (
              <div
                className="nvr-table-region"
                role="region"
                tabIndex={0}
                aria-label={isPt ? "Armazenamento por canal de câmera" : "Per-camera storage"}
                style={{ borderTop: "1px solid var(--border-hairline)" }}
              >
                <table className="nvr-table mono">
                  <thead>
                    <tr>
                      <th scope="col">{isPt ? "Canal" : "Camera Channel"}</th>
                      <th scope="col">{isPt ? "Uso de Disco" : "Disk Usage"}</th>
                      <th scope="col">
                        {isPt
                          ? "Detalhamento (Cont / Mov / Evt / Clips)"
                          : "Breakdown (Cont / Mot / Evt / Clips)"}
                      </th>
                      <th scope="col">{isPt ? "Taxa de Escrita" : "Write Rate"}</th>
                      <th scope="col">{isPt ? "Retenção Est." : "Est. Runway"}</th>
                      <th scope="col" style={{ textAlign: "right" }}>
                        {isPt ? "Limpeza Rápida" : "Quick Cleanup"}
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {cameraGranularRows.map((row) => {
                      const cam = row.camera;
                      const camUpper = cam.toUpperCase();
                      const clipsAndPrev =
                        row.events_media_bytes +
                        row.reviews_media_bytes +
                        row.previews_bytes +
                        row.exports_bytes;
                      const denom = Math.max(
                        1,
                        row.continuous_bytes +
                          row.motion_bytes +
                          row.event_segments_bytes +
                          clipsAndPrev
                      );
                      const pCont = (row.continuous_bytes / denom) * 100;
                      const pMot = (row.motion_bytes / denom) * 100;
                      const pEvt = (row.event_segments_bytes / denom) * 100;
                      const pClips = (clipsAndPrev / denom) * 100;
                      const actualSpan = formatRetentionSpan(row.oldest_segment_ts);
                      const rowAgeBadge =
                        cleanupOlderDays > 0 ? ` (> ${cleanupOlderDays}d)` : "";
                      const evBtnKey = `cam:${cam}:events`;
                      const hasExcess = row.retained_past_continuous_bytes > 0;
                      const mp4BtnKey = hasExcess
                        ? `cam:${cam}:excess`
                        : `cam:${cam}:recordings`;
                      const mp4Target: StorageCleanupTarget = hasExcess
                        ? "excess_recordings"
                        : "recordings";
                      const evArmed = isKeyArmed(evBtnKey);
                      const evBusy = cleanupBusyKey === evBtnKey;
                      const mp4Armed = isKeyArmed(mp4BtnKey);
                      const mp4Busy = cleanupBusyKey === mp4BtnKey;
                      const mp4IdleLabel = hasExcess
                        ? `${isPt ? "Podar > Cont." : "Prune > Cont."}${rowAgeBadge}`
                        : cleanupOlderDays > 0
                        ? `${isPt ? "Limpar MP4" : "Clear MP4"}${rowAgeBadge}`
                        : isPt
                        ? "Limpar Tudo MP4"
                        : "Clear All MP4";

                      return (
                        <tr key={cam}>
                          <td>
                            <div
                              style={{
                                display: "flex",
                                alignItems: "center",
                                gap: 6,
                                flexWrap: "wrap",
                              }}
                            >
                              <span style={{ fontWeight: 700, textTransform: "uppercase" }}>
                                {cam}
                              </span>
                              {!row.configured && (
                                <span
                                  className="nvr-badge mono"
                                  title={
                                    isPt
                                      ? "Câmera ausente no config.yml atual, mas possui arquivos antigos no disco"
                                      : "Camera no longer in config.yml, but still holds historical data on disk"
                                  }
                                >
                                  {isPt ? "LEGADO" : "ORPHAN"}
                                </span>
                              )}
                            </div>
                            {row.events_count > 0 && (
                              <div
                                style={{
                                  fontSize: "var(--text-xs)",
                                  color: "var(--text-muted)",
                                }}
                              >
                                {row.events_count} {isPt ? "eventos" : "events"}
                                {row.pinned_events_count > 0
                                  ? ` (${row.pinned_events_count} ★)`
                                  : ""}
                              </div>
                            )}
                          </td>
                          <td>
                            <div style={{ fontWeight: 700 }}>
                              {formatStorageBytes(row.total_bytes)}
                            </div>
                            {actualSpan && (
                              <div
                                style={{
                                  fontSize: "var(--text-xs)",
                                  color: "var(--text-muted)",
                                }}
                              >
                                {isPt ? `${actualSpan} no disco` : `${actualSpan} on disk`}
                              </div>
                            )}
                          </td>
                          <td>
                            {row.total_bytes > 0 && (
                              <div className="nvr-storage-mini-bar" aria-hidden="true">
                                {pCont > 0 && (
                                  <div
                                    style={{
                                      width: `${pCont}%`,
                                      background: "var(--signal-live)",
                                    }}
                                  />
                                )}
                                {pMot > 0 && (
                                  <div
                                    style={{
                                      width: `${pMot}%`,
                                      background: "var(--signal-motion)",
                                    }}
                                  />
                                )}
                                {pEvt > 0 && (
                                  <div
                                    style={{
                                      width: `${pEvt}%`,
                                      background: "var(--signal-alert)",
                                    }}
                                  />
                                )}
                                {pClips > 0 && (
                                  <div
                                    style={{
                                      width: `${pClips}%`,
                                      background: "var(--signal-accent)",
                                    }}
                                  />
                                )}
                              </div>
                            )}
                            <div
                              style={{
                                fontSize: "var(--text-xs)",
                                color: "var(--text-secondary)",
                              }}
                            >
                              Cont {formatStorageShortBytes(row.continuous_bytes)} ·{" "}
                              {isPt ? "Mov" : "Mot"} {formatStorageShortBytes(row.motion_bytes)} ·
                              Evt {formatStorageShortBytes(row.event_segments_bytes)}
                              {clipsAndPrev > 0
                                ? ` · Clips ${formatStorageShortBytes(clipsAndPrev)}`
                                : ""}
                            </div>
                            {hasExcess && (
                              <div
                                style={{
                                  fontSize: "var(--text-xs)",
                                  color: "var(--signal-motion-text)",
                                }}
                              >
                                {formatStorageShortBytes(row.retained_past_continuous_bytes)}{" "}
                                {isPt
                                  ? `acima de ${row.continuous_days}d cont.`
                                  : `past ${row.continuous_days}d cont.`}
                              </div>
                            )}
                          </td>
                          <td>
                            {Math.max(0, toFiniteNumber(row.bandwidth_mb_hr, 0)).toFixed(1)} MB/h
                          </td>
                          <td style={{ color: "var(--signal-live-text)" }}>
                            <div>
                              {formatCameraTelemetryValue(
                                row.estimated_retention_days ??
                                  stats?.service?.estimated_retention_days,
                                1,
                                "d"
                              )}
                            </div>
                            {storageBreakdown && (
                              <div
                                style={{
                                  fontSize: "var(--text-xs)",
                                  color: "var(--text-muted)",
                                }}
                              >
                                {row.continuous_days}d / {row.event_retention_days}d
                              </div>
                            )}
                          </td>
                          <td style={{ textAlign: "right" }}>
                            <div
                              style={{
                                display: "inline-flex",
                                alignItems: "center",
                                gap: 5,
                                justifyContent: "flex-end",
                              }}
                            >
                              <button
                                type="button"
                                className={`nvr-btn xs mono nvr-cleanup-btn ${
                                  evArmed ? "danger-armed" : "danger-subtle"
                                }`}
                                aria-busy={evBusy}
                                aria-label={
                                  evBusy
                                    ? `${isPt ? "Limpando..." : "Cleaning..."} [${camUpper}]`
                                    : evArmed
                                    ? `${
                                        isPt ? "Confirmar?" : "Confirm Clear?"
                                      } — ${
                                        isPt ? "Limpar Eventos" : "Clear Events"
                                      }${rowAgeBadge} [${camUpper}]`
                                    : `${
                                        isPt ? "Limpar Eventos" : "Clear Events"
                                      }${rowAgeBadge} [${camUpper}]`
                                }
                                title={
                                  isPt
                                    ? `Limpar eventos da câmera ${camUpper}${rowAgeBadge}`
                                    : `Clear events for camera ${camUpper}${rowAgeBadge}`
                                }
                                disabled={cleanupBusyKey !== null}
                                onClick={() => {
                                  triggerTwoStepCleanup(
                                    evBtnKey,
                                    "events",
                                    `Limpar Eventos${rowAgeBadge} [${camUpper}]`,
                                    `Clear Events${rowAgeBadge} [${camUpper}]`,
                                    cam
                                  );
                                }}
                              >
                                {renderStableCleanupLabel(
                                  `${isPt ? "Limpar Eventos" : "Clear Events"}${rowAgeBadge}`,
                                  evBusy
                                    ? isPt
                                      ? "Limpando..."
                                      : "Cleaning..."
                                    : evArmed
                                    ? isPt
                                      ? "Confirmar?"
                                      : "Confirm Clear?"
                                    : `${isPt ? "Limpar Eventos" : "Clear Events"}${rowAgeBadge}`
                                )}
                              </button>
                              <button
                                type="button"
                                className={`nvr-btn xs mono nvr-cleanup-btn ${
                                  mp4Armed ? "danger-armed" : "danger-subtle"
                                }`}
                                aria-busy={mp4Busy}
                                aria-label={
                                  mp4Busy
                                    ? `${isPt ? "Limpando..." : "Cleaning..."} ${mp4IdleLabel} [${camUpper}]`
                                    : mp4Armed
                                    ? `${
                                        isPt ? "Confirmar?" : "Confirm Clear?"
                                      } — ${mp4IdleLabel} [${camUpper}]`
                                    : `${mp4IdleLabel} [${camUpper}]`
                                }
                                title={
                                  hasExcess
                                    ? isPt
                                      ? `Podar gravações MP4 de ${camUpper} acima de ${row.continuous_days}d`
                                      : `Prune ${camUpper} MP4 segments older than ${row.continuous_days}d`
                                    : isPt
                                    ? `Limpar gravações MP4 de ${camUpper}${rowAgeBadge || " (tudo)"}`
                                    : `Clear ${camUpper} MP4 recordings${rowAgeBadge || " (all)"}`
                                }
                                disabled={cleanupBusyKey !== null}
                                onClick={() => {
                                  triggerTwoStepCleanup(
                                    mp4BtnKey,
                                    mp4Target,
                                    `${
                                      hasExcess
                                        ? `Podar > Cont.${rowAgeBadge}`
                                        : cleanupOlderDays > 0
                                        ? `Limpar MP4${rowAgeBadge}`
                                        : "Limpar Tudo MP4"
                                    } [${camUpper}]`,
                                    `${
                                      hasExcess
                                        ? `Prune > Cont.${rowAgeBadge}`
                                        : cleanupOlderDays > 0
                                        ? `Clear MP4${rowAgeBadge}`
                                        : "Clear All MP4"
                                    } [${camUpper}]`,
                                    cam
                                  );
                                }}
                              >
                                {renderStableCleanupLabel(
                                  mp4IdleLabel,
                                  mp4Busy
                                    ? isPt
                                      ? "Limpando..."
                                      : "Cleaning..."
                                    : mp4Armed
                                    ? isPt
                                      ? "Confirmar?"
                                      : "Confirm Clear?"
                                    : mp4IdleLabel
                                )}
                              </button>
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        </div>
      </div>

      {/* Right Real-Time Log Stream & Heuristic Alerts Column */}
      <div className="nvr-panel">
        <div className="nvr-panel-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{t.system.liveLogs}</span>
            <button
              type="button"
              className={`nvr-btn mono ${rightMode === "alerts" ? "active" : ""}`}
              style={{
                height: 22,
                padding: "0 7px",
                fontSize: "var(--text-xs)",
                display: "inline-flex",
                alignItems: "center",
                gap: 4,
                borderColor:
                  critCount > 0
                    ? "var(--signal-alert)"
                    : warnCount > 0
                    ? "var(--signal-motion)"
                    : undefined,
                color:
                  rightMode === "alerts"
                    ? undefined
                    : critCount > 0
                    ? "var(--signal-alert-text)"
                    : warnCount > 0
                    ? "var(--signal-motion-text)"
                    : undefined,
              }}
              onClick={() => setRightMode("alerts")}
              title={
                isPt
                  ? "Alertas heurísticos de configuração, streams, retenção e otimização"
                  : "Heuristic configuration, stream, retention, and optimization alerts"
              }
            >
              <AlertTriangle size={11} />
              <span>
                {isPt ? "ALERTAS" : "ALERTS"} ({heuristicAlerts.length})
              </span>
            </button>
            {(["ninjanvr", "go2rtc", "nginx", "cameras"] as const).map((svc) => (
              <button
                key={svc}
                type="button"
                className={`nvr-btn mono nvr-system-chip-btn ${
                  rightMode === "logs" && logService === svc ? "active" : ""
                }`}
                onClick={() => {
                  setRightMode("logs");
                  setLogService(svc);
                  if (svc === "cameras") {
                    for (const cam of cameraNames) {
                      void fetch(`/api/${encodeURIComponent(cam)}/logs?timeout_ms=2500`, {
                        credentials: "include",
                      }).catch(() => {});
                    }
                  }
                }}
              >
                {svc.toUpperCase()}
              </button>
            ))}
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <input
              className="nvr-input nvr-system-filter-input"
              style={{ width: 130 }}
              aria-label="Filter service logs"
              placeholder={
                rightMode === "alerts"
                  ? isPt
                    ? "Filtrar alertas..."
                    : "Filter alerts..."
                  : isPt
                  ? "Filtrar logs..."
                  : "Filter logs..."
              }
              value={logSearch}
              onChange={(e) => setLogSearch(e.target.value)}
            />
            {rightMode === "logs" && (
              <button
                type="button"
                className={`nvr-btn ${logPaused ? "active" : ""}`}
                style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                onClick={() => setLogPaused((p) => !p)}
              >
                {logPaused ? <Play size={11} /> : <Pause size={11} />}
                {logPaused ? t.system.resumeStream : t.system.pauseStream}
              </button>
            )}
            <button
              type="button"
              className="nvr-btn"
              style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
              aria-label="Copy service logs to clipboard"
              title={t.system.copyLogs}
              onClick={() => {
                if (rightMode === "alerts") {
                  const text = filteredAlerts
                    .map(
                      (a) =>
                        `[${a.severity.toUpperCase()}][${a.category.toUpperCase()}][${a.target}] ${a.title}\n  Detail: ${a.detail}\n  Recommendation: ${a.recommendation}`
                    )
                    .join("\n\n");
                  void safeCopyText(
                    text,
                    `Copied ${filteredAlerts.length} heuristic alert(s) to clipboard`
                  );
                  return;
                }
                const visibleLogs = logLines.filter((l) =>
                  logSearch ? l.toLowerCase().includes(logSearch.toLowerCase()) : true
                );
                void safeCopyText(
                  visibleLogs.join("\n"),
                  `Copied ${visibleLogs.length} log line(s) to clipboard`
                );
              }}
            >
              <Copy size={11} />
            </button>
          </div>
        </div>
        {rightMode === "alerts" ? (
          <div
            className="nvr-scroll-y"
            style={{
              background: "var(--bg-deep)",
              padding: "10px",
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <div
              className="mono"
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                flexWrap: "wrap",
                gap: 8,
                rowGap: 6,
                padding: "6px 8px",
                background: "var(--bg-canvas)",
                border: "1px solid var(--border-hairline)",
                borderRadius: 3,
                fontSize: "var(--text-xs)",
              }}
            >
              <span style={{ color: "var(--text-secondary)" }}>
                {isPt
                  ? "DIAGNÓSTICO HEURÍSTICO DE PIPELINE & CONFIGURAÇÃO"
                  : "HEURISTIC PIPELINE & CONFIGURATION DIAGNOSTICS"}
              </span>
              <div style={{ display: "flex", flexWrap: "wrap", gap: 6 }}>
                <span className="nvr-badge alert mono">
                  {critCount} {isPt ? "CRÍTICOS" : "CRITICAL"}
                </span>
                <span className="nvr-badge motion mono">
                  {warnCount} {isPt ? "AVISOS" : "WARNING"}
                </span>
                <span className="nvr-badge mono">
                  {optCount} {isPt ? "OTIMIZAÇÃO" : "OPTIMIZATION"}
                </span>
              </div>
            </div>

            {filteredAlerts.length === 0 ? (
              <div
                className="mono"
                style={{
                  padding: 16,
                  display: "flex",
                  alignItems: "center",
                  gap: 8,
                  color: "var(--signal-live-text)",
                  fontSize: "var(--text-xs)",
                  background: "var(--bg-canvas)",
                  border: "1px solid var(--border-hairline)",
                  borderRadius: 3,
                }}
              >
                <CheckCircle2 size={14} />
                <span>
                  {isPt
                    ? "Nenhum alerta heurístico ou gargalo de configuração detectado."
                    : "No heuristic alerts or configuration bottlenecks detected."}
                </span>
              </div>
            ) : (
              filteredAlerts.map((alert) => {
                const isCrit = alert.severity === "critical";
                const isWarn = alert.severity === "warning";
                const borderColor = isCrit
                  ? "var(--signal-alert)"
                  : isWarn
                  ? "var(--signal-motion)"
                  : "var(--border-strong)";
                const badgeClass = isCrit
                  ? "nvr-badge alert mono"
                  : isWarn
                  ? "nvr-badge motion mono"
                  : "nvr-badge mono";

                return (
                  <div
                    key={alert.id}
                    style={{
                      background: "var(--bg-canvas)",
                      border: `1px solid ${borderColor}`,
                      borderRadius: 3,
                      padding: "9px 10px",
                      display: "flex",
                      flexDirection: "column",
                      gap: 5,
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: 6,
                        flexWrap: "wrap",
                      }}
                    >
                      <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                        <span className={badgeClass}>
                          {alert.severity.toUpperCase()}
                        </span>
                        <span className="nvr-badge mono">
                          {alert.category.toUpperCase()} · {alert.target}
                        </span>
                      </div>
                      <span
                        className="mono"
                        style={{
                          fontSize: "var(--text-xs)",
                          color: "var(--text-muted)",
                        }}
                      >
                        {alert.id}
                      </span>
                    </div>
                    <div
                      style={{
                        fontSize: "var(--text-sm)",
                        fontWeight: 700,
                        color: "var(--text-primary)",
                      }}
                    >
                      {alert.title}
                    </div>
                    <div
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        lineHeight: 1.45,
                      }}
                    >
                      {alert.detail}
                    </div>
                    <div
                      className="mono"
                      style={{
                        marginTop: 2,
                        padding: "5px 7px",
                        background: "var(--bg-deep)",
                        border: "1px solid var(--border-hairline)",
                        borderRadius: 2,
                        fontSize: "var(--text-xs)",
                        color: "var(--signal-accent-text)",
                        lineHeight: 1.4,
                      }}
                    >
                      <strong>{isPt ? "RECOMENDAÇÃO: " : "RECOMMENDATION: "}</strong>
                      {alert.recommendation}
                    </div>
                  </div>
                );
              })
            )}
          </div>
        ) : (
          <div
            className="nvr-scroll-y mono"
            style={{
              background: "var(--bg-deep)",
              padding: "8px 10px",
              fontSize: "var(--text-xs)",
              lineHeight: 1.5,
              userSelect: "text",
            }}
          >
            {logLines
              .filter((l) =>
                logSearch ? l.toLowerCase().includes(logSearch.toLowerCase()) : true
              )
              .map((line, idx) => (
                <div
                  key={idx}
                  style={{
                    padding: "2px 0",
                    borderBottom: "1px solid var(--border-divider)",
                    color:
                      line.includes("ERR") || line.includes("WARN")
                        ? "var(--signal-motion-text)"
                        : "var(--text-primary)",
                    wordBreak: "break-all",
                  }}
                >
                  {line}
                </div>
              ))}
          </div>
        )}
      </div>
    </div>
  );
};
