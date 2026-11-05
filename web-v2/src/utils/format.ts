import type { ExportItem } from "../types/nvr";

export type HudPosition =
  | "bottom-right"
  | "top-right"
  | "top-left"
  | "bottom-left"
  | "hidden";

export type WorkspaceTab =
  | "live"
  | "sync"
  | "review"
  | "explore"
  | "exports"
  | "zones"
  | "system";

export type LiveLayoutMode = "2x2" | "focus" | "single" | "spotlight" | "map";
export type MatrixDensity = "2x2" | "3x3" | "4x4";
export type SpotlightFilter = "all_activity" | "objects" | "person" | "vehicle";
export type SpotlightTimelineMode = "reactive" | "wall";
export type FirstWeekday = "sunday" | "monday";
export type ThemeMode = "dark" | "light" | "system";
export type EffectiveTheme = "dark" | "light";

export function resolveInitialThemeMode(
  fallback: ThemeMode = "dark"
): ThemeMode {
  try {
    if (typeof window === "undefined") return fallback;
    const saved = window.localStorage?.getItem("ninjanvr_theme_mode");
    if (saved === "dark" || saved === "light" || saved === "system") {
      return saved;
    }
  } catch {
    // ignore storage access errors
  }
  return fallback;
}

export function resolveEffectiveTheme(
  mode: ThemeMode,
  prefersLight?: boolean
): EffectiveTheme {
  if (mode === "light") return "light";
  if (mode === "dark") return "dark";
  if (typeof prefersLight === "boolean") {
    return prefersLight ? "light" : "dark";
  }
  if (
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function"
  ) {
    try {
      return window.matchMedia("(prefers-color-scheme: light)").matches
        ? "light"
        : "dark";
    } catch {
      return "dark";
    }
  }
  return "dark";
}

export function cycleThemeMode(current: ThemeMode): ThemeMode {
  if (current === "dark") return "light";
  if (current === "light") return "system";
  return "dark";
}

export function applyThemeToDocument(
  mode: ThemeMode,
  highContrast: boolean,
  prefersLight?: boolean
): EffectiveTheme {
  const effective = resolveEffectiveTheme(mode, prefersLight);
  if (typeof document !== "undefined" && document.documentElement) {
    const root = document.documentElement;
    if (root.dataset) {
      root.dataset.theme = effective;
      root.dataset.highContrast = highContrast ? "true" : "false";
    }
    if (typeof root.setAttribute === "function") {
      root.setAttribute("data-theme", effective);
      root.setAttribute("data-high-contrast", highContrast ? "true" : "false");
    }
  }
  return effective;
}

export function resolveInitialTab(): WorkspaceTab {
  if (typeof window === "undefined") return "live";
  const path = window.location.pathname.toLowerCase();
  if (path.startsWith("/sync")) return "sync";
  if (path.startsWith("/review")) return "review";
  if (path.startsWith("/explore")) return "explore";
  if (path.startsWith("/export")) return "exports";
  if (path.startsWith("/config") || path.startsWith("/settings")) return "zones";
  if (path.startsWith("/system") || path.startsWith("/logs")) return "system";
  return "live";
}

export function resolveInitialHudPosition(): HudPosition {
  try {
    if (typeof window === "undefined") return "bottom-right";
    const saved = window.localStorage?.getItem("ninjanvr_hud_position");
    if (
      saved === "bottom-right" ||
      saved === "top-right" ||
      saved === "top-left" ||
      saved === "bottom-left" ||
      saved === "hidden"
    ) {
      return saved;
    }
  } catch {
    // ignore storage access errors
  }
  return "bottom-right";
}

export function resolveInitialLiveLayout(): LiveLayoutMode {
  try {
    if (typeof window === "undefined") return "2x2";
    const saved = window.localStorage?.getItem("ninjanvr_live_layout");
    if (
      saved === "2x2" ||
      saved === "focus" ||
      saved === "single" ||
      saved === "spotlight" ||
      saved === "map"
    ) {
      return saved;
    }
  } catch {
    // ignore storage access errors
  }
  return "2x2";
}

export function resolveInitialMatrixDensity(): MatrixDensity {
  try {
    if (typeof window === "undefined") return "2x2";
    const saved = window.localStorage?.getItem("ninjanvr_matrix_density");
    if (saved === "2x2" || saved === "3x3" || saved === "4x4") {
      return saved;
    }
  } catch {
    // ignore storage access errors
  }
  return "2x2";
}

export function resolveInitialSpotlightTimelineMode(
  fallback: SpotlightTimelineMode = "reactive"
): SpotlightTimelineMode {
  try {
    if (typeof window === "undefined") return fallback;
    const saved = window.localStorage?.getItem(
      "ninjanvr_spotlight_timeline_mode"
    );
    if (saved === "reactive" || saved === "wall") {
      return saved;
    }
  } catch {
    // ignore storage access errors
  }
  return fallback;
}

export function resolveInitialBool(key: string, fallback: boolean): boolean {
  try {
    if (typeof window === "undefined") return fallback;
    const saved = window.localStorage?.getItem(key);
    if (saved === "true") return true;
    if (saved === "false") return false;
  } catch {
    // ignore storage access errors
  }
  return fallback;
}

