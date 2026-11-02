import React from "react";
import { Copy, Pause, Play } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import { formatUptime } from "../utils/format";
import { CanvasSparkline } from "../components/VideoPlayers";

interface SystemWorkspaceProps {
  cameraNames: string[];
  logService: "frigate" | "go2rtc" | "nginx";
  setLogService: (svc: "frigate" | "go2rtc" | "nginx") => void;
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
  const { t } = useI18n();
  const stats = useNvrStore((s) => s.stats);
  const statsHistory = useNvrStore((s) => s.statsHistory);

  const sysCpu = stats?.cpu_usages?.["frigate.full_system"]?.cpu || "6.5";
  const sysMem = stats?.cpu_usages?.["frigate.full_system"]?.mem || "26.0";
  const inferMs = stats?.detectors?.detector01?.inference_speed?.toFixed(1) || "7.0";
  const [aiBriefText, setAiBriefText] = React.useState<string | null>(null);
  const [aiBriefBusy, setAiBriefBusy] = React.useState(false);

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
                  parseFloat(s.cpu_usages?.["frigate.full_system"]?.cpu || "6.5")
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
                  parseFloat(s.cpu_usages?.["frigate.full_system"]?.mem || "26.0")
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

      {/* Right Real-Time Log Stream Column */}
      <div className="nvr-panel">
        <div className="nvr-panel-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <span>{t.system.liveLogs}</span>
            {(["frigate", "go2rtc", "nginx"] as const).map((svc) => (
              <button
                key={svc}
                className={`nvr-btn mono ${logService === svc ? "active" : ""}`}
                style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                onClick={() => setLogService(svc)}
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
              placeholder="Filter logs..."
              value={logSearch}
              onChange={(e) => setLogSearch(e.target.value)}
            />
            <button
              className={`nvr-btn ${logPaused ? "active" : ""}`}
              style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
              onClick={() => setLogPaused((p) => !p)}
            >
              {logPaused ? <Play size={11} /> : <Pause size={11} />}
              {logPaused ? t.system.resumeStream : t.system.pauseStream}
            </button>
            <button
              className="nvr-btn"
              style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
              aria-label="Copy service logs to clipboard"
              title={t.system.copyLogs}
              onClick={() => {
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
      </div>
    </div>
  );
};
