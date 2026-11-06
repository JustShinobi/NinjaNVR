import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronLeft,
  ChevronRight,
  Download,
  Grid,
  Scissors,
  SkipBack,
  SkipForward,
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
import { useNvrStore } from "../store/useNvrStore";

export type TimelineZoomMinutes = 5 | 15 | 60 | 360 | 1440;

const ZOOM_LEVELS: TimelineZoomMinutes[] = [5, 15, 60, 360, 1440];

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
    label: string
  ) => Promise<{ exportId?: string; videoUrl?: string } | void>;
  titlePrefix?: string;
  defaultRangeMode?: boolean;
}

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
  const safeCam = encodeURIComponent(
    (fallbackCamera || "camera").trim() || "camera"
  );
  const fallbackUrl = `/api/${safeCam}/latest.webp`;
  if (!rawThumbPath || typeof rawThumbPath !== "string") {
    return fallbackUrl;
  }
  const stripped = rawThumbPath.trim().replace(/^\/media\/[^/]+/, "");
  if (
    !stripped ||
    /[\u0000-\u0020\u007f\\]/.test(stripped) ||
    stripped.startsWith("//") ||
    stripped.includes("..") ||
    /%2e/i.test(stripped) ||
    /%2f/i.test(stripped) ||
    /%5c/i.test(stripped) ||
    /%00/i.test(stripped)
  ) {
    return fallbackUrl;
  }
  if (
    stripped.startsWith("/clips/") ||
    stripped.startsWith("/api/") ||
    stripped.startsWith("/thumbs/") ||
    stripped.startsWith("/previews/")
  ) {
    return stripped;
  }
  return fallbackUrl;
}

function formatDayShort(dayStr: string): string {
  const parsed = parseStrictYmd(dayStr);
  if (!parsed) return dayStr;
  const d = new Date(parsed[0], parsed[1] - 1, parsed[2]);
  return d.toLocaleDateString([], { weekday: "short", month: "short", day: "2-digit" });
}