export function resolveInitialPlaybackSpeed(): number {
  try {
    if (typeof window === "undefined") return 1;
    const saved = Number(window.localStorage?.getItem("ninjanvr_playback_speed"));
    if (saved === 0.5 || saved === 1 || saved === 2 || saved === 4) {
      return saved;
    }
  } catch {
    // ignore storage access errors
  }
  return 1;
}

export function resolveInitialFirstWeekday(): FirstWeekday {
  try {
    if (typeof window === "undefined") return "sunday";
    const saved = window.localStorage?.getItem("ninjanvr_first_weekday");
    if (saved === "monday" || saved === "sunday") return saved;
  } catch {
    // ignore storage access errors
  }
  return "sunday";
}

export function formatTs(unixSec: number): string {
  if (typeof unixSec !== "number" || !Number.isFinite(unixSec)) {
    return "--:--:--";
  }
  const d = new Date(unixSec * 1000);
  if (Number.isNaN(d.getTime())) {
    return "--:--:--";
  }
  return d.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

export function formatClockTime(unixSec: number): string {
  return formatTs(unixSec);
}

export function formatDuration(sec: number): string {
  const safeSec = typeof sec === "number" && Number.isFinite(sec) ? sec : 0;
  const total = Math.max(1, Math.round(safeSec));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m === 0) return `${s}s`;
  return `${m}m ${s.toString().padStart(2, "0")}s`;
}

export function formatDateTime(unixSec: number): string {
  if (typeof unixSec !== "number" || !Number.isFinite(unixSec)) {
    return "--:--:--";
  }
  const d = new Date(unixSec * 1000);
  if (Number.isNaN(d.getTime())) {
    return "--:--:--";
  }
  return d.toLocaleString([], {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

export function formatRelTime(unixSec: number): string {
  if (typeof unixSec !== "number" || !Number.isFinite(unixSec) || unixSec <= 0) {
    return "0s ago";
  }
  const diff = Math.max(0, Math.round(Date.now() / 1000 - unixSec));
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

export function formatUptime(sec: number): string {
  const safeSec =
    typeof sec === "number" && Number.isFinite(sec)
      ? Math.max(0, Math.floor(sec))
      : 0;
  const h = Math.floor(safeSec / 3600);
  const m = Math.floor((safeSec % 3600) / 60);
  const s = safeSec % 60;
  return `${h}h ${m}m ${s}s`;
}

export interface HeaderTelemetryMeta {
  wsDotClass: "live" | "motion";
  wsStatusLabel: string;
  channelBadge: string;
  tooltip: string;
}

export function formatDisplayVersion(
  rawServerVer?: string | null,
  fallbackRelease: string = "CONSOLE"
): string {
  const clean = (rawServerVer || "").trim();
  if (!clean) return fallbackRelease;
  return clean.startsWith("v") ? clean : `v${clean}`;
}

export function resolveHeaderTelemetryMeta(
  wsOnline: boolean,
  channelCount: number
): HeaderTelemetryMeta {
  const safeCount =
    typeof channelCount === "number" && Number.isFinite(channelCount)
      ? Math.max(0, Math.floor(channelCount))
      : 0;
  const channelBadge = `${safeCount} CH`;
  const wsDotClass = wsOnline ? "live" : "motion";
  const wsStatusLabel = wsOnline
    ? "Real-time WebSocket stream online"
    : "WebSocket reconnecting (HTTP fallback active)";
  return {
    wsDotClass,
    wsStatusLabel,
    channelBadge,
    tooltip: `Inspect System Telemetry & OpenVINO Detector · ${channelBadge} · ${wsStatusLabel}`,
  };
}

export function findMatchingExport(
  exportsList: ExportItem[],
  camera: string,
  startTime: number,
  endTime: number | null
): ExportItem | null {
  if (
    !exportsList ||
    exportsList.length === 0 ||
    typeof startTime !== "number" ||
    !Number.isFinite(startTime)
  ) {
    return null;
  }
  const itemStart = Math.floor(startTime);
  const safeEnd =
    typeof endTime === "number" && Number.isFinite(endTime) && endTime > startTime
      ? endTime
      : startTime + 15;
  const itemEnd = Math.ceil(safeEnd);
  for (const ex of exportsList) {
    if (!ex || ex.camera !== camera || typeof ex.id !== "string") continue;
    const parts = ex.id.split("_");
    if (parts.length >= 3) {
      const rawStartStr = parts[parts.length - 2].trim();
      const rawEndStr = parts[parts.length - 1].trim();
      const expStart = rawStartStr ? Number(rawStartStr) : Number.NaN;
      const expEnd = rawEndStr ? Number(rawEndStr) : Number.NaN;
      if (
        Number.isFinite(expStart) &&
        Number.isFinite(expEnd) &&
        expStart > 0 &&
        expEnd > 0 &&
        expStart <= itemEnd &&
        expEnd >= itemStart
      ) {
        return ex;
      }
    }
    if (ex.name && ex.name.includes(String(itemStart))) {
      return ex;
    }
  }
  return null;
}
