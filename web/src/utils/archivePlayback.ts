import React, { createContext, useContext } from "react";
import type {
  RecordingDaySummary,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import { resolveRealCameraSource } from "../store/useNvrStore";
import { computeReviewPlaybackWindow } from "./forensicTriage";

export const MIN_VOD_WINDOW_SEC = 12;
export const DEFAULT_VOD_WINDOW_SEC = 180;
export const DEFAULT_VOD_EDGE_MARGIN_SEC = 2;
export const MAX_ARCHIVE_EXPORT_SPAN_SEC = 86400;

export interface VodPlaybackWindowOptions {
  durationSec?: number;
  preRollSec?: number;
  nowSec?: number;
}

export interface VodPlaybackWindow {
  startSec: number;
  endSec: number;
}

export interface LoadedVodWindow extends VodPlaybackWindow {
  camera: string;
}

export interface VodSeekRequest {
  camera: string;
  targetTs: number;
  nowSec?: number;
  windowSpanSec?: number;
  edgeMarginSec?: number;
  preRollSec?: number;
  configuredCameras?: Record<string, unknown> | string[] | null;
}

export type VodSeekAction =
  | {
      kind: "seek_in_window";
      offsetSec: number;
      window: LoadedVodWindow;
    }
  | {
      kind: "reload_window";
      offsetSec: number;
      window: LoadedVodWindow;
    };

export interface ArchiveExportRangeValidation {
  valid: boolean;
  startSec: number;
  endSec: number;
  durationSec: number;
  error?: string;
}

function resolveFiniteNowSec(nowSec?: number): number {
  if (typeof nowSec === "number" && Number.isFinite(nowSec) && nowSec > 0) {
    return Math.floor(nowSec);
  }
  return Math.floor(Date.now() / 1000);
}

/**
 * Computes an integer `[startSec, endSec]` VOD window clamped so `endSec <= nowSec`
 * (never requesting future HLS segments on live time-shift rewinds) while preserving
 * at least a 12-second manifest span whenever `nowSec >= 12`.
 */
export function computeVodPlaybackWindow(
  anchorTs: number,
  options?: VodPlaybackWindowOptions
): VodPlaybackWindow {
  const safeNow = resolveFiniteNowSec(options?.nowSec);
  const safeDuration =
    typeof options?.durationSec === "number" &&
    Number.isFinite(options.durationSec) &&
    options.durationSec > 0
      ? Math.max(MIN_VOD_WINDOW_SEC, Math.ceil(options.durationSec))
      : DEFAULT_VOD_WINDOW_SEC;
  const safePreRoll =
    typeof options?.preRollSec === "number" &&
    Number.isFinite(options.preRollSec) &&
    options.preRollSec >= 0
      ? Math.floor(options.preRollSec)
      : 0;
  const fallbackAnchor = Math.max(0, safeNow - safeDuration);
  const rawAnchor =
    typeof anchorTs === "number" && Number.isFinite(anchorTs) && anchorTs > 0
      ? Math.floor(anchorTs)
      : fallbackAnchor;

  const candidateStart = Math.max(0, rawAnchor - safePreRoll);
  const desiredEnd = candidateStart + safeDuration;
  const endSec = Math.min(safeNow, Math.max(candidateStart + MIN_VOD_WINDOW_SEC, desiredEnd));
  const startSec =
    endSec - candidateStart >= MIN_VOD_WINDOW_SEC
      ? candidateStart
      : Math.max(0, endSec - MIN_VOD_WINDOW_SEC);

  return {
    startSec: Math.floor(startSec),
    endSec: Math.floor(endSec),
  };
}

/**
 * Determines whether a seek to `request.targetTs` can reuse the currently loaded
 * HLS manifest via `videoEl.currentTime` (`seek_in_window`) or requires loading
 * a fresh VOD window (`reload_window`), always translating simulated fleet
 * channels (`_sim_XX`) to their underlying physical camera source.
 */
export function resolveVodSeekAction(
  currentWindow: LoadedVodWindow | null,
  request: VodSeekRequest
): VodSeekAction {
  const realCamera = resolveRealCameraSource(
    request.camera,
    request.configuredCameras
  );
  const safeNow = resolveFiniteNowSec(request.nowSec);
  const safeMargin =
    typeof request.edgeMarginSec === "number" &&
    Number.isFinite(request.edgeMarginSec) &&
    request.edgeMarginSec >= 0
      ? request.edgeMarginSec
      : DEFAULT_VOD_EDGE_MARGIN_SEC;

  if (
    currentWindow &&
    Number.isFinite(currentWindow.startSec) &&
    Number.isFinite(currentWindow.endSec) &&
    currentWindow.endSec > currentWindow.startSec &&
    typeof request.targetTs === "number" &&
    Number.isFinite(request.targetTs)
  ) {
    const currentRealCamera = resolveRealCameraSource(
      currentWindow.camera,
      request.configuredCameras
    );
    const maxReusableTs = Math.max(
      currentWindow.startSec,
      currentWindow.endSec - safeMargin
    );
    if (
      currentRealCamera === realCamera &&
      request.targetTs >= currentWindow.startSec &&
      request.targetTs <= maxReusableTs
    ) {
      const normalizedCurrent: LoadedVodWindow =
        currentWindow.camera === realCamera
          ? currentWindow
          : {
              camera: realCamera,
              startSec: currentWindow.startSec,
              endSec: currentWindow.endSec,
            };
      return {
        kind: "seek_in_window",
        offsetSec: Math.max(0, request.targetTs - normalizedCurrent.startSec),
        window: normalizedCurrent,
      };
    }
  }

  const nextBounds = computeVodPlaybackWindow(request.targetTs, {
    durationSec: request.windowSpanSec,
    preRollSec: request.preRollSec,
    nowSec: safeNow,
  });
  const nextWindow: LoadedVodWindow = {
    camera: realCamera,
    startSec: nextBounds.startSec,
    endSec: nextBounds.endSec,
  };
  const clampedTarget =
    typeof request.targetTs === "number" && Number.isFinite(request.targetTs)
      ? Math.min(nextWindow.endSec, Math.max(nextWindow.startSec, request.targetTs))
      : nextWindow.startSec;

  return {
    kind: "reload_window",
    offsetSec: Math.max(0, clampedTarget - nextWindow.startSec),
    window: nextWindow,
  };
}

/**
 * Computes the VOD playback window and true clip export bounds for a `ReviewItem`
 * without mutating `review.start_time` / `review.end_time` when scrubbing or
 * jumping to a trigger frame.
 */
export function resolveReviewArchivePlayback(
  review: Pick<ReviewItem, "camera" | "start_time" | "end_time">,
  playheadTs: number | null | undefined,
  options?: {
    currentWindow?: LoadedVodWindow | null;
    nowSec?: number;
    configuredCameras?: Record<string, unknown> | string[] | null;
  }
): {
  realCamera: string;
  vodWindow: LoadedVodWindow;
  exportWindow: VodPlaybackWindow;
  seekAction: VodSeekAction | null;
} {
  const realCamera = resolveRealCameraSource(
    review.camera,
    options?.configuredCameras
  );
  const incidentBounds = computeReviewPlaybackWindow(
    review.start_time,
    review.end_time,
    4,
    6,
    20
  );
  const exportWindow = computeReviewPlaybackWindow(
    review.start_time,
    review.end_time,
    5,
    5,
    20
  );
  const baseWindow: LoadedVodWindow =
    options?.currentWindow &&
    resolveRealCameraSource(
      options.currentWindow.camera,
      options?.configuredCameras
    ) === realCamera
      ? {
          camera: realCamera,
          startSec: options.currentWindow.startSec,
          endSec: options.currentWindow.endSec,
        }
      : {
          camera: realCamera,
          startSec: incidentBounds.startSec,
          endSec: incidentBounds.endSec,
        };

  if (typeof playheadTs !== "number" || !Number.isFinite(playheadTs)) {
    return {
      realCamera,
      vodWindow: {
        camera: realCamera,
        startSec: incidentBounds.startSec,
        endSec: incidentBounds.endSec,
      },
      exportWindow,
      seekAction: null,
    };
  }

  const seekAction = resolveVodSeekAction(baseWindow, {
    camera: realCamera,
    targetTs: playheadTs,
    nowSec: options?.nowSec,
    windowSpanSec: 60,
    preRollSec: 4,
    edgeMarginSec: DEFAULT_VOD_EDGE_MARGIN_SEC,
    configuredCameras: options?.configuredCameras,
  });

  return {
    realCamera,
    vodWindow: seekAction.window,
    exportWindow,
    seekAction,
  };
}

/**
 * Validates `[startSec, endSec]` archive clip export bounds.
 */
export function validateArchiveExportRange(
  startSec: number,
  endSec: number,
  maxSpanSec: number = MAX_ARCHIVE_EXPORT_SPAN_SEC
): ArchiveExportRangeValidation {
  if (
    typeof startSec !== "number" ||
    typeof endSec !== "number" ||
    !Number.isFinite(startSec) ||
    !Number.isFinite(endSec) ||
    startSec < 0
  ) {
    return {
      valid: false,
      startSec: 0,
      endSec: 0,
      durationSec: 0,
      error: "Invalid export time window: start and end timestamps must be finite",
    };
  }
  const normalizedStart = Math.floor(startSec);
  const normalizedEnd = Math.ceil(endSec);
  const durationSec = normalizedEnd - normalizedStart;
  if (durationSec <= 0) {
    return {
      valid: false,
      startSec: normalizedStart,
      endSec: normalizedEnd,
      durationSec: 0,
      error: "Invalid export time window: end time must be greater than start time",
    };
  }
  const safeMax =
    typeof maxSpanSec === "number" &&
    Number.isFinite(maxSpanSec) &&
    maxSpanSec > 0
      ? maxSpanSec
      : MAX_ARCHIVE_EXPORT_SPAN_SEC;
  if (durationSec > safeMax) {
    return {
      valid: false,
      startSec: normalizedStart,
      endSec: normalizedEnd,
      durationSec,
      error: `Export time window exceeds maximum allowed duration (${Math.round(
        safeMax / 3600
      )}h)`,
    };
  }
  return {
    valid: true,
    startSec: normalizedStart,
    endSec: normalizedEnd,
    durationSec,
  };
}

export interface ArchiveMetadataTransportOptions {
  fetchImpl?: typeof fetch;
  ttlMs?: number;
  nowMs?: () => number;
  getConfiguredCameras?: () => Record<string, unknown> | string[] | null | undefined;
}

export type ArchiveMetadataTransportInput =
  | typeof fetch
  | ArchiveMetadataTransportOptions
  | undefined;

export interface ArchiveMetadataQueryOptions {
  configuredCameras?: Record<string, unknown> | string[] | null;
  force?: boolean;
}

export interface ArchiveMetadataCache {
  getRecordings(
    camera: string,
    range: { after: number; before: number },
    options?: ArchiveMetadataQueryOptions
  ): Promise<RecordingSegment[]>;
  getSummary(
    camera: string,
    options?: ArchiveMetadataQueryOptions
  ): Promise<RecordingDaySummary[]>;
  invalidate(
    camera?: string,
    configuredCameras?: Record<string, unknown> | string[] | null
  ): void;
  clear(): void;
}

interface RecordingsInflightEntry {
  after: number;
  before: number;
  promise: Promise<{ ok: boolean; segments: RecordingSegment[] }>;
}

interface RecordingsCachedEntry {
  after: number;
  before: number;
  fetchedAtMs: number;
  segments: RecordingSegment[];
}

interface SummaryCachedEntry {
  fetchedAtMs: number;
  days: RecordingDaySummary[];
}

function sliceCoveringSegments(
  segments: RecordingSegment[],
  entryAfter: number,
  entryBefore: number,
  reqAfter: number,
  reqBefore: number
): RecordingSegment[] {
  if (entryAfter === reqAfter && entryBefore === reqBefore) {
    return [...segments];
  }
  return segments.filter((seg) => {
    const startOk =
      typeof seg?.start_time !== "number" ||
      !Number.isFinite(seg.start_time) ||
      seg.start_time <= reqBefore;
    const endOk =
      typeof seg?.end_time !== "number" ||
      !Number.isFinite(seg.end_time) ||
      seg.end_time >= reqAfter;
    return startOk && endOk;
  });
}

/**
 * Deduplicates concurrent and overlapping `/api/:realCam/recordings` and
 * `/api/:realCam/recordings/summary` requests across `App.tsx` and
 * `TimelineScrubber.tsx`, translating virtual `_sim_XX` channels through
 * `resolveRealCameraSource`.
 */
export function createArchiveMetadataCache(
  transport?: ArchiveMetadataTransportInput
): ArchiveMetadataCache {
  const opts: ArchiveMetadataTransportOptions =
    typeof transport === "function"
      ? { fetchImpl: transport }
      : transport ?? {};
  const getFetch = (): typeof fetch => opts.fetchImpl ?? fetch;
  const ttlMs =
    typeof opts.ttlMs === "number" && Number.isFinite(opts.ttlMs) && opts.ttlMs >= 0
      ? opts.ttlMs
      : 3000;
  const nowMs = opts.nowMs ?? (() => Date.now());

  const recordingsInflight = new Map<string, RecordingsInflightEntry[]>();
  const recordingsCache = new Map<string, RecordingsCachedEntry[]>();
  const summaryInflight = new Map<
    string,
    Promise<{ ok: boolean; days: RecordingDaySummary[] }>
  >();
  const summaryCache = new Map<string, SummaryCachedEntry>();

  const resolveCam = (
    camera: string,
    configuredCameras?: Record<string, unknown> | string[] | null
  ): string =>
    resolveRealCameraSource(
      camera,
      configuredCameras ?? opts.getConfiguredCameras?.()
    );

  return {
    async getRecordings(camera, range, options) {
      const realCam = resolveCam(camera, options?.configuredCameras);
      const reqAfter = Number.isFinite(range.after) ? Math.floor(range.after) : 0;
      const reqBefore = Number.isFinite(range.before)
        ? Math.ceil(Math.max(range.before, reqAfter))
        : reqAfter;
      const now = nowMs();

      if (!options?.force && ttlMs > 0) {
        const cachedList = recordingsCache.get(realCam);
        if (cachedList && cachedList.length > 0) {
          const fresh = cachedList.filter((e) => now - e.fetchedAtMs <= ttlMs);
          if (fresh.length !== cachedList.length) {
            if (fresh.length > 0) recordingsCache.set(realCam, fresh);
            else recordingsCache.delete(realCam);
          }
          const covering = fresh.find(
            (e) => e.after <= reqAfter && e.before >= reqBefore
          );
          if (covering) {
            return sliceCoveringSegments(
              covering.segments,
              covering.after,
              covering.before,
              reqAfter,
              reqBefore
            );
          }
        }
      }

      if (!options?.force) {
        const inflightList = recordingsInflight.get(realCam) ?? [];
        const coveringInflight = inflightList.find(
          (e) => e.after <= reqAfter && e.before >= reqBefore
        );
        if (coveringInflight) {
          const shared = await coveringInflight.promise;
          if (shared.ok) {
            return sliceCoveringSegments(
              shared.segments,
              coveringInflight.after,
              coveringInflight.before,
              reqAfter,
              reqBefore
            );
          }
          return [];
        }
      }

      const url = `/api/${encodeURIComponent(
        realCam
      )}/recordings?after=${reqAfter}&before=${reqBefore}`;
      let entryRef: RecordingsInflightEntry | null = null;
      const promise = (async (): Promise<{
        ok: boolean;
        segments: RecordingSegment[];
      }> => {
        try {
          const res = await getFetch()(url, { credentials: "include" });
          if (!res.ok) return { ok: false, segments: [] };
          const json = await res.json().catch(() => null);
          if (!Array.isArray(json)) return { ok: false, segments: [] };
          const segments = json as RecordingSegment[];
          if (ttlMs > 0) {
            const existing = (recordingsCache.get(realCam) ?? []).filter(
              (e) =>
                nowMs() - e.fetchedAtMs <= ttlMs &&
                !(e.after >= reqAfter && e.before <= reqBefore)
            );
            existing.unshift({
              after: reqAfter,
              before: reqBefore,
              fetchedAtMs: nowMs(),
              segments,
            });
            recordingsCache.set(realCam, existing.slice(0, 6));
          }
          return { ok: true, segments };
        } catch {
          return { ok: false, segments: [] };
        } finally {
          const cur = recordingsInflight.get(realCam);
          if (cur && entryRef) {
            const next = cur.filter((x) => x !== entryRef);
            if (next.length > 0) recordingsInflight.set(realCam, next);
            else recordingsInflight.delete(realCam);
          }
        }
      })();

      entryRef = { after: reqAfter, before: reqBefore, promise };
      const list = recordingsInflight.get(realCam) ?? [];
      list.push(entryRef);
      recordingsInflight.set(realCam, list);

      const out = await promise;
      return out.ok ? [...out.segments] : [];
    },

    async getSummary(camera, options) {
      const realCam = resolveCam(camera, options?.configuredCameras);
      const now = nowMs();

      if (!options?.force && ttlMs > 0) {
        const cached = summaryCache.get(realCam);
        if (cached) {
          if (now - cached.fetchedAtMs <= ttlMs) {
            return [...cached.days];
          }
          summaryCache.delete(realCam);
        }
      }

      if (!options?.force) {
        const existingInflight = summaryInflight.get(realCam);
        if (existingInflight) {
          const shared = await existingInflight;
          return shared.ok ? [...shared.days] : [];
        }
      }

      const url = `/api/${encodeURIComponent(realCam)}/recordings/summary`;
      let summaryRef: Promise<{
        ok: boolean;
        days: RecordingDaySummary[];
      }> | null = null;
      const promise = (async (): Promise<{
        ok: boolean;
        days: RecordingDaySummary[];
      }> => {
        try {
          const res = await getFetch()(url, { credentials: "include" });
          if (!res.ok) return { ok: false, days: [] };
          const json = await res.json().catch(() => null);
          if (!Array.isArray(json)) return { ok: false, days: [] };
          const days = json as RecordingDaySummary[];
          if (ttlMs > 0) {
            summaryCache.set(realCam, {
              fetchedAtMs: nowMs(),
              days,
            });
          }
          return { ok: true, days };
        } catch {
          return { ok: false, days: [] };
        } finally {
          if (summaryRef && summaryInflight.get(realCam) === summaryRef) {
            summaryInflight.delete(realCam);
          }
        }
      })();

      summaryRef = promise;
      summaryInflight.set(realCam, promise);
      const out = await promise;
      return out.ok ? [...out.days] : [];
    },

    invalidate(camera, configuredCameras) {
      if (!camera) {
        recordingsCache.clear();
        recordingsInflight.clear();
        summaryCache.clear();
        summaryInflight.clear();
        return;
      }
      const realCam = resolveCam(camera, configuredCameras);
      recordingsCache.delete(realCam);
      recordingsInflight.delete(realCam);
      summaryCache.delete(realCam);
      summaryInflight.delete(realCam);
    },

    clear() {
      recordingsCache.clear();
      recordingsInflight.clear();
      summaryCache.clear();
      summaryInflight.clear();
    },
  };
}

const ArchiveMetadataCacheContext = createContext<ArchiveMetadataCache | null>(
  null
);

export const ArchiveMetadataCacheProvider: React.FC<{
  cache: ArchiveMetadataCache;
  children: React.ReactNode;
}> = ({ cache, children }) =>
  React.createElement(
    ArchiveMetadataCacheContext.Provider,
    { value: cache },
    children
  );

export function useArchiveMetadataCache(): ArchiveMetadataCache | null {
  return useContext(ArchiveMetadataCacheContext);
}
