import {
  CameraConfig,
  CameraMapPlacement,
  FloorplanRoom,
  FloorplanWall,
} from "../types/nvr";

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
  const sx = Math.max(0, Math.min(1, finiteOr(camX, 0.5)));
  const sy = Math.max(0, Math.min(1, finiteOr(camY, 0.5)));
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
  const safeRadius = Math.max(0, Math.min(1000, finiteOr(radius, 100)));
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
  const dxM = (x2 - x1) * safeW;
  const dyM = (y2 - y1) * safeH;
  const currentLen = Math.hypot(dxM, dyM);
  const angleRad = currentLen > 1e-4 ? Math.atan2(dyM, dxM) : 0;
  const absCos = Math.abs(Math.cos(angleRad));
  const absSin = Math.abs(Math.sin(angleRad));
  const maxFitM = Math.min(
    maxPossibleM,
    absCos > 1e-6 ? safeW / absCos : maxPossibleM,
    absSin > 1e-6 ? safeH / absSin : maxPossibleM
  );
  const clampedLen = Math.max(0.1, Math.min(maxFitM, safeTargetM));
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
  const maxPossibleM = Math.hypot(safeW, safeH);
  const rawLenM = Math.max(0.2, Math.hypot((x2 - x1) * safeW, (y2 - y1) * safeH));
  const safeDeg = finiteOr(targetAngleDeg, 0);
  const rad = (safeDeg * Math.PI) / 180;
  const absCos = Math.abs(Math.cos(rad));
  const absSin = Math.abs(Math.sin(rad));
  const maxFitM = Math.min(
    maxPossibleM,
    absCos > 1e-6 ? safeW / absCos : maxPossibleM,
    absSin > 1e-6 ? safeH / absSin : maxPossibleM
  );
  const lenM = Math.min(maxFitM, rawLenM);
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
    if (
      trimmed.includes("\\") ||
      trimmed.includes("..") ||
      /[?#]/.test(trimmed) ||
      /^\/%2f/i.test(trimmed) ||
      /%2e%2e|%5c|%252e|%255c|%00/i.test(trimmed)
    ) {
      return "";
    }
    try {
      const decoded = decodeURIComponent(trimmed);
      const segments = decoded.split("/");
      if (
        !decoded.startsWith("/") ||
        decoded.startsWith("//") ||
        decoded.includes("\\") ||
        decoded.includes("..") ||
        decoded.includes("%") ||
        segments.some((seg) => seg === "." || seg === "..") ||
        /[\u0000-\u001F\u007F?#]/.test(decoded) ||
        /^\/api\/(?:auth|logout|users|tokens|config|restart)\b/i.test(decoded)
      ) {
        return "";
      }
    } catch {
      return "";
    }
    return trimmed;
  }
  if (/^https?:\/\//i.test(trimmed)) {
    if (/[\s<>"'`\\]/.test(trimmed)) return "";
    try {
      const parsed = new URL(trimmed);
      if (
        (parsed.protocol !== "http:" && parsed.protocol !== "https:") ||
        parsed.username ||
        parsed.password
      ) {
        return "";
      }
      return trimmed;
    } catch {
      return "";
    }
  }
  if (/^data:image\/(png|jpeg|jpg|webp|gif);base64,[a-z0-9+/=\s]+$/i.test(trimmed)) {
    return trimmed;
  }
  if (/^data:image\/svg\+xml;base64,([a-z0-9+/=\s]+)$/i.test(trimmed)) {
    const b64 = trimmed.slice(trimmed.indexOf(",") + 1).replace(/\s+/g, "");
    try {
      const decoded = globalThis.atob(b64);
      if (
        /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/.test(decoded) ||
        /<script|<iframe|<object|<embed|<audio\b|<video\b|<portal\b|<feimage\b|<use\b|<foreignobject|<set\b|<animate|<handler\b|<listener\b|<link\b|<meta\b|<base\b|<style|@import|<!entity|<!doctype|j\s*a\s*v\s*a\s*s\s*c\s*r\s*i\s*p\s*t\s*:|v\s*b\s*s\s*c\s*r\s*i\s*p\s*t\s*:|data:\s*text\/html|(?:xlink:)?href\s*=\s*["']?\s*(?:javascript:|vbscript:|data:|https?:|\/\/)|\bon[a-z]+\s*=|&#/i.test(
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

export function sanitizeFloorplanRoom(
  room: Partial<FloorplanRoom> | null | undefined,
  index: number = 0
): FloorplanRoom {
  const safeIdx = Number.isFinite(index) ? Math.max(0, Math.trunc(index)) : 0;
  const rawW = finiteOr(room?.w ?? NaN, 0.2);
  const rawH = finiteOr(room?.h ?? NaN, 0.2);
  const w = Number(Math.max(0.01, Math.min(1, Math.abs(rawW) || 0.2)).toFixed(3));
  const h = Number(Math.max(0.01, Math.min(1, Math.abs(rawH) || 0.2)).toFixed(3));
  const rawX = finiteOr(room?.x ?? NaN, 0);
  const rawY = finiteOr(room?.y ?? NaN, 0);
  const x = Number(Math.max(0, Math.min(1 - w, rawX)).toFixed(3));
  const y = Number(Math.max(0, Math.min(1 - h, rawY)).toFixed(3));
  const id =
    typeof room?.id === "string" && room.id.trim()
      ? room.id.trim()
      : `room_${safeIdx + 1}`;
  const label =
    typeof room?.label === "string" && room.label.trim()
      ? room.label.trim()
      : "ROOM";
  const cleanColor =
    typeof room?.color === "string" && /^#[0-9a-fA-F]{3,8}$/.test(room.color.trim())
      ? room.color.trim()
      : undefined;

  const out: FloorplanRoom = {
    ...(room as FloorplanRoom),
    id,
    label,
    x,
    y,
    w,
    h,
  };
  if (room?.color !== undefined) {
    out.color = cleanColor ?? null;
  }
  return out;
}

export function sanitizeFloorplanWall(
  wall: Partial<FloorplanWall> | null | undefined
): FloorplanWall {
  const p = Array.isArray(wall?.points) ? wall.points : [0.2, 0.2, 0.4, 0.2];
  const x1 = Number(Math.max(0, Math.min(1, finiteOr(p[0], 0.2))).toFixed(3));
  const y1 = Number(Math.max(0, Math.min(1, finiteOr(p[1], 0.2))).toFixed(3));
  const x2 = Number(Math.max(0, Math.min(1, finiteOr(p[2], 0.4))).toFixed(3));
  const y2 = Number(Math.max(0, Math.min(1, finiteOr(p[3], 0.2))).toFixed(3));
  const validKinds = new Set(["wall", "door", "window"]);
  const out: FloorplanWall = {
    ...(wall as FloorplanWall),
    points: [x1, y1, x2, y2],
  };
  if (wall?.kind !== undefined) {
    out.kind =
      typeof wall.kind === "string" && validKinds.has(wall.kind)
        ? wall.kind
        : "wall";
  }
  return out;
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

  const camNames = Object.keys(existingCameras || {}).filter(
    (k) => k !== "__proto__" && k !== "constructor" && k !== "prototype"
  );
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

  const sanitizedRooms: FloorplanRoom[] = (
    Array.isArray(floorplanRooms) ? floorplanRooms : []
  ).map((r, idx) => sanitizeFloorplanRoom(r, idx));

  const sanitizedWalls: FloorplanWall[] = (
    Array.isArray(floorplanWalls) ? floorplanWalls : []
  ).map((w) => sanitizeFloorplanWall(w));

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

// ============================================================================
// Pure CAD Draft State Reducers & Interaction Transitions
// ============================================================================

export type FloorplanRoomResizeHandle =
  | "nw"
  | "ne"
  | "sw"
  | "se"
  | "n"
  | "s"
  | "e"
  | "w";

export interface FloorplanRoomDraft {
  startX: number;
  startY: number;
  currentX: number;
  currentY: number;
}

export interface FloorplanRoomDragState {
  id: string;
  startNormX: number;
  startNormY: number;
  origX: number;
  origY: number;
}

export interface FloorplanRoomResizeState {
  id: string;
  handle: FloorplanRoomResizeHandle;
  anchorX: number;
  anchorY: number;
  origRoom: FloorplanRoom;
}

export interface FloorplanWallDragState {
  index: number;
  handle: "start" | "end" | "move";
  startNormX: number;
  startNormY: number;
  origPoints: [number, number, number, number];
}

export function computeFloorplanSnapVertices(
  walls: FloorplanWall[],
  rooms: FloorplanRoom[]
): Array<{ x: number; y: number }> {
  const pts: Array<{ x: number; y: number }> = [];
  for (const rawW of Array.isArray(walls) ? walls : []) {
    const w = sanitizeFloorplanWall(rawW);
    pts.push({ x: w.points[0], y: w.points[1] });
    pts.push({ x: w.points[2], y: w.points[3] });
  }
  const safeRooms = Array.isArray(rooms) ? rooms : [];
  for (let i = 0; i < safeRooms.length; i++) {
    const r = sanitizeFloorplanRoom(safeRooms[i], i);
    pts.push({ x: r.x, y: r.y });
    pts.push({ x: r.x + r.w, y: r.y });
    pts.push({ x: r.x, y: r.y + r.h });
    pts.push({ x: r.x + r.w, y: r.y + r.h });
  }
  return pts;
}

export function applyFloorplanCameraDrag(
  placements: Record<string, CameraMapPlacement>,
  camera: string,
  rawX: number,
  rawY: number
): Record<string, CameraMapPlacement> {
  if (!camera) return placements;
  const nx = Math.max(0.03, Math.min(0.97, finiteOr(rawX, 0.5)));
  const ny = Math.max(0.03, Math.min(0.97, finiteOr(rawY, 0.5)));
  const current = placements?.[camera] || {
    x: nx,
    y: ny,
    angle_deg: 0,
    fov_deg: 90,
    range: 0.25,
  };
  return {
    ...(placements || {}),
    [camera]: {
      ...current,
      x: Number(nx.toFixed(3)),
      y: Number(ny.toFixed(3)),
    },
  };
}

export function applyFloorplanFovTipDrag(
  placements: Record<string, CameraMapPlacement>,
  camera: string,
  tipX: number,
  tipY: number
): Record<string, CameraMapPlacement> {
  if (!camera) return placements;
  const current = placements?.[camera] || {
    x: 0.5,
    y: 0.5,
    angle_deg: 0,
    fov_deg: 90,
    range: 0.25,
  };
  const aim = computeCameraAimFromTip(current.x, current.y, tipX, tipY);
  return {
    ...(placements || {}),
    [camera]: {
      ...current,
      angle_deg: aim.angle_deg,
      range: aim.range,
    },
  };
}

export function applyFloorplanWallEndpointDrag(
  walls: FloorplanWall[],
  drag: FloorplanWallDragState,
  rawX: number,
  rawY: number,
  options: {
    snapStep?: number;
    orthoSnap?: boolean;
    vertices?: Array<{ x: number; y: number }>;
    floorplanWidthM?: number;
    floorplanHeightM?: number;
  } = {}
): FloorplanWall[] {
  const {
    snapStep = 0.005,
    orthoSnap = true,
    vertices = [],
    floorplanWidthM = DEFAULT_FLOORPLAN_WIDTH_M,
    floorplanHeightM = DEFAULT_FLOORPLAN_HEIGHT_M,
  } = options;
  const { index, handle, startNormX, startNormY, origPoints } = drag;

  if (handle === "move") {
    const safeRawX = finiteOr(rawX, startNormX);
    const safeRawY = finiteOr(rawY, startNormY);
    const rawDx =
      snapStep > 0
        ? Math.round((safeRawX - startNormX) / snapStep) * snapStep
        : safeRawX - startNormX;
    const rawDy =
      snapStep > 0
        ? Math.round((safeRawY - startNormY) / snapStep) * snapStep
        : safeRawY - startNormY;
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
    return walls.map((w, idx) =>
      idx === index ? { ...w, points: nextPts } : w
    );
  }

  if (handle === "start") {
    const snapped = snapWallEndpoint(
      origPoints[2],
      origPoints[3],
      rawX,
      rawY,
      {
        snapStep,
        orthoSnap,
        vertices: vertices.filter(
          (v) => Math.hypot(v.x - origPoints[0], v.y - origPoints[1]) > 0.005
        ),
        floorplanWidthM,
        floorplanHeightM,
      }
    );
    return walls.map((w, idx) =>
      idx === index
        ? {
            ...w,
            points: [snapped.x, snapped.y, origPoints[2], origPoints[3]],
          }
        : w
    );
  }

  const snapped = snapWallEndpoint(
    origPoints[0],
    origPoints[1],
    rawX,
    rawY,
    {
      snapStep,
      orthoSnap,
      vertices: vertices.filter(
        (v) => Math.hypot(v.x - origPoints[2], v.y - origPoints[3]) > 0.005
      ),
      floorplanWidthM,
      floorplanHeightM,
    }
  );
  return walls.map((w, idx) =>
    idx === index
      ? {
          ...w,
          points: [origPoints[0], origPoints[1], snapped.x, snapped.y],
        }
      : w
  );
}

export function applyFloorplanRoomDrag(
  rooms: FloorplanRoom[],
  drag: FloorplanRoomDragState,
  rawX: number,
  rawY: number,
  snapStep: number = 0.005
): FloorplanRoom[] {
  const dx = finiteOr(rawX, drag.startNormX) - drag.startNormX;
  const dy = finiteOr(rawY, drag.startNormY) - drag.startNormY;
  return rooms.map((r) => {
    if (r.id !== drag.id) return r;
    const nextX = snapFloorplanCoord(
      Math.max(0, Math.min(1 - r.w, drag.origX + dx)),
      snapStep
    );
    const nextY = snapFloorplanCoord(
      Math.max(0, Math.min(1 - r.h, drag.origY + dy)),
      snapStep
    );
    return {
      ...r,
      x: Math.min(1 - r.w, nextX),
      y: Math.min(1 - r.h, nextY),
    };
  });
}

export function applyFloorplanRoomResize(
  rooms: FloorplanRoom[],
  resize: FloorplanRoomResizeState,
  rawX: number,
  rawY: number,
  snapStep: number = 0.005
): FloorplanRoom[] {
  const snappedX = snapFloorplanCoord(rawX, snapStep);
  const snappedY = snapFloorplanCoord(rawY, snapStep);
  const { origRoom, handle } = resize;
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
  return rooms.map((r) => (r.id === resize.id ? { ...r, ...norm } : r));
}

export function applyFloorplanRoomDrawCommit(
  rooms: FloorplanRoom[],
  draft: FloorplanRoomDraft,
  options: {
    floorplanWidthM?: number;
    floorplanHeightM?: number;
    nowMs?: number;
  } = {}
): {
  rooms: FloorplanRoom[];
  createdRoom: FloorplanRoom;
  selectedRoomId: string;
} {
  const safeW = Math.max(
    1,
    finiteOr(options.floorplanWidthM ?? DEFAULT_FLOORPLAN_WIDTH_M, DEFAULT_FLOORPLAN_WIDTH_M)
  );
  const safeH = Math.max(
    1,
    finiteOr(options.floorplanHeightM ?? DEFAULT_FLOORPLAN_HEIGHT_M, DEFAULT_FLOORPLAN_HEIGHT_M)
  );
  const nowMs = Number.isFinite(options.nowMs) ? options.nowMs! : Date.now();
  const dragDist = Math.hypot(
    draft.currentX - draft.startX,
    draft.currentY - draft.startY
  );
  const norm =
    dragDist >= 0.012
      ? normalizeFloorplanRoomRect(
          draft.startX,
          draft.startY,
          draft.currentX,
          draft.currentY,
          0.015
        )
      : normalizeFloorplanRoomRect(
          draft.startX,
          draft.startY,
          Math.min(1, draft.startX + 3.0 / safeW),
          Math.min(1, draft.startY + 2.4 / safeH),
          0.02
        );
  const safeRooms = Array.isArray(rooms) ? rooms : [];
  const nextIdx = safeRooms.length;
  const defaultLabel = DEFAULT_ROOM_LABELS[nextIdx % DEFAULT_ROOM_LABELS.length];
  const newId = `room_${nowMs}_${nextIdx + 1}`;
  const createdRoom: FloorplanRoom = {
    id: newId,
    label: defaultLabel,
    x: norm.x,
    y: norm.y,
    w: norm.w,
    h: norm.h,
    color: ROOM_COLOR_PALETTE[nextIdx % ROOM_COLOR_PALETTE.length],
  };
  return {
    rooms: [...safeRooms, createdRoom],
    createdRoom,
    selectedRoomId: newId,
  };
}

export function deleteSelectedFloorplanEntity(state: {
  rooms: FloorplanRoom[];
  walls: FloorplanWall[];
  selectedRoomId: string | null;
  selectedWallIdx: number | null;
}): {
  rooms: FloorplanRoom[];
  walls: FloorplanWall[];
  selectedRoomId: string | null;
  selectedWallIdx: number | null;
  deleted: "room" | "wall" | null;
} {
  const safeRooms = Array.isArray(state.rooms) ? state.rooms : [];
  const safeWalls = Array.isArray(state.walls) ? state.walls : [];
  if (state.selectedRoomId) {
    return {
      rooms: safeRooms.filter((r) => r.id !== state.selectedRoomId),
      walls: safeWalls,
      selectedRoomId: null,
      selectedWallIdx: state.selectedWallIdx,
      deleted: "room",
    };
  }
  if (
    state.selectedWallIdx !== null &&
    state.selectedWallIdx >= 0 &&
    state.selectedWallIdx < safeWalls.length
  ) {
    return {
      rooms: safeRooms,
      walls: safeWalls.filter((_, idx) => idx !== state.selectedWallIdx),
      selectedRoomId: state.selectedRoomId,
      selectedWallIdx: null,
      deleted: "wall",
    };
  }
  return {
    rooms: safeRooms,
    walls: safeWalls,
    selectedRoomId: state.selectedRoomId,
    selectedWallIdx: state.selectedWallIdx,
    deleted: null,
  };
}

export const FloorplanCadStateReducers = {
  computeFloorplanSnapVertices,
  applyFloorplanCameraDrag,
  applyFloorplanFovTipDrag,
  applyFloorplanWallEndpointDrag,
  applyFloorplanRoomDrag,
  applyFloorplanRoomResize,
  applyFloorplanRoomDrawCommit,
  deleteSelectedFloorplanEntity,
} as const;
