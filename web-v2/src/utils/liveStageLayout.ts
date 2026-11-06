import type React from "react";
import {
  LiveLayoutMode,
  MatrixDensity,
  SpotlightTimelineMode,
} from "./format";
import { prewarmStream } from "./msePlayerEngine";

export type LiveLayout = LiveLayoutMode;

export type StageTileRole = "grid" | "hero" | "dock";
export type StageTileRenderMode = "mse" | "vod" | "snapshot";

export const DEFAULT_MAX_DOCK_LIVE_DECODERS = 6;
export const DEFAULT_MAX_GRID_LIVE_DECODERS = 8;

export interface LiveStageTileDescriptor {
  camera: string;
  role: StageTileRole;
  renderMode: StageTileRenderMode;
  useMainStream: boolean;
  dockIndex: number;
  dockTotal: number;
  isTimelineSelected: boolean;
  isSpotlightHeld: boolean;
}

export interface ComputeLiveStageDescriptorsParams {
  liveLayout: LiveLayout;
  visibleMatrixCams: string[];
  groupCams: string[];
  selectedCamera: string;
  spotlightHeroCam?: string;
  spotlightPromotedCams?: string[];
  hoveredDockCam: string | null;
  useMainStream: boolean;
  timeShiftTs: number | null;
  maxDockLiveDecoders?: number;
  maxGridLiveDecoders?: number;
}

function buildCanonicalCameraList(primaryList: string[], heroCam: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const cam of primaryList) {
    if (cam && !seen.has(cam)) {
      seen.add(cam);
      out.push(cam);
    }
  }
  if (heroCam && !seen.has(heroCam)) {
    out.unshift(heroCam);
  }
  return out;
}

/**
 * Pure stage descriptor calculator mapping any live layout state (`2x2`, `focus`, `single`, `spotlight`)
 * into a flat array of `LiveStageTileDescriptor` items with stable camera keys so React never
 * unmounts `<LiveMsePlayer>` when a camera moves between Grid, Hero Stage, and Dock.
 */
