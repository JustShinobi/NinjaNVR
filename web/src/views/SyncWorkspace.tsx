import React, { useEffect, useMemo, useRef, useState } from "react";
import { Sparkles } from "lucide-react";
import { useI18n } from "../i18n";
import { resolveRealCameraSource, useNvrStore } from "../store/useNvrStore";
import { RecordingSegment, ReviewItem } from "../types/nvr";
import { formatDateTime, formatTs } from "../utils/format";
import { VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";
import { useCompactViewport } from "../utils/useCompactViewport";

export const MAX_SYNC_QUAD_SLOTS = 4;
export const SYNC_SEEK_DEBOUNCE_MS = 120;

export function clampSyncTimestamp(
  ts: number,
  nowSec: number = Math.floor(Date.now() / 1000)
): number {
  const safeNow =
    Number.isFinite(nowSec) && nowSec > 60
      ? Math.floor(nowSec)
      : Math.floor(Date.now() / 1000);
  const maxAllowed = Math.max(1, safeNow - 5);
  if (!Number.isFinite(ts) || ts <= 0) {
    return Math.max(1, safeNow - 30);
  }
  return Math.max(1, Math.min(maxAllowed, Math.floor(ts)));
}

export function computeSyncVodWindow(
  masterTs: number,
  windowSec: number = 120,
  nowSec: number = Math.floor(Date.now() / 1000)
): { startTime: number; endTime: number } {
  const startTime = clampSyncTimestamp(masterTs, nowSec);
  const safeDur = Number.isFinite(windowSec)
    ? Math.max(12, Math.min(3600, Math.floor(windowSec)))
    : 120;
  const safeNow =
    Number.isFinite(nowSec) && nowSec > 60
      ? Math.floor(nowSec)
      : Math.floor(Date.now() / 1000);
  const endTime = Math.max(
    startTime + 12,
    Math.min(startTime + safeDur, Math.max(startTime + 12, safeNow))
  );
  return { startTime, endTime };
}

export function resolveSafeSyncThumbSrc(
  thumbPath: string | null | undefined,
  camera: string
): string {
  const rawCam = (camera || "").trim();
  const cleanCam =
    !rawCam || rawCam === "." || rawCam === ".." || rawCam.includes("..")
      ? "camera"
      : rawCam;
  const encodedCam = encodeURIComponent(cleanCam);
  const fallback = `/api/${encodedCam}/latest.webp`;
  if (typeof thumbPath !== "string") return fallback;
  const trimmed = thumbPath.trim();
  if (!trimmed || /[\u0000-\u001F\u007F\\?#]/.test(trimmed)) return fallback;
  const stripped = trimmed.replace(/^\/media\/[^/]+/, "").trim();
  if (
    !stripped ||
    !stripped.startsWith("/") ||
    stripped.startsWith("//") ||
    /^\/%2f/i.test(stripped) ||
    stripped.includes("..") ||
    /%2e%2e|%5c|%252e|%255c|%00/i.test(stripped)
  ) {
    return fallback;
  }
  try {
    const decoded = decodeURIComponent(stripped);
    const segments = decoded.split("/");
    if (
      !decoded.startsWith("/") ||
      decoded.startsWith("//") ||
      decoded.includes("\\") ||
      decoded.includes("..") ||
      decoded.includes("%") ||
      segments.some((seg) => seg === "." || seg === "..") ||
      /[\u0000-\u001F\u007F?#]/.test(decoded) ||
      /^\/api\/(?:auth|logout|users|tokens|config|restart)\b/i.test(decoded)
    ) {
      return fallback;
    }
  } catch {
    return fallback;
  }
  return stripped;
}

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
  if (!allCameras.includes(camera)) {
    return resolved.slice(0, MAX_SYNC_QUAD_SLOTS);
  }
  const rawSlot = Number.isFinite(slotIndex) ? Math.trunc(slotIndex) : 0;
  const safeSlot = Math.max(0, Math.min(rawSlot, resolved.length - 1));
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
  const uniqueCams: string[] = [];
  for (const cam of Array.isArray(cameraNames) ? cameraNames : []) {
    if (typeof cam === "string" && cam && !uniqueCams.includes(cam)) {
      uniqueCams.push(cam);
    }
  }
  if (!Number.isFinite(timestampSec)) {
    return uniqueCams.slice(0, MAX_SYNC_QUAD_SLOTS);
  }
  const safeWindow = Number.isFinite(windowSec) ? Math.max(1, windowSec) : 60;
  const scores = new Map<string, number>();
  for (const cam of uniqueCams) {
    scores.set(cam, 0);
  }
  for (const rev of Array.isArray(reviews) ? reviews : []) {
    if (!rev || !scores.has(rev.camera) || !Number.isFinite(rev.start_time)) {
      continue;
    }
    const revStart = rev.start_time;
    const revEnd =
      rev.end_time !== undefined &&
      rev.end_time !== null &&
      Number.isFinite(rev.end_time)
        ? Math.max(revStart, rev.end_time)
        : revStart + 15;
    if (
      revStart <= timestampSec + safeWindow &&
      revEnd >= timestampSec - safeWindow
    ) {
      const dist = Math.abs(revStart - timestampSec);
      const proximityBonus = Math.max(
        1,
        safeWindow - Math.min(safeWindow, dist)
      );
      const severityWeight = rev.severity === "alert" ? 200 : 100;
      scores.set(
        rev.camera,
        (scores.get(rev.camera) || 0) + severityWeight + proximityBonus
      );
    }
  }

  const indexMap = new Map<string, number>();
  uniqueCams.forEach((cam, idx) => indexMap.set(cam, idx));

  const sorted = [...uniqueCams].sort((a, b) => {
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
  const isCompact = useCompactViewport();
  const selectedCamera = useNvrStore((s) => s.selectedCamera);
  const setSelectedCamera = useNvrStore((s) => s.setSelectedCamera);
  const hudPosition = useNvrStore((s) => s.hudPosition);
  const syncQuadCameras = useNvrStore((s) => s.syncQuadCameras);
  const setSyncQuadCameras = useNvrStore((s) => s.setSyncQuadCameras);
  const config = useNvrStore((s) => s.config);

  const safeMasterTs = clampSyncTimestamp(syncMasterTs);
  const safeSyncSpeed =
    Number.isFinite(syncSpeed) && syncSpeed > 0
      ? Math.max(0.25, Math.min(16, syncSpeed))
      : 1;
  const [committedVodTs, setCommittedVodTs] = useState<number>(() => safeMasterTs);
  const seekDebounceTimerRef = useRef<number | null>(null);

  useEffect(() => {
    if (syncMasterTs !== safeMasterTs) {
      setSyncMasterTs(safeMasterTs);
    }
  }, [syncMasterTs, safeMasterTs, setSyncMasterTs]);

  useEffect(() => {
    if (seekDebounceTimerRef.current !== null) {
      window.clearTimeout(seekDebounceTimerRef.current);
      seekDebounceTimerRef.current = null;
    }
    if (committedVodTs === safeMasterTs) {
      return;
    }
    seekDebounceTimerRef.current = window.setTimeout(() => {
      seekDebounceTimerRef.current = null;
      setCommittedVodTs(safeMasterTs);
    }, SYNC_SEEK_DEBOUNCE_MS);
    return () => {
      if (seekDebounceTimerRef.current !== null) {
        window.clearTimeout(seekDebounceTimerRef.current);
        seekDebounceTimerRef.current = null;
      }
    };
  }, [safeMasterTs, committedVodTs]);

  const vodWindow = useMemo(
    () => computeSyncVodWindow(committedVodTs, 120),
    [committedVodTs]
  );

  const activeQuadCameras = useMemo(
    () => resolveSyncQuadCameras(cameraNames, syncQuadCameras),
    [cameraNames, syncQuadCameras]
  );

  const applyBoundedSyncSeek = (nextTs: number) => {
    const clamped = clampSyncTimestamp(nextTs);
    setSyncMasterTs(clamped);
  };

  const stepSyncMasterBy = (deltaSec: number) => {
    if (!Number.isFinite(deltaSec)) return;
    setSyncMasterTs((prev) => clampSyncTimestamp(clampSyncTimestamp(prev) + deltaSec));
  };

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
          {!isCompact && <span className="nvr-badge rewind">{t.sync.syncLocked}</span>}
          <span className="mono" style={{ fontWeight: 700, color: "var(--text-primary)" }}>
            {formatDateTime(safeMasterTs)}
          </span>
          <button className="nvr-btn" onClick={() => stepSyncMasterBy(-30)}>
            {t.sync.jumpBack30}
          </button>
          <button className="nvr-btn" onClick={() => stepSyncMasterBy(-10)}>
            {t.sync.jumpBack10}
          </button>
          <button className="nvr-btn" onClick={() => stepSyncMasterBy(10)}>
            {t.sync.jumpFwd10}
          </button>
          <button className="nvr-btn" onClick={() => stepSyncMasterBy(30)}>
            {t.sync.jumpFwd30}
          </button>
          <span className="nvr-divider" aria-hidden="true" />
          {(() => {
            const slotControls = (
              <>
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
                autoPickActiveCamerasAtTimestamp(cameraNames, reviews, safeMasterTs)
              )
            }
            title="Automatically select the 4 cameras with active alerts or detections near the current timestamp"
          >
            <Sparkles size={11} />
            Auto-Pick 4 Active @ Timestamp
          </button>
              </>
            );
            return isCompact ? (
              <details className="nvr-sync-slots" data-testid="sync-slot-disclosure">
                <summary className="nvr-btn">
                  {activeQuadCameras.map((c) => c.toUpperCase()).join(" · ")}
                </summary>
                <div className="nvr-sync-slots-body">{slotControls}</div>
              </details>
            ) : (
              slotControls
            );
          })()}
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
              const isSelected = Math.abs(rev.start_time - safeMasterTs) < 5;
              const thumbSrc = resolveSafeSyncThumbSrc(
                rev.thumb_path,
                rev.camera
              );
              return (
                <div
                  key={rev.id}
                  role="button"
                  tabIndex={0}
                  className={`nvr-list-row ${isSelected ? "is-selected" : ""}`}
                  onClick={() => applyBoundedSyncSeek(Math.floor(rev.start_time))}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      applyBoundedSyncSeek(Math.floor(rev.start_time));
                    }
                  }}
                >
                  <img
                    src={thumbSrc}
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
          {activeQuadCameras.map((cam, slotIdx) => {
            const realCam = resolveRealCameraSource(
              cam,
              config?.cameras || cameraNames
            );
            return (
              <div key={`${cam}-${slotIdx}`} className="nvr-cam-tile">
                <VodHlsPlayer
                  camera={realCam}
                  startTime={vodWindow.startTime}
                  endTime={vodWindow.endTime}
                  playbackRate={safeSyncSpeed}
                  controls={false}
                />
                <div className={`nvr-cam-hud pos-${hudPosition}`}>
                  <span className="nvr-badge rewind">
                    S{slotIdx + 1} · {cam.toUpperCase()}
                    <span className="nvr-sync-badge-ts"> // SYNC @ {formatTs(safeMasterTs)}</span>
                  </span>
                  <button
                    className="nvr-btn nvr-tile-action"
                    style={{
                      height: 22,
                      padding: "0 8px",
                      fontSize: "var(--text-xs)",
                      pointerEvents: "auto",
                    }}
                    onClick={() =>
                      handleQuickExport(realCam, safeMasterTs, safeMasterTs + 60)
                    }
                  >
                    {t.live.export60s}
                  </button>
                </div>
              </div>
            );
          })}
        </div>
      </div>

      <TimelineScrubber
        camera={selectedCamera}
        cameras={cameraNames}
        onSelectCamera={(cam) => setSelectedCamera(cam)}
        segments={focusSegments}
        reviews={reviews}
        currentTs={safeMasterTs}
        onSeek={(ts) =>
          applyBoundedSyncSeek(ts ?? Math.floor(Date.now() / 1000) - 30)
        }
        onOpenExports={() => useNvrStore.getState().setActiveTab("exports")}
        onExportRange={handleQuickExport}
        titlePrefix="4-CH SYNC TIMELINE"
      />
    </>
  );
};
