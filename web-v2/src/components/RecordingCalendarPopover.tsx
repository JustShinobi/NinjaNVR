import React, { useEffect, useMemo, useRef, useState } from "react";
import { Calendar, ChevronDown, ChevronLeft, ChevronRight, Clock, X } from "lucide-react";
import { RecordingDaySummary } from "../types/nvr";

interface RecordingCalendarPopoverProps {
  daysSummary: RecordingDaySummary[];
  selectedDate: string; // "YYYY-MM-DD" or ""
  selectedTime?: string; // "HH:MM:SS"
  showTimePicker?: boolean;
  allowClear?: boolean;
  buttonLabelPrefix?: string;
  openUpward?: boolean;
  onSelectDate: (dateStr: string, daySummary?: RecordingDaySummary) => void;
  onSelectDateTime?: (dateStr: string, timeStr: string) => void;
  onClear?: () => void;
}

const WEEKDAYS = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];

function formatReadableDate(dateStr: string): string {
  if (!dateStr) return "All Dates";
  const parts = dateStr.split("-").map(Number);
  if (parts.length !== 3) return dateStr;
  const d = new Date(parts[0], parts[1] - 1, parts[2]);
  return d.toLocaleDateString([], {
    month: "short",
    day: "2-digit",
    year: "numeric",
  });
}