export function computeLiveStageDescriptors(
  params: ComputeLiveStageDescriptorsParams
): LiveStageTileDescriptor[] {
  const {
    liveLayout,
    visibleMatrixCams,
    groupCams,
    selectedCamera,
    spotlightHeroCam,
    spotlightPromotedCams = [],
    hoveredDockCam,
    useMainStream,
    timeShiftTs,
    maxDockLiveDecoders = DEFAULT_MAX_DOCK_LIVE_DECODERS,
    maxGridLiveDecoders = DEFAULT_MAX_GRID_LIVE_DECODERS,
  } = params;

  const safeMaxDock =
    typeof maxDockLiveDecoders === "number" && Number.isFinite(maxDockLiveDecoders)
      ? Math.max(0, Math.floor(maxDockLiveDecoders))
      : DEFAULT_MAX_DOCK_LIVE_DECODERS;
  const safeMaxGrid =
    typeof maxGridLiveDecoders === "number" && Number.isFinite(maxGridLiveDecoders)
      ? Math.max(0, Math.floor(maxGridLiveDecoders))
      : DEFAULT_MAX_GRID_LIVE_DECODERS;

  if (liveLayout === "2x2") {
    const canonicalMatrixCams = buildCanonicalCameraList(visibleMatrixCams, "");
    return canonicalMatrixCams.map((cam, idx) => {
      const isTimelineSelected = cam === selectedCamera;
      const isTimeShifted = isTimelineSelected && timeShiftTs !== null;
      const isLiveGridTile =
        idx < safeMaxGrid ||
        isTimelineSelected ||
        hoveredDockCam === cam ||
        !/_sim_\d+$/.test(cam);
      return {
        camera: cam,
        role: "grid",
        renderMode: isTimeShifted
          ? "vod"
          : isLiveGridTile
          ? "mse"
          : "snapshot",
        useMainStream,
        dockIndex: -1,
        dockTotal: 0,
        isTimelineSelected,
        isSpotlightHeld: false,
      };
    });
  }

  if (liveLayout === "single") {
    const heroCam = selectedCamera || groupCams[0] || visibleMatrixCams[0] || "";
    if (!heroCam) return [];
    return [
      {
        camera: heroCam,
        role: "hero",
        renderMode: timeShiftTs !== null ? "vod" : "mse",
        useMainStream: true,
        dockIndex: -1,
        dockTotal: 0,
        isTimelineSelected: true,
        isSpotlightHeld: false,
      },
    ];
  }

  if (liveLayout === "spotlight") {
    const heroCam =
      spotlightHeroCam || selectedCamera || groupCams[0] || visibleMatrixCams[0] || "";
    if (!heroCam) return [];
    const orderedCams = buildCanonicalCameraList(groupCams, heroCam);
    const dockCams = orderedCams.filter((c) => c !== heroCam);
    const dockTotal = dockCams.length;
    const heldSet = new Set(spotlightPromotedCams);

    return orderedCams.map((cam) => {
      if (cam === heroCam) {
        return {
          camera: cam,
          role: "hero",
          renderMode: timeShiftTs !== null ? "vod" : "mse",
          useMainStream: true,
          dockIndex: -1,
          dockTotal,
          isTimelineSelected: true,
          isSpotlightHeld: heldSet.has(cam),
        };
      }
      const dockIndex = dockCams.indexOf(cam);
      const isLiveDock =
        dockIndex < safeMaxDock || hoveredDockCam === cam;
      return {
        camera: cam,
        role: "dock",
        renderMode: isLiveDock ? "mse" : "snapshot",
        useMainStream: false,
        dockIndex,
        dockTotal,
        isTimelineSelected: false,
        isSpotlightHeld: heldSet.has(cam),
      };
    });
  }

  // Default: "focus" layout
  const heroCam = selectedCamera || groupCams[0] || visibleMatrixCams[0] || "";
  if (!heroCam) return [];
  const orderedCams = buildCanonicalCameraList(groupCams, heroCam);
  const dockCams = orderedCams.filter((c) => c !== heroCam);
  const dockTotal = dockCams.length;

  return orderedCams.map((cam) => {
    if (cam === heroCam) {
      return {
        camera: cam,
        role: "hero",
        renderMode: timeShiftTs !== null ? "vod" : "mse",
        useMainStream: true,
        dockIndex: -1,
        dockTotal,
        isTimelineSelected: true,
        isSpotlightHeld: false,
      };
    }
    const dockIndex = dockCams.indexOf(cam);
    const isLiveDock =
      dockIndex < safeMaxDock || hoveredDockCam === cam;
    return {
      camera: cam,
      role: "dock",
      renderMode: isLiveDock ? "mse" : "snapshot",
      useMainStream: false,
      dockIndex,
      dockTotal,
      isTimelineSelected: false,
      isSpotlightHeld: false,
    };
  });
}

export interface ResolveTileClickActionParams {
  liveLayout: LiveLayout;
  role: StageTileRole;
  camera: string;
  selectedCamera: string;
  isDoubleClick?: boolean;
}

export interface TileClickActionResult {
  nextLayout: LiveLayout;
  nextSelectedCamera: string;
  promotedToFocus: boolean;
  shouldPrewarmMainStream: boolean;
}

/**
 * Resolves ergonomic single-click and double-click tile actions across all live layouts:
 * - In `2x2` grid: single-clicking the already-focused camera or double-clicking any camera
 *   promotes directly to `focus` stage; single-clicking an unfocused camera selects its timeline
 *   and proactively pre-warms its main stream.
 * - In `focus` / `spotlight` dock: single-clicking promotes the dock camera to hero in-place.
 */
