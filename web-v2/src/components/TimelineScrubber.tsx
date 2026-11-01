import React, { useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronLeft,
  ChevronRight,
  Clock,
  Download,
  Film,
  Play,
  Scissors,
  X,
  ZoomIn,
} from "lucide-react";
import { RecordingSegment, ReviewItem } from "../types/nvr";

export type TimelineZoomMinutes = 15 | 60 | 360 | 1440;

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

function formatDuration(sec: number): string {
  const total = Math.max(1, Math.round(sec));
  const m = Math.floor(total / 60);
  const s = total % 60;
  if (m === 0) return `${s}s`;
  return `${m}m ${s.toString().padStart(2, "0")}s`;
}

export const TimelineScrubber: React.FC<TimelineScrubberProps> = ({
  camera,
  cameras,
  onSelectCamera,
  segments,
  reviews,
  currentTs,
  onSeek,
  onExportRange,
  titlePrefix,
}) => {
  const trackRef = useRef<HTMLDivElement | null>(null);
  const [zoomMin, setZoomMin] = useState<TimelineZoomMinutes>(60);
  // windowEndOffsetSec = 0 means window ends at "now"; negative pans into the past
  const [windowEndOffsetSec, setWindowEndOffsetSec] = useState<number>(0);
  const [nowSec, setNowSec] = useState<number>(() => Math.floor(Date.now() / 1000));

  // Hover state
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
    "playhead" | "in" | "out" | "window" | null
  >(null);

  useEffect(() => {
    const timer = window.setInterval(() => {
      setNowSec(Math.floor(Date.now() / 1000));
    }, 2000);
    return () => window.clearInterval(timer);
  }, []);

  const windowSpanSec = zoomMin * 60;
  const winEnd = nowSec + windowEndOffsetSec;
  const winStart = winEnd - windowSpanSec;

  const clientXToTs = (clientX: number): number => {
    const el = trackRef.current;
    if (!el) return winEnd;
    const rect = el.getBoundingClientRect();
    const frac = Math.max(0, Math.min(1, (clientX - rect.left) / Math.max(1, rect.width)));
    return Math.round(winStart + frac * windowSpanSec);
  };

  // Global pointer move / up when dragging playhead or IN/OUT handles
  useEffect(() => {
    if (!draggingTarget) return;
    const onMove = (e: PointerEvent) => {
      const ts = clientXToTs(e.clientX);
      if (draggingTarget === "playhead") {
        onSeek(ts >= nowSec - 3 ? null : ts);
      } else if (draggingTarget === "in") {
        setRangeStart(Math.min(ts, rangeEnd - 5));
        setReadyDownloadUrl(null);
      } else if (draggingTarget === "out") {
        setRangeEnd(Math.max(ts, rangeStart + 5));
        setReadyDownloadUrl(null);
      }
    };
    const onUp = () => {
      setDraggingTarget(null);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
  });

  // Toggle Range Export Mode and initialize `[IN, OUT]` around current playhead
  const toggleRangeSelector = () => {
    if (!rangeMode) {
      const anchor = currentTs ?? winEnd - 60;
      const s = Math.max(winStart, Math.min(winEnd - 20, anchor - 30));
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
        label: formatHHMM(ts),
      });
    }
    return out;
  }, [winStart, windowSpanSec]);

  // Camera reviews inside current window
  const visibleReviews = useMemo(() => {
    return reviews.filter(
      (r) =>
        (r.camera === camera || camera === "all") &&
        (r.end_time || r.start_time + 15) >= winStart &&
        r.start_time <= winEnd
    );
  }, [reviews, camera, winStart, winEnd]);

  const playheadPct =
    currentTs === null
      ? windowEndOffsetSec === 0
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
        `${camera}_${formatHHMMSS(rangeStart).replace(/:/g, "")}_${formatDuration(
          rangeEnd - rangeStart
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
      {/* Top Row: Channel Selector, Playhead Controls, Zoom & Pan, and Segment Export Button */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 8,
          flexWrap: "wrap",
        }}
      >
        {/* Left: Channel Selector + Playhead Status & Quick Step */}
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

          <span style={{ color: "var(--border-strong)" }}>|</span>

          <span
            className={`nvr-badge ${currentTs === null ? "live" : "rewind"}`}
            style={{ height: 22 }}
          >
            <Clock size={11} />
            {currentTs === null ? "LIVE EDGE" : `PLAYHEAD ${formatHHMMSS(currentTs)}`}
          </span>

          <button
            className="nvr-btn mono"
            style={{ height: 22, padding: "0 7px", fontSize: 10.5 }}
            onClick={() => onSeek((currentTs ?? nowSec) - 30)}
            title="Rewind 30 seconds"
          >
            -30s
          </button>
          <button
            className="nvr-btn mono"
            style={{ height: 22, padding: "0 7px", fontSize: 10.5 }}
            onClick={() => onSeek((currentTs ?? nowSec) - 10)}
            title="Rewind 10 seconds"
          >
            -10s
          </button>
          <button
            className="nvr-btn mono"
            style={{ height: 22, padding: "0 7px", fontSize: 10.5 }}
            onClick={() => {
              const next = (currentTs ?? nowSec) + 10;
              onSeek(next >= nowSec - 2 ? null : next);
            }}
            title="Forward 10 seconds"
          >
            +10s
          </button>
          <button
            className={`nvr-btn mono ${currentTs === null ? "success" : ""}`}
            style={{ height: 22, padding: "0 8px", fontSize: 10.5 }}
            onClick={() => {
              setWindowEndOffsetSec(0);
              onSeek(null);
            }}
          >
            LIVE
          </button>
        </div>

        {/* Center/Right: Zoom (15m / 1h / 6h / 24h), Pan (◀ / ▶), and Segment Clipper Toggle */}
        <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
          <div style={{ display: "flex", alignItems: "center", gap: 3 }}>
            <ZoomIn size={12} color="var(--text-muted)" />
            {(
              [
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
              onClick={() => setWindowEndOffsetSec((off) => off - Math.round(windowSpanSec * 0.4))}
            >
              <ChevronLeft size={12} />
            </button>
            <button
              className="nvr-btn"
              style={{ height: 22, padding: "0 5px" }}
              title="Pan timeline later"
              disabled={windowEndOffsetSec >= 0}
              onClick={() =>
                setWindowEndOffsetSec((off) => Math.min(0, off + Math.round(windowSpanSec * 0.4)))
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

      {/* Interactive Segment Export Control Bar (Appears when Range Clipper is active) */}
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
              EXPORT RANGE [{camera.toUpperCase()}]:
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
                  const anchor = currentTs ?? nowSec - 30;
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
                  const anchor = currentTs ?? nowSec;
                  setRangeEnd(Math.max(anchor, rangeStart + 5));
                }}
              >
                Set OUT ]
              </button>
            </div>

            <span className="nvr-badge" style={{ color: "#38bdf8" }}>
              {formatDuration(rangeEnd - rangeStart)}
            </span>

            {/* Quick Duration Presets */}
            {(
              [
                { sec: 30, label: "30s" },
                { sec: 60, label: "1m" },
                { sec: 180, label: "3m" },
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

      {/* Main Interactive Timeline Track with Ruler, Motion/Object Bars, Event Pins, and Draggable IN/OUT Handles */}
      <div
        ref={trackRef}
        className="nvr-timeline-track"
        style={{
          position: "relative",
          height: 44,
          background: "#06080c",
          border: "1px solid var(--border-strong)",
          borderRadius: 3,
          cursor: "pointer",
          overflow: "hidden",
        }}
        onMouseMove={(e) => {
          const rect = e.currentTarget.getBoundingClientRect();
          setHoverX(e.clientX - rect.left);
          setHoverTs(clientXToTs(e.clientX));
        }}
        onMouseLeave={() => setHoverTs(null)}
        onPointerDown={(e) => {
          if ((e.target as HTMLElement).dataset.handle) return;
          const ts = clientXToTs(e.clientX);
          onSeek(ts >= nowSec - 3 ? null : ts);
          setDraggingTarget("playhead");
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
              pointerEvents: "none",
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
        {segments.map((seg) => {
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
              {/* Continuous recorded base strip */}
              <div
                style={{
                  position: "absolute",
                  left: `${leftPct}%`,
                  width: `${widthPct}%`,
                  bottom: 0,
                  height: 6,
                  background: "rgba(16, 185, 129, 0.38)",
                  pointerEvents: "none",
                }}
              />
              {/* Motion / Object activity bar */}
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
                    pointerEvents: "none",
                  }}
                />
              )}
            </React.Fragment>
          );
        })}

        {/* 3. Review Event Markers (Clickable crimson/amber pins) */}
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
              title={`${rev.camera.toUpperCase()} · ${(rev.data?.objects || ["motion"]).join(
                ", "
              )} @ ${formatHHMMSS(rev.start_time)}`}
              style={{
                position: "absolute",
                left: `${sPct}%`,
                width: `${wPct}%`,
                top: 16,
                height: 6,
                borderRadius: 2,
                background: rev.severity === "alert" ? "#ef4444" : "#f59e0b",
                boxShadow: "0 0 4px rgba(0,0,0,0.6)",
                pointerEvents: "none",
              }}
            />
          );
        })}

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
            {/* Left Draggable [ IN Handle */}
            <div
              data-handle="in"
              onPointerDown={(e) => {
                e.stopPropagation();
                setDraggingTarget("in");
              }}
              title="Drag IN handle to adjust export start time"
              style={{
                position: "absolute",
                left: -6,
                top: 0,
                bottom: 0,
                width: 12,
                background: "#0ea5e9",
                cursor: "ew-resize",
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

            {/* Right Draggable OUT ] Handle */}
            <div
              data-handle="out"
              onPointerDown={(e) => {
                e.stopPropagation();
                setDraggingTarget("out");
              }}
              title="Drag OUT handle to adjust export end time"
              style={{
                position: "absolute",
                right: -6,
                top: 0,
                bottom: 0,
                width: 14,
                background: "#0ea5e9",
                cursor: "ew-resize",
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

        {/* 5. Hover Scrub Crosshair & Timestamp Tooltip */}
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
                left: `${Math.max(45, Math.min((trackRef.current?.clientWidth || 800) - 85, hoverX))}px`,
                top: 2,
                transform: "translateX(-50%)",
                background: "#0e131d",
                border: "1px solid var(--signal-accent)",
                color: "#fff",
                padding: "1px 6px",
                borderRadius: 2,
                fontSize: 10,
                fontWeight: 700,
                pointerEvents: "none",
                zIndex: 10,
                whiteSpace: "nowrap",
              }}
            >
              {formatHHMMSS(hoverTs)}
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
