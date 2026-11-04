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
  if (typeof window === "undefined") return fallback;
  const saved = window.localStorage?.getItem("ninjanvr_theme_mode");
  if (saved === "dark" || saved === "light" || saved === "system") {
    return saved;
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
  return "bottom-right";
}

export function resolveInitialLiveLayout(): LiveLayoutMode {
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
  return "2x2";
}

export function resolveInitialMatrixDensity(): MatrixDensity {
  if (typeof window === "undefined") return "2x2";
  const saved = window.localStorage?.getItem("ninjanvr_matrix_density");
  if (saved === "2x2" || saved === "3x3" || saved === "4x4") {
    return saved;
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
  if (typeof window === "undefined") return fallback;
  const saved = window.localStorage?.getItem(key);
  if (saved === "true") return true;
  if (saved === "false") return false;
  return fallback;
}

export function resolveInitialPlaybackSpeed(): number {
  if (typeof window === "undefined") return 1;
  const saved = Number(window.localStorage?.getItem("ninjanvr_playback_speed"));
  if (saved === 0.5 || saved === 1 || saved === 2 || saved === 4) {
    return saved;
  }
  return 1;
}

export function resolveInitialFirstWeekday(): FirstWeekday {
  if (typeof window === "undefined") return "sunday";
  const saved = window.localStorage?.getItem("ninjanvr_first_weekday");
  if (saved === "monday" || saved === "sunday") return saved;
  return "sunday";
}

export function formatTs(unixSec: number): string {
  const d = new Date(unixSec * 1000);
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
  const total = Math.max(1, Math.round(sec));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  if (h > 0) return `${h}h ${m}m ${s}s`;
  if (m === 0) return `${s}s`;
  return `${m}m ${s.toString().padStart(2, "0")}s`;
}

export function formatDateTime(unixSec: number): string {
  const d = new Date(unixSec * 1000);
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
  const diff = Math.max(0, Math.round(Date.now() / 1000 - unixSec));
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

export function formatUptime(sec: number): string {
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
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
  const safeCount = Math.max(0, Math.floor(channelCount || 0));
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
  if (!exportsList || exportsList.length === 0) return null;
  const itemStart = Math.floor(startTime);
  const itemEnd = Math.ceil(endTime && endTime > startTime ? endTime : startTime + 15);
  for (const ex of exportsList) {
    if (ex.camera !== camera) continue;
    const parts = ex.id.split("_");
    if (parts.length >= 3) {
      const expStart = Number(parts[parts.length - 2]);
      const expEnd = Number(parts[parts.length - 1]);
      if (
        Number.isFinite(expStart) &&
        Number.isFinite(expEnd) &&
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
