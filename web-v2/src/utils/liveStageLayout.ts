import type React from "react";
import { LiveLayoutMode, MatrixDensity } from "./format";
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

  if (liveLayout === "2x2") {
    return visibleMatrixCams.map((cam, idx) => {
      const isTimelineSelected = cam === selectedCamera;
      const isTimeShifted = isTimelineSelected && timeShiftTs !== null;
      const isLiveGridTile =
        idx < maxGridLiveDecoders ||
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
          renderMode: "mse",
          useMainStream: true,
          dockIndex: -1,
          dockTotal,
          isTimelineSelected: cam === selectedCamera,
          isSpotlightHeld: heldSet.has(cam),
        };
      }
      const dockIndex = dockCams.indexOf(cam);
      const isLiveDock =
        dockIndex < maxDockLiveDecoders || hoveredDockCam === cam;
      return {
        camera: cam,
        role: "dock",
        renderMode: isLiveDock ? "mse" : "snapshot",
        useMainStream: false,
        dockIndex,
        dockTotal,
        isTimelineSelected: cam === selectedCamera,
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
      dockIndex < maxDockLiveDecoders || hoveredDockCam === cam;
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
  const visibleRows = Math.min(
    DEFAULT_MAX_DOCK_LIVE_DECODERS,
    Math.max(1, descriptor.dockTotal)
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
    if (descriptor.dockIndex >= DEFAULT_MAX_DOCK_LIVE_DECODERS) {
      return {
        display: "none",
      };
    }
    return {
      gridColumn: 2,
      gridRow: descriptor.dockIndex + 1,
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