export const RecordingCalendarPopover: React.FC<RecordingCalendarPopoverProps> = ({
  daysSummary,
  selectedDate,
  selectedTime = "12:00:00",
  showTimePicker = false,
  allowClear = false,
  buttonLabelPrefix = "Jump to Date",
  openUpward = true,
  onSelectDate,
  onSelectDateTime,
  onClear,
}) => {
  const wrapperRef = useRef<HTMLDivElement | null>(null);
  const [isOpen, setIsOpen] = useState(false);

  // Month being viewed in the calendar grid
  const initialDate = useMemo(() => {
    if (selectedDate) {
      const p = selectedDate.split("-").map(Number);
      if (p.length === 3) return new Date(p[0], p[1] - 1, 1);
    }
    if (daysSummary.length > 0) {
      const p = daysSummary[0].day.split("-").map(Number);
      if (p.length === 3) return new Date(p[0], p[1] - 1, 1);
    }
    const now = new Date();
    return new Date(now.getFullYear(), now.getMonth(), 1);
  }, [selectedDate, daysSummary]);

  const [viewYear, setViewYear] = useState<number>(initialDate.getFullYear());
  const [viewMonth, setViewMonth] = useState<number>(initialDate.getMonth()); // 0..11
  const [timeInput, setTimeInput] = useState<string>(selectedTime);

  useEffect(() => {
    setTimeInput(selectedTime);
  }, [selectedTime]);

  // Sync month view when selectedDate changes externally
  useEffect(() => {
    if (!selectedDate) return;
    const p = selectedDate.split("-").map(Number);
    if (p.length === 3 && !Number.isNaN(p[0]) && !Number.isNaN(p[1])) {
      setViewYear(p[0]);
      setViewMonth(p[1] - 1);
    }
  }, [selectedDate]);

  // Close popover on outside click or Escape
  useEffect(() => {
    if (!isOpen) return;
    const onDocClick = (e: MouseEvent) => {
      if (wrapperRef.current && !wrapperRef.current.contains(e.target as Node)) {
        setIsOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setIsOpen(false);
    };
    document.addEventListener("mousedown", onDocClick);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDocClick);
      document.removeEventListener("keydown", onKey);
    };
  }, [isOpen]);

  // Map "YYYY-MM-DD" -> RecordingDaySummary for O(1) lookup
  const dayMap = useMemo(() => {
    const m = new Map<string, RecordingDaySummary>();
    for (const d of daysSummary) {
      m.set(d.day, d);
    }
    return m;
  }, [daysSummary]);

  // Build calendar cells for (viewYear, viewMonth)
  const calendarCells = useMemo(() => {
    const firstDayOfWeek = new Date(viewYear, viewMonth, 1).getDay(); // 0=Sun..6=Sat
    const daysInMonth = new Date(viewYear, viewMonth + 1, 0).getDate();
    const cells: Array<{
      dateStr: string;
      dayNum: number;
      summary?: RecordingDaySummary;
    } | null> = [];

    for (let i = 0; i < firstDayOfWeek; i++) {
      cells.push(null);
    }
    for (let d = 1; d <= daysInMonth; d++) {
      const mStr = String(viewMonth + 1).padStart(2, "0");
      const dStr = String(d).padStart(2, "0");
      const dateStr = `${viewYear}-${mStr}-${dStr}`;
      cells.push({
        dateStr,
        dayNum: d,
        summary: dayMap.get(dateStr),
      });
    }
    return cells;
  }, [viewYear, viewMonth, dayMap]);

  const monthTitle = useMemo(() => {
    return new Date(viewYear, viewMonth, 1).toLocaleDateString([], {
      month: "long",
      year: "numeric",
    });
  }, [viewYear, viewMonth]);

  const stepMonth = (delta: number) => {
    const next = new Date(viewYear, viewMonth + delta, 1);
    setViewYear(next.getFullYear());
    setViewMonth(next.getMonth());
  };

  return (
    <div ref={wrapperRef} style={{ position: "relative", display: "inline-flex" }}>
      {/* Unmistakable Calendar Trigger Button */}
      <button
        type="button"
        className={`nvr-btn mono ${isOpen ? "primary" : ""}`}
        onClick={() => setIsOpen((v) => !v)}
        style={{
          height: 26,
          padding: "0 10px",
          fontSize: 11,
          fontWeight: 600,
          borderColor: selectedDate ? "#38bdf8" : "var(--border-strong)",
          background: isOpen ? "rgba(14, 165, 233, 0.2)" : "var(--bg-elevated)",
          display: "inline-flex",
          alignItems: "center",
          gap: 6,
        }}
        title="Open recording calendar (highlights days with recorded video and events)"
      >
        <Calendar size={13} color="#38bdf8" />
        <span style={{ color: "var(--text-secondary)", fontWeight: 500 }}>
          {buttonLabelPrefix}:
        </span>
        <span style={{ color: "#fff", fontWeight: 700 }}>
          {formatReadableDate(selectedDate)}
        </span>
        {showTimePicker && selectedTime && (
          <span style={{ color: "#fcd34d" }}>{selectedTime}</span>
        )}
        <ChevronDown size={12} color="var(--text-muted)" />
      </button>

      {/* Calendar Popover */}
      {isOpen && (
        <div
          className="nvr-calendar-popover"
          style={{
            position: "absolute",
            ...(openUpward ? { bottom: 32 } : { top: 32 }),
            left: 0,
            width: 310,
            background: "#0c1017",
            border: "1px solid #38bdf8",
            borderRadius: 4,
            boxShadow: "0 14px 36px rgba(0, 0, 0, 0.85)",
            padding: 12,
            zIndex: 100,
            display: "flex",
            flexDirection: "column",
            gap: 10,
          }}
        >
          {/* Header: Month Navigation */}
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
            }}
          >
            <button
              type="button"
              className="nvr-btn"
              style={{ height: 24, padding: "0 6px" }}
              onClick={() => stepMonth(-1)}
              title="Previous Month"
            >
              <ChevronLeft size={13} />
            </button>
            <span
              className="mono"
              style={{ fontSize: 12, fontWeight: 700, color: "var(--text-primary)" }}
            >
              {monthTitle}
            </span>
            <div style={{ display: "flex", gap: 4 }}>
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, padding: "0 6px" }}
                onClick={() => stepMonth(1)}
                title="Next Month"
              >
                <ChevronRight size={13} />
              </button>
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, padding: "0 5px" }}
                onClick={() => setIsOpen(false)}
                title="Close calendar"
              >
                <X size={12} />
              </button>
            </div>
          </div>

          {/* Recording Activity Legend */}
          <div
            className="mono"
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              fontSize: 9.5,
              color: "var(--text-secondary)",
              padding: "4px 6px",
              background: "#07090d",
              borderRadius: 2,
              border: "1px solid var(--border-hairline)",
            }}
          >
            <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
              <span
                style={{
                  width: 7,
                  height: 7,
                  borderRadius: "50%",
                  background: "#ef4444",
                  display: "inline-block",
                }}
              />
              Events
            </span>
            <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
              <span
                style={{
                  width: 7,
                  height: 7,
                  borderRadius: "50%",
                  background: "#10b981",
                  display: "inline-block",
                }}
              />
              Recorded
            </span>
            <span style={{ color: "var(--text-muted)" }}>
              {daysSummary.length} days on disk
            </span>
          </div>

          {/* Weekday Headers */}
          <div
            className="mono"
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(7, 1fr)",
              gap: 3,
              textAlign: "center",
              fontSize: 10,
              color: "var(--text-muted)",
              fontWeight: 600,
            }}
          >
            {WEEKDAYS.map((w) => (
              <div key={w}>{w}</div>
            ))}
          </div>

          {/* Days Grid with Frigate-style Recording Indicators */}
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(7, 1fr)",
              gap: 4,
            }}
          >
            {calendarCells.map((cell, idx) => {
              if (!cell) {
                return <div key={`empty-${idx}`} style={{ height: 36 }} />;
              }
              const hasRec = Boolean(cell.summary);
              const evCount = cell.summary?.events || 0;
              const isSelected = cell.dateStr === selectedDate;

              return (
                <button
                  key={cell.dateStr}
                  type="button"
                  disabled={!hasRec}
                  onClick={() => {
                    onSelectDate(cell.dateStr, cell.summary);
                    if (!showTimePicker) {
                      setIsOpen(false);
                    }
                  }}
                  title={
                    hasRec
                      ? `${cell.dateStr}: ${evCount} events recorded`
                      : `${cell.dateStr}: No recordings on disk`
                  }
                  className="mono"
                  style={{
                    height: 36,
                    borderRadius: 3,
                    border: isSelected
                      ? "1.5px solid #38bdf8"
                      : hasRec
                      ? evCount > 0
                        ? "1px solid rgba(239, 68, 68, 0.55)"
                        : "1px solid rgba(16, 185, 129, 0.45)"
                      : "1px solid rgba(255, 255, 255, 0.04)",
                    background: isSelected
                      ? "rgba(14, 165, 233, 0.28)"
                      : hasRec
                      ? evCount > 0
                        ? "rgba(239, 68, 68, 0.14)"
                        : "rgba(16, 185, 129, 0.12)"
                      : "#06080c",
                    color: hasRec ? "#fff" : "var(--text-muted)",
                    opacity: hasRec ? 1 : 0.28,
                    cursor: hasRec ? "pointer" : "not-allowed",
                    display: "flex",
                    flexDirection: "column",
                    alignItems: "center",
                    justifyContent: "center",
                    padding: 2,
                    position: "relative",
                  }}
                >
                  <span style={{ fontSize: 11, fontWeight: 700 }}>{cell.dayNum}</span>
                  {hasRec && (
                    <span
                      style={{
                        fontSize: 8.5,
                        color: evCount > 0 ? "#fca5a5" : "#34d399",
                        lineHeight: 1,
                        marginTop: 2,
                      }}
                    >
                      {evCount > 0 ? `${evCount}` : "●"}
                    </span>
                  )}
                  {/* Bottom underline bar like Frigate calendar */}
                  {hasRec && (
                    <span
                      style={{
                        position: "absolute",
                        bottom: 1,
                        left: 4,
                        right: 4,
                        height: 2,
                        borderRadius: 1,
                        background: evCount > 0 ? "#ef4444" : "#10b981",
                      }}
                    />
                  )}
                </button>
              );
            })}
          </div>

          {/* Optional Time Picker & Action Footer */}
          {showTimePicker && (
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: 6,
                paddingTop: 6,
                borderTop: "1px solid var(--border-hairline)",
              }}
            >
              <div style={{ display: "flex", alignItems: "center", gap: 5 }}>
                <Clock size={12} color="#fcd34d" />
                <span className="mono" style={{ fontSize: 10.5, color: "var(--text-secondary)" }}>
                  Time:
                </span>
                <input
                  type="time"
                  step="1"
                  className="nvr-input mono"
                  style={{ height: 24, fontSize: 11, padding: "0 6px" }}
                  value={timeInput}
                  onChange={(e) => setTimeInput(e.target.value)}
                />
              </div>
              <button
                type="button"
                className="nvr-btn primary"
                style={{ height: 24, fontSize: 11 }}
                onClick={() => {
                  const targetDate =
                    selectedDate || daysSummary[0]?.day || new Date().toISOString().slice(0, 10);
                  onSelectDateTime?.(targetDate, timeInput);
                  setIsOpen(false);
                }}
              >
                Jump to Time
              </button>
            </div>
          )}

          {allowClear && selectedDate && (
            <button
              type="button"
              className="nvr-btn"
              style={{ height: 24, fontSize: 11, width: "100%" }}
              onClick={() => {
                onClear?.();
                setIsOpen(false);
              }}
            >
              Show All Recorded Dates
            </button>
          )}
        </div>
      )}
    </div>
  );
};
