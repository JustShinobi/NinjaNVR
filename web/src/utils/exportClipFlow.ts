import type {
  ExportJobItem,
  ExportJobStatus,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import { resolveSafeMediaUrl } from "./mediaUrlPolicy";

export interface TimelineFrameIndexEntry {
  ts: number;
  tsMs: number;
  url: string;
}

export interface ClipOverlayGeometry {
  visible: boolean;
  leftPct: number;
  widthPct: number;
  showInHandle: boolean;
  showOutHandle: boolean;
  offscreenIn: "left" | "right" | null;
  offscreenOut: "left" | "right" | null;
}

export interface ClipPreFlightSummary {
  durationSec: number;
  segmentCount: number;
  estimatedSizeMb: number;
  estimatedSizeLabel: string;
  gapCount: number;
  missingDurationSec: number;
  hasCoverage: boolean;
}

export function isSafeTimelineCameraSlug(camera: string): boolean {
  return (
    typeof camera === "string" &&
    camera.length > 0 &&
    camera.length <= 64 &&
    camera !== "." &&
    camera !== ".." &&
    !camera.includes("..") &&
    /^[A-Za-z0-9_-]+$/.test(camera)
  );
}

/**
 * Validates `/api/preview/<camera>/start/<s>/end/<e>/frames` responses so only
 * same-origin frame URLs belonging to `camera` are retained, sorted ascending by `tsMs`.
 */
export function sanitizePreviewFrameListPayload(
  camera: string,
  payload: unknown
): TimelineFrameIndexEntry[] {
  const cleanCam = (camera || "").trim();
  if (!isSafeTimelineCameraSlug(cleanCam)) return [];
  const rawArr = Array.isArray(payload)
    ? payload
    : Array.isArray((payload as { frames?: unknown } | null)?.frames)
    ? ((payload as { frames: unknown[] }).frames ?? [])
    : [];
  const expectedPrefix = `/api/preview/${cleanCam}/frame/`;
  const out: TimelineFrameIndexEntry[] = [];
  for (const item of rawArr) {
    const rawUrl =
      typeof (item as { url?: unknown } | null)?.url === "string"
        ? ((item as { url: string }).url || "").trim()
        : "";
    if (!rawUrl.startsWith(expectedPrefix)) continue;
    const rest = rawUrl.slice(expectedPrefix.length);
    const m = /^(\d{1,16})\.jpg$/.exec(rest);
    if (!m) continue;
    const tsMs = Number(m[1]);
    if (!Number.isFinite(tsMs) || tsMs <= 0) continue;
    const safeUrl = resolveSafeMediaUrl(rawUrl, {
      kind: "thumbnail",
      fallbackUrl: "",
      allowedPrefixes: ["/api/preview/"],
      rejectSpaces: true,
    });
    if (!safeUrl) continue;
    out.push({ ts: tsMs / 1000, tsMs, url: safeUrl });
    if (out.length >= 512) break;
  }
  out.sort((a, b) => a.tsMs - b.tsMs);
  return out;
}

function findNearestIndexedFrame(
  frames: ReadonlyArray<TimelineFrameIndexEntry>,
  targetMs: number
): TimelineFrameIndexEntry | null {
  if (frames.length === 0 || !Number.isFinite(targetMs)) return null;
  let lo = 0;
  let hi = frames.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >>> 1;
    const cur = frames[mid].tsMs;
    if (cur === targetMs) return frames[mid];
    if (cur < targetMs) lo = mid + 1;
    else hi = mid - 1;
  }
  const before = hi >= 0 ? frames[hi] : null;
  const after = lo < frames.length ? frames[lo] : null;
  if (before && after) {
    return targetMs - before.tsMs <= after.tsMs - targetMs ? before : after;
  }
  return before ?? after;
}

/**
 * Resolves a timestamp-accurate thumbnail URL for `targetTsSec`:
 * - Returns `null` when `hasRecording === false` and no indexed frame is within ±2s
 *   (never showing a stale event thumbnail or live snapshot over an offline gap).
 * - Uses `PreviewFrameStore` indexed frames (`0ms`, zero FFmpeg) when close enough.
 * - Falls back to `/api/preview/<camera>/at/<bucketMs>.jpg` (1s hover bucket or 250ms trim bucket).
 */