export function resolveTileClickAction(
  params: ResolveTileClickActionParams
): TileClickActionResult {
  const { liveLayout, role, camera, selectedCamera, isDoubleClick = false } = params;

  if (liveLayout === "2x2") {
    if (isDoubleClick || camera === selectedCamera) {
      return {
        nextLayout: "focus",
        nextSelectedCamera: camera,
        promotedToFocus: true,
        shouldPrewarmMainStream: true,
      };
    }
    return {
      nextLayout: "2x2",
      nextSelectedCamera: camera,
      promotedToFocus: false,
      shouldPrewarmMainStream: true,
    };
  }

  if (role === "dock") {
    return {
      nextLayout: liveLayout === "spotlight" ? "spotlight" : "focus",
      nextSelectedCamera: camera,
      promotedToFocus: true,
      shouldPrewarmMainStream: true,
    };
  }

  if (liveLayout === "spotlight" && role === "hero" && isDoubleClick) {
    return {
      nextLayout: "focus",
      nextSelectedCamera: camera,
      promotedToFocus: true,
      shouldPrewarmMainStream: true,
    };
  }

  return {
    nextLayout: liveLayout,
    nextSelectedCamera: camera,
    promotedToFocus: false,
    shouldPrewarmMainStream: false,
  };
}

export function resolvePersistentStageContainerStyle(
  liveLayout: LiveLayout,
  descriptors: LiveStageTileDescriptor[],
  matrixDensity: MatrixDensity,
  adaptiveGridStyle?: React.CSSProperties
): React.CSSProperties | undefined {
  if (liveLayout === "2x2") {
    return adaptiveGridStyle;
  }
  if (liveLayout === "single") {
    return {
      gridTemplateColumns: "minmax(0, 1fr)",
      gridTemplateRows: "minmax(0, 1fr)",
    };
  }
  const dockCount = descriptors.filter((d) => d.role === "dock").length;
  if (dockCount === 0) {
    return {
      gridTemplateColumns: "minmax(0, 1fr)",
      gridTemplateRows: "minmax(0, 1fr)",
    };
  }
  const visibleRows = Math.min(
    DEFAULT_MAX_DOCK_LIVE_DECODERS,
    Math.max(1, dockCount)
  );
  return {
    gridTemplateColumns: "minmax(0, 1fr) 310px",
    gridTemplateRows: `repeat(${visibleRows}, minmax(0, 1fr))`,
    overflow: "hidden",
  };
}

export function resolvePersistentTileStyle(
  liveLayout: LiveLayout,
  descriptor: LiveStageTileDescriptor
): React.CSSProperties | undefined {
  if (liveLayout === "2x2" || liveLayout === "single") {
    return undefined;
  }
  const safeDockTotal =
    typeof descriptor.dockTotal === "number" &&
    Number.isFinite(descriptor.dockTotal)
      ? Math.max(0, Math.floor(descriptor.dockTotal))
      : 0;
  const visibleRows = Math.min(
    DEFAULT_MAX_DOCK_LIVE_DECODERS,
    Math.max(1, safeDockTotal)
  );
  if (descriptor.role === "hero") {
    return {
      gridColumn: 1,
      gridRow: `1 / span ${visibleRows}`,
      ...(descriptor.isSpotlightHeld
        ? {
            outline: "1px solid var(--signal-alert)",
            outlineOffset: -1,
          }
        : undefined),
    };
  }
  if (descriptor.role === "dock") {
    if (
      typeof descriptor.dockIndex !== "number" ||
      !Number.isFinite(descriptor.dockIndex) ||
      descriptor.dockIndex < 0 ||
      descriptor.dockIndex >= DEFAULT_MAX_DOCK_LIVE_DECODERS
    ) {
      return {
        display: "none",
      };
    }
    return {
      gridColumn: 2,
      gridRow: Math.floor(descriptor.dockIndex) + 1,
      minHeight: 0,
      opacity: liveLayout === "spotlight" ? 0.84 : 1,
      cursor: "pointer",
    };
  }
  return undefined;
}

