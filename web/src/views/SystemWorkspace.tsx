import React from "react";
import { formatMeasured, parseMeasured, resolvePrimaryInferenceMs, sparklineCeiling } from "../utils/detectorTelemetry";
import { AlertTriangle, CheckCircle2, Copy, Pause, Play } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import {
  CameraRuntimeStats,
  HeuristicAlertItem,
  NinjaConfigResponse,
  NinjaStatsResponse,
} from "../types/nvr";
import { formatUptime } from "../utils/format";
import { CanvasSparkline } from "../components/VideoPlayers";

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
  if (daysList.length >= 2) {
    const minD = Math.min(...daysList);
    const maxD = Math.max(...daysList);
    if (maxD - minD >= 3 || maxD > estDays + 1) {
      out.push({
        id: "retention_runway_asymmetry",
        severity: "warning",
        category: "storage",
        target: "record.continuous.days",
        title: "Retention policy exceeds disk runway & camera asymmetry",
        detail: `Configured continuous retention varies from ${minD}d to ${maxD}d across cameras, while free storage only supports ~${estDays.toFixed(
          1
        )}d at current write rate.`,
        recommendation:
          "Standardize cameras.*.record.continuous.days (e.g. 3d across all cameras) or enable continuous_substream / mode_tiering.",
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
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const config = useNvrStore((s) => s.config);
  const stats = useNvrStore((s) => s.stats);
  const statsHistory = useNvrStore((s) => s.statsHistory);

  const sysCpu = formatMeasured(parseMeasured(stats?.cpu_usages?.["ninjanvr.full_system"]?.cpu), 1, "%");
  const sysMem = formatMeasured(parseMeasured(stats?.cpu_usages?.["ninjanvr.full_system"]?.mem), 1, "%");
  const inferMs = formatMeasured(resolvePrimaryInferenceMs(stats), 1, " ms");
  const inferHistory = statsHistory.map((s) => resolvePrimaryInferenceMs(s));
  const [aiBriefText, setAiBriefText] = React.useState<string | null>(null);
  const [aiBriefBusy, setAiBriefBusy] = React.useState(false);
  const [syncStorageBusy, setSyncStorageBusy] = React.useState(false);
  const isMountedRef = React.useRef(true);

  React.useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

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
  const [rightMode, setRightMode] = React.useState<"alerts" | "logs">("alerts");
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
    <div className="nvr-workbench" style={{ gridTemplateColumns: "1.1fr 1fr" }}>
      {/* Left Telemetry Column */}
      <div className="nvr-panel">
        <div className="nvr-panel-header">
          <span>
            HARDWARE & PIPELINE TELEMETRY (UPTIME{" "}
            {formatUptime(Math.max(0, toFiniteNumber(stats?.service?.uptime, 0)))})
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
              style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}
            >
              <span>AI SITUATION BRIEF & MCP SERVER (/api/ai/context · /api/mcp)</span>
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
                  Copy MCP URL
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
                        notify("Generated 24h AI Situation Brief");
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
                  {aiBriefBusy ? "Generating..." : "Generate 24h AI Brief"}
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
              gridTemplateColumns: "repeat(3, 1fr)",
              gap: 10,
            }}
          >
            <div
              style={{
                background: "var(--bg-canvas)",
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
                    ? `INFERÊNCIA · ${stats.primary_detector.device}`
                    : "INFERÊNCIA"}
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
                color="#0ea5e9"
                maxVal={sparklineCeiling(inferHistory)}
              />
            </div>

            <div
              style={{
                background: "var(--bg-canvas)",
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
                <span style={{ color: "var(--text-secondary)" }}>SYSTEM CPU</span>
                <span
                  className="mono"
                  style={{ color: "var(--signal-live-text)", fontWeight: 700 }}
                >
                  {sysCpu}
                </span>
              </div>
              <CanvasSparkline
                values={statsHistory.map((s) =>
                  toFiniteNumber(s.cpu_usages?.["ninjanvr.full_system"]?.cpu, 6.5)
                )}
                color="#10b981"
                maxVal={100}
              />
            </div>

            <div
              style={{
                background: "var(--bg-canvas)",
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
                <span style={{ color: "var(--text-secondary)" }}>MEMORY RSS</span>
                <span
                  className="mono"
                  style={{ color: "var(--signal-motion-text)", fontWeight: 700 }}
                >
                  {sysMem}
                </span>
              </div>
              <CanvasSparkline
                values={statsHistory.map((s) =>
                  toFiniteNumber(s.cpu_usages?.["ninjanvr.full_system"]?.mem, 26.0)
                )}
                color="#f59e0b"
                maxVal={100}
              />
            </div>
          </div>

          {/* Per-Camera Pipeline Table */}
          <div
            style={{
              background: "var(--bg-canvas)",
              border: "1px solid var(--border-hairline)",
              borderRadius: 3,
            }}
          >
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
                  className="nvr-input mono"
                  style={{ height: 22, width: 136, fontSize: "var(--text-xs)" }}
                  placeholder="Search camera..."
                  aria-label="Search camera health"
                  value={healthSearch}
                  onChange={(e) => setHealthSearch(e.target.value)}
                />
                {(
                  [
                    ["all", "All"],
                    ["offline", "Offline / 0 FPS"],
                    ["active_ai", "Active AI"],
                  ] as Array<[HealthStatusFilter, string]>
                ).map(([flt, label]) => (
                  <button
                    key={flt}
                    type="button"
                    className={`nvr-btn mono ${
                      healthStatusFilter === flt ? "active" : ""
                    }`}
                    style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                    onClick={() => setHealthStatusFilter(flt)}
                  >
                    {label}
                  </button>
                ))}
              </div>
            </div>
            <table className="nvr-table mono">
              <thead>
                <tr>
                  <th
                    style={{ cursor: "pointer" }}
                    onClick={() => toggleHealthSort("channel")}
                  >
                    Channel{" "}
                    {healthSortCol === "channel"
                      ? healthSortDir === "asc"
                        ? "▲"
                        : "▼"
                      : ""}
                  </th>
                  <th
                    style={{ cursor: "pointer" }}
                    onClick={() => toggleHealthSort("camera_fps")}
                  >
                    Cam FPS{" "}
                    {healthSortCol === "camera_fps"
                      ? healthSortDir === "asc"
                        ? "▲"
                        : "▼"
                      : ""}
                  </th>
                  <th>Proc FPS</th>
                  <th
                    style={{ cursor: "pointer" }}
                    onClick={() => toggleHealthSort("detection_fps")}
                  >
                    AI Detect FPS{" "}
                    {healthSortCol === "detection_fps"
                      ? healthSortDir === "asc"
                        ? "▲"
                        : "▼"
                      : ""}
                  </th>
                  <th
                    style={{ cursor: "pointer" }}
                    onClick={() => toggleHealthSort("ffmpeg_cpu")}
                  >
                    FFmpeg CPU{" "}
                    {healthSortCol === "ffmpeg_cpu"
                      ? healthSortDir === "asc"
                        ? "▲"
                        : "▼"
                      : ""}
                  </th>
                  <th>Capture CPU</th>
                </tr>
              </thead>
              <tbody>
                {filteredHealthCameras.map((cam) => {
                  const cs = stats?.cameras?.[cam];
                  return (
                    <tr key={cam}>
                      <td style={{ fontWeight: 700, textTransform: "uppercase" }}>{cam}</td>
                      <td>{toFiniteNumber(cs?.camera_fps, 5.0).toFixed(1)}</td>
                      <td>{toFiniteNumber(cs?.process_fps, 5.0).toFixed(1)}</td>
                      <td style={{ color: "var(--signal-accent-text)" }}>
                        {toFiniteNumber(cs?.detection_fps, 0.0).toFixed(1)}
                      </td>
                      <td>{toFiniteNumber(cs?.ffmpeg_cpu, 1.2).toFixed(1)}%</td>
                      <td>{toFiniteNumber(cs?.capture_cpu, 1.0).toFixed(1)}%</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>

          {/* Storage Volumes */}
          <div
            style={{
              background: "var(--bg-canvas)",
              border: "1px solid var(--border-hairline)",
              borderRadius: 3,
            }}
          >
            <div
              className="nvr-panel-header"
              style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}
            >
              <span>{t.system.storageVolumes}</span>
              <button
                className="nvr-btn mono"
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                title="Reconcile SQLite recordings table against on-disk MP4 files"
                disabled={syncStorageBusy}
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
                        `Synced Storage DB: removed ${data.removed_orphans ?? 0} orphan segment(s) in ${data.duration_ms ?? 0}ms`
                      );
                    } else {
                      notify(`Storage DB sync failed (${res.status})`);
                    }
                  } catch {
                    if (isMountedRef.current) {
                      notify("Storage DB sync request failed");
                    }
                  } finally {
                    if (isMountedRef.current) {
                      setSyncStorageBusy(false);
                    }
                  }
                }}
              >
                {syncStorageBusy ? "Syncing..." : "Sync Storage DB"}
              </button>
            </div>
            <table className="nvr-table mono">
              <thead>
                <tr>
                  <th>Mount Path</th>
                  <th>FS</th>
                  <th>Used</th>
                  <th>Total</th>
                  <th>Utilization</th>
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
                            <span className="nvr-badge mono">COLD ARCHIVE TIER</span>
                          ) : mPath.includes("recordings") ? (
                            <span className="nvr-badge live mono">HOT TIER</span>
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
            {stats?.service?.camera_storage &&
              Object.keys(stats.service.camera_storage).length > 0 && (
                <table className="nvr-table mono" style={{ borderTop: "1px solid var(--border-hairline)" }}>
                  <thead>
                    <tr>
                      <th>Camera Channel</th>
                      <th>Disk Usage</th>
                      <th>Write Rate</th>
                      <th>Est. Runway</th>
                    </tr>
                  </thead>
                  <tbody>
                    {Object.entries(stats.service.camera_storage).map(([cam, cs]) => (
                      <tr key={cam}>
                        <td style={{ fontWeight: 700, textTransform: "uppercase" }}>{cam}</td>
                        <td>{(Math.max(0, toFiniteNumber(cs.usage, 0)) / 1024).toFixed(2)} GB</td>
                        <td>{Math.max(0, toFiniteNumber(cs.bandwidth, 0)).toFixed(1)} MB/h</td>
                        <td style={{ color: "var(--signal-live-text)" }}>
                          {toFiniteNumber(
                            cs.estimated_retention_days ?? stats.service.estimated_retention_days,
                            14
                          ).toFixed(1)}d
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
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
                className={`nvr-btn mono ${
                  rightMode === "logs" && logService === svc ? "active" : ""
                }`}
                style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
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
              className="nvr-input"
              style={{ height: 22, width: 130, fontSize: "var(--text-xs)" }}
              aria-label="Filter service logs"
              placeholder={
                rightMode === "alerts"
                  ? isPt
                    ? "Filtrar alertas..."
                    : "Filter alerts..."
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
                gap: 8,
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
              <div style={{ display: "flex", gap: 6 }}>
                <span className="nvr-badge alert mono">{critCount} CRITICAL</span>
                <span className="nvr-badge motion mono">{warnCount} WARNING</span>
                <span className="nvr-badge mono">{optCount} OPTIMIZATION</span>
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
                    borderBottom: "1px solid rgba(255,255,255,0.03)",
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