export function resolveTimelineHoverThumbnailUrl(
  camera: string,
  targetTsSec: number,
  indexedFrames: ReadonlyArray<TimelineFrameIndexEntry>,
  options?: {
    fineTrim?: boolean;
    hasRecording?: boolean;
  }
): string | null {
  const cleanCam = (camera || "").trim();
  if (
    !isSafeTimelineCameraSlug(cleanCam) ||
    typeof targetTsSec !== "number" ||
    !Number.isFinite(targetTsSec) ||
    targetTsSec <= 0
  ) {
    return null;
  }
  const targetMs = Math.round(targetTsSec * 1000);
  const fineTrim = Boolean(options?.fineTrim);
  const hasRecording = options?.hasRecording ?? true;
  const expectedFramePrefix = `/api/preview/${encodeURIComponent(cleanCam)}/frame/`;
  const nearest = findNearestIndexedFrame(indexedFrames, targetMs);

  if (nearest && nearest.url.startsWith(expectedFramePrefix)) {
    const deltaMs = Math.abs(nearest.tsMs - targetMs);
    const maxIndexedDeltaMs = fineTrim
      ? 750
      : hasRecording
      ? 3500
      : 2000;
    if (deltaMs <= maxIndexedDeltaMs) {
      return nearest.url;
    }
  }

  if (!hasRecording) {
    return null;
  }

  const bucketStepMs = fineTrim ? 250 : 1000;
  const bucketMs = Math.max(
    bucketStepMs,
    Math.round(targetMs / bucketStepMs) * bucketStepMs
  );
  const candidate = `/api/preview/${encodeURIComponent(cleanCam)}/at/${bucketMs}.jpg`;
  const safe = resolveSafeMediaUrl(candidate, {
    kind: "thumbnail",
    fallbackUrl: "",
    allowedPrefixes: ["/api/preview/"],
    rejectSpaces: true,
  });
  return safe || null;
}

/**
 * Returns the `ReviewItem` that strictly contains `tsSec` (with a 1s boundary tolerance),
 * never matching a distant review minutes away.
 */
export function findExactContainingReview(
  reviews: ReadonlyArray<ReviewItem>,
  camera: string,
  realCamera: string,
  tsSec: number
): ReviewItem | null {
  if (!Array.isArray(reviews) || !Number.isFinite(tsSec)) return null;
  let best: ReviewItem | null = null;
  let bestSpan = Infinity;
  for (const r of reviews) {
    if (!r || !Number.isFinite(r.start_time)) continue;
    if (r.camera !== camera && r.camera !== realCamera && camera !== "all") {
      continue;
    }
    const end =
      typeof r.end_time === "number" &&
      Number.isFinite(r.end_time) &&
      r.end_time > r.start_time
        ? r.end_time
        : r.start_time + 15;
    if (tsSec >= r.start_time - 1 && tsSec <= end + 1) {
      const span = end - r.start_time;
      if (
        !best ||
        (r.severity === "alert" && best.severity !== "alert") ||
        (r.severity === best.severity && span < bestSpan)
      ) {
        best = r;
        bestSpan = span;
      }
    }
  }
  return best;
}

/**
 * Computes the new `customWinEnd` when panning the timeline via mouse wheel or drag,
 * without touching `playheadTs` (`currentTs`) or `[rangeStart, rangeEnd]`.
 */
export function computePannedWindowEnd(params: {
  curWinEnd: number;
  deltaSec: number;
  windowSpanSec: number;
  minArchiveSec: number;
  nowSec: number;
}): number | null {
  const { curWinEnd, deltaSec, windowSpanSec, minArchiveSec, nowSec } = params;
  const safeNow = Number.isFinite(nowSec) ? Math.floor(nowSec) : Math.floor(Date.now() / 1000);
  const safeSpan = Number.isFinite(windowSpanSec) && windowSpanSec >= 60 ? windowSpanSec : 300;
  const safeMin = Number.isFinite(minArchiveSec)
    ? Math.min(safeNow - safeSpan, Math.floor(minArchiveSec))
    : safeNow - 30 * 86400;
  const base = Number.isFinite(curWinEnd) ? curWinEnd : safeNow;
  const candidate = base + (Number.isFinite(deltaSec) ? deltaSec : 0);
  const clamped = Math.max(safeMin + safeSpan, Math.min(safeNow, Math.round(candidate)));
  if (clamped >= safeNow - 1.5) {
    return null;
  }
  return clamped;
}

