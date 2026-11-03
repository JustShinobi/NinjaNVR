import React, { useEffect, useMemo, useRef, useState } from "react";
import { Calendar, ChevronDown, ChevronLeft, ChevronRight, Clock, X } from "lucide-react";
import { useNvrStore } from "../store/useNvrStore";
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

const WEEKDAYS_SUN = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
const WEEKDAYS_MON = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];

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
  const firstWeekday = useNvrStore((s) => s.firstWeekday);
  const weekdayLabels = firstWeekday === "monday" ? WEEKDAYS_MON : WEEKDAYS_SUN;

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
    const rawFirstDay = new Date(viewYear, viewMonth, 1).getDay(); // 0=Sun..6=Sat
    const firstDayOfWeek =
      firstWeekday === "monday" ? (rawFirstDay + 6) % 7 : rawFirstDay;
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
  }, [viewYear, viewMonth, dayMap, firstWeekday]);

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

  const isFilteredActive = isOpen || (allowClear && Boolean(selectedDate));

  return (
    <div ref={wrapperRef} style={{ position: "relative", display: "inline-flex" }}>
      {/* Calendar Trigger Button: neutral when closed, accent only when open or actively filtering */}
      <button
        type="button"
        className={`nvr-btn ${isFilteredActive ? "active" : ""}`}
        aria-expanded={isOpen}
        aria-haspopup="dialog"
        onClick={() => setIsOpen((v) => !v)}
        style={{
          height: 26,
          padding: "0 10px",
          fontSize: "var(--text-xs)",
          display: "inline-flex",
          alignItems: "center",
          gap: 6,
        }}
        title="Open recording calendar (highlights days with recorded video and events)"
      >
        <Calendar size={13} color="var(--text-secondary)" />
        <span style={{ color: "var(--text-secondary)", fontWeight: 500 }}>
          {buttonLabelPrefix}:
        </span>
        <span className="mono" style={{ color: "var(--text-primary)", fontWeight: 700 }}>
          {formatReadableDate(selectedDate)}
        </span>
        {showTimePicker && selectedTime && (
          <span className="mono" style={{ color: "var(--text-secondary)" }}>
            {selectedTime}
          </span>
        )}
        <ChevronDown size={12} color="var(--text-muted)" />
      </button>

      {/* Calendar Popover */}
      {isOpen && (
        <div
          role="dialog"
          aria-label="Recording Calendar"
          className="nvr-calendar-popover"
          style={{
            position: "absolute",
            ...(openUpward ? { bottom: 32 } : { top: 32 }),
            left: 0,
            width: 316,
            background: "var(--bg-panel)",
            border: "1px solid var(--signal-accent)",
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
              aria-label="Previous Month"
              title="Previous Month"
            >
              <ChevronLeft size={13} />
            </button>
            <span
              style={{
                fontSize: "var(--text-sm)",
                fontWeight: 700,
                color: "var(--text-primary)",
              }}
            >
              {monthTitle}
            </span>
            <div style={{ display: "flex", gap: 4 }}>
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, padding: "0 6px" }}
                onClick={() => stepMonth(1)}
                aria-label="Next Month"
                title="Next Month"
              >
                <ChevronRight size={13} />
              </button>
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, padding: "0 5px" }}
                onClick={() => setIsOpen(false)}
                aria-label="Close calendar"
                title="Close calendar"
              >
                <X size={12} />
              </button>
            </div>
          </div>

          {/* Recording Activity Legend */}
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              fontSize: "var(--text-xs)",
              color: "var(--text-secondary)",
              padding: "4px 6px",
              background: "var(--bg-deep)",
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
                  background: "var(--signal-alert)",
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
                  background: "var(--signal-live)",
                  display: "inline-block",
                }}
              />
              Recorded
            </span>
            <span className="mono" style={{ color: "var(--text-muted)" }}>
              {daysSummary.length}d on disk
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
              fontSize: "var(--text-xs)",
              color: "var(--text-muted)",
              fontWeight: 600,
            }}
          >
            {weekdayLabels.map((w) => (
              <div key={w}>{w}</div>
            ))}
          </div>

          {/* Days Grid with Recording Activity Indicators */}
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(7, 1fr)",
              gap: 4,
            }}
          >
            {calendarCells.map((cell, idx) => {
              if (!cell) {
                return <div key={`empty-${idx}`} style={{ height: 38 }} />;
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
                    height: 38,
                    borderRadius: 3,
                    border: isSelected
                      ? "1.5px solid var(--signal-accent-text)"
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
                      : "var(--bg-deep)",
                    color: hasRec ? "var(--text-primary)" : "var(--text-muted)",
                    opacity: hasRec ? 1 : 0.35,
                    cursor: hasRec ? "pointer" : "not-allowed",
                    display: "flex",
                    flexDirection: "column",
                    alignItems: "center",
                    justifyContent: "center",
                    padding: 2,
                    position: "relative",
                  }}
                >
                  <span style={{ fontSize: "var(--text-xs)", fontWeight: 700 }}>{cell.dayNum}</span>
                  {hasRec && (
                    <span
                      style={{
                        fontSize: 10,
                        color:
                          evCount > 0 ? "var(--signal-alert-text)" : "var(--signal-live-text)",
                        lineHeight: 1,
                        marginTop: 1,
                      }}
                    >
                      {evCount > 0 ? `${evCount}` : "●"}
                    </span>
                  )}
                  {hasRec && (
                    <span
                      style={{
                        position: "absolute",
                        bottom: 1,
                        left: 4,
                        right: 4,
                        height: 2,
                        borderRadius: 1,
                        background:
                          evCount > 0 ? "var(--signal-alert)" : "var(--signal-live)",
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
                <Clock size={12} color="var(--signal-motion-text)" />
                <label
                  htmlFor="nvr-calendar-time-input"
                  style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                >
                  Time:
                </label>
                <input
                  id="nvr-calendar-time-input"
                  type="time"
                  step="1"
                  className="nvr-input mono"
                  style={{ height: 24, fontSize: "var(--text-xs)", padding: "0 6px" }}
                  value={timeInput}
                  onChange={(e) => setTimeInput(e.target.value)}
                />
              </div>
              <button
                type="button"
                className="nvr-btn primary"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
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
              style={{ height: 24, fontSize: "var(--text-xs)", width: "100%" }}
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
