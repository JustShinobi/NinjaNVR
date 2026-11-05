import React, { useEffect, useMemo, useRef, useState } from "react";
import { Camera, Check, ChevronLeft, ChevronRight, Eye, Play, RefreshCw } from "lucide-react";
import { useI18n } from "../i18n";
import {
  expandSimulatedFleet,
  resolveRealCameraSource,
  SIMULATED_FLEET_GROUPS,
  SimulatedFleetCount,
  useNvrStore,
} from "../store/useNvrStore";

export {
  expandSimulatedFleet,
  resolveRealCameraSource,
  SIMULATED_FLEET_GROUPS,
  type SimulatedFleetCount,
};
import {
  CameraConfig,
  CameraGroupConfig,
  CameraLiveActivity,
  CameraMapPlacement,
  CameraPtzInfoResponse,
  CameraRuntimeStats,
  FloorplanRoom,
  FloorplanWall,
  RecordingSegment,
  ReviewItem,
  StateRoiConfig,
} from "../types/nvr";
import {
  HudPosition,
  MatrixDensity,
  SpotlightFilter,
  formatTs,
  resolveEffectiveTheme,
} from "../utils/format";
import { resolveCanvasThemePalette } from "../utils/themeTokens";
import { LiveMsePlayer, PushToTalkButton, VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";
import { TacticalCameraPicker } from "../components/TacticalCameraPicker";
import {
  computeLiveStageDescriptors,
  resolveEffectiveSpotlightHeroCam,
  resolvePersistentStageContainerStyle,
  resolvePersistentTileStyle,
  resolveTileClickAction,
  shouldRenderLiveAlertRibbon,
  shouldRenderSpotlightTimeline,
  triggerMainStreamPrewarmIfNeeded,
} from "../utils/liveStageLayout";

export const MAX_DOCK_LIVE_DECODERS = 6;

export interface RoiStateBadgeItem {
  roiName: string;
  state: string;
  label: string;
  isAnomaly: boolean;
  anomalyState?: string | null;
  anomalyTimeoutSec: number;
  color: string;
}

export function formatRoiStateBadge(
  roiName: string,
  state: string,
  roiCfg?: StateRoiConfig
): RoiStateBadgeItem {
  const cleanRoi = roiName.trim();
  const cleanState = state.trim() || "closed";
  const anomalyState = roiCfg?.anomaly_state ?? null;
  const isAnomaly = Boolean(
    anomalyState &&
      anomalyState.trim().toLowerCase() === cleanState.toLowerCase()
  );
  const anomalyTimeoutSec = roiCfg?.anomaly_timeout_sec ?? 300;
  return {
    roiName: cleanRoi,
    state: cleanState,
    label: `${cleanRoi.toUpperCase()}: ${cleanState.toUpperCase()}`,
    isAnomaly,
    anomalyState,
    anomalyTimeoutSec,
    color: isAnomaly ? "var(--signal-alert-text)" : "var(--text-primary)",
  };
}

export function resolveCameraRoiStateBadges(
  camCfg?: CameraConfig,
  act?: CameraLiveActivity,
  camStat?: CameraRuntimeStats
): RoiStateBadgeItem[] {
  const configuredRois = camCfg?.state_rois || {};
  const liveStates: Record<string, string> = {
    ...(camStat?.states || {}),
    ...(act?.states || {}),
  };
  const roiKeys = Array.from(
    new Set([...Object.keys(configuredRois), ...Object.keys(liveStates)])
  );
  if (roiKeys.length === 0) return [];
  return roiKeys.map((roiName) => {
    const roiCfg = configuredRois[roiName];
    const fallbackState =
      roiCfg?.classes && roiCfg.classes.length > 0 ? roiCfg.classes[0] : "closed";
    const state = liveStates[roiName] ?? fallbackState;
    return formatRoiStateBadge(roiName, state, roiCfg);
  });
}

export function getMatrixPageSize(density: MatrixDensity): number {
  if (density === "3x3") return 9;
  if (density === "4x4") return 16;
  return 4;
}

export function resolveAdaptiveMatrixGridStyle(
  visibleCount: number,
  density: MatrixDensity
): React.CSSProperties | undefined {
  const maxSlots = getMatrixPageSize(density);
  if (visibleCount <= 0 || visibleCount >= maxSlots) {
    return undefined;
  }
  if (visibleCount === 1) {
    return {
      gridTemplateColumns: "minmax(0, 1fr)",
      gridTemplateRows: "minmax(0, 1fr)",
    };
  }
  if (visibleCount === 2) {
    return {
      gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
      gridTemplateRows: "minmax(0, 1fr)",
    };
  }
  if (visibleCount <= 4) {
    return {
      gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
      gridTemplateRows: "repeat(2, minmax(0, 1fr))",
    };
  }
  if (visibleCount <= 6) {
    return {
      gridTemplateColumns: "repeat(3, minmax(0, 1fr))",
      gridTemplateRows: "repeat(2, minmax(0, 1fr))",
    };
  }
  if (visibleCount <= 9) {
    return {
      gridTemplateColumns: "repeat(3, minmax(0, 1fr))",
      gridTemplateRows: "repeat(3, minmax(0, 1fr))",
    };
  }
  if (visibleCount <= 12) {
    return {
      gridTemplateColumns: "repeat(4, minmax(0, 1fr))",
      gridTemplateRows: "repeat(3, minmax(0, 1fr))",
    };
  }
  return undefined;
}

export function resolveOrderedDashboardCameras(
  cameraNames: string[],
  camerasConfig?: Record<string, CameraConfig>,
  activeGroup: string = "all",
  cameraGroups?: Record<string, CameraGroupConfig>
): string[] {
  const indexMap = new Map<string, number>();
  cameraNames.forEach((cam, idx) => indexMap.set(cam, idx));

  const groupSet =
    activeGroup &&
    activeGroup !== "all" &&
    cameraGroups &&
    cameraGroups[activeGroup]?.cameras
      ? new Set(cameraGroups[activeGroup].cameras)
      : null;

  const filtered = cameraNames.filter((cam) => {
    const camCfg = camerasConfig?.[cam];
    if (camCfg?.ui?.dashboard === false) {
      return false;
    }
    if (groupSet && !groupSet.has(cam)) {
      return false;
    }
    return true;
  });

  return [...filtered].sort((a, b) => {
    const orderA = camerasConfig?.[a]?.ui?.order ?? 0;
    const orderB = camerasConfig?.[b]?.ui?.order ?? 0;
    if (orderA !== orderB) {
      return orderA - orderB;
    }
    return (indexMap.get(a) ?? 0) - (indexMap.get(b) ?? 0);
  });
}

export function sliceMatrixPage(
  cameras: string[],
  page: number,
  density: MatrixDensity
): {
  visibleMatrixCams: string[];
  pageSize: number;
  totalPages: number;
  safePage: number;
} {
  const pageSize = getMatrixPageSize(density);
  const totalPages = Math.max(1, Math.ceil(cameras.length / pageSize));
  const safePage = Math.max(0, Math.min(page, totalPages - 1));
  const visibleMatrixCams = cameras.slice(
    safePage * pageSize,
    (safePage + 1) * pageSize
  );
  return { visibleMatrixCams, pageSize, totalPages, safePage };
}

export function nextMatrixTourPage(
  currentPage: number,
  totalPages: number
): number {
  if (totalPages <= 1) return 0;
  return (Math.max(0, currentPage) + 1) % totalPages;
}

export function resolveDockTileMode(
  index: number,
  cam: string,
  hoveredCam: string | null,
  maxLive: number = MAX_DOCK_LIVE_DECODERS,
  allDockCams?: string[]
): "mse" | "snapshot" {
  if (maxLive <= 0) return "snapshot";
  if (cam === hoveredCam) return "mse";
  const hoveredInBeyond =
    hoveredCam !== null &&
    (!allDockCams || allDockCams.indexOf(hoveredCam) >= maxLive);
  const effectiveCap = hoveredInBeyond ? Math.max(0, maxLive - 1) : maxLive;
  return index < effectiveCap ? "mse" : "snapshot";
}

function finiteOr(val: number, fallback: number): number {
  return Number.isFinite(val) ? val : fallback;
}

export function computeDefaultMapPlacement(
  index: number,
  totalCameras: number
): CameraMapPlacement {
  const safeTotal = Number.isFinite(totalCameras)
    ? Math.trunc(totalCameras)
    : 1;
  const safeIndex = Number.isFinite(index) ? Math.trunc(index) : 0;
  const n = Math.max(1, safeTotal);
  const cols = Math.max(1, Math.ceil(Math.sqrt(n)));
  const rows = Math.max(1, Math.ceil(n / cols));
  const safeIdx = Math.max(0, Math.min(safeIndex, n - 1));
  const col = safeIdx % cols;
  const row = Math.floor(safeIdx / cols);

  const rawX = cols <= 1 ? 0.5 : 0.1 + (col / (cols - 1)) * 0.8;
  const rawY = rows <= 1 ? 0.5 : 0.12 + (row / (rows - 1)) * 0.76;

  const x = Number(Math.max(0.1, Math.min(0.9, rawX)).toFixed(3));
  const y = Number(Math.max(0.12, Math.min(0.88, rawY)).toFixed(3));
  const angle_deg = (safeIdx * 90 + 45) % 360;
  const range =
    n > 16
      ? Number(Math.max(0.08, 0.25 / Math.sqrt(n / 4)).toFixed(3))
      : 0.25;

  return {
    x,
    y,
    angle_deg,
    fov_deg: 90,
    range,
  };
}

export type FloorplanStudioTool = "select" | "draw_room" | "draw_wall";

export const DEFAULT_FLOORPLAN_WIDTH_M = 20;
export const DEFAULT_FLOORPLAN_HEIGHT_M = 12;

export const DEFAULT_ROOM_LABELS = [
  "SALA",
  "ESCRITÓRIO",
  "QUARTO",
  "COZINHA",
  "GARAGEM",
  "CORREDOR",
  "VARANDA",
  "HALL",
];

export const ROOM_COLOR_PALETTE = [
  "#0ea5e9",
  "#10b981",
  "#f59e0b",
  "#8b5cf6",
  "#ec4899",
  "#64748b",
];

export function computeCameraAimFromTip(
  camX: number,
  camY: number,
  tipX: number,
  tipY: number
): { angle_deg: number; range: number } {
  const sx = finiteOr(camX, 0.5);
  const sy = finiteOr(camY, 0.5);
  const tx = finiteOr(tipX, sx + 0.25);
  const ty = finiteOr(tipY, sy);
  const dx = tx - sx;
  const dy = ty - sy;
  const rawDeg =
    dx === 0 && dy === 0 ? 0 : (Math.atan2(dy, dx) * 180) / Math.PI;
  const normalizedDeg = ((rawDeg % 360) + 360) % 360;
  const angle_deg = Number(normalizedDeg.toFixed(1)) % 360;
  const dist = Math.hypot(dx, dy);
  const range = Number(Math.max(0.05, Math.min(0.5, dist)).toFixed(3));
  return { angle_deg, range };
}

export function computeFovConeTipSvgPoint(
  cx: number,
  cy: number,
  angleDeg: number,
  radius: number
): { x: number; y: number } {
  const safeCx = finiteOr(cx, 500);
  const safeCy = finiteOr(cy, 300);
  const safeAngle = finiteOr(angleDeg, 0);
  const safeRadius = Math.max(0, finiteOr(radius, 100));
  const rad = (safeAngle * Math.PI) / 180;
  return {
    x: Number((safeCx + safeRadius * Math.cos(rad)).toFixed(1)),
    y: Number((safeCy + safeRadius * Math.sin(rad)).toFixed(1)),
  };
}

/**
 * Accurately maps a browser client pointer (clientX, clientY) into normalized [0, 1]
 * floorplan coordinates inside the SVG viewBox (default 1000x600), accounting for
 * `preserveAspectRatio="xMidYMid meet"` letterboxing/pillarboxing when the SVG element
 * aspect ratio differs from 1000:600.
 */
export function mapClientPointToFloorplan(
  clientX: number,
  clientY: number,
  rect: { left: number; top: number; width: number; height: number },
  viewBoxWidth: number = 1000,
  viewBoxHeight: number = 600
): { x: number; y: number } {
  if (
    !rect ||
    !Number.isFinite(clientX) ||
    !Number.isFinite(clientY) ||
    !Number.isFinite(rect.left) ||
    !Number.isFinite(rect.top) ||
    !Number.isFinite(rect.width) ||
    !Number.isFinite(rect.height) ||
    !Number.isFinite(viewBoxWidth) ||
    !Number.isFinite(viewBoxHeight) ||
    rect.width <= 0 ||
    rect.height <= 0 ||
    viewBoxWidth <= 0 ||
    viewBoxHeight <= 0
  ) {
    return { x: 0.5, y: 0.5 };
  }
  const scale = Math.min(
    rect.width / viewBoxWidth,
    rect.height / viewBoxHeight
  );
  const drawnW = viewBoxWidth * scale;
  const drawnH = viewBoxHeight * scale;
  if (
    drawnW <= 0 ||
    drawnH <= 0 ||
    !Number.isFinite(drawnW) ||
    !Number.isFinite(drawnH)
  ) {
    return { x: 0.5, y: 0.5 };
  }
  const offsetX = (rect.width - drawnW) / 2;
  const offsetY = (rect.height - drawnH) / 2;
  const normX = (clientX - rect.left - offsetX) / drawnW;
  const normY = (clientY - rect.top - offsetY) / drawnH;
  return {
    x: Number(Math.max(0, Math.min(1, normX)).toFixed(4)),
    y: Number(Math.max(0, Math.min(1, normY)).toFixed(4)),
  };
}

export function snapFloorplanCoord(val: number, step: number = 0.02): number {
  const safeVal = finiteOr(val, 0);
  const clamped = Math.max(0, Math.min(1, safeVal));
  if (!Number.isFinite(step) || step <= 0) return Number(clamped.toFixed(3));
  const snapped = Math.round(clamped / step) * step;
  return Number(Math.max(0, Math.min(1, snapped)).toFixed(3));
}

export function normalizeFloorplanRoomRect(
  x1: number,
  y1: number,
  x2: number,
  y2: number,
  minSize: number = 0.02
): { x: number; y: number; w: number; h: number } {
  const safeMin = Math.max(0.005, Math.min(1, finiteOr(minSize, 0.02)));
  const cx1 = Math.max(0, Math.min(1, finiteOr(x1, 0)));
  const cy1 = Math.max(0, Math.min(1, finiteOr(y1, 0)));
  const cx2 = Math.max(0, Math.min(1, finiteOr(x2, cx1 + safeMin)));
  const cy2 = Math.max(0, Math.min(1, finiteOr(y2, cy1 + safeMin)));
  const rawX = Math.min(cx1, cx2);
  const rawY = Math.min(cy1, cy2);
  const rawW = Math.max(safeMin, Math.abs(cx2 - cx1));
  const rawH = Math.max(safeMin, Math.abs(cy2 - cy1));
  const w = Number(Math.min(1, rawW).toFixed(3));
  const h = Number(Math.min(1, rawH).toFixed(3));
  const x = Number(Math.max(0, Math.min(1 - w, rawX)).toFixed(3));
  const y = Number(Math.max(0, Math.min(1 - h, rawY)).toFixed(3));
  return { x, y, w, h };
}

export function computeRoomMetrics(
  room: Pick<FloorplanRoom, "x" | "y" | "w" | "h">,
  floorplanWidthM: number = DEFAULT_FLOORPLAN_WIDTH_M,
  floorplanHeightM: number = DEFAULT_FLOORPLAN_HEIGHT_M
): {
  widthM: number;
  heightM: number;
  areaSqM: number;
  xM: number;
  yM: number;
} {
  const safeW = Math.max(1, finiteOr(floorplanWidthM, DEFAULT_FLOORPLAN_WIDTH_M));
  const safeH = Math.max(1, finiteOr(floorplanHeightM, DEFAULT_FLOORPLAN_HEIGHT_M));
  const rw = Math.max(0, Math.min(1, finiteOr(room?.w, 0.2)));
  const rh = Math.max(0, Math.min(1, finiteOr(room?.h, 0.2)));
  const rx = Math.max(0, Math.min(1, finiteOr(room?.x, 0)));
  const ry = Math.max(0, Math.min(1, finiteOr(room?.y, 0)));
  const widthM = Number((rw * safeW).toFixed(2));
  const heightM = Number((rh * safeH).toFixed(2));
  const areaSqM = Number((widthM * heightM).toFixed(2));
  const xM = Number((rx * safeW).toFixed(2));
  const yM = Number((ry * safeH).toFixed(2));
  return { widthM, heightM, areaSqM, xM, yM };
}

export function resizeRoomByMeters(
  room: FloorplanRoom,
  targetWidthM: number,
  targetHeightM: number,
  floorplanWidthM: number = DEFAULT_FLOORPLAN_WIDTH_M,
  floorplanHeightM: number = DEFAULT_FLOORPLAN_HEIGHT_M
): FloorplanRoom {
  const safeW = Math.max(1, finiteOr(floorplanWidthM, DEFAULT_FLOORPLAN_WIDTH_M));
  const safeH = Math.max(1, finiteOr(floorplanHeightM, DEFAULT_FLOORPLAN_HEIGHT_M));
  const safeTargetW = Math.max(0.1, finiteOr(targetWidthM, safeW * 0.2));
  const safeTargetH = Math.max(0.1, finiteOr(targetHeightM, safeH * 0.2));
  const normW = Math.max(0.01, Math.min(1, safeTargetW / safeW));
  const normH = Math.max(0.01, Math.min(1, safeTargetH / safeH));
  const w = Number(normW.toFixed(3));
  const h = Number(normH.toFixed(3));
  const rx = Math.max(0, Math.min(1, finiteOr(room?.x, 0)));
  const ry = Math.max(0, Math.min(1, finiteOr(room?.y, 0)));
  const x = Number(Math.max(0, Math.min(1 - w, rx)).toFixed(3));
  const y = Number(Math.max(0, Math.min(1 - h, ry)).toFixed(3));
  return { ...room, x, y, w, h };
}

export function computeWallMetrics(
  points: [number, number, number, number],
  floorplanWidthM: number = DEFAULT_FLOORPLAN_WIDTH_M,
  floorplanHeightM: number = DEFAULT_FLOORPLAN_HEIGHT_M
): {
  lengthM: number;
  angleDeg: number;
  dxM: number;
  dyM: number;
  midX: number;
  midY: number;
} {
  const x1 = Math.max(0, Math.min(1, finiteOr(points?.[0], 0)));
  const y1 = Math.max(0, Math.min(1, finiteOr(points?.[1], 0)));
  const x2 = Math.max(0, Math.min(1, finiteOr(points?.[2], x1)));
  const y2 = Math.max(0, Math.min(1, finiteOr(points?.[3], y1)));
  const safeW = Math.max(1, finiteOr(floorplanWidthM, DEFAULT_FLOORPLAN_WIDTH_M));
  const safeH = Math.max(1, finiteOr(floorplanHeightM, DEFAULT_FLOORPLAN_HEIGHT_M));
  const dxM = (x2 - x1) * safeW;
  const dyM = (y2 - y1) * safeH;
  const lengthM = Number(Math.hypot(dxM, dyM).toFixed(2));
  const rawDeg =
    dxM === 0 && dyM === 0 ? 0 : (Math.atan2(dyM, dxM) * 180) / Math.PI;
  const angleDeg = Number((((rawDeg % 360) + 360) % 360).toFixed(1)) % 360;
  const midX = Number(((x1 + x2) / 2).toFixed(3));
  const midY = Number(((y1 + y2) / 2).toFixed(3));
  return { lengthM, angleDeg, dxM, dyM, midX, midY };
}

export function resizeWallToLengthMeters(
  points: [number, number, number, number],
  targetLengthM: number,
  floorplanWidthM: number = DEFAULT_FLOORPLAN_WIDTH_M,
  floorplanHeightM: number = DEFAULT_FLOORPLAN_HEIGHT_M
): [number, number, number, number] {
  const x1 = Math.max(0, Math.min(1, finiteOr(points?.[0], 0.2)));
  const y1 = Math.max(0, Math.min(1, finiteOr(points?.[1], 0.2)));
  const x2 = Math.max(0, Math.min(1, finiteOr(points?.[2], 0.4)));
  const y2 = Math.max(0, Math.min(1, finiteOr(points?.[3], 0.2)));
  const safeW = Math.max(1, finiteOr(floorplanWidthM, DEFAULT_FLOORPLAN_WIDTH_M));
  const safeH = Math.max(1, finiteOr(floorplanHeightM, DEFAULT_FLOORPLAN_HEIGHT_M));
  const maxPossibleM = Math.hypot(safeW, safeH);
  const safeTargetM = finiteOr(targetLengthM, 1.0);
  const clampedLen = Math.max(0.1, Math.min(maxPossibleM, safeTargetM));
  const dxM = (x2 - x1) * safeW;
  const dyM = (y2 - y1) * safeH;
  const currentLen = Math.hypot(dxM, dyM);
  const angleRad = currentLen > 1e-4 ? Math.atan2(dyM, dxM) : 0;
  const nextDxNorm = (clampedLen * Math.cos(angleRad)) / safeW;
  const nextDyNorm = (clampedLen * Math.sin(angleRad)) / safeH;
  let nx1 = x1;
  let ny1 = y1;
  let nx2 = x1 + nextDxNorm;
  let ny2 = y1 + nextDyNorm;
  if (nx2 > 1) {
    const over = nx2 - 1;
    nx1 = Math.max(0, nx1 - over);
    nx2 = 1;
  } else if (nx2 < 0) {
    const under = -nx2;
    nx1 = Math.min(1, nx1 + under);
    nx2 = 0;
  }
  if (ny2 > 1) {
    const over = ny2 - 1;
    ny1 = Math.max(0, ny1 - over);
    ny2 = 1;
  } else if (ny2 < 0) {
    const under = -ny2;
    ny1 = Math.min(1, ny1 + under);
    ny2 = 0;
  }
  return [
    Number(Math.max(0, Math.min(1, nx1)).toFixed(3)),
    Number(Math.max(0, Math.min(1, ny1)).toFixed(3)),
    Number(Math.max(0, Math.min(1, nx2)).toFixed(3)),
    Number(Math.max(0, Math.min(1, ny2)).toFixed(3)),
  ];
}

export function rotateWallToAngleDegrees(
  points: [number, number, number, number],
  targetAngleDeg: number,
  floorplanWidthM: number = DEFAULT_FLOORPLAN_WIDTH_M,
  floorplanHeightM: number = DEFAULT_FLOORPLAN_HEIGHT_M
): [number, number, number, number] {
  const x1 = Math.max(0, Math.min(1, finiteOr(points?.[0], 0.2)));
  const y1 = Math.max(0, Math.min(1, finiteOr(points?.[1], 0.2)));
  const x2 = Math.max(0, Math.min(1, finiteOr(points?.[2], 0.4)));
  const y2 = Math.max(0, Math.min(1, finiteOr(points?.[3], 0.2)));
  const safeW = Math.max(1, finiteOr(floorplanWidthM, DEFAULT_FLOORPLAN_WIDTH_M));
  const safeH = Math.max(1, finiteOr(floorplanHeightM, DEFAULT_FLOORPLAN_HEIGHT_M));
  const lenM = Math.max(0.2, Math.hypot((x2 - x1) * safeW, (y2 - y1) * safeH));
  const safeDeg = finiteOr(targetAngleDeg, 0);
  const rad = (safeDeg * Math.PI) / 180;
  let nx1 = x1;
  let ny1 = y1;
  let nx2 = x1 + (lenM * Math.cos(rad)) / safeW;
  let ny2 = y1 + (lenM * Math.sin(rad)) / safeH;
  if (nx2 > 1) {
    nx1 = Math.max(0, nx1 - (nx2 - 1));
    nx2 = 1;
  } else if (nx2 < 0) {
    nx1 = Math.min(1, nx1 - nx2);
    nx2 = 0;
  }
  if (ny2 > 1) {
    ny1 = Math.max(0, ny1 - (ny2 - 1));
    ny2 = 1;
  } else if (ny2 < 0) {
    ny1 = Math.min(1, ny1 - ny2);
    ny2 = 0;
  }
  return [
    Number(Math.max(0, Math.min(1, nx1)).toFixed(3)),
    Number(Math.max(0, Math.min(1, ny1)).toFixed(3)),
    Number(Math.max(0, Math.min(1, nx2)).toFixed(3)),
    Number(Math.max(0, Math.min(1, ny2)).toFixed(3)),
  ];
}

export function snapWallEndpoint(
  startX: number | null,
  startY: number | null,
  rawX: number,
  rawY: number,
  options: {
    snapStep?: number;
    orthoSnap?: boolean;
    vertices?: Array<{ x: number; y: number }>;
    vertexSnapRadius?: number;
    floorplanWidthM?: number;
    floorplanHeightM?: number;
  } = {}
): { x: number; y: number; snappedToVertex: boolean } {
  const {
    snapStep = 0.005,
    orthoSnap = true,
    vertices = [],
    vertexSnapRadius = 0.015,
    floorplanWidthM = DEFAULT_FLOORPLAN_WIDTH_M,
    floorplanHeightM = DEFAULT_FLOORPLAN_HEIGHT_M,
  } = options;

  const safeRawX = Math.max(0, Math.min(1, finiteOr(rawX, 0.5)));
  const safeRawY = Math.max(0, Math.min(1, finiteOr(rawY, 0.5)));
  const safeRadius = Math.max(0, finiteOr(vertexSnapRadius, 0.015));

  // 1. Check magnetic vertex snap to existing wall endpoints / room corners
  let bestVertex: { x: number; y: number } | null = null;
  let bestDist = safeRadius;
  for (const v of vertices) {
    if (!Number.isFinite(v.x) || !Number.isFinite(v.y)) continue;
    const d = Math.hypot(safeRawX - v.x, safeRawY - v.y);
    if (d <= bestDist) {
      bestDist = d;
      bestVertex = v;
    }
  }
  if (bestVertex) {
    return {
      x: Number(Math.max(0, Math.min(1, bestVertex.x)).toFixed(3)),
      y: Number(Math.max(0, Math.min(1, bestVertex.y)).toFixed(3)),
      snappedToVertex: true,
    };
  }

  let nextX = snapFloorplanCoord(safeRawX, snapStep);
  let nextY = snapFloorplanCoord(safeRawY, snapStep);

  // 2. Apply orthogonal / 45-degree angle snap if start point is known
  if (
    orthoSnap &&
    startX !== null &&
    startY !== null &&
    Number.isFinite(startX) &&
    Number.isFinite(startY)
  ) {
    const safeW = Math.max(1, finiteOr(floorplanWidthM, DEFAULT_FLOORPLAN_WIDTH_M));
    const safeH = Math.max(1, finiteOr(floorplanHeightM, DEFAULT_FLOORPLAN_HEIGHT_M));
    const dxM = (nextX - startX) * safeW;
    const dyM = (nextY - startY) * safeH;
    const distM = Math.hypot(dxM, dyM);
    if (distM >= 0.15) {
      const deg = (((Math.atan2(dyM, dxM) * 180) / Math.PI) + 360) % 360;
      const snapAngles = [0, 45, 90, 135, 180, 225, 270, 315, 360];
      for (const targetDeg of snapAngles) {
        if (Math.abs(deg - targetDeg) <= 7) {
          if (targetDeg === 0 || targetDeg === 180 || targetDeg === 360) {
            nextY = Number(startY.toFixed(3));
          } else if (targetDeg === 90 || targetDeg === 270) {
            nextX = Number(startX.toFixed(3));
          } else {
            const rad = (targetDeg * Math.PI) / 180;
            nextX = snapFloorplanCoord(
              startX + (distM * Math.cos(rad)) / safeW,
              snapStep
            );
            nextY = snapFloorplanCoord(
              startY + (distM * Math.sin(rad)) / safeH,
              snapStep
            );
          }
          break;
        }
      }
    }
  }

  return {
    x: Number(Math.max(0, Math.min(1, nextX)).toFixed(3)),
    y: Number(Math.max(0, Math.min(1, nextY)).toFixed(3)),
    snappedToVertex: false,
  };
}

export function extractRoomBoundaryWalls(room: FloorplanRoom): FloorplanWall[] {
  const rx = Math.max(0, Math.min(1, finiteOr(room?.x, 0)));
  const ry = Math.max(0, Math.min(1, finiteOr(room?.y, 0)));
  const rw = Math.max(0.01, Math.min(1, finiteOr(room?.w, 0.2)));
  const rh = Math.max(0.01, Math.min(1, finiteOr(room?.h, 0.2)));
  const x1 = Number(rx.toFixed(3));
  const y1 = Number(ry.toFixed(3));
  const x2 = Number(Math.min(1, rx + rw).toFixed(3));
  const y2 = Number(Math.min(1, ry + rh).toFixed(3));
  const baseLabel = room?.label || "ROOM";
  return [
    { points: [x1, y1, x2, y1], label: `${baseLabel} N`, kind: "wall" },
    { points: [x2, y1, x2, y2], label: `${baseLabel} E`, kind: "wall" },
    { points: [x2, y2, x1, y2], label: `${baseLabel} S`, kind: "wall" },
    { points: [x1, y2, x1, y1], label: `${baseLabel} W`, kind: "wall" },
  ];
}

export function buildFovConeSvgPath(
  cx: number,
  cy: number,
  angleDeg: number,
  fovDeg: number,
  radius: number
): string {
  const safeCx = finiteOr(cx, 500);
  const safeCy = finiteOr(cy, 300);
  const safeAngle = finiteOr(angleDeg, 0);
  const safeFov = finiteOr(fovDeg, 90);
  const safeRadius = Math.max(1, Math.min(1000, finiteOr(radius, 100)));
  const half = Math.max(10, Math.min(89, safeFov / 2));
  const startRad = ((safeAngle - half) * Math.PI) / 180;
  const endRad = ((safeAngle + half) * Math.PI) / 180;
  const x1 = safeCx + safeRadius * Math.cos(startRad);
  const y1 = safeCy + safeRadius * Math.sin(startRad);
  const x2 = safeCx + safeRadius * Math.cos(endRad);
  const y2 = safeCy + safeRadius * Math.sin(endRad);
  const largeArc = half * 2 > 180 ? 1 : 0;
  return `M ${safeCx.toFixed(1)} ${safeCy.toFixed(1)} L ${x1.toFixed(
    1
  )} ${y1.toFixed(1)} A ${safeRadius.toFixed(1)} ${safeRadius.toFixed(
    1
  )} 0 ${largeArc} 1 ${x2.toFixed(1)} ${y2.toFixed(1)} Z`;
}

export function sanitizeFloorplanUrl(
  rawUrl: string | null | undefined
): string {
  if (typeof rawUrl !== "string") return "";
  const trimmed = rawUrl.trim();
  if (!trimmed || /[\u0000-\u001F\u007F]/.test(trimmed)) return "";
  if (trimmed.startsWith("/") && !trimmed.startsWith("//")) {
    return trimmed;
  }
  if (/^https?:\/\//i.test(trimmed)) {
    return trimmed;
  }
  if (/^data:image\/(png|jpeg|jpg|webp|gif);base64,[a-z0-9+/=\s]+$/i.test(trimmed)) {
    return trimmed;
  }
  if (/^data:image\/svg\+xml;base64,([a-z0-9+/=\s]+)$/i.test(trimmed)) {
    const b64 = trimmed.slice(trimmed.indexOf(",") + 1).replace(/\s+/g, "");
    try {
      const decoded = globalThis.atob(b64);
      if (
        /<script|javascript:|vbscript:|onload\s*=|onerror\s*=|<foreignobject/i.test(
          decoded
        )
      ) {
        return "";
      }
      return trimmed;
    } catch {
      return "";
    }
  }
  return "";
}

export function sanitizeMapPlacement(
  placement: Partial<CameraMapPlacement> | null | undefined,
  fallback: CameraMapPlacement = {
    x: 0.5,
    y: 0.5,
    angle_deg: 0,
    fov_deg: 90,
    range: 0.25,
  }
): CameraMapPlacement {
  const x = Number(
    Math.max(0, Math.min(1, finiteOr(placement?.x ?? NaN, fallback.x))).toFixed(
      3
    )
  );
  const y = Number(
    Math.max(0, Math.min(1, finiteOr(placement?.y ?? NaN, fallback.y))).toFixed(
      3
    )
  );
  const rawAngle = finiteOr(placement?.angle_deg ?? NaN, fallback.angle_deg);
  const angle_deg = Number((((rawAngle % 360) + 360) % 360).toFixed(1)) % 360;
  const fov_deg = Number(
    Math.max(
      10,
      Math.min(180, finiteOr(placement?.fov_deg ?? NaN, fallback.fov_deg))
    ).toFixed(1)
  );
  const range = Number(
    Math.max(
      0.05,
      Math.min(0.5, finiteOr(placement?.range ?? NaN, fallback.range))
    ).toFixed(3)
  );
  return { x, y, angle_deg, fov_deg, range };
}

export function buildFloorplanSavePayload(params: {
  existingCameras: Record<string, CameraConfig>;
  mapPlacements: Record<string, CameraMapPlacement>;
  existingUi?: Record<string, unknown>;
  floorplanUrl: string;
  floorplanOpacity: number;
  floorplanWidthM: number;
  floorplanHeightM: number;
  floorplanRooms: FloorplanRoom[];
  floorplanWalls: FloorplanWall[];
}): {
  cameras: Record<string, { map: CameraMapPlacement }>;
  ui: Record<string, unknown>;
} {
  const {
    existingCameras,
    mapPlacements,
    existingUi,
    floorplanUrl,
    floorplanOpacity,
    floorplanWidthM,
    floorplanHeightM,
    floorplanRooms,
    floorplanWalls,
  } = params;

  const camNames = Object.keys(existingCameras || {});
  const camerasPatch: Record<string, { map: CameraMapPlacement }> = {};
  camNames.forEach((cam, idx) => {
    const fallback =
      existingCameras[cam]?.map ||
      computeDefaultMapPlacement(idx, camNames.length);
    const chosen = mapPlacements[cam] || fallback;
    camerasPatch[cam] = {
      map: sanitizeMapPlacement(chosen, fallback),
    };
  });

  const sanitizedRooms: FloorplanRoom[] = (floorplanRooms || []).map(
    (r, idx) => {
      const norm = normalizeFloorplanRoomRect(
        r.x,
        r.y,
        r.x + r.w,
        r.y + r.h,
        0.01
      );
      return {
        ...r,
        id: r.id || `room_${idx + 1}`,
        label: (r.label || "ROOM").trim() || "ROOM",
        x: norm.x,
        y: norm.y,
        w: norm.w,
        h: norm.h,
      };
    }
  );

  const sanitizedWalls: FloorplanWall[] = (floorplanWalls || []).map((w) => {
    const p = w.points || [0.2, 0.2, 0.4, 0.2];
    const x1 = Number(Math.max(0, Math.min(1, finiteOr(p[0], 0.2))).toFixed(3));
    const y1 = Number(Math.max(0, Math.min(1, finiteOr(p[1], 0.2))).toFixed(3));
    const x2 = Number(Math.max(0, Math.min(1, finiteOr(p[2], 0.4))).toFixed(3));
    const y2 = Number(Math.max(0, Math.min(1, finiteOr(p[3], 0.2))).toFixed(3));
    return {
      ...w,
      points: [x1, y1, x2, y2],
    };
  });

  const cleanUrl = sanitizeFloorplanUrl(floorplanUrl);
  const safeOp = Number(
    Math.max(0.1, Math.min(1, finiteOr(floorplanOpacity, 0.45))).toFixed(2)
  );
  const safeW = Number(
    Math.max(
      2,
      Math.min(500, finiteOr(floorplanWidthM, DEFAULT_FLOORPLAN_WIDTH_M))
    ).toFixed(2)
  );
  const safeH = Number(
    Math.max(
      2,
      Math.min(500, finiteOr(floorplanHeightM, DEFAULT_FLOORPLAN_HEIGHT_M))
    ).toFixed(2)
  );

  return {
    cameras: camerasPatch,
    ui: {
      ...(existingUi || {}),
      floorplan: {
        background_data_url: cleanUrl || null,
        opacity: safeOp,
        width_m: safeW,
        height_m: safeH,
        rooms: sanitizedRooms,
        walls: sanitizedWalls,
      },
    },
  };
}

export function resolveSafeAlertThumbSrc(
  thumbPath: string | null | undefined,
  camera: string
): string {
  const encodedCam = encodeURIComponent(camera || "camera");
  const fallback = `/api/${encodedCam}/latest.webp`;
  if (typeof thumbPath !== "string") return fallback;
  const trimmed = thumbPath.trim();
  if (!trimmed || /[\u0000-\u001F\u007F]/.test(trimmed)) return fallback;
  const stripped = trimmed.replace(/^\/media\/[^/]+/, "").trim();
  if (!stripped || !stripped.startsWith("/") || stripped.startsWith("//")) {
    return fallback;
  }
  return stripped;
}

export function sanitizeTimeShiftSeek(
  ts: number | null | undefined,
  nowSec: number = Math.floor(Date.now() / 1000)
): number | null {
  if (ts === null || ts === undefined) return null;
  if (!Number.isFinite(ts) || ts <= 0) return null;
  const safeNow =
    Number.isFinite(nowSec) && nowSec > 60
      ? Math.floor(nowSec)
      : Math.floor(Date.now() / 1000);
  if (ts >= safeNow - 2) {
    return null;
  }
  return Math.floor(ts);
}

export function isCameraPtzEnabled(
  camCfg?: CameraConfig,
  ptzInfo?: CameraPtzInfoResponse | null
): boolean {
  if (ptzInfo?.enabled) return true;
  return Boolean(camCfg?.onvif?.host && camCfg.onvif.host.trim().length > 0);
}

export function resolveCameraPtzPresets(
  camCfg?: CameraConfig,
  ptzInfo?: CameraPtzInfoResponse | null
): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  const pushUnique = (val?: string | null) => {
    const clean = val ? val.trim() : "";
    if (clean && !seen.has(clean)) {
      seen.add(clean);
      out.push(clean);
    }
  };
  for (const p of ptzInfo?.presets || []) {
    pushUnique(p.token || p.name);
  }
  for (const name of ptzInfo?.preset_names || []) {
    pushUnique(name);
  }
  pushUnique(camCfg?.onvif?.return_preset);
  for (const z of Object.values(camCfg?.zones || {})) {
    pushUnique(z.preset);
  }
  return out;
}