/**
 * Computes `[ IN ... OUT ]` overlay geometry relative to `[winStart, winEnd]`,
 * hiding off-screen handles rather than pinning fake `[` or `]` handles at 0% / 100%.
 */
export function computeClipOverlayGeometry(
  rangeStart: number,
  rangeEnd: number,
  winStart: number,
  winEnd: number
): ClipOverlayGeometry {
  const span = winEnd - winStart;
  if (
    !Number.isFinite(rangeStart) ||
    !Number.isFinite(rangeEnd) ||
    !Number.isFinite(winStart) ||
    !Number.isFinite(winEnd) ||
    span <= 0 ||
    rangeEnd <= rangeStart
  ) {
    return {
      visible: false,
      leftPct: 0,
      widthPct: 0,
      showInHandle: false,
      showOutHandle: false,
      offscreenIn: null,
      offscreenOut: null,
    };
  }

  const offscreenIn: "left" | "right" | null =
    rangeStart < winStart ? "left" : rangeStart > winEnd ? "right" : null;
  const offscreenOut: "left" | "right" | null =
    rangeEnd < winStart ? "left" : rangeEnd > winEnd ? "right" : null;

  const overlaps = rangeEnd > winStart && rangeStart < winEnd;
  if (!overlaps) {
    return {
      visible: false,
      leftPct: rangeEnd <= winStart ? 0 : 100,
      widthPct: 0,
      showInHandle: false,
      showOutHandle: false,
      offscreenIn,
      offscreenOut,
    };
  }

  const clampedStart = Math.max(winStart, Math.min(winEnd, rangeStart));
  const clampedEnd = Math.max(winStart, Math.min(winEnd, rangeEnd));
  const leftPct = ((clampedStart - winStart) / span) * 100;
  const widthPct = Math.max(0.4, ((clampedEnd - clampedStart) / span) * 100);

  return {
    visible: true,
    leftPct,
    widthPct,
    showInHandle: offscreenIn === null,
    showOutHandle: offscreenOut === null,
    offscreenIn,
    offscreenOut,
  };
}

export interface HandleDragCursorState {
  lastClientX: number;
  currentTs: number;
}

/**
 * Advances a dragged `IN` or `OUT` handle incrementally from `prevState`:
 * - Holding `Shift` scales movement by `0.15x` and preserves `0.25s` quarter-second granularity.
 * - Releasing `Shift` mid-drag resumes `1.0x` sensitivity from the handle's current position
 *   without ever teleporting to raw `clientX`.
 */
export function stepHandleDragTs(params: {
  clientX: number;
  rectLeft: number;
  rectWidth: number;
  winStart: number;
  winEnd: number;
  shiftKey: boolean;
  prevState: HandleDragCursorState | null;
}): HandleDragCursorState {
  const { clientX, rectLeft, rectWidth, winStart, winEnd, shiftKey, prevState } =
    params;
  const span = Math.max(60, winEnd - winStart);
  const safeWidth = Math.max(1, rectWidth);
  if (!prevState || !Number.isFinite(prevState.currentTs)) {
    const frac = Math.max(0, Math.min(1, (clientX - rectLeft) / safeWidth));
    const initTs = winStart + frac * span;
    const quantized = shiftKey
      ? Math.round(initTs * 4) / 4
      : Math.round(initTs);
    return { lastClientX: clientX, currentTs: quantized };
  }
  const dx = clientX - prevState.lastClientX;
  const scale = shiftKey ? 0.15 : 1.0;
  const rawNext = prevState.currentTs + (dx / safeWidth) * span * scale;
  return {
    lastClientX: clientX,
    currentTs: rawNext,
  };
}

export function quantizeClipBoundaryTs(
  ts: number,
  fineQuarterSec: boolean,
  minBoundSec: number,
  maxBoundSec: number
): number {
  const clamped = Math.max(minBoundSec, Math.min(maxBoundSec, ts));
  return fineQuarterSec
    ? Math.round(clamped * 4) / 4
    : Math.round(clamped);
}

