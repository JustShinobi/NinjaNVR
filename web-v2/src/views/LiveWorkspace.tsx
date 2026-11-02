import React, { useState } from "react";
import { Camera, Check, Eye, Play } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import { RecordingSegment, ReviewItem } from "../types/nvr";
import { HudPosition, formatTs } from "../utils/format";
import { LiveMsePlayer, VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";

interface LiveWorkspaceProps {
  cameraNames: string[];
  reviews: ReviewItem[];
  unreviewedAlerts: ReviewItem[];
  focusSegments: RecordingSegment[];
  handleMarkReviews: (ids: string[], reviewed?: boolean) => Promise<void>;
  handleQuickExport: (
    camera: string,
    startSec: number,
    endSec: number,
    label?: string
  ) => Promise<{ exportId?: string; videoUrl?: string } | void>;
}

export const LiveWorkspace: React.FC<LiveWorkspaceProps> = ({
  cameraNames,
  reviews,
  unreviewedAlerts,
  focusSegments,
  handleMarkReviews,
  handleQuickExport,
}) => {
  const { t } = useI18n();
  const [hoveredAlertId, setHoveredAlertId] = useState<string | null>(null);

  const liveLayout = useNvrStore((s) => s.liveLayout);
  const setLiveLayout = useNvrStore((s) => s.setLiveLayout);
  const selectedCamera = useNvrStore((s) => s.selectedCamera);
  const setSelectedCamera = useNvrStore((s) => s.setSelectedCamera);
  const hudPosition = useNvrStore((s) => s.hudPosition);
  const setHudPosition = useNvrStore((s) => s.setHudPosition);
  const useMainStream = useNvrStore((s) => s.useMainStream);
  const setUseMainStream = useNvrStore((s) => s.setUseMainStream);
  const showVectors = useNvrStore((s) => s.showVectors);
  const setShowVectors = useNvrStore((s) => s.setShowVectors);
  const timeShiftTs = useNvrStore((s) => s.timeShiftTs);
  const setTimeShiftTs = useNvrStore((s) => s.setTimeShiftTs);
  const playbackSpeed = useNvrStore((s) => s.playbackSpeed);
  const config = useNvrStore((s) => s.config);
  const stats = useNvrStore((s) => s.stats);
  const cameraActivity = useNvrStore((s) => s.cameraActivity);

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <button
            className={`nvr-btn ${liveLayout === "2x2" ? "active" : ""}`}
            onClick={() => {
              setLiveLayout("2x2");
              setTimeShiftTs(null);
            }}
          >
            {t.live.grid2x2}
          </button>
          <button
            className={`nvr-btn ${liveLayout === "focus" ? "active" : ""}`}
            onClick={() => setLiveLayout("focus")}
          >
            {t.live.focusStage}
          </button>
          <button
            className={`nvr-btn ${liveLayout === "single" ? "active" : ""}`}
            onClick={() => setLiveLayout("single")}
          >
            {t.live.singleFull}
          </button>
          {liveLayout !== "2x2" && (
            <>
              <span className="nvr-divider" aria-hidden="true" />
              {cameraNames.map((cam) => (
                <button
                  key={cam}
                  className={`nvr-btn mono ${selectedCamera === cam ? "active" : ""}`}
                  onClick={() => setSelectedCamera(cam)}
                  style={{ textTransform: "uppercase" }}
                >
                  {cam}
                </button>
              ))}
            </>
          )}
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <select
            className="nvr-select mono"
            aria-label="Camera HUD badge position"
            style={{ height: 28, fontSize: "var(--text-xs)" }}
            value={hudPosition}
            onChange={(e) => {
              setHudPosition(e.target.value as HudPosition);
            }}
            title="Configure where camera info badges appear so they never cover camera OSD text"
          >
            <option value="bottom-right">HUD: Bottom-Right</option>
            <option value="top-right">HUD: Top-Right</option>
            <option value="top-left">HUD: Top-Left</option>
            <option value="bottom-left">HUD: Bottom-Left</option>
            <option value="hidden">HUD: Hidden</option>
          </select>
          <button
            className={`nvr-btn ${useMainStream ? "active" : ""}`}
            onClick={() => setUseMainStream((v) => !v)}
            title="Switch between 1080p Main Stream and 720p Substream"
          >
            {useMainStream ? t.live.hdStream : t.live.subStream}
          </button>
          <button
            className={`nvr-btn ${showVectors ? "active" : ""}`}
            onClick={() => setShowVectors((v) => !v)}
            title="Overlay configured Motion Masks and Detection Zones"
          >
            <Eye size={13} />
            {t.live.zonesOverlay}
          </button>
          {unreviewedAlerts.length > 0 && (
            <button
              className="nvr-btn success"
              onClick={() =>
                handleMarkReviews(
                  unreviewedAlerts.map((r) => r.id),
                  true
                )
              }
            >
              <Check size={13} />
              {t.live.markAllReviewed} ({unreviewedAlerts.length})
            </button>
          )}
        </div>
      </div>

      {/* Zero-Decoder-Bloat Unreviewed Alert Ribbon (Video mounts ONLY on pointer hover!) */}
      {unreviewedAlerts.length > 0 && (
        <div className="nvr-alert-ribbon" aria-label={t.live.unreviewedAlerts}>
          <div
            style={{
              minWidth: 118,
              display: "flex",
              flexDirection: "column",
              justifyContent: "center",
              gap: 3,
              paddingRight: 6,
              borderRight: "1px solid var(--border-hairline)",
            }}
          >
            <div
              className="mono"
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                color: "var(--signal-alert-text)",
              }}
            >
              {unreviewedAlerts.length} UNREVIEWED
            </div>
            <div
              style={{
                fontSize: "var(--text-xs)",
                color: "var(--text-muted)",
                lineHeight: 1.25,
              }}
            >
              Hover for MP4 · Click for Time-Shift
            </div>
          </div>

          {unreviewedAlerts.slice(0, 14).map((item) => {
            const isHovered = hoveredAlertId === item.id;
            const thumbSrc = item.thumb_path
              ? item.thumb_path.replace("/media/frigate", "")
              : `/api/${item.camera}/latest.webp`;
            const objs = item.data?.objects || ["person"];

            return (
              <div
                key={item.id}
                role="button"
                tabIndex={0}
                className={`nvr-alert-card severity-${item.severity}`}
                onPointerEnter={() => setHoveredAlertId(item.id)}
                onPointerLeave={() =>
                  setHoveredAlertId((curr) => (curr === item.id ? null : curr))
                }
                onClick={() => {
                  setSelectedCamera(item.camera);
                  setLiveLayout("focus");
                  setTimeShiftTs(Math.floor(item.start_time));
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    setSelectedCamera(item.camera);
                    setLiveLayout("focus");
                    setTimeShiftTs(Math.floor(item.start_time));
                  }
                }}
              >
                {isHovered ? (
                  <video
                    src={`/api/review/${encodeURIComponent(item.id)}/preview?format=mp4`}
                    autoPlay
                    muted
                    loop
                    playsInline
                  />
                ) : (
                  <img
                    src={thumbSrc}
                    alt={`${item.camera} alert`}
                    width={156}
                    height={68}
                    loading="lazy"
                  />
                )}
                <div className="nvr-alert-card-hud">
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                    }}
                  >
                    <span
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        textTransform: "uppercase",
                        color: "var(--text-primary)",
                      }}
                    >
                      {item.camera}
                    </span>
                    <button
                      className="nvr-btn"
                      style={{
                        height: 20,
                        padding: "0 5px",
                        fontSize: "var(--text-xs)",
                        background: "rgba(16,185,129,0.25)",
                        borderColor: "rgba(16,185,129,0.6)",
                      }}
                      title={t.live.markReviewed}
                      aria-label={`Mark ${item.camera} alert at ${formatTs(
                        item.start_time
                      )} as reviewed`}
                      onClick={(e) => {
                        e.stopPropagation();
                        handleMarkReviews([item.id], true);
                      }}
                    >
                      <Check size={10} />
                    </button>
                  </div>
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      fontSize: "var(--text-xs)",
                    }}
                  >
                    <span
                      className="mono"
                      style={{ color: "var(--signal-motion-text)", textTransform: "uppercase" }}
                    >
                      {objs[0]}
                    </span>
                    <span className="mono" style={{ color: "var(--text-primary)" }}>
                      {formatTs(item.start_time)}
                    </span>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {liveLayout === "2x2" && (
        <div className="nvr-matrix-2x2">
          {cameraNames.map((cam) => {
            const camCfg = config?.cameras?.[cam];
            const camStat = stats?.cameras?.[cam];
            const act = cameraActivity[cam];
            const hasPerson =
              (act?.objects || []).length > 0 || (camStat?.detection_fps || 0) > 1.5;
            const hasMotion = act?.motion || (camStat?.detection_fps || 0) > 0.2;
            const isTimelineCam = selectedCamera === cam;
            const isTimeShifted = isTimelineCam && timeShiftTs !== null;

            return (
              <div
                key={cam}
                className={`nvr-cam-tile ${isTimeShifted ? "has-native-controls" : ""}`}
                style={{
                  outline: isTimelineCam ? "1px solid var(--signal-accent)" : "none",
                  outlineOffset: -1,
                }}
              >
                {isTimeShifted && timeShiftTs !== null ? (
                  <VodHlsPlayer
                    camera={cam}
                    startTime={timeShiftTs}
                    endTime={timeShiftTs + 180}
                    playbackRate={playbackSpeed}
                  />
                ) : (
                  <LiveMsePlayer
                    camera={cam}
                    useMainStream={useMainStream}
                    cameraConfig={camCfg}
                    showVectors={showVectors}
                    onClick={() => {
                      if (selectedCamera === cam) {
                        setLiveLayout("focus");
                      } else {
                        setSelectedCamera(cam);
                      }
                    }}
                  />
                )}
                <div className={`nvr-cam-hud pos-${hudPosition}`}>
                  <span className={`nvr-badge ${isTimeShifted ? "rewind" : "live"}`}>
                    <span className={`nvr-dot ${isTimeShifted ? "motion" : "live"}`} />
                    {cam.toUpperCase()}
                    {isTimeShifted && timeShiftTs !== null
                      ? ` @ ${formatTs(timeShiftTs)}`
                      : ""}
                  </span>
                  <span className="nvr-badge">
                    {camCfg?.detect?.width || 1280}×{camCfg?.detect?.height || 720} ·{" "}
                    {camStat?.camera_fps?.toFixed(1) || "5.0"} FPS
                  </span>
                  {hasPerson && <span className="nvr-badge alert">PERSON</span>}
                  {hasMotion && !hasPerson && (
                    <span className="nvr-badge motion">{t.live.motionActive}</span>
                  )}
                  <span className="nvr-badge">
                    AI {camStat?.detection_fps?.toFixed(1) || "0.0"} FPS
                  </span>
                  <button
                    className="nvr-btn nvr-tile-action mono"
                    style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                    onClick={(e) => {
                      e.stopPropagation();
                      setSelectedCamera(cam);
                      setTimeShiftTs(Math.floor(Date.now() / 1000) - 30);
                    }}
                  >
                    {t.live.rewind30s}
                  </button>
                  {isTimeShifted && (
                    <button
                      className="nvr-btn success mono"
                      style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                      onClick={(e) => {
                        e.stopPropagation();
                        setTimeShiftTs(null);
                      }}
                    >
                      LIVE
                    </button>
                  )}
                  <button
                    className="nvr-btn nvr-tile-action"
                    style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                    onClick={(e) => {
                      e.stopPropagation();
                      setSelectedCamera(cam);
                      setLiveLayout("focus");
                    }}
                  >
                    Expand
                  </button>
                  <a
                    href={`/api/${cam}/latest.jpg`}
                    download={`${cam}-snapshot.jpg`}
                    className="nvr-btn nvr-tile-action"
                    style={{
                      height: 22,
                      padding: "0 6px",
                      fontSize: "var(--text-xs)",
                      textDecoration: "none",
                    }}
                    onClick={(e) => e.stopPropagation()}
                    title={t.live.snapshot}
                    aria-label={`Download ${cam} snapshot`}
                  >
                    <Camera size={11} />
                  </a>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {(liveLayout === "focus" || liveLayout === "single") && (
        <div
          className={
            liveLayout === "focus" ? "nvr-focus-layout" : "nvr-matrix-single"
          }
        >
          <div className="nvr-focus-stage">
            <div
              className={`nvr-cam-tile ${timeShiftTs !== null ? "has-native-controls" : ""}`}
              style={{ flex: 1 }}
            >
              {timeShiftTs === null ? (
                <LiveMsePlayer
                  camera={selectedCamera}
                  useMainStream={true}
                  cameraConfig={config?.cameras?.[selectedCamera]}
                  showVectors={showVectors}
                />
              ) : (
                <VodHlsPlayer
                  camera={selectedCamera}
                  startTime={timeShiftTs}
                  endTime={timeShiftTs + 180}
                  playbackRate={playbackSpeed}
                />
              )}
              <div className={`nvr-cam-hud pos-${hudPosition}`}>
                <span className={`nvr-badge ${timeShiftTs === null ? "live" : "rewind"}`}>
                  <span
                    className={`nvr-dot ${timeShiftTs === null ? "live" : "motion"}`}
                  />
                  {selectedCamera.toUpperCase()} //{" "}
                  {timeShiftTs === null
                    ? t.live.liveBadge
                    : `${t.live.timeShiftBadge} @ ${formatTs(timeShiftTs)}`}
                </span>
                <span className="nvr-badge">
                  {stats?.cameras?.[selectedCamera]?.camera_fps?.toFixed(1) || "5.0"} FPS ·
                  AI{" "}
                  {stats?.cameras?.[selectedCamera]?.detection_fps?.toFixed(1) || "0.0"} FPS
                </span>
                {timeShiftTs !== null && (
                  <button
                    className="nvr-btn success"
                    onClick={() => setTimeShiftTs(null)}
                  >
                    <Play size={12} />
                    {t.live.returnToLive}
                  </button>
                )}
              </div>
            </div>
          </div>

          {liveLayout === "focus" && (
            <div className="nvr-focus-dock">
              {cameraNames
                .filter((c) => c !== selectedCamera)
                .map((cam) => (
                  <div
                    key={cam}
                    className="nvr-cam-tile"
                    style={{ height: "33.33%", minHeight: 160 }}
                  >
                    <LiveMsePlayer
                      camera={cam}
                      useMainStream={false}
                      cameraConfig={config?.cameras?.[cam]}
                      showVectors={showVectors}
                      onClick={() => setSelectedCamera(cam)}
                    />
                    <div className={`nvr-cam-hud pos-${hudPosition}`}>
                      <span className="nvr-badge live">{cam.toUpperCase()}</span>
                      <span className="nvr-badge">
                        {stats?.cameras?.[cam]?.camera_fps?.toFixed(1) || "5.0"} FPS
                      </span>
                    </div>
                  </div>
                ))}
            </div>
          )}
        </div>
      )}

      <TimelineScrubber
        camera={selectedCamera}
        cameras={cameraNames}
        onSelectCamera={(cam) => setSelectedCamera(cam)}
        segments={focusSegments}
        reviews={reviews}
        currentTs={timeShiftTs}
        onSeek={(ts) => setTimeShiftTs(ts)}
        onExportRange={handleQuickExport}
        titlePrefix="TIMELINE"
      />
    </>
  );
};