export type PtzMoveDirection =
  | "left"
  | "right"
  | "up"
  | "down"
  | "zoom_in"
  | "zoom_out";

export function buildPtzMovePayload(
  direction: PtzMoveDirection,
  speed = 0.5,
  mode: "continuous" | "relative" = "continuous"
): {
  mode: "continuous" | "relative";
  action: string;
  pan: number;
  tilt: number;
  zoom: number;
  speed: number;
} {
  const safeSpeed = Number.isFinite(speed) ? speed : 0.5;
  const spd = Math.max(0.05, Math.min(1.0, safeSpeed));
  let pan = 0;
  let tilt = 0;
  let zoom = 0;
  if (direction === "left") pan = -spd;
  else if (direction === "right") pan = spd;
  else if (direction === "up") tilt = spd;
  else if (direction === "down") tilt = -spd;
  else if (direction === "zoom_in") zoom = spd;
  else if (direction === "zoom_out") zoom = -spd;
  return { mode, action: direction, pan, tilt, zoom, speed: spd };
}

export interface PtzMoveLifecycleOptions {
  camera: string;
  speed?: number;
  fetchImpl?: typeof fetch;
  onMoveStarted?: (camera: string, direction: PtzMoveDirection) => void;
  onMoveStopped?: (camera: string) => void;
}

export interface PtzMoveLifecycleController {
  isMoving: () => boolean;
  getActiveDirection: () => PtzMoveDirection | null;
  getCamera: () => string;
  startMove: (direction: PtzMoveDirection) => Promise<void>;
  stopMove: (force?: boolean) => Promise<void>;
  setCamera: (nextCamera: string) => Promise<void>;
  handleWindowBlur: () => Promise<void>;
  handleVisibilityChange: (visibilityState?: string) => Promise<void>;
  handleKeyBlur: () => Promise<void>;
  dispose: () => Promise<void>;
}

export function createPtzMoveLifecycle(
  options: PtzMoveLifecycleOptions
): PtzMoveLifecycleController {
  let currentCamera = options.camera;
  const speed = options.speed ?? 0.5;
  const fetchFn: typeof fetch =
    options.fetchImpl ?? ((input, init) => fetch(input, init));
  let moving = false;
  let activeDirection: PtzMoveDirection | null = null;
  let disposed = false;

  const sendStopForCamera = async (targetCam: string) => {
    if (!targetCam) return;
    try {
      await fetchFn(`/api/${encodeURIComponent(targetCam)}/ptz/stop`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        keepalive: true,
        body: JSON.stringify({ pan_tilt: true, zoom: true }),
      });
    } catch {
      // ignore transient network error
    }
    options.onMoveStopped?.(targetCam);
  };

  const stopMove = async (force = false) => {
    if (!moving && !force) return;
    const camToStop = currentCamera;
    moving = false;
    activeDirection = null;
    await sendStopForCamera(camToStop);
  };

  const startMove = async (direction: PtzMoveDirection) => {
    if (disposed || !currentCamera) return;
    const targetCam = currentCamera;
    moving = true;
    activeDirection = direction;
    const payload = buildPtzMovePayload(direction, speed, "continuous");
    options.onMoveStarted?.(targetCam, direction);
    try {
      await fetchFn(`/api/${encodeURIComponent(targetCam)}/ptz/move`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(payload),
      });
    } catch {
      // ignore transient network error
    }
  };

  const setCamera = async (nextCamera: string) => {
    if (nextCamera === currentCamera) return;
    if (moving) {
      const prevCam = currentCamera;
      moving = false;
      activeDirection = null;
      currentCamera = nextCamera;
      await sendStopForCamera(prevCam);
      return;
    }
    currentCamera = nextCamera;
  };

  return {
    isMoving: () => moving,
    getActiveDirection: () => activeDirection,
    getCamera: () => currentCamera,
    startMove,
    stopMove,
    setCamera,
    handleWindowBlur: () => stopMove(false),
    handleVisibilityChange: (visibilityState?: string) => {
      if (!visibilityState || visibilityState === "hidden") {
        return stopMove(false);
      }
      return stopMove(false);
    },
    handleKeyBlur: () => stopMove(false),
    dispose: () => {
      disposed = true;
      return stopMove(false);
    },
  };
}