/**
 * Computes dragged handle timestamp supporting `Shift` fine-precision trim (`0.15x` scale).
 */
export function computePrecisionHandleTs(params: {
  clientX: number;
  rectLeft: number;
  rectWidth: number;
  winStart: number;
  winEnd: number;
  shiftKey: boolean;
  fineAnchor?: { startClientX: number; startTs: number } | null;
}): number {
  const { clientX, rectLeft, rectWidth, winStart, winEnd, shiftKey, fineAnchor } = params;
  const span = Math.max(60, winEnd - winStart);
  const safeWidth = Math.max(1, rectWidth);
  if (shiftKey && fineAnchor && Number.isFinite(fineAnchor.startTs)) {
    const dx = clientX - fineAnchor.startClientX;
    const rawDeltaSec = (dx / safeWidth) * span * 0.15;
    return Math.round((fineAnchor.startTs + rawDeltaSec) * 4) / 4;
  }
  const frac = Math.max(0, Math.min(1, (clientX - rectLeft) / safeWidth));
  return Math.round(winStart + frac * span);
}

/**
 * Updates `IN` or `OUT` via keyboard (`I` / `O`) or `ClockInput`, automatically re-anchoring
 * the opposite boundary when the user navigated >24h away or crossed the opposite marker.
 */
export function reanchorClipRangeBoundary(params: {
  which: "in" | "out";
  candidateTs: number;
  curStart: number;
  curEnd: number;
  minArchiveSec: number;
  nowSec: number;
  maxSpanSec?: number;
}): { start: number; end: number } | null {
  const {
    which,
    candidateTs,
    curStart,
    curEnd,
    minArchiveSec,
    nowSec,
    maxSpanSec = 86400,
  } = params;
  if (!Number.isFinite(candidateTs) || !Number.isFinite(nowSec)) return null;
  const clampedTs = Math.max(minArchiveSec, Math.min(nowSec, Math.round(candidateTs)));
  const prevSpan = Math.min(
    3600,
    Math.max(30, Number.isFinite(curEnd - curStart) ? Math.round(curEnd - curStart) : 60)
  );

  if (which === "in") {
    if (clampedTs < curEnd && curEnd - clampedTs <= maxSpanSec) {
      return { start: clampedTs, end: Math.ceil(curEnd) };
    }
    const reanchoredEnd = Math.min(nowSec, clampedTs + prevSpan);
    if (reanchoredEnd <= clampedTs) {
      const shiftedStart = Math.max(minArchiveSec, nowSec - 30);
      return nowSec > shiftedStart ? { start: shiftedStart, end: nowSec } : null;
    }
    return { start: clampedTs, end: reanchoredEnd };
  }

  if (clampedTs > curStart && clampedTs - curStart <= maxSpanSec) {
    return { start: Math.floor(curStart), end: clampedTs };
  }
  const reanchoredStart = Math.max(minArchiveSec, clampedTs - prevSpan);
  if (clampedTs <= reanchoredStart) {
    const shiftedEnd = Math.min(nowSec, reanchoredStart + 30);
    return shiftedEnd > reanchoredStart
      ? { start: reanchoredStart, end: shiftedEnd }
      : null;
  }
  return { start: reanchoredStart, end: clampedTs };
}

export function mergeDeduplicatedSegments(
  primary: ReadonlyArray<RecordingSegment>,
  extra: ReadonlyArray<RecordingSegment>
): RecordingSegment[] {
  if (extra.length === 0) return [...primary];
  if (primary.length === 0) return [...extra];
  const byId = new Map<string, RecordingSegment>();
  for (const s of primary) {
    if (s && s.id) byId.set(s.id, s);
  }
  for (const s of extra) {
    if (s && s.id && !byId.has(s.id)) {
      byId.set(s.id, s);
    }
  }
  return Array.from(byId.values()).sort((a, b) => a.start_time - b.start_time);
}

/**
 * Parses either `"HH:MM[:SS]"` (on `referenceTs`'s local date) or multi-day
 * `"YYYY-MM-DD HH:MM[:SS]"` / `"MM-DD HH:MM[:SS]"` into a Unix timestamp (seconds).
 */
