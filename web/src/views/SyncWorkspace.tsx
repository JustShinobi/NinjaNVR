import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronLeft,
  ChevronRight,
  Pause,
  Play,
  Sparkles,
  Volume2,
  VolumeX,
} from "lucide-react";
import { useI18n } from "../i18n";
import { resolveRealCameraSource, useNvrStore } from "../store/useNvrStore";
import { RecordingSegment, ReviewItem } from "../types/nvr";
import { formatDateTime, formatTs } from "../utils/format";
import {
  VodHlsPlayer,
  resolveVodFrameStepDurationSec,
} from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";
import { useCompactViewport } from "../utils/useCompactViewport";
import { resolveSafeMediaUrl } from "../utils/mediaUrlPolicy";

export const MAX_SYNC_QUAD_SLOTS = 4;
export const SYNC_SEEK_DEBOUNCE_MS = 120;
export const SYNC_DRIFT_THRESHOLD_SEC = 0.45;

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

/**
 * Clamps a synchronized master playhead timestamp while preserving sub-second
 * millisecond precision for frame-by-frame (`±1f`) stepping and smooth clock ticks.
 */
export function clampSyncPlayheadTs(
  ts: number,
  nowSec: number = Date.now() / 1000
): number {
  const safeNow =
    Number.isFinite(nowSec) && nowSec > 60 ? nowSec : Date.now() / 1000;
  const maxAllowed = Math.max(1, Math.floor(safeNow) - 5);
  if (!Number.isFinite(ts) || ts <= 0) {
    return Math.max(1, Math.floor(safeNow) - 30);
  }
  const clamped = Math.max(1, Math.min(maxAllowed, ts));
  return Math.round(clamped * 1000) / 1000;
}

export function shouldReconcileSyncDrift(
  tileAbsTs: number,
  masterTs: number,
  thresholdSec: number = SYNC_DRIFT_THRESHOLD_SEC
): boolean {
  if (!Number.isFinite(tileAbsTs) || !Number.isFinite(masterTs)) {
    return false;
  }
  const safeThreshold =
    Number.isFinite(thresholdSec) && thresholdSec > 0
      ? thresholdSec
      : SYNC_DRIFT_THRESHOLD_SEC;
  return Math.abs(tileAbsTs - masterTs) > safeThreshold;
}

