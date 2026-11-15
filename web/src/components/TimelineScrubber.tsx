import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  ArrowDownToLine,
  ChevronLeft,
  ChevronRight,
  Download,
  Grid,
  Play,
  Scissors,
  SkipBack,
  SkipForward,
  Square,
  Volume2,
  VolumeX,
  X,
} from "lucide-react";
import {
  RecordingDaySummary,
  RecordingHourSummary,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import { RecordingCalendarPopover } from "./RecordingCalendarPopover";
import { TacticalCameraPicker } from "./TacticalCameraPicker";
import { formatDuration, resolveEffectiveTheme } from "../utils/format";
import { resolveCanvasThemePalette } from "../utils/themeTokens";
import { resolveRealCameraSource, useNvrStore } from "../store/useNvrStore";
import { useI18n } from "../i18n";
import {
  createArchiveMetadataCache,
  MAX_ARCHIVE_EXPORT_SPAN_SEC,
  useArchiveMetadataCache,
} from "../utils/archivePlayback";
import { resolveSafeMediaUrl } from "../utils/mediaUrlPolicy";
import {
  HandleDragCursorState,
  TimelineFrameIndexEntry,
  computeClipOverlayGeometry,
  computeClipPreFlightSummary,
  computePannedWindowEnd,
  findExactContainingReview,
  formatClipBoundaryInput,
  isSameLocalCalendarDay,
  mergeDeduplicatedSegments,
  parseFlexibleClipDateTime,
  quantizeClipBoundaryTs,
  reanchorClipRangeBoundary,
  resolveTimelineHoverThumbnailUrl,
  sanitizePreviewFrameListPayload,
  stepHandleDragTs,
} from "../utils/exportClipFlow";

export type TimelineZoomMinutes = 5 | 15 | 60 | 360 | 1440;

const ZOOM_LEVELS: TimelineZoomMinutes[] = [5, 15, 60, 360, 1440];

export interface TimelineTrimFeedbackState {
  mode:
    | "dragging_in"
    | "dragging_out"
    | "settled_in"
    | "settled_out"
    | "preview_clip";
  targetTs: number;
  rangeStart: number;
  rangeEnd: number;
  previewFrameUrl: string | null;
  badgeLabel: string;
}

interface TimelineScrubberProps {
  camera: string;
  cameras?: string[];
  stageCameras?: string[];
  cameraGroups?: Record<string, import("../types/nvr").CameraGroupConfig>;
  onSelectCamera?: (cam: string) => void;
  onFocusCamera?: (cam: string) => void;
  segments: RecordingSegment[];
  reviews: ReviewItem[];
  currentTs: number | null; // null = LIVE EDGE
  onSeek: (ts: number | null) => void;
  onExportRange: (
    camera: string,
    startSec: number,
    endSec: number,
    label: string,
    includeAudio?: boolean
  ) => Promise<{ exportId?: string; videoUrl?: string; queued?: boolean } | void>;
  titlePrefix?: string;
  defaultRangeMode?: boolean;
  /** Opens Export clip mode with this range; a new `nonce` re-opens it. */
  clipRequest?: TimelineClipRequest | null;
  /** Shown as "Open in Exports" after a clip is saved or queued. */
  onOpenExports?: () => void;
  /** Optional controlled audio toggle for clip export. */
  includeAudio?: boolean;
  onIncludeAudioChange?: (includeAudio: boolean) => void;
  /** Real-time trim feedback for the primary video monitor (IN/OUT scrub, pause-on-cut, and clip loop). */
  onTrimFeedback?: (state: TimelineTrimFeedbackState | null) => void;
}

export interface TimelineClipRequest {
  camera: string;
  start: number;
  end: number;
  nonce: number;
}

/** Zoom that keeps a clip of `spanSec` comfortably visible while editing it. */
export function resolveClipZoom(spanSec: number): TimelineZoomMinutes {
  const span = Number.isFinite(spanSec) && spanSec > 0 ? spanSec : 30;
  if (span <= 4 * 60) return 5;
  if (span <= 12 * 60) return 15;
  if (span <= 50 * 60) return 60;
  return 360;
}

/**
 * Parses "HH:MM" or "HH:MM:SS" (24 h) as a time on the same local day as
 * `referenceTs`. Returns null for anything that is not a valid clock time.
 */
export function parseClockOnDay(text: string, referenceTs: number): number | null {
  if (!Number.isFinite(referenceTs)) return null;
  const m = /^\s*(\d{1,2}):(\d{2})(?::(\d{2}))?\s*$/.exec(text || "");
  if (!m) return null;
  const hh = Number(m[1]);
  const mm = Number(m[2]);
  const ss = m[3] === undefined ? 0 : Number(m[3]);
  if (hh > 23 || mm > 59 || ss > 59) return null;
  const d = new Date(referenceTs * 1000);
  if (Number.isNaN(d.getTime())) return null;
  d.setHours(hh, mm, ss, 0);
  return Math.floor(d.getTime() / 1000);
}

/**
 * Clip range for "This event": the camera's review containing `anchorTs`, or
 * else the latest one that started within the previous 10 minutes, padded by
 * `padSec` and never extending past `nowSec`.
 */
export function resolveEventClipRange(
  reviews: ReviewItem[],
  camera: string,
  anchorTs: number,
  nowSec: number,
  padSec = 5,
  realCamera?: string
): { start: number; end: number } | null {
  if (!Array.isArray(reviews) || !Number.isFinite(anchorTs)) return null;
  const safePad =
    typeof padSec === "number" && Number.isFinite(padSec) && padSec >= 0
      ? padSec
      : 5;
  const own = reviews.filter(
    (r) =>
      r &&
      (r.camera === camera || Boolean(realCamera && r.camera === realCamera)) &&
      Number.isFinite(r.start_time)
  );
  const endOf = (r: ReviewItem) =>
    typeof r.end_time === "number" &&
    Number.isFinite(r.end_time) &&
    r.end_time > r.start_time
      ? r.end_time
      : r.start_time + 15;
  let pick =
    own.find((r) => r.start_time <= anchorTs && anchorTs <= endOf(r)) || null;
  if (!pick) {
    pick =
      own
        .filter((r) => r.start_time <= anchorTs && anchorTs - r.start_time <= 600)
        .sort((a, b) => b.start_time - a.start_time)[0] || null;
  }
  if (!pick) return null;
  const start = Math.floor(pick.start_time - safePad);
  const end = Math.ceil(
    Math.min(Number.isFinite(nowSec) ? nowSec : Infinity, endOf(pick) + safePad)
  );
  return end > start ? { start, end } : null;
}

/** Event lanes on the seek track: alerts on top, detections right below. */
export function resolveReviewLane(severity: string): { y: number; h: number } {
  return severity === "alert" ? { y: 2, h: 5 } : { y: 9, h: 5 };
}

/** Text input that edits a unix timestamp as `HH:MM:SS` or `YYYY-MM-DD HH:MM:SS` for multi-day clips. */
const ClockInput: React.FC<{
  value: number;
  includeDate: boolean;
  ariaLabel: string;
  onCommit: (ts: number) => boolean;
}> = ({ value, includeDate, ariaLabel, onCommit }) => {
  const shown = formatClipBoundaryInput(value, includeDate);
  const [draft, setDraft] = useState<string>(shown);
  const [invalid, setInvalid] = useState(false);
  useEffect(() => {
    setDraft(shown);
    setInvalid(false);
  }, [shown]);
  const commit = () => {
    const ts = parseFlexibleClipDateTime(draft, value);
    if (ts === null || !onCommit(ts)) {
      setInvalid(true);
      setDraft(shown);
      return;
    }
    setInvalid(false);
  };
  return (
    <input
      className="nvr-input mono sm nvr-clock-input"
      style={includeDate ? { width: 152 } : undefined}
      aria-label={ariaLabel}
      aria-invalid={invalid}
      title={
        includeDate
          ? "YYYY-MM-DD HH:MM:SS (or HH:MM:SS)"
          : "HH:MM:SS (or YYYY-MM-DD HH:MM:SS to cross days)"
      }
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          commit();
        } else if (e.key === "Escape") {
          setDraft(shown);
          (e.target as HTMLInputElement).blur();
        }
      }}
    />
  );
};

