import React, { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Calendar, ChevronDown, ChevronLeft, ChevronRight, Clock, X } from "lucide-react";
import { useI18n } from "../i18n";
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

const WEEKDAYS_SUN_EN = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
const WEEKDAYS_MON_EN = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
const WEEKDAYS_SUN_PT = ["Do", "Se", "Te", "Qa", "Qi", "Sx", "Sá"];
const WEEKDAYS_MON_PT = ["Se", "Te", "Qa", "Qi", "Sx", "Sá", "Do"];

function parseValidCalendarDate(dateStr: string): [number, number, number] | null {
  const m = (dateStr || "").trim().match(/^(\d{4})-(\d{2})-(\d{2})$/);
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

export function formatReadableDate(
  dateStr: string,
  allDatesLabel = "All Dates",
  localeTag?: string
): string {
  if (!dateStr) return allDatesLabel;
  const parsed = parseValidCalendarDate(dateStr);
  if (!parsed) return dateStr;
  const d = new Date(parsed[0], parsed[1] - 1, parsed[2]);
  return d.toLocaleDateString(localeTag ? [localeTag] : [], {
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
  buttonLabelPrefix,
  openUpward = true,
  onSelectDate,
  onSelectDateTime,
  onClear,
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const localeTag = isPt ? "pt-BR" : "en-US";
  const effectiveButtonPrefix = buttonLabelPrefix ?? t.calendar.jumpToDate;
  const wrapperRef = useRef<HTMLDivElement | null>(null);
  const triggerBtnRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [isOpen, setIsOpen] = useState(false);
  const [fixedPos, setFixedPos] = useState<React.CSSProperties | null>(null);
  const firstWeekday = useNvrStore((s) => s.firstWeekday);
  const weekdayLabels =
    firstWeekday === "monday"
      ? isPt
        ? WEEKDAYS_MON_PT
        : WEEKDAYS_MON_EN
      : isPt
      ? WEEKDAYS_SUN_PT
      : WEEKDAYS_SUN_EN;

  // Month being viewed in the calendar grid
  const initialDate = useMemo(() => {
    if (selectedDate) {
      const p = parseValidCalendarDate(selectedDate);
      if (p) {
        return new Date(p[0], p[1] - 1, 1);
      }
    }
    if (daysSummary.length > 0 && daysSummary[0]?.day) {
      const p = parseValidCalendarDate(daysSummary[0].day);
      if (p) {
        return new Date(p[0], p[1] - 1, 1);
      }
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
    const p = parseValidCalendarDate(selectedDate);
    if (p) {
      setViewYear(p[0]);
      setViewMonth(p[1] - 1);
    }
  }, [selectedDate]);

  // Close popover on outside click or Escape, and keep fixed portal position synced
  useEffect(() => {
    if (!isOpen) return;
    const updateAnchor = () => {
      if (typeof window === "undefined" || !triggerBtnRef.current) return;
      const rect = triggerBtnRef.current.getBoundingClientRect();
      const popW = Math.min(316, Math.max(240, window.innerWidth - 24));
      const clampedLeft = Math.max(
        12,
        Math.min(rect.left, window.innerWidth - popW - 12)
      );
      const preferUp =
        openUpward ||
        (window.innerHeight - rect.bottom < 360 && rect.top > 360);
      if (preferUp) {
        setFixedPos({
          position: "fixed",
          top: "auto",
          left: clampedLeft,
          bottom: Math.max(12, window.innerHeight - rect.top + 6),
          width: popW,
          zIndex: 1300,
        });
      } else {
        setFixedPos({
          position: "fixed",
          top: Math.max(12, rect.bottom + 6),
          bottom: "auto",
          left: clampedLeft,
          width: popW,
          zIndex: 1300,
        });
      }
    };
    updateAnchor();
    const onDocClick = (e: MouseEvent) => {
      const node = e.target as Node;
      if (
        wrapperRef.current &&
        !wrapperRef.current.contains(node) &&
        (!popoverRef.current || !popoverRef.current.contains(node))
      ) {
        setIsOpen(false);
      }
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        setIsOpen(false);
        triggerBtnRef.current?.focus();
      }
    };
    document.addEventListener("mousedown", onDocClick);
    document.addEventListener("keydown", onKey);
    window.addEventListener("resize", updateAnchor);
    window.addEventListener("scroll", updateAnchor, true);
    return () => {
      document.removeEventListener("mousedown", onDocClick);
      document.removeEventListener("keydown", onKey);
      window.removeEventListener("resize", updateAnchor);
      window.removeEventListener("scroll", updateAnchor, true);
    };
  }, [isOpen, openUpward]);

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
    return new Date(viewYear, viewMonth, 1).toLocaleDateString([localeTag], {
      month: "long",
      year: "numeric",
    });
  }, [viewYear, viewMonth, localeTag]);

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
        ref={triggerBtnRef}
        type="button"
        className={`nvr-btn sm ${isFilteredActive ? "active" : ""}`}
        aria-expanded={isOpen}
        aria-haspopup="dialog"
        onClick={() => setIsOpen((v) => !v)}
        style={{
          padding: "0 10px",
          fontSize: "var(--text-xs)",
          display: "inline-flex",
          alignItems: "center",
          gap: 6,
        }}
        title={t.calendar.openTooltip}
      >
        <Calendar size={13} color="var(--text-secondary)" />
        <span style={{ color: "var(--text-secondary)", fontWeight: 500 }}>
          {effectiveButtonPrefix}:
        </span>
        <span className="mono" style={{ color: "var(--text-primary)", fontWeight: 700 }}>
          {formatReadableDate(selectedDate, t.calendar.allDates, localeTag)}
        </span>
        {showTimePicker && selectedTime && (
          <span className="mono" style={{ color: "var(--text-secondary)" }}>
            {selectedTime}
          </span>
        )}
        <ChevronDown size={12} color="var(--text-muted)" />
      </button>

      {/* Calendar Popover (portaled to body when DOM is available so .nvr-subbar overflow never clips it) */}
      {isOpen &&
        (() => {
          const dialogNode = (
            <div
              ref={popoverRef}
              role="dialog"
              aria-label={t.calendar.dialogAria}
              className="nvr-calendar-popover"
              style={{
                position: fixedPos ? "fixed" : "absolute",
                ...(fixedPos
                  ? fixedPos
                  : openUpward
                    ? { bottom: 32, left: 0, width: 316, zIndex: 1300 }
                    : { top: 32, left: 0, width: 316, zIndex: 1300 }),
                maxWidth: "calc(100vw - 24px)",
                background: "var(--bg-panel)",
                border: "1px solid var(--signal-accent)",
                borderRadius: 4,
                boxShadow: "0 14px 36px rgba(0, 0, 0, 0.85)",
                padding: 12,
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
              className="nvr-btn sm"
              style={{ padding: "0 6px" }}
              onClick={() => stepMonth(-1)}
              aria-label={t.calendar.prevMonth}
              title={t.calendar.prevMonth}
            >
              <ChevronLeft size={13} />
            </button>
            <span
              style={{
                fontSize: "var(--text-sm)",
                fontWeight: 700,
                color: "var(--text-primary)",
                textTransform: "capitalize",
              }}
            >
              {monthTitle}
            </span>
            <div style={{ display: "flex", gap: 4 }}>
              <button
                type="button"
                className="nvr-btn sm"
                style={{ padding: "0 6px" }}
                onClick={() => stepMonth(1)}
                aria-label={t.calendar.nextMonth}
                title={t.calendar.nextMonth}
              >
                <ChevronRight size={13} />
              </button>
              <button
                type="button"
                className="nvr-btn sm"
                style={{ padding: "0 5px" }}
                onClick={() => setIsOpen(false)}
                aria-label={t.calendar.closeCalendar}
                title={t.calendar.closeCalendar}
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
              background: "var(--bg-subcard)",
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
              {t.calendar.eventsLegend}
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
              {t.calendar.recordedLegend}
            </span>
            <span className="mono" style={{ color: "var(--text-muted)" }}>
              {t.calendar.daysOnDisk.replace("{n}", String(daysSummary.length))}
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
            {weekdayLabels.map((w, wIdx) => (
              <div key={`${w}-${wIdx}`}>{w}</div>
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
                      ? t.calendar.eventsRecordedTooltip
                          .replace("{date}", cell.dateStr)
                          .replace("{count}", String(evCount))
                      : t.calendar.noRecordingsTooltip.replace("{date}", cell.dateStr)
                  }
                  className="mono"
                  style={{
                    minHeight: 38,
                    borderRadius: 3,
                    border: isSelected
                      ? "1.5px solid var(--signal-accent-text)"
                      : hasRec
                      ? evCount > 0
                        ? "1px solid rgba(239, 68, 68, 0.55)"
                        : "1px solid rgba(16, 185, 129, 0.45)"
                      : "1px solid var(--border-divider)",
                    background: isSelected
                      ? "rgba(14, 165, 233, 0.28)"
                      : hasRec
                      ? evCount > 0
                        ? "rgba(239, 68, 68, 0.14)"
                        : "rgba(16, 185, 129, 0.12)"
                      : "var(--bg-subcard)",
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
                  {t.calendar.timeLabel}
                </label>
                <input
                  id="nvr-calendar-time-input"
                  type="time"
                  step="1"
                  className="nvr-input sm mono"
                  style={{ fontSize: "var(--text-xs)", padding: "0 6px" }}
                  value={timeInput}
                  onChange={(e) => setTimeInput(e.target.value)}
                />
              </div>
              <button
                type="button"
                className="nvr-btn primary sm"
                onClick={() => {
                  const now = new Date();
                  const localToday = `${now.getFullYear()}-${String(
                    now.getMonth() + 1
                  ).padStart(2, "0")}-${String(now.getDate()).padStart(2, "0")}`;
                  const targetDate =
                    selectedDate || daysSummary[0]?.day || localToday;
                  const safeTime = timeInput.trim() || selectedTime || "12:00:00";
                  onSelectDateTime?.(targetDate, safeTime);
                  setIsOpen(false);
                }}
              >
                {t.calendar.jumpToTime}
              </button>
            </div>
          )}

          {allowClear && selectedDate && (
            <button
              type="button"
              className="nvr-btn sm"
              style={{ width: "100%" }}
              onClick={() => {
                onClear?.();
                setIsOpen(false);
              }}
            >
              {t.calendar.showAllDates}
            </button>
          )}
            </div>
          );
          return typeof document !== "undefined" && document.body
            ? createPortal(dialogNode, document.body)
            : dialogNode;
        })()}
    </div>
  );
};