export function advanceSyncClockTick(
  currentMasterTs: number,
  elapsedMs: number,
  playbackSpeed: number,
  nowSec: number = Date.now() / 1000
): number {
  const base = clampSyncPlayheadTs(currentMasterTs, nowSec);
  if (!Number.isFinite(elapsedMs) || elapsedMs <= 0) {
    return base;
  }
  const safeSpd =
    Number.isFinite(playbackSpeed) && playbackSpeed > 0
      ? Math.max(0.25, Math.min(16, playbackSpeed))
      : 1;
  const dtSec = Math.min(5, elapsedMs / 1000) * safeSpd;
  return clampSyncPlayheadTs(base + dtSec, nowSec);
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
  return resolveSafeMediaUrl(thumbPath, {
    kind: "thumbnail",
    fallbackCamera: camera,
  });
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
    label?: string,
    includeAudio?: boolean
  ) => Promise<{ exportId?: string; videoUrl?: string; queued?: boolean } | void>;
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

  const [syncPaused, setSyncPaused] = useState<boolean>(false);
  const [syncMuted, setSyncMuted] = useState<boolean>(true);
  const safeMasterTs = clampSyncPlayheadTs(syncMasterTs);
  const safeSyncSpeed =
    Number.isFinite(syncSpeed) && syncSpeed > 0
      ? Math.max(0.25, Math.min(16, syncSpeed))
      : 1;

  // `committedSeekTs` tracks explicit user/transport seeks so continuous playback `timeupdate`
  // advances `syncMasterTs` without re-seeking the primary video decoder on every tick.
  const [committedSeekTs, setCommittedSeekTs] = useState<number>(() => safeMasterTs);
  const [slotDriftTargets, setSlotDriftTargets] = useState<Record<number, number>>({});
  const seekDebounceTimerRef = useRef<number | null>(null);
  const masterClockTsRef = useRef<number>(safeMasterTs);
  const lastPlaybackTickRef = useRef<number>(safeMasterTs);
  const lastVideoTimeUpdateMsRef = useRef<number>(0);
  const lastUiClockCommitMsRef = useRef<number>(0);
  const lastSlotReconcileMsRef = useRef<Record<number, number>>({});

  useEffect(() => {
    if (syncMasterTs !== safeMasterTs) {
      setSyncMasterTs(safeMasterTs);
    }
  }, [syncMasterTs, safeMasterTs, setSyncMasterTs]);

  // Detect external jumps (e.g. "Open in Sync" from Review/Explore) vs smooth playback ticks
  useEffect(() => {
    if (Math.abs(safeMasterTs - lastPlaybackTickRef.current) <= 0.01) {
      return;
    }
    masterClockTsRef.current = safeMasterTs;
    lastPlaybackTickRef.current = safeMasterTs;
    if (seekDebounceTimerRef.current !== null) {
      window.clearTimeout(seekDebounceTimerRef.current);
      seekDebounceTimerRef.current = null;
    }
    seekDebounceTimerRef.current = window.setTimeout(() => {
      seekDebounceTimerRef.current = null;
      setCommittedSeekTs(safeMasterTs);
      setSlotDriftTargets({});
    }, SYNC_SEEK_DEBOUNCE_MS);
    return () => {
      if (seekDebounceTimerRef.current !== null) {
        window.clearTimeout(seekDebounceTimerRef.current);
        seekDebounceTimerRef.current = null;
      }
    };
  }, [safeMasterTs]);

  const [vodWindow, setVodWindow] = useState<{ startTime: number; endTime: number }>(
    () => computeSyncVodWindow(safeMasterTs, 180)
  );

  useEffect(() => {
    setVodWindow((prev) => {
      if (
        safeMasterTs >= prev.startTime &&
        safeMasterTs <= prev.endTime - 4
      ) {
        return prev;
      }
      return computeSyncVodWindow(safeMasterTs, 180);
    });
  }, [safeMasterTs]);

  const activeQuadCameras = useMemo(
    () => resolveSyncQuadCameras(cameraNames, syncQuadCameras),
    [cameraNames, syncQuadCameras]
  );

  const focusedSyncCamera = useMemo(() => {
    if (selectedCamera && activeQuadCameras.includes(selectedCamera)) {
      return selectedCamera;
    }
    return activeQuadCameras[0] || "";
  }, [selectedCamera, activeQuadCameras]);

  const focusedRealCamera = useMemo(
    () => resolveRealCameraSource(focusedSyncCamera, config?.cameras || cameraNames),
    [focusedSyncCamera, config?.cameras, cameraNames]
  );

  // Fallback wall-clock master ticker while playing if primary `<video>` has not emitted `timeupdate`
  // (e.g. during an offline recording gap on slot 0 or non-decoding test environments).
  useEffect(() => {
    if (syncPaused || activeQuadCameras.length === 0 || typeof window === "undefined") {
      return;
    }
    let prevWallMs = Date.now();
    const timer = window.setInterval(() => {
      const nowMs = Date.now();
      const dtMs = nowMs - prevWallMs;
      prevWallMs = nowMs;
      if (nowMs - lastVideoTimeUpdateMsRef.current <= 650) {
        return;
      }
      const nextTs = advanceSyncClockTick(
        masterClockTsRef.current,
        dtMs,
        safeSyncSpeed,
        nowMs / 1000
      );
      masterClockTsRef.current = nextTs;
      lastPlaybackTickRef.current = nextTs;
      setSyncMasterTs(nextTs);
    }, 300);
    return () => window.clearInterval(timer);
  }, [syncPaused, activeQuadCameras.length, safeSyncSpeed, setSyncMasterTs]);

  const applyBoundedSyncSeek = useCallback(
    (nextTs: number, immediate = false) => {
      const clamped = clampSyncPlayheadTs(nextTs);
      masterClockTsRef.current = clamped;
      lastPlaybackTickRef.current = clamped;
      setSyncMasterTs(clamped);
      if (immediate) {
        if (seekDebounceTimerRef.current !== null) {
          window.clearTimeout(seekDebounceTimerRef.current);
          seekDebounceTimerRef.current = null;
        }
        setCommittedSeekTs(clamped);
        setSlotDriftTargets({});
      } else {
        // Trigger debounced seek via safeMasterTs effect
        lastPlaybackTickRef.current = -1;
      }
    },
    [setSyncMasterTs]
  );

  const stepSyncMasterBy = (deltaSec: number) => {
    if (!Number.isFinite(deltaSec)) return;
    applyBoundedSyncSeek(clampSyncPlayheadTs(masterClockTsRef.current) + deltaSec, true);
  };

  const stepSyncFrameAll = (dir: -1 | 1) => {
    setSyncPaused(true);
    const camCfg =
      config?.cameras?.[focusedSyncCamera] ?? config?.cameras?.[focusedRealCamera];
    const frameStepSec = resolveVodFrameStepDurationSec(camCfg);
    applyBoundedSyncSeek(masterClockTsRef.current + dir * frameStepSec, true);
  };

  const handleTileTimeUpdate = useCallback(
    (slotIdx: number, _curSec: number, absTs?: number) => {
      if (syncPaused || typeof absTs !== "number" || !Number.isFinite(absTs)) {
        return;
      }
      const nowMs = Date.now();
      if (slotIdx === 0) {
        lastVideoTimeUpdateMsRef.current = nowMs;
        const clamped = clampSyncPlayheadTs(absTs, nowMs / 1000);
        masterClockTsRef.current = clamped;
        if (nowMs - lastUiClockCommitMsRef.current >= 250) {
          lastUiClockCommitMsRef.current = nowMs;
          lastPlaybackTickRef.current = clamped;
          setSyncMasterTs(clamped);
        }
        return;
      }

      if (
        shouldReconcileSyncDrift(
          absTs,
          masterClockTsRef.current,
          SYNC_DRIFT_THRESHOLD_SEC
        )
      ) {
        const lastReconcile = lastSlotReconcileMsRef.current[slotIdx] || 0;
        if (nowMs - lastReconcile >= 800) {
          lastSlotReconcileMsRef.current[slotIdx] = nowMs;
          const lockTarget = masterClockTsRef.current;
          setSlotDriftTargets((prev) => ({
            ...prev,
            [slotIdx]: lockTarget,
          }));
        }
      }
    },
    [syncPaused, setSyncMasterTs]
  );

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
          {!isCompact && (
            <span className={`nvr-badge ${syncPaused ? "motion" : "rewind"}`}>
              {syncPaused ? t.sync.syncPausedBadge : t.sync.syncLocked}
            </span>
          )}
          <span
            className="mono"
            data-testid="sync-master-clock"
            style={{ fontWeight: 700, color: "var(--text-primary)" }}
          >
            {formatDateTime(Math.floor(safeMasterTs))}
          </span>

          {/* Master Transport Controls: Play/Pause All, Step -1f/+1f All, Audio Mute/Unmute */}
          <div
            className="nvr-segmented mono"
            role="group"
            aria-label={t.sync.masterClock}
          >
            <button
              type="button"
              data-testid="sync-play-pause"
              className={`nvr-btn ${!syncPaused ? "active" : ""}`}
              aria-pressed={!syncPaused}
              aria-label={syncPaused ? t.sync.playAll : t.sync.pauseAll}
              title={syncPaused ? t.sync.playAll : t.sync.pauseAll}
              onClick={() => setSyncPaused((p) => !p)}
            >
              {syncPaused ? <Play size={12} /> : <Pause size={12} />}
              {syncPaused ? t.sync.playAll : t.sync.pauseAll}
            </button>
            <button
              type="button"
              data-testid="sync-step-prev"
              className="nvr-btn mono"
              aria-label={t.sync.stepPrevFrame}
              title={t.sync.stepPrevFrame}
              onClick={() => stepSyncFrameAll(-1)}
            >
              <ChevronLeft size={12} />
              {t.sync.stepPrevFrame}
            </button>
            <button
              type="button"
              data-testid="sync-step-next"
              className="nvr-btn mono"
              aria-label={t.sync.stepNextFrame}
              title={t.sync.stepNextFrame}
              onClick={() => stepSyncFrameAll(1)}
            >
              {t.sync.stepNextFrame}
              <ChevronRight size={12} />
            </button>
            <button
              type="button"
              data-testid="sync-audio-toggle"
              className={`nvr-btn mono ${!syncMuted ? "active" : ""}`}
              aria-pressed={!syncMuted}
              aria-label={syncMuted ? t.sync.unmuteFocused : t.sync.muteFocused}
              title={
                focusedSyncCamera
                  ? `${syncMuted ? t.sync.unmuteFocused : t.sync.muteFocused} (${focusedSyncCamera.toUpperCase()})`
                  : syncMuted
                  ? t.sync.unmuteFocused
                  : t.sync.muteFocused
              }
              onClick={() => setSyncMuted((m) => !m)}
            >
              {syncMuted ? <VolumeX size={12} /> : <Volume2 size={12} />}
              {focusedSyncCamera ? focusedSyncCamera.toUpperCase() : ""}
            </button>
          </div>

          <button
            type="button"
            className="nvr-btn sm"
            onClick={() => stepSyncMasterBy(-30)}
          >
            {t.sync.jumpBack30}
          </button>
          <button
            type="button"
            className="nvr-btn sm"
            onClick={() => stepSyncMasterBy(-10)}
          >
            {t.sync.jumpBack10}
          </button>
          <button
            type="button"
            className="nvr-btn sm"
            onClick={() => stepSyncMasterBy(10)}
          >
            {t.sync.jumpFwd10}
          </button>
          <button
            type="button"
            className="nvr-btn sm"
            onClick={() => stepSyncMasterBy(30)}
          >
            {t.sync.jumpFwd30}
          </button>
          <span className="nvr-divider" aria-hidden="true" />
          {(() => {
            const slotControls = (
              <>
                {activeQuadCameras.map((slotCam, slotIdx) => (
                  <select
                    key={`slot-${slotIdx}`}
                    className="nvr-select sm mono"
                    aria-label={`Sync Quad Slot ${slotIdx + 1}`}
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
                  className="nvr-btn sm mono"
                  onClick={() =>
                    setSyncQuadCameras(
                      autoPickActiveCamerasAtTimestamp(cameraNames, reviews, safeMasterTs)
                    )
                  }
                  title={t.sync.autoPickHint}
                >
                  <Sparkles size={11} />
                  {t.sync.autoPickBtn}
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
              type="button"
              className={`nvr-btn sm mono ${syncSpeed === spd ? "active" : ""}`}
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
                <button
                  key={rev.id}
                  type="button"
                  className={`nvr-list-row nvr-list-row-btn ${
                    isSelected ? "is-selected" : ""
                  }`}
                  onClick={() => applyBoundedSyncSeek(Math.floor(rev.start_time), true)}
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
                  <div style={{ flex: 1, textAlign: "left", minWidth: 0 }}>
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
                </button>
              );
            })}
          </div>
        </div>

        <div className="nvr-matrix-2x2">
          {activeQuadCameras.length === 0 ? (
            <div
              className="mono"
              style={{
                gridColumn: "1 / -1",
                gridRow: "1 / -1",
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                color: "var(--text-muted)",
                background: "var(--video-stage-bg)",
                padding: 24,
              }}
            >
              {t.sync.emptyCameras}
            </div>
          ) : (
            activeQuadCameras.map((cam, slotIdx) => {
              const realCam = resolveRealCameraSource(
                cam,
                config?.cameras || cameraNames
              );
              const isFocusedTile = cam === focusedSyncCamera;
              const tilePlayheadTarget =
                slotIdx === 0
                  ? committedSeekTs
                  : slotDriftTargets[slotIdx] ?? committedSeekTs;
              return (
                <div
                  key={`${cam}-${slotIdx}`}
                  className={`nvr-cam-tile ${
                    isFocusedTile ? "is-timeline-focused" : ""
                  }`}
                  onClick={() => setSelectedCamera(cam)}
                >
                  <VodHlsPlayer
                    camera={realCam}
                    cameraConfig={
                      config?.cameras?.[cam] ?? config?.cameras?.[realCam]
                    }
                    startTime={vodWindow.startTime}
                    endTime={vodWindow.endTime}
                    playheadTs={tilePlayheadTarget}
                    paused={syncPaused}
                    controlledMuted={isFocusedTile ? syncMuted : true}
                    enableKeyboardStep={isFocusedTile}
                    playbackRate={safeSyncSpeed}
                    controls={false}
                    onTimeUpdate={(curSec, absSec) =>
                      handleTileTimeUpdate(slotIdx, curSec, absSec)
                    }
                  />
                  <div className={`nvr-cam-hud pos-${hudPosition}`}>
                    <span className="nvr-badge rewind">
                      S{slotIdx + 1} · {cam.toUpperCase()}
                      <span className="nvr-sync-badge-ts">
                        {" "}
                        // SYNC @ {formatTs(Math.floor(safeMasterTs))}
                      </span>
                    </span>
                    <button
                      type="button"
                      className="nvr-btn sm nvr-tile-action"
                      style={{
                        pointerEvents: "auto",
                      }}
                      onClick={(e) => {
                        e.stopPropagation();
                        void handleQuickExport(
                          realCam,
                          Math.floor(safeMasterTs),
                          Math.floor(safeMasterTs) + 60
                        );
                      }}
                    >
                      {t.live.export60s}
                    </button>
                  </div>
                </div>
              );
            })
          )}
        </div>
      </div>

      <TimelineScrubber
        camera={focusedSyncCamera || selectedCamera}
        cameras={cameraNames}
        onSelectCamera={(cam) => setSelectedCamera(cam)}
        segments={focusSegments}
        reviews={reviews}
        currentTs={Math.floor(safeMasterTs)}
        onSeek={(ts) =>
          applyBoundedSyncSeek(
            ts ?? Math.floor(Date.now() / 1000) - 30,
            false
          )
        }
        onOpenExports={() => useNvrStore.getState().setActiveTab("exports")}
        onExportRange={handleQuickExport}
        titlePrefix={t.sync.timelineTitle}
      />
    </>
  );
};