function formatHHMM(unixSec: number): string {
  if (!Number.isFinite(unixSec)) return "--:--";
  const d = new Date(unixSec * 1000);
  if (Number.isNaN(d.getTime())) return "--:--";
  return d.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

function formatHHMMSS(unixSec: number): string {
  if (!Number.isFinite(unixSec)) return "--:--:--";
  const d = new Date(unixSec * 1000);
  if (Number.isNaN(d.getTime())) return "--:--:--";
  return d.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

function formatFullDateTime(unixSec: number): string {
  if (!Number.isFinite(unixSec)) return "--:--:--";
  const d = new Date(unixSec * 1000);
  if (Number.isNaN(d.getTime())) return "--:--:--";
  return d.toLocaleString([], {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

function parseStrictYmd(dayStr: string): [number, number, number] | null {
  const m = (dayStr || "").trim().match(/^(\d{4})-(\d{2})-(\d{2})$/);
  if (!m) return null;
  const year = Number(m[1]);
  const month = Number(m[2]);
  const day = Number(m[3]);
  if (
    !Number.isFinite(year) ||
    !Number.isFinite(month) ||
    !Number.isFinite(day) ||
    month < 1 ||
    month > 12 ||
    day < 1 ||
    day > 31
  ) {
    return null;
  }
  const d = new Date(year, month - 1, day);
  if (
    Number.isNaN(d.getTime()) ||
    d.getFullYear() !== year ||
    d.getMonth() !== month - 1 ||
    d.getDate() !== day
  ) {
    return null;
  }
  return [year, month, day];
}

export function sanitizeTimelineThumbUrl(
  rawThumbPath: string | undefined | null,
  fallbackCamera: string
): string {
  return resolveSafeMediaUrl(rawThumbPath, {
    kind: "thumbnail",
    fallbackCamera,
    allowedPrefixes: ["/clips/", "/api/", "/thumbs/", "/previews/"],
    rejectSpaces: true,
  });
}

function formatDayShort(dayStr: string): string {
  const parsed = parseStrictYmd(dayStr);
  if (!parsed) return dayStr;
  const d = new Date(parsed[0], parsed[1] - 1, parsed[2]);
  return d.toLocaleDateString([], {
    weekday: "short",
    month: "short",
    day: "2-digit",
  });
}

function toLocalDateInputValue(unixSec: number): string {
  const safeSec = Number.isFinite(unixSec)
    ? unixSec
    : Math.floor(Date.now() / 1000);
  const d = new Date(safeSec * 1000);
  if (Number.isNaN(d.getTime())) return "1970-01-01";
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function toLocalTimeInputValue(unixSec: number): string {
  const safeSec = Number.isFinite(unixSec)
    ? unixSec
    : Math.floor(Date.now() / 1000);
  const d = new Date(safeSec * 1000);
  if (Number.isNaN(d.getTime())) return "00:00:00";
  const h = String(d.getHours()).padStart(2, "0");
  const m = String(d.getMinutes()).padStart(2, "0");
  const s = String(d.getSeconds()).padStart(2, "0");
  return `${h}:${m}:${s}`;
}

export function parseLocalDayBounds(
  selectedDay: string,
  nowSec: number
): { dayStart: number; dayEnd: number; isToday: boolean } {
  const safeNow = Number.isFinite(nowSec)
    ? Math.floor(nowSec)
    : Math.floor(Date.now() / 1000);
  const todayStr = toLocalDateInputValue(safeNow);
  const parsed = parseStrictYmd(selectedDay);
  if (parsed) {
    const startMs = new Date(
      parsed[0],
      parsed[1] - 1,
      parsed[2],
      0,
      0,
      0
    ).getTime();
    if (Number.isFinite(startMs)) {
      const start = Math.floor(startMs / 1000);
      const fullEnd = start + 86399;
      const isToday = selectedDay.trim() === todayStr;
      const end = isToday
        ? Math.max(start + 300, Math.min(safeNow, fullEnd))
        : fullEnd;
      return {
        dayStart: start,
        dayEnd: end,
        isToday,
      };
    }
  }
  const d = new Date(safeNow * 1000);
  d.setHours(0, 0, 0, 0);
  const fallbackStart = Math.floor(d.getTime() / 1000);
  const start = Number.isFinite(fallbackStart) ? fallbackStart : safeNow - 3600;
  return {
    dayStart: start,
    dayEnd: Math.max(start + 300, safeNow),
    isToday: true,
  };
}

export function computeClientXTimestamp(
  clientX: number,
  rectLeft: number,
  rectWidth: number,
  winStart: number,
  winEnd: number
): number {
  const safeStart = Number.isFinite(winStart) ? winStart : 0;
  const safeEnd =
    Number.isFinite(winEnd) && winEnd > safeStart ? winEnd : safeStart + 300;
  if (
    !Number.isFinite(rectWidth) ||
    rectWidth <= 0 ||
    !Number.isFinite(clientX) ||
    !Number.isFinite(rectLeft)
  ) {
    return Math.round(safeEnd);
  }
  const span = safeEnd - safeStart;
  const frac = Math.max(0, Math.min(1, (clientX - rectLeft) / rectWidth));
  return Math.round(safeStart + frac * span);
}

export const TimelineScrubber: React.FC<TimelineScrubberProps> = ({
  camera,
  cameras,
  stageCameras,
  cameraGroups,
  onSelectCamera,
  onFocusCamera,
  segments: initialSegments,
  reviews: initialReviews,
  currentTs,
  onSeek,
  onExportRange,
  titlePrefix,
  defaultRangeMode = false,
  clipRequest = null,
  onOpenExports,
  includeAudio: controlledIncludeAudio,
  onIncludeAudioChange,
  onTrimFeedback,
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const trackRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const onTrimFeedbackRef = useRef(onTrimFeedback);
  onTrimFeedbackRef.current = onTrimFeedback;

  const themeMode = useNvrStore((s) => s.themeMode);
  const highContrast = useNvrStore((s) => s.highContrast);
  const configuredCameras = useNvrStore((s) => s.config?.cameras);
  const realCamera = useMemo(
    () => resolveRealCameraSource(camera, configuredCameras),
    [camera, configuredCameras]
  );
  const contextMetadataCache = useArchiveMetadataCache();
  const localMetadataCache = useMemo(() => createArchiveMetadataCache(), []);
  const archiveMetadataCache = contextMetadataCache ?? localMetadataCache;
  const effectiveTheme = resolveEffectiveTheme(themeMode);

  const [trackWidth, setTrackWidth] = useState<number>(800);
  const [zoomMin, setZoomMin] = useState<TimelineZoomMinutes>(60);
  const [nowSec, setNowSec] = useState<number>(() =>
    Math.floor(Date.now() / 1000)
  );
  // Explicit window end timestamp (null = locked to live `nowSec`)
  const [customWinEnd, setCustomWinEnd] = useState<number | null>(null);

  // Multi-Day Archive Summary & On-Demand Historical Segments/Reviews/Preview Frames
  const [daysSummary, setDaysSummary] = useState<RecordingDaySummary[]>([]);
  const [selectedDay, setSelectedDay] = useState<string>(() =>
    toLocalDateInputValue(Math.floor(Date.now() / 1000))
  );
  const [selectedHour, setSelectedHour] = useState<string | null>(null);
  const [showArchiveDeck, setShowArchiveDeck] = useState<boolean>(false);
  const [showStoryboard, setShowStoryboard] = useState<boolean>(false);
  const [windowSegments, setWindowSegments] =
    useState<RecordingSegment[]>(initialSegments);
  const [clipRangeSegments, setClipRangeSegments] = useState<
    RecordingSegment[]
  >([]);
  const [windowReviews, setWindowReviews] =
    useState<ReviewItem[]>(initialReviews);
  const [indexedFrames, setIndexedFrames] = useState<TimelineFrameIndexEntry[]>(
    []
  );
  const [hoveredStoryboardId, setHoveredStoryboardId] = useState<string | null>(
    null
  );

  // Hover state on fine timeline track
  const [hoverTs, setHoverTs] = useState<number | null>(null);
  const [hoverX, setHoverX] = useState<number>(0);
  const [debouncedTier2ThumbUrl, setDebouncedTier2ThumbUrl] = useState<
    string | null
  >(null);
  const [failedThumbUrl, setFailedThumbUrl] = useState<string | null>(null);

  // Range Export Selector state (`[ IN ... OUT ]`)
  const [rangeMode, setRangeMode] = useState<boolean>(defaultRangeMode);
  const [rangeStart, setRangeStart] = useState<number>(
    () => (currentTs ?? Math.floor(Date.now() / 1000)) - 90
  );
  const [rangeEnd, setRangeEnd] = useState<number>(
    () => (currentTs ?? Math.floor(Date.now() / 1000)) - 30
  );
  const [localIncludeAudio, setLocalIncludeAudio] = useState<boolean>(true);
  const effectiveIncludeAudio = controlledIncludeAudio ?? localIncludeAudio;
  const setEffectiveIncludeAudio = (next: boolean) => {
    setLocalIncludeAudio(next);
    onIncludeAudioChange?.(next);
  };

  const [clipLabel, setClipLabel] = useState<string>("");
  const [exportBusy, setExportBusy] = useState<boolean>(false);
  const [readyDownloadUrl, setReadyDownloadUrl] = useState<string | null>(null);
  const [exportSaved, setExportSaved] = useState<boolean>(false);
  const [exportQueued, setExportQueued] = useState<boolean>(false);
  const [isPreviewingClip, setIsPreviewingClip] = useState<boolean>(false);
  const [fineShiftActive, setFineShiftActive] = useState<boolean>(false);

  const [draggingTarget, setDraggingTarget] = useState<
    "track" | "in" | "out" | null
  >(null);
  const dragOriginRef = useRef<{
    startX: number;
    startWinEnd: number;
    moved: boolean;
  } | null>(null);
  const handleDragCursorRef = useRef<HandleDragCursorState | null>(null);
  const trimDebounceTimerRef = useRef<number | null>(null);
  const lastDebouncedScrubUrlRef = useRef<string | null>(null);
  const activePointerElRef = useRef<HTMLElement | null>(null);
  const activePointerIdRef = useRef<number | null>(null);
  const wheelAnimRef = useRef<{
    rafId: number | null;
    curWinEnd: number;
    targetWinEnd: number;
  }>({
    rafId: null,
    curWinEnd: 0,
    targetWinEnd: 0,
  });

  const releaseActivePointerCapture = useCallback(() => {
    const pid = activePointerIdRef.current;
    const el = activePointerElRef.current;
    activePointerIdRef.current = null;
    activePointerElRef.current = null;
    if (pid !== null && el) {
      try {
        if (el.hasPointerCapture?.(pid)) {
          el.releasePointerCapture(pid);
        } else {
          el.releasePointerCapture?.(pid);
        }
      } catch {
        // ignore
      }
    }
  }, []);

  useEffect(() => {
    if (!showArchiveDeck || !showStoryboard) {
      setHoveredStoryboardId(null);
    }
  }, [showArchiveDeck, showStoryboard, camera]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    const clearHover = () => {
      setHoveredStoryboardId(null);
      setHoverTs(null);
    };
    const onVisChange = () => {
      if (
        typeof document !== "undefined" &&
        document.visibilityState !== "visible"
      ) {
        clearHover();
      }
    };
    window.addEventListener("blur", clearHover);
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", onVisChange);
    }
    return () => {
      window.removeEventListener("blur", clearHover);
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", onVisChange);
      }
    };
  }, []);

  useEffect(() => {
    return () => {
      if (
        wheelAnimRef.current.rafId !== null &&
        typeof window !== "undefined" &&
        typeof window.cancelAnimationFrame === "function"
      ) {
        window.cancelAnimationFrame(wheelAnimRef.current.rafId);
        wheelAnimRef.current.rafId = null;
      }
      if (trimDebounceTimerRef.current !== null && typeof window !== "undefined") {
        window.clearTimeout(trimDebounceTimerRef.current);
        trimDebounceTimerRef.current = null;
      }
      releaseActivePointerCapture();
    };
  }, [releaseActivePointerCapture]);

  // Reset per-camera transient trim & thumbnail state when switching camera channels
  useEffect(() => {
    setIndexedFrames([]);
    setClipRangeSegments([]);
    setFailedThumbUrl(null);
    setIsPreviewingClip(false);
    setReadyDownloadUrl(null);
    setExportSaved(false);
    setExportQueued(false);
    lastDebouncedScrubUrlRef.current = null;
    onTrimFeedbackRef.current?.(null);
  }, [camera, realCamera]);

  // Clear transient failed thumbnail flag when cursor leaves or segments refresh
  useEffect(() => {
    if (hoverTs === null) {
      setFailedThumbUrl(null);
    }
  }, [hoverTs]);
  useEffect(() => {
    setFailedThumbUrl(null);
  }, [windowSegments]);

  useEffect(() => {
    const timer = window.setInterval(() => {
      setNowSec(Math.floor(Date.now() / 1000));
    }, 2000);
    return () => window.clearInterval(timer);
  }, []);

  const dayBounds = useMemo(
    () => parseLocalDayBounds(selectedDay, nowSec),
    [selectedDay, nowSec]
  );

  // Earliest archive bound across loaded 30d summary (allowing continuous cross-midnight pan & multi-day clips)
  const minArchiveSec = useMemo(() => {
    let earliest = nowSec - 30 * 86400;
    for (const d of daysSummary) {
      const parsed = parseStrictYmd(d.day);
      if (parsed) {
        const ts = Math.floor(
          new Date(parsed[0], parsed[1] - 1, parsed[2], 0, 0, 0).getTime() /
            1000
        );
        if (Number.isFinite(ts) && ts < earliest) {
          earliest = ts;
        }
      }
    }
    return Math.min(earliest, dayBounds.dayStart - 86400);
  }, [daysSummary, nowSec, dayBounds.dayStart]);

  const windowSpanSec = Math.max(60, zoomMin * 60);
  const rawWinEnd = customWinEnd ?? (dayBounds.isToday ? nowSec : dayBounds.dayEnd);
  const winEnd = Math.max(
    minArchiveSec + windowSpanSec,
    Math.min(nowSec, Math.round(rawWinEnd))
  );
  const winStart = winEnd - windowSpanSec;

  // Sync selectedDay display badge when continuous panning crosses midnight
  useEffect(() => {
    if (customWinEnd === null) return;
    const midTs = Math.round((winStart + winEnd) / 2);
    const midDay = toLocalDateInputValue(midTs);
    if (midDay !== selectedDay) {
      setSelectedDay(midDay);
    }
  }, [customWinEnd, winStart, winEnd, selectedDay]);

  const clampArchiveTs = useCallback(
    (ts: number): number =>
      Math.max(minArchiveSec, Math.min(nowSec, Math.round(ts))),
    [minArchiveSec, nowSec]
  );

  const clampWinEndContinuous = useCallback(
    (candidateEnd: number): number | null =>
      computePannedWindowEnd({
        curWinEnd: candidateEnd,
        deltaSec: 0,
        windowSpanSec,
        minArchiveSec,
        nowSec,
      }),
    [windowSpanSec, minArchiveSec, nowSec]
  );

  // Re-center timeline window when parent seeks to a timestamp outside the current window
  useEffect(() => {
    if (currentTs === null || !Number.isFinite(currentTs)) return;
    const targetDay = toLocalDateInputValue(currentTs);
    if (targetDay !== selectedDay) {
      setSelectedDay(targetDay);
    }
    if (currentTs < winStart || currentTs > winEnd) {
      setCustomWinEnd(
        clampWinEndContinuous(currentTs + Math.round(windowSpanSec * 0.5))
      );
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [currentTs]);

  // Fetch 30-day recording summary for the active camera
  useEffect(() => {
    if (!realCamera) return;
    let cancelled = false;
    archiveMetadataCache
      .getSummary(realCamera, { configuredCameras })
      .then((arr) => {
        if (!cancelled && Array.isArray(arr)) {
          setDaysSummary(arr);
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [realCamera, archiveMetadataCache, configuredCameras]);

  // Decoupled viewport segment fetch: strictly bounded to <= 86400s so `/api/:cam/recordings`
  // never clamps past days to the trailing 24h!
  const viewportFetchBounds = useMemo(() => {
    const rawAfter = Math.min(dayBounds.dayStart, winStart);
    const rawBefore = Math.min(nowSec + 60, Math.max(dayBounds.dayEnd, winEnd));
    const bucketBefore = Math.ceil((rawBefore + 60) / 60) * 60;
    const clampedAfter = Math.max(rawAfter, bucketBefore - 86400);
    const bucketAfter = Math.floor(clampedAfter / 60) * 60;
    return { after: bucketAfter, before: bucketBefore };
  }, [dayBounds.dayStart, dayBounds.dayEnd, winStart, winEnd, nowSec]);

  const viewportFetchAfter = viewportFetchBounds.after;
  const viewportFetchBefore = viewportFetchBounds.before;

  useEffect(() => {
    if (!realCamera) return;
    let cancelled = false;

    Promise.all([
      archiveMetadataCache.getRecordings(
        realCamera,
        { after: viewportFetchAfter, before: viewportFetchBefore },
        { configuredCameras }
      ),
      fetch(
        `/api/review?cameras=${encodeURIComponent(
          realCamera
        )}&after=${viewportFetchAfter}&before=${viewportFetchBefore}&limit=300`,
        { credentials: "include" }
      ).then((r) => (r.ok ? r.json() : [])),
    ])
      .then(([segs, revs]) => {
        if (cancelled) return;
        if (Array.isArray(segs)) setWindowSegments(segs);
        if (Array.isArray(revs)) setWindowReviews(revs);
      })
      .catch(() => {});

    return () => {
      cancelled = true;
    };
  }, [
    realCamera,
    viewportFetchAfter,
    viewportFetchBefore,
    archiveMetadataCache,
    configuredCameras,
  ]);

  // Separate settled clip-range segment fetch when `[rangeStart, rangeEnd]` lies outside viewportFetchBounds
  // (e.g. operator jumped 3 days back via Calendar while clip is still on today, or clip crosses midnight)
  const isSettledClipOutsideViewport =
    rangeMode &&
    draggingTarget !== "in" &&
    draggingTarget !== "out" &&
    (rangeStart < viewportFetchAfter || rangeEnd > viewportFetchBefore);

  useEffect(() => {
    if (!realCamera || !isSettledClipOutsideViewport) {
      if (!isSettledClipOutsideViewport) setClipRangeSegments([]);
      return;
    }
    let cancelled = false;
    const after = Math.floor(rangeStart);
    const before = Math.ceil(Math.max(rangeStart + 1, rangeEnd));
    archiveMetadataCache
      .getRecordings(realCamera, { after, before }, { configuredCameras })
      .then((segs) => {
        if (!cancelled && Array.isArray(segs)) {
          setClipRangeSegments(segs);
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [
    realCamera,
    isSettledClipOutsideViewport,
    rangeStart,
    rangeEnd,
    archiveMetadataCache,
    configuredCameras,
  ]);

  const combinedClipSegments = useMemo(
    () => mergeDeduplicatedSegments(windowSegments, clipRangeSegments),
    [windowSegments, clipRangeSegments]
  );

  // Fetch Tier-1 PreviewFrameStore timestamp index covering the active viewport
  const framesWinStart = Math.floor(winStart / 30) * 30;
  const framesWinEnd = Math.ceil(winEnd / 30) * 30;
  useEffect(() => {
    if (!realCamera || !Number.isFinite(framesWinStart) || !Number.isFinite(framesWinEnd)) {
      setIndexedFrames([]);
      return;
    }
    let cancelled = false;
    const ctrl =
      typeof AbortController !== "undefined" ? new AbortController() : null;
    fetch(
      `/api/preview/${encodeURIComponent(
        realCamera
      )}/start/${framesWinStart}/end/${framesWinEnd}/frames?limit=240`,
      {
        credentials: "include",
        ...(ctrl ? { signal: ctrl.signal } : {}),
      }
    )
      .then((r) => (r.ok ? r.json() : []))
      .then((payload) => {
        if (!cancelled) {
          setIndexedFrames(sanitizePreviewFrameListPayload(realCamera, payload));
        }
      })
      .catch(() => {
        if (!cancelled) setIndexedFrames([]);
      });
    return () => {
      cancelled = true;
      ctrl?.abort();
    };
  }, [realCamera, framesWinStart, framesWinEnd]);

  const clientXToTs = useCallback(
    (clientX: number): number => {
      const el = trackRef.current;
      if (!el) return winEnd;
      const rect = el.getBoundingClientRect();
      return computeClientXTimestamp(
        clientX,
        rect.left,
        rect.width,
        winStart,
        winEnd
      );
    },
    [winStart, winEnd]
  );

  const tsToTrackX = useCallback(
    (ts: number): number => {
      const w = trackRef.current?.clientWidth || trackWidth || 800;
      const frac = Math.max(0, Math.min(1, (ts - winStart) / Math.max(1, windowSpanSec)));
      return Math.round(frac * w);
    },
    [winStart, windowSpanSec, trackWidth]
  );

  const hasRecordingAtTs = useCallback(
    (ts: number): boolean => {
      for (const s of combinedClipSegments) {
        if (ts >= s.start_time - 1.5 && ts <= s.end_time + 1.5) {
          return true;
        }
      }
      return false;
    },
    [combinedClipSegments]
  );

  const emitTrimState = useCallback(
    (
      mode: TimelineTrimFeedbackState["mode"] | null,
      targetTs?: number,
      nextStart?: number,
      nextEnd?: number
    ) => {
      if (trimDebounceTimerRef.current !== null && typeof window !== "undefined") {
        window.clearTimeout(trimDebounceTimerRef.current);
        trimDebounceTimerRef.current = null;
      }
      if (!mode || targetTs === undefined) {
        lastDebouncedScrubUrlRef.current = null;
        onTrimFeedbackRef.current?.(null);
        return;
      }
      const s = nextStart ?? rangeStart;
      const e = nextEnd ?? rangeEnd;
      const fine = mode !== "preview_clip";
      const candidateUrl = resolveTimelineHoverThumbnailUrl(
        realCamera,
        targetTs,
        indexedFrames,
        {
          fineTrim: fine,
          hasRecording: hasRecordingAtTs(targetTs),
        }
      );
      const label =
        mode === "dragging_in" || mode === "settled_in"
          ? `${isPt ? "PONTO IN" : "TRIM IN"} · ${formatHHMMSS(targetTs)} (${formatDuration(
              Math.max(1, e - s)
            )})`
          : mode === "dragging_out" || mode === "settled_out"
          ? `${isPt ? "PONTO OUT" : "TRIM OUT"} · ${formatHHMMSS(targetTs)} (${formatDuration(
              Math.max(1, e - s)
            )})`
          : `${isPt ? "PREVIEW CLIPE" : "CLIP PREVIEW"} · ${formatHHMMSS(s)} → ${formatHHMMSS(
              e
            )} (${formatDuration(Math.max(1, e - s))})`;

      const isDragging = mode === "dragging_in" || mode === "dragging_out";
      const isTier1Immediate = Boolean(candidateUrl?.includes("/frame/"));
      if (!isDragging || isTier1Immediate || !candidateUrl) {
        if (candidateUrl) lastDebouncedScrubUrlRef.current = candidateUrl;
        onTrimFeedbackRef.current?.({
          mode,
          targetTs,
          rangeStart: s,
          rangeEnd: e,
          previewFrameUrl: candidateUrl,
          badgeLabel: label,
        });
        return;
      }

      // During rapid handle drag on Tier-2 `/at/` URLs, update badge immediately while debouncing
      // FFmpeg frame URL switches by 55ms so we never flood the 3-permit extraction semaphore!
      onTrimFeedbackRef.current?.({
        mode,
        targetTs,
        rangeStart: s,
        rangeEnd: e,
        previewFrameUrl: lastDebouncedScrubUrlRef.current,
        badgeLabel: label,
      });
      if (typeof window !== "undefined") {
        trimDebounceTimerRef.current = window.setTimeout(() => {
          trimDebounceTimerRef.current = null;
          lastDebouncedScrubUrlRef.current = candidateUrl;
          onTrimFeedbackRef.current?.({
            mode,
            targetTs,
            rangeStart: s,
            rangeEnd: e,
            previewFrameUrl: candidateUrl,
            badgeLabel: label,
          });
        }, 55);
      }
    },
    [realCamera, indexedFrames, hasRecordingAtTs, rangeStart, rangeEnd, isPt]
  );

  // Global pointer move / up when panning timeline track OR dragging IN/OUT handles
  useEffect(() => {
    if (!draggingTarget) return;
    let latestStart = rangeStart;
    let latestEnd = rangeEnd;

    const onMove = (e: PointerEvent) => {
      if (draggingTarget === "track" && dragOriginRef.current) {
        const el = trackRef.current;
        const width = el?.clientWidth || 800;
        const dx = e.clientX - dragOriginRef.current.startX;
        if (Math.abs(dx) >= 5) {
          dragOriginRef.current.moved = true;
        }
        if (dragOriginRef.current.moved) {
          // Track drag ONLY pans the viewport — never alters playhead or IN/OUT!
          const deltaSec = -(dx / Math.max(1, width)) * windowSpanSec;
          setCustomWinEnd(
            clampWinEndContinuous(dragOriginRef.current.startWinEnd + deltaSec)
          );
        }
        return;
      }

      const el = trackRef.current;
      const rect = el?.getBoundingClientRect();
      const rectLeft = rect?.left ?? 0;
      const rectWidth = rect?.width || 800;

      setFineShiftActive(e.shiftKey);
      const nextCursor = stepHandleDragTs({
        clientX: e.clientX,
        rectLeft,
        rectWidth,
        winStart,
        winEnd,
        shiftKey: e.shiftKey,
        prevState: handleDragCursorRef.current,
      });

      if (draggingTarget === "in") {
        const maxIn = latestEnd - 1;
        const minIn = Math.max(
          minArchiveSec,
          latestEnd - MAX_ARCHIVE_EXPORT_SPAN_SEC
        );
        const nextIn = quantizeClipBoundaryTs(
          nextCursor.currentTs,
          e.shiftKey,
          minIn,
          maxIn
        );
        handleDragCursorRef.current = {
          lastClientX: e.clientX,
          currentTs: nextIn,
        };
        latestStart = nextIn;
        setRangeStart(nextIn);
        setReadyDownloadUrl(null);
        setExportSaved(false);
        setExportQueued(false);
        setIsPreviewingClip(false);
        setHoverTs(nextIn);
        setHoverX(tsToTrackX(nextIn));
        emitTrimState("dragging_in", nextIn, nextIn, latestEnd);
      } else if (draggingTarget === "out") {
        const minOut = latestStart + 1;
        const maxOut = Math.min(
          nowSec,
          latestStart + MAX_ARCHIVE_EXPORT_SPAN_SEC
        );
        const nextOut = quantizeClipBoundaryTs(
          nextCursor.currentTs,
          e.shiftKey,
          minOut,
          maxOut
        );
        handleDragCursorRef.current = {
          lastClientX: e.clientX,
          currentTs: nextOut,
        };
        latestEnd = nextOut;
        setRangeEnd(nextOut);
        setReadyDownloadUrl(null);
        setExportSaved(false);
        setExportQueued(false);
        setIsPreviewingClip(false);
        setHoverTs(nextOut);
        setHoverX(tsToTrackX(nextOut));
        emitTrimState("dragging_out", nextOut, latestStart, nextOut);
      }
    };

    const onUp = (e: PointerEvent) => {
      const endedTarget = draggingTarget;
      releaseActivePointerCapture();
      handleDragCursorRef.current = null;
      setFineShiftActive(false);

      if (endedTarget === "track" && dragOriginRef.current) {
        const origin = dragOriginRef.current;
        dragOriginRef.current = null;
        setDraggingTarget(null);
        // Only an intentional click (< 5px movement) seeks the playhead; pan drag never moves markers!
        if (!origin.moved) {
          const ts = clampArchiveTs(clientXToTs(e.clientX));
          setIsPreviewingClip(false);
          emitTrimState(null);
          onSeek(ts >= nowSec - 3 ? null : ts);
        }
        return;
      }

      setDraggingTarget(null);
      if (endedTarget === "in") {
        onSeek(latestStart);
        emitTrimState("settled_in", latestStart, latestStart, latestEnd);
      } else if (endedTarget === "out") {
        onSeek(latestEnd);
        emitTrimState("settled_out", latestEnd, latestStart, latestEnd);
      }
    };

    const onCancel = () => {
      releaseActivePointerCapture();
      dragOriginRef.current = null;
      handleDragCursorRef.current = null;
      setFineShiftActive(false);
      setDraggingTarget(null);
      setHoverTs(null);
      emitTrimState(null);
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onCancel);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onCancel);
    };
  }, [
    draggingTarget,
    clientXToTs,
    clampArchiveTs,
    clampWinEndContinuous,
    emitTrimState,
    minArchiveSec,
    nowSec,
    onSeek,
    rangeEnd,
    rangeStart,
    releaseActivePointerCapture,
    tsToTrackX,
    winEnd,
    winStart,
    windowSpanSec,
  ]);

  // Mouse-wheel / Trackpad listener:
  // - Ctrl/Alt/Meta + Wheel = Zoom anchored at cursor
  // - Plain Wheel / Horizontal Swipe = Smooth viewport PAN ONLY (never alters playhead or IN/OUT!)
  useEffect(() => {
    const el = trackRef.current;
    if (!el) return;

    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      e.stopPropagation();
      const rawDelta =
        Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
      if (rawDelta === 0) return;

      if (e.ctrlKey || e.altKey || e.metaKey) {
        const curIdx = ZOOM_LEVELS.indexOf(zoomMin);
        const nextIdx =
          rawDelta < 0
            ? Math.max(0, curIdx - 1)
            : Math.min(ZOOM_LEVELS.length - 1, curIdx + 1);
        const nextZoom = ZOOM_LEVELS[nextIdx];
        if (nextZoom !== zoomMin) {
          const rect = el.getBoundingClientRect();
          const cursorFrac =
            rect.width > 0
              ? Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width))
              : 0.5;
          const anchorTs = clampArchiveTs(clientXToTs(e.clientX));
          const nextSpan = nextZoom * 60;
          const nextEnd = Math.round(anchorTs + (1 - cursorFrac) * nextSpan);
          setZoomMin(nextZoom);
          setCustomWinEnd(
            computePannedWindowEnd({
              curWinEnd: nextEnd,
              deltaSec: 0,
              windowSpanSec: nextSpan,
              minArchiveSec,
              nowSec,
            })
          );
        }
        return;
      }

      const normalizedNotch = Math.max(
        -2.5,
        Math.min(2.5, rawDelta / 100)
      );
      const sign = rawDelta > 0 ? 1 : -1;
      const notchFactor =
        Math.abs(normalizedNotch) < 0.25 ? sign * 0.25 : normalizedNotch;
      const deltaSec = e.shiftKey
        ? notchFactor * 10
        : notchFactor * Math.max(10, windowSpanSec * 0.04);

      const anim = wheelAnimRef.current;
      if (anim.rafId === null) {
        anim.curWinEnd = winEnd;
        anim.targetWinEnd = winEnd;
      }
      anim.targetWinEnd = Math.max(
        minArchiveSec + windowSpanSec,
        Math.min(nowSec, anim.targetWinEnd + deltaSec)
      );

      const stepFrame = () => {
        const diffWin = anim.targetWinEnd - anim.curWinEnd;
        if (Math.abs(diffWin) < 0.2) {
          anim.curWinEnd = anim.targetWinEnd;
          anim.rafId = null;
          setCustomWinEnd(clampWinEndContinuous(anim.curWinEnd));
          return;
        }
        anim.curWinEnd += diffWin * 0.34;
        setCustomWinEnd(clampWinEndContinuous(anim.curWinEnd));
        anim.rafId = window.requestAnimationFrame(stepFrame);
      };

      if (anim.rafId === null) {
        anim.rafId = window.requestAnimationFrame(stepFrame);
      }
    };

    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
    };
  }, [
    zoomMin,
    windowSpanSec,
    minArchiveSec,
    winEnd,
    nowSec,
    clientXToTs,
    clampArchiveTs,
    clampWinEndContinuous,
  ]);

  // Jump to a specific Day from the Multi-Day Calendar (never overwrites IN/OUT!)
  const handleSelectDay = (dayObj: RecordingDaySummary) => {
    const parts = parseStrictYmd(dayObj.day || "");
    if (!parts) return;
    const localDayStart = Math.floor(
      new Date(parts[0], parts[1] - 1, parts[2], 0, 0, 0).getTime() / 1000
    );
    if (!Number.isFinite(localDayStart)) return;
    setSelectedDay(dayObj.day.trim());
    setSelectedHour(null);
    const localDayEnd = Math.min(nowSec, localDayStart + 86400);

    const activeHours = [...(dayObj.hours || [])].sort((a, b) =>
      a.hour.localeCompare(b.hour)
    );
    const firstWithEvents =
      activeHours.find((h) => h.events > 0) ||
      activeHours.find((h) => h.motion > 0) ||
      activeHours[0];

    let targetTs = localDayStart + 12 * 3600;
    if (firstWithEvents) {
      const hNum = parseInt(firstWithEvents.hour, 10) || 0;
      targetTs =
        firstWithEvents.start_ts && firstWithEvents.start_ts > 0
          ? firstWithEvents.start_ts
          : localDayStart + hNum * 3600;
      setSelectedHour(firstWithEvents.hour);
    }

    setZoomMin(60);
    setCustomWinEnd(Math.min(localDayEnd, targetTs + 3600));
    setIsPreviewingClip(false);
    emitTrimState(null);
    onSeek(targetTs);
  };

  // Jump to a specific Hour (`00`..`23`) within the selected Day (never overwrites IN/OUT!)
  const handleSelectHour = (
    dayStr: string,
    hourStr: string,
    hourMeta?: RecordingHourSummary
  ) => {
    const parts = parseStrictYmd(dayStr || "");
    if (!parts) return;
    const hNum = parseInt(hourStr, 10) || 0;
    const localHourStart =
      hourMeta?.start_ts && hourMeta.start_ts > 0
        ? hourMeta.start_ts
        : Math.floor(
            new Date(parts[0], parts[1] - 1, parts[2], hNum, 0, 0).getTime() /
              1000
          );
    if (!Number.isFinite(localHourStart)) return;
    setSelectedHour(hourStr);
    const localHourEnd = Math.min(nowSec, localHourStart + 3600);

    setZoomMin(60);
    setCustomWinEnd(localHourEnd);
    setIsPreviewingClip(false);
    emitTrimState(null);
    onSeek(localHourStart);
  };

  // Jump directly to a user-typed Date + Time (never overwrites IN/OUT!)
  const handleDirectDateTimeJump = (dateStr: string, timeStr: string) => {
    const dParts = parseStrictYmd(dateStr || "");
    const tParts = (timeStr || "").split(":").map(Number);
    if (
      !dParts ||
      tParts.length < 2 ||
      tParts.some((n) => !Number.isFinite(n))
    ) {
      return;
    }
    const dt = new Date(
      dParts[0],
      dParts[1] - 1,
      dParts[2],
      tParts[0] || 0,
      tParts[1] || 0,
      tParts[2] || 0
    );
    const ts = Math.floor(dt.getTime() / 1000);
    if (!Number.isFinite(ts) || ts <= 0) return;
    setSelectedDay(dateStr.trim());
    setSelectedHour(String(tParts[0] || 0).padStart(2, "0"));
    setCustomWinEnd(
      clampWinEndContinuous(ts + Math.round(windowSpanSec * 0.5))
    );
    setIsPreviewingClip(false);
    emitTrimState(null);
    onSeek(ts >= nowSec - 3 ? null : ts);
  };

  // Activity-Only Skip: Jump to Previous or Next Event/Motion Segment (never overwrites IN/OUT!)
  const jumpToActivity = (direction: "prev" | "next") => {
    const anchor = clampArchiveTs(currentTs ?? winEnd);
    const candidates: number[] = [];
    for (const r of windowReviews) {
      candidates.push(Math.floor(r.start_time));
    }
    for (const s of windowSegments) {
      const hasAudio =
        s.dbfs !== undefined &&
        s.dbfs !== 0 &&
        (s.dbfs < 0 ? s.dbfs > -50 : s.dbfs > 10);
      if (s.objects > 0 || s.motion > 0 || hasAudio) {
        candidates.push(Math.floor(s.start_time));
      }
    }
    candidates.sort((a, b) => a - b);

    setIsPreviewingClip(false);
    emitTrimState(null);
    if (direction === "prev") {
      const prev = [...candidates].reverse().find((t) => t < anchor - 8);
      const target = clampArchiveTs(prev ?? anchor - 300);
      if (target < winStart) {
        setCustomWinEnd(
          clampWinEndContinuous(target + Math.round(windowSpanSec * 0.8))
        );
      }
      onSeek(target);
    } else {
      const next = candidates.find((t) => t > anchor + 8);
      const target = clampArchiveTs(next ?? anchor + 300);
      if (target >= nowSec - 3) {
        setCustomWinEnd(null);
        onSeek(null);
      } else {
        if (target > winEnd) {
          setCustomWinEnd(
            clampWinEndContinuous(target + Math.round(windowSpanSec * 0.5))
          );
        }
        onSeek(target);
      }
    }
  };

  /** Enters Export clip mode on [start, end] and zooms so the clip is easy to adjust. */
  const openClipMode = (start: number, end: number) => {
    const s = Math.floor(Math.max(minArchiveSec, Math.min(start, end)));
    const e = Math.ceil(Math.min(nowSec, Math.max(start, end)));
    if (!(e > s)) return;
    const clampedEnd = Math.min(e, s + MAX_ARCHIVE_EXPORT_SPAN_SEC);
    const zoom = resolveClipZoom(clampedEnd - s);
    const span = zoom * 60;
    const targetEnd = Math.round(
      clampedEnd + Math.max(0, span - (clampedEnd - s)) / 2
    );
    setZoomMin(zoom);
    setCustomWinEnd(
      targetEnd >= nowSec - 5 ? null : clampWinEndContinuous(targetEnd)
    );
    setRangeStart(s);
    setRangeEnd(clampedEnd);
    setReadyDownloadUrl(null);
    setExportSaved(false);
    setExportQueued(false);
    setIsPreviewingClip(false);
    setRangeMode(true);
    onSeek(s);
    emitTrimState("settled_in", s, s, clampedEnd);
  };

  const closeClipMode = () => {
    setRangeMode(false);
    setExportSaved(false);
    setExportQueued(false);
    setIsPreviewingClip(false);
    emitTrimState(null);
  };

  const toggleRangeSelector = () => {
    if (!rangeMode) {
      const anchor = Math.round(currentTs ?? nowSec);
      openClipMode(anchor - 30, anchor);
    } else {
      closeClipMode();
    }
  };

  const panWindowToTimestamp = (targetTs: number) => {
    setCustomWinEnd(
      clampWinEndContinuous(Math.round(targetTs + windowSpanSec * 0.5))
    );
  };

  const ticks = useMemo(() => {
    const tickStepSec =
      zoomMin <= 5
        ? 60
        : zoomMin <= 15
        ? 120
        : zoomMin <= 60
        ? 600
        : zoomMin <= 360
        ? 3600
        : 10800;
    const firstTick = Math.ceil(winStart / tickStepSec) * tickStepSec;
    const spansDays = !isSameLocalCalendarDay(winStart, winEnd);
    const out: Array<{ pct: number; label: string }> = [];
    for (let ts = firstTick; ts <= winEnd; ts += tickStepSec) {
      const pct = ((ts - winStart) / windowSpanSec) * 100;
      if (pct < 2 || pct > 98) continue;
      out.push({
        pct,
        label:
          zoomMin >= 1440 || (spansDays && formatHHMM(ts) === "00:00")
            ? `${toLocalDateInputValue(ts).slice(5)} ${formatHHMM(ts)}`
            : formatHHMM(ts),
      });
    }
    return out;
  }, [winStart, winEnd, windowSpanSec, zoomMin]);

  const visibleReviews = useMemo(() => {
    return windowReviews.filter(
      (r) =>
        (r.camera === camera || r.camera === realCamera || camera === "all") &&
        (r.end_time || r.start_time + 15) >= winStart &&
        r.start_time <= winEnd
    );
  }, [windowReviews, camera, realCamera, winStart, winEnd]);

  // Compute unrecorded offline gaps (> 25s) between consecutive recording segments
  const offlineGaps = useMemo(() => {
    const valid = windowSegments.filter(
      (s) => Number.isFinite(s.start_time) && Number.isFinite(s.end_time)
    );
    if (valid.length < 2) return [];
    const sorted = [...valid].sort((a, b) => a.start_time - b.start_time);
    const gaps: Array<{
      start_time: number;
      end_time: number;
      duration: number;
    }> = [];
    let prevEnd = sorted[0].end_time;
    for (let i = 1; i < sorted.length; i++) {
      const cur = sorted[i];
      const gapDur = cur.start_time - prevEnd;
      if (gapDur > 25) {
        gaps.push({
          start_time: prevEnd,
          end_time: cur.start_time,
          duration: Math.round(gapDur),
        });
      }
      if (cur.end_time > prevEnd) {
        prevEnd = cur.end_time;
      }
    }
    return gaps;
  }, [windowSegments]);

  const activePeekTs =
    draggingTarget === "in"
      ? rangeStart
      : draggingTarget === "out"
      ? rangeEnd
      : hoverTs;

  const exactHoveredReview = useMemo(() => {
    if (activePeekTs === null) return null;
    return findExactContainingReview(
      visibleReviews,
      camera,
      realCamera,
      activePeekTs
    );
  }, [activePeekTs, visibleReviews, camera, realCamera]);

  const hoveredSegmentOnTrack = useMemo(() => {
    if (activePeekTs === null || windowSegments.length === 0) return null;
    for (const s of windowSegments) {
      if (activePeekTs >= s.start_time - 1.5 && activePeekTs <= s.end_time + 1.5) {
        return s;
      }
    }
    return null;
  }, [activePeekTs, windowSegments]);

  const hoveredGapOnTrack = useMemo(() => {
    if (activePeekTs === null || offlineGaps.length === 0) return null;
    for (const g of offlineGaps) {
      if (activePeekTs >= g.start_time && activePeekTs <= g.end_time) {
        return g;
      }
    }
    return null;
  }, [activePeekTs, offlineGaps]);

  const isDraggingTrimHandle =
    draggingTarget === "in" || draggingTarget === "out";

  const candidateHoverThumbUrl = useMemo(() => {
    if (activePeekTs === null) return null;
    return resolveTimelineHoverThumbnailUrl(
      realCamera,
      activePeekTs,
      indexedFrames,
      {
        fineTrim: isDraggingTrimHandle,
        hasRecording: hoveredSegmentOnTrack !== null,
      }
    );
  }, [
    activePeekTs,
    realCamera,
    indexedFrames,
    isDraggingTrimHandle,
    hoveredSegmentOnTrack,
  ]);

  // Tier-1 indexed `/frame/` URLs resolve at 0ms; Tier-2 `/at/` FFmpeg extractions debounce by 65ms
  useEffect(() => {
    if (!candidateHoverThumbUrl) {
      setDebouncedTier2ThumbUrl(null);
      return;
    }
    if (candidateHoverThumbUrl.includes("/frame/")) {
      setDebouncedTier2ThumbUrl(candidateHoverThumbUrl);
      return;
    }
    const timer = window.setTimeout(
      () => {
        setDebouncedTier2ThumbUrl(candidateHoverThumbUrl);
      },
      isDraggingTrimHandle ? 45 : 70
    );
    return () => window.clearTimeout(timer);
  }, [candidateHoverThumbUrl, isDraggingTrimHandle]);

  const effectiveHoverThumbUrl =
    candidateHoverThumbUrl && candidateHoverThumbUrl.includes("/frame/")
      ? candidateHoverThumbUrl
      : debouncedTier2ThumbUrl;

  const activeDaySummary = useMemo(
    () =>
      daysSummary.find((d) => d.day === selectedDay) || daysSummary[0] || null,
    [daysSummary, selectedDay]
  );

  const day24Hours = useMemo(() => {
    const map = new Map<string, RecordingHourSummary>();
    for (const h of activeDaySummary?.hours || []) {
      map.set(h.hour.padStart(2, "0"), h);
    }
    const slots: Array<{ hour: string; meta?: RecordingHourSummary }> = [];
    for (let i = 0; i < 24; i++) {
      const hStr = String(i).padStart(2, "0");
      slots.push({ hour: hStr, meta: map.get(hStr) });
    }
    return slots;
  }, [activeDaySummary]);

  const clipAnchorTs = Math.round(currentTs ?? nowSec);

  /** Applies a new clip range (supporting multi-day spans up to 24h) and optionally seeks the monitor. */
  const applyClipRange = (
    start: number,
    end: number,
    focusBoundary?: "in" | "out" | null
  ): boolean => {
    const s = Math.max(minArchiveSec, Math.round(start * 4) / 4);
    const e = Math.min(nowSec, Math.round(end * 4) / 4);
    if (!(e - s >= 1) || e - s > MAX_ARCHIVE_EXPORT_SPAN_SEC) return false;
    setRangeStart(s);
    setRangeEnd(e);
    setReadyDownloadUrl(null);
    setExportSaved(false);
    setExportQueued(false);
    setIsPreviewingClip(false);
    if (focusBoundary === "in") {
      if (s < winStart || s > winEnd) panWindowToTimestamp(s);
      onSeek(s);
      emitTrimState("settled_in", s, s, e);
    } else if (focusBoundary === "out") {
      if (e < winStart || e > winEnd) panWindowToTimestamp(e);
      onSeek(e);
      emitTrimState("settled_out", e, s, e);
    }
    return true;
  };

  /** Sets `IN` or `OUT` to `candidateTs`, automatically re-anchoring the opposite marker when >24h away. */
  const commitBoundaryWithReanchor = (
    which: "in" | "out",
    candidateTs: number
  ): boolean => {
    const resolved = reanchorClipRangeBoundary({
      which,
      candidateTs,
      curStart: rangeStart,
      curEnd: rangeEnd,
      minArchiveSec,
      nowSec,
      maxSpanSec: MAX_ARCHIVE_EXPORT_SPAN_SEC,
    });
    if (!resolved) return false;
    return applyClipRange(resolved.start, resolved.end, which);
  };

  const nudgeBoundary = (which: "in" | "out", deltaSec: number) => {
    if (which === "in") {
      const nextIn = Math.max(
        minArchiveSec,
        Math.min(rangeEnd - 1, rangeStart + deltaSec)
      );
      applyClipRange(nextIn, rangeEnd, "in");
    } else {
      const nextOut = Math.min(
        nowSec,
        Math.max(rangeStart + 1, rangeEnd + deltaSec)
      );
      applyClipRange(rangeStart, nextOut, "out");
    }
  };

  const handleToggleClipPreview = () => {
    if (isPreviewingClip) {
      setIsPreviewingClip(false);
      emitTrimState("settled_in", rangeStart, rangeStart, rangeEnd);
      return;
    }
    setIsPreviewingClip(true);
    if (rangeStart < winStart || rangeStart > winEnd) {
      panWindowToTimestamp(rangeStart);
    }
    onSeek(rangeStart);
    emitTrimState("preview_clip", rangeStart, rangeStart, rangeEnd);
  };

  const eventClipRange = useMemo(
    () =>
      resolveEventClipRange(
        windowReviews,
        camera,
        clipAnchorTs,
        nowSec,
        5,
        realCamera
      ),
    [windowReviews, camera, realCamera, clipAnchorTs, nowSec]
  );

  const preFlight = useMemo(
    () =>
      computeClipPreFlightSummary(combinedClipSegments, rangeStart, rangeEnd),
    [combinedClipSegments, rangeStart, rangeEnd]
  );

  const clipSpansMultipleDays = !isSameLocalCalendarDay(rangeStart, rangeEnd);

  // External "Export this event" / "Clip 30s" requests open clip mode pre-filled.
  const lastClipNonceRef = useRef<number | null>(null);
  useEffect(() => {
    if (!clipRequest || clipRequest.nonce === lastClipNonceRef.current) return;
    lastClipNonceRef.current = clipRequest.nonce;
    openClipMode(clipRequest.start, clipRequest.end);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [clipRequest]);

  // Keyboard while clipping: Esc leaves, I / O set start / end to playhead (re-anchoring if >24h away), [ / ] jump to IN / OUT.
  useEffect(() => {
    if (!rangeMode) return;
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const tag = target?.tagName?.toLowerCase();
      if (
        tag === "input" ||
        tag === "textarea" ||
        tag === "select" ||
        target?.isContentEditable
      ) {
        return;
      }
      if (e.altKey || e.ctrlKey || e.metaKey) return;
      if (e.key === "Escape") {
        e.preventDefault();
        closeClipMode();
      } else if (e.key === "i" || e.key === "I") {
        e.preventDefault();
        commitBoundaryWithReanchor("in", clipAnchorTs);
      } else if (e.key === "o" || e.key === "O") {
        e.preventDefault();
        commitBoundaryWithReanchor("out", clipAnchorTs);
      } else if (e.key === "[") {
        e.preventDefault();
        applyClipRange(rangeStart, rangeEnd, "in");
      } else if (e.key === "]") {
        e.preventDefault();
        applyClipRange(rangeStart, rangeEnd, "out");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rangeMode, clipAnchorTs, rangeStart, rangeEnd, minArchiveSec, nowSec]);

  const playheadPct =
    currentTs === null
      ? customWinEnd === null
        ? 99.6
        : -1
      : ((currentTs - winStart) / windowSpanSec) * 100;

  const clipGeometry = useMemo(
    () => computeClipOverlayGeometry(rangeStart, rangeEnd, winStart, winEnd),
    [rangeStart, rangeEnd, winStart, winEnd]
  );

  // Observe track width for crisp High-DPI Canvas rendering
  useEffect(() => {
    const el = trackRef.current;
    if (!el) return;
    setTrackWidth(el.clientWidth || 800);
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        if (entry.contentRect.width > 0) {
          setTrackWidth(Math.round(entry.contentRect.width));
        }
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Draw recording segments, motion/object heatbars, and review pins onto High-DPI 2D Canvas
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const palette = resolveCanvasThemePalette(effectiveTheme);
    const w = Math.max(100, trackWidth);
    const h = 36;
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);

    // 1. Vertical tick grid lines
    ctx.strokeStyle = palette.gridLineStroke;
    ctx.lineWidth = 1;
    for (const tk of ticks) {
      const x = Math.round((tk.pct / 100) * w) + 0.5;
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x, h);
      ctx.stroke();
    }

    // 2. Continuous recording base bar & motion/object activity bars
    for (const seg of windowSegments) {
      if (
        !Number.isFinite(seg.start_time) ||
        !Number.isFinite(seg.end_time) ||
        seg.end_time < winStart ||
        seg.start_time > winEnd
      ) {
        continue;
      }
      const x0 = Math.max(
        0,
        Math.min(w, ((seg.start_time - winStart) / windowSpanSec) * w)
      );
      const safeDur =
        typeof seg.duration === "number" &&
        Number.isFinite(seg.duration) &&
        seg.duration > 0
          ? seg.duration
          : Math.max(1, seg.end_time - seg.start_time);
      const segW = Math.max(1.5, (safeDur / windowSpanSec) * w);

      ctx.fillStyle = palette.continuousBarFill;
      ctx.fillRect(x0, h - 5, segW, 5);

      if (seg.dbfs !== undefined && seg.dbfs !== 0) {
        const normAudio =
          seg.dbfs < 0
            ? Math.max(0, Math.min(1, (seg.dbfs + 55) / 50))
            : Math.max(0, Math.min(1, seg.dbfs / 100));
        if (normAudio > 0.05) {
          const audioH = Math.max(4, Math.round(normAudio * 14));
          ctx.fillStyle = palette.audioWaveformFill;
          ctx.fillRect(x0, h - 5 - audioH, segW, audioH);
        }
      }

      if (seg.objects > 0) {
        ctx.fillStyle = palette.objectBarFill;
        ctx.fillRect(x0, h - 19, segW, 14);
      } else if (seg.motion > 0) {
        ctx.fillStyle = palette.motionBarFill;
        ctx.fillRect(x0, h - 15, segW, 10);
      }
    }

    // 2b. Hatched offline gap bars between consecutive recording segments
    for (const gap of offlineGaps) {
      if (gap.end_time < winStart || gap.start_time > winEnd) continue;
      const gx0 = Math.max(
        0,
        Math.min(w, ((gap.start_time - winStart) / windowSpanSec) * w)
      );
      const gx1 = Math.max(
        0,
        Math.min(w, ((gap.end_time - winStart) / windowSpanSec) * w)
      );
      const gw = Math.max(2, gx1 - gx0);
      ctx.fillStyle = palette.gapFill;
      ctx.fillRect(gx0, h - 9, gw, 9);
      ctx.save();
      ctx.beginPath();
      ctx.rect(gx0, h - 9, gw, 9);
      ctx.clip();
      ctx.strokeStyle = palette.gapHatchStroke;
      ctx.lineWidth = 1;
      for (let sx = gx0 - 9; sx < gx0 + gw + 9; sx += 6) {
        ctx.beginPath();
        ctx.moveTo(sx, h);
        ctx.lineTo(sx + 7, h - 9);
        ctx.stroke();
      }
      ctx.restore();
    }

    // 3. Review events in two lanes: alerts on top, detections right below
    for (const rev of visibleReviews) {
      const x0 = Math.max(
        0,
        Math.min(w, ((rev.start_time - winStart) / windowSpanSec) * w)
      );
      const dur = Math.max(
        15,
        (rev.end_time || rev.start_time + 15) - rev.start_time
      );
      const revW = Math.max(3, (dur / windowSpanSec) * w);
      const lane = resolveReviewLane(rev.severity);
      ctx.fillStyle =
        rev.severity === "alert" ? palette.alertPinFill : palette.motionPinFill;
      ctx.fillRect(x0, lane.y, revW, lane.h);
    }
  }, [
    effectiveTheme,
    highContrast,
    trackWidth,
    ticks,
    windowSegments,
    offlineGaps,
    visibleReviews,
    winStart,
    winEnd,
    windowSpanSec,
  ]);

  const handleTriggerExport = async () => {
    setExportBusy(true);
    setReadyDownloadUrl(null);
    setExportQueued(false);
    try {
      const label =
        clipLabel.trim() ||
        `${camera}_${toLocalDateInputValue(rangeStart)}_${formatHHMMSS(
          rangeStart
        ).replace(/:/g, "")}`;
      const res = await onExportRange(
        realCamera,
        rangeStart,
        rangeEnd,
        label,
        effectiveIncludeAudio
      );
      if (res && res.queued) {
        setExportQueued(true);
        setExportSaved(true);
      } else if (res && (res.exportId || res.videoUrl)) {
        setExportSaved(true);
      }
      if (res && !res.queued && res.videoUrl) {
        const safeDownloadUrl = resolveSafeMediaUrl(res.videoUrl, {
          kind: "export_video",
          fallbackUrl: "",
          allowedPrefixes: ["/exports/", "/api/", "/vod/", "/clips/"],
          rejectSpaces: true,
        });
        if (safeDownloadUrl) {
          setReadyDownloadUrl(safeDownloadUrl);
        }
      }
    } finally {
      setExportBusy(false);
    }
  };

  const displayAnchorTs = Math.round(currentTs ?? winEnd);
  const showThumbImg =
    Boolean(effectiveHoverThumbUrl) &&
    effectiveHoverThumbUrl !== failedThumbUrl;

  return (
    <div
      className="nvr-timeline-dock"
      style={{
        background: "var(--bg-panel)",
        borderTop: "1px solid var(--border-hairline)",
        padding: "6px 12px 8px",
        display: "flex",
        flexDirection: "column",
        gap: 6,
        userSelect: "none",
      }}
    >
      {/* =====================================================================
          EXPANDABLE MULTI-DAY ARCHIVE & 24-HOUR HEATMAP + STORYBOARD DECK
         ===================================================================== */}
      {showArchiveDeck && (
        <div
          style={{
            background: "var(--bg-canvas)",
            border: "1px solid var(--border-strong)",
            borderRadius: 3,
            padding: "8px 10px",
            display: "flex",
            flexDirection: "column",
            gap: 8,
          }}
        >
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: 8,
              flexWrap: "wrap",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 5,
                overflowX: "auto",
                paddingBottom: 2,
                flex: 1,
              }}
            >
              <span
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-accent-text)",
                  marginRight: 4,
                  whiteSpace: "nowrap",
                }}
              >
                1. {isPt ? "DIA" : "SELECT DAY"} ({daysSummary.length}d):
              </span>
              {daysSummary.map((d) => {
                const isSelected = d.day === selectedDay;
                return (
                  <button
                    key={d.day}
                    className={`nvr-btn mono ${isSelected ? "active" : ""} sm`}
                    style={{
                      display: "inline-flex",
                      alignItems: "center",
                      gap: 6,
                    }}
                    onClick={() => handleSelectDay(d)}
                  >
                    <span style={{ fontWeight: 700 }}>
                      {formatDayShort(d.day)}
                    </span>
                    <span
                      style={{
                        fontSize: 10.5,
                        padding: "0 4px",
                        borderRadius: 2,
                        background:
                          d.events > 0
                            ? "rgba(239, 68, 68, 0.22)"
                            : "rgba(16, 185, 129, 0.16)",
                        color:
                          d.events > 0
                            ? "var(--signal-alert-text)"
                            : "var(--signal-live-text)",
                      }}
                    >
                      {d.events} ev
                    </span>
                  </button>
                );
              })}
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
              <RecordingCalendarPopover
                daysSummary={daysSummary}
                selectedDate={selectedDay}
                selectedTime={toLocalTimeInputValue(displayAnchorTs)}
                showTimePicker={true}
                openUpward={true}
                buttonLabelPrefix="Calendar"
                onSelectDate={(dateStr, dayObj) => {
                  if (dayObj) {
                    handleSelectDay(dayObj);
                  } else {
                    handleDirectDateTimeJump(dateStr, "12:00:00");
                  }
                }}
                onSelectDateTime={(dateStr, timeStr) => {
                  handleDirectDateTimeJump(dateStr, timeStr);
                }}
              />
              <button
                className={`nvr-btn ${showStoryboard ? "primary" : ""} sm`}
                onClick={() => setShowStoryboard((s) => !s)}
                title="Scan visual thumbnails across this day/hour to spot changes quickly"
              >
                <Grid size={12} />
                {showStoryboard
                  ? "Hide Storyboard"
                  : `Storyboard Scan (${windowReviews.length})`}
              </button>
              <button
                className="nvr-btn sm"
                onClick={() => setShowArchiveDeck(false)}
                aria-label="Collapse multi-day navigator"
                title="Collapse multi-day navigator"
              >
                <X size={12} />
              </button>
            </div>
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
            <span
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                color: "var(--signal-motion-text)",
                marginRight: 4,
                whiteSpace: "nowrap",
              }}
            >
              2. {isPt ? "HORA" : "CLICK HOUR"} ({selectedDay}):
            </span>
            <div
              style={{
                flex: 1,
                display: "grid",
                gridTemplateColumns: "repeat(24, minmax(0, 1fr))",
                gap: 3,
              }}
            >
              {day24Hours.map(({ hour, meta }) => {
                const hasRecording = Boolean(meta && meta.duration > 0);
                const hasEvents = Boolean(meta && meta.events > 0);
                const hasMotion = Boolean(meta && meta.motion > 0);
                const isSelected = selectedHour === hour;
                return (
                  <button
                    key={hour}
                    onClick={() => handleSelectHour(selectedDay, hour, meta)}
                    disabled={!hasRecording}
                    title={
                      hasRecording
                        ? `${selectedDay} ${hour}:00 (${meta?.events || 0} events, ${Math.round(
                            (meta?.duration || 0) / 60
                          )}m recorded)`
                        : `${selectedDay} ${hour}:00 (No recording)`
                    }
                    className="mono"
                    style={{
                      height: 36,
                      borderRadius: 2,
                      border: isSelected
                        ? "1px solid var(--signal-accent-text)"
                        : hasEvents
                        ? "1px solid rgba(239, 68, 68, 0.55)"
                        : hasRecording
                        ? "1px solid rgba(16, 185, 129, 0.35)"
                        : "1px solid var(--border-divider)",
                      background: isSelected
                        ? "rgba(14, 165, 233, 0.28)"
                        : hasEvents
                        ? "rgba(239, 68, 68, 0.18)"
                        : hasMotion
                        ? "rgba(245, 158, 11, 0.15)"
                        : hasRecording
                        ? "rgba(16, 185, 129, 0.10)"
                        : "var(--bg-deep)",
                      color: hasRecording
                        ? "var(--text-primary)"
                        : "var(--text-muted)",
                      opacity: hasRecording ? 1 : 0.4,
                      cursor: hasRecording ? "pointer" : "default",
                      display: "flex",
                      flexDirection: "column",
                      alignItems: "center",
                      justifyContent: "center",
                      padding: 1,
                    }}
                  >
                    <span
                      style={{ fontSize: "var(--text-xs)", fontWeight: 700 }}
                    >
                      {hour}h
                    </span>
                    <span
                      style={{
                        fontSize: 10,
                        color: hasEvents
                          ? "var(--signal-alert-text)"
                          : hasMotion
                          ? "var(--signal-motion-text)"
                          : "var(--signal-live-text)",
                      }}
                    >
                      {hasEvents
                        ? `${meta?.events}ev`
                        : hasRecording
                        ? "rec"
                        : "-"}
                    </span>
                  </button>
                );
              })}
            </div>
          </div>

          {showStoryboard && (
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 8,
                overflowX: "auto",
                padding: "6px 4px",
                borderTop: "1px solid var(--border-hairline)",
              }}
            >
              {windowReviews.length === 0 ? (
                <div
                  style={{
                    fontSize: "var(--text-xs)",
                    color: "var(--text-muted)",
                  }}
                >
                  No motion/object events recorded in this time window. Select
                  another day or hour above.
                </div>
              ) : (
                [...windowReviews]
                  .sort((a, b) => a.start_time - b.start_time)
                  .map((rev) => {
                    const thumbSrc = sanitizeTimelineThumbUrl(
                      rev.thumb_path,
                      rev.camera
                    );
                    const isHov = hoveredStoryboardId === rev.id;
                    return (
                      <div
                        key={rev.id}
                        role="button"
                        tabIndex={0}
                        onPointerEnter={() => setHoveredStoryboardId(rev.id)}
                        onPointerLeave={() =>
                          setHoveredStoryboardId((c) =>
                            c === rev.id ? null : c
                          )
                        }
                        onPointerCancel={() =>
                          setHoveredStoryboardId((c) =>
                            c === rev.id ? null : c
                          )
                        }
                        onBlur={() =>
                          setHoveredStoryboardId((c) =>
                            c === rev.id ? null : c
                          )
                        }
                        onClick={() => {
                          const ts = Math.floor(rev.start_time);
                          setCustomWinEnd(
                            clampWinEndContinuous(
                              ts + Math.round(windowSpanSec * 0.5)
                            )
                          );
                          onSeek(ts);
                        }}
                        onKeyDown={(e) => {
                          if (e.key === "Enter" || e.key === " ") {
                            e.preventDefault();
                            const ts = Math.floor(rev.start_time);
                            setCustomWinEnd(
                              clampWinEndContinuous(
                                ts + Math.round(windowSpanSec * 0.5)
                              )
                            );
                            onSeek(ts);
                          }
                        }}
                        style={{
                          width: 148,
                          minWidth: 148,
                          background: "var(--bg-deep)",
                          border:
                            currentTs && Math.abs(currentTs - rev.start_time) < 15
                              ? "1px solid var(--signal-accent-text)"
                              : "1px solid var(--border-hairline)",
                          borderRadius: 3,
                          overflow: "hidden",
                          cursor: "pointer",
                          flexShrink: 0,
                        }}
                      >
                        <div
                          style={{
                            width: "100%",
                            height: 74,
                            position: "relative",
                          }}
                        >
                          {isHov ? (
                            <video
                              src={`/api/review/${encodeURIComponent(
                                rev.id
                              )}/preview?format=mp4`}
                              autoPlay
                              muted
                              loop
                              playsInline
                              onError={() =>
                                setHoveredStoryboardId((c) =>
                                  c === rev.id ? null : c
                                )
                              }
                              style={{
                                width: "100%",
                                height: "100%",
                                objectFit: "contain",
                              }}
                            />
                          ) : (
                            <img
                              src={thumbSrc}
                              alt={`${rev.camera} event at ${formatHHMMSS(
                                rev.start_time
                              )}`}
                              width={148}
                              height={74}
                              loading="lazy"
                              style={{
                                width: "100%",
                                height: "100%",
                                objectFit: "contain",
                              }}
                            />
                          )}
                        </div>
                        <div
                          className="mono"
                          style={{
                            padding: "4px 6px",
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            fontSize: "var(--text-xs)",
                          }}
                        >
                          <span
                            style={{
                              color: "var(--signal-accent-text)",
                              fontWeight: 700,
                            }}
                          >
                            {formatHHMMSS(rev.start_time)}
                          </span>
                          <button
                            className="nvr-btn sm"
                            title="Open Segment Clipper on this exact event"
                            onClick={(e) => {
                              e.stopPropagation();
                              const ts = Math.floor(rev.start_time);
                              openClipMode(
                                Math.max(0, ts - 5),
                                Math.min(
                                  nowSec,
                                  Math.ceil((rev.end_time || ts + 20) + 5)
                                )
                              );
                            }}
                          >
                            <Scissors size={10} />
                            Clip
                          </button>
                        </div>
                      </div>
                    );
                  })
              )}
            </div>
          )}
        </div>
      )}

      {/* =====================================================================
          MAIN TIMELINE NAVIGATION BAR (ALWAYS ACCESSIBLE, EVEN IN CLIP MODE!)
         ===================================================================== */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 8,
          flexWrap: "wrap",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 6,
            flexWrap: "wrap",
          }}
        >
          {titlePrefix && (
            <span
              className="nvr-tl-desktop"
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                letterSpacing: "0.04em",
                color: "var(--text-secondary)",
              }}
            >
              {titlePrefix}
            </span>
          )}
          {stageCameras &&
            stageCameras.length > 1 &&
            stageCameras.length <= 4 &&
            cameras &&
            cameras.length > stageCameras.length &&
            onSelectCamera && (
              <div
                className="mono"
                role="group"
                aria-label="On-stage cameras"
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: 3,
                  background: "var(--bg-subcard)",
                  border: "1px solid var(--border-hairline)",
                  borderRadius: 3,
                  padding: "1px 4px",
                }}
              >
                <span
                  style={{
                    fontSize: "var(--text-xs)",
                    color: "var(--text-muted)",
                    padding: "0 3px",
                  }}
                  title="Cameras currently visible on the stage page above"
                >
                  STAGE:
                </span>
                {stageCameras.map((sc) => (
                  <button
                    key={sc}
                    type="button"
                    className={`nvr-btn mono ${camera === sc ? "active" : ""} sm`}
                    style={{ textTransform: "uppercase" }}
                    onClick={() => onSelectCamera(sc)}
                  >
                    {sc}
                  </button>
                ))}
              </div>
            )}
          {cameras && cameras.length > 0 && onSelectCamera && (
            <TacticalCameraPicker
              cameras={cameras}
              selectedCamera={camera}
              onSelect={onSelectCamera}
              cameraGroups={cameraGroups}
              ariaLabel="Timeline camera selector"
              openUpward={true}
            />
          )}
          {onFocusCamera && (
            <button
              type="button"
              className="nvr-btn mono sm"
              title="Expand active timeline camera into Focus stage view"
              onClick={() => onFocusCamera(camera)}
            >
              ⤢ Focus
            </button>
          )}

          <RecordingCalendarPopover
            daysSummary={daysSummary}
            selectedDate={selectedDay}
            selectedTime={toLocalTimeInputValue(displayAnchorTs)}
            showTimePicker={true}
            openUpward={true}
            buttonLabelPrefix="Calendar"
            onSelectDate={(dateStr, dayObj) => {
              if (dayObj) {
                handleSelectDay(dayObj);
              } else {
                handleDirectDateTimeJump(dateStr, "12:00:00");
              }
            }}
            onSelectDateTime={(dateStr, timeStr) => {
              handleDirectDateTimeJump(dateStr, timeStr);
            }}
          />

          <button
            className={`nvr-btn nvr-tl-desktop ${showArchiveDeck ? "active" : ""} sm`}
            onClick={() => setShowArchiveDeck((v) => !v)}
          >
            <Grid size={12} />
            {showArchiveDeck
              ? "Hide 24h Matrix"
              : `24h Activity Matrix (${daysSummary.length || 1}d)`}
          </button>

          <span
            className={`nvr-badge ${currentTs === null ? "live" : "rewind"}`}
            style={{ height: 24 }}
          >
            <span
              className={`nvr-dot ${currentTs === null ? "live" : "motion"}`}
            />
            {currentTs === null
              ? "LIVE EDGE"
              : formatFullDateTime(Math.round(currentTs))}
          </span>

          <button
            className="nvr-btn sm"
            onClick={() => jumpToActivity("prev")}
            title="Jump to previous motion or person event (does not overwrite clip IN/OUT)"
          >
            <SkipBack size={11} />
            Prev Event
          </button>
          <button
            className="nvr-btn sm"
            onClick={() => jumpToActivity("next")}
            title="Jump to next motion or person event (does not overwrite clip IN/OUT)"
          >
            Next Event
            <SkipForward size={11} />
          </button>

          <button
            className="nvr-btn mono sm"
            onClick={() => {
              setIsPreviewingClip(false);
              emitTrimState(null);
              onSeek(clampArchiveTs((currentTs ?? winEnd) - 30));
            }}
          >
            -30s
          </button>
          <button
            className="nvr-btn mono sm"
            onClick={() => {
              setIsPreviewingClip(false);
              emitTrimState(null);
              const next = clampArchiveTs((currentTs ?? winEnd) + 30);
              onSeek(next >= nowSec - 2 ? null : next);
            }}
          >
            +30s
          </button>
          {currentTs !== null && (
            <button
              className="nvr-btn success sm"
              onClick={() => {
                setSelectedDay(toLocalDateInputValue(nowSec));
                setCustomWinEnd(null);
                setIsPreviewingClip(false);
                emitTrimState(null);
                onSeek(null);
              }}
            >
              Return to Live
            </button>
          )}
        </div>

        {/* Right: Zoom (5m / 15m / 1h / 6h / 24h), Pan, and Segment Clipper Toggle */}
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 6,
            flexWrap: "wrap",
          }}
        >
          <span
            className="nvr-timeline-legend nvr-tl-desktop mono"
            aria-label={isPt ? "Legenda" : "Legend"}
          >
            <span>
              <i className="is-alert" aria-hidden="true" />
              {isPt ? "Alertas" : "Alerts"}
            </span>
            <span>
              <i className="is-detection" aria-hidden="true" />
              {isPt ? "Detecções" : "Detections"}
            </span>
          </span>
          <div style={{ display: "flex", alignItems: "center", gap: 3 }}>
            <span
              style={{
                fontSize: "var(--text-xs)",
                color: "var(--text-muted)",
                marginRight: 2,
              }}
            >
              Zoom:
            </span>
            {(
              [
                { min: 5, label: "5m" },
                { min: 15, label: "15m" },
                { min: 60, label: "1h" },
                { min: 360, label: "6h" },
                { min: 1440, label: "24h" },
              ] as const
            ).map((z) => (
              <button
                key={z.min}
                className={`nvr-btn mono ${zoomMin === z.min ? "active" : ""} sm`}
                onClick={() => {
                  setZoomMin(z.min);
                  const anchor =
                    currentTs ??
                    (rangeMode
                      ? Math.round((rangeStart + rangeEnd) / 2)
                      : null);
                  if (anchor !== null) {
                    const spanSec = z.min * 60;
                    setCustomWinEnd(
                      computePannedWindowEnd({
                        curWinEnd: Math.round(anchor + spanSec / 2),
                        deltaSec: 0,
                        windowSpanSec: spanSec,
                        minArchiveSec,
                        nowSec,
                      })
                    );
                  }
                }}
              >
                {z.label}
              </button>
            ))}
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: 3 }}>
            <button
              className="nvr-btn sm"
              aria-label="Pan timeline earlier"
              title={isPt ? "Navegar para trás na janela (sem mover marcadores)" : "Pan timeline earlier (without moving markers)"}
              disabled={winStart <= minArchiveSec}
              onClick={() =>
                setCustomWinEnd(
                  clampWinEndContinuous(winEnd - Math.round(windowSpanSec * 0.5))
                )
              }
            >
              <ChevronLeft size={12} />
            </button>
            <button
              className="nvr-btn sm"
              aria-label="Pan timeline later"
              title={isPt ? "Navegar para frente na janela (sem mover marcadores)" : "Pan timeline later (without moving markers)"}
              disabled={winEnd >= nowSec}
              onClick={() =>
                setCustomWinEnd(
                  clampWinEndContinuous(winEnd + Math.round(windowSpanSec * 0.5))
                )
              }
            >
              <ChevronRight size={12} />
            </button>
          </div>

          <button
            type="button"
            className={`nvr-btn sm ${rangeMode ? "active" : ""}`}
            title={
              isPt
                ? "Abrir/fechar mesa de recorte MP4 [IN ... OUT]"
                : "Toggle MP4 clip trimmer [IN ... OUT]"
            }
            onClick={toggleRangeSelector}
          >
            <Scissors size={12} />
            {t.live.exportClip}
          </button>
        </div>
      </div>

      {/* =====================================================================
          EXPORT CLIP TRIM BAR (STACKED CLEANLY BELOW NAVIGATION BAR)
         ===================================================================== */}
      {rangeMode && (
        <div
          className="nvr-clip-bar"
          role="group"
          aria-label={`${t.live.exportClip} · ${camera}`}
        >
          <span className="mono nvr-clip-bar-title">
            <Scissors size={12} />
            {t.live.exportClip.toUpperCase()} · {camera.toUpperCase()}
          </span>
          <span className="nvr-divider" aria-hidden="true" />

          {/* IN Boundary + Fine Nudge + Playhead Set */}
          <label className="nvr-clip-field mono">
            <button
              type="button"
              className="nvr-btn sm mono"
              style={{ padding: "0 5px" }}
              title={
                isPt
                  ? "Ver quadro exato de INÍCIO no vídeo (Atalho: [)"
                  : "Jump video to exact START frame (Hotkey: [)"
              }
              onClick={() => applyClipRange(rangeStart, rangeEnd, "in")}
            >
              [ {isPt ? "IN" : "IN"}
            </button>
            <ClockInput
              value={rangeStart}
              includeDate={clipSpansMultipleDays}
              ariaLabel={isPt ? "Início do clipe" : "Clip start time"}
              onCommit={(ts) => commitBoundaryWithReanchor("in", ts)}
            />
          </label>
          <div style={{ display: "inline-flex", gap: 2 }}>
            <button
              type="button"
              className="nvr-btn mono sm"
              style={{ padding: "0 5px" }}
              title={isPt ? "Recuar IN em 1 segundo" : "Nudge IN -1s"}
              onClick={() => nudgeBoundary("in", -1)}
            >
              -1s
            </button>
            <button
              type="button"
              className="nvr-btn mono sm"
              style={{ padding: "0 5px" }}
              title={isPt ? "Avançar IN em 1 segundo" : "Nudge IN +1s"}
              disabled={rangeStart + 1 >= rangeEnd}
              onClick={() => nudgeBoundary("in", 1)}
            >
              +1s
            </button>
            <button
              type="button"
              className="nvr-btn sm"
              title={
                isPt
                  ? "Usar o playhead como início (Atalho: I)"
                  : "Set start to the playhead (Hotkey: I)"
              }
              onClick={() => commitBoundaryWithReanchor("in", clipAnchorTs)}
            >
              <ArrowDownToLine size={11} />I
            </button>
          </div>

          <span aria-hidden="true" className="nvr-clip-arrow">
            →
          </span>

          {/* OUT Boundary + Fine Nudge + Playhead Set */}
          <label className="nvr-clip-field mono">
            <button
              type="button"
              className="nvr-btn sm mono"
              style={{ padding: "0 5px" }}
              title={
                isPt
                  ? "Ver quadro exato de FIM no vídeo (Atalho: ])"
                  : "Jump video to exact END frame (Hotkey: ])"
              }
              onClick={() => applyClipRange(rangeStart, rangeEnd, "out")}
            >
              {isPt ? "OUT" : "OUT"} ]
            </button>
            <ClockInput
              value={rangeEnd}
              includeDate={clipSpansMultipleDays}
              ariaLabel={isPt ? "Fim do clipe" : "Clip end time"}
              onCommit={(ts) => commitBoundaryWithReanchor("out", ts)}
            />
          </label>
          <div style={{ display: "inline-flex", gap: 2 }}>
            <button
              type="button"
              className="nvr-btn mono sm"
              style={{ padding: "0 5px" }}
              title={isPt ? "Recuar OUT em 1 segundo" : "Nudge OUT -1s"}
              disabled={rangeEnd - 1 <= rangeStart}
              onClick={() => nudgeBoundary("out", -1)}
            >
              -1s
            </button>
            <button
              type="button"
              className="nvr-btn mono sm"
              style={{ padding: "0 5px" }}
              title={isPt ? "Avançar OUT em 1 segundo" : "Nudge OUT +1s"}
              disabled={rangeEnd + 1 > nowSec}
              onClick={() => nudgeBoundary("out", 1)}
            >
              +1s
            </button>
            <button
              type="button"
              className="nvr-btn sm"
              title={
                isPt
                  ? "Usar o playhead como fim (Atalho: O)"
                  : "Set end to the playhead (Hotkey: O)"
              }
              onClick={() => commitBoundaryWithReanchor("out", clipAnchorTs)}
            >
              <ArrowDownToLine size={11} />O
            </button>
          </div>

          {/* Pre-Flight Duration + Estimated Size + Gaps Badge */}
          <span className="nvr-badge mono nvr-clip-duration">
            {formatDuration(rangeEnd - rangeStart)}
            {preFlight.estimatedSizeLabel
              ? ` · ${preFlight.estimatedSizeLabel}`
              : ""}
          </span>
          {combinedClipSegments.length > 0 && !preFlight.hasCoverage && (
            <span
              className="nvr-badge alert mono"
              title={
                isPt
                  ? "Não há segmentos gravados cobrindo o intervalo selecionado"
                  : "No recorded segments cover the selected time range"
              }
            >
              <AlertTriangle size={11} />
              {isPt ? "Sem gravação" : "No recording"}
            </span>
          )}
          {preFlight.hasCoverage && preFlight.gapCount > 0 && (
            <span
              className="nvr-badge motion mono"
              title={
                isPt
                  ? `${preFlight.gapCount} lacuna(s) sem gravação (${formatDuration(
                      preFlight.missingDurationSec
                    )}) dentro do intervalo`
                  : `${preFlight.gapCount} offline gap(s) (${formatDuration(
                      preFlight.missingDurationSec
                    )}) inside selected range`
              }
            >
              <AlertTriangle size={11} />
              {isPt
                ? `${preFlight.gapCount} gap (${formatDuration(
                    preFlight.missingDurationSec
                  )})`
                : `${preFlight.gapCount} gap (${formatDuration(
                    preFlight.missingDurationSec
                  )})`}
            </span>
          )}

          <span className="nvr-divider" aria-hidden="true" />
          <div
            className="nvr-segmented mono"
            role="group"
            aria-label={isPt ? "Intervalo rápido" : "Quick range"}
          >
            <button
              type="button"
              className={`nvr-btn ${
                eventClipRange &&
                eventClipRange.start === rangeStart &&
                eventClipRange.end === rangeEnd
                  ? "active"
                  : ""
              }`}
              disabled={!eventClipRange}
              title={
                isPt
                  ? "O evento sob o playhead, com 5 s de margem"
                  : "The event under the playhead, with 5 s of padding"
              }
              onClick={() => {
                if (eventClipRange) {
                  applyClipRange(
                    eventClipRange.start,
                    eventClipRange.end,
                    "in"
                  );
                }
              }}
            >
              {isPt ? "Este evento" : "This event"}
            </button>
            {([30, 60, 300, 900, 3600] as const).map((sec) => {
              const active =
                Math.abs(rangeEnd - Math.min(nowSec, clipAnchorTs)) <= 2 &&
                Math.abs(rangeEnd - rangeStart - sec) <= 2;
              const label =
                sec < 60
                  ? `${sec}s`
                  : sec < 3600
                  ? `${sec / 60}m`
                  : `${sec / 3600}h`;
              return (
                <button
                  key={sec}
                  type="button"
                  className={`nvr-btn ${active ? "active" : ""}`}
                  title={
                    isPt
                      ? `${label} terminando no playhead`
                      : `${label} ending at the playhead`
                  }
                  onClick={() =>
                    applyClipRange(clipAnchorTs - sec, clipAnchorTs, "in")
                  }
                >
                  {label}
                </button>
              );
            })}
          </div>

          <span style={{ flex: 1 }} />

          {/* Audio Toggle */}
          <button
            type="button"
            className={`nvr-btn sm mono ${effectiveIncludeAudio ? "active" : ""}`}
            aria-label="Include Audio"
            aria-pressed={effectiveIncludeAudio}
            title={
              effectiveIncludeAudio
                ? isPt
                  ? "Áudio incluído no MP4 (clique para remover áudio)"
                  : "Audio included in MP4 (click to strip audio)"
                : isPt
                ? "Áudio removido do MP4 (clique para incluir)"
                : "Audio stripped from MP4 (click to include)"
            }
            onClick={() => setEffectiveIncludeAudio(!effectiveIncludeAudio)}
          >
            {effectiveIncludeAudio ? (
              <Volume2 size={11} />
            ) : (
              <VolumeX size={11} />
            )}
            {effectiveIncludeAudio
              ? isPt
                ? "Áudio"
                : "Audio"
              : isPt
              ? "Sem áudio"
              : "Muted"}
          </button>

          <input
            className="nvr-input sm"
            style={{ width: 155 }}
            placeholder={isPt ? "Rótulo (opcional)" : "Label (optional)"}
            aria-label={isPt ? "Rótulo do clipe" : "Clip label"}
            value={clipLabel}
            onChange={(e) => setClipLabel(e.target.value)}
          />
          <button
            type="button"
            className={`nvr-btn sm ${isPreviewingClip ? "active" : ""}`}
            onClick={handleToggleClipPreview}
            title={
              isPt
                ? "Reproduzir apenas o trecho [IN → OUT] em loop"
                : "Play only the selected [IN → OUT] clip range"
            }
          >
            {isPreviewingClip ? <Square size={10} /> : <Play size={11} />}
            {isPreviewingClip
              ? isPt
                ? "Parar"
                : "Stop"
              : "Preview"}
          </button>
          <button
            type="button"
            className="nvr-btn sm primary"
            disabled={
              exportBusy ||
              rangeEnd - rangeStart < 1 ||
              (combinedClipSegments.length > 0 && !preFlight.hasCoverage)
            }
            onClick={handleTriggerExport}
          >
            {exportBusy
              ? isPt
                ? "Enfileirando…"
                : "Queueing…"
              : `Export MP4 · ${formatDuration(rangeEnd - rangeStart)}`}
          </button>
          {readyDownloadUrl && (
            <a
              href={readyDownloadUrl}
              download
              className="nvr-btn sm"
              style={{ textDecoration: "none" }}
            >
              <Download size={11} />
              Download
            </a>
          )}
          {exportSaved && onOpenExports && (
            <button
              type="button"
              className="nvr-btn sm success"
              role="status"
              onClick={onOpenExports}
            >
              {exportQueued
                ? isPt
                  ? "Exportando em segundo plano · Ver progresso →"
                  : "Exporting in background · View progress →"
                : isPt
                ? "Clipe salvo · Abrir em Exports →"
                : "Clip saved · Open in Exports →"}
            </button>
          )}
          {!defaultRangeMode && (
            <button
              type="button"
              className="nvr-btn sm"
              onClick={closeClipMode}
              title={
                isPt
                  ? "Sair do modo de recorte (Esc)"
                  : "Leave clip mode (Esc)"
              }
            >
              {isPt ? "Fechar" : "Done"}
            </button>
          )}
        </div>
      )}

      {/* =====================================================================
          DEDICATED TIME RULER HEADER (ABOVE SEEK TRACK - ZERO OVERLAP)
         ===================================================================== */}
      <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
        <div
          className="nvr-timeline-ruler"
          style={{
            position: "relative",
            height: 16,
            width: "100%",
            pointerEvents: "none",
            overflow: "hidden",
          }}
        >
          {ticks.map((tk) => (
            <span
              key={`${tk.label}-${tk.pct.toFixed(2)}`}
              className="mono"
              style={{
                position: "absolute",
                top: 0,
                left: `${tk.pct}%`,
                transform: "translateX(-50%)",
                fontSize: "var(--text-xs)",
                lineHeight: "16px",
                color: "var(--text-muted)",
                whiteSpace: "nowrap",
              }}
            >
              {tk.label}
            </span>
          ))}
        </div>

        {/* ===================================================================
            HIGH-DPI CANVAS SEEK TRACK + HONEST RANGE CLIPPER + PLAYHEAD NEEDLE
           =================================================================== */}
        <div
          ref={trackRef}
          className="nvr-timeline-track"
          title={
            isPt
              ? "Scroll = navegar janela · Ctrl+Scroll = Zoom · Clique = posicionar agulha · Shift+Arrastar IN/OUT = ajuste fino"
              : "Wheel = pan window · Ctrl+Wheel = Zoom · Click = seek playhead · Shift+Drag IN/OUT = fine trim"
          }
          style={{
            position: "relative",
            height: 36,
            background: "var(--bg-deep)",
            border: "1px solid var(--border-strong)",
            borderRadius: 3,
            cursor: draggingTarget === "track" ? "grabbing" : "grab",
            touchAction: "none",
            overflow: "visible",
          }}
          onPointerMove={(e) => {
            if (draggingTarget === "in" || draggingTarget === "out") return;
            const rect = e.currentTarget.getBoundingClientRect();
            setHoverX(e.clientX - rect.left);
            if (draggingTarget !== "track") {
              setHoverTs(clientXToTs(e.clientX));
            }
          }}
          onPointerLeave={() => {
            if (!draggingTarget) setHoverTs(null);
          }}
          onPointerCancel={() => {
            releaseActivePointerCapture();
            if (!draggingTarget) setHoverTs(null);
          }}
          onLostPointerCapture={() => {
            activePointerIdRef.current = null;
            activePointerElRef.current = null;
          }}
          onBlur={() => {
            if (!draggingTarget) setHoverTs(null);
          }}
          onPointerDown={(e) => {
            if ((e.target as HTMLElement).closest("[data-handle]")) return;
            try {
              e.currentTarget.setPointerCapture(e.pointerId);
              activePointerElRef.current = e.currentTarget;
              activePointerIdRef.current = e.pointerId;
            } catch {
              // ignore
            }
            dragOriginRef.current = {
              startX: e.clientX,
              startWinEnd: winEnd,
              moved: false,
            };
            setDraggingTarget("track");
          }}
        >
          <canvas
            ref={canvasRef}
            style={{
              position: "absolute",
              inset: 0,
              width: "100%",
              height: "100%",
              borderRadius: 3,
              pointerEvents: "none",
              display: "block",
            }}
          />

          {/* Off-Screen IN / OUT Jump Pills (non-overlapping, theme-token safe) */}
          {rangeMode &&
            clipGeometry.visible &&
            clipGeometry.offscreenIn === "left" && (
              <button
                type="button"
                data-handle="jump-in"
                className="nvr-btn mono sm"
                onClick={(e) => {
                  e.stopPropagation();
                  panWindowToTimestamp(rangeStart);
                }}
                title={
                  isPt
                    ? "IN está fora da tela à esquerda — clique para ir até o ponto IN"
                    : "IN handle is off-screen to the left — click to pan to IN"
                }
                style={{
                  position: "absolute",
                  left: 4,
                  top: 5,
                  height: 24,
                  padding: "0 6px",
                  zIndex: 12,
                  background: "var(--bg-panel)",
                  borderColor: "var(--signal-accent)",
                  color: "var(--signal-accent-text)",
                }}
              >
                ← IN {formatHHMMSS(rangeStart)}
              </button>
            )}
          {rangeMode &&
            !clipGeometry.visible &&
            clipGeometry.offscreenOut === "left" && (
              <button
                type="button"
                data-handle="jump-clip-left"
                className="nvr-btn mono sm"
                onClick={(e) => {
                  e.stopPropagation();
                  panWindowToTimestamp(rangeEnd);
                }}
                title={
                  isPt
                    ? "Trecho de recorte está à esquerda — clique para ir até o clipe"
                    : "Clip selection is off-screen to the left — click to pan to clip"
                }
                style={{
                  position: "absolute",
                  left: 4,
                  top: 5,
                  height: 24,
                  padding: "0 6px",
                  zIndex: 12,
                  background: "var(--bg-panel)",
                  borderColor: "var(--signal-accent)",
                  color: "var(--signal-accent-text)",
                }}
              >
                ← CLIP {formatHHMMSS(rangeStart)}
              </button>
            )}
          {rangeMode &&
            clipGeometry.visible &&
            clipGeometry.offscreenOut === "right" && (
              <button
                type="button"
                data-handle="jump-out"
                className="nvr-btn mono sm"
                onClick={(e) => {
                  e.stopPropagation();
                  panWindowToTimestamp(rangeEnd);
                }}
                title={
                  isPt
                    ? "OUT está fora da tela à direita — clique para ir até o ponto OUT"
                    : "OUT handle is off-screen to the right — click to pan to OUT"
                }
                style={{
                  position: "absolute",
                  right: 4,
                  top: 5,
                  height: 24,
                  padding: "0 6px",
                  zIndex: 12,
                  background: "var(--bg-panel)",
                  borderColor: "var(--signal-accent)",
                  color: "var(--signal-accent-text)",
                }}
              >
                OUT {formatHHMMSS(rangeEnd)} →
              </button>
            )}
          {rangeMode &&
            !clipGeometry.visible &&
            clipGeometry.offscreenIn === "right" && (
              <button
                type="button"
                data-handle="jump-clip-right"
                className="nvr-btn mono sm"
                onClick={(e) => {
                  e.stopPropagation();
                  panWindowToTimestamp(rangeStart);
                }}
                title={
                  isPt
                    ? "Trecho de recorte está à direita — clique para ir até o clipe"
                    : "Clip selection is off-screen to the right — click to pan to clip"
                }
                style={{
                  position: "absolute",
                  right: 4,
                  top: 5,
                  height: 24,
                  padding: "0 6px",
                  zIndex: 12,
                  background: "var(--bg-panel)",
                  borderColor: "var(--signal-accent)",
                  color: "var(--signal-accent-text)",
                }}
              >
                CLIP {formatHHMMSS(rangeStart)} →
              </button>
            )}

          {/* Visual Range Export Overlay (`[ IN ... OUT ]`) with Honest Draggable Handles */}
          {rangeMode && clipGeometry.visible && (
            <div
              style={{
                position: "absolute",
                left: `${clipGeometry.leftPct}%`,
                width: `${clipGeometry.widthPct}%`,
                top: 0,
                bottom: 0,
                background: "rgba(14, 165, 233, 0.20)",
                borderTop: "2px solid var(--signal-accent-text)",
                borderBottom: "2px solid var(--signal-accent-text)",
                borderLeft: clipGeometry.showInHandle
                  ? undefined
                  : "1px dashed var(--signal-accent-text)",
                borderRight: clipGeometry.showOutHandle
                  ? undefined
                  : "1px dashed var(--signal-accent-text)",
                pointerEvents: "none",
                zIndex: 8,
              }}
            >
              {clipGeometry.showInHandle && (
                <div
                  data-handle="in"
                  onPointerDown={(e) => {
                    e.stopPropagation();
                    try {
                      e.currentTarget.setPointerCapture(e.pointerId);
                      activePointerElRef.current = e.currentTarget;
                      activePointerIdRef.current = e.pointerId;
                    } catch {
                      // ignore
                    }
                    handleDragCursorRef.current = {
                      lastClientX: e.clientX,
                      currentTs: rangeStart,
                    };
                    setDraggingTarget("in");
                    setHoverTs(rangeStart);
                    setHoverX(tsToTrackX(rangeStart));
                    emitTrimState("dragging_in", rangeStart, rangeStart, rangeEnd);
                  }}
                  title={
                    isPt
                      ? "Arraste [ para ajustar INÍCIO (Segure Shift para ajuste fino)"
                      : "Drag [ to adjust START time (Hold Shift for fine precision)"
                  }
                  style={{
                    position: "absolute",
                    left: -7,
                    top: 0,
                    bottom: 0,
                    width: 14,
                    background: "var(--signal-accent)",
                    cursor: "ew-resize",
                    touchAction: "none",
                    pointerEvents: "auto",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    borderRadius: "2px 0 0 2px",
                  }}
                >
                  <span
                    className="mono"
                    style={{
                      fontSize: 10,
                      fontWeight: 800,
                      color: "#000000",
                      pointerEvents: "none",
                    }}
                  >
                    [
                  </span>
                </div>
              )}

              {clipGeometry.showOutHandle && (
                <div
                  data-handle="out"
                  onPointerDown={(e) => {
                    e.stopPropagation();
                    try {
                      e.currentTarget.setPointerCapture(e.pointerId);
                      activePointerElRef.current = e.currentTarget;
                      activePointerIdRef.current = e.pointerId;
                    } catch {
                      // ignore
                    }
                    handleDragCursorRef.current = {
                      lastClientX: e.clientX,
                      currentTs: rangeEnd,
                    };
                    setDraggingTarget("out");
                    setHoverTs(rangeEnd);
                    setHoverX(tsToTrackX(rangeEnd));
                    emitTrimState("dragging_out", rangeEnd, rangeStart, rangeEnd);
                  }}
                  title={
                    isPt
                      ? "Arraste ] para ajustar FIM (Segure Shift para ajuste fino)"
                      : "Drag ] to adjust END time (Hold Shift for fine precision)"
                  }
                  style={{
                    position: "absolute",
                    right: -7,
                    top: 0,
                    bottom: 0,
                    width: 14,
                    background: "var(--signal-accent)",
                    cursor: "ew-resize",
                    touchAction: "none",
                    pointerEvents: "auto",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    borderRadius: "0 2px 2px 0",
                  }}
                >
                  <span
                    className="mono"
                    style={{
                      fontSize: 10,
                      fontWeight: 800,
                      color: "#000000",
                      pointerEvents: "none",
                    }}
                  >
                    ]
                  </span>
                </div>
              )}
            </div>
          )}

          {/* Hover & Handle Scrub Crosshair + Timestamp-Accurate Frame Peeker */}
          {activePeekTs !== null && draggingTarget !== "track" && (
            <>
              <div
                style={{
                  position: "absolute",
                  left: `${hoverX}px`,
                  top: 0,
                  bottom: 0,
                  width: isDraggingTrimHandle ? 2 : 1,
                  background: isDraggingTrimHandle
                    ? "var(--signal-accent-text)"
                    : "var(--text-secondary)",
                  pointerEvents: "none",
                  zIndex: 9,
                }}
              />
              <div
                className="mono"
                style={{
                  position: "absolute",
                  left: `${Math.max(
                    85,
                    Math.min(
                      (trackRef.current?.clientWidth || 800) - 95,
                      hoverX
                    )
                  )}px`,
                  bottom: 44,
                  transform: "translateX(-50%)",
                  background: "var(--bg-panel)",
                  border: "1px solid var(--signal-accent)",
                  color: "var(--text-primary)",
                  padding: showThumbImg ? 5 : "3px 8px",
                  borderRadius: 3,
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  pointerEvents: "none",
                  zIndex: 30,
                  whiteSpace: "nowrap",
                  boxShadow: "0 6px 18px rgba(0,0,0,0.75)",
                  display: "flex",
                  flexDirection: "column",
                  alignItems: "center",
                  gap: 3,
                }}
              >
                {isDraggingTrimHandle && (
                  <span
                    style={{
                      color: "var(--signal-accent-text)",
                      fontSize: 10.5,
                    }}
                  >
                    {draggingTarget === "in"
                      ? isPt
                        ? "AJUSTANDO INÍCIO"
                        : "TRIMMING IN"
                      : isPt
                      ? "AJUSTANDO FIM"
                      : "TRIMMING OUT"}
                    {fineShiftActive ? " · PRECISION (SHIFT)" : ""} ·{" "}
                    {formatDuration(Math.max(1, rangeEnd - rangeStart))}
                  </span>
                )}
                {showThumbImg && effectiveHoverThumbUrl && (
                  <img
                    src={effectiveHoverThumbUrl}
                    alt="Timeline frame preview"
                    width={160}
                    height={90}
                    onError={() => setFailedThumbUrl(effectiveHoverThumbUrl)}
                    style={{
                      width: 160,
                      height: 90,
                      objectFit: "contain",
                      background: "#000000",
                      borderRadius: 2,
                    }}
                  />
                )}
                <span>{formatFullDateTime(Math.round(activePeekTs))}</span>
                {exactHoveredReview && (
                  <span
                    style={{
                      fontSize: 10.5,
                      color:
                        exactHoveredReview.severity === "alert"
                          ? "var(--signal-alert-text)"
                          : "var(--signal-motion-text)",
                    }}
                  >
                    {exactHoveredReview.severity.toUpperCase()}
                    {exactHoveredReview.data?.objects?.length
                      ? ` · ${exactHoveredReview.data.objects
                          .slice(0, 2)
                          .join(", ")
                          .toUpperCase()}`
                      : ""}
                  </span>
                )}
                {hoveredGapOnTrack && (
                  <span
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--signal-alert-text)",
                      fontVariantNumeric: "tabular-nums",
                    }}
                  >
                    OFFLINE GAP: {formatHHMMSS(hoveredGapOnTrack.start_time)} –{" "}
                    {formatHHMMSS(hoveredGapOnTrack.end_time)} (
                    {formatDuration(hoveredGapOnTrack.duration)})
                  </span>
                )}
                {!hoveredGapOnTrack && hoveredSegmentOnTrack === null && windowSegments.length > 0 && (
                  <span
                    style={{
                      fontSize: 10.5,
                      color: "var(--text-muted)",
                    }}
                  >
                    {isPt ? "Sem gravação neste instante" : "No recording at cursor"}
                  </span>
                )}
                {hoveredSegmentOnTrack &&
                  hoveredSegmentOnTrack.dbfs !== undefined &&
                  hoveredSegmentOnTrack.dbfs !== 0 && (
                    <span
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--signal-accent-text)",
                        fontVariantNumeric: "tabular-nums",
                      }}
                    >
                      AUDIO:{" "}
                      {hoveredSegmentOnTrack.dbfs < 0
                        ? `${hoveredSegmentOnTrack.dbfs} dBFS`
                        : `${hoveredSegmentOnTrack.dbfs - 90} dBFS`}
                    </span>
                  )}
              </div>
            </>
          )}

          {/* Active Playhead Needle */}
          {playheadPct >= 0 && playheadPct <= 100 && (
            <div
              style={{
                position: "absolute",
                left: `${playheadPct}%`,
                top: -2,
                bottom: -2,
                width: 2,
                background:
                  currentTs === null
                    ? "var(--signal-live)"
                    : "var(--signal-motion-text)",
                boxShadow: "0 0 6px rgba(0,0,0,0.8)",
                pointerEvents: "none",
                zIndex: 11,
              }}
            />
          )}
        </div>
      </div>
    </div>
  );
};