export function parseFlexibleClipDateTime(
  text: string,
  referenceTs: number
): number | null {
  if (!Number.isFinite(referenceTs)) return null;
  const raw = (text || "").trim();
  if (!raw) return null;

  const fullMatch =
    /^(?:(\d{4})-)?(\d{1,2})-(\d{1,2})[T\s]+(\d{1,2}):(\d{2})(?::(\d{2}))?$/.exec(
      raw
    );
  if (fullMatch) {
    const refDate = new Date(referenceTs * 1000);
    if (Number.isNaN(refDate.getTime())) return null;
    const year = fullMatch[1] ? Number(fullMatch[1]) : refDate.getFullYear();
    const month = Number(fullMatch[2]);
    const day = Number(fullMatch[3]);
    const hh = Number(fullMatch[4]);
    const mm = Number(fullMatch[5]);
    const ss = fullMatch[6] === undefined ? 0 : Number(fullMatch[6]);
    if (
      month < 1 ||
      month > 12 ||
      day < 1 ||
      day > 31 ||
      hh > 23 ||
      mm > 59 ||
      ss > 59
    ) {
      return null;
    }
    const dt = new Date(year, month - 1, day, hh, mm, ss, 0);
    if (
      Number.isNaN(dt.getTime()) ||
      dt.getFullYear() !== year ||
      dt.getMonth() !== month - 1 ||
      dt.getDate() !== day
    ) {
      return null;
    }
    return Math.floor(dt.getTime() / 1000);
  }

  const clockMatch = /^(\d{1,2}):(\d{2})(?::(\d{2}))?$/.exec(raw);
  if (!clockMatch) return null;
  const hh = Number(clockMatch[1]);
  const mm = Number(clockMatch[2]);
  const ss = clockMatch[3] === undefined ? 0 : Number(clockMatch[3]);
  if (hh > 23 || mm > 59 || ss > 59) return null;
  const d = new Date(referenceTs * 1000);
  if (Number.isNaN(d.getTime())) return null;
  d.setHours(hh, mm, ss, 0);
  return Math.floor(d.getTime() / 1000);
}