export const TacticalPtzHudOverlay: React.FC<{
  camera: string;
  cameraConfig?: CameraConfig;
  ptzInfo?: CameraPtzInfoResponse | null;
  onPtzUpdate?: (camera: string, nextInfo: CameraPtzInfoResponse) => void;
}> = ({ camera, cameraConfig, ptzInfo, onPtzUpdate }) => {
  const ptzEnabled = isCameraPtzEnabled(cameraConfig, ptzInfo);
  const onPtzUpdateRef = useRef(onPtzUpdate);
  onPtzUpdateRef.current = onPtzUpdate;
  const ptzInfoRef = useRef(ptzInfo);
  ptzInfoRef.current = ptzInfo;
  const lifecycleRef = useRef<PtzMoveLifecycleController | null>(null);

  useEffect(() => {
    if (!ptzEnabled) {
      if (lifecycleRef.current) {
        void lifecycleRef.current.dispose();
        lifecycleRef.current = null;
      }
      return;
    }
    const ctrl = createPtzMoveLifecycle({
      camera,
      speed: 0.5,
      onMoveStarted: (cam) => {
        if (onPtzUpdateRef.current) {
          onPtzUpdateRef.current(cam, {
            ...(ptzInfoRef.current || { enabled: true }),
            active_preset: null,
          });
        }
      },
    });
    lifecycleRef.current = ctrl;

    const onWindowBlur = () => {
      void ctrl.handleWindowBlur();
    };
    const onWindowPointerEnd = () => {
      void ctrl.stopMove(false);
    };
    const onVisibilityChange = () => {
      if (typeof document !== "undefined" && document.visibilityState === "hidden") {
        void ctrl.handleVisibilityChange("hidden");
      } else {
        void ctrl.handleVisibilityChange();
      }
    };

    if (typeof window !== "undefined") {
      window.addEventListener("blur", onWindowBlur);
      window.addEventListener("pagehide", onWindowBlur);
      window.addEventListener("pointerup", onWindowPointerEnd);
      window.addEventListener("pointercancel", onWindowPointerEnd);
    }
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", onVisibilityChange);
    }

    return () => {
      if (typeof window !== "undefined") {
        window.removeEventListener("blur", onWindowBlur);
        window.removeEventListener("pagehide", onWindowBlur);
        window.removeEventListener("pointerup", onWindowPointerEnd);
        window.removeEventListener("pointercancel", onWindowPointerEnd);
      }
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", onVisibilityChange);
      }
      void ctrl.dispose();
      if (lifecycleRef.current === ctrl) {
        lifecycleRef.current = null;
      }
    };
  }, [camera, ptzEnabled]);

  if (!ptzEnabled) {
    return null;
  }

  const presets = resolveCameraPtzPresets(cameraConfig, ptzInfo);
  const activePreset =
    ptzInfo?.active_preset !== undefined
      ? ptzInfo.active_preset
      : cameraConfig?.onvif?.return_preset || null;

  const startContinuousMove = (dir: PtzMoveDirection) => {
    void lifecycleRef.current?.startMove(dir);
  };

  const stopContinuousMove = (force = false) => {
    void lifecycleRef.current?.stopMove(force);
  };

  const bindDirectionButtonProps = (
    dir: PtzMoveDirection,
    ariaLabel: string,
    title: string
  ) => ({
    type: "button" as const,
    className: "nvr-btn nvr-tile-action mono",
    style: { height: 20, padding: "0 5px", fontSize: "var(--text-xs)" },
    "aria-label": ariaLabel,
    title,
    onPointerDown: (e: React.PointerEvent<HTMLButtonElement>) => {
      e.stopPropagation();
      startContinuousMove(dir);
    },
    onPointerUp: (e: React.PointerEvent<HTMLButtonElement>) => {
      e.stopPropagation();
      stopContinuousMove(false);
    },
    onPointerCancel: (e: React.PointerEvent<HTMLButtonElement>) => {
      e.stopPropagation();
      stopContinuousMove(false);
    },
    onPointerLeave: (e: React.PointerEvent<HTMLButtonElement>) => {
      e.stopPropagation();
      stopContinuousMove(false);
    },
    onKeyDown: (e: React.KeyboardEvent<HTMLButtonElement>) => {
      if ((e.key === " " || e.key === "Enter") && !e.repeat) {
        e.preventDefault();
        e.stopPropagation();
        startContinuousMove(dir);
      }
    },
    onKeyUp: (e: React.KeyboardEvent<HTMLButtonElement>) => {
      if (e.key === " " || e.key === "Enter") {
        e.preventDefault();
        e.stopPropagation();
        stopContinuousMove(false);
      }
    },
    onBlur: () => {
      stopContinuousMove(false);
    },
  });

  const sendPreset = async (preset: string) => {
    const clean = preset.trim();
    if (!clean) return;
    stopContinuousMove(false);
    try {
      const res = await fetch(`/api/${encodeURIComponent(camera)}/ptz/preset`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ preset: clean }),
      });
      if (res.ok && onPtzUpdate) {
        onPtzUpdate(camera, {
          ...(ptzInfo || { enabled: true }),
          active_preset: clean,
        });
      }
    } catch {
      // ignore
    }
  };

  return (
    <div
      className="nvr-ptz-hud mono"
      aria-label={`${camera} PTZ Controls`}
      onClick={(e) => e.stopPropagation()}
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 3,
        padding: "2px 5px",
        background: "rgba(7, 10, 15, 0.88)",
        border: "1px solid var(--border-strong)",
        borderRadius: 3,
        pointerEvents: "auto",
      }}
    >
      <span
        className="nvr-badge mono"
        style={{
          height: 20,
          padding: "0 5px",
          fontSize: "var(--text-xs)",
          color: "var(--signal-accent-text)",
        }}
      >
        PTZ{activePreset ? `:${activePreset}` : ""}
      </span>
      <button {...bindDirectionButtonProps("left", `PTZ Pan Left ${camera}`, "Pan Left")}>
        ◀
      </button>
      <button {...bindDirectionButtonProps("up", `PTZ Tilt Up ${camera}`, "Tilt Up")}>
        ▲
      </button>
      <button {...bindDirectionButtonProps("down", `PTZ Tilt Down ${camera}`, "Tilt Down")}>
        ▼
      </button>
      <button {...bindDirectionButtonProps("right", `PTZ Pan Right ${camera}`, "Pan Right")}>
        ▶
      </button>
      <button {...bindDirectionButtonProps("zoom_in", `PTZ Zoom In ${camera}`, "Zoom In")}>
        Z+
      </button>
      <button {...bindDirectionButtonProps("zoom_out", `PTZ Zoom Out ${camera}`, "Zoom Out")}>
        Z-
      </button>
      <button
        type="button"
        className="nvr-btn danger nvr-tile-action mono"
        style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
        aria-label={`PTZ Stop ${camera}`}
        title="Stop PTZ Movement"
        onClick={() => stopContinuousMove(true)}
      >
        ■
      </button>
      {presets.length > 0 && (
        <select
          className="nvr-select mono"
          aria-label={`PTZ Preset Selector ${camera}`}
          style={{ height: 20, padding: "0 4px", fontSize: "var(--text-xs)" }}
          value={activePreset || ""}
          onChange={(e) => {
            if (e.target.value) {
              void sendPreset(e.target.value);
            }
          }}
        >
          <option value="">Preset...</option>
          {presets.map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
      )}
    </div>
  );
};

interface LiveWorkspaceProps {
  cameraNames: string[];
  reviews: ReviewItem[];
  unreviewedAlerts: ReviewItem[];
  focusSegments: RecordingSegment[];
  handleMarkReviews: (ids: string[], reviewed?: boolean) => Promise<void>;
  handleQuickExport: (
    camera: string,
    startSec: number,
    endSec: number,
    label?: string
  ) => Promise<{ exportId?: string; videoUrl?: string } | void>;
}

