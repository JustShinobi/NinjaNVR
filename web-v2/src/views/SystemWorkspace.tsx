import React from "react";
import { AlertTriangle, CheckCircle2, Copy, Pause, Play } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import { HeuristicAlertItem, NinjaConfigResponse, NinjaStatsResponse } from "../types/nvr";
import { formatUptime } from "../utils/format";
import { CanvasSparkline } from "../components/VideoPlayers";

function buildClientFallbackHeuristics(
  config: NinjaConfigResponse | null,
  stats: NinjaStatsResponse | null
): HeuristicAlertItem[] {
  if (stats?.service?.heuristics && stats.service.heuristics.length > 0) {
    return stats.service.heuristics;
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
    const clean = raw.split("#")[0].trim();
    if (
      (clean.startsWith("rtsp://") || clean.startsWith("rtsps://")) &&
      !clean.includes("127.0.0.1") &&
      !clean.includes("localhost")
    ) {
      if (!urlMap[clean]) urlMap[clean] = [];
      urlMap[clean].push(sName);
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
    const w = cCfg.detect?.width || 1280;
    const h = cCfg.detect?.height || 720;
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
    const ffVal = parseFloat(stats?.cameras?.[cName]?.ffmpeg_cpu || "0");
    if (ffVal > 0) {
      ffCpus.push([cName, ffVal]);
    }
  }
  if (ffCpus.length >= 2) {
    const minFf = Math.max(0.3, Math.min(...ffCpus.map(([, v]) => v)));
    for (const [cName, ffVal] of ffCpus) {
      if (ffVal >= 1.4 && ffVal >= minFf * 1.8) {
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
  const estDays = stats?.service?.estimated_retention_days ?? 14;
  if (recSt && recSt.total > 0) {
    const usedPct = (recSt.used / recSt.total) * 100;
    const freeGb = recSt.free / 1024;
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
    .map(([, c]) => c.record?.continuous?.days ?? 14);
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
        "Standardize snapshots.quality: 100 and record.preview.quality: very_high across all cameras.",
    });
  }

  return out;
}

interface SystemWorkspaceProps {
  cameraNames: string[];
  logService: "ninjanvr" | "go2rtc" | "nginx";
  setLogService: (svc: "ninjanvr" | "go2rtc" | "nginx") => void;
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

  const sysCpu = stats?.cpu_usages?.["ninjanvr.full_system"]?.cpu || "6.5";
  const sysMem = stats?.cpu_usages?.["ninjanvr.full_system"]?.mem || "26.0";
  const inferMs = stats?.detectors?.detector01?.inference_speed?.toFixed(1) || "7.0";
  const [aiBriefText, setAiBriefText] = React.useState<string | null>(null);
  const [aiBriefBusy, setAiBriefBusy] = React.useState(false);
  const [rightMode, setRightMode] = React.useState<"alerts" | "logs">("alerts");

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
            HARDWARE & PIPELINE TELEMETRY (UPTIME {formatUptime(stats?.service?.uptime || 0)})
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
                    navigator.clipboard?.writeText(url);
                    notify(`Copied MCP JSON-RPC endpoint: ${url}`);
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
                    setAiBriefBusy(true);
                    try {
                      const res = await fetch("/api/ai/context?hours=24", {
                        credentials: "include",
                      });
                      if (res.ok) {
                        const data = await res.json();
                        setAiBriefText(
                          data.markdown_brief || JSON.stringify(data, null, 2)
                        );
                        notify("Generated 24h AI Situation Brief");
                      } else {
                        notify(`Failed to fetch AI context (${res.status})`);
                      }
                    } catch {
                      notify("Failed to reach /api/ai/context");
                    } finally {
                      setAiBriefBusy(false);
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
                <span style={{ color: "var(--text-secondary)" }}>OPENVINO</span>
                <span
                  className="mono"
                  style={{ color: "var(--signal-accent-text)", fontWeight: 700 }}
                >
                  {inferMs} ms
                </span>
              </div>
              <CanvasSparkline
                values={statsHistory.map(
                  (s) => s.detectors?.detector01?.inference_speed || 7.0
                )}
                color="#0ea5e9"
                maxVal={20}
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
                  {sysCpu}%
                </span>
              </div>
              <CanvasSparkline
                values={statsHistory.map((s) =>
                  parseFloat(s.cpu_usages?.["ninjanvr.full_system"]?.cpu || "6.5")
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
                  {sysMem}%
                </span>
              </div>
              <CanvasSparkline
                values={statsHistory.map((s) =>
                  parseFloat(s.cpu_usages?.["ninjanvr.full_system"]?.mem || "26.0")
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
            <div className="nvr-panel-header">
              <span>{t.system.cameraHealth}</span>
            </div>
            <table className="nvr-table mono">
              <thead>
                <tr>
                  <th>Channel</th>
                  <th>Cam FPS</th>
                  <th>Proc FPS</th>
                  <th>AI Detect FPS</th>
                  <th>FFmpeg CPU</th>
                  <th>Capture CPU</th>
                </tr>
              </thead>
              <tbody>
                {cameraNames.map((cam) => {
                  const cs = stats?.cameras?.[cam];
                  return (
                    <tr key={cam}>
                      <td style={{ fontWeight: 700, textTransform: "uppercase" }}>{cam}</td>
                      <td>{cs?.camera_fps?.toFixed(1) || "5.0"}</td>
                      <td>{cs?.process_fps?.toFixed(1) || "5.0"}</td>
                      <td style={{ color: "var(--signal-accent-text)" }}>
                        {cs?.detection_fps?.toFixed(1) || "0.0"}
                      </td>
                      <td>{cs?.ffmpeg_cpu || "1.2"}%</td>
                      <td>{cs?.capture_cpu || "1.0"}%</td>
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
                onClick={async () => {
                  try {
                    const res = await fetch("/api/recordings/sync", {
                      method: "POST",
                      credentials: "include",
                    });
                    if (res.ok) {
                      const data = await res.json();
                      notify(
                        `Synced Storage DB: removed ${data.removed_orphans ?? 0} orphan segment(s) in ${data.duration_ms ?? 0}ms`
                      );
                    } else {
                      notify(`Storage DB sync failed (${res.status})`);
                    }
                  } catch {
                    notify("Storage DB sync request failed");
                  }
                }}
              >
                Sync Storage DB
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
                  const pct = Math.min(
                    100,
                    Math.round((st.used / Math.max(1, st.total)) * 100)
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
                      <td>{(st.used / 1024).toFixed(1)} GB</td>
                      <td>{(st.total / 1024).toFixed(1)} GB</td>
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
                        <td>{((cs.usage || 0) / 1024).toFixed(2)} GB</td>
                        <td>{(cs.bandwidth || 0).toFixed(1)} MB/h</td>
                        <td style={{ color: "var(--signal-live-text)" }}>
                          {(cs.estimated_retention_days ?? stats.service.estimated_retention_days ?? 14).toFixed(1)}d
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
            {(["ninjanvr", "go2rtc", "nginx"] as const).map((svc) => (
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
                  navigator.clipboard?.writeText(text);
                  notify(`Copied ${filteredAlerts.length} heuristic alert(s) to clipboard`);
                  return;
                }
                const visibleLogs = logLines.filter((l) =>
                  logSearch ? l.toLowerCase().includes(logSearch.toLowerCase()) : true
                );
                navigator.clipboard?.writeText(visibleLogs.join("\n"));
                notify(`Copied ${visibleLogs.length} log line(s) to clipboard`);
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
