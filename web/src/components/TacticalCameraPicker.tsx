import React, { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { ChevronDown, ChevronLeft, ChevronRight, Search, Video } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import {
  CameraGroupConfig,
  CameraLiveActivity,
  CameraStats,
} from "../types/nvr";

export const DEFAULT_PICKER_THRESHOLD = 6;

export type PickerStatusFilter = "all" | "alert" | "offline";

export interface TacticalCameraPickerProps {
  cameras: string[];
  selectedCamera: string;
  onSelect: (cam: string) => void;
  threshold?: number;
  cameraGroups?: Record<string, CameraGroupConfig>;
  cameraStats?: Record<string, CameraStats>;
  cameraActivity?: Record<string, CameraLiveActivity>;
  ariaLabel?: string;
  openUpward?: boolean;
}

export function resolvePickerMode(
  cameraCount: number,
  threshold: number = DEFAULT_PICKER_THRESHOLD
): "inline" | "combobox" {
  const safeThreshold = Number.isFinite(threshold)
    ? Math.max(0, Math.floor(threshold))
    : DEFAULT_PICKER_THRESHOLD;
  return cameraCount <= safeThreshold ? "inline" : "combobox";
}

export function getCameraPickerStatus(
  cam: string,
  cameraStats?: Record<string, CameraStats>,
  cameraActivity?: Record<string, CameraLiveActivity>
): "alert" | "offline" | "live" {
  const act =
    cameraActivity && Object.prototype.hasOwnProperty.call(cameraActivity, cam)
      ? cameraActivity[cam]
      : undefined;
  const stat =
    cameraStats && Object.prototype.hasOwnProperty.call(cameraStats, cam)
      ? cameraStats[cam]
      : undefined;
  if (
    Boolean(act?.tamper) ||
    (act?.objects && act.objects.length > 0) ||
    Boolean(act?.motion && (act?.max_Area ?? 5000) >= 3500)
  ) {
    return "alert";
  }
  if (stat && (!Number.isFinite(stat.camera_fps) || stat.camera_fps <= 0)) {
    return "offline";
  }
  if (stat && Number.isFinite(stat.detection_fps) && stat.detection_fps > 0 && !cameraActivity) {
    return "alert";
  }
  return "live";
}

export function filterPickerCameras(
  cameras: string[],
  options: {
    query?: string;
    statusFilter?: PickerStatusFilter;
    groupFilter?: string;
    cameraGroups?: Record<string, CameraGroupConfig>;
    cameraStats?: Record<string, CameraStats>;
    cameraActivity?: Record<string, CameraLiveActivity>;
  } = {}
): string[] {
  const {
    query = "",
    statusFilter = "all",
    groupFilter = "all",
    cameraGroups,
    cameraStats,
    cameraActivity,
  } = options;
  const trimmedQuery = query.trim().toLowerCase();
  const groupSet =
    groupFilter &&
    groupFilter !== "all" &&
    cameraGroups &&
    Object.prototype.hasOwnProperty.call(cameraGroups, groupFilter) &&
    Array.isArray(cameraGroups[groupFilter]?.cameras)
      ? new Set(cameraGroups[groupFilter].cameras)
      : null;

  return cameras.filter((cam) => {
    if (groupSet && !groupSet.has(cam)) {
      return false;
    }
    if (trimmedQuery && !cam.toLowerCase().includes(trimmedQuery)) {
      return false;
    }
    if (statusFilter !== "all") {
      const status = getCameraPickerStatus(cam, cameraStats, cameraActivity);
      if (statusFilter === "alert" && status !== "alert") {
        return false;
      }
      if (statusFilter === "offline" && status !== "offline") {
        return false;
      }
    }
    return true;
  });
}

export function stepPickerCamera(
  cameras: string[],
  selectedCamera: string,
  direction: "prev" | "next"
): string {
  if (cameras.length === 0) return selectedCamera;
  const currentIdx = cameras.indexOf(selectedCamera);
  if (currentIdx === -1) {
    return cameras[0];
  }
  const delta = direction === "prev" ? -1 : 1;
  const nextIdx = (currentIdx + delta + cameras.length) % cameras.length;
  return cameras[nextIdx];
}

export const TacticalCameraPicker: React.FC<TacticalCameraPickerProps> = ({
  cameras,
  selectedCamera,
  onSelect,
  threshold = DEFAULT_PICKER_THRESHOLD,
  cameraGroups,
  cameraStats,
  cameraActivity,
  ariaLabel,
  openUpward = false,
}) => {
  const { t } = useI18n();
  const storeStats = useNvrStore((s) => s.stats?.cameras);
  const storeActivity = useNvrStore((s) => s.cameraActivity);
  const effectiveStats = cameraStats ?? storeStats;
  const effectiveActivity = cameraActivity ?? storeActivity;
  const effectiveAriaLabel = ariaLabel ?? t.picker.defaultAria;
  const mode = resolvePickerMode(cameras.length, threshold);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState<PickerStatusFilter>("all");
  const [groupFilter, setGroupFilter] = useState<string>("all");
  const [highlightedIdx, setHighlightedIdx] = useState<number>(0);

  const rootRef = useRef<HTMLDivElement | null>(null);
  const triggerBtnRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const searchInputRef = useRef<HTMLInputElement | null>(null);
  const [fixedPos, setFixedPos] = useState<React.CSSProperties | null>(null);

  const filteredCameras = useMemo(
    () =>
      filterPickerCameras(cameras, {
        query,
        statusFilter,
        groupFilter,
        cameraGroups,
        cameraStats: effectiveStats,
        cameraActivity: effectiveActivity,
      }),
    [cameras, query, statusFilter, groupFilter, cameraGroups, effectiveStats, effectiveActivity]
  );

  useEffect(() => {
    if (mode !== "combobox" && open) {
      setOpen(false);
    }
  }, [mode, open]);

  useEffect(() => {
    if (!open || mode !== "combobox") return;
    setHighlightedIdx(0);
    const tmr = window.setTimeout(() => {
      searchInputRef.current?.focus();
    }, 10);
    return () => window.clearTimeout(tmr);
  }, [open, mode, query, statusFilter, groupFilter]);

  useEffect(() => {
    if (!open || mode !== "combobox") return;
    const updateAnchor = () => {
      if (typeof window === "undefined" || !triggerBtnRef.current) return;
      const rect = triggerBtnRef.current.getBoundingClientRect();
      const popW = Math.min(340, Math.max(240, window.innerWidth - 24));
      const clampedLeft = Math.max(
        12,
        Math.min(rect.left, window.innerWidth - popW - 12)
      );
      const preferUp =
        openUpward ||
        (window.innerHeight - rect.bottom < 320 && rect.top > 320);
      setFixedPos(
        preferUp
          ? {
              position: "fixed",
              top: "auto",
              left: clampedLeft,
              bottom: Math.max(12, window.innerHeight - rect.top + 4),
              width: popW,
              zIndex: 1300,
            }
          : {
              position: "fixed",
              top: Math.max(12, rect.bottom + 4),
              bottom: "auto",
              left: clampedLeft,
              width: popW,
              zIndex: 1300,
            }
      );
    };
    updateAnchor();
    const handlePointerDown = (e: PointerEvent) => {
      const node = e.target as Node;
      if (
        rootRef.current &&
        !rootRef.current.contains(node) &&
        (!popoverRef.current || !popoverRef.current.contains(node))
      ) {
        setOpen(false);
      }
    };
    window.addEventListener("pointerdown", handlePointerDown);
    window.addEventListener("resize", updateAnchor);
    window.addEventListener("scroll", updateAnchor, true);
    return () => {
      window.removeEventListener("pointerdown", handlePointerDown);
      window.removeEventListener("resize", updateAnchor);
      window.removeEventListener("scroll", updateAnchor, true);
    };
  }, [open, mode, openUpward]);

  if (mode === "inline") {
    return (
      <div
        className="nvr-picker-inline"
        role="group"
        aria-label={effectiveAriaLabel}
        style={{ display: "inline-flex", alignItems: "center", gap: 4, flexWrap: "wrap" }}
      >
        {cameras.map((cam) => {
          const hasCamStat = Boolean(
            effectiveStats && Object.prototype.hasOwnProperty.call(effectiveStats, cam)
          );
          const hasCamAct = Boolean(
            effectiveActivity && Object.prototype.hasOwnProperty.call(effectiveActivity, cam)
          );
          const st =
            hasCamStat || hasCamAct
              ? getCameraPickerStatus(cam, effectiveStats, effectiveActivity)
              : null;
          const itemDot =
            st === "alert" ? "motion" : st === "offline" ? "alert" : "live";
          return (
            <button
              key={cam}
              type="button"
              className={`nvr-btn sm mono ${selectedCamera === cam ? "active" : ""}`}
              style={{
                textTransform: "uppercase",
                display: "inline-flex",
                alignItems: "center",
                gap: 5,
              }}
              onClick={() => onSelect(cam)}
            >
              {st && <span className={`nvr-dot ${itemDot}`} />}
              {cam}
            </button>
          );
        })}
      </div>
    );
  }

  const currentIdx = Math.max(0, cameras.indexOf(selectedCamera));
  const displayIdx = cameras.length > 0 ? currentIdx + 1 : 0;
  const activeCamLabel = (selectedCamera || cameras[0] || "CAM").toUpperCase();
  const safeHighlightedIdx =
    filteredCameras.length > 0
      ? Math.min(Math.max(0, highlightedIdx), filteredCameras.length - 1)
      : 0;
  const activeStatus = getCameraPickerStatus(
    selectedCamera || cameras[0] || "",
    effectiveStats,
    effectiveActivity
  );
  const dotClass =
    activeStatus === "alert"
      ? "motion"
      : activeStatus === "offline"
      ? "alert"
      : "live";

  const groupEntries = cameraGroups
    ? Object.entries(cameraGroups).sort(
        ([aKey, aVal], [bKey, bVal]) =>
          ((aVal.order ?? 0) - (bVal.order ?? 0)) || aKey.localeCompare(bKey)
      )
    : [];

  return (
    <div ref={rootRef} className="nvr-picker-combobox" aria-label={effectiveAriaLabel}>
      <button
        type="button"
        className="nvr-btn sm mono"
        style={{ padding: "0 6px" }}
        aria-label={t.picker.prevCamera}
        title={t.picker.prevCamera}
        onClick={() => onSelect(stepPickerCamera(cameras, selectedCamera, "prev"))}
      >
        <ChevronLeft size={12} />
      </button>

      <button
        ref={triggerBtnRef}
        type="button"
        className={`nvr-btn sm mono nvr-picker-trigger ${open ? "active" : ""}`}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span className={`nvr-dot ${dotClass}`} />
        <Video size={11} />
        <span>
          {activeCamLabel} ({displayIdx} / {cameras.length})
        </span>
        <ChevronDown size={11} />
      </button>

      <button
        type="button"
        className="nvr-btn sm mono"
        style={{ padding: "0 6px" }}
        aria-label={t.picker.nextCamera}
        title={t.picker.nextCamera}
        onClick={() => onSelect(stepPickerCamera(cameras, selectedCamera, "next"))}
      >
        <ChevronRight size={12} />
      </button>

      {open &&
        (() => {
          const popoverNode = (
            <div
              ref={popoverRef}
              className="nvr-picker-popover"
              role="dialog"
              aria-label={t.picker.dialogAria}
              style={
                fixedPos
                  ? fixedPos
                  : openUpward
                    ? { top: "auto", bottom: "calc(100% + 4px)" }
                    : undefined
              }
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.preventDefault();
                  e.stopPropagation();
                  setOpen(false);
                  triggerBtnRef.current?.focus();
                } else if (e.key === "ArrowDown") {
              e.preventDefault();
              setHighlightedIdx((idx) =>
                filteredCameras.length > 0 ? (idx + 1) % filteredCameras.length : 0
              );
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setHighlightedIdx((idx) =>
                filteredCameras.length > 0
                  ? (idx - 1 + filteredCameras.length) % filteredCameras.length
                  : 0
              );
            } else if (e.key === "Enter") {
              e.preventDefault();
              const chosen = filteredCameras[safeHighlightedIdx];
              if (chosen) {
                onSelect(chosen);
                setOpen(false);
              }
            }
          }}
        >
          <div className="nvr-picker-search-row">
            <Search size={12} style={{ color: "var(--text-muted)", flexShrink: 0 }} />
            <input
              ref={searchInputRef}
              type="text"
              className="nvr-input sm mono"
              style={{ flex: 1 }}
              placeholder={t.picker.searchPlaceholder.replace("{n}", String(cameras.length))}
              aria-label={t.picker.searchAria}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </div>

          <div className="nvr-picker-filter-row">
            {(
              [
                ["all", t.picker.filterAll],
                ["alert", t.picker.filterAlert],
                ["offline", t.picker.filterOffline],
              ] as Array<[PickerStatusFilter, string]>
            ).map(([flt, label]) => (
              <button
                key={flt}
                type="button"
                className={`nvr-btn sm mono ${statusFilter === flt ? "active" : ""}`}
                style={{ padding: "0 7px" }}
                onClick={() => setStatusFilter(flt)}
              >
                {label}
              </button>
            ))}
          </div>

          {groupEntries.length > 0 && (
            <div className="nvr-picker-filter-row">
              <button
                type="button"
                className={`nvr-btn sm mono ${groupFilter === "all" ? "active" : ""}`}
                style={{ padding: "0 6px" }}
                onClick={() => setGroupFilter("all")}
              >
                {t.picker.allGroups}
              </button>
              {groupEntries.map(([gName]) => (
                <button
                  key={gName}
                  type="button"
                  className={`nvr-btn sm mono ${groupFilter === gName ? "active" : ""}`}
                  style={{
                    padding: "0 6px",
                    textTransform: "uppercase",
                  }}
                  onClick={() => setGroupFilter(gName)}
                >
                  {gName}
                </button>
              ))}
            </div>
          )}

          <div className="nvr-picker-list" role="listbox" aria-label={t.picker.optionsAria}>
            {filteredCameras.length === 0 ? (
              <div
                className="mono"
                style={{
                  padding: "10px 8px",
                  fontSize: "var(--text-xs)",
                  color: "var(--text-muted)",
                  textAlign: "center",
                }}
              >
                {t.picker.noMatchingCameras}
              </div>
            ) : (
              filteredCameras.map((cam, idx) => {
                const isSelected = cam === selectedCamera;
                const isHighlighted = idx === safeHighlightedIdx;
                const st = getCameraPickerStatus(cam, effectiveStats, effectiveActivity);
                const itemDot =
                  st === "alert" ? "motion" : st === "offline" ? "alert" : "live";
                const fps =
                  effectiveStats && Object.prototype.hasOwnProperty.call(effectiveStats, cam)
                    ? effectiveStats[cam]?.camera_fps
                    : undefined;
                return (
                  <button
                    key={cam}
                    type="button"
                    role="option"
                    aria-selected={isSelected}
                    className={`nvr-picker-option mono ${
                      isSelected ? "is-selected" : ""
                    } ${isHighlighted ? "is-highlighted" : ""}`}
                    onMouseEnter={() => setHighlightedIdx(idx)}
                    onClick={() => {
                      onSelect(cam);
                      setOpen(false);
                    }}
                  >
                    <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                      <span className={`nvr-dot ${itemDot}`} />
                      <span style={{ textTransform: "uppercase", fontWeight: 600 }}>
                        {cam}
                      </span>
                    </span>
                    <span style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                      {typeof fps === "number" && Number.isFinite(fps)
                        ? `${fps.toFixed(1)} FPS`
                        : st.toUpperCase()}
                    </span>
                  </button>
                );
              })
            )}
          </div>
            </div>
          );
          return typeof document !== "undefined" && document.body
            ? createPortal(popoverNode, document.body)
            : popoverNode;
        })()}
    </div>
  );
};