export const LiveWorkspace: React.FC<LiveWorkspaceProps> = ({
  cameraNames,
  reviews,
  unreviewedAlerts,
  focusSegments,
  handleMarkReviews,
  handleQuickExport,
}) => {
  const { t } = useI18n();
  const [hoveredAlertId, setHoveredAlertId] = useState<string | null>(null);
  const [hoveredDockCam, setHoveredDockCam] = useState<string | null>(null);

  const liveLayout = useNvrStore((s) => s.liveLayout);
  const setLiveLayout = useNvrStore((s) => s.setLiveLayout);
  const activeCameraGroup = useNvrStore((s) => s.activeCameraGroup);
  const setActiveCameraGroup = useNvrStore((s) => s.setActiveCameraGroup);
  const matrixDensity = useNvrStore((s) => s.matrixDensity);
  const setMatrixDensity = useNvrStore((s) => s.setMatrixDensity);
  const matrixPage = useNvrStore((s) => s.matrixPage);
  const setMatrixPage = useNvrStore((s) => s.setMatrixPage);
  const matrixTourEnabled = useNvrStore((s) => s.matrixTourEnabled);
  const setMatrixTourEnabled = useNvrStore((s) => s.setMatrixTourEnabled);
  const matrixTourIntervalSec = useNvrStore((s) => s.matrixTourIntervalSec);
  const spotlightFilter = useNvrStore((s) => s.spotlightFilter);
  const setSpotlightFilter = useNvrStore((s) => s.setSpotlightFilter);
  const spotlightHoldSec = useNvrStore((s) => s.spotlightHoldSec);
  const setSpotlightHoldSec = useNvrStore((s) => s.setSpotlightHoldSec);
  const spotlightTimelineMode = useNvrStore((s) => s.spotlightTimelineMode);
  const setSpotlightTimelineMode = useNvrStore(
    (s) => s.setSpotlightTimelineMode
  );
  const selectedCamera = useNvrStore((s) => s.selectedCamera);
  const setSelectedCamera = useNvrStore((s) => s.setSelectedCamera);
  const hudPosition = useNvrStore((s) => s.hudPosition);
  const setHudPosition = useNvrStore((s) => s.setHudPosition);
  const useMainStream = useNvrStore((s) => s.useMainStream);
  const setUseMainStream = useNvrStore((s) => s.setUseMainStream);
  const showVectors = useNvrStore((s) => s.showVectors);
  const setShowVectors = useNvrStore((s) => s.setShowVectors);
  const showAlertRibbon = useNvrStore((s) => s.showAlertRibbon);
  const timeShiftTs = useNvrStore((s) => s.timeShiftTs);
  const setTimeShiftTs = useNvrStore((s) => s.setTimeShiftTs);
  const playbackSpeed = useNvrStore((s) => s.playbackSpeed);
  const simulatedFleetCount = useNvrStore((s) => s.simulatedFleetCount);
  const setSimulatedFleetCount = useNvrStore((s) => s.setSimulatedFleetCount);
  const themeMode = useNvrStore((s) => s.themeMode);
  const highContrast = useNvrStore((s) => s.highContrast);
  const effectiveTheme = resolveEffectiveTheme(themeMode);
  const mapTheme = useMemo(
    () => resolveCanvasThemePalette(effectiveTheme),
    [effectiveTheme, highContrast]
  );
  const config = useNvrStore((s) => s.config);
  const setConfig = useNvrStore((s) => s.setConfig);
  const stats = useNvrStore((s) => s.stats);
  const cameraActivity = useNvrStore((s) => s.cameraActivity);

  const fleet = useMemo(
    () =>
      expandSimulatedFleet(
        cameraNames,
        config?.cameras,
        config?.camera_groups,
        simulatedFleetCount
      ),
    [cameraNames, config?.cameras, config?.camera_groups, simulatedFleetCount]
  );
  const effectiveCameraNames = fleet.cameraNames;
  const effectiveCamerasConfig = fleet.camerasConfig;
  const effectiveCameraGroups = fleet.cameraGroups;

  const lastActiveRef = useRef<Record<string, number>>({});
  const lastSurgeRef = useRef<Record<string, number>>({});
  const prevMatchRef = useRef<Record<string, boolean>>({});
  const [spotlightTrail, setSpotlightTrail] = useState<Array<{ cam: string; ts: number }>>([]);
  const [lockedSpotlightCam, setLockedSpotlightCam] = useState<string | null>(null);
  const [nowTick, setNowTick] = useState<number>(() => Math.floor(Date.now() / 1000));
  const [mapEditMode, setMapEditMode] = useState(false);
  const [mapTool, setMapTool] = useState<FloorplanStudioTool>("select");
  const [draggingCam, setDraggingCam] = useState<string | null>(null);
  const [draggingAimCam, setDraggingAimCam] = useState<string | null>(null);
  const [mapFloorplanUrl, setMapFloorplanUrl] = useState<string>(() => {
    try {
      return sanitizeFloorplanUrl(
        window.localStorage?.getItem("ninjanvr_floorplan_url") || ""
      );
    } catch {
      return "";
    }
  });
  const [floorplanOpacity, setFloorplanOpacity] = useState<number>(0.45);
  const [floorplanWidthM, setFloorplanWidthM] = useState<number>(
    DEFAULT_FLOORPLAN_WIDTH_M
  );
  const [floorplanHeightM, setFloorplanHeightM] = useState<number>(
    DEFAULT_FLOORPLAN_HEIGHT_M
  );
  const [floorplanRooms, setFloorplanRooms] = useState<FloorplanRoom[]>([]);
  const [floorplanWalls, setFloorplanWalls] = useState<FloorplanWall[]>([]);
  const [selectedRoomId, setSelectedRoomId] = useState<string | null>(null);
  const [selectedWallIdx, setSelectedWallIdx] = useState<number | null>(null);
  const [showMeasurements, setShowMeasurements] = useState<boolean>(true);
  const [snapStep, setSnapStep] = useState<number>(0.005);
  const [orthoSnap, setOrthoSnap] = useState<boolean>(true);
  const [wallKindDraft, setWallKindDraft] = useState<"wall" | "door" | "window">("wall");
  const [quickWallLenM, setQuickWallLenM] = useState<number>(3.0);
  const [cursorNorm, setCursorNorm] = useState<{ x: number; y: number } | null>(
    null
  );
  const [roomDraft, setRoomDraft] = useState<{
    startX: number;
    startY: number;
    currentX: number;
    currentY: number;
  } | null>(null);
  const [draggingRoom, setDraggingRoom] = useState<{
    id: string;
    startNormX: number;
    startNormY: number;
    origX: number;
    origY: number;
  } | null>(null);
  const [resizingRoom, setResizingRoom] = useState<{
    id: string;
    handle: "nw" | "ne" | "sw" | "se" | "n" | "s" | "e" | "w";
    anchorX: number;
    anchorY: number;
    origRoom: FloorplanRoom;
  } | null>(null);
  const [wallStartPoint, setWallStartPoint] = useState<{
    x: number;
    y: number;
  } | null>(null);
  const [wallPreviewPoint, setWallPreviewPoint] = useState<{
    x: number;
    y: number;
    snappedToVertex?: boolean;
  } | null>(null);
  const [draggingWall, setDraggingWall] = useState<{
    index: number;
    handle: "start" | "end" | "move";
    startNormX: number;
    startNormY: number;
    origPoints: [number, number, number, number];
  } | null>(null);
  const [mapPlacements, setMapPlacements] = useState<Record<string, CameraMapPlacement>>({});
  const [mapSaveStatus, setMapSaveStatus] = useState<string | null>(null);
  const [ptzInfoByCam, setPtzInfoByCam] = useState<Record<string, CameraPtzInfoResponse>>({});
  const [pttResetEpoch, setPttResetEpoch] = useState<number>(0);
  const activePttCamerasRef = useRef<Set<string>>(new Set());
  const ptzFetchedCamsRef = useRef<Set<string>>(new Set());
  const mapSaveTimerRef = useRef<number | null>(null);
  const activeFileReaderRef = useRef<FileReader | null>(null);
  const mapSvgRef = useRef<SVGSVGElement | null>(null);
  const floorplanFileInputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    return () => {
      if (mapSaveTimerRef.current !== null) {
        window.clearTimeout(mapSaveTimerRef.current);
        mapSaveTimerRef.current = null;
      }
      if (activeFileReaderRef.current) {
        try {
          activeFileReaderRef.current.abort();
        } catch {
          // ignore
        }
        activeFileReaderRef.current = null;
      }
    };
  }, []);

  useEffect(() => {
    const releaseActivePtt = () => {
      if (activePttCamerasRef.current.size > 0) {
        activePttCamerasRef.current.clear();
        setPttResetEpoch((n) => n + 1);
      }
    };
    const onVisibility = () => {
      if (typeof document !== "undefined" && document.visibilityState === "hidden") {
        releaseActivePtt();
      }
    };
    if (typeof window !== "undefined") {
      window.addEventListener("blur", releaseActivePtt);
      window.addEventListener("pagehide", releaseActivePtt);
    }
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", onVisibility);
    }
    return () => {
      if (typeof window !== "undefined") {
        window.removeEventListener("blur", releaseActivePtt);
        window.removeEventListener("pagehide", releaseActivePtt);
      }
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", onVisibility);
      }
    };
  }, []);

  useEffect(() => {
    if (activePttCamerasRef.current.size > 0) {
      activePttCamerasRef.current.clear();
      setPttResetEpoch((n) => n + 1);
    }
  }, [liveLayout, selectedCamera, matrixPage, activeCameraGroup, timeShiftTs]);

  const snapVertices = useMemo(() => {
    const pts: Array<{ x: number; y: number }> = [];
    for (const w of floorplanWalls) {
      pts.push({ x: w.points[0], y: w.points[1] });
      pts.push({ x: w.points[2], y: w.points[3] });
    }
    for (const r of floorplanRooms) {
      pts.push({ x: r.x, y: r.y });
      pts.push({ x: r.x + r.w, y: r.y });
      pts.push({ x: r.x, y: r.y + r.h });
      pts.push({ x: r.x + r.w, y: r.y + r.h });
    }
    return pts;
  }, [floorplanWalls, floorplanRooms]);

  useEffect(() => {
    if (liveLayout !== "map" || !mapEditMode) return;
    const onKeyDown = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const tag = target?.tagName?.toLowerCase();
      if (tag === "input" || tag === "textarea" || tag === "select") return;
      if (e.key === "Escape") {
        setRoomDraft(null);
        setWallStartPoint(null);
        setWallPreviewPoint(null);
        setSelectedRoomId(null);
        setSelectedWallIdx(null);
      } else if (e.key === "Delete" || e.key === "Backspace") {
        if (selectedRoomId) {
          e.preventDefault();
          setFloorplanRooms((prev) =>
            prev.filter((r) => r.id !== selectedRoomId)
          );
          setSelectedRoomId(null);
        } else if (selectedWallIdx !== null) {
          e.preventDefault();
          setFloorplanWalls((prev) =>
            prev.filter((_, idx) => idx !== selectedWallIdx)
          );
          setSelectedWallIdx(null);
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [liveLayout, mapEditMode, selectedRoomId, selectedWallIdx]);

  useEffect(() => {
    if (mapEditMode) return;
    const fp = config?.ui?.floorplan;
    if (fp) {
      if (typeof fp.opacity === "number" && Number.isFinite(fp.opacity)) {
        setFloorplanOpacity(Math.max(0.1, Math.min(1, fp.opacity)));
      }
      if (typeof fp.width_m === "number" && Number.isFinite(fp.width_m) && fp.width_m >= 2) {
        setFloorplanWidthM(Math.min(500, fp.width_m));
      }
      if (typeof fp.height_m === "number" && Number.isFinite(fp.height_m) && fp.height_m >= 2) {
        setFloorplanHeightM(Math.min(500, fp.height_m));
      }
      if (Array.isArray(fp.rooms)) {
        setFloorplanRooms(fp.rooms);
      }
      if (Array.isArray(fp.walls)) {
        setFloorplanWalls(fp.walls);
      }
      if (fp.background_data_url) {
        const cleanBg = sanitizeFloorplanUrl(fp.background_data_url);
        if (cleanBg) {
          setMapFloorplanUrl(cleanBg);
        }
      }
    }
  }, [config?.ui?.floorplan, mapEditMode]);

  useEffect(() => {
    let cancelled = false;
    const cams = config?.cameras || {};
    for (const [camName, camCfg] of Object.entries(cams)) {
      if (isCameraPtzEnabled(camCfg) && !ptzFetchedCamsRef.current.has(camName)) {
        ptzFetchedCamsRef.current.add(camName);
        fetch(`/api/${encodeURIComponent(camName)}/ptz/info`, {
          credentials: "include",
        })
          .then((res) => (res.ok ? res.json() : null))
          .then((data: CameraPtzInfoResponse | null) => {
            if (!cancelled && data) {
              setPtzInfoByCam((prev) => ({ ...prev, [camName]: data }));
            }
          })
          .catch(() => {});
      }
    }
    return () => {
      cancelled = true;
    };
  }, [config?.cameras]);

  const handlePtzUpdate = (camName: string, nextInfo: CameraPtzInfoResponse) => {
    setPtzInfoByCam((prev) => ({
      ...prev,
      [camName]: { ...(prev[camName] || {}), ...nextInfo },
    }));
  };

  useEffect(() => {
    if (mapEditMode) return;
    setMapPlacements((prev) => {
      const next = { ...prev };
      effectiveCameraNames.forEach((cam, idx) => {
        const cfgMap = effectiveCamerasConfig?.[cam]?.map;
        const fallbackPlacement = computeDefaultMapPlacement(
          idx,
          effectiveCameraNames.length
        );
        const isUnconfiguredDefault =
          cfgMap &&
          Math.abs((cfgMap.x ?? 0.5) - 0.5) < 0.001 &&
          Math.abs((cfgMap.y ?? 0.5) - 0.5) < 0.001 &&
          Math.abs(cfgMap.angle_deg ?? 0) < 0.001;
        if (cfgMap && !isUnconfiguredDefault) {
          next[cam] = sanitizeMapPlacement(cfgMap, fallbackPlacement);
        } else if (!next[cam]) {
          next[cam] = fallbackPlacement;
        }
      });
      return next;
    });
  }, [effectiveCameraNames, effectiveCamerasConfig, mapEditMode]);

  const handleFloorplanFileUpload = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (!file) return;
    if (activeFileReaderRef.current) {
      try {
        activeFileReaderRef.current.abort();
      } catch {
        // ignore
      }
    }
    const reader = new FileReader();
    activeFileReaderRef.current = reader;
    reader.onload = () => {
      activeFileReaderRef.current = null;
      if (typeof reader.result === "string") {
        const cleanDataUrl = sanitizeFloorplanUrl(reader.result);
        if (!cleanDataUrl) return;
        setMapFloorplanUrl(cleanDataUrl);
        setMapEditMode(true);
        try {
          window.localStorage?.setItem("ninjanvr_floorplan_url", cleanDataUrl);
        } catch {
          // ignore quota errors for large data URLs in localStorage; saved via config.yml
        }
      }
    };
    reader.readAsDataURL(file);
    e.target.value = "";
  };

  const handleClearFloorplanImage = () => {
    setMapFloorplanUrl("");
    setMapEditMode(true);
    try {
      window.localStorage?.removeItem("ninjanvr_floorplan_url");
    } catch {
      // ignore
    }
  };

  const handleSaveMapPlacements = async () => {
    try {
      const cfgRes = await fetch("/api/config", { credentials: "include" });
      if (!cfgRes.ok) return;
      const currentCfg = await cfgRes.json();
      if (currentCfg && currentCfg.cameras) {
        const payload = buildFloorplanSavePayload({
          existingCameras: currentCfg.cameras,
          mapPlacements,
          existingUi: currentCfg.ui,
          floorplanUrl: mapFloorplanUrl,
          floorplanOpacity,
          floorplanWidthM,
          floorplanHeightM,
          floorplanRooms,
          floorplanWalls,
        });
        const saveRes = await fetch("/api/config/structured?save_option=saveonly", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify(payload),
        });
        if (saveRes.ok) {
          const savedData = await saveRes.json().catch(() => ({}));
          if (savedData.config) {
            setConfig(savedData.config);
          }
          setMapSaveStatus("Saved to config.yml");
          if (mapSaveTimerRef.current !== null) {
            window.clearTimeout(mapSaveTimerRef.current);
          }
          mapSaveTimerRef.current = window.setTimeout(() => {
            mapSaveTimerRef.current = null;
            setMapSaveStatus(null);
          }, 2500);
        }
      }
    } catch {
      // ignore
    }
  };

  useEffect(() => {
    const nowSec = Math.floor(Date.now() / 1000);
    for (const cam of cameraNames) {
      const act = cameraActivity[cam];
      const objs = act?.objects || [];
      const activeNonStationary = objs.filter((o) => !o.stationary);
      // Ignore tiny < 3500 px² motion boxes caused by 1-second camera OSD clock digit ticks
      const hasRealMotion = Boolean(act?.motion && (act?.max_Area ?? 5000) >= 3500);

      let matches = false;
      if (spotlightFilter === "all_activity") {
        matches = hasRealMotion || activeNonStationary.length > 0;
      } else if (spotlightFilter === "objects") {
        matches = activeNonStationary.length > 0;
      } else if (spotlightFilter === "person") {
        matches = activeNonStationary.some((o) => o.label === "person");
      } else if (spotlightFilter === "vehicle") {
        matches = activeNonStationary.some((o) =>
          ["car", "truck", "motorcycle", "bicycle"].includes(o.label)
        );
      }

      const wasMatching = Boolean(prevMatchRef.current[cam]);
      if (matches) {
        lastActiveRef.current[cam] = nowSec;
        if (!wasMatching || nowSec - (lastSurgeRef.current[cam] || 0) >= 8) {
          lastSurgeRef.current[cam] = nowSec;
          setSpotlightTrail((prev) => {
            if (prev.length > 0 && prev[prev.length - 1].cam === cam) {
              const updated = [...prev];
              updated[updated.length - 1] = { cam, ts: nowSec };
              return updated;
            }
            return [...prev.slice(-4), { cam, ts: nowSec }];
          });
        }
      }
      prevMatchRef.current[cam] = matches;
    }
    setNowTick(nowSec);
  }, [cameraActivity, cameraNames, spotlightFilter]);

  useEffect(() => {
    if (liveLayout !== "spotlight" && simulatedFleetCount <= 0) return;
    const tmr = window.setInterval(() => {
      setNowTick(Math.floor(Date.now() / 1000));
    }, 1500);
    return () => window.clearInterval(tmr);
  }, [liveLayout, simulatedFleetCount]);

  const groupCams = useMemo(() => {
    const resolved = resolveOrderedDashboardCameras(
      effectiveCameraNames,
      effectiveCamerasConfig,
      activeCameraGroup,
      effectiveCameraGroups
    );
    return resolved.length > 0 || activeCameraGroup !== "all"
      ? resolved
      : effectiveCameraNames;
  }, [
    effectiveCameraNames,
    effectiveCamerasConfig,
    activeCameraGroup,
    effectiveCameraGroups,
  ]);

  const { visibleMatrixCams, pageSize, totalPages, safePage } = useMemo(
    () => sliceMatrixPage(groupCams, matrixPage, matrixDensity),
    [groupCams, matrixPage, matrixDensity]
  );

  // Bi-directional Stage <-> Timeline synchronization:
  // Track explicit camera jumps initiated from the Timeline/Search picker so the page-change
  // effect never clobbers a newly selected camera on another page back to visibleMatrixCams[0].
  const pendingStageCameraRef = useRef<string | null>(null);

  useEffect(() => {
    if (liveLayout !== "2x2" || visibleMatrixCams.length === 0) return;
    if (pendingStageCameraRef.current) {
      if (visibleMatrixCams.includes(pendingStageCameraRef.current)) {
        if (selectedCamera !== pendingStageCameraRef.current) {
          setSelectedCamera(pendingStageCameraRef.current);
        }
        pendingStageCameraRef.current = null;
      }
      return;
    }
    if (!visibleMatrixCams.includes(selectedCamera)) {
      setSelectedCamera(visibleMatrixCams[0]);
    }
  }, [
    liveLayout,
    safePage,
    activeCameraGroup,
    matrixDensity,
    visibleMatrixCams,
    selectedCamera,
    setSelectedCamera,
  ]);

  // When the operator selects ANY camera from the Timeline or Search picker, automatically
  // jump the Matrix Page (and group if needed) so that camera is immediately visible on stage.
  const selectCameraAndRevealOnStage = (cam: string) => {
    pendingStageCameraRef.current = cam;
    setSelectedCamera(cam);
    if (liveLayout === "spotlight") {
      const ts = Math.floor(Date.now() / 1000);
      lastActiveRef.current[cam] = ts;
      lastSurgeRef.current[cam] = ts;
      setNowTick(ts);
      setLockedSpotlightCam(cam);
      return;
    }
    if (liveLayout === "2x2") {
      const idxInGroup = groupCams.indexOf(cam);
      if (idxInGroup >= 0) {
        const targetPage = Math.floor(idxInGroup / Math.max(1, pageSize));
        if (targetPage !== safePage) {
          setMatrixPage(targetPage);
        } else {
          pendingStageCameraRef.current = null;
        }
      } else {
        setActiveCameraGroup("all");
        const allOrdered = resolveOrderedDashboardCameras(
          effectiveCameraNames,
          effectiveCamerasConfig,
          "all",
          effectiveCameraGroups
        );
        const idxInAll = allOrdered.indexOf(cam);
        if (idxInAll >= 0) {
          setMatrixPage(Math.floor(idxInAll / Math.max(1, pageSize)));
        }
      }
    }
  };

  useEffect(() => {
    if (liveLayout !== "2x2" || !matrixTourEnabled || totalPages <= 1) return;
    const tmr = window.setInterval(() => {
      setMatrixPage((prev) => nextMatrixTourPage(prev, totalPages));
    }, Math.max(3, matrixTourIntervalSec) * 1000);
    return () => window.clearInterval(tmr);
  }, [
    liveLayout,
    matrixTourEnabled,
    totalPages,
    matrixTourIntervalSec,
    setMatrixPage,
  ]);

  const cameraGroupEntries = useMemo(() => {
    if (!effectiveCameraGroups) return [];
    return Object.entries(effectiveCameraGroups).sort(
      ([aKey, aVal], [bKey, bVal]) =>
        ((aVal.order ?? 0) - (bVal.order ?? 0)) || aKey.localeCompare(bKey)
    );
  }, [effectiveCameraGroups]);

  const groupCamSet = useMemo(() => new Set(groupCams), [groupCams]);
  const groupSpotlightTrail = useMemo(
    () => spotlightTrail.filter((item) => groupCamSet.has(item.cam)),
    [spotlightTrail, groupCamSet]
  );
  const groupSelectedCam = groupCamSet.has(selectedCamera)
    ? selectedCamera
    : groupCams[0] || selectedCamera;
  const groupLockedSpotlightCam =
    lockedSpotlightCam && groupCamSet.has(lockedSpotlightCam)
      ? lockedSpotlightCam
      : null;

  const spotlightPromotedCams = [...groupCams]
    .filter((cam) => nowTick - (lastActiveRef.current[cam] || 0) <= spotlightHoldSec)
    .sort(
      (a, b) =>
        (lastSurgeRef.current[b] || lastActiveRef.current[b] || 0) -
        (lastSurgeRef.current[a] || lastActiveRef.current[a] || 0)
    );
  const liveSpotlightHeroCam = resolveEffectiveSpotlightHeroCam({
    promotedCams: spotlightPromotedCams,
    trailCams: groupSpotlightTrail,
    selectedCamera: groupSelectedCam,
    fallbackCam: groupCams[0],
    timeShiftTs: null,
    lockedHeroCam: null,
  });
  const spotlightHeroCam = resolveEffectiveSpotlightHeroCam({
    promotedCams: spotlightPromotedCams,
    trailCams: groupSpotlightTrail,
    selectedCamera: groupSelectedCam,
    fallbackCam: groupCams[0],
    timeShiftTs,
    lockedHeroCam: groupLockedSpotlightCam || liveSpotlightHeroCam,
  });

  useEffect(() => {
    if (liveLayout !== "spotlight") return;
    if (timeShiftTs === null) {
      setLockedSpotlightCam(liveSpotlightHeroCam || null);
    } else if (!lockedSpotlightCam && liveSpotlightHeroCam) {
      setLockedSpotlightCam(liveSpotlightHeroCam);
    }
  }, [liveLayout, timeShiftTs, liveSpotlightHeroCam, lockedSpotlightCam]);

  useEffect(() => {
    if (
      liveLayout === "spotlight" &&
      timeShiftTs === null &&
      spotlightHeroCam &&
      selectedCamera !== spotlightHeroCam
    ) {
      setSelectedCamera(spotlightHeroCam);
    }
  }, [
    liveLayout,
    timeShiftTs,
    spotlightHeroCam,
    selectedCamera,
    setSelectedCamera,
  ]);

  const lastLayoutSwitchMsRef = useRef<number>(0);

  const stageGroupCams =
    groupCams.length > MAX_DOCK_LIVE_DECODERS + 1 &&
    visibleMatrixCams.length > 0
      ? [
          ...visibleMatrixCams,
          ...groupCams.filter((c) => !visibleMatrixCams.includes(c)),
        ]
      : groupCams;

  const stageDescriptors = computeLiveStageDescriptors({
    liveLayout,
    visibleMatrixCams,
    groupCams: stageGroupCams,
    selectedCamera,
    spotlightHeroCam,
    spotlightPromotedCams,
    hoveredDockCam,
    useMainStream,
    timeShiftTs,
    maxDockLiveDecoders: MAX_DOCK_LIVE_DECODERS,
  });

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <button
            className={`nvr-btn ${
              liveLayout === "2x2" && matrixDensity === "2x2" ? "active" : ""
            }`}
            onClick={() => {
              setLiveLayout("2x2");
              setMatrixDensity("2x2");
              setTimeShiftTs(null);
            }}
          >
            {t.live.grid2x2}
          </button>
          <button
            className={`nvr-btn mono ${
              liveLayout === "2x2" && matrixDensity === "3x3" ? "active" : ""
            }`}
            onClick={() => {
              setLiveLayout("2x2");
              setMatrixDensity("3x3");
              setTimeShiftTs(null);
            }}
            title="3x3 Paginated Tactical Matrix (9 channels per page)"
          >
            3x3 Grid
          </button>
          <button
            className={`nvr-btn mono ${
              liveLayout === "2x2" && matrixDensity === "4x4" ? "active" : ""
            }`}
            onClick={() => {
              setLiveLayout("2x2");
              setMatrixDensity("4x4");
              setTimeShiftTs(null);
            }}
            title="4x4 High-Density Tactical Matrix (16 channels per page)"
          >
            4x4 Grid
          </button>
          <button
            className={`nvr-btn ${liveLayout === "focus" ? "active" : ""}`}
            onClick={() => setLiveLayout("focus")}
          >
            {t.live.focusStage}
          </button>
          <button
            className={`nvr-btn ${liveLayout === "single" ? "active" : ""}`}
            onClick={() => setLiveLayout("single")}
          >
            {t.live.singleFull}
          </button>
          <button
            className={`nvr-btn ${liveLayout === "spotlight" ? "active" : ""}`}
            onClick={() => {
              setLiveLayout("spotlight");
              setTimeShiftTs(null);
            }}
            title="Zero-CPU Dynamic Birdseye: automatically enlarges cameras with active motion or tracked objects"
          >
            Spotlight
          </button>
          <button
            className={`nvr-btn ${liveLayout === "map" ? "active" : ""}`}
            onClick={() => {
              setLiveLayout("map");
              setTimeShiftTs(null);
            }}
            title="Interactive Tactical Map / Floorplan with FOV cones and real-time motion/object pulses"
          >
            Tactical Map
          </button>

          {cameraGroupEntries.length > 0 && (
            <>
              <span className="nvr-divider" aria-hidden="true" />
              <button
                type="button"
                className={`nvr-btn mono ${
                  activeCameraGroup === "all" ? "active" : ""
                }`}
                style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
                onClick={() => setActiveCameraGroup("all")}
              >
                ALL ({effectiveCameraNames.length})
              </button>
              {cameraGroupEntries.map(([gName, gCfg]) => (
                <button
                  key={gName}
                  type="button"
                  className={`nvr-btn mono ${
                    activeCameraGroup === gName ? "active" : ""
                  }`}
                  style={{
                    height: 24,
                    padding: "0 7px",
                    fontSize: "var(--text-xs)",
                    textTransform: "uppercase",
                  }}
                  onClick={() => setActiveCameraGroup(gName)}
                >
                  {gName} ({gCfg.cameras?.length || 0})
                </button>
              ))}
            </>
          )}

          {liveLayout === "2x2" && totalPages > 1 && (
            <>
              <span className="nvr-divider" aria-hidden="true" />
              <button
                type="button"
                className="nvr-btn mono"
                style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
                aria-label="Previous matrix page"
                onClick={() =>
                  setMatrixPage((p) => (p - 1 + totalPages) % totalPages)
                }
              >
                <ChevronLeft size={12} />
              </button>
              <span
                className="nvr-badge mono"
                style={{ height: 24, padding: "0 8px" }}
              >
                Page {safePage + 1} / {totalPages}
              </span>
              <button
                type="button"
                className="nvr-btn mono"
                style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
                aria-label="Next matrix page"
                onClick={() =>
                  setMatrixPage((p) => nextMatrixTourPage(p, totalPages))
                }
              >
                <ChevronRight size={12} />
              </button>
              <button
                type="button"
                className={`nvr-btn mono ${matrixTourEnabled ? "active" : ""}`}
                style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => setMatrixTourEnabled((v) => !v)}
                title="Auto-Tour carousel across matrix pages"
              >
                <RefreshCw size={11} />
                Tour {matrixTourIntervalSec}s
              </button>
            </>
          )}

          {liveLayout === "spotlight" && (
            <>
              <span className="nvr-divider" aria-hidden="true" />
              {(
                [
                  ["all_activity", "All Motion"],
                  ["objects", "Active Objects"],
                  ["person", "Person Only"],
                  ["vehicle", "Vehicles"],
                ] as Array<[SpotlightFilter, string]>
              ).map(([flt, lbl]) => (
                <button
                  key={flt}
                  className={`nvr-btn mono ${spotlightFilter === flt ? "active" : ""}`}
                  style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
                  onClick={() => setSpotlightFilter(flt)}
                >
                  {lbl}
                </button>
              ))}
              <select
                className="nvr-select mono"
                aria-label="Spotlight hold duration"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                value={spotlightHoldSec}
                onChange={(e) => setSpotlightHoldSec(Number(e.target.value))}
                title="How long a promoted camera stays on the main stage before returning to standby"
              >
                <option value={15}>Hold: 15s</option>
                <option value={30}>Hold: 30s</option>
                <option value={60}>Hold: 60s</option>
              </select>
              <span className="nvr-divider" aria-hidden="true" />
              <button
                type="button"
                data-testid="spotlight-timeline-mode-reactive"
                className={`nvr-btn mono ${
                  spotlightTimelineMode === "reactive" ? "active" : ""
                }`}
                style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
                onClick={() => setSpotlightTimelineMode("reactive")}
                title="Show Timeline scrubber in Spotlight and auto-pause Spotlight camera rotation while scrubbing or playing time-shift"
              >
                {t.live.spotlightTimelineReactive}
              </button>
              <button
                type="button"
                data-testid="spotlight-timeline-mode-wall"
                className={`nvr-btn mono ${
                  spotlightTimelineMode === "wall" ? "active" : ""
                }`}
                style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
                onClick={() => setSpotlightTimelineMode("wall")}
                title="Hide bottom Timeline scrubber during live Spotlight wall monitoring (automatically appears during time-shift)"
              >
                {t.live.spotlightTimelineWall}
              </button>
              {timeShiftTs !== null && (
                <>
                  <span
                    data-testid="spotlight-paused-badge"
                    className="nvr-badge rewind mono"
                    style={{ height: 24, padding: "0 8px" }}
                  >
                    {t.live.spotlightPausedBadge}
                  </span>
                  <button
                    type="button"
                    data-testid="spotlight-resume-btn"
                    className="nvr-btn success mono"
                    style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                    onClick={() => setTimeShiftTs(null)}
                  >
                    <Play size={11} />
                    {t.live.resumeSpotlight}
                  </button>
                </>
              )}
              {spotlightTrail.length > 0 && (
                <span
                  className="mono"
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: 4,
                    fontSize: "var(--text-xs)",
                    color: "var(--text-secondary)",
                    marginLeft: 4,
                  }}
                >
                  <span>TRAIL:</span>
                  {spotlightTrail.map((item, idx) => {
                    const ago = Math.max(0, nowTick - item.ts);
                    const isCurrent = item.cam === spotlightHeroCam;
                    return (
                      <button
                        key={`${item.cam}-${item.ts}-${idx}`}
                        type="button"
                        className={`nvr-btn mono ${isCurrent ? "active" : ""}`}
                        style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                        onClick={() => {
                          lastActiveRef.current[item.cam] = nowTick;
                          lastSurgeRef.current[item.cam] = nowTick;
                          setNowTick(nowTick);
                          setSelectedCamera(item.cam);
                          setLockedSpotlightCam(item.cam);
                        }}
                      >
                        {item.cam.toUpperCase()} ({ago}s)
                      </button>
                    );
                  })}
                </span>
              )}
            </>
          )}
          {(liveLayout === "focus" || liveLayout === "single") && (
            <>
              <span className="nvr-divider" aria-hidden="true" />
              <TacticalCameraPicker
                cameras={groupCams}
                selectedCamera={selectedCamera}
                onSelect={setSelectedCamera}
                cameraGroups={effectiveCameraGroups}
                cameraStats={stats?.cameras}
                cameraActivity={cameraActivity}
                ariaLabel="Focus camera selector"
              />
            </>
          )}
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          {simulatedFleetCount > 0 && (
            <button
              type="button"
              className="nvr-btn danger mono"
              style={{ height: 26, padding: "0 8px", fontSize: "var(--text-xs)", fontWeight: 700 }}
              aria-label={`Exit Simulation (Restore ${cameraNames.filter((c) => !/_sim_\d+$/.test(c)).length || 4} Cameras)`}
              onClick={() => setSimulatedFleetCount(0)}
            >
              Exit Simulation ({simulatedFleetCount} Cams → {cameraNames.filter((c) => !/_sim_\d+$/.test(c)).length || 4})
            </button>
          )}
          <select
            className="nvr-select mono"
            aria-label="Camera HUD badge position"
            style={{ height: 28, fontSize: "var(--text-xs)" }}
            value={hudPosition}
            onChange={(e) => {
              setHudPosition(e.target.value as HudPosition);
            }}
            title="Configure where camera info badges appear so they never cover camera OSD text"
          >
            <option value="bottom-right">HUD: Bottom-Right</option>
            <option value="top-right">HUD: Top-Right</option>
            <option value="top-left">HUD: Top-Left</option>
            <option value="bottom-left">HUD: Bottom-Left</option>
            <option value="hidden">HUD: Hidden</option>
          </select>
          <button
            className={`nvr-btn ${useMainStream ? "active" : ""}`}
            onClick={() => setUseMainStream((v) => !v)}
            title="Switch between 1080p Main Stream and 720p Substream"
          >
            {useMainStream ? t.live.hdStream : t.live.subStream}
          </button>
          <button
            className={`nvr-btn ${showVectors ? "active" : ""}`}
            onClick={() => setShowVectors((v) => !v)}
            title="Overlay configured Motion Masks and Detection Zones"
          >
            <Eye size={13} />
            {t.live.zonesOverlay}
          </button>
          {liveLayout !== "spotlight" &&
            liveLayout !== "map" &&
            unreviewedAlerts.length > 0 && (
              <button
                className="nvr-btn success"
                onClick={() =>
                  handleMarkReviews(
                    unreviewedAlerts.map((r) => r.id),
                    true
                  )
                }
              >
                <Check size={13} />
                {t.live.markAllReviewed} ({unreviewedAlerts.length})
              </button>
            )}
        </div>
      </div>

      {/* Zero-Decoder-Bloat Unreviewed Alert Ribbon (Video mounts ONLY on pointer hover!) */}
      {shouldRenderLiveAlertRibbon(
        liveLayout,
        showAlertRibbon,
        unreviewedAlerts.length
      ) && (
        <div className="nvr-alert-ribbon" aria-label={t.live.unreviewedAlerts}>
          <div
            style={{
              minWidth: 118,
              display: "flex",
              flexDirection: "column",
              justifyContent: "center",
              gap: 3,
              paddingRight: 6,
              borderRight: "1px solid var(--border-hairline)",
            }}
          >
            <div
              className="mono"
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                color: "var(--signal-alert-text)",
              }}
            >
              {unreviewedAlerts.length} UNREVIEWED
            </div>
            <div
              style={{
                fontSize: "var(--text-xs)",
                color: "var(--text-muted)",
                lineHeight: 1.25,
              }}
            >
              Hover for MP4 · Click for Time-Shift
            </div>
          </div>

          {unreviewedAlerts.slice(0, 14).map((item) => {
            const isHovered = hoveredAlertId === item.id;
            const thumbSrc = resolveSafeAlertThumbSrc(
              item.thumb_path,
              item.camera
            );
            const objs = item.data?.objects || ["person"];

            return (
              <div
                key={item.id}
                role="button"
                tabIndex={0}
                className={`nvr-alert-card severity-${item.severity}`}
                onPointerEnter={() => setHoveredAlertId(item.id)}
                onPointerLeave={() =>
                  setHoveredAlertId((curr) => (curr === item.id ? null : curr))
                }
                onClick={() => {
                  setSelectedCamera(item.camera);
                  setLiveLayout("focus");
                  setTimeShiftTs(Math.floor(item.start_time));
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    setSelectedCamera(item.camera);
                    setLiveLayout("focus");
                    setTimeShiftTs(Math.floor(item.start_time));
                  }
                }}
              >
                {isHovered ? (
                  <video
                    src={`/api/review/${encodeURIComponent(item.id)}/preview?format=mp4`}
                    autoPlay
                    muted
                    loop
                    playsInline
                  />
                ) : (
                  <img
                    src={thumbSrc}
                    alt={`${item.camera} alert`}
                    width={156}
                    height={68}
                    loading="lazy"
                  />
                )}
                <div className="nvr-alert-card-hud">
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                    }}
                  >
                    <span
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        textTransform: "uppercase",
                        color: "var(--text-primary)",
                      }}
                    >
                      {item.camera}
                    </span>
                    <button
                      className="nvr-btn"
                      style={{
                        height: 20,
                        padding: "0 5px",
                        fontSize: "var(--text-xs)",
                        background: "rgba(16,185,129,0.25)",
                        borderColor: "rgba(16,185,129,0.6)",
                      }}
                      title={t.live.markReviewed}
                      aria-label={`Mark ${item.camera} alert at ${formatTs(
                        item.start_time
                      )} as reviewed`}
                      onClick={(e) => {
                        e.stopPropagation();
                        handleMarkReviews([item.id], true);
                      }}
                    >
                      <Check size={10} />
                    </button>
                  </div>
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      fontSize: "var(--text-xs)",
                    }}
                  >
                    <span
                      className="mono"
                      style={{ color: "var(--signal-motion-text)", textTransform: "uppercase" }}
                    >
                      {objs[0]}
                    </span>
                    <span className="mono" style={{ color: "var(--text-primary)" }}>
                      {formatTs(item.start_time)}
                    </span>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {liveLayout !== "map" && (
        <div
          className={`nvr-persistent-stage ${
            liveLayout === "2x2"
              ? `is-layout-grid nvr-matrix-${matrixDensity}`
              : liveLayout === "single"
              ? "is-layout-single nvr-matrix-single"
              : liveLayout === "spotlight"
              ? "is-layout-spotlight nvr-focus-layout"
              : "is-layout-focus nvr-focus-layout"
          }`}
          style={resolvePersistentStageContainerStyle(
            liveLayout,
            stageDescriptors,
            matrixDensity,
            resolveAdaptiveMatrixGridStyle(
              visibleMatrixCams.length,
              matrixDensity
            )
          )}
        >
          {stageDescriptors.map((descriptor) => {
            const cam = descriptor.camera;
            const realCam = resolveRealCameraSource(
              cam,
              config?.cameras || cameraNames
            );
            const camCfg =
              effectiveCamerasConfig?.[cam] ?? config?.cameras?.[realCam];
            const camStat = stats?.cameras?.[cam] ?? stats?.cameras?.[realCam];
            const act = cameraActivity[cam] ?? cameraActivity[realCam];
            const activeLabels = Array.from(
              new Set((act?.objects || []).map((o) => o.label.toUpperCase()))
            );
            const hasObjects = activeLabels.length > 0;
            const hasMotion = Boolean(
              act?.motion && (act?.max_Area ?? 5000) >= 3500
            );
            const isTimelineCam = descriptor.isTimelineSelected;
            const isTimeShifted = descriptor.renderMode === "vod";
            const isDock = descriptor.role === "dock";
            const isHero = descriptor.role === "hero";
            const isGrid = descriptor.role === "grid";

            const handleTileActivate = (isDoubleClick = false) => {
              const nowMs = Date.now();
              if (nowMs - lastLayoutSwitchMsRef.current < 320) {
                return;
              }
              const action = resolveTileClickAction({
                liveLayout,
                role: descriptor.role,
                camera: cam,
                selectedCamera,
                isDoubleClick,
              });
              triggerMainStreamPrewarmIfNeeded(
                realCam,
                action.shouldPrewarmMainStream
              );
              if (liveLayout === "spotlight" && isDock) {
                const ts = Math.floor(nowMs / 1000);
                lastActiveRef.current[cam] = ts;
                lastSurgeRef.current[cam] = ts;
                setNowTick(ts);
                setLockedSpotlightCam(cam);
              }
              if (action.nextSelectedCamera !== selectedCamera) {
                setSelectedCamera(action.nextSelectedCamera);
              }
              if (action.nextLayout !== liveLayout) {
                lastLayoutSwitchMsRef.current = nowMs;
                setLiveLayout(action.nextLayout);
              } else if (isDock && action.promotedToFocus) {
                lastLayoutSwitchMsRef.current = nowMs;
              }
            };

            return (
              <div
                key={cam}
                data-role={descriptor.role}
                className={`nvr-cam-tile ${
                  isTimeShifted ? "has-native-controls" : ""
                } ${
                  isGrid && isTimelineCam && visibleMatrixCams.length > 1
                    ? "is-timeline-focused"
                    : ""
                }`}
                style={resolvePersistentTileStyle(liveLayout, descriptor)}
                onPointerEnter={
                  isDock || isGrid ? () => setHoveredDockCam(cam) : undefined
                }
                onPointerLeave={
                  isDock || isGrid
                    ? () => setHoveredDockCam((c) => (c === cam ? null : c))
                    : undefined
                }
                onDoubleClick={() => handleTileActivate(true)}
              >
                {isGrid && isTimelineCam && visibleMatrixCams.length > 1 && (
                  <div className="nvr-timeline-reticle" aria-hidden="true">
                    <span className="nvr-reticle-corner tl" />
                    <span className="nvr-reticle-corner tr" />
                    <span className="nvr-reticle-corner bl" />
                    <span className="nvr-reticle-corner br" />
                    <span className="nvr-timeline-focus-tab mono">
                      ▼ TIMELINE · {cam.toUpperCase()}
                    </span>
                  </div>
                )}

                {descriptor.renderMode === "vod" && timeShiftTs !== null ? (
                  <VodHlsPlayer
                    camera={realCam}
                    startTime={timeShiftTs}
                    endTime={timeShiftTs + 180}
                    playbackRate={playbackSpeed}
                  />
                ) : descriptor.renderMode === "snapshot" ? (
                  <img
                    className="nvr-cam-poster"
                    src={`/api/${encodeURIComponent(realCam)}/latest.webp?height=360&cache=${nowTick}`}
                    alt={`${cam} dock snapshot`}
                    loading="lazy"
                    onClick={() => handleTileActivate(false)}
                  />
                ) : (
                  <LiveMsePlayer
                    camera={cam}
                    useMainStream={descriptor.useMainStream}
                    cameraConfig={camCfg}
                    showVectors={showVectors}
                    onClick={
                      isHero ? undefined : () => handleTileActivate(false)
                    }
                  />
                )}

                {isDock ? (
                  <div className={`nvr-cam-hud pos-${hudPosition}`}>
                    <span
                      className={`nvr-badge ${
                        liveLayout === "spotlight" ? "mono" : "live"
                      }`}
                    >
                      {cam.toUpperCase()}
                      {liveLayout === "spotlight"
                        ? ` · ${
                            descriptor.renderMode === "mse"
                              ? "STANDBY"
                              : "SNAPSHOT"
                          }`
                        : ""}
                    </span>
                    {liveLayout !== "spotlight" && (
                      <span className="nvr-badge">
                        {descriptor.renderMode === "mse"
                          ? `${camStat?.camera_fps?.toFixed(1) || "5.0"} FPS`
                          : "WEBP"}
                      </span>
                    )}
                    {Boolean(act?.night_mode || camStat?.night_mode) && (
                      <span className="nvr-badge mono">IR NIGHT</span>
                    )}
                  </div>
                ) : (
                  <div className={`nvr-cam-hud pos-${hudPosition}`}>
                    <span
                      className={`nvr-badge ${
                        isTimeShifted
                          ? "rewind"
                          : descriptor.isSpotlightHeld
                          ? "alert"
                          : "live"
                      }`}
                    >
                      <span
                        className={`nvr-dot ${
                          isTimeShifted || descriptor.isSpotlightHeld
                            ? "motion"
                            : "live"
                        }`}
                      />
                      {liveLayout === "spotlight"
                        ? isTimeShifted && timeShiftTs !== null
                          ? `SPOTLIGHT // ${cam.toUpperCase()} // ${
                              t.live.timeShiftBadge
                            } @ ${formatTs(timeShiftTs)}`
                          : `SPOTLIGHT // ${cam.toUpperCase()}`
                        : isHero
                        ? `${cam.toUpperCase()} // ${
                            isTimeShifted && timeShiftTs !== null
                              ? `${t.live.timeShiftBadge} @ ${formatTs(
                                  timeShiftTs
                                )}`
                              : t.live.liveBadge
                          }`
                        : `${cam.toUpperCase()}${
                            isTimeShifted && timeShiftTs !== null
                              ? ` @ ${formatTs(timeShiftTs)}`
                              : ""
                          }`}
                    </span>
                    {isGrid && isTimelineCam && visibleMatrixCams.length > 1 && (
                      <span
                        className="nvr-badge mono"
                        style={{
                          borderColor: "var(--signal-accent)",
                          color: "var(--signal-accent-text)",
                        }}
                        title="Active camera bound to the bottom Timeline scrubber"
                      >
                        ● TIMELINE
                      </span>
                    )}
                    <span className="nvr-badge">
                      {isHero
                        ? `${camStat?.camera_fps?.toFixed(1) || "5.0"} FPS · AI ${
                            camStat?.detection_fps?.toFixed(1) || "0.0"
                          } FPS`
                        : `${camCfg?.detect?.width || 1280}×${
                            camCfg?.detect?.height || 720
                          } · ${camStat?.camera_fps?.toFixed(1) || "5.0"} FPS`}
                    </span>
                    {hasObjects && (
                      <span className="nvr-badge alert">
                        {activeLabels.join(", ")}
                      </span>
                    )}
                    {hasMotion && !hasObjects && (
                      <span className="nvr-badge motion">
                        {t.live.motionActive}
                      </span>
                    )}
                    {Boolean(act?.tamper) && (
                      <span
                        className="nvr-badge alert mono"
                        title="Camera tamper / blind-feed occlusion detected"
                      >
                        TAMPER
                      </span>
                    )}
                    {Boolean(act?.night_mode || camStat?.night_mode) && (
                      <span
                        className="nvr-badge mono"
                        title="Infrared (IR) monochrome night profile active — spider/insect flash filter engaged"
                      >
                        IR NIGHT
                      </span>
                    )}
                    {resolveCameraRoiStateBadges(camCfg, act, camStat).map(
                      (badge) => (
                        <span
                          key={`roi-${badge.roiName}`}
                          data-testid={
                            liveLayout === "spotlight"
                              ? `roi-badge-spotlight-${cam}-${badge.roiName}`
                              : isHero
                              ? `roi-badge-focus-${cam}-${badge.roiName}`
                              : `roi-badge-${cam}-${badge.roiName}`
                          }
                          className={`nvr-badge mono ${
                            badge.isAnomaly ? "alert" : ""
                          }`}
                          style={
                            badge.isAnomaly
                              ? {
                                  color: "var(--signal-alert-text)",
                                  borderColor: "var(--signal-alert)",
                                }
                              : undefined
                          }
                          title={
                            badge.isAnomaly
                              ? `Anomalous ROI state (${badge.label}) — alert after ${badge.anomalyTimeoutSec}s`
                              : `ROI state (${badge.label})`
                          }
                        >
                          {badge.label}
                        </span>
                      )
                    )}
                    {isGrid && (
                      <>
                        <span className="nvr-badge">
                          AI {camStat?.detection_fps?.toFixed(1) || "0.0"} FPS
                        </span>
                        {(
                          [
                            [
                              "enabled",
                              "EN",
                              act?.config?.enabled ?? act?.enabled ?? true,
                            ],
                            ["detect", "DET", act?.config?.detect ?? true],
                            ["record", "REC", act?.config?.record ?? true],
                            ["alerts", "ALR", act?.config?.alerts ?? true],
                          ] as Array<
                            [
                              "enabled" | "detect" | "record" | "alerts",
                              string,
                              boolean,
                            ]
                          >
                        ).map(([flagKey, shortLbl, isOn]) => (
                          <button
                            key={flagKey}
                            type="button"
                            className={`nvr-btn nvr-tile-action mono ${
                              isOn ? "active" : ""
                            }`}
                            style={{
                              height: 22,
                              padding: "0 5px",
                              fontSize: "var(--text-xs)",
                            }}
                            title={`Toggle ${flagKey} at runtime without server restart`}
                            onClick={(e) => {
                              e.stopPropagation();
                              fetch(
                                `/api/${encodeURIComponent(realCam)}/state`,
                                {
                                  method: "POST",
                                  headers: {
                                    "Content-Type": "application/json",
                                  },
                                  credentials: "include",
                                  body: JSON.stringify({ [flagKey]: !isOn }),
                                }
                              ).catch(() => {});
                            }}
                          >
                            {shortLbl}:{isOn ? "ON" : "OFF"}
                          </button>
                        ))}
                        <button
                          className="nvr-btn nvr-tile-action mono"
                          style={{
                            height: 22,
                            padding: "0 6px",
                            fontSize: "var(--text-xs)",
                          }}
                          onClick={(e) => {
                            e.stopPropagation();
                            setSelectedCamera(cam);
                            setTimeShiftTs(Math.floor(Date.now() / 1000) - 30);
                          }}
                        >
                          {t.live.rewind30s}
                        </button>
                        {isTimeShifted && (
                          <button
                            className="nvr-btn success mono"
                            style={{
                              height: 22,
                              padding: "0 6px",
                              fontSize: "var(--text-xs)",
                            }}
                            onClick={(e) => {
                              e.stopPropagation();
                              setTimeShiftTs(null);
                            }}
                          >
                            LIVE
                          </button>
                        )}
                        <button
                          className="nvr-btn nvr-tile-action"
                          style={{
                            height: 22,
                            padding: "0 6px",
                            fontSize: "var(--text-xs)",
                          }}
                          onClick={(e) => {
                            e.stopPropagation();
                            triggerMainStreamPrewarmIfNeeded(realCam, true);
                            setSelectedCamera(cam);
                            setLiveLayout("focus");
                          }}
                        >
                          Expand
                        </button>
                      </>
                    )}
                    {isHero && liveLayout === "spotlight" && !isTimeShifted && (
                      <>
                        <button
                          type="button"
                          data-testid="spotlight-hero-rewind30s"
                          className="nvr-btn nvr-tile-action mono"
                          style={{
                            height: 22,
                            padding: "0 6px",
                            fontSize: "var(--text-xs)",
                          }}
                          onClick={(e) => {
                            e.stopPropagation();
                            setLockedSpotlightCam(cam);
                            setSelectedCamera(cam);
                            setTimeShiftTs(Math.floor(Date.now() / 1000) - 30);
                          }}
                        >
                          {t.live.rewind30s}
                        </button>
                        <button
                          type="button"
                          data-testid="spotlight-hero-focus"
                          className="nvr-btn nvr-tile-action mono"
                          style={{
                            height: 22,
                            padding: "0 6px",
                            fontSize: "var(--text-xs)",
                          }}
                          onClick={(e) => {
                            e.stopPropagation();
                            triggerMainStreamPrewarmIfNeeded(realCam, true);
                            setSelectedCamera(cam);
                            setLiveLayout("focus");
                          }}
                        >
                          {t.live.focusCameraBtn}
                        </button>
                      </>
                    )}
                    {isHero && isTimeShifted ? (
                      <button
                        className="nvr-btn success"
                        onClick={() => setTimeShiftTs(null)}
                      >
                        <Play size={12} />
                        {t.live.returnToLive}
                      </button>
                    ) : !isTimeShifted ? (
                      <PushToTalkButton
                        key={`ptt-${realCam}-${pttResetEpoch}`}
                        camera={realCam}
                        height={22}
                        onStateChange={(state) => {
                          if (state === "connecting" || state === "talking") {
                            activePttCamerasRef.current.add(realCam);
                          } else {
                            activePttCamerasRef.current.delete(realCam);
                          }
                        }}
                      />
                    ) : null}
                    {isGrid && (
                      <a
                        href={`/api/${encodeURIComponent(realCam)}/latest.jpg`}
                        download={`${cam}-snapshot.jpg`}
                        className="nvr-btn nvr-tile-action"
                        style={{
                          height: 22,
                          padding: "0 6px",
                          fontSize: "var(--text-xs)",
                          textDecoration: "none",
                        }}
                        onClick={(e) => e.stopPropagation()}
                        title={t.live.snapshot}
                        aria-label={`Download ${cam} snapshot`}
                      >
                        <Camera size={11} />
                      </a>
                    )}
                    <TacticalPtzHudOverlay
                      camera={realCam}
                      cameraConfig={camCfg}
                      ptzInfo={ptzInfoByCam[realCam]}
                      onPtzUpdate={handlePtzUpdate}
                    />
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}

      {liveLayout === "map" && (
        <div
          className="nvr-focus-layout"
          style={{ display: "grid", gridTemplateColumns: "1fr 370px", gap: 8, flex: 1, minHeight: 0 }}
        >
          <div
            className="nvr-panel"
            style={{
              position: "relative",
              display: "flex",
              flexDirection: "column",
              background: "var(--bg-deep)",
              border: "1px solid var(--border-hairline)",
              overflow: "hidden",
            }}
          >
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                padding: "6px 10px",
                borderBottom: "1px solid var(--border-hairline)",
                background: "var(--bg-panel)",
                gap: 8,
                flexWrap: "wrap",
              }}
            >
              <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700 }}>
                TACTICAL FLOORPLAN // SECTOR FOV CONES &amp; LIVE ACTIVITY PULSE
              </span>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <label
                  className="mono"
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: 4,
                    fontSize: "var(--text-xs)",
                    color: "var(--text-secondary)",
                  }}
                  title="Total real-world floorplan scale in meters (Width × Height)"
                >
                  <span>SCALE:</span>
                  <input
                    type="number"
                    className="nvr-input mono"
                    aria-label="Floorplan total width in meters"
                    min={2}
                    max={500}
                    step={1}
                    style={{ height: 24, width: 52, fontSize: "var(--text-xs)", padding: "0 4px" }}
                    value={floorplanWidthM}
                    onChange={(e) => {
                      const v = Math.max(2, Math.min(500, Number(e.target.value) || 20));
                      setFloorplanWidthM(v);
                      setMapEditMode(true);
                    }}
                  />
                  <span>m ×</span>
                  <input
                    type="number"
                    className="nvr-input mono"
                    aria-label="Floorplan total height in meters"
                    min={2}
                    max={500}
                    step={1}
                    style={{ height: 24, width: 52, fontSize: "var(--text-xs)", padding: "0 4px" }}
                    value={floorplanHeightM}
                    onChange={(e) => {
                      const v = Math.max(2, Math.min(500, Number(e.target.value) || 12));
                      setFloorplanHeightM(v);
                      setMapEditMode(true);
                    }}
                  />
                  <span>m</span>
                </label>

                <button
                  type="button"
                  className={`nvr-btn mono ${showMeasurements ? "active" : ""}`}
                  style={{ height: 24, fontSize: "var(--text-xs)" }}
                  onClick={() => setShowMeasurements((v) => !v)}
                  title="Toggle metric dimensions (meters & m²) on rooms and walls"
                >
                  Medidas (m)
                </button>

                <input
                  ref={floorplanFileInputRef}
                  type="file"
                  accept="image/png,image/jpeg,image/webp,image/svg+xml"
                  style={{ display: "none" }}
                  onChange={handleFloorplanFileUpload}
                />
                <button
                  type="button"
                  className="nvr-btn mono"
                  style={{ height: 24, fontSize: "var(--text-xs)" }}
                  onClick={() => floorplanFileInputRef.current?.click()}
                  title="Upload PNG, SVG, WebP, or JPEG floorplan background"
                >
                  Upload Floorplan
                </button>
                {mapFloorplanUrl && (
                  <button
                    type="button"
                    className="nvr-btn mono"
                    style={{ height: 24, fontSize: "var(--text-xs)" }}
                    onClick={handleClearFloorplanImage}
                    title="Clear uploaded or linked floorplan image"
                  >
                    Clear Image
                  </button>
                )}
                <input
                  className="nvr-input mono"
                  style={{ height: 24, width: 150, fontSize: "var(--text-xs)" }}
                  placeholder="Floorplan SVG/PNG URL"
                  value={
                    mapFloorplanUrl.startsWith("data:")
                      ? "[Uploaded Floorplan Image]"
                      : mapFloorplanUrl
                  }
                  onChange={(e) => {
                    if (e.target.value === "[Uploaded Floorplan Image]") return;
                    const cleanUrl = sanitizeFloorplanUrl(e.target.value);
                    setMapFloorplanUrl(cleanUrl);
                    try {
                      if (cleanUrl) {
                        window.localStorage?.setItem(
                          "ninjanvr_floorplan_url",
                          cleanUrl
                        );
                      } else {
                        window.localStorage?.removeItem("ninjanvr_floorplan_url");
                      }
                    } catch {
                      // ignore
                    }
                  }}
                />
                <label
                  className="mono"
                  style={{
                    display: "inline-flex",
                    alignItems: "center",
                    gap: 4,
                    fontSize: "var(--text-xs)",
                    color: "var(--text-secondary)",
                  }}
                  title="Floorplan background & room fill opacity (10%..100%)"
                >
                  <span>OPACITY {Math.round(floorplanOpacity * 100)}%</span>
                  <input
                    type="range"
                    min={10}
                    max={100}
                    value={Math.round(floorplanOpacity * 100)}
                    style={{ width: 60 }}
                    onChange={(e) => {
                      const nextOp = Number(e.target.value) / 100;
                      setFloorplanOpacity(nextOp);
                      setMapEditMode(true);
                    }}
                  />
                </label>
                <button
                  type="button"
                  className={`nvr-btn mono ${mapEditMode ? "active" : ""}`}
                  style={{ height: 24, fontSize: "var(--text-xs)" }}
                  onClick={() => {
                    setMapEditMode((v) => !v);
                    setWallStartPoint(null);
                    setWallPreviewPoint(null);
                    setRoomDraft(null);
                    setDraggingRoom(null);
                    setResizingRoom(null);
                    setDraggingWall(null);
                  }}
                >
                  {mapEditMode ? "Lock Map" : "Edit Map"}
                </button>
                {mapEditMode && (
                  <button
                    type="button"
                    className="nvr-btn primary mono"
                    style={{ height: 24, fontSize: "var(--text-xs)" }}
                    onClick={handleSaveMapPlacements}
                  >
                    {mapSaveStatus || "Save Placement"}
                  </button>
                )}
              </div>
            </div>

            {mapEditMode && (
              <div
                style={{
                  display: "flex",
                  flexDirection: "column",
                  borderBottom: "1px solid var(--border-hairline)",
                  background: "var(--bg-canvas)",
                }}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    padding: "5px 10px",
                    gap: 8,
                    flexWrap: "wrap",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                    <span
                      className="mono"
                      style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                    >
                      STUDIO TOOL:
                    </span>
                    <button
                      type="button"
                      className={`nvr-btn mono ${mapTool === "select" ? "active" : ""}`}
                      style={{ height: 24, fontSize: "var(--text-xs)" }}
                      onClick={() => {
                        setMapTool("select");
                        setWallStartPoint(null);
                        setWallPreviewPoint(null);
                        setRoomDraft(null);
                      }}
                    >
                      Select / Move / Resize
                    </button>
                    <button
                      type="button"
                      className={`nvr-btn mono ${mapTool === "draw_room" ? "active" : ""}`}
                      style={{ height: 24, fontSize: "var(--text-xs)" }}
                      onClick={() => {
                        setMapTool("draw_room");
                        setWallStartPoint(null);
                        setWallPreviewPoint(null);
                      }}
                    >
                      Draw Room (Rect)
                    </button>
                    <button
                      type="button"
                      className={`nvr-btn mono ${
                        mapTool === "draw_wall" && wallKindDraft === "wall" ? "active" : ""
                      }`}
                      style={{ height: 24, fontSize: "var(--text-xs)" }}
                      onClick={() => {
                        setMapTool("draw_wall");
                        setWallKindDraft("wall");
                        setWallStartPoint(null);
                        setWallPreviewPoint(null);
                      }}
                    >
                      Draw Wall
                    </button>
                    <button
                      type="button"
                      className={`nvr-btn mono ${
                        mapTool === "draw_wall" && wallKindDraft === "door" ? "active" : ""
                      }`}
                      style={{ height: 24, fontSize: "var(--text-xs)" }}
                      onClick={() => {
                        setMapTool("draw_wall");
                        setWallKindDraft("door");
                        setWallStartPoint(null);
                        setWallPreviewPoint(null);
                      }}
                    >
                      Draw Door
                    </button>
                    <button
                      type="button"
                      className={`nvr-btn mono ${
                        mapTool === "draw_wall" && wallKindDraft === "window" ? "active" : ""
                      }`}
                      style={{ height: 24, fontSize: "var(--text-xs)" }}
                      onClick={() => {
                        setMapTool("draw_wall");
                        setWallKindDraft("window");
                        setWallStartPoint(null);
                        setWallPreviewPoint(null);
                      }}
                    >
                      Draw Window
                    </button>

                    <span className="nvr-divider" aria-hidden="true" />

                    <label
                      className="mono"
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 4,
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                      }}
                      title="Grid snap precision in meters"
                    >
                      <span>SNAP:</span>
                      <select
                        className="nvr-select mono"
                        aria-label="Floorplan grid snap step"
                        style={{ height: 24, fontSize: "var(--text-xs)", padding: "0 4px" }}
                        value={String(snapStep)}
                        onChange={(e) => setSnapStep(Number(e.target.value))}
                      >
                        <option value="0">Free (1cm)</option>
                        <option value="0.005">0.10m (Fine)</option>
                        <option value="0.0125">0.25m</option>
                        <option value="0.025">0.50m (Grid)</option>
                      </select>
                    </label>

                    <button
                      type="button"
                      className={`nvr-btn mono ${orthoSnap ? "active" : ""}`}
                      style={{ height: 24, fontSize: "var(--text-xs)" }}
                      onClick={() => setOrthoSnap((v) => !v)}
                      title="Snap wall angles to 0° / 45° / 90° and snap endpoints to corners"
                    >
                      Ortho 90°
                    </button>
                  </div>

                  <div
                    className="mono"
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 8,
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                    }}
                  >
                    {cursorNorm && (
                      <span
                        className="nvr-badge mono"
                        style={{ height: 22, padding: "0 6px" }}
                      >
                        X: {(cursorNorm.x * floorplanWidthM).toFixed(2)}m · Y:{" "}
                        {(cursorNorm.y * floorplanHeightM).toFixed(2)}m
                      </span>
                    )}
                    {mapTool === "select" && (
                      <span>
                        Click room/wall to edit meters · Drag room/wall to move · Drag handles to resize
                      </span>
                    )}
                    {mapTool === "draw_room" && (
                      <span>
                        Click &amp; drag anywhere on canvas to draw room with live dimensions (m)
                      </span>
                    )}
                    {mapTool === "draw_wall" && (
                      <span>
                        {wallStartPoint
                          ? "Click end point (or release drag) to finish wall segment"
                          : "Click & drag (or click two points) to draw wall / door with meter readout"}
                      </span>
                    )}
                  </div>
                </div>

                {/* Quick Metric Editor Bar for Selected Room / Selected Wall / Quick Wall Creation */}
                {(() => {
                  const selRoom = floorplanRooms.find((r) => r.id === selectedRoomId);
                  const selWall =
                    selectedWallIdx !== null ? floorplanWalls[selectedWallIdx] : null;

                  if (selRoom) {
                    const rmMetrics = computeRoomMetrics(
                      selRoom,
                      floorplanWidthM,
                      floorplanHeightM
                    );
                    return (
                      <div
                        data-testid="floorplan-quick-room-bar"
                        className="mono"
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: 8,
                          padding: "5px 10px",
                          borderTop: "1px solid var(--border-hairline)",
                          background: "var(--bg-subcard)",
                          fontSize: "var(--text-xs)",
                          flexWrap: "wrap",
                        }}
                      >
                        <span style={{ fontWeight: 700, color: "var(--signal-accent-text)" }}>
                          ROOM SELECTED:
                        </span>
                        <input
                          className="nvr-input mono"
                          aria-label="Selected room label"
                          style={{ height: 24, width: 115, fontSize: "var(--text-xs)" }}
                          value={selRoom.label}
                          onChange={(e) => {
                            const nextLabel = e.target.value;
                            setFloorplanRooms((prev) =>
                              prev.map((r) =>
                                r.id === selRoom.id ? { ...r, label: nextLabel } : r
                              )
                            );
                          }}
                        />
                        <label style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                          <span>Largura W (m):</span>
                          <input
                            type="number"
                            className="nvr-input mono"
                            aria-label="Selected room width in meters"
                            step={0.1}
                            min={0.3}
                            max={floorplanWidthM}
                            style={{ height: 24, width: 64, fontSize: "var(--text-xs)", padding: "0 4px" }}
                            value={rmMetrics.widthM}
                            onChange={(e) => {
                              const nextW = Math.max(0.2, Number(e.target.value) || 0.5);
                              setFloorplanRooms((prev) =>
                                prev.map((r) =>
                                  r.id === selRoom.id
                                    ? resizeRoomByMeters(
                                        r,
                                        nextW,
                                        rmMetrics.heightM,
                                        floorplanWidthM,
                                        floorplanHeightM
                                      )
                                    : r
                                )
                              );
                            }}
                          />
                        </label>
                        <label style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                          <span>Comprimento H (m):</span>
                          <input
                            type="number"
                            className="nvr-input mono"
                            aria-label="Selected room height in meters"
                            step={0.1}
                            min={0.3}
                            max={floorplanHeightM}
                            style={{ height: 24, width: 64, fontSize: "var(--text-xs)", padding: "0 4px" }}
                            value={rmMetrics.heightM}
                            onChange={(e) => {
                              const nextH = Math.max(0.2, Number(e.target.value) || 0.5);
                              setFloorplanRooms((prev) =>
                                prev.map((r) =>
                                  r.id === selRoom.id
                                    ? resizeRoomByMeters(
                                        r,
                                        rmMetrics.widthM,
                                        nextH,
                                        floorplanWidthM,
                                        floorplanHeightM
                                      )
                                    : r
                                )
                              );
                            }}
                          />
                        </label>
                        <span className="nvr-badge mono" style={{ height: 22, padding: "0 6px" }}>
                          {rmMetrics.areaSqM.toFixed(2)} m²
                        </span>
                        <div style={{ display: "inline-flex", alignItems: "center", gap: 3 }}>
                          {ROOM_COLOR_PALETTE.map((col) => (
                            <button
                              key={col}
                              type="button"
                              aria-label={`Set room color ${col}`}
                              onClick={() =>
                                setFloorplanRooms((prev) =>
                                  prev.map((r) =>
                                    r.id === selRoom.id ? { ...r, color: col } : r
                                  )
                                )
                              }
                              style={{
                                width: 16,
                                height: 16,
                                borderRadius: 3,
                                background: col,
                                border:
                                  (selRoom.color || "#0ea5e9") === col
                                    ? "2px solid #ffffff"
                                    : "1px solid var(--border-strong)",
                                cursor: "pointer",
                              }}
                            />
                          ))}
                        </div>
                        <button
                          type="button"
                          className="nvr-btn mono"
                          style={{ height: 24, fontSize: "var(--text-xs)" }}
                          title="Generate 4 individual editable walls around this room"
                          onClick={() => {
                            const newWalls = extractRoomBoundaryWalls(selRoom);
                            setFloorplanWalls((prev) => [...prev, ...newWalls]);
                          }}
                        >
                          + 4 Walls
                        </button>
                        <button
                          type="button"
                          className="nvr-btn mono"
                          style={{ height: 24, fontSize: "var(--text-xs)" }}
                          title="Duplicate room"
                          onClick={() => {
                            const dupId = `room_${Date.now()}_${floorplanRooms.length + 1}`;
                            const dupRoom: FloorplanRoom = {
                              ...selRoom,
                              id: dupId,
                              label: `${selRoom.label} 2`,
                              x: Number(Math.min(1 - selRoom.w, selRoom.x + 0.04).toFixed(3)),
                              y: Number(Math.min(1 - selRoom.h, selRoom.y + 0.04).toFixed(3)),
                            };
                            setFloorplanRooms((prev) => [...prev, dupRoom]);
                            setSelectedRoomId(dupId);
                          }}
                        >
                          Duplicate
                        </button>
                        <button
                          type="button"
                          className="nvr-btn danger mono"
                          style={{ height: 24, fontSize: "var(--text-xs)" }}
                          onClick={() => {
                            setFloorplanRooms((prev) =>
                              prev.filter((r) => r.id !== selRoom.id)
                            );
                            setSelectedRoomId(null);
                          }}
                        >
                          Delete Room
                        </button>
                      </div>
                    );
                  }

                  if (selWall && selectedWallIdx !== null) {
                    const wMetrics = computeWallMetrics(
                      selWall.points,
                      floorplanWidthM,
                      floorplanHeightM
                    );
                    const wallIdx = selectedWallIdx;
                    return (
                      <div
                        data-testid="floorplan-quick-wall-bar"
                        className="mono"
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: 8,
                          padding: "5px 10px",
                          borderTop: "1px solid var(--border-hairline)",
                          background: "var(--bg-subcard)",
                          fontSize: "var(--text-xs)",
                          flexWrap: "wrap",
                        }}
                      >
                        <span style={{ fontWeight: 700, color: "var(--signal-accent-text)" }}>
                          WALL #{wallIdx + 1}:
                        </span>
                        <input
                          className="nvr-input mono"
                          placeholder={`Wall #${wallIdx + 1}`}
                          aria-label="Selected wall label"
                          style={{ height: 24, width: 100, fontSize: "var(--text-xs)" }}
                          value={selWall.label || ""}
                          onChange={(e) => {
                            const val = e.target.value;
                            setFloorplanWalls((prev) =>
                              prev.map((w, idx) =>
                                idx === wallIdx ? { ...w, label: val } : w
                              )
                            );
                          }}
                        />
                        <select
                          className="nvr-select mono"
                          aria-label="Selected wall kind"
                          style={{ height: 24, fontSize: "var(--text-xs)", padding: "0 4px" }}
                          value={selWall.kind || "wall"}
                          onChange={(e) => {
                            const nextKind = e.target.value;
                            setFloorplanWalls((prev) =>
                              prev.map((w, idx) =>
                                idx === wallIdx ? { ...w, kind: nextKind } : w
                              )
                            );
                          }}
                        >
                          <option value="wall">Wall (Parede)</option>
                          <option value="door">Door (Porta)</option>
                          <option value="window">Window (Janela)</option>
                        </select>
                        <label style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                          <span>Medida (m):</span>
                          <input
                            type="number"
                            className="nvr-input mono"
                            aria-label="Selected wall length in meters"
                            step={0.1}
                            min={0.1}
                            max={Math.hypot(floorplanWidthM, floorplanHeightM)}
                            style={{ height: 24, width: 68, fontSize: "var(--text-xs)", padding: "0 4px" }}
                            value={wMetrics.lengthM}
                            onChange={(e) => {
                              const targetM = Math.max(0.1, Number(e.target.value) || 0.5);
                              setFloorplanWalls((prev) =>
                                prev.map((w, idx) =>
                                  idx === wallIdx
                                    ? {
                                        ...w,
                                        points: resizeWallToLengthMeters(
                                          w.points,
                                          targetM,
                                          floorplanWidthM,
                                          floorplanHeightM
                                        ),
                                      }
                                    : w
                                )
                              );
                            }}
                          />
                        </label>
                        {[1, 2, 3, 4, 5].map((presetM) => (
                          <button
                            key={presetM}
                            type="button"
                            className="nvr-btn mono"
                            style={{ height: 22, padding: "0 5px", fontSize: "var(--text-xs)" }}
                            title={`Set wall length to ${presetM}.0m`}
                            onClick={() => {
                              setFloorplanWalls((prev) =>
                                prev.map((w, idx) =>
                                  idx === wallIdx
                                    ? {
                                        ...w,
                                        points: resizeWallToLengthMeters(
                                          w.points,
                                          presetM,
                                          floorplanWidthM,
                                          floorplanHeightM
                                        ),
                                      }
                                    : w
                                )
                              );
                            }}
                          >
                            {presetM}m
                          </button>
                        ))}
                        <label style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                          <span>Ângulo (°):</span>
                          <input
                            type="number"
                            className="nvr-input mono"
                            aria-label="Selected wall angle in degrees"
                            step={15}
                            min={0}
                            max={360}
                            style={{ height: 24, width: 58, fontSize: "var(--text-xs)", padding: "0 4px" }}
                            value={Math.round(wMetrics.angleDeg)}
                            onChange={(e) => {
                              const deg = Number(e.target.value) || 0;
                              setFloorplanWalls((prev) =>
                                prev.map((w, idx) =>
                                  idx === wallIdx
                                    ? {
                                        ...w,
                                        points: rotateWallToAngleDegrees(
                                          w.points,
                                          deg,
                                          floorplanWidthM,
                                          floorplanHeightM
                                        ),
                                      }
                                    : w
                                )
                              );
                            }}
                          />
                        </label>
                        <button
                          type="button"
                          className="nvr-btn mono"
                          style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
                          onClick={() =>
                            setFloorplanWalls((prev) =>
                              prev.map((w, idx) =>
                                idx === wallIdx
                                  ? {
                                      ...w,
                                      points: rotateWallToAngleDegrees(
                                        w.points,
                                        0,
                                        floorplanWidthM,
                                        floorplanHeightM
                                      ),
                                    }
                                  : w
                              )
                            )
                          }
                        >
                          0° Horiz
                        </button>
                        <button
                          type="button"
                          className="nvr-btn mono"
                          style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
                          onClick={() =>
                            setFloorplanWalls((prev) =>
                              prev.map((w, idx) =>
                                idx === wallIdx
                                  ? {
                                      ...w,
                                      points: rotateWallToAngleDegrees(
                                        w.points,
                                        90,
                                        floorplanWidthM,
                                        floorplanHeightM
                                      ),
                                    }
                                  : w
                              )
                            )
                          }
                        >
                          90° Vert
                        </button>
                        <button
                          type="button"
                          className="nvr-btn danger mono"
                          style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
                          onClick={() => {
                            setFloorplanWalls((prev) =>
                              prev.filter((_, idx) => idx !== wallIdx)
                            );
                            setSelectedWallIdx(null);
                          }}
                        >
                          Delete Wall
                        </button>
                      </div>
                    );
                  }

                  if (mapTool === "draw_wall") {
                    return (
                      <div
                        className="mono"
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: 8,
                          padding: "5px 10px",
                          borderTop: "1px solid var(--border-hairline)",
                          background: "var(--bg-subcard)",
                          fontSize: "var(--text-xs)",
                          flexWrap: "wrap",
                        }}
                      >
                        <span style={{ color: "var(--text-secondary)" }}>
                          QUICK INSERT WALL BY METERS:
                        </span>
                        <input
                          type="number"
                          className="nvr-input mono"
                          aria-label="Quick insert wall length in meters"
                          step={0.1}
                          min={0.2}
                          max={floorplanWidthM}
                          style={{ height: 24, width: 64, fontSize: "var(--text-xs)", padding: "0 4px" }}
                          value={quickWallLenM}
                          onChange={(e) =>
                            setQuickWallLenM(Math.max(0.2, Number(e.target.value) || 1))
                          }
                        />
                        <span>m</span>
                        <button
                          type="button"
                          className="nvr-btn mono"
                          style={{ height: 24, fontSize: "var(--text-xs)" }}
                          onClick={() => {
                            const startX = 0.2;
                            const startY = Number(
                              Math.min(0.85, 0.2 + floorplanWalls.length * 0.05).toFixed(3)
                            );
                            const pts = resizeWallToLengthMeters(
                              [startX, startY, startX + 0.1, startY],
                              quickWallLenM,
                              floorplanWidthM,
                              floorplanHeightM
                            );
                            const newWall: FloorplanWall = {
                              points: pts,
                              kind: wallKindDraft,
                            };
                            setFloorplanWalls((prev) => {
                              const next = [...prev, newWall];
                              setSelectedWallIdx(next.length - 1);
                              return next;
                            });
                            setSelectedRoomId(null);
                          }}
                        >
                          + Insert Horiz ({quickWallLenM}m)
                        </button>
                        <button
                          type="button"
                          className="nvr-btn mono"
                          style={{ height: 24, fontSize: "var(--text-xs)" }}
                          onClick={() => {
                            const startX = Number(
                              Math.min(0.85, 0.2 + floorplanWalls.length * 0.05).toFixed(3)
                            );
                            const startY = 0.2;
                            const pts = resizeWallToLengthMeters(
                              [startX, startY, startX, startY + 0.1],
                              quickWallLenM,
                              floorplanWidthM,
                              floorplanHeightM
                            );
                            const newWall: FloorplanWall = {
                              points: pts,
                              kind: wallKindDraft,
                            };
                            setFloorplanWalls((prev) => {
                              const next = [...prev, newWall];
                              setSelectedWallIdx(next.length - 1);
                              return next;
                            });
                            setSelectedRoomId(null);
                          }}
                        >
                          + Insert Vert ({quickWallLenM}m)
                        </button>
                        {wallStartPoint && (
                          <button
                            type="button"
                            className="nvr-btn mono"
                            style={{ height: 24, fontSize: "var(--text-xs)" }}
                            onClick={() => {
                              setWallStartPoint(null);
                              setWallPreviewPoint(null);
                            }}
                          >
                            Cancel Wall Draft (Esc)
                          </button>
                        )}
                      </div>
                    );
                  }

                  return null;
                })()}
              </div>
            )}

            <svg
              ref={mapSvgRef}
              viewBox="0 0 1000 600"
              style={{
                width: "100%",
                height: "100%",
                flex: 1,
                userSelect: "none",
                touchAction: "none",
                cursor:
                  draggingCam ||
                  draggingAimCam ||
                  draggingRoom ||
                  resizingRoom ||
                  draggingWall
                    ? "grabbing"
                    : mapEditMode && (mapTool === "draw_room" || mapTool === "draw_wall")
                    ? "crosshair"
                    : "default",
                background: mapTheme.floorplanStageBg,
              }}
              onPointerDown={(e) => {
                if (!mapEditMode || !mapSvgRef.current) return;
                const rect = mapSvgRef.current.getBoundingClientRect();
                const pt = mapClientPointToFloorplan(
                  e.clientX,
                  e.clientY,
                  rect,
                  1000,
                  600
                );
                const snappedX = snapFloorplanCoord(pt.x, snapStep);
                const snappedY = snapFloorplanCoord(pt.y, snapStep);

                if (mapTool === "draw_room") {
                  setSelectedRoomId(null);
                  setSelectedWallIdx(null);
                  setRoomDraft({
                    startX: snappedX,
                    startY: snappedY,
                    currentX: snappedX,
                    currentY: snappedY,
                  });
                } else if (mapTool === "draw_wall") {
                  const snappedPt = snapWallEndpoint(
                    wallStartPoint ? wallStartPoint.x : null,
                    wallStartPoint ? wallStartPoint.y : null,
                    pt.x,
                    pt.y,
                    {
                      snapStep,
                      orthoSnap,
                      vertices: snapVertices,
                      floorplanWidthM,
                      floorplanHeightM,
                    }
                  );
                  if (!wallStartPoint) {
                    setWallStartPoint({ x: snappedPt.x, y: snappedPt.y });
                    setWallPreviewPoint({
                      x: snappedPt.x,
                      y: snappedPt.y,
                      snappedToVertex: snappedPt.snappedToVertex,
                    });
                    setSelectedRoomId(null);
                    setSelectedWallIdx(null);
                  } else {
                    if (
                      Math.hypot(
                        snappedPt.x - wallStartPoint.x,
                        snappedPt.y - wallStartPoint.y
                      ) >= 0.01
                    ) {
                      const createdWall: FloorplanWall = {
                        points: [
                          wallStartPoint.x,
                          wallStartPoint.y,
                          snappedPt.x,
                          snappedPt.y,
                        ],
                        kind: wallKindDraft,
                      };
                      setFloorplanWalls((prev) => {
                        const next = [...prev, createdWall];
                        setSelectedWallIdx(next.length - 1);
                        return next;
                      });
                    }
                    setWallStartPoint(null);
                    setWallPreviewPoint(null);
                  }
                } else {
                  setSelectedRoomId(null);
                  setSelectedWallIdx(null);
                }
              }}
              onPointerMove={(e) => {
                if (!mapSvgRef.current) return;
                const rect = mapSvgRef.current.getBoundingClientRect();
                const pt = mapClientPointToFloorplan(
                  e.clientX,
                  e.clientY,
                  rect,
                  1000,
                  600
                );
                const rawX = pt.x;
                const rawY = pt.y;
                if (mapEditMode) {
                  setCursorNorm({ x: rawX, y: rawY });
                }

                if (!mapEditMode) return;

                if (draggingAimCam) {
                  setMapPlacements((prev) => {
                    const current = prev[draggingAimCam] || {
                      x: 0.5,
                      y: 0.5,
                      angle_deg: 0,
                      fov_deg: 90,
                      range: 0.25,
                    };
                    const aim = computeCameraAimFromTip(
                      current.x,
                      current.y,
                      rawX,
                      rawY
                    );
                    return {
                      ...prev,
                      [draggingAimCam]: {
                        ...current,
                        angle_deg: aim.angle_deg,
                        range: aim.range,
                      },
                    };
                  });
                  return;
                }

                if (draggingCam) {
                  const nx = Math.max(0.03, Math.min(0.97, rawX));
                  const ny = Math.max(0.03, Math.min(0.97, rawY));
                  setMapPlacements((prev) => ({
                    ...prev,
                    [draggingCam]: {
                      ...(prev[draggingCam] || {
                        x: nx,
                        y: ny,
                        angle_deg: 0,
                        fov_deg: 90,
                        range: 0.25,
                      }),
                      x: Number(nx.toFixed(3)),
                      y: Number(ny.toFixed(3)),
                    },
                  }));
                  return;
                }

                if (draggingWall) {
                  const { index, handle, startNormX, startNormY, origPoints } =
                    draggingWall;
                  if (handle === "move") {
                    const dx = snapFloorplanCoord(rawX - startNormX, snapStep) - snapFloorplanCoord(0, snapStep);
                    const dy = snapFloorplanCoord(rawY - startNormY, snapStep) - snapFloorplanCoord(0, snapStep);
                    const rawDx = snapStep > 0 ? Math.round((rawX - startNormX) / snapStep) * snapStep : rawX - startNormX;
                    const rawDy = snapStep > 0 ? Math.round((rawY - startNormY) / snapStep) * snapStep : rawY - startNormY;
                    void dx;
                    void dy;
                    const minX = Math.min(origPoints[0], origPoints[2]);
                    const maxX = Math.max(origPoints[0], origPoints[2]);
                    const minY = Math.min(origPoints[1], origPoints[3]);
                    const maxY = Math.max(origPoints[1], origPoints[3]);
                    const clampedDx = Math.max(-minX, Math.min(1 - maxX, rawDx));
                    const clampedDy = Math.max(-minY, Math.min(1 - maxY, rawDy));
                    const nextPts: [number, number, number, number] = [
                      Number((origPoints[0] + clampedDx).toFixed(3)),
                      Number((origPoints[1] + clampedDy).toFixed(3)),
                      Number((origPoints[2] + clampedDx).toFixed(3)),
                      Number((origPoints[3] + clampedDy).toFixed(3)),
                    ];
                    setFloorplanWalls((prev) =>
                      prev.map((w, idx) =>
                        idx === index ? { ...w, points: nextPts } : w
                      )
                    );
                  } else if (handle === "start") {
                    const snapped = snapWallEndpoint(
                      origPoints[2],
                      origPoints[3],
                      rawX,
                      rawY,
                      {
                        snapStep,
                        orthoSnap,
                        vertices: snapVertices.filter(
                          (v) =>
                            Math.hypot(v.x - origPoints[0], v.y - origPoints[1]) >
                            0.005
                        ),
                        floorplanWidthM,
                        floorplanHeightM,
                      }
                    );
                    setFloorplanWalls((prev) =>
                      prev.map((w, idx) =>
                        idx === index
                          ? {
                              ...w,
                              points: [
                                snapped.x,
                                snapped.y,
                                origPoints[2],
                                origPoints[3],
                              ],
                            }
                          : w
                      )
                    );
                  } else if (handle === "end") {
                    const snapped = snapWallEndpoint(
                      origPoints[0],
                      origPoints[1],
                      rawX,
                      rawY,
                      {
                        snapStep,
                        orthoSnap,
                        vertices: snapVertices.filter(
                          (v) =>
                            Math.hypot(v.x - origPoints[2], v.y - origPoints[3]) >
                            0.005
                        ),
                        floorplanWidthM,
                        floorplanHeightM,
                      }
                    );
                    setFloorplanWalls((prev) =>
                      prev.map((w, idx) =>
                        idx === index
                          ? {
                              ...w,
                              points: [
                                origPoints[0],
                                origPoints[1],
                                snapped.x,
                                snapped.y,
                              ],
                            }
                          : w
                      )
                    );
                  }
                  return;
                }

                if (resizingRoom) {
                  const snappedX = snapFloorplanCoord(rawX, snapStep);
                  const snappedY = snapFloorplanCoord(rawY, snapStep);
                  const { origRoom, handle } = resizingRoom;
                  let x1 = origRoom.x;
                  let y1 = origRoom.y;
                  let x2 = origRoom.x + origRoom.w;
                  let y2 = origRoom.y + origRoom.h;

                  if (handle === "nw" || handle === "sw" || handle === "w") {
                    x1 = snappedX;
                  }
                  if (handle === "ne" || handle === "se" || handle === "e") {
                    x2 = snappedX;
                  }
                  if (handle === "nw" || handle === "ne" || handle === "n") {
                    y1 = snappedY;
                  }
                  if (handle === "sw" || handle === "se" || handle === "s") {
                    y2 = snappedY;
                  }

                  const norm = normalizeFloorplanRoomRect(x1, y1, x2, y2, 0.015);
                  setFloorplanRooms((prev) =>
                    prev.map((r) =>
                      r.id === resizingRoom.id ? { ...r, ...norm } : r
                    )
                  );
                  return;
                }

                if (draggingRoom) {
                  const dx = rawX - draggingRoom.startNormX;
                  const dy = rawY - draggingRoom.startNormY;
                  setFloorplanRooms((prev) =>
                    prev.map((r) => {
                      if (r.id !== draggingRoom.id) return r;
                      const nextX = snapFloorplanCoord(
                        Math.max(0, Math.min(1 - r.w, draggingRoom.origX + dx)),
                        snapStep
                      );
                      const nextY = snapFloorplanCoord(
                        Math.max(0, Math.min(1 - r.h, draggingRoom.origY + dy)),
                        snapStep
                      );
                      return {
                        ...r,
                        x: Math.min(1 - r.w, nextX),
                        y: Math.min(1 - r.h, nextY),
                      };
                    })
                  );
                  return;
                }

                if (roomDraft) {
                  const snappedX = snapFloorplanCoord(rawX, snapStep);
                  const snappedY = snapFloorplanCoord(rawY, snapStep);
                  setRoomDraft((prev) =>
                    prev
                      ? { ...prev, currentX: snappedX, currentY: snappedY }
                      : null
                  );
                  return;
                }

                if (mapTool === "draw_wall" && wallStartPoint) {
                  const snapped = snapWallEndpoint(
                    wallStartPoint.x,
                    wallStartPoint.y,
                    rawX,
                    rawY,
                    {
                      snapStep,
                      orthoSnap,
                      vertices: snapVertices,
                      floorplanWidthM,
                      floorplanHeightM,
                    }
                  );
                  setWallPreviewPoint({
                    x: snapped.x,
                    y: snapped.y,
                    snappedToVertex: snapped.snappedToVertex,
                  });
                }
              }}
              onPointerUp={() => {
                if (roomDraft) {
                  const dragDist = Math.hypot(
                    roomDraft.currentX - roomDraft.startX,
                    roomDraft.currentY - roomDraft.startY
                  );
                  // If the user dragged a rectangle, use exact dragged bounds; if a single click, create a 3.0m x 2.4m room at click
                  const norm =
                    dragDist >= 0.012
                      ? normalizeFloorplanRoomRect(
                          roomDraft.startX,
                          roomDraft.startY,
                          roomDraft.currentX,
                          roomDraft.currentY,
                          0.015
                        )
                      : normalizeFloorplanRoomRect(
                          roomDraft.startX,
                          roomDraft.startY,
                          Math.min(1, roomDraft.startX + 3.0 / floorplanWidthM),
                          Math.min(1, roomDraft.startY + 2.4 / floorplanHeightM),
                          0.02
                        );
                  const nextIdx = floorplanRooms.length;
                  const defaultLabel =
                    DEFAULT_ROOM_LABELS[nextIdx % DEFAULT_ROOM_LABELS.length];
                  const newId = `room_${Date.now()}_${nextIdx + 1}`;
                  const createdRoom: FloorplanRoom = {
                    id: newId,
                    label: defaultLabel,
                    x: norm.x,
                    y: norm.y,
                    w: norm.w,
                    h: norm.h,
                    color:
                      ROOM_COLOR_PALETTE[nextIdx % ROOM_COLOR_PALETTE.length],
                  };
                  setFloorplanRooms((prev) => [...prev, createdRoom]);
                  setSelectedRoomId(newId);
                  setSelectedWallIdx(null);
                  setRoomDraft(null);
                } else if (
                  mapTool === "draw_wall" &&
                  wallStartPoint &&
                  wallPreviewPoint
                ) {
                  // Support click-and-drag wall creation: if dragged >= 0.25m, finalize wall on pointerUp!
                  const dragMetrics = computeWallMetrics(
                    [
                      wallStartPoint.x,
                      wallStartPoint.y,
                      wallPreviewPoint.x,
                      wallPreviewPoint.y,
                    ],
                    floorplanWidthM,
                    floorplanHeightM
                  );
                  if (dragMetrics.lengthM >= 0.25) {
                    const createdWall: FloorplanWall = {
                      points: [
                        wallStartPoint.x,
                        wallStartPoint.y,
                        wallPreviewPoint.x,
                        wallPreviewPoint.y,
                      ],
                      kind: wallKindDraft,
                    };
                    setFloorplanWalls((prev) => {
                      const next = [...prev, createdWall];
                      setSelectedWallIdx(next.length - 1);
                      return next;
                    });
                    setSelectedRoomId(null);
                    setWallStartPoint(null);
                    setWallPreviewPoint(null);
                  }
                }
                setDraggingCam(null);
                setDraggingAimCam(null);
                setDraggingRoom(null);
                setResizingRoom(null);
                setDraggingWall(null);
              }}
              onPointerLeave={() => {
                setCursorNorm(null);
                setDraggingCam(null);
                setDraggingAimCam(null);
                setDraggingRoom(null);
                setResizingRoom(null);
                setDraggingWall(null);
              }}
            >
              <defs>
                <pattern id="tacticalGrid" width="50" height="50" patternUnits="userSpaceOnUse">
                  <path
                    d="M 50 0 L 0 0 0 50"
                    fill="none"
                    stroke={mapTheme.floorplanGridStroke}
                    strokeWidth="1"
                  />
                </pattern>
              </defs>
              <rect width="1000" height="600" fill="url(#tacticalGrid)" />
              {sanitizeFloorplanUrl(mapFloorplanUrl) && (
                <image
                  href={sanitizeFloorplanUrl(mapFloorplanUrl)}
                  x="40"
                  y="30"
                  width="920"
                  height="540"
                  preserveAspectRatio="xMidYMid meet"
                  opacity={floorplanOpacity}
                  style={{ pointerEvents: "none" }}
                />
              )}
              <rect
                x="60"
                y="45"
                width="880"
                height="510"
                fill="none"
                stroke={mapTheme.floorplanBorderStroke}
                strokeWidth="1.5"
                strokeDasharray="6 4"
                style={{ pointerEvents: "none" }}
              />
              {showMeasurements && (
                <text
                  x="66"
                  y="38"
                  fill={mapTheme.floorplanRoomLabelFill}
                  opacity="0.65"
                  fontSize="11"
                  fontFamily="JetBrains Mono, monospace"
                  style={{ pointerEvents: "none" }}
                >
                  {`SCALE: ${floorplanWidthM}m × ${floorplanHeightM}m (1 grid = ${(
                    floorplanWidthM / 20
                  ).toFixed(2)}m)`}
                </text>
              )}

              {/* Vector Rooms Layer */}
              {floorplanRooms.map((room) => {
                const rx = room.x * 1000;
                const ry = room.y * 600;
                const rw = room.w * 1000;
                const rh = room.h * 600;
                const isRoomSelected = selectedRoomId === room.id;
                const strokeCol = room.color || "#38bdf8";
                const rmMetrics = computeRoomMetrics(
                  room,
                  floorplanWidthM,
                  floorplanHeightM
                );
                const isInteractiveInSelect =
                  mapEditMode && mapTool === "select";

                return (
                  <g
                    key={room.id}
                    style={{
                      pointerEvents:
                        mapEditMode && mapTool !== "select" ? "none" : "auto",
                    }}
                    onPointerDown={(e) => {
                      if (!isInteractiveInSelect || !mapSvgRef.current) return;
                      e.stopPropagation();
                      setSelectedRoomId(room.id);
                      setSelectedWallIdx(null);
                      const rect = mapSvgRef.current.getBoundingClientRect();
                      const pt = mapClientPointToFloorplan(
                        e.clientX,
                        e.clientY,
                        rect,
                        1000,
                        600
                      );
                      setDraggingRoom({
                        id: room.id,
                        startNormX: pt.x,
                        startNormY: pt.y,
                        origX: room.x,
                        origY: room.y,
                      });
                    }}
                  >
                    <rect
                      x={rx}
                      y={ry}
                      width={rw}
                      height={rh}
                      fill={
                        isRoomSelected
                          ? mapTheme.floorplanRoomSelectedFill
                          : mapTheme.floorplanRoomFill
                      }
                      fillOpacity={floorplanOpacity}
                      stroke={strokeCol}
                      strokeWidth={isRoomSelected ? 2.5 : 1.5}
                      style={{
                        cursor: isInteractiveInSelect ? "move" : "default",
                      }}
                    />
                    <text
                      x={rx + rw / 2}
                      y={ry + rh / 2 - (showMeasurements ? 4 : -4)}
                      textAnchor="middle"
                      fill={mapTheme.floorplanRoomLabelFill}
                      fontSize="12"
                      fontFamily="JetBrains Mono, monospace"
                      fontWeight="700"
                      style={{ pointerEvents: "none" }}
                    >
                      {room.label}
                    </text>
                    {showMeasurements && (
                      <text
                        x={rx + rw / 2}
                        y={ry + rh / 2 + 12}
                        textAnchor="middle"
                        fill={strokeCol}
                        fontSize="11"
                        fontFamily="JetBrains Mono, monospace"
                        fontWeight="600"
                        style={{ pointerEvents: "none" }}
                      >
                        {rw < 155
                          ? `${rmMetrics.widthM.toFixed(2)}×${rmMetrics.heightM.toFixed(2)}m`
                          : `${rmMetrics.widthM.toFixed(2)}m × ${rmMetrics.heightM.toFixed(
                              2
                            )}m (${rmMetrics.areaSqM.toFixed(1)}m²)`}
                      </text>
                    )}
                    {mapEditMode && isRoomSelected && (
                      <>
                        {/* Edge dimension callouts */}
                        <text
                          x={rx + rw / 2}
                          y={Math.max(14, ry - 6)}
                          textAnchor="middle"
                          fill="#38bdf8"
                          fontSize="11"
                          fontFamily="JetBrains Mono, monospace"
                          fontWeight="700"
                          style={{ pointerEvents: "none" }}
                        >
                          {`↔ ${rmMetrics.widthM.toFixed(2)}m`}
                        </text>
                        <text
                          x={Math.max(28, rx - 8)}
                          y={ry + rh / 2 + 4}
                          textAnchor="end"
                          fill="#38bdf8"
                          fontSize="11"
                          fontFamily="JetBrains Mono, monospace"
                          fontWeight="700"
                          style={{ pointerEvents: "none" }}
                        >
                          {`↕ ${rmMetrics.heightM.toFixed(2)}m`}
                        </text>
                        {(
                          [
                            {
                              key: "nw",
                              cx: rx,
                              cy: ry,
                              cursor: "nwse-resize",
                            },
                            {
                              key: "ne",
                              cx: rx + rw,
                              cy: ry,
                              cursor: "nesw-resize",
                            },
                            {
                              key: "sw",
                              cx: rx,
                              cy: ry + rh,
                              cursor: "nesw-resize",
                            },
                            {
                              key: "se",
                              cx: rx + rw,
                              cy: ry + rh,
                              cursor: "nwse-resize",
                            },
                            {
                              key: "n",
                              cx: rx + rw / 2,
                              cy: ry,
                              cursor: "ns-resize",
                            },
                            {
                              key: "s",
                              cx: rx + rw / 2,
                              cy: ry + rh,
                              cursor: "ns-resize",
                            },
                            {
                              key: "w",
                              cx: rx,
                              cy: ry + rh / 2,
                              cursor: "ew-resize",
                            },
                            {
                              key: "e",
                              cx: rx + rw,
                              cy: ry + rh / 2,
                              cursor: "ew-resize",
                            },
                          ] as Array<{
                            key: "nw" | "ne" | "sw" | "se" | "n" | "s" | "e" | "w";
                            cx: number;
                            cy: number;
                            cursor: string;
                          }>
                        ).map((h) => (
                          <rect
                            key={h.key}
                            x={h.cx - 5}
                            y={h.cy - 5}
                            width={10}
                            height={10}
                            fill="#38bdf8"
                            stroke={mapTheme.floorplanCamNodeStroke}
                            strokeWidth={1.5}
                            style={{ cursor: h.cursor }}
                            onPointerDown={(e) => {
                              e.stopPropagation();
                              setResizingRoom({
                                id: room.id,
                                handle: h.key,
                                anchorX: room.x,
                                anchorY: room.y,
                                origRoom: room,
                              });
                            }}
                          />
                        ))}
                      </>
                    )}
                  </g>
                );
              })}

              {/* Live Room Draft Preview with Real-Time Metric Dimensions */}
              {roomDraft && (
                (() => {
                  const norm = normalizeFloorplanRoomRect(
                    roomDraft.startX,
                    roomDraft.startY,
                    roomDraft.currentX,
                    roomDraft.currentY,
                    0.01
                  );
                  const draftMetrics = computeRoomMetrics(
                    norm,
                    floorplanWidthM,
                    floorplanHeightM
                  );
                  const dx = norm.x * 1000;
                  const dy = norm.y * 600;
                  const dw = norm.w * 1000;
                  const dh = norm.h * 600;
                  return (
                    <g style={{ pointerEvents: "none" }}>
                      <rect
                        x={dx}
                        y={dy}
                        width={dw}
                        height={dh}
                        fill="rgba(56, 189, 248, 0.22)"
                        stroke="#38bdf8"
                        strokeWidth="2"
                        strokeDasharray="5 3"
                      />
                      <rect
                        x={dx + dw / 2 - 82}
                        y={dy + dh / 2 - 12}
                        width={164}
                        height={24}
                        rx={4}
                        fill="rgba(7, 10, 15, 0.88)"
                        stroke="#38bdf8"
                        strokeWidth="1"
                      />
                      <text
                        x={dx + dw / 2}
                        y={dy + dh / 2 + 4}
                        textAnchor="middle"
                        fill="#f8fafc"
                        fontSize="11"
                        fontFamily="JetBrains Mono, monospace"
                        fontWeight="700"
                      >
                        {`${draftMetrics.widthM.toFixed(2)}m × ${draftMetrics.heightM.toFixed(
                          2
                        )}m (${draftMetrics.areaSqM.toFixed(1)}m²)`}
                      </text>
                    </g>
                  );
                })()
              )}

              {/* Vector Structural Walls, Doors & Windows Layer */}
              {floorplanWalls.map((wall, wIdx) => {
                const [x1, y1, x2, y2] = wall.points;
                const sx1 = x1 * 1000;
                const sy1 = y1 * 600;
                const sx2 = x2 * 1000;
                const sy2 = y2 * 600;
                const isWallSelected = selectedWallIdx === wIdx;
                const isDoor = wall.kind === "door";
                const isWindow = wall.kind === "window";
                const kindLabel = isDoor ? "Door" : isWindow ? "Window" : "Wall";
                const wMetrics = computeWallMetrics(
                  wall.points,
                  floorplanWidthM,
                  floorplanHeightM
                );
                const midSvgX = (sx1 + sx2) / 2;
                const midSvgY = (sy1 + sy2) / 2;
                const isInteractiveInSelect =
                  mapEditMode && mapTool === "select";

                return (
                  <g
                    key={`wall-${wIdx}`}
                    style={{
                      pointerEvents:
                        mapEditMode && mapTool !== "select" ? "none" : "auto",
                    }}
                  >
                    {/* Wide invisible hit target for easy selection and dragging */}
                    <line
                      x1={sx1}
                      y1={sy1}
                      x2={sx2}
                      y2={sy2}
                      stroke="transparent"
                      strokeWidth="16"
                      strokeLinecap="round"
                      style={{
                        cursor: isInteractiveInSelect ? "move" : "default",
                      }}
                      onPointerDown={(e) => {
                        if (!mapEditMode || !mapSvgRef.current) return;
                        if (mapTool === "select") {
                          e.stopPropagation();
                          setSelectedWallIdx(wIdx);
                          setSelectedRoomId(null);
                          const rect =
                            mapSvgRef.current.getBoundingClientRect();
                          const pt = mapClientPointToFloorplan(
                            e.clientX,
                            e.clientY,
                            rect,
                            1000,
                            600
                          );
                          setDraggingWall({
                            index: wIdx,
                            handle: "move",
                            startNormX: pt.x,
                            startNormY: pt.y,
                            origPoints: [...wall.points] as [
                              number,
                              number,
                              number,
                              number,
                            ],
                          });
                        }
                      }}
                    >
                      <title>
                        {mapEditMode
                          ? `Click to select & edit ${kindLabel} #${wIdx + 1} (${wMetrics.lengthM.toFixed(2)}m)`
                          : `${wall.label || `${kindLabel} #${wIdx + 1}`} (${wMetrics.lengthM.toFixed(
                              2
                            )}m)`}
                      </title>
                    </line>

                    {/* Visible wall / door / window line */}
                    <line
                      x1={sx1}
                      y1={sy1}
                      x2={sx2}
                      y2={sy2}
                      stroke={
                        isWallSelected
                          ? "#38bdf8"
                          : isDoor
                          ? "#f59e0b"
                          : isWindow
                          ? "#38bdf8"
                          : mapTheme.floorplanWallStroke
                      }
                      strokeWidth={
                        isWallSelected ? 6 : isDoor ? 4 : isWindow ? 5.5 : 4.5
                      }
                      strokeDasharray={isDoor ? "8 4" : undefined}
                      strokeLinecap="round"
                      style={{ pointerEvents: "none" }}
                    />
                    {isWindow && (
                      <line
                        x1={sx1}
                        y1={sy1}
                        x2={sx2}
                        y2={sy2}
                        stroke="#e0f2fe"
                        strokeWidth={1.6}
                        strokeDasharray="10 3"
                        strokeLinecap="round"
                        style={{ pointerEvents: "none" }}
                      />
                    )}

                    {/* Endpoint nodes (draggable when selected in edit mode) */}
                    <circle
                      cx={sx1}
                      cy={sy1}
                      r={isWallSelected && mapEditMode ? 6.5 : 3.5}
                      fill={
                        isWallSelected
                          ? "#38bdf8"
                          : mapTheme.floorplanWallNodeFill
                      }
                      stroke={
                        isWallSelected ? "#f8fafc" : "none"
                      }
                      strokeWidth={isWallSelected ? 1.5 : 0}
                      style={{
                        cursor:
                          isInteractiveInSelect && isWallSelected
                            ? "crosshair"
                            : "default",
                      }}
                      onPointerDown={(e) => {
                        if (!isInteractiveInSelect || !mapSvgRef.current) return;
                        e.stopPropagation();
                        setSelectedWallIdx(wIdx);
                        setSelectedRoomId(null);
                        const rect = mapSvgRef.current.getBoundingClientRect();
                        const pt = mapClientPointToFloorplan(
                          e.clientX,
                          e.clientY,
                          rect,
                          1000,
                          600
                        );
                        setDraggingWall({
                          index: wIdx,
                          handle: "start",
                          startNormX: pt.x,
                          startNormY: pt.y,
                          origPoints: [...wall.points] as [
                            number,
                            number,
                            number,
                            number,
                          ],
                        });
                      }}
                    />
                    <circle
                      cx={sx2}
                      cy={sy2}
                      r={isWallSelected && mapEditMode ? 6.5 : 3.5}
                      fill={
                        isWallSelected
                          ? "#38bdf8"
                          : mapTheme.floorplanWallNodeFill
                      }
                      stroke={
                        isWallSelected ? "#f8fafc" : "none"
                      }
                      strokeWidth={isWallSelected ? 1.5 : 0}
                      style={{
                        cursor:
                          isInteractiveInSelect && isWallSelected
                            ? "crosshair"
                            : "default",
                      }}
                      onPointerDown={(e) => {
                        if (!isInteractiveInSelect || !mapSvgRef.current) return;
                        e.stopPropagation();
                        setSelectedWallIdx(wIdx);
                        setSelectedRoomId(null);
                        const rect = mapSvgRef.current.getBoundingClientRect();
                        const pt = mapClientPointToFloorplan(
                          e.clientX,
                          e.clientY,
                          rect,
                          1000,
                          600
                        );
                        setDraggingWall({
                          index: wIdx,
                          handle: "end",
                          startNormX: pt.x,
                          startNormY: pt.y,
                          origPoints: [...wall.points] as [
                            number,
                            number,
                            number,
                            number,
                          ],
                        });
                      }}
                    />

                    {/* Wall Measurement Pill on Canvas */}
                    {(isWallSelected || (mapEditMode && showMeasurements)) && (
                      <g style={{ pointerEvents: "none" }}>
                        <rect
                          x={midSvgX - (isWallSelected ? 46 : 30)}
                          y={midSvgY - 10}
                          width={isWallSelected ? 92 : 60}
                          height={18}
                          rx={3}
                          fill="rgba(7, 10, 15, 0.84)"
                          stroke={
                            isWallSelected
                              ? "#38bdf8"
                              : isDoor
                              ? "#f59e0b"
                              : isWindow
                              ? "#38bdf8"
                              : "rgba(148, 163, 184, 0.45)"
                          }
                          strokeWidth={1}
                        />
                        <text
                          x={midSvgX}
                          y={midSvgY + 3}
                          textAnchor="middle"
                          fill={isWallSelected ? "#38bdf8" : "#f8fafc"}
                          fontSize="11"
                          fontFamily="JetBrains Mono, monospace"
                          fontWeight="700"
                        >
                          {isWallSelected
                            ? `${wMetrics.lengthM.toFixed(2)}m · ${Math.round(
                                wMetrics.angleDeg
                              )}°`
                            : `${wMetrics.lengthM.toFixed(2)}m`}
                        </text>
                      </g>
                    )}
                  </g>
                );
              })}

              {/* Live Wall Draft Preview with Real-Time Meter & Angle Readout */}
              {wallStartPoint && wallPreviewPoint && (
                (() => {
                  const sx1 = wallStartPoint.x * 1000;
                  const sy1 = wallStartPoint.y * 600;
                  const sx2 = wallPreviewPoint.x * 1000;
                  const sy2 = wallPreviewPoint.y * 600;
                  const draftWallMetrics = computeWallMetrics(
                    [
                      wallStartPoint.x,
                      wallStartPoint.y,
                      wallPreviewPoint.x,
                      wallPreviewPoint.y,
                    ],
                    floorplanWidthM,
                    floorplanHeightM
                  );
                  const midX = (sx1 + sx2) / 2;
                  const midY = (sy1 + sy2) / 2;
                  return (
                    <g style={{ pointerEvents: "none" }}>
                      <line
                        x1={sx1}
                        y1={sy1}
                        x2={sx2}
                        y2={sy2}
                        stroke={wallKindDraft === "door" ? "#f59e0b" : "#38bdf8"}
                        strokeWidth="3.5"
                        strokeDasharray="6 3"
                        strokeLinecap="round"
                      />
                      <circle
                        cx={sx1}
                        cy={sy1}
                        r={5}
                        fill="#38bdf8"
                        stroke="#f8fafc"
                        strokeWidth="1.5"
                      />
                      <circle
                        cx={sx2}
                        cy={sy2}
                        r={wallPreviewPoint.snappedToVertex ? 8 : 5}
                        fill={
                          wallPreviewPoint.snappedToVertex
                            ? "rgba(16, 185, 129, 0.35)"
                            : "#38bdf8"
                        }
                        stroke={
                          wallPreviewPoint.snappedToVertex ? "#10b981" : "#f8fafc"
                        }
                        strokeWidth="2"
                      />
                      <rect
                        x={midX - 54}
                        y={midY - 24}
                        width={108}
                        height={20}
                        rx={4}
                        fill="rgba(7, 10, 15, 0.9)"
                        stroke="#38bdf8"
                        strokeWidth="1"
                      />
                      <text
                        x={midX}
                        y={midY - 10}
                        textAnchor="middle"
                        fill="#f8fafc"
                        fontSize="11"
                        fontFamily="JetBrains Mono, monospace"
                        fontWeight="700"
                      >
                        {`${draftWallMetrics.lengthM.toFixed(2)}m @ ${Math.round(
                          draftWallMetrics.angleDeg
                        )}°`}
                      </text>
                    </g>
                  );
                })()
              )}

              {cameraNames.map((cam) => {
                const place = mapPlacements[cam] || {
                  x: 0.5,
                  y: 0.5,
                  angle_deg: 0,
                  fov_deg: 90,
                  range: 0.25,
                };
                const cx = place.x * 1000;
                const cy = place.y * 600;
                const radius = Math.max(50, Math.min(450, place.range * 800));
                const tipPt = computeFovConeTipSvgPoint(
                  cx,
                  cy,
                  place.angle_deg,
                  radius
                );
                const act = cameraActivity[cam];
                const objs = act?.objects || [];
                const hasObjects = objs.length > 0;
                const hasMotion = Boolean(act?.motion && (act?.max_Area ?? 5000) >= 3500);
                const isSelected = selectedCamera === cam;
                const isDrawingMode =
                  mapEditMode &&
                  (mapTool === "draw_room" || mapTool === "draw_wall");

                const coneFill = hasObjects
                  ? "rgba(239, 68, 68, 0.30)"
                  : hasMotion
                  ? "rgba(245, 158, 11, 0.24)"
                  : "rgba(56, 189, 248, 0.13)";
                const coneStroke = hasObjects
                  ? "#ef4444"
                  : hasMotion
                  ? "#f59e0b"
                  : isSelected
                  ? "#38bdf8"
                  : "rgba(56, 189, 248, 0.45)";

                return (
                  <g
                    key={cam}
                    style={{
                      pointerEvents: isDrawingMode ? "none" : "auto",
                    }}
                  >
                    {/* FOV Cone is non-blocking so rooms/walls underneath remain clickable */}
                    <path
                      d={buildFovConeSvgPath(cx, cy, place.angle_deg, place.fov_deg, radius)}
                      fill={coneFill}
                      stroke={coneStroke}
                      strokeWidth={isSelected ? 2 : 1.2}
                      style={{ pointerEvents: "none" }}
                    />
                    {(mapEditMode || isSelected) && (
                      <>
                        <line
                          x1={cx}
                          y1={cy}
                          x2={tipPt.x}
                          y2={tipPt.y}
                          stroke={coneStroke}
                          strokeWidth="1.2"
                          strokeDasharray="3 3"
                          opacity="0.8"
                          style={{ pointerEvents: "none" }}
                        />
                        <circle
                          cx={tipPt.x}
                          cy={tipPt.y}
                          r={isSelected ? 6.5 : 5}
                          fill="#f8fafc"
                          stroke="#0ea5e9"
                          strokeWidth="2"
                          style={{ cursor: "crosshair" }}
                          onPointerDown={(e) => {
                            if (isDrawingMode) return;
                            e.stopPropagation();
                            setSelectedCamera(cam);
                            setMapEditMode(true);
                            setMapTool("select");
                            setDraggingAimCam(cam);
                          }}
                        >
                          <title>
                            Drag FOV Cone Tip Handle to rotate camera heading and range
                          </title>
                        </circle>
                      </>
                    )}
                    {(hasObjects || hasMotion) && (
                      <circle
                        cx={cx}
                        cy={cy}
                        r={20}
                        fill="none"
                        stroke={coneStroke}
                        strokeWidth="2"
                        opacity="0.75"
                        style={{ pointerEvents: "none" }}
                      />
                    )}
                    <circle
                      cx={cx}
                      cy={cy}
                      r={isSelected ? 9 : 7}
                      fill={hasObjects ? "#ef4444" : hasMotion ? "#f59e0b" : "#0ea5e9"}
                      stroke={mapTheme.floorplanCamNodeStroke}
                      strokeWidth="2"
                      style={{ cursor: mapEditMode ? "grab" : "pointer" }}
                      onPointerDown={(e) => {
                        if (isDrawingMode) return;
                        e.stopPropagation();
                        setSelectedCamera(cam);
                        setMapEditMode(true);
                        setMapTool("select");
                        setDraggingCam(cam);
                      }}
                    />
                    <text
                      x={cx + 12}
                      y={cy + 4}
                      fill={mapTheme.floorplanCamLabelFill}
                      fontSize="12"
                      fontFamily="JetBrains Mono, monospace"
                      fontWeight="700"
                      style={{ cursor: mapEditMode ? "grab" : "pointer" }}
                      onPointerDown={(e) => {
                        if (isDrawingMode) return;
                        e.stopPropagation();
                        setSelectedCamera(cam);
                        setMapEditMode(true);
                        setMapTool("select");
                        setDraggingCam(cam);
                      }}
                    >
                      {cam.toUpperCase()}
                    </text>
                    {objs.slice(0, 2).map((obj, oIdx) => (
                      <text
                        key={`${obj.id}-${oIdx}`}
                        x={cx + 12}
                        y={cy + 19 + oIdx * 14}
                        fill={mapTheme.floorplanObjectLabelFill}
                        fontSize="11"
                        fontFamily="JetBrains Mono, monospace"
                        style={{ pointerEvents: "none" }}
                      >
                        {obj.label} {Math.round((obj.score || 0.85) * 100)}%
                      </text>
                    ))}
                  </g>
                );
              })}
            </svg>
          </div>

          <div
            className="nvr-panel"
            style={{
              display: "flex",
              flexDirection: "column",
              gap: 10,
              padding: 10,
              background: "var(--bg-panel)",
              border: "1px solid var(--border-hairline)",
              overflowY: "auto",
            }}
          >
            <div
              className="mono"
              style={{
                display: "flex",
                justifyContent: "space-between",
                alignItems: "center",
                fontSize: "var(--text-xs)",
                fontWeight: 700,
              }}
            >
              <span>SECTOR FEED // {selectedCamera.toUpperCase()}</span>
              <button
                type="button"
                className="nvr-btn mono"
                style={{ height: 22, fontSize: "var(--text-xs)" }}
                onClick={() => setLiveLayout("focus")}
              >
                Open Focus
              </button>
            </div>
            <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
              <TacticalCameraPicker
                cameras={groupCams}
                selectedCamera={selectedCamera}
                onSelect={setSelectedCamera}
                cameraGroups={config?.camera_groups}
                cameraStats={stats?.cameras}
                cameraActivity={cameraActivity}
                ariaLabel="Map sector camera selector"
              />
            </div>

            <div className="nvr-cam-tile" style={{ height: 190, flexShrink: 0 }}>
              <LiveMsePlayer
                camera={selectedCamera}
                useMainStream={false}
                cameraConfig={config?.cameras?.[selectedCamera]}
                showVectors={showVectors}
              />
            </div>

            {mapPlacements[selectedCamera] && (
              <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  HEADING ANGLE: {Math.round(mapPlacements[selectedCamera].angle_deg)}° · FOV:{" "}
                  {Math.round(mapPlacements[selectedCamera].fov_deg)}° · RANGE:{" "}
                  {(mapPlacements[selectedCamera].range * floorplanWidthM).toFixed(1)}m (
                  {Math.round(mapPlacements[selectedCamera].range * 100)}%)
                </div>
                <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 6 }}>
                  <label className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    <div>Heading (°)</div>
                    <input
                      type="range"
                      min={0}
                      max={360}
                      style={{ width: "100%" }}
                      value={mapPlacements[selectedCamera].angle_deg}
                      onChange={(e) => {
                        const v = Number(e.target.value);
                        setMapEditMode(true);
                        setMapPlacements((prev) => ({
                          ...prev,
                          [selectedCamera]: { ...prev[selectedCamera], angle_deg: v },
                        }));
                      }}
                    />
                  </label>
                  <label className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    <div>FOV (°)</div>
                    <input
                      type="range"
                      min={30}
                      max={180}
                      style={{ width: "100%" }}
                      value={mapPlacements[selectedCamera].fov_deg}
                      onChange={(e) => {
                        const v = Number(e.target.value);
                        setMapEditMode(true);
                        setMapPlacements((prev) => ({
                          ...prev,
                          [selectedCamera]: { ...prev[selectedCamera], fov_deg: v },
                        }));
                      }}
                    />
                  </label>
                  <label className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    <div>Alcance (m)</div>
                    <input
                      type="range"
                      min={5}
                      max={50}
                      style={{ width: "100%" }}
                      value={Math.round(mapPlacements[selectedCamera].range * 100)}
                      onChange={(e) => {
                        const v = Number(e.target.value) / 100;
                        setMapEditMode(true);
                        setMapPlacements((prev) => ({
                          ...prev,
                          [selectedCamera]: { ...prev[selectedCamera], range: v },
                        }));
                      }}
                    />
                  </label>
                </div>
              </div>
            )}

            {/* Vector Floorplan Rooms & Walls Metric Inspector */}
            <div
              style={{
                borderTop: "1px solid var(--border-hairline)",
                paddingTop: 8,
                display: "flex",
                flexDirection: "column",
                gap: 6,
              }}
            >
              <div
                className="mono"
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                }}
              >
                <span>
                  ROOMS ({floorplanRooms.length}) // MEDIDAS EM METROS
                </span>
                <button
                  type="button"
                  className="nvr-btn mono"
                  style={{ height: 22, fontSize: "var(--text-xs)" }}
                  onClick={() => {
                    setMapEditMode(true);
                    const nextIdx = floorplanRooms.length;
                    const defaultLabel =
                      DEFAULT_ROOM_LABELS[nextIdx % DEFAULT_ROOM_LABELS.length];
                    const newId = `room_${Date.now()}_${nextIdx + 1}`;
                    const wNorm = Number(
                      Math.min(0.8, 4.0 / floorplanWidthM).toFixed(3)
                    );
                    const hNorm = Number(
                      Math.min(0.8, 3.0 / floorplanHeightM).toFixed(3)
                    );
                    const newRoom: FloorplanRoom = {
                      id: newId,
                      label: defaultLabel,
                      x: Number(
                        Math.min(1 - wNorm, 0.1 + (nextIdx % 4) * 0.06).toFixed(3)
                      ),
                      y: Number(
                        Math.min(1 - hNorm, 0.12 + (nextIdx % 4) * 0.06).toFixed(3)
                      ),
                      w: wNorm,
                      h: hNorm,
                      color:
                        ROOM_COLOR_PALETTE[nextIdx % ROOM_COLOR_PALETTE.length],
                    };
                    setFloorplanRooms((prev) => [...prev, newRoom]);
                    setSelectedRoomId(newId);
                    setSelectedWallIdx(null);
                  }}
                >
                  + Room (4×3m)
                </button>
              </div>

              {floorplanRooms.map((room) => {
                const isSel = selectedRoomId === room.id;
                const rmMetrics = computeRoomMetrics(
                  room,
                  floorplanWidthM,
                  floorplanHeightM
                );
                return (
                  <div
                    key={room.id}
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: 4,
                      padding: "6px",
                      border: `1px solid ${
                        isSel ? "var(--signal-accent)" : "var(--border-hairline)"
                      }`,
                      background: "var(--bg-deep)",
                    }}
                    onClick={() => {
                      setSelectedRoomId(room.id);
                      setSelectedWallIdx(null);
                    }}
                  >
                    <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                      <input
                        className="nvr-input mono"
                        aria-label={`Room label ${room.id}`}
                        style={{ height: 22, flex: 1, fontSize: "var(--text-xs)" }}
                        value={room.label}
                        onChange={(e) => {
                          const nextLabel = e.target.value;
                          setMapEditMode(true);
                          setFloorplanRooms((prev) =>
                            prev.map((r) =>
                              r.id === room.id ? { ...r, label: nextLabel } : r
                            )
                          );
                        }}
                      />
                      <span
                        className="mono"
                        style={{ fontSize: "var(--text-xs)", color: "var(--signal-accent-text)" }}
                      >
                        {rmMetrics.areaSqM.toFixed(1)}m²
                      </span>
                      <button
                        type="button"
                        className="nvr-btn mono"
                        style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                        onClick={(e) => {
                          e.stopPropagation();
                          setMapEditMode(true);
                          setFloorplanRooms((prev) =>
                            prev.filter((r) => r.id !== room.id)
                          );
                          if (selectedRoomId === room.id) {
                            setSelectedRoomId(null);
                          }
                        }}
                        title="Delete room"
                      >
                        Del
                      </button>
                    </div>
                    <div
                      className="mono"
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: 6,
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                      }}
                    >
                      <span>W:</span>
                      <input
                        type="number"
                        className="nvr-input mono"
                        aria-label={`Room width meters ${room.id}`}
                        step={0.1}
                        min={0.2}
                        max={floorplanWidthM}
                        style={{ height: 22, width: 58, fontSize: "var(--text-xs)", padding: "0 4px" }}
                        value={rmMetrics.widthM}
                        onChange={(e) => {
                          const nextW = Math.max(0.2, Number(e.target.value) || 0.5);
                          setMapEditMode(true);
                          setFloorplanRooms((prev) =>
                            prev.map((r) =>
                              r.id === room.id
                                ? resizeRoomByMeters(
                                    r,
                                    nextW,
                                    rmMetrics.heightM,
                                    floorplanWidthM,
                                    floorplanHeightM
                                  )
                                : r
                            )
                          );
                        }}
                      />
                      <span>m × H:</span>
                      <input
                        type="number"
                        className="nvr-input mono"
                        aria-label={`Room height meters ${room.id}`}
                        step={0.1}
                        min={0.2}
                        max={floorplanHeightM}
                        style={{ height: 22, width: 58, fontSize: "var(--text-xs)", padding: "0 4px" }}
                        value={rmMetrics.heightM}
                        onChange={(e) => {
                          const nextH = Math.max(0.2, Number(e.target.value) || 0.5);
                          setMapEditMode(true);
                          setFloorplanRooms((prev) =>
                            prev.map((r) =>
                              r.id === room.id
                                ? resizeRoomByMeters(
                                    r,
                                    rmMetrics.widthM,
                                    nextH,
                                    floorplanWidthM,
                                    floorplanHeightM
                                  )
                                : r
                            )
                          );
                        }}
                      />
                      <span>m</span>
                    </div>
                  </div>
                );
              })}

              {/* Walls & Doors Inspector Section with Direct Meter Editing */}
              <div
                className="mono"
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  marginTop: 6,
                }}
              >
                <span>
                  WALLS &amp; DOORS ({floorplanWalls.length}) // METROS
                </span>
                <div style={{ display: "flex", gap: 4 }}>
                  <button
                    type="button"
                    className="nvr-btn mono"
                    style={{ height: 22, fontSize: "var(--text-xs)" }}
                    onClick={() => {
                      setMapEditMode(true);
                      const pts = resizeWallToLengthMeters(
                        [0.2, 0.25, 0.4, 0.25],
                        3.0,
                        floorplanWidthM,
                        floorplanHeightM
                      );
                      setFloorplanWalls((prev) => {
                        const next = [...prev, { points: pts, kind: "wall" }];
                        setSelectedWallIdx(next.length - 1);
                        return next;
                      });
                      setSelectedRoomId(null);
                    }}
                  >
                    + Wall (3m)
                  </button>
                  {floorplanWalls.length > 0 && mapEditMode && (
                    <button
                      type="button"
                      className="nvr-btn mono"
                      style={{ height: 22, fontSize: "var(--text-xs)" }}
                      onClick={() => {
                        setFloorplanWalls((prev) => prev.slice(0, -1));
                        setSelectedWallIdx(null);
                      }}
                      title="Remove last drawn wall segment"
                    >
                      Undo Wall
                    </button>
                  )}
                </div>
              </div>

              {floorplanWalls.map((wall, wIdx) => {
                const isWallSel = selectedWallIdx === wIdx;
                const wMetrics = computeWallMetrics(
                  wall.points,
                  floorplanWidthM,
                  floorplanHeightM
                );
                return (
                  <div
                    key={`wall-row-${wIdx}`}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 6,
                      padding: "4px 6px",
                      border: `1px solid ${
                        isWallSel
                          ? "var(--signal-accent)"
                          : "var(--border-hairline)"
                      }`,
                      background: "var(--bg-deep)",
                    }}
                    onClick={() => {
                      setSelectedWallIdx(wIdx);
                      setSelectedRoomId(null);
                    }}
                  >
                    <span
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        minWidth: 42,
                        color:
                          wall.kind === "door"
                            ? "var(--signal-motion-text)"
                            : wall.kind === "window"
                            ? "var(--signal-accent-text)"
                            : "var(--text-primary)",
                      }}
                    >
                      {wall.kind === "door"
                        ? `D#${wIdx + 1}`
                        : wall.kind === "window"
                        ? `J#${wIdx + 1}`
                        : `W#${wIdx + 1}`}
                    </span>
                    <input
                      type="number"
                      className="nvr-input mono"
                      aria-label={`Wall ${wIdx + 1} length in meters`}
                      step={0.1}
                      min={0.1}
                      max={Math.hypot(floorplanWidthM, floorplanHeightM)}
                      style={{ height: 22, width: 62, fontSize: "var(--text-xs)", padding: "0 4px" }}
                      value={wMetrics.lengthM}
                      onChange={(e) => {
                        const targetM = Math.max(0.1, Number(e.target.value) || 0.5);
                        setMapEditMode(true);
                        setFloorplanWalls((prev) =>
                          prev.map((w, idx) =>
                            idx === wIdx
                              ? {
                                  ...w,
                                  points: resizeWallToLengthMeters(
                                    w.points,
                                    targetM,
                                    floorplanWidthM,
                                    floorplanHeightM
                                  ),
                                }
                              : w
                          )
                        );
                      }}
                    />
                    <span
                      className="mono"
                      style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                    >
                      m · {Math.round(wMetrics.angleDeg)}°
                    </span>
                    <button
                      type="button"
                      className="nvr-btn mono"
                      style={{ height: 22, padding: "0 5px", fontSize: "var(--text-xs)" }}
                      title="Rotate wall 90°"
                      onClick={(e) => {
                        e.stopPropagation();
                        setMapEditMode(true);
                        setFloorplanWalls((prev) =>
                          prev.map((w, idx) =>
                            idx === wIdx
                              ? {
                                  ...w,
                                  points: rotateWallToAngleDegrees(
                                    w.points,
                                    (wMetrics.angleDeg + 90) % 360,
                                    floorplanWidthM,
                                    floorplanHeightM
                                  ),
                                }
                              : w
                          )
                        );
                      }}
                    >
                      90°
                    </button>
                    <button
                      type="button"
                      className="nvr-btn mono"
                      style={{
                        height: 22,
                        padding: "0 6px",
                        fontSize: "var(--text-xs)",
                        marginLeft: "auto",
                      }}
                      onClick={(e) => {
                        e.stopPropagation();
                        setMapEditMode(true);
                        setFloorplanWalls((prev) =>
                          prev.filter((_, idx) => idx !== wIdx)
                        );
                        if (selectedWallIdx === wIdx) {
                          setSelectedWallIdx(null);
                        }
                      }}
                      title="Delete wall"
                    >
                      Del
                    </button>
                  </div>
                );
              })}
            </div>
          </div>
        </div>
      )}

      {shouldRenderSpotlightTimeline(
        liveLayout,
        spotlightTimelineMode,
        timeShiftTs
      ) && (
        <TimelineScrubber
          camera={liveLayout === "spotlight" ? spotlightHeroCam : selectedCamera}
          cameras={groupCams}
          stageCameras={liveLayout === "2x2" ? visibleMatrixCams : undefined}
          cameraGroups={effectiveCameraGroups}
          onSelectCamera={selectCameraAndRevealOnStage}
          onFocusCamera={
            liveLayout !== "focus"
              ? (cam) => {
                  triggerMainStreamPrewarmIfNeeded(
                    resolveRealCameraSource(cam, config?.cameras || cameraNames),
                    true
                  );
                  setSelectedCamera(cam);
                  setLiveLayout("focus");
                }
              : undefined
          }
          segments={focusSegments}
          reviews={reviews}
          currentTs={timeShiftTs}
          onSeek={(ts) => setTimeShiftTs(sanitizeTimeShiftSeek(ts))}
          onExportRange={handleQuickExport}
          titlePrefix="TIMELINE"
        />
      )}
    </>
  );
};
