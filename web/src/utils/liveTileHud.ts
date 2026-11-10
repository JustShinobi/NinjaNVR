import type { WorkspaceTab } from "./format";

/**
 * Live tile HUD rules ("HUD by exception").
 *
 * A camera tile shows three layers of information:
 *  1. Identity — always: camera name + a status dot (live / time-shifted / offline).
 *  2. Exception — only while it is true: ONE chip, chosen by priority.
 *  3. On demand — action bar on hover/tap and the Camera Details sheet (telemetry and
 *     the enabled/detect/record/alerts switches).
 *
 * Normal state is not news: "EN:ON DET:ON REC:ON ALR:ON", resolution and FPS never
 * render over the video. Keep this module free of React so it stays unit-testable.
 */

export type TileIdentityTone = "live" | "rewind" | "offline";

export type TileExceptionKind = "tamper" | "object" | "roi" | "record-off" | "motion";

export interface TileException {
  kind: TileExceptionKind;
  label: string;
  tone: "alert" | "motion";
  /** Additional exceptions hidden behind the primary chip (shown as `+N`). */
  hiddenCount: number;
}

export interface TileExceptionInput {
  tamper?: boolean;
  objectLabels?: readonly string[];
  /** Labels of ROI state badges currently flagged anomalous. */
  roiAnomalies?: readonly string[];
  recordEnabled?: boolean;
  motion?: boolean;
}

const OBJECT_LABEL_LIMIT = 2;

/**
 * Picks the single exception chip for a tile.
 * Priority: TAMPER > tracked objects > anomalous ROI > REC OFF > MOTION.
 */
export function resolveTileException(input: TileExceptionInput): TileException | null {
  const candidates: Array<Omit<TileException, "hiddenCount">> = [];
  if (input.tamper) {
    candidates.push({ kind: "tamper", label: "TAMPER", tone: "alert" });
  }
  const labels = Array.from(
    new Set((input.objectLabels ?? []).map((l) => l.trim().toUpperCase()).filter(Boolean))
  );
  if (labels.length > 0) {
    const shown = labels.slice(0, OBJECT_LABEL_LIMIT).join(", ");
    const extra = labels.length - OBJECT_LABEL_LIMIT;
    candidates.push({
      kind: "object",
      label: extra > 0 ? `${shown} +${extra}` : shown,
      tone: "alert",
    });
  }
  const roi = (input.roiAnomalies ?? []).map((l) => l.trim()).filter(Boolean);
  if (roi.length > 0) {
    candidates.push({ kind: "roi", label: roi[0].toUpperCase(), tone: "alert" });
  }
  if (input.recordEnabled === false) {
    candidates.push({ kind: "record-off", label: "REC OFF", tone: "motion" });
  }
  // Motion is implied by any object/tamper chip, so it only surfaces on its own.
  if (input.motion && labels.length === 0 && !input.tamper) {
    candidates.push({ kind: "motion", label: "MOTION", tone: "motion" });
  }
  if (candidates.length === 0) return null;
  return { ...candidates[0], hiddenCount: candidates.length - 1 };
}

export function resolveTileIdentityTone(params: {
  timeShifted: boolean;
  cameraFps?: number;
  hasStats: boolean;
}): TileIdentityTone {
  if (params.timeShifted) return "rewind";
  if (params.hasStats && (params.cameraFps ?? 0) <= 0) return "offline";
  return "live";
}

/** Single server-side snapshot endpoint (full-res main stream, privacy-masked). */
export function buildCameraSnapshotUrl(camera: string): string {
  return `/api/${encodeURIComponent(camera)}/snapshot.jpg`;
}

export interface CameraTelemetryRow {
  key: "resolution" | "camera_fps" | "ai_fps" | "image";
  label: string;
  value: string;
}

export function formatCameraTelemetryRows(params: {
  detectWidth?: number;
  detectHeight?: number;
  cameraFps?: number;
  detectionFps?: number;
  nightMode?: boolean;
  isPt?: boolean;
}): CameraTelemetryRow[] {
  const pt = Boolean(params.isPt);
  const fmt = (v: number | undefined) =>
    Number.isFinite(v) ? `${(v as number).toFixed(1)} FPS` : "—";
  return [
    {
      key: "resolution",
      label: pt ? "Resolução" : "Resolution",
      value:
        params.detectWidth && params.detectHeight
          ? `${params.detectWidth}×${params.detectHeight}`
          : "—",
    },
    { key: "camera_fps", label: pt ? "Câmera" : "Camera", value: fmt(params.cameraFps) },
    { key: "ai_fps", label: pt ? "Detecção IA" : "AI detect", value: fmt(params.detectionFps) },
    {
      key: "image",
      label: pt ? "Imagem" : "Image",
      value: params.nightMode ? (pt ? "IR noturno" : "IR night") : pt ? "Dia" : "Day",
    },
  ];
}

export type CameraRuntimeFlag = "enabled" | "detect" | "record" | "alerts";

export function resolveCameraRuntimeFlags(config?: {
  enabled?: boolean;
  detect?: boolean;
  record?: boolean;
  alerts?: boolean;
}, activityEnabled?: boolean): Record<CameraRuntimeFlag, boolean> {
  return {
    enabled: config?.enabled ?? activityEnabled ?? true,
    detect: config?.detect ?? true,
    record: config?.record ?? true,
    alerts: config?.alerts ?? true,
  };
}