function toLocalDateInputValue(unixSec: number): string {
  const safeSec = Number.isFinite(unixSec) ? unixSec : Math.floor(Date.now() / 1000);
  const d = new Date(safeSec * 1000);
  if (Number.isNaN(d.getTime())) return "1970-01-01";
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function toLocalTimeInputValue(unixSec: number): string {
  const safeSec = Number.isFinite(unixSec) ? unixSec : Math.floor(Date.now() / 1000);
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
  const safeNow = Number.isFinite(nowSec) ? Math.floor(nowSec) : Math.floor(Date.now() / 1000);
  const todayStr = toLocalDateInputValue(safeNow);
  const parsed = parseStrictYmd(selectedDay);
  if (parsed) {
    const startMs = new Date(parsed[0], parsed[1] - 1, parsed[2], 0, 0, 0).getTime();
    if (Number.isFinite(startMs)) {
      const start = Math.floor(startMs / 1000);
      const fullEnd = start + 86399;
      const isToday = selectedDay.trim() === todayStr;
      const end = isToday ? Math.max(start + 300, Math.min(safeNow, fullEnd)) : fullEnd;
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
  const safeEnd = Number.isFinite(winEnd) && winEnd > safeStart ? winEnd : safeStart + 300;
  if (!Number.isFinite(rectWidth) || rectWidth <= 0 || !Number.isFinite(clientX) || !Number.isFinite(rectLeft)) {
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
}) => {
  const trackRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const themeMode = useNvrStore((s) => s.themeMode);
  const highContrast = useNvrStore((s) => s.highContrast);
  const effectiveTheme = resolveEffectiveTheme(themeMode);
  const [trackWidth, setTrackWidth] = useState<number>(800);
  const [zoomMin, setZoomMin] = useState<TimelineZoomMinutes>(60);
  const [nowSec, setNowSec] = useState<number>(() => Math.floor(Date.now() / 1000));
  // Explicit window end timestamp (null = locked to live `nowSec`)
  const [customWinEnd, setCustomWinEnd] = useState<number | null>(null);

  // Multi-Day Archive Summary & On-Demand Historical Segments/Reviews
  const [daysSummary, setDaysSummary] = useState<RecordingDaySummary[]>([]);
  const [selectedDay, setSelectedDay] = useState<string>(() =>
    toLocalDateInputValue(Math.floor(Date.now() / 1000))
  );
  const [selectedHour, setSelectedHour] = useState<string | null>(null);
  const [showArchiveDeck, setShowArchiveDeck] = useState<boolean>(false);
  const [showStoryboard, setShowStoryboard] = useState<boolean>(false);
  const [windowSegments, setWindowSegments] = useState<RecordingSegment[]>(initialSegments);
  const [windowReviews, setWindowReviews] = useState<ReviewItem[]>(initialReviews);
  const [hoveredStoryboardId, setHoveredStoryboardId] = useState<string | null>(null);

  // Hover state on fine timeline track
  const [hoverTs, setHoverTs] = useState<number | null>(null);
  const [hoverX, setHoverX] = useState<number>(0);

  // Range Export Selector state (`[ IN ... OUT ]`)
  const [rangeMode, setRangeMode] = useState<boolean>(defaultRangeMode);
  const [rangeStart, setRangeStart] = useState<number>(
    () => (currentTs ?? Math.floor(Date.now() / 1000)) - 90
  );
  const [rangeEnd, setRangeEnd] = useState<number>(
    () => (currentTs ?? Math.floor(Date.now() / 1000)) - 30
  );
  const [clipLabel, setClipLabel] = useState<string>("");
  const [exportBusy, setExportBusy] = useState<boolean>(false);
  const [readyDownloadUrl, setReadyDownloadUrl] = useState<string | null>(null);
  const [draggingTarget, setDraggingTarget] = useState<
    "track" | "in" | "out" | null
  >(null);
  const dragOriginRef = useRef<{
    startX: number;
    startWinEnd: number;
    startPlayhead: number;
    moved: boolean;
  } | null>(null);
  const activePointerIdRef = useRef<number | null>(null);
  const wheelSeekTimerRef = useRef<number | null>(null);
  const pendingWheelPlayheadRef = useRef<number | null>(null);
  const [scrubPreviewTs, setScrubPreviewTs] = useState<number | null>(null);
  const wheelAnimRef = useRef<{
    rafId: number | null;
    curWinEnd: number;
    targetWinEnd: number;
    curPlayhead: number;
    targetPlayhead: number;
  }>({
    rafId: null,
    curWinEnd: 0,
    targetWinEnd: 0,
    curPlayhead: 0,
    targetPlayhead: 0,
  });

  const releaseTrackPointerCapture = useCallback(() => {
    const pid = activePointerIdRef.current;
    activePointerIdRef.current = null;
    if (pid !== null && trackRef.current) {
      try {
        if (trackRef.current.hasPointerCapture?.(pid)) {
          trackRef.current.releasePointerCapture(pid);
        } else {
          trackRef.current.releasePointerCapture?.(pid);
        }
      } catch {
        // ignore
      }
    }
  }, []);

  // Ensure Zero-Decoder-Bloat: unmount any hovered storyboard preview video on deck close, camera switch, or tab hide/blur
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
      if (typeof document !== "undefined" && document.visibilityState !== "visible") {
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

  // Cleanup RAF, wheel seek timer, and pointer capture on unmount
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
      if (wheelSeekTimerRef.current !== null && typeof window !== "undefined") {
        window.clearTimeout(wheelSeekTimerRef.current);
        wheelSeekTimerRef.current = null;
      }
      releaseTrackPointerCapture();
    };
  }, [releaseTrackPointerCapture]);

  useEffect(() => {
    const timer = window.setInterval(() => {
      setNowSec(Math.floor(Date.now() / 1000));
    }, 2000);
    return () => window.clearInterval(timer);
  }, []);

  // Compute exact [00:00:00 .. 23:59:59 (or now)] bounds for the currently selected day
  const dayBounds = useMemo(
    () => parseLocalDayBounds(selectedDay, nowSec),
    [selectedDay, nowSec]
  );

  const daySpanSec = Math.max(300, dayBounds.dayEnd - dayBounds.dayStart);
  const windowSpanSec = Math.max(60, Math.min(zoomMin * 60, daySpanSec));
  const rawWinEnd = customWinEnd ?? dayBounds.dayEnd;
  const winEnd = Math.max(
    dayBounds.dayStart + windowSpanSec,
    Math.min(dayBounds.dayEnd, rawWinEnd)
  );
  const winStart = Math.max(dayBounds.dayStart, winEnd - windowSpanSec);
  const fetchBucketStart = Math.floor(dayBounds.dayStart / 15) * 15;
  const fetchBucketEnd = Math.ceil((dayBounds.dayEnd + 60) / 15) * 15;

  const clampTsToDay = useCallback(
    (ts: number): number =>
      Math.max(dayBounds.dayStart, Math.min(dayBounds.dayEnd, Math.round(ts))),
    [dayBounds]
  );

  const clampWinEndToDay = useCallback(
    (candidateEnd: number): number | null => {
      const clamped = Math.max(
        dayBounds.dayStart + windowSpanSec,
        Math.min(dayBounds.dayEnd, candidateEnd)
      );
      if (dayBounds.isToday && clamped >= nowSec - 1.5) {
        return null;
      }
      return clamped;
    },
    [dayBounds, windowSpanSec, nowSec]
  );

  // Clear local scrubPreviewTs once parent currentTs syncs
  useEffect(() => {
    setScrubPreviewTs(null);
  }, [currentTs]);

  // Re-center timeline window when parent seeks to a timestamp outside the current window
  useEffect(() => {
    if (currentTs === null || !Number.isFinite(currentTs)) return;
    const targetDay = toLocalDateInputValue(currentTs);
    if (targetDay !== selectedDay) {
      setSelectedDay(targetDay);
    }
    if (currentTs < winStart || currentTs > winEnd) {
      setCustomWinEnd(currentTs + Math.round(windowSpanSec * 0.5));
    }
  }, [currentTs]);

  // Fetch 30-day recording summary for the active camera
  useEffect(() => {
    if (!camera) return;
    let cancelled = false;
    fetch(`/api/${encodeURIComponent(camera)}/recordings/summary`, {
      credentials: "include",
    })
      .then((r) => (r.ok ? r.json() : []))
      .then((arr: RecordingDaySummary[]) => {
        if (!cancelled && Array.isArray(arr)) {
          setDaysSummary(arr);
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [camera]);

  // Fetch exact recording segments and review items covering `[fetchBucketStart, fetchBucketEnd]`
  useEffect(() => {
    if (!camera) return;
    let cancelled = false;

    Promise.all([
      fetch(
        `/api/${encodeURIComponent(camera)}/recordings?after=${fetchBucketStart}&before=${fetchBucketEnd}`,
        { credentials: "include" }
      ).then((r) => (r.ok ? r.json() : [])),
      fetch(
        `/api/review?cameras=${encodeURIComponent(
          camera
        )}&after=${fetchBucketStart}&before=${fetchBucketEnd}&limit=250`,
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
  }, [camera, fetchBucketStart, fetchBucketEnd]);

  const clientXToTs = useCallback(
    (clientX: number): number => {
      const el = trackRef.current;
      if (!el) return winEnd;
      const rect = el.getBoundingClientRect();
      return computeClientXTimestamp(clientX, rect.left, rect.width, winStart, winEnd);
    },
    [winStart, winEnd]
  );

  // Global pointer move / up when dragging timeline track (touch/mouse pan) or IN/OUT handles
  useEffect(() => {
    if (!draggingTarget) return;
    const onMove = (e: PointerEvent) => {
      if (draggingTarget === "track" && dragOriginRef.current) {
        const el = trackRef.current;
        const width = el?.clientWidth || 800;
        const dx = e.clientX - dragOriginRef.current.startX;
        if (Math.abs(dx) >= 5) {
          dragOriginRef.current.moved = true;
        }
        if (dragOriginRef.current.moved) {
          const deltaSec = -(dx / Math.max(1, width)) * windowSpanSec;
          setCustomWinEnd(
            clampWinEndToDay(dragOriginRef.current.startWinEnd + deltaSec)
          );
          const nextPh = clampTsToDay(dragOriginRef.current.startPlayhead + deltaSec);
          setScrubPreviewTs(nextPh);
          setHoverTs(nextPh);
        }
        return;
      }

      const ts = clampTsToDay(clientXToTs(e.clientX));
      if (draggingTarget === "in") {
        setRangeStart(Math.max(dayBounds.dayStart, Math.min(ts, rangeEnd - 5)));
        setReadyDownloadUrl(null);
      } else if (draggingTarget === "out") {
        setRangeEnd(Math.min(dayBounds.dayEnd, Math.max(ts, rangeStart + 5)));
        setReadyDownloadUrl(null);
      }
    };
    const onUp = (e: PointerEvent) => {
      releaseTrackPointerCapture();
      if (draggingTarget === "track" && dragOriginRef.current) {
        const origin = dragOriginRef.current;
        dragOriginRef.current = null;
        setDraggingTarget(null);
        if (!origin.moved) {
          const ts = clampTsToDay(clientXToTs(e.clientX));
          onSeek(dayBounds.isToday && ts >= nowSec - 3 ? null : ts);
          if (rangeMode && (rangeEnd < winStart || rangeStart > winEnd)) {
            setRangeStart(ts);
            setRangeEnd(Math.min(dayBounds.dayEnd, ts + 60));
            setReadyDownloadUrl(null);
          }
        } else {
          const el = trackRef.current;
          const width = el?.clientWidth || 800;
          const dx = e.clientX - origin.startX;
          const deltaSec = -Math.round((dx / Math.max(1, width)) * windowSpanSec);
          const targetTs = clampTsToDay(origin.startPlayhead + deltaSec);
          onSeek(dayBounds.isToday && targetTs >= nowSec - 3 ? null : targetTs);
        }
        return;
      }
      setDraggingTarget(null);
    };
    const onCancel = () => {
      releaseTrackPointerCapture();
      dragOriginRef.current = null;
      setDraggingTarget(null);
      setScrubPreviewTs(null);
      setHoverTs(null);
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
    clampTsToDay,
    clampWinEndToDay,
    dayBounds,
    nowSec,
    onSeek,
    rangeEnd,
    rangeMode,
    rangeStart,
    releaseTrackPointerCapture,
    winEnd,
    winStart,
    windowSpanSec,
  ]);

  // Native non-passive wheel listener on the timeline track for PC mouse-wheel scrubbing & Ctrl+wheel zoom
  useEffect(() => {
    const el = trackRef.current;
    if (!el) return;

    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const rawDelta =
        Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
      if (rawDelta === 0) return;

      // Ctrl + Wheel or Alt + Wheel = Zoom In / Out around hovered timestamp (clamped to selectedDay)
      if (e.ctrlKey || e.altKey) {
        const curIdx = ZOOM_LEVELS.indexOf(zoomMin);
        const nextIdx =
          rawDelta < 0
            ? Math.max(0, curIdx - 1)
            : Math.min(ZOOM_LEVELS.length - 1, curIdx + 1);
        const nextZoom = ZOOM_LEVELS[nextIdx];
        if (nextZoom !== zoomMin) {
          const anchor = clampTsToDay(clientXToTs(e.clientX));
          setZoomMin(nextZoom);
          const nextSpan = Math.min(nextZoom * 60, daySpanSec);
          const nextEnd = Math.max(
            dayBounds.dayStart + nextSpan,
            Math.min(dayBounds.dayEnd, anchor + Math.round(nextSpan * 0.5))
          );
          setCustomWinEnd(
            dayBounds.isToday && nextEnd >= nowSec - 2 ? null : nextEnd
          );
        }
        return;
      }

      // Normal Wheel = Smooth 60fps interpolated scrub/pan within selectedDay (Shift = fine step)
      const normalizedNotch = Math.max(
        -2.5,
        Math.min(2.5, rawDelta === 0 ? 0 : rawDelta / 100)
      );
      const sign = rawDelta > 0 ? 1 : -1;
      const notchFactor =
        Math.abs(normalizedNotch) < 0.25 ? sign * 0.25 : normalizedNotch;
      const deltaSec = e.shiftKey
        ? notchFactor * 8
        : notchFactor * Math.max(8, windowSpanSec * 0.025);

      const anim = wheelAnimRef.current;
      if (anim.rafId === null) {
        anim.curWinEnd = winEnd;
        anim.targetWinEnd = winEnd;
        const startPh = pendingWheelPlayheadRef.current ?? currentTs ?? winEnd;
        anim.curPlayhead = startPh;
        anim.targetPlayhead = startPh;
      }

      anim.targetWinEnd = Math.max(
        dayBounds.dayStart + windowSpanSec,
        Math.min(dayBounds.dayEnd, anim.targetWinEnd + deltaSec)
      );
      anim.targetPlayhead = Math.max(
        dayBounds.dayStart,
        Math.min(dayBounds.dayEnd, anim.targetPlayhead + deltaSec)
      );

      pendingWheelPlayheadRef.current = Math.round(anim.targetPlayhead);
      setHoverTs(Math.round(anim.targetPlayhead));

      const stepFrame = () => {
        const diffWin = anim.targetWinEnd - anim.curWinEnd;
        const diffPh = anim.targetPlayhead - anim.curPlayhead;
        if (Math.abs(diffWin) < 0.15 && Math.abs(diffPh) < 0.15) {
          anim.curWinEnd = anim.targetWinEnd;
          anim.curPlayhead = anim.targetPlayhead;
          anim.rafId = null;
          setCustomWinEnd(clampWinEndToDay(anim.curWinEnd));
          setScrubPreviewTs(Math.round(anim.curPlayhead));
          return;
        }
        anim.curWinEnd += diffWin * 0.32;
        anim.curPlayhead += diffPh * 0.32;
        setCustomWinEnd(clampWinEndToDay(anim.curWinEnd));
        setScrubPreviewTs(Math.round(anim.curPlayhead));
        anim.rafId = window.requestAnimationFrame(stepFrame);
      };

      if (anim.rafId === null) {
        anim.rafId = window.requestAnimationFrame(stepFrame);
      }

      if (wheelSeekTimerRef.current) {
        window.clearTimeout(wheelSeekTimerRef.current);
      }
      wheelSeekTimerRef.current = window.setTimeout(() => {
        const finalTarget = clampTsToDay(
          pendingWheelPlayheadRef.current ?? Math.round(anim.targetPlayhead)
        );
        pendingWheelPlayheadRef.current = null;
        onSeek(
          dayBounds.isToday && finalTarget >= nowSec - 3 ? null : finalTarget
        );
      }, 130);
    };

    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
    };
  }, [
    zoomMin,
    windowSpanSec,
    daySpanSec,
    dayBounds,
    winEnd,
    currentTs,
    nowSec,
    clientXToTs,
    clampTsToDay,
    clampWinEndToDay,
    onSeek,
  ]);

  // Jump to a specific Day from the Multi-Day Calendar
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

    // Pick the first active hour or noon of that day
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
    onSeek(targetTs);
    if (rangeMode) {
      setRangeStart(targetTs);
      setRangeEnd(targetTs + 60);
    }
  };

  // Jump to a specific Hour (`00`..`23`) within the selected Day
  const handleSelectHour = (dayStr: string, hourStr: string, hourMeta?: RecordingHourSummary) => {
    const parts = parseStrictYmd(dayStr || "");
    if (!parts) return;
    const hNum = parseInt(hourStr, 10) || 0;
    const localHourStart =
      hourMeta?.start_ts && hourMeta.start_ts > 0
        ? hourMeta.start_ts
        : Math.floor(new Date(parts[0], parts[1] - 1, parts[2], hNum, 0, 0).getTime() / 1000);
    if (!Number.isFinite(localHourStart)) return;
    setSelectedHour(hourStr);
    const localHourEnd = Math.min(nowSec, localHourStart + 3600);

    setZoomMin(60);
    setCustomWinEnd(localHourEnd);
    onSeek(localHourStart);
    if (rangeMode) {
      setRangeStart(localHourStart);
      setRangeEnd(Math.min(localHourEnd, localHourStart + 60));
    }
  };

  // Jump directly to a user-typed Date + Time (`YYYY-MM-DD` + `HH:MM:SS`)
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
    setCustomWinEnd(Math.min(nowSec, ts + Math.round(windowSpanSec * 0.5)));
    onSeek(ts >= nowSec - 3 ? null : ts);
    if (rangeMode) {
      setRangeStart(ts);
      setRangeEnd(Math.min(nowSec, ts + 60));
    }
  };

  // Activity-Only Skip: Jump to Previous or Next Event/Motion Segment within selectedDay
  const jumpToActivity = (direction: "prev" | "next") => {
    const anchor = clampTsToDay(currentTs ?? winEnd);
    const candidates: number[] = [];
    for (const r of windowReviews) {
      candidates.push(Math.floor(r.start_time));
    }
    for (const s of windowSegments) {
      const hasAudio =
        s.dbfs !== undefined && s.dbfs !== 0 && (s.dbfs < 0 ? s.dbfs > -50 : s.dbfs > 10);
      if (s.objects > 0 || s.motion > 0 || hasAudio) {
        candidates.push(Math.floor(s.start_time));
      }
    }
    candidates.sort((a, b) => a - b);

    if (direction === "prev") {
      const prev = [...candidates].reverse().find((t) => t < anchor - 8);
      const target = clampTsToDay(prev ?? anchor - 300);
      if (target < winStart) {
        setCustomWinEnd(clampWinEndToDay(target + Math.round(windowSpanSec * 0.8)));
      }
      onSeek(target);
      if (rangeMode) {
        setRangeStart(target);
        setRangeEnd(clampTsToDay(target + 45));
      }
    } else {
      const next = candidates.find((t) => t > anchor + 8);
      const target = clampTsToDay(next ?? anchor + 300);
      if (dayBounds.isToday && target >= nowSec - 3) {
        setCustomWinEnd(null);
        onSeek(null);
      } else {
        if (target > winEnd) {
          setCustomWinEnd(clampWinEndToDay(target + Math.round(windowSpanSec * 0.5)));
        }
        onSeek(target);
        if (rangeMode) {
          setRangeStart(target);
          setRangeEnd(clampTsToDay(target + 45));
        }
      }
    }
  };

  // Toggle Range Export Mode and initialize `[IN, OUT]` around current viewed window/playhead
  const toggleRangeSelector = () => {
    if (!rangeMode) {
      const anchor = currentTs ?? winEnd - 60;
      const s = Math.max(winStart, Math.min(winEnd - 15, anchor - 20));
      const e = Math.min(winEnd, s + 60);
      setRangeStart(Math.floor(s));
      setRangeEnd(Math.floor(e));
      setReadyDownloadUrl(null);
      setRangeMode(true);
    } else {
      setRangeMode(false);
    }
  };

  // World-space sliding clock ticks aligned to natural time intervals so ticks glide smoothly with the track
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
    const out: Array<{ pct: number; label: string }> = [];
    for (let ts = firstTick; ts <= winEnd; ts += tickStepSec) {
      const pct = ((ts - winStart) / windowSpanSec) * 100;
      if (pct < 2 || pct > 98) continue;
      out.push({
        pct,
        label:
          zoomMin >= 1440
            ? `${toLocalDateInputValue(ts).slice(5)} ${formatHHMM(ts)}`
            : formatHHMM(ts),
      });
    }
    return out;
  }, [winStart, winEnd, windowSpanSec, zoomMin]);

  const visibleReviews = useMemo(() => {
    return windowReviews.filter(
      (r) =>
        (r.camera === camera || camera === "all") &&
        (r.end_time || r.start_time + 15) >= winStart &&
        r.start_time <= winEnd
    );
  }, [windowReviews, camera, winStart, winEnd]);

  // Nearest review to hoverTs for thumbnail preview tooltip on the timeline
  const hoveredReviewOnTrack = useMemo(() => {
    if (hoverTs === null || visibleReviews.length === 0) return null;
    let best: ReviewItem | null = null;
    let bestDist = 180; // within 3 minutes
    for (const r of visibleReviews) {
      const mid = (r.start_time + (r.end_time || r.start_time + 15)) / 2;
      const dist = Math.abs(hoverTs - mid);
      if (dist < bestDist) {
        bestDist = dist;
        best = r;
      }
    }
    return best;
  }, [hoverTs, visibleReviews]);

  // Hovered recording segment for acoustic dBFS & motion telemetry on hover
  const hoveredSegmentOnTrack = useMemo(() => {
    if (hoverTs === null || windowSegments.length === 0) return null;
    for (const s of windowSegments) {
      if (hoverTs >= s.start_time && hoverTs <= s.end_time) {
        return s;
      }
    }
    return null;
  }, [hoverTs, windowSegments]);

  // Compute unrecorded offline gaps (> 25s) between consecutive recording segments
  const offlineGaps = useMemo(() => {
    const valid = windowSegments.filter(
      (s) => Number.isFinite(s.start_time) && Number.isFinite(s.end_time)
    );
    if (valid.length < 2) return [];
    const sorted = [...valid].sort((a, b) => a.start_time - b.start_time);
    const gaps: Array<{ start_time: number; end_time: number; duration: number }> = [];
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

  const hoveredGapOnTrack = useMemo(() => {
    if (hoverTs === null || offlineGaps.length === 0) return null;
    for (const g of offlineGaps) {
      if (hoverTs >= g.start_time && hoverTs <= g.end_time) {
        return g;
      }
    }
    return null;
  }, [hoverTs, offlineGaps]);

  const activeDaySummary = useMemo(
    () => daysSummary.find((d) => d.day === selectedDay) || daysSummary[0] || null,
    [daysSummary, selectedDay]
  );

  // Populate 24 hourly slots ("00" .. "23") for the selected day
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

  const effectivePlayheadTs = scrubPreviewTs ?? currentTs;

  const playheadPct =
    effectivePlayheadTs === null
      ? customWinEnd === null
        ? 99.6
        : -1
      : ((effectivePlayheadTs - winStart) / windowSpanSec) * 100;

  const rangeStartPct = Math.max(
    0,
    Math.min(100, ((rangeStart - winStart) / windowSpanSec) * 100)
  );
  const rangeEndPct = Math.max(
    0,
    Math.min(100, ((rangeEnd - winStart) / windowSpanSec) * 100)
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
      const x0 = Math.max(0, Math.min(w, ((seg.start_time - winStart) / windowSpanSec) * w));
      const safeDur =
        typeof seg.duration === "number" && Number.isFinite(seg.duration) && seg.duration > 0
          ? seg.duration
          : Math.max(1, seg.end_time - seg.start_time);
      const segW = Math.max(1.5, (safeDur / windowSpanSec) * w);

      // Continuous recording base lane (bottom 5px)
      ctx.fillStyle = palette.continuousBarFill;
      ctx.fillRect(x0, h - 5, segW, 5);

      // Acoustic activity layer (cyan/emerald waveform bar when segment has non-zero dBFS above ambient noise floor)
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
        ctx.fillRect(x0, h - 23, segW, 18);
      } else if (seg.motion > 0) {
        ctx.fillStyle = palette.motionBarFill;
        ctx.fillRect(x0, h - 15, segW, 10);
      }
    }

    // 2b. Hatched red/amber offline gap bars between consecutive recording segments
    for (const gap of offlineGaps) {
      if (gap.end_time < winStart || gap.start_time > winEnd) continue;
      const gx0 = Math.max(0, Math.min(w, ((gap.start_time - winStart) / windowSpanSec) * w));
      const gx1 = Math.max(0, Math.min(w, ((gap.end_time - winStart) / windowSpanSec) * w));
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

    // 3. Review event markers along upper track lane (top: 4px, height: 6px)
    for (const rev of visibleReviews) {
      const x0 = Math.max(0, Math.min(w, ((rev.start_time - winStart) / windowSpanSec) * w));
      const dur = Math.max(15, (rev.end_time || rev.start_time + 15) - rev.start_time);
      const revW = Math.max(3, (dur / windowSpanSec) * w);
      ctx.fillStyle =
        rev.severity === "alert" ? palette.alertPinFill : palette.motionPinFill;
      ctx.fillRect(x0, 4, revW, 6);
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
    try {
      const label =
        clipLabel.trim() ||
        `${camera}_${toLocalDateInputValue(rangeStart)}_${formatHHMMSS(rangeStart).replace(
          /:/g,
          ""
        )}`;
      const res = await onExportRange(camera, rangeStart, rangeEnd, label);
      if (res && res.videoUrl) {
        const candidate = res.videoUrl.trim();
        if (
          candidate &&
          !/[\u0000-\u0020\u007f\\]/.test(candidate) &&
          !candidate.startsWith("//") &&
          !candidate.includes("..") &&
          !/%2e/i.test(candidate) &&
          !/%2f/i.test(candidate) &&
          !/%5c/i.test(candidate) &&
          !/%00/i.test(candidate) &&
          (candidate.startsWith("/exports/") ||
            candidate.startsWith("/api/") ||
            candidate.startsWith("/vod/") ||
            candidate.startsWith("/clips/"))
        ) {
          setReadyDownloadUrl(candidate);
        }
      }
    } finally {
      setExportBusy(false);
    }
  };

  const displayAnchorTs = Math.round(effectivePlayheadTs ?? winEnd);

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
          {/* Row 1: Multi-Day Strip (12+ Days of History with Event Counts) + Direct Date/Time Jump */}
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
                1. SELECT DAY ({daysSummary.length}d):
              </span>
              {daysSummary.map((d) => {
                const isSelected = d.day === selectedDay;
                return (
                  <button
                    key={d.day}
                    className={`nvr-btn mono ${isSelected ? "active" : ""}`}
                    style={{
                      height: 26,
                      padding: "0 8px",
                      fontSize: "var(--text-xs)",
                      display: "inline-flex",
                      alignItems: "center",
                      gap: 6,
                    }}
                    onClick={() => handleSelectDay(d)}
                  >
                    <span style={{ fontWeight: 700 }}>{formatDayShort(d.day)}</span>
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

            {/* Interactive Calendar Popover + Time Jump & Storyboard Toggle */}
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
                className={`nvr-btn ${showStoryboard ? "primary" : ""}`}
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                onClick={() => setShowStoryboard((s) => !s)}
                title="Scan visual thumbnails across this day/hour to spot changes quickly"
              >
                <Grid size={12} />
                {showStoryboard ? "Hide Storyboard" : `Storyboard Scan (${windowReviews.length})`}
              </button>
              <button
                className="nvr-btn"
                style={{ height: 24, padding: "0 5px" }}
                onClick={() => setShowArchiveDeck(false)}
                aria-label="Collapse multi-day navigator"
                title="Collapse multi-day navigator"
              >
                <X size={12} />
              </button>
            </div>
          </div>

          {/* Row 2: 24-Hour Activity Heatmap Bar (00h .. 23h) for the Selected Day */}
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
              2. CLICK HOUR ({selectedDay}):
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
                      color: hasRecording ? "var(--text-primary)" : "var(--text-muted)",
                      opacity: hasRecording ? 1 : 0.4,
                      cursor: hasRecording ? "pointer" : "default",
                      display: "flex",
                      flexDirection: "column",
                      alignItems: "center",
                      justifyContent: "center",
                      padding: 1,
                    }}
                  >
                    <span style={{ fontSize: "var(--text-xs)", fontWeight: 700 }}>{hour}h</span>
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
                      {hasEvents ? `${meta?.events}ev` : hasRecording ? "rec" : "-"}
                    </span>
                  </button>
                );
              })}
            </div>
          </div>

          {/* Row 3: Visual Storyboard Filmstrip (Chronological Snapshots for Fast Visual Scanning) */}
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
                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                  No motion/object events recorded in this time window. Select another day or hour above.
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
                          setHoveredStoryboardId((c) => (c === rev.id ? null : c))
                        }
                        onPointerCancel={() =>
                          setHoveredStoryboardId((c) => (c === rev.id ? null : c))
                        }
                        onBlur={() =>
                          setHoveredStoryboardId((c) => (c === rev.id ? null : c))
                        }
                        onClick={() => {
                          const ts = Math.floor(rev.start_time);
                          setCustomWinEnd(Math.min(nowSec, ts + Math.round(windowSpanSec * 0.5)));
                          onSeek(ts);
                          setRangeStart(Math.max(0, ts - 5));
                          setRangeEnd(Math.min(nowSec, (rev.end_time || ts + 20) + 5));
                        }}
                        onKeyDown={(e) => {
                          if (e.key === "Enter" || e.key === " ") {
                            e.preventDefault();
                            const ts = Math.floor(rev.start_time);
                            setCustomWinEnd(
                              Math.min(nowSec, ts + Math.round(windowSpanSec * 0.5))
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
                        <div style={{ width: "100%", height: 74, position: "relative" }}>
                          {isHov ? (
                            <video
                              src={`/api/review/${encodeURIComponent(rev.id)}/preview?format=mp4`}
                              autoPlay
                              muted
                              loop
                              playsInline
                              onError={() =>
                                setHoveredStoryboardId((c) => (c === rev.id ? null : c))
                              }
                              style={{ width: "100%", height: "100%", objectFit: "contain" }}
                            />
                          ) : (
                            <img
                              src={thumbSrc}
                              alt={`${rev.camera} event at ${formatHHMMSS(rev.start_time)}`}
                              width={148}
                              height={74}
                              loading="lazy"
                              style={{ width: "100%", height: "100%", objectFit: "contain" }}
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
                          <span style={{ color: "var(--signal-accent-text)", fontWeight: 700 }}>
                            {formatHHMMSS(rev.start_time)}
                          </span>
                          <button
                            className="nvr-btn"
                            style={{ height: 20, padding: "0 6px", fontSize: 10.5 }}
                            title="Open Segment Clipper on this exact event"
                            onClick={(e) => {
                              e.stopPropagation();
                              const ts = Math.floor(rev.start_time);
                              onSeek(ts);
                              setRangeStart(Math.max(0, ts - 5));
                              setRangeEnd(
                                Math.min(nowSec, Math.ceil((rev.end_time || ts + 20) + 5))
                              );
                              setReadyDownloadUrl(null);
                              setRangeMode(true);
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
          MAIN TIMELINE CONTROL BAR
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
        {/* Left: Channel Selector + Multi-Day Calendar Button + Playhead & Activity Jump */}
        <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
          {titlePrefix && (
            <span
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
                    className={`nvr-btn mono ${camera === sc ? "active" : ""}`}
                    style={{
                      height: 22,
                      padding: "0 7px",
                      fontSize: "var(--text-xs)",
                      textTransform: "uppercase",
                    }}
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
              className="nvr-btn mono"
              style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
              title="Expand active timeline camera into Focus stage view"
              onClick={() => onFocusCamera(camera)}
            >
              ⤢ Focus
            </button>
          )}

          {/* Direct Recording Calendar Popover (neutral when closed) */}
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

          {/* 24h Activity Matrix Toggle: neutral when closed, accent when open */}
          <button
            className={`nvr-btn ${showArchiveDeck ? "active" : ""}`}
            style={{
              height: 26,
              padding: "0 9px",
              fontSize: "var(--text-xs)",
            }}
            onClick={() => setShowArchiveDeck((v) => !v)}
          >
            <Grid size={12} />
            {showArchiveDeck
              ? "Hide 24h Matrix"
              : `24h Activity Matrix (${daysSummary.length || 1}d)`}
          </button>

          <span
            className={`nvr-badge ${effectivePlayheadTs === null ? "live" : "rewind"}`}
            style={{ height: 24 }}
          >
            <span className={`nvr-dot ${effectivePlayheadTs === null ? "live" : "motion"}`} />
            {effectivePlayheadTs === null
              ? "LIVE EDGE"
              : formatFullDateTime(Math.round(effectivePlayheadTs))}
          </span>

          {/* Skip Silence: Prev / Next Activity Event */}
          <button
            className="nvr-btn"
            style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
            onClick={() => jumpToActivity("prev")}
            title="Jump to previous motion or person event (skip empty gaps)"
          >
            <SkipBack size={11} />
            Prev Event
          </button>
          <button
            className="nvr-btn"
            style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
            onClick={() => jumpToActivity("next")}
            title="Jump to next motion or person event (skip empty gaps)"
          >
            Next Event
            <SkipForward size={11} />
          </button>

          <button
            className="nvr-btn mono"
            style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
            onClick={() => onSeek(clampTsToDay((currentTs ?? winEnd) - 30))}
          >
            -30s
          </button>
          <button
            className="nvr-btn mono"
            style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
            onClick={() => {
              const next = clampTsToDay((currentTs ?? winEnd) + 30);
              onSeek(dayBounds.isToday && next >= nowSec - 2 ? null : next);
            }}
          >
            +30s
          </button>
          {currentTs !== null && (
            <button
              className="nvr-btn success"
              style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
              onClick={() => {
                setSelectedDay(toLocalDateInputValue(nowSec));
                setCustomWinEnd(null);
                onSeek(null);
              }}
            >
              Return to Live
            </button>
          )}
        </div>

        {/* Right: Zoom (5m / 15m / 1h / 6h / 24h), Pan, and Segment Clipper Toggle */}
        <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
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
                className={`nvr-btn mono ${zoomMin === z.min ? "active" : ""}`}
                style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  setZoomMin(z.min);
                  if (currentTs !== null) {
                    const spanSec = z.min * 60;
                    setCustomWinEnd(
                      Math.max(
                        dayBounds.dayStart + spanSec,
                        Math.min(dayBounds.dayEnd, Math.round(currentTs + spanSec / 2))
                      )
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
              className="nvr-btn"
              style={{ height: 24, padding: "0 5px" }}
              aria-label="Pan timeline earlier within selected day"
              title="Pan timeline earlier within selected day"
              disabled={winStart <= dayBounds.dayStart}
              onClick={() =>
                setCustomWinEnd(
                  clampWinEndToDay(winEnd - Math.round(windowSpanSec * 0.5))
                )
              }
            >
              <ChevronLeft size={12} />
            </button>
            <button
              className="nvr-btn"
              style={{ height: 24, padding: "0 5px" }}
              aria-label="Pan timeline later within selected day"
              title="Pan timeline later within selected day"
              disabled={winEnd >= dayBounds.dayEnd}
              onClick={() =>
                setCustomWinEnd(
                  clampWinEndToDay(winEnd + Math.round(windowSpanSec * 0.5))
                )
              }
            >
              <ChevronRight size={12} />
            </button>
          </div>

          <button
            className={`nvr-btn ${rangeMode ? "primary" : ""}`}
            style={{
              height: 26,
              padding: "0 10px",
              fontSize: "var(--text-xs)",
            }}
            onClick={toggleRangeSelector}
          >
            <Scissors size={12} />
            {rangeMode ? "Close Segment Clipper" : "Select Segment to Export"}
          </button>
        </div>
      </div>

      {/* =====================================================================
          INTERACTIVE SEGMENT EXPORT CONTROL BAR (`[ IN ... OUT ]`)
         ===================================================================== */}
      {rangeMode && (
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 8,
            padding: "5px 10px",
            background: "rgba(14, 165, 233, 0.08)",
            border: "1px solid rgba(14, 165, 233, 0.4)",
            borderRadius: 3,
            flexWrap: "wrap",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
            <span
              className="mono"
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                color: "var(--signal-accent-text)",
              }}
            >
              EXPORT [{camera.toUpperCase()} · {toLocalDateInputValue(rangeStart)}]:
            </span>

            {/* IN Start Controls */}
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              <span style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                IN:
              </span>
              <button
                className="nvr-btn mono"
                style={{ height: 22, padding: "0 5px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  setRangeStart((s) => Math.max(dayBounds.dayStart, s - 10));
                  setReadyDownloadUrl(null);
                }}
              >
                -10s
              </button>
              <span
                className="mono"
                style={{
                  padding: "2px 6px",
                  background: "var(--bg-deep)",
                  border: "1px solid var(--border-strong)",
                  borderRadius: 2,
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-motion-text)",
                }}
              >
                {formatHHMMSS(rangeStart)}
              </span>
              <button
                className="nvr-btn mono"
                style={{ height: 22, padding: "0 5px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  setRangeStart((s) => Math.min(rangeEnd - 5, s + 10));
                  setReadyDownloadUrl(null);
                }}
              >
                +10s
              </button>
              <button
                className="nvr-btn"
                style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                title="Set IN point to current playhead"
                onClick={() => {
                  const anchor = Math.round(effectivePlayheadTs ?? winEnd - 60);
                  setRangeStart(anchor);
                  if (rangeEnd <= anchor + 5 || rangeEnd - anchor > 600) {
                    setRangeEnd(Math.min(dayBounds.dayEnd, anchor + 60));
                  }
                  setReadyDownloadUrl(null);
                }}
              >
                [ Set IN
              </button>
            </div>

            <span style={{ color: "var(--border-strong)" }}>→</span>

            {/* OUT End Controls */}
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              <span style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                OUT:
              </span>
              <button
                className="nvr-btn mono"
                style={{ height: 22, padding: "0 5px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  setRangeEnd((e) => Math.max(rangeStart + 5, e - 10));
                  setReadyDownloadUrl(null);
                }}
              >
                -10s
              </button>
              <span
                className="mono"
                style={{
                  padding: "2px 6px",
                  background: "var(--bg-deep)",
                  border: "1px solid var(--border-strong)",
                  borderRadius: 2,
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-motion-text)",
                }}
              >
                {formatHHMMSS(rangeEnd)}
              </span>
              <button
                className="nvr-btn mono"
                style={{ height: 22, padding: "0 5px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  setRangeEnd((e) => Math.min(nowSec, e + 10));
                  setReadyDownloadUrl(null);
                }}
              >
                +10s
              </button>
              <button
                className="nvr-btn"
                style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                title="Set OUT point to current playhead"
                onClick={() => {
                  const anchor = Math.round(effectivePlayheadTs ?? winEnd);
                  setRangeEnd(anchor);
                  if (rangeStart >= anchor - 5 || anchor - rangeStart > 600) {
                    setRangeStart(Math.max(dayBounds.dayStart, anchor - 60));
                  }
                  setReadyDownloadUrl(null);
                }}
              >
                Set OUT ]
              </button>
            </div>

            <span className="nvr-badge" style={{ color: "var(--signal-accent-text)" }}>
              {formatDuration(rangeEnd - rangeStart)}
            </span>

            {(
              [
                { sec: 30, label: "30s" },
                { sec: 60, label: "1m" },
                { sec: 180, label: "3m" },
                { sec: 300, label: "5m" },
              ] as const
            ).map((p) => (
              <button
                key={p.sec}
                className="nvr-btn mono"
                style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  setRangeEnd(Math.min(nowSec, rangeStart + p.sec));
                  setReadyDownloadUrl(null);
                }}
              >
                {p.label}
              </button>
            ))}
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <input
              className="nvr-input"
              style={{ height: 24, width: 145, fontSize: "var(--text-xs)" }}
              placeholder="Clip label (optional)"
              aria-label="Clip export label"
              value={clipLabel}
              onChange={(e) => setClipLabel(e.target.value)}
            />
            <button
              className="nvr-btn"
              style={{ height: 24, fontSize: "var(--text-xs)" }}
              onClick={() => onSeek(rangeStart)}
              title="Preview selected segment from IN point"
            >
              Preview
            </button>
            <button
              className="nvr-btn success"
              style={{ height: 24, fontSize: "var(--text-xs)", fontWeight: 600 }}
              disabled={exportBusy}
              onClick={handleTriggerExport}
            >
              {exportBusy
                ? "Remuxing MP4..."
                : `Export MP4 (${formatDuration(rangeEnd - rangeStart)})`}
            </button>
            {readyDownloadUrl && (
              <a
                href={readyDownloadUrl}
                download
                className="nvr-btn primary"
                style={{
                  height: 24,
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  textDecoration: "none",
                }}
              >
                <Download size={12} />
                Download MP4
              </a>
            )}
            <button
              className="nvr-btn"
              style={{ height: 24, padding: "0 5px" }}
              onClick={() => setRangeMode(false)}
              aria-label="Close segment clipper"
              title="Close segment clipper"
            >
              <X size={12} />
            </button>
          </div>
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
              key={tk.label}
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
            HIGH-DPI CANVAS SEEK TRACK + RANGE CLIPPER + PLAYHEAD NEEDLE
           =================================================================== */}
        <div
          ref={trackRef}
          className="nvr-timeline-track"
          title="Use mouse wheel to scrub time (Shift+Wheel = 10s fine step, Ctrl+Wheel = Zoom) · Drag to pan · Click/Tap to seek"
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
            releaseTrackPointerCapture();
            if (!draggingTarget) setHoverTs(null);
          }}
          onLostPointerCapture={() => {
            activePointerIdRef.current = null;
          }}
          onBlur={() => {
            if (!draggingTarget) setHoverTs(null);
          }}
          onPointerDown={(e) => {
            if ((e.target as HTMLElement).dataset.handle) return;
            try {
              e.currentTarget.setPointerCapture(e.pointerId);
              activePointerIdRef.current = e.pointerId;
            } catch {
              // ignore
            }
            dragOriginRef.current = {
              startX: e.clientX,
              startWinEnd: winEnd,
              startPlayhead: currentTs ?? winEnd,
              moved: false,
            };
            setDraggingTarget("track");
          }}
        >
          {/* High-DPI 2D Canvas for recording segments, activity bars, and event markers */}
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

          {/* Visual Range Export Overlay (`[ IN ... OUT ]`) with Draggable Handles */}
          {rangeMode && (
            <div
              style={{
                position: "absolute",
                left: `${rangeStartPct}%`,
                width: `${Math.max(0.5, rangeEndPct - rangeStartPct)}%`,
                top: 0,
                bottom: 0,
                background: "rgba(14, 165, 233, 0.20)",
                borderTop: "2px solid var(--signal-accent-text)",
                borderBottom: "2px solid var(--signal-accent-text)",
                zIndex: 8,
              }}
            >
              <div
                data-handle="in"
                onPointerDown={(e) => {
                  e.stopPropagation();
                  setDraggingTarget("in");
                }}
                title="Drag IN handle to adjust export start time"
                style={{
                  position: "absolute",
                  left: -7,
                  top: 0,
                  bottom: 0,
                  width: 14,
                  background: "var(--signal-accent)",
                  cursor: "ew-resize",
                  touchAction: "none",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  borderRadius: "2px 0 0 2px",
                }}
              >
                <span
                  className="mono"
                  style={{ fontSize: 10, fontWeight: 800, color: "#000000", pointerEvents: "none" }}
                >
                  [
                </span>
              </div>

              <div
                data-handle="out"
                onPointerDown={(e) => {
                  e.stopPropagation();
                  setDraggingTarget("out");
                }}
                title="Drag OUT handle to adjust export end time"
                style={{
                  position: "absolute",
                  right: -7,
                  top: 0,
                  bottom: 0,
                  width: 14,
                  background: "var(--signal-accent)",
                  cursor: "ew-resize",
                  touchAction: "none",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  borderRadius: "0 2px 2px 0",
                }}
              >
                <span
                  className="mono"
                  style={{ fontSize: 10, fontWeight: 800, color: "#000000", pointerEvents: "none" }}
                >
                  ]
                </span>
              </div>
            </div>
          )}

          {/* Hover Scrub Crosshair + Floating Thumbnail & Timestamp Peeker */}
          {hoverTs !== null && (
            <>
              <div
                style={{
                  position: "absolute",
                  left: `${hoverX}px`,
                  top: 0,
                  bottom: 0,
                  width: 1,
                  background: "rgba(255, 255, 255, 0.55)",
                  pointerEvents: "none",
                  zIndex: 9,
                }}
              />
              <div
                className="mono"
                style={{
                  position: "absolute",
                  left: `${Math.max(
                    75,
                    Math.min((trackRef.current?.clientWidth || 800) - 95, hoverX)
                  )}px`,
                  bottom: 58,
                  transform: "translateX(-50%)",
                  background: "var(--bg-panel)",
                  border: "1px solid var(--signal-accent)",
                  color: "var(--text-primary)",
                  padding: hoveredReviewOnTrack ? 4 : "2px 7px",
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
                {hoveredReviewOnTrack && (
                  <img
                    src={sanitizeTimelineThumbUrl(
                      hoveredReviewOnTrack.thumb_path,
                      hoveredReviewOnTrack.camera
                    )}
                    alt="preview"
                    width={128}
                    height={72}
                    style={{
                      width: 128,
                      height: 72,
                      objectFit: "contain",
                      background: "#000000",
                      borderRadius: 2,
                    }}
                  />
                )}
                <span>{formatFullDateTime(hoverTs)}</span>
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
                  currentTs === null ? "var(--signal-live)" : "var(--signal-motion-text)",
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
