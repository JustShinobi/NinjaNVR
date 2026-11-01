import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Calendar,
  ChevronLeft,
  ChevronRight,
  Clock,
  Download,
  Film,
  Grid,
  Play,
  Scissors,
  SkipBack,
  SkipForward,
  X,
  ZoomIn,
} from "lucide-react";
import {
  RecordingDaySummary,
  RecordingHourSummary,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import { RecordingCalendarPopover } from "./RecordingCalendarPopover";

export type TimelineZoomMinutes = 5 | 15 | 60 | 360 | 1440;

const ZOOM_LEVELS: TimelineZoomMinutes[] = [5, 15, 60, 360, 1440];

interface TimelineScrubberProps {
  camera: string;
  cameras?: string[];
  onSelectCamera?: (cam: string) => void;
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
}

function formatHHMM(unixSec: number): string {
  const d = new Date(unixSec * 1000);
  return d.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

function formatHHMMSS(unixSec: number): string {
  const d = new Date(unixSec * 1000);
  return d.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

function formatFullDateTime(unixSec: number): string {
  const d = new Date(unixSec * 1000);
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

function formatDayShort(dayStr: string): string {
  const parts = dayStr.split("-").map(Number);
  if (parts.length !== 3) return dayStr;
  const d = new Date(parts[0], parts[1] - 1, parts[2]);
  return d.toLocaleDateString([], { weekday: "short", month: "short", day: "2-digit" });
}

function formatDuration(sec: number): string {
  const total = Math.max(1, Math.round(sec));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m === 0) return `${s}s`;
  return `${m}m ${s.toString().padStart(2, "0")}s`;
}

function toLocalDateInputValue(unixSec: number): string {
  const d = new Date(unixSec * 1000);
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function toLocalTimeInputValue(unixSec: number): string {
  const d = new Date(unixSec * 1000);
  const h = String(d.getHours()).padStart(2, "0");
  const m = String(d.getMinutes()).padStart(2, "0");
  const s = String(d.getSeconds()).padStart(2, "0");
  return `${h}:${m}:${s}`;
}

export const TimelineScrubber: React.FC<TimelineScrubberProps> = ({
  camera,
  cameras,
  onSelectCamera,
  segments: initialSegments,
  reviews: initialReviews,
  currentTs,
  onSeek,
  onExportRange,
  titlePrefix,
}) => {
  const trackRef = useRef<HTMLDivElement | null>(null);
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
  const [rangeMode, setRangeMode] = useState<boolean>(false);
  const [rangeStart, setRangeStart] = useState<number>(() => Math.floor(Date.now() / 1000) - 90);
  const [rangeEnd, setRangeEnd] = useState<number>(() => Math.floor(Date.now() / 1000) - 30);
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
  const wheelSeekTimerRef = useRef<number | null>(null);
  const pendingWheelPlayheadRef = useRef<number | null>(null);

  useEffect(() => {
    const timer = window.setInterval(() => {
      setNowSec(Math.floor(Date.now() / 1000));
    }, 2000);
    return () => window.clearInterval(timer);
  }, []);

  const windowSpanSec = zoomMin * 60;
  const winEnd = customWinEnd ?? nowSec;
  const winStart = winEnd - windowSpanSec;
  const fetchBucketStart = Math.floor((winStart - 1800) / 15) * 15;
  const fetchBucketEnd = Math.ceil((winEnd + 1800) / 15) * 15;

  // Re-center timeline window when parent seeks to a timestamp outside the current window
  useEffect(() => {
    if (currentTs === null) return;
    setSelectedDay(toLocalDateInputValue(currentTs));
    if (currentTs < winStart || currentTs > winEnd) {
      setCustomWinEnd(Math.min(nowSec, currentTs + Math.round(windowSpanSec * 0.5)));
    }
  }, [currentTs]);

  // Fetch 30-day recording summary for the active camera
  useEffect(() => {
    if (!camera) return;
    fetch(`/api/${encodeURIComponent(camera)}/recordings/summary`, {
      credentials: "include",
    })
      .then((r) => (r.ok ? r.json() : []))
      .then((arr: RecordingDaySummary[]) => {
        if (Array.isArray(arr) && arr.length > 0) {
          setDaysSummary(arr);
        }
      })
      .catch(() => {});
  }, [camera]);

  // Fetch exact recording segments and review items covering `[fetchBucketStart, fetchBucketEnd]`
  // so historical days from 2, 5, or 12 days ago always have full segment & event data!
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
      const frac = Math.max(0, Math.min(1, (clientX - rect.left) / Math.max(1, rect.width)));
      return Math.round(winStart + frac * windowSpanSec);
    },
    [winStart, winEnd, windowSpanSec]
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
          const deltaSec = -Math.round((dx / Math.max(1, width)) * windowSpanSec);
          const nextWinEnd = Math.min(
            nowSec,
            dragOriginRef.current.startWinEnd + deltaSec
          );
          setCustomWinEnd(nextWinEnd >= nowSec - 2 ? null : nextWinEnd);
          setHoverTs(
            Math.min(nowSec, dragOriginRef.current.startPlayhead + deltaSec)
          );
        }
        return;
      }

      const ts = clientXToTs(e.clientX);
      if (draggingTarget === "in") {
        setRangeStart(Math.min(ts, rangeEnd - 5));
        setReadyDownloadUrl(null);
      } else if (draggingTarget === "out") {
        setRangeEnd(Math.max(ts, rangeStart + 5));
        setReadyDownloadUrl(null);
      }
    };
    const onUp = (e: PointerEvent) => {
      if (draggingTarget === "track" && dragOriginRef.current) {
        const origin = dragOriginRef.current;
        dragOriginRef.current = null;
        setDraggingTarget(null);
        if (!origin.moved) {
          const ts = clientXToTs(e.clientX);
          onSeek(ts >= nowSec - 3 ? null : ts);
        } else {
          const el = trackRef.current;
          const width = el?.clientWidth || 800;
          const dx = e.clientX - origin.startX;
          const deltaSec = -Math.round((dx / Math.max(1, width)) * windowSpanSec);
          const targetTs = Math.min(nowSec, origin.startPlayhead + deltaSec);
          onSeek(targetTs >= nowSec - 3 ? null : targetTs);
        }
        return;
      }
      setDraggingTarget(null);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onUp);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onUp);
    };
  }, [draggingTarget, clientXToTs, nowSec, onSeek, rangeEnd, rangeStart, windowSpanSec]);

  // Native non-passive wheel listener on the timeline track for PC mouse-wheel scrubbing & Ctrl+wheel zoom
  useEffect(() => {
    const el = trackRef.current;
    if (!el) return;

    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const rawDelta =
        Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
      if (rawDelta === 0) return;

      // Ctrl + Wheel or Alt + Wheel = Zoom In / Out around hovered timestamp
      if (e.ctrlKey || e.altKey) {
        const curIdx = ZOOM_LEVELS.indexOf(zoomMin);
        const nextIdx =
          rawDelta < 0
            ? Math.max(0, curIdx - 1)
            : Math.min(ZOOM_LEVELS.length - 1, curIdx + 1);
        const nextZoom = ZOOM_LEVELS[nextIdx];
        if (nextZoom !== zoomMin) {
          const anchor = clientXToTs(e.clientX);
          setZoomMin(nextZoom);
          const nextSpan = nextZoom * 60;
          const nextEnd = Math.min(nowSec, anchor + Math.round(nextSpan * 0.5));
          setCustomWinEnd(nextEnd >= nowSec - 2 ? null : nextEnd);
        }
        return;
      }

      // Normal Wheel = Scrub / Pan through time (Shift = fine 10s step)
      const dir = rawDelta > 0 ? 1 : -1;
      const stepSec = e.shiftKey
        ? 10
        : Math.max(15, Math.round(windowSpanSec * 0.08));

      const nextWinEnd = Math.min(nowSec, winEnd + dir * stepSec);
      setCustomWinEnd(nextWinEnd >= nowSec - 2 ? null : nextWinEnd);

      const basePlayhead =
        pendingWheelPlayheadRef.current ?? currentTs ?? winEnd;
      const nextPlayhead = Math.min(nowSec, basePlayhead + dir * stepSec);
      pendingWheelPlayheadRef.current = nextPlayhead;
      setHoverTs(nextPlayhead);

      if (wheelSeekTimerRef.current) {
        window.clearTimeout(wheelSeekTimerRef.current);
      }
      wheelSeekTimerRef.current = window.setTimeout(() => {
        const finalTarget = pendingWheelPlayheadRef.current ?? nextPlayhead;
        pendingWheelPlayheadRef.current = null;
        onSeek(finalTarget >= nowSec - 3 ? null : finalTarget);
      }, 140);
    };

    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
    };
  }, [zoomMin, windowSpanSec, winEnd, currentTs, nowSec, clientXToTs, onSeek]);

  // Jump to a specific Day from the Multi-Day Calendar
  const handleSelectDay = (dayObj: RecordingDaySummary) => {
    setSelectedDay(dayObj.day);
    setSelectedHour(null);
    const parts = dayObj.day.split("-").map(Number);
    const localDayStart = new Date(parts[0], parts[1] - 1, parts[2], 0, 0, 0).getTime() / 1000;
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
    setSelectedHour(hourStr);
    const parts = dayStr.split("-").map(Number);
    const hNum = parseInt(hourStr, 10) || 0;
    const localHourStart =
      hourMeta?.start_ts && hourMeta.start_ts > 0
        ? hourMeta.start_ts
        : new Date(parts[0], parts[1] - 1, parts[2], hNum, 0, 0).getTime() / 1000;
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
    const dParts = dateStr.split("-").map(Number);
    const tParts = timeStr.split(":").map(Number);
    if (dParts.length !== 3 || tParts.length < 2) return;
    const dt = new Date(
      dParts[0],
      dParts[1] - 1,
      dParts[2],
      tParts[0] || 0,
      tParts[1] || 0,
      tParts[2] || 0
    );
    const ts = Math.floor(dt.getTime() / 1000);
    if (Number.isNaN(ts) || ts <= 0) return;
    setSelectedDay(dateStr);
    setSelectedHour(String(tParts[0] || 0).padStart(2, "0"));
    setCustomWinEnd(Math.min(nowSec, ts + Math.round(windowSpanSec * 0.5)));
    onSeek(ts >= nowSec - 3 ? null : ts);
    if (rangeMode) {
      setRangeStart(ts);
      setRangeEnd(Math.min(nowSec, ts + 60));
    }
  };

  // Activity-Only Skip: Jump to Previous or Next Event/Motion Segment (skips quiet hours!)
  const jumpToActivity = (direction: "prev" | "next") => {
    const anchor = currentTs ?? nowSec;
    const candidates: number[] = [];
    for (const r of windowReviews) {
      candidates.push(Math.floor(r.start_time));
    }
    for (const s of windowSegments) {
      if (s.objects > 0 || s.motion > 0) {
        candidates.push(Math.floor(s.start_time));
      }
    }
    candidates.sort((a, b) => a - b);

    if (direction === "prev") {
      const prev = [...candidates].reverse().find((t) => t < anchor - 8);
      const target = prev ?? anchor - 300;
      if (target < winStart) {
        setCustomWinEnd(target + Math.round(windowSpanSec * 0.8));
      }
      onSeek(target);
      if (rangeMode) {
        setRangeStart(target);
        setRangeEnd(target + 45);
      }
    } else {
      const next = candidates.find((t) => t > anchor + 8);
      const fallbackNext = Math.min(nowSec, anchor + 300);
      const target = next ?? fallbackNext;
      if (target >= nowSec - 3) {
        setCustomWinEnd(null);
        onSeek(null);
      } else {
        if (target > winEnd) {
          setCustomWinEnd(Math.min(nowSec, target + Math.round(windowSpanSec * 0.5)));
        }
        onSeek(target);
        if (rangeMode) {
          setRangeStart(target);
          setRangeEnd(Math.min(nowSec, target + 45));
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

  // Time ruler tick marks (8 ticks across the track)
  const ticks = useMemo(() => {
    const count = 8;
    const step = windowSpanSec / count;
    const out: Array<{ pct: number; label: string }> = [];
    for (let i = 0; i <= count; i++) {
      const ts = Math.round(winStart + i * step);
      out.push({
        pct: (i / count) * 100,
        label: zoomMin >= 1440 ? `${toLocalDateInputValue(ts).slice(5)} ${formatHHMM(ts)}` : formatHHMM(ts),
      });
    }
    return out;
  }, [winStart, windowSpanSec, zoomMin]);

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

  const activeDaySummary = useMemo(
    () => daysSummary.find((d) => d.day === selectedDay) || daysSummary[0] || null,
    [daysSummary, selectedDay]
  );

  // Build 24 hour slots ("00" .. "23") for the selected day
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

  const playheadPct =
    currentTs === null
      ? customWinEnd === null
        ? 99.6
        : -1
      : ((currentTs - winStart) / windowSpanSec) * 100;

  const rangeStartPct = Math.max(
    0,
    Math.min(100, ((rangeStart - winStart) / windowSpanSec) * 100)
  );
  const rangeEndPct = Math.max(
    0,
    Math.min(100, ((rangeEnd - winStart) / windowSpanSec) * 100)
  );

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
        setReadyDownloadUrl(res.videoUrl);
      } else {
        const id = `${camera}_${Math.floor(rangeStart)}_${Math.floor(rangeEnd)}`;
        setReadyDownloadUrl(`/exports/${id}.mp4`);
      }
    } finally {
      setExportBusy(false);
    }
  };

  const displayAnchorTs = currentTs ?? winEnd;

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
            background: "#080b10",
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
                className="mono"
                style={{
                  fontSize: 10.5,
                  fontWeight: 700,
                  color: "#38bdf8",
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
                      fontSize: 10.5,
                      display: "inline-flex",
                      alignItems: "center",
                      gap: 6,
                    }}
                    onClick={() => handleSelectDay(d)}
                  >
                    <span style={{ fontWeight: 700 }}>{formatDayShort(d.day)}</span>
                    <span
                      style={{
                        fontSize: 9.5,
                        padding: "0 4px",
                        borderRadius: 2,
                        background:
                          d.events > 0
                            ? "rgba(239, 68, 68, 0.22)"
                            : "rgba(16, 185, 129, 0.16)",
                        color: d.events > 0 ? "#fca5a5" : "#34d399",
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
                style={{ height: 24, fontSize: 10.5 }}
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
                title="Collapse multi-day navigator"
              >
                <X size={12} />
              </button>
            </div>
          </div>

          {/* Row 2: 24-Hour Activity Heatmap Bar (00h .. 23h) for the Selected Day */}
          <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
            <span
              className="mono"
              style={{
                fontSize: 10.5,
                fontWeight: 700,
                color: "#fcd34d",
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
                        ? `${selectedDay} ${hour}:00 — ${meta?.events || 0} events, ${Math.round(
                            (meta?.duration || 0) / 60
                          )}m recorded`
                        : `${selectedDay} ${hour}:00 — No recording`
                    }
                    className="mono"
                    style={{
                      height: 34,
                      borderRadius: 2,
                      border: isSelected
                        ? "1px solid #38bdf8"
                        : hasEvents
                        ? "1px solid rgba(239, 68, 68, 0.55)"
                        : hasRecording
                        ? "1px solid rgba(16, 185, 129, 0.35)"
                        : "1px solid rgba(255, 255, 255, 0.05)",
                      background: isSelected
                        ? "rgba(14, 165, 233, 0.28)"
                        : hasEvents
                        ? "rgba(239, 68, 68, 0.18)"
                        : hasMotion
                        ? "rgba(245, 158, 11, 0.15)"
                        : hasRecording
                        ? "rgba(16, 185, 129, 0.10)"
                        : "#050608",
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
                    <span style={{ fontSize: 10, fontWeight: 700 }}>{hour}h</span>
                    <span
                      style={{
                        fontSize: 8.5,
                        color: hasEvents
                          ? "#fca5a5"
                          : hasMotion
                          ? "#fcd34d"
                          : "#34d399",
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
                <div className="mono" style={{ fontSize: 11, color: "var(--text-muted)" }}>
                  No motion/object events recorded in this time window. Select another day or hour above.
                </div>
              ) : (
                [...windowReviews]
                  .sort((a, b) => a.start_time - b.start_time)
                  .map((rev) => {
                    const thumbSrc = rev.thumb_path
                      ? rev.thumb_path.replace("/media/frigate", "")
                      : `/api/${rev.camera}/latest.webp`;
                    const isHov = hoveredStoryboardId === rev.id;
                    return (
                      <div
                        key={rev.id}
                        onPointerEnter={() => setHoveredStoryboardId(rev.id)}
                        onPointerLeave={() =>
                          setHoveredStoryboardId((c) => (c === rev.id ? null : c))
                        }
                        onClick={() => {
                          const ts = Math.floor(rev.start_time);
                          setCustomWinEnd(Math.min(nowSec, ts + Math.round(windowSpanSec * 0.5)));
                          onSeek(ts);
                          setRangeStart(Math.max(0, ts - 5));
                          setRangeEnd(Math.min(nowSec, (rev.end_time || ts + 20) + 5));
                        }}
                        style={{
                          width: 148,
                          minWidth: 148,
                          background: "#05070a",
                          border:
                            currentTs && Math.abs(currentTs - rev.start_time) < 15
                              ? "1px solid #38bdf8"
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
                              style={{ width: "100%", height: "100%", objectFit: "contain" }}
                            />
                          ) : (
                            <img
                              src={thumbSrc}
                              alt={rev.camera}
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
                            fontSize: 9.5,
                          }}
                        >
                          <span style={{ color: "#38bdf8", fontWeight: 700 }}>
                            {formatHHMMSS(rev.start_time)}
                          </span>
                          <button
                            className="nvr-btn"
                            style={{ height: 18, padding: "0 5px", fontSize: 9 }}
                            title="Open Segment Clipper on this exact event"
                            onClick={(e) => {
                              e.stopPropagation();
                              const ts = Math.floor(rev.start_time);
                              onSeek(ts);
                              setRangeStart(Math.max(0, ts - 5));
                              setRangeEnd(Math.min(nowSec, Math.ceil((rev.end_time || ts + 20) + 5)));
                              setReadyDownloadUrl(null);
                              setRangeMode(true);
                            }}
                          >
                            <Scissors size={9} />
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
              className="mono"
              style={{ fontSize: 10.5, fontWeight: 700, color: "var(--text-secondary)" }}
            >
              {titlePrefix}
            </span>
          )}
          {cameras && cameras.length > 0 && onSelectCamera && (
            <div style={{ display: "flex", gap: 3 }}>
              {cameras.map((cam) => (
                <button
                  key={cam}
                  className={`nvr-btn mono ${camera === cam ? "active" : ""}`}
                  style={{ height: 22, padding: "0 7px", fontSize: 10, textTransform: "uppercase" }}
                  onClick={() => onSelectCamera(cam)}
                >
                  {cam}
                </button>
              ))}
            </div>
          )}

          {/* Prominent Multi-Day Archive & Heatmap Button + Direct Recording Calendar Popover */}
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
            className={`nvr-btn ${showArchiveDeck ? "primary" : ""}`}
            style={{
              height: 26,
              padding: "0 9px",
              fontSize: 11,
              fontWeight: 600,
              borderColor: "#f59e0b",
              color: showArchiveDeck ? "#38bdf8" : "#fcd34d",
            }}
            onClick={() => setShowArchiveDeck((v) => !v)}
          >
            <Grid size={12} />
            {showArchiveDeck
              ? "Hide 24h Matrix"
              : `24h Activity Matrix (${daysSummary.length || 1}d)`}
          </button>

          <span
            className={`nvr-badge ${currentTs === null ? "live" : "rewind"}`}
            style={{ height: 22 }}
          >
            <Clock size={11} />
            {currentTs === null ? "LIVE EDGE" : formatFullDateTime(currentTs)}
          </span>

          {/* Skip Silence: Prev / Next Activity Event */}
          <button
            className="nvr-btn mono"
            style={{ height: 22, padding: "0 7px", fontSize: 10 }}
            onClick={() => jumpToActivity("prev")}
            title="Jump to previous motion or person event (skip empty gaps)"
          >
            <SkipBack size={11} />
            Prev Event
          </button>
          <button
            className="nvr-btn mono"
            style={{ height: 22, padding: "0 7px", fontSize: 10 }}
            onClick={() => jumpToActivity("next")}
            title="Jump to next motion or person event (skip empty gaps)"
          >
            Next Event
            <SkipForward size={11} />
          </button>

          <button
            className="nvr-btn mono"
            style={{ height: 22, padding: "0 6px", fontSize: 10 }}
            onClick={() => onSeek((currentTs ?? winEnd) - 30)}
          >
            -30s
          </button>
          <button
            className="nvr-btn mono"
            style={{ height: 22, padding: "0 6px", fontSize: 10 }}
            onClick={() => {
              const next = (currentTs ?? winEnd) + 30;
              onSeek(next >= nowSec - 2 ? null : next);
            }}
          >
            +30s
          </button>
          <button
            className={`nvr-btn mono ${currentTs === null ? "success" : ""}`}
            style={{ height: 22, padding: "0 8px", fontSize: 10.5 }}
            onClick={() => {
              setCustomWinEnd(null);
              onSeek(null);
            }}
          >
            LIVE
          </button>
        </div>

        {/* Right: Zoom (15m / 1h / 6h / 24h), Pan (◀ / ▶), and Segment Clipper Toggle */}
        <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
          <div style={{ display: "flex", alignItems: "center", gap: 3 }}>
            <ZoomIn size={12} color="var(--text-muted)" />
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
                style={{ height: 22, padding: "0 6px", fontSize: 10 }}
                onClick={() => setZoomMin(z.min)}
              >
                {z.label}
              </button>
            ))}
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: 3 }}>
            <button
              className="nvr-btn"
              style={{ height: 22, padding: "0 5px" }}
              title="Pan timeline earlier"
              onClick={() =>
                setCustomWinEnd((curr) => (curr ?? nowSec) - Math.round(windowSpanSec * 0.5))
              }
            >
              <ChevronLeft size={12} />
            </button>
            <button
              className="nvr-btn"
              style={{ height: 22, padding: "0 5px" }}
              title="Pan timeline later"
              disabled={customWinEnd === null || customWinEnd >= nowSec}
              onClick={() =>
                setCustomWinEnd((curr) => {
                  const next = (curr ?? nowSec) + Math.round(windowSpanSec * 0.5);
                  return next >= nowSec ? null : next;
                })
              }
            >
              <ChevronRight size={12} />
            </button>
          </div>

          <button
            className={`nvr-btn ${rangeMode ? "primary" : ""}`}
            style={{
              height: 24,
              padding: "0 10px",
              fontSize: 11,
              fontWeight: 600,
              borderColor: rangeMode ? "var(--signal-accent)" : "#38bdf8",
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
            <span className="mono" style={{ fontSize: 11, fontWeight: 700, color: "#38bdf8" }}>
              EXPORT [{camera.toUpperCase()} · {toLocalDateInputValue(rangeStart)}]:
            </span>

            {/* IN Start Controls */}
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              <span className="mono" style={{ fontSize: 10.5, color: "var(--text-secondary)" }}>
                IN:
              </span>
              <button
                className="nvr-btn mono"
                style={{ height: 20, padding: "0 5px", fontSize: 10 }}
                onClick={() => setRangeStart((s) => s - 10)}
              >
                -10s
              </button>
              <span
                className="mono"
                style={{
                  padding: "2px 6px",
                  background: "#05070a",
                  border: "1px solid var(--border-strong)",
                  borderRadius: 2,
                  fontSize: 11,
                  fontWeight: 700,
                  color: "#fcd34d",
                }}
              >
                {formatHHMMSS(rangeStart)}
              </span>
              <button
                className="nvr-btn mono"
                style={{ height: 20, padding: "0 5px", fontSize: 10 }}
                onClick={() => setRangeStart((s) => Math.min(rangeEnd - 5, s + 10))}
              >
                +10s
              </button>
              <button
                className="nvr-btn mono"
                style={{ height: 20, padding: "0 6px", fontSize: 10 }}
                title="Set IN point to current playhead"
                onClick={() => {
                  const anchor = currentTs ?? winEnd - 30;
                  setRangeStart(Math.min(anchor, rangeEnd - 5));
                }}
              >
                [ Set IN
              </button>
            </div>

            <span style={{ color: "var(--border-strong)" }}>→</span>

            {/* OUT End Controls */}
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              <span className="mono" style={{ fontSize: 10.5, color: "var(--text-secondary)" }}>
                OUT:
              </span>
              <button
                className="nvr-btn mono"
                style={{ height: 20, padding: "0 5px", fontSize: 10 }}
                onClick={() => setRangeEnd((e) => Math.max(rangeStart + 5, e - 10))}
              >
                -10s
              </button>
              <span
                className="mono"
                style={{
                  padding: "2px 6px",
                  background: "#05070a",
                  border: "1px solid var(--border-strong)",
                  borderRadius: 2,
                  fontSize: 11,
                  fontWeight: 700,
                  color: "#fcd34d",
                }}
              >
                {formatHHMMSS(rangeEnd)}
              </span>
              <button
                className="nvr-btn mono"
                style={{ height: 20, padding: "0 5px", fontSize: 10 }}
                onClick={() => setRangeEnd((e) => Math.min(nowSec, e + 10))}
              >
                +10s
              </button>
              <button
                className="nvr-btn mono"
                style={{ height: 20, padding: "0 6px", fontSize: 10 }}
                title="Set OUT point to current playhead"
                onClick={() => {
                  const anchor = currentTs ?? winEnd;
                  setRangeEnd(Math.max(anchor, rangeStart + 5));
                }}
              >
                Set OUT ]
              </button>
            </div>

            <span className="nvr-badge" style={{ color: "#38bdf8" }}>
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
                style={{ height: 20, padding: "0 6px", fontSize: 10 }}
                onClick={() => setRangeEnd(Math.min(nowSec, rangeStart + p.sec))}
              >
                {p.label}
              </button>
            ))}
          </div>

          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <input
              className="nvr-input mono"
              style={{ height: 24, width: 145, fontSize: 11 }}
              placeholder="Clip label (optional)"
              value={clipLabel}
              onChange={(e) => setClipLabel(e.target.value)}
            />
            <button
              className="nvr-btn"
              style={{ height: 24, fontSize: 11 }}
              onClick={() => onSeek(rangeStart)}
              title="Preview selected segment from IN point"
            >
              <Play size={11} />
              Preview
            </button>
            <button
              className="nvr-btn success"
              style={{ height: 24, fontSize: 11, fontWeight: 600 }}
              disabled={exportBusy}
              onClick={handleTriggerExport}
            >
              <Film size={12} />
              {exportBusy
                ? "Remuxing MP4..."
                : `Export MP4 (${formatDuration(rangeEnd - rangeStart)})`}
            </button>
            {readyDownloadUrl && (
              <a
                href={readyDownloadUrl}
                download
                className="nvr-btn primary mono"
                style={{ height: 24, fontSize: 11, fontWeight: 700, textDecoration: "none" }}
              >
                <Download size={12} />
                Download MP4
              </a>
            )}
            <button
              className="nvr-btn"
              style={{ height: 24, padding: "0 5px" }}
              onClick={() => setRangeMode(false)}
              title="Close segment clipper"
            >
              <X size={12} />
            </button>
          </div>
        </div>
      )}

      {/* =====================================================================
          FINE TIMELINE TRACK WITH RULER, HEAT BARS, EVENT PINS & THUMB TOOLTIP
         ===================================================================== */}
      <div
        ref={trackRef}
        className="nvr-timeline-track"
        title="Scroll mouse wheel to scrub time (Shift+Wheel = 10s fine step, Ctrl+Wheel = Zoom) · Drag to pan · Click/Tap to seek"
        style={{
          position: "relative",
          height: 44,
          background: "#06080c",
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
        onPointerDown={(e) => {
          if ((e.target as HTMLElement).dataset.handle) return;
          try {
            e.currentTarget.setPointerCapture(e.pointerId);
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
        {/* Inner clipped layer for bars */}
        <div
          style={{
            position: "absolute",
            inset: 0,
            overflow: "hidden",
            borderRadius: 3,
            pointerEvents: "none",
          }}
        >
          {/* 1. Time Ruler Ticks & Labels along top edge */}
          {ticks.map((tk, i) => (
            <div
              key={i}
              style={{
                position: "absolute",
                left: `${tk.pct}%`,
                top: 0,
                bottom: 0,
                borderLeft: "1px solid rgba(255, 255, 255, 0.07)",
              }}
            >
              <span
                className="mono"
                style={{
                  position: "absolute",
                  top: 2,
                  left: 4,
                  fontSize: 9.5,
                  color: "var(--text-muted)",
                  whiteSpace: "nowrap",
                }}
              >
                {tk.label}
              </span>
            </div>
          ))}

          {/* 2. Continuous Recording Base Bar & Motion/Object Activity Bars */}
          {windowSegments.map((seg) => {
            if (seg.end_time < winStart || seg.start_time > winEnd) return null;
            const leftPct = Math.max(
              0,
              Math.min(100, ((seg.start_time - winStart) / windowSpanSec) * 100)
            );
            const widthPct = Math.max(0.2, (seg.duration / windowSpanSec) * 100);
            const hasObj = seg.objects > 0;
            const hasMot = seg.motion > 0;
            return (
              <React.Fragment key={seg.id}>
                <div
                  style={{
                    position: "absolute",
                    left: `${leftPct}%`,
                    width: `${widthPct}%`,
                    bottom: 0,
                    height: 6,
                    background: "rgba(16, 185, 129, 0.38)",
                  }}
                />
                {(hasMot || hasObj) && (
                  <div
                    style={{
                      position: "absolute",
                      left: `${leftPct}%`,
                      width: `${widthPct}%`,
                      bottom: 6,
                      height: hasObj ? 20 : 11,
                      background: hasObj
                        ? "rgba(239, 68, 68, 0.65)"
                        : "rgba(245, 158, 11, 0.45)",
                    }}
                  />
                )}
              </React.Fragment>
            );
          })}

          {/* 3. Review Event Markers */}
          {visibleReviews.map((rev) => {
            const sPct = Math.max(
              0,
              Math.min(100, ((rev.start_time - winStart) / windowSpanSec) * 100)
            );
            const dur = Math.max(15, (rev.end_time || rev.start_time + 15) - rev.start_time);
            const wPct = Math.max(0.4, (dur / windowSpanSec) * 100);
            return (
              <div
                key={rev.id}
                style={{
                  position: "absolute",
                  left: `${sPct}%`,
                  width: `${wPct}%`,
                  top: 16,
                  height: 6,
                  borderRadius: 2,
                  background: rev.severity === "alert" ? "#ef4444" : "#f59e0b",
                }}
              />
            );
          })}
        </div>

        {/* 4. Visual Range Export Overlay (`[ IN ... OUT ]`) with Draggable Handles */}
        {rangeMode && (
          <div
            style={{
              position: "absolute",
              left: `${rangeStartPct}%`,
              width: `${Math.max(0.5, rangeEndPct - rangeStartPct)}%`,
              top: 0,
              bottom: 0,
              background: "rgba(14, 165, 233, 0.22)",
              borderTop: "2px solid #38bdf8",
              borderBottom: "2px solid #38bdf8",
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
                left: -8,
                top: 0,
                bottom: 0,
                width: 16,
                background: "#0ea5e9",
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
                style={{ fontSize: 8.5, fontWeight: 800, color: "#000", pointerEvents: "none" }}
              >
                IN
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
                right: -8,
                top: 0,
                bottom: 0,
                width: 16,
                background: "#0ea5e9",
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
                style={{ fontSize: 8.5, fontWeight: 800, color: "#000", pointerEvents: "none" }}
              >
                OUT
              </span>
            </div>
          </div>
        )}

        {/* 5. Hover Scrub Crosshair + Floating Thumbnail & Timestamp Peeker */}
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
                bottom: 48,
                transform: "translateX(-50%)",
                background: "#0c1018",
                border: "1px solid var(--signal-accent)",
                color: "#fff",
                padding: hoveredReviewOnTrack ? 4 : "2px 7px",
                borderRadius: 3,
                fontSize: 10,
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
                  src={hoveredReviewOnTrack.thumb_path.replace("/media/frigate", "")}
                  alt="preview"
                  style={{
                    width: 128,
                    height: 72,
                    objectFit: "contain",
                    background: "#000",
                    borderRadius: 2,
                  }}
                />
              )}
              <span>{formatFullDateTime(hoverTs)}</span>
            </div>
          </>
        )}

        {/* 6. Active Playhead Needle */}
        {playheadPct >= 0 && playheadPct <= 100 && (
          <div
            style={{
              position: "absolute",
              left: `${playheadPct}%`,
              top: 0,
              bottom: 0,
              width: 2,
              background: currentTs === null ? "#10b981" : "#fbbf24",
              boxShadow: "0 0 6px rgba(0,0,0,0.8)",
              pointerEvents: "none",
              zIndex: 11,
            }}
          />
        )}
      </div>
    </div>
  );
};