export function isSameLocalCalendarDay(tsA: number, tsB: number): boolean {
  if (!Number.isFinite(tsA) || !Number.isFinite(tsB)) return true;
  const a = new Date(tsA * 1000);
  const b = new Date(tsB * 1000);
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

export function formatClipBoundaryInput(
  unixSec: number,
  includeDate: boolean
): string {
  if (!Number.isFinite(unixSec)) return "--:--:--";
  const d = new Date(unixSec * 1000);
  if (Number.isNaN(d.getTime())) return "--:--:--";
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  if (!includeDate) {
    return `${hh}:${mm}:${ss}`;
  }
  const yyyy = d.getFullYear();
  const mo = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return `${yyyy}-${mo}-${dd} ${hh}:${mm}:${ss}`;
}

export function formatEstimatedClipSize(sizeMb: number): string {
  if (!Number.isFinite(sizeMb) || sizeMb <= 0) return "";
  if (sizeMb >= 1024) {
    return `~${(sizeMb / 1024).toFixed(2)} GB`;
  }
  if (sizeMb >= 10) {
    return `~${Math.round(sizeMb)} MB`;
  }
  return `~${sizeMb.toFixed(1)} MB`;
}

/**
 * Computes pre-flight telemetry for `[rangeStart, rangeEnd]`:
 * overlapping segment count, proportional MP4 size estimate, and unrecorded gaps.
 */
export function computeClipPreFlightSummary(
  segments: ReadonlyArray<RecordingSegment>,
  rangeStart: number,
  rangeEnd: number
): ClipPreFlightSummary {
  const durationSec =
    Number.isFinite(rangeStart) && Number.isFinite(rangeEnd) && rangeEnd > rangeStart
      ? Math.round(rangeEnd - rangeStart)
      : 0;
  if (durationSec <= 0 || !Array.isArray(segments) || segments.length === 0) {
    return {
      durationSec,
      segmentCount: 0,
      estimatedSizeMb: 0,
      estimatedSizeLabel: "",
      gapCount: durationSec > 0 ? 1 : 0,
      missingDurationSec: durationSec,
      hasCoverage: false,
    };
  }

  const overlapping = segments
    .filter(
      (s) =>
        s &&
        Number.isFinite(s.start_time) &&
        Number.isFinite(s.end_time) &&
        s.end_time > rangeStart &&
        s.start_time < rangeEnd
    )
    .sort((a, b) => a.start_time - b.start_time);

  if (overlapping.length === 0) {
    return {
      durationSec,
      segmentCount: 0,
      estimatedSizeMb: 0,
      estimatedSizeLabel: "",
      gapCount: 1,
      missingDurationSec: durationSec,
      hasCoverage: false,
    };
  }

  let estimatedSizeMb = 0;
  let gapCount = 0;
  let missingDurationSec = 0;
  let cursor = rangeStart;

  for (const seg of overlapping) {
    const segStart = seg.start_time;
    const segEnd = Math.max(segStart + 0.1, seg.end_time);
    const gapBefore = segStart - cursor;
    if (gapBefore > 20) {
      gapCount += 1;
      missingDurationSec += Math.round(gapBefore);
    }
    const ovStart = Math.max(rangeStart, segStart);
    const ovEnd = Math.min(rangeEnd, segEnd);
    if (ovEnd > ovStart) {
      const segDur =
        typeof seg.duration === "number" && Number.isFinite(seg.duration) && seg.duration > 0
          ? seg.duration
          : segEnd - segStart;
      const frac = Math.min(1, (ovEnd - ovStart) / Math.max(0.5, segDur));
      if (
        typeof seg.segment_size === "number" &&
        Number.isFinite(seg.segment_size) &&
        seg.segment_size > 0
      ) {
        estimatedSizeMb += seg.segment_size * frac;
      }
    }
    if (segEnd > cursor) {
      cursor = segEnd;
    }
  }

  const tailGap = rangeEnd - cursor;
  if (tailGap > 20) {
    gapCount += 1;
    missingDurationSec += Math.round(tailGap);
  }

  return {
    durationSec,
    segmentCount: overlapping.length,
    estimatedSizeMb,
    estimatedSizeLabel: formatEstimatedClipSize(estimatedSizeMb),
    gapCount,
    missingDurationSec,
    hasCoverage: true,
  };
}

const VALID_JOB_STATUSES: ReadonlySet<ExportJobStatus> = new Set([
  "queued",
  "running",
  "completed",
  "failed",
  "cancelled",
]);

export function parseExportJobPayload(raw: unknown): ExportJobItem | null {
  if (!raw || typeof raw !== "object") return null;
  const obj = raw as Record<string, unknown>;
  const id = typeof obj.id === "string" ? obj.id.trim() : "";
  const camera = typeof obj.camera === "string" ? obj.camera.trim() : "";
  const status = typeof obj.status === "string" ? (obj.status as ExportJobStatus) : null;
  if (!id || !camera || !status || !VALID_JOB_STATUSES.has(status)) {
    return null;
  }
  const start_time = Number(obj.start_time);
  const end_time = Number(obj.end_time);
  const progress_pct = Number(obj.progress_pct);
  const created_at = Number(obj.created_at);
  const updated_at = Number(obj.updated_at);
  return {
    id,
    camera,
    name: typeof obj.name === "string" && obj.name.trim() ? obj.name : id,
    start_time: Number.isFinite(start_time) ? start_time : 0,
    end_time: Number.isFinite(end_time) ? end_time : 0,
    include_audio: obj.include_audio !== false,
    status,
    progress_pct: Number.isFinite(progress_pct)
      ? Math.max(0, Math.min(100, progress_pct))
      : 0,
    created_at: Number.isFinite(created_at) ? created_at : 0,
    updated_at: Number.isFinite(updated_at) ? updated_at : 0,
    video_url: typeof obj.video_url === "string" ? obj.video_url : null,
    thumb_url: typeof obj.thumb_url === "string" ? obj.thumb_url : null,
    error: typeof obj.error === "string" ? obj.error : null,
  };
}

export function reconcileExportJobList(
  prev: ReadonlyArray<ExportJobItem>,
  incoming: ExportJobItem
): ExportJobItem[] {
  const map = new Map<string, ExportJobItem>();
  for (const item of prev) {
    map.set(item.id, item);
  }
  map.set(incoming.id, incoming);
  const merged = Array.from(map.values()).sort(
    (a, b) => b.created_at - a.created_at || b.id.localeCompare(a.id)
  );
  return merged.slice(0, 64);
}
