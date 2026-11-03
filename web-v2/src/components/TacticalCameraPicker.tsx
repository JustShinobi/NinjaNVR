import React, { useEffect, useMemo, useRef, useState } from "react";
import { ChevronDown, ChevronLeft, ChevronRight, Search, Video } from "lucide-react";
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
  return cameraCount <= threshold ? "inline" : "combobox";
}

export function getCameraPickerStatus(
  cam: string,
  cameraStats?: Record<string, CameraStats>,
  cameraActivity?: Record<string, CameraLiveActivity>
): "alert" | "offline" | "live" {
  const act = cameraActivity?.[cam];
  const stat = cameraStats?.[cam];
  if (
    Boolean(act?.tamper) ||
    (act?.objects && act.objects.length > 0) ||
    Boolean(act?.motion && (act?.max_Area ?? 5000) >= 3500)
  ) {
    return "alert";
  }
  if (stat && stat.camera_fps <= 0) {
    return "offline";
  }
  if (stat && stat.detection_fps > 0 && !cameraActivity) {
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
    cameraGroups[groupFilter]?.cameras
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
  ariaLabel = "Camera selector",
  openUpward = false,
}) => {
  const mode = resolvePickerMode(cameras.length, threshold);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState<PickerStatusFilter>("all");
  const [groupFilter, setGroupFilter] = useState<string>("all");
  const [highlightedIdx, setHighlightedIdx] = useState<number>(0);

  const rootRef = useRef<HTMLDivElement | null>(null);
  const searchInputRef = useRef<HTMLInputElement | null>(null);

  const filteredCameras = useMemo(
    () =>
      filterPickerCameras(cameras, {
        query,
        statusFilter,
        groupFilter,
        cameraGroups,
        cameraStats,
        cameraActivity,
      }),
    [cameras, query, statusFilter, groupFilter, cameraGroups, cameraStats, cameraActivity]
  );

  useEffect(() => {
    if (!open) return;
    setHighlightedIdx(0);
    const tmr = window.setTimeout(() => {
      searchInputRef.current?.focus();
    }, 10);
    return () => window.clearTimeout(tmr);
  }, [open, query, statusFilter, groupFilter]);

  useEffect(() => {
    if (!open) return;
    const handlePointerDown = (e: PointerEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  if (mode === "inline") {
    return (
      <div
        className="nvr-picker-inline"
        role="group"
        aria-label={ariaLabel}
        style={{ display: "inline-flex", alignItems: "center", gap: 4, flexWrap: "wrap" }}
      >
        {cameras.map((cam) => (
          <button
            key={cam}
            type="button"
            className={`nvr-btn mono ${selectedCamera === cam ? "active" : ""}`}
            style={{
              height: 24,
              padding: "0 8px",
              fontSize: "var(--text-xs)",
              textTransform: "uppercase",
            }}
            onClick={() => onSelect(cam)}
          >
            {cam}
          </button>
        ))}
      </div>
    );
  }

  const currentIdx = Math.max(0, cameras.indexOf(selectedCamera));
  const activeCamLabel = (selectedCamera || cameras[0] || "CAM").toUpperCase();
  const activeStatus = getCameraPickerStatus(
    selectedCamera || cameras[0] || "",
    cameraStats,
    cameraActivity
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
    <div ref={rootRef} className="nvr-picker-combobox" aria-label={ariaLabel}>
      <button
        type="button"
        className="nvr-btn mono"
        style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
        aria-label="Previous camera"
        title="Previous camera"
        onClick={() => onSelect(stepPickerCamera(cameras, selectedCamera, "prev"))}
      >
        <ChevronLeft size={12} />
      </button>

      <button
        type="button"
        className={`nvr-btn mono nvr-picker-trigger ${open ? "active" : ""}`}
        style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        <span className={`nvr-dot ${dotClass}`} />
        <Video size={11} />
        <span>
          {activeCamLabel} ({currentIdx + 1} / {cameras.length})
        </span>
        <ChevronDown size={11} />
      </button>

      <button
        type="button"
        className="nvr-btn mono"
        style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
        aria-label="Next camera"
        title="Next camera"
        onClick={() => onSelect(stepPickerCamera(cameras, selectedCamera, "next"))}
      >
        <ChevronRight size={12} />
      </button>

      {open && (
        <div
          className="nvr-picker-popover"
          role="dialog"
          aria-label="Searchable camera selector"
          style={
            openUpward
              ? { top: "auto", bottom: "calc(100% + 4px)" }
              : undefined
          }
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              e.preventDefault();
              setOpen(false);
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
              const chosen = filteredCameras[highlightedIdx];
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
              className="nvr-input mono"
              style={{ flex: 1, height: 24, fontSize: "var(--text-xs)" }}
              placeholder={`Search ${cameras.length} cameras...`}
              aria-label="Search cameras"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </div>

          <div className="nvr-picker-filter-row">
            {(
              [
                ["all", "All"],
                ["alert", "Alert/Motion"],
                ["offline", "Offline"],
              ] as Array<[PickerStatusFilter, string]>
            ).map(([flt, label]) => (
              <button
                key={flt}
                type="button"
                className={`nvr-btn mono ${statusFilter === flt ? "active" : ""}`}
                style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
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
                className={`nvr-btn mono ${groupFilter === "all" ? "active" : ""}`}
                style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                onClick={() => setGroupFilter("all")}
              >
                ALL GROUPS
              </button>
              {groupEntries.map(([gName]) => (
                <button
                  key={gName}
                  type="button"
                  className={`nvr-btn mono ${groupFilter === gName ? "active" : ""}`}
                  style={{
                    height: 22,
                    padding: "0 6px",
                    fontSize: "var(--text-xs)",
                    textTransform: "uppercase",
                  }}
                  onClick={() => setGroupFilter(gName)}
                >
                  {gName}
                </button>
              ))}
            </div>
          )}

          <div className="nvr-picker-list" role="listbox" aria-label="Camera options">
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
                No matching cameras
              </div>
            ) : (
              filteredCameras.map((cam, idx) => {
                const isSelected = cam === selectedCamera;
                const isHighlighted = idx === highlightedIdx;
                const st = getCameraPickerStatus(cam, cameraStats, cameraActivity);
                const itemDot =
                  st === "alert" ? "motion" : st === "offline" ? "alert" : "live";
                const fps = cameraStats?.[cam]?.camera_fps;
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
                      {fps !== undefined ? `${fps.toFixed(1)} FPS` : st.toUpperCase()}
                    </span>
                  </button>
                );
              })
            )}
          </div>
        </div>
      )}
    </div>
  );
};