export function triggerMainStreamPrewarmIfNeeded(
  realCamera: string,
  shouldPrewarm: boolean
): void {
  if (shouldPrewarm && realCamera) {
    prewarmStream(realCamera);
  }
}

export function shouldRenderSpotlightTimeline(
  liveLayout: LiveLayout,
  spotlightTimelineMode: SpotlightTimelineMode,
  timeShiftTs: number | null
): boolean {
  if (liveLayout !== "spotlight") return true;
  if (spotlightTimelineMode === "reactive") return true;
  return timeShiftTs !== null;
}

export function shouldRenderLiveAlertRibbon(
  liveLayout: LiveLayout,
  showAlertRibbon: boolean,
  unreviewedCount: number
): boolean {
  if (liveLayout === "spotlight" || liveLayout === "map") return false;
  return Boolean(showAlertRibbon && unreviewedCount > 0);
}

export interface ResolveEffectiveSpotlightHeroCamParams {
  promotedCams: string[];
  trailCams: Array<{ cam: string; ts: number }>;
  selectedCamera: string;
  fallbackCam?: string;
  timeShiftTs: number | null;
  lockedHeroCam: string | null;
}

export function resolveEffectiveSpotlightHeroCam(
  params: ResolveEffectiveSpotlightHeroCamParams
): string {
  const {
    promotedCams,
    trailCams,
    selectedCamera,
    fallbackCam = "",
    timeShiftTs,
    lockedHeroCam,
  } = params;
  if (timeShiftTs !== null && lockedHeroCam && lockedHeroCam.trim().length > 0) {
    return lockedHeroCam;
  }
  const latestTrailCam =
    trailCams.length > 0 ? trailCams[trailCams.length - 1].cam : "";
  return promotedCams[0] || latestTrailCam || selectedCamera || fallbackCam || "";
}

/* ------------------------------------------------------------------ */
/* Live Matrix toolbar & event strip helpers                          */
/* ------------------------------------------------------------------ */

const MATRIX_DENSITY_STEPS: Array<{ density: MatrixDensity; pageSize: number }> = [
  { density: "2x2", pageSize: 4 },
  { density: "3x3", pageSize: 9 },
  { density: "4x4", pageSize: 16 },
];

export interface MatrixDensityOption {
  density: MatrixDensity;
  /** A denser grid only helps when there are more cameras than the previous step holds. */
  enabled: boolean;
  /** Camera count the previous step holds (shown in the disabled tooltip). */
  needsMoreThan: number;
  active: boolean;
}

/**
 * Densities the operator can actually tell apart. With 4 cameras, 3×3 and 4×4
 * render the same adaptive 2×2 grid, so they are disabled and the effective
 * density (2×2) is the one shown as active.
 */
export function resolveMatrixDensityOptions(
  cameraCount: number,
  selected: MatrixDensity
): { options: MatrixDensityOption[]; effective: MatrixDensity } {
  const count = Number.isFinite(cameraCount) && cameraCount > 0 ? Math.floor(cameraCount) : 0;
  const options = MATRIX_DENSITY_STEPS.map((step, idx) => {
    const needsMoreThan = idx === 0 ? 0 : MATRIX_DENSITY_STEPS[idx - 1].pageSize;
    return { density: step.density, enabled: idx === 0 || count > needsMoreThan, needsMoreThan };
  });
  // A disabled selection falls back to the densest step that still makes a difference.
  const selectedIdx = Math.max(0, options.findIndex((o) => o.density === selected));
  let effective: MatrixDensity = "2x2";
  for (let i = 0; i <= selectedIdx; i++) {
    if (options[i].enabled) effective = options[i].density;
  }
  return {
    options: options.map((o) => ({ ...o, active: o.density === effective })),
    effective,
  };
}

