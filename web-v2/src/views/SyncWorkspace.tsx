import React from "react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import { RecordingSegment, ReviewItem } from "../types/nvr";
import { formatDateTime, formatTs } from "../utils/format";
import { VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";

interface SyncWorkspaceProps {
  cameraNames: string[];
  syncMasterTs: number;
  setSyncMasterTs: React.Dispatch<React.SetStateAction<number>>;
  syncSpeed: number;
  setSyncSpeed: (spd: number) => void;
  reviews: ReviewItem[];
  focusSegments: RecordingSegment[];
  handleQuickExport: (
    camera: string,
    startSec: number,
    endSec: number,
    label?: string
  ) => Promise<{ exportId?: string; videoUrl?: string } | void>;
}

export const SyncWorkspace: React.FC<SyncWorkspaceProps> = ({
  cameraNames,
  syncMasterTs,
  setSyncMasterTs,
  syncSpeed,
  setSyncSpeed,
  reviews,
  focusSegments,
  handleQuickExport,
}) => {
  const { t } = useI18n();
  const selectedCamera = useNvrStore((s) => s.selectedCamera);
  const setSelectedCamera = useNvrStore((s) => s.setSelectedCamera);
  const hudPosition = useNvrStore((s) => s.hudPosition);

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span className="nvr-badge rewind">{t.sync.syncLocked}</span>
          <span className="mono" style={{ fontWeight: 700, color: "var(--text-primary)" }}>
            {formatDateTime(syncMasterTs)}
          </span>
          <button className="nvr-btn" onClick={() => setSyncMasterTs((ts) => ts - 30)}>
            {t.sync.jumpBack30}
          </button>
          <button className="nvr-btn" onClick={() => setSyncMasterTs((ts) => ts - 10)}>
            {t.sync.jumpBack10}
          </button>
          <button className="nvr-btn" onClick={() => setSyncMasterTs((ts) => ts + 10)}>
            {t.sync.jumpFwd10}
          </button>
          <button className="nvr-btn" onClick={() => setSyncMasterTs((ts) => ts + 30)}>
            {t.sync.jumpFwd30}
          </button>
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          {[1, 2, 4].map((spd) => (
            <button
              key={spd}
              className={`nvr-btn mono ${syncSpeed === spd ? "active" : ""}`}
              onClick={() => setSyncSpeed(spd)}
            >
              {spd}x
            </button>
          ))}
        </div>
      </div>

      <div className="nvr-workbench">
        <div className="nvr-panel">
          <div className="nvr-panel-header">
            <span>{t.sync.recentIncidents}</span>
            <span className="mono">{reviews.length}</span>
          </div>
          <div className="nvr-scroll-y">
            {reviews.slice(0, 35).map((rev) => {
              const isSelected = Math.abs(rev.start_time - syncMasterTs) < 5;
              return (
                <div
                  key={rev.id}
                  role="button"
                  tabIndex={0}
                  className={`nvr-list-row ${isSelected ? "is-selected" : ""}`}
                  onClick={() => setSyncMasterTs(Math.floor(rev.start_time))}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      setSyncMasterTs(Math.floor(rev.start_time));
                    }
                  }}
                >
                  <img
                    src={rev.thumb_path.replace(/^\/media\/[^/]+/, "")}
                    alt={rev.camera}
                    width={68}
                    height={40}
                    loading="lazy"
                    style={{
                      width: 68,
                      height: 40,
                      objectFit: "cover",
                      borderRadius: 2,
                      border: "1px solid var(--border-hairline)",
                    }}
                  />
                  <div style={{ flex: 1 }}>
                    <div
                      style={{
                        display: "flex",
                        justifyContent: "space-between",
                        fontWeight: 600,
                      }}
                    >
                      <span style={{ textTransform: "uppercase" }}>{rev.camera}</span>
                      <span
                        className="mono"
                        style={{
                          fontSize: "var(--text-xs)",
                          color: "var(--signal-accent-text)",
                        }}
                      >
                        {formatTs(rev.start_time)}
                      </span>
                    </div>
                    <div
                      className="mono"
                      style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                    >
                      {(rev.data?.objects || ["motion"]).join(", ")} · {rev.severity}
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        </div>

        <div className="nvr-matrix-2x2">
          {cameraNames.map((cam) => (
            <div key={cam} className="nvr-cam-tile">
              <VodHlsPlayer
                camera={cam}
                startTime={syncMasterTs}
                endTime={syncMasterTs + 120}
                playbackRate={syncSpeed}
                controls={false}
              />
              <div className={`nvr-cam-hud pos-${hudPosition}`}>
                <span className="nvr-badge rewind">
                  {cam.toUpperCase()} // SYNC @ {formatTs(syncMasterTs)}
                </span>
                <button
                  className="nvr-btn nvr-tile-action"
                  style={{
                    height: 22,
                    padding: "0 8px",
                    fontSize: "var(--text-xs)",
                    pointerEvents: "auto",
                  }}
                  onClick={() => handleQuickExport(cam, syncMasterTs, syncMasterTs + 60)}
                >
                  {t.live.export60s}
                </button>
              </div>
            </div>
          ))}
        </div>
      </div>

      <TimelineScrubber
        camera={selectedCamera}
        cameras={cameraNames}
        onSelectCamera={(cam) => setSelectedCamera(cam)}
        segments={focusSegments}
        reviews={reviews}
        currentTs={syncMasterTs}
        onSeek={(ts) => setSyncMasterTs(ts ?? Math.floor(Date.now() / 1000) - 30)}
        onExportRange={handleQuickExport}
        titlePrefix="4-CH SYNC TIMELINE"
      />
    </>
  );
};
