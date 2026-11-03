import React, { useMemo } from "react";
import { Sparkles } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import { RecordingSegment, ReviewItem } from "../types/nvr";
import { formatDateTime, formatTs } from "../utils/format";
import { VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";

export const MAX_SYNC_QUAD_SLOTS = 4;

export function resolveSyncQuadCameras(
  cameraNames: string[],
  syncQuadCameras: string[]
): string[] {
  const validSet = new Set(cameraNames);
  const picked: string[] = [];
  for (const cam of syncQuadCameras) {
    if (validSet.has(cam) && !picked.includes(cam)) {
      picked.push(cam);
      if (picked.length >= MAX_SYNC_QUAD_SLOTS) break;
    }
  }
  for (const cam of cameraNames) {
    if (picked.length >= Math.min(MAX_SYNC_QUAD_SLOTS, cameraNames.length)) {
      break;
    }
    if (!picked.includes(cam)) {
      picked.push(cam);
    }
  }
  return picked.slice(0, MAX_SYNC_QUAD_SLOTS);
}

export function assignSyncQuadSlot(
  currentQuad: string[],
  slotIndex: number,
  camera: string,
  allCameras: string[]
): string[] {
  const resolved = resolveSyncQuadCameras(allCameras, currentQuad);
  if (resolved.length === 0) return [];
  const safeSlot = Math.max(0, Math.min(slotIndex, resolved.length - 1));
  const next = [...resolved];
  const existingIdx = next.indexOf(camera);
  if (existingIdx !== -1 && existingIdx !== safeSlot) {
    next[existingIdx] = next[safeSlot];
  }
  next[safeSlot] = camera;
  return next.slice(0, MAX_SYNC_QUAD_SLOTS);
}

export function autoPickActiveCamerasAtTimestamp(
  cameraNames: string[],
  reviews: ReviewItem[],
  timestampSec: number,
  windowSec: number = 60
): string[] {
  const scores = new Map<string, number>();
  for (const cam of cameraNames) {
    scores.set(cam, 0);
  }
  for (const rev of reviews) {
    if (!scores.has(rev.camera)) continue;
    const revStart = rev.start_time;
    const revEnd = rev.end_time ?? revStart + 15;
    if (
      revStart <= timestampSec + windowSec &&
      revEnd >= timestampSec - windowSec
    ) {
      const dist = Math.abs(revStart - timestampSec);
      const proximityBonus = Math.max(1, windowSec - Math.min(windowSec, dist));
      const severityWeight = rev.severity === "alert" ? 200 : 100;
      scores.set(
        rev.camera,
        (scores.get(rev.camera) || 0) + severityWeight + proximityBonus
      );
    }
  }

  const indexMap = new Map<string, number>();
  cameraNames.forEach((cam, idx) => indexMap.set(cam, idx));

  const sorted = [...cameraNames].sort((a, b) => {
    const scoreDiff = (scores.get(b) || 0) - (scores.get(a) || 0);
    if (scoreDiff !== 0) return scoreDiff;
    return (indexMap.get(a) ?? 0) - (indexMap.get(b) ?? 0);
  });

  return sorted.slice(0, MAX_SYNC_QUAD_SLOTS);
}

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
  const syncQuadCameras = useNvrStore((s) => s.syncQuadCameras);
  const setSyncQuadCameras = useNvrStore((s) => s.setSyncQuadCameras);

  const activeQuadCameras = useMemo(
    () => resolveSyncQuadCameras(cameraNames, syncQuadCameras),
    [cameraNames, syncQuadCameras]
  );

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
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
          <span className="nvr-divider" aria-hidden="true" />
          {activeQuadCameras.map((slotCam, slotIdx) => (
            <select
              key={`slot-${slotIdx}`}
              className="nvr-select mono"
              aria-label={`Sync Quad Slot ${slotIdx + 1}`}
              style={{ height: 24, fontSize: "var(--text-xs)", padding: "0 6px" }}
              value={slotCam}
              onChange={(e) =>
                setSyncQuadCameras(
                  assignSyncQuadSlot(
                    activeQuadCameras,
                    slotIdx,
                    e.target.value,
                    cameraNames
                  )
                )
              }
            >
              {cameraNames.map((c) => (
                <option key={c} value={c}>
                  S{slotIdx + 1}: {c.toUpperCase()}
                </option>
              ))}
            </select>
          ))}
          <button
            type="button"
            className="nvr-btn mono"
            style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
            onClick={() =>
              setSyncQuadCameras(
                autoPickActiveCamerasAtTimestamp(cameraNames, reviews, syncMasterTs)
              )
            }
            title="Automatically select the 4 cameras with active alerts or detections near the current timestamp"
          >
            <Sparkles size={11} />
            Auto-Pick 4 Active @ Timestamp
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
          {activeQuadCameras.map((cam, slotIdx) => (
            <div key={`${cam}-${slotIdx}`} className="nvr-cam-tile">
              <VodHlsPlayer
                camera={cam}
                startTime={syncMasterTs}
                endTime={syncMasterTs + 120}
                playbackRate={syncSpeed}
                controls={false}
              />
              <div className={`nvr-cam-hud pos-${hudPosition}`}>
                <span className="nvr-badge rewind">
                  S{slotIdx + 1} · {cam.toUpperCase()} // SYNC @ {formatTs(syncMasterTs)}
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
