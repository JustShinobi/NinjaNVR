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

export type LiveLayoutMode = "2x2" | "focus" | "single";
export type FirstWeekday = "sunday" | "monday";

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
  if (saved === "2x2" || saved === "focus" || saved === "single") {
    return saved;
  }
  return "2x2";
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