/* ---------------------------------------------------------------------------
 * Mobile shell
 * ------------------------------------------------------------------------- */

/** Viewports narrower than this use the mobile shell (bottom tab bar, 1-column live). */
export const MOBILE_BREAKPOINT_PX = 768;

/** Phones in landscape are wider than the breakpoint but short and touch-driven. */
export const COMPACT_LANDSCAPE_MAX_HEIGHT_PX = 500;

export const COMPACT_MEDIA_QUERY = `(max-width: ${MOBILE_BREAKPOINT_PX - 1}px), (pointer: coarse) and (max-height: ${COMPACT_LANDSCAPE_MAX_HEIGHT_PX}px)`;

export function isCompactViewport(width: number, height?: number, coarsePointer = false): boolean {
  if (!Number.isFinite(width) || width <= 0) return false;
  if (width < MOBILE_BREAKPOINT_PX) return true;
  return (
    coarsePointer &&
    typeof height === "number" &&
    Number.isFinite(height) &&
    height > 0 &&
    height <= COMPACT_LANDSCAPE_MAX_HEIGHT_PX
  );
}

export type { WorkspaceTab };

export interface MobileNavLayout {
  primary: WorkspaceTab[];
  overflow: WorkspaceTab[];
}

/** Four thumb-reachable tabs plus a "More" sheet for the rest. */
export function resolveMobileNavTabs(role: string): MobileNavLayout {
  const primary: WorkspaceTab[] = ["live", "sync", "review", "exports"];
  const overflow: WorkspaceTab[] = ["explore"];
  if (role === "admin") overflow.push("zones", "system");
  return { primary, overflow };
}

export function formatBadgeCount(count: number, max = 99): string {
  if (!Number.isFinite(count) || count <= 0) return "";
  return count > max ? `${max}+` : String(Math.floor(count));
}

/* ---------------------------------------------------------------------------
 * Compact (phone) live stage
 * ------------------------------------------------------------------------- */

export type CompactGridColumns = 1 | 2;

/**
 * On phones the matrix becomes a vertically scrolling list of 16:9 tiles
 * (1 or 2 columns) and focus/spotlight keep only the hero tile — the camera
 * picker replaces the dock column so no extra decoders run on mobile data.
 */
export function resolveCompactStageContainerStyle(
  liveLayout: string,
  columns: CompactGridColumns
): Record<string, string | number> {
  if (liveLayout === "2x2") {
    return {
      gridTemplateColumns: columns === 2 ? "repeat(2, minmax(0, 1fr))" : "minmax(0, 1fr)",
      gridTemplateRows: "none",
      // max-content: 16:9 tiles keep their height instead of being squeezed to the viewport
      gridAutoRows: "max-content",
      alignContent: "start",
      gap: 8,
      overflowY: "auto",
    };
  }
  return {
    gridTemplateColumns: "minmax(0, 1fr)",
    gridTemplateRows: "none",
    gridAutoRows: "max-content",
    alignContent: "start",
    flex: "0 0 auto",
    overflowY: "visible",
  };
}

export function filterCompactStageDescriptors<T extends { role: string }>(
  liveLayout: string,
  descriptors: readonly T[]
): T[] {
  if (liveLayout === "2x2") return [...descriptors];
  return descriptors.filter((d) => d.role !== "dock");
}

export function sanitizeCompactGridColumns(raw: unknown): CompactGridColumns {
  return raw === 2 || raw === "2" ? 2 : 1;
}

/** Number of Review filters that differ from their defaults (drives the mobile "Filters (N)" button). */
export function countActiveReviewFilters(filters: {
  camera?: string;
  severity?: string;
  object?: string;
  status?: string;
  date?: string;
  excludeObject?: string;
  zone?: string;
  plate?: string;
}): number {
  let n = 0;
  if (filters.camera && filters.camera !== "all") n += 1;
  if (filters.severity && filters.severity !== "all") n += 1;
  if (filters.object && filters.object !== "all") n += 1;
  if (filters.status && filters.status !== "unreviewed") n += 1;
  if (filters.date && filters.date.trim()) n += 1;
  if (filters.excludeObject && filters.excludeObject !== "none") n += 1;
  if (filters.zone && filters.zone !== "all") n += 1;
  if (filters.plate && filters.plate.trim()) n += 1;
  return n;
}

/** Number of Object Explore filters that differ from their defaults (search text excluded: it stays visible). */
export function countActiveExploreFilters(filters: {
  camera?: string;
  label?: string;
  excludeLabel?: string;
  zone?: string;
  minScore?: number;
  stationary?: string;
  retainedOnly?: boolean;
  plate?: string;
  date?: string;
}): number {
  let n = 0;
  if (filters.camera && filters.camera !== "all") n += 1;
  if (filters.label && filters.label !== "all") n += 1;
  if (filters.excludeLabel && filters.excludeLabel !== "none") n += 1;
  if (filters.zone && filters.zone !== "all") n += 1;
  if (typeof filters.minScore === "number" && filters.minScore > 0) n += 1;
  if (filters.stationary && filters.stationary !== "all") n += 1;
  if (filters.retainedOnly) n += 1;
  if (filters.plate && filters.plate.trim()) n += 1;
  if (filters.date && filters.date.trim()) n += 1;
  return n;
}