/** "55s ago" / "4m ago" / "2h ago" (or "há 4 min" in Portuguese). */
export function formatRelativeAgo(nowSec: number, tsSec: number, isPt = false): string {
  if (!Number.isFinite(nowSec) || !Number.isFinite(tsSec)) return "";
  const diff = Math.max(0, Math.floor(nowSec - tsSec));
  if (diff < 60) return isPt ? `há ${diff}s` : `${diff}s ago`;
  const min = Math.floor(diff / 60);
  if (min < 60) return isPt ? `há ${min} min` : `${min}m ago`;
  const hours = Math.floor(min / 60);
  if (hours < 24) return isPt ? `há ${hours} h` : `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return isPt ? `há ${days} d` : `${days}d ago`;
}

/** Most recent first, capped, ignoring entries without a finite start time. */
export function selectRecentEvents<T extends { start_time: number }>(
  items: T[],
  limit = 10
): T[] {
  if (!Array.isArray(items) || limit <= 0) return [];
  return items
    .filter((it) => it && Number.isFinite(it.start_time))
    .slice()
    .sort((a, b) => b.start_time - a.start_time)
    .slice(0, Math.floor(limit));
}

/** Columns × rows the matrix uses for `visibleCount` tiles (mirrors the adaptive grid). */
export function resolveMatrixGridShape(visibleCount: number): { cols: number; rows: number } {
  const n = Number.isFinite(visibleCount) ? Math.max(1, Math.floor(visibleCount)) : 1;
  if (n === 1) return { cols: 1, rows: 1 };
  if (n === 2) return { cols: 2, rows: 1 };
  if (n <= 4) return { cols: 2, rows: 2 };
  if (n <= 6) return { cols: 3, rows: 2 };
  if (n <= 9) return { cols: 3, rows: 3 };
  if (n <= 12) return { cols: 4, rows: 3 };
  return { cols: 4, rows: 4 };
}

/**
 * Largest tile of the given aspect ratio that fits `cols × rows` tiles (with
 * `gap` px between them) inside the stage. Returns whole pixels.
 */
export function computeAspectFitTiles(
  stageWidth: number,
  stageHeight: number,
  cols: number,
  rows: number,
  gap = 2,
  aspect = 16 / 9
): { tileWidth: number; tileHeight: number } | null {
  if (
    ![stageWidth, stageHeight, cols, rows, gap, aspect].every((v) => Number.isFinite(v)) ||
    stageWidth <= 0 ||
    stageHeight <= 0 ||
    cols < 1 ||
    rows < 1 ||
    aspect <= 0
  ) {
    return null;
  }
  const byWidth = (stageWidth - gap * (cols - 1)) / cols;
  const byHeight = ((stageHeight - gap * (rows - 1)) / rows) * aspect;
  const tileWidth = Math.floor(Math.min(byWidth, byHeight));
  const tileHeight = Math.floor(tileWidth / aspect);
  if (tileWidth < 16 || tileHeight < 9) return null;
  return { tileWidth, tileHeight };
}

/**
 * Matrix grid style with 16:9 tiles centred on the stage, so letterboxing
 * collects at the outer edges instead of between the videos. Returns
 * undefined until the stage has been measured.
 */
export function resolveAspectMatrixGridStyle(
  stageSize: { width: number; height: number } | null,
  visibleCount: number,
  gap = 2
): React.CSSProperties | undefined {
  if (!stageSize) return undefined;
  const { cols, rows } = resolveMatrixGridShape(visibleCount);
  const fit = computeAspectFitTiles(stageSize.width, stageSize.height, cols, rows, gap);
  if (!fit) return undefined;
  return {
    gridTemplateColumns: `repeat(${cols}, ${fit.tileWidth}px)`,
    gridTemplateRows: `repeat(${rows}, ${fit.tileHeight}px)`,
    gap,
    justifyContent: "center",
    alignContent: "center",
    background: "var(--video-stage-bg)",
  };
}
