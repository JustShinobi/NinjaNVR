import React, { useEffect, useMemo, useRef, useState } from "react";
import {
  Camera,
  Check,
  ChevronLeft,
  ChevronRight,
  Eye,
  Grid2x2,
  Maximize2,
  MoreHorizontal,
  Move,
  Play,
  RefreshCw,
  RotateCcw,
  Rows2,
  Scissors,
} from "lucide-react";
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
  CameraHardwareInfoResponse,
  CameraLiveActivity,
  CameraPtzInfoResponse,
  CameraRuntimeStats,
  RecordingSegment,
  ReviewItem,
  StateRoiConfig,
} from "../types/nvr";
import { FloorplanStudioCanvas } from "../components/FloorplanStudioCanvas";
export {
  DEFAULT_FLOORPLAN_HEIGHT_M,
  DEFAULT_FLOORPLAN_WIDTH_M,
  DEFAULT_ROOM_LABELS,
  FloorplanCadStateReducers,
  type FloorplanRoomDragState,
  type FloorplanRoomDraft,
  type FloorplanRoomResizeHandle,
  type FloorplanRoomResizeState,
  type FloorplanStudioTool,
  type FloorplanWallDragState,
  ROOM_COLOR_PALETTE,
  applyFloorplanCameraDrag,
  applyFloorplanFovTipDrag,
  applyFloorplanRoomDrag,
  applyFloorplanRoomDrawCommit,
  applyFloorplanRoomResize,
  applyFloorplanWallEndpointDrag,
  buildFloorplanSavePayload,
  buildFovConeSvgPath,
  computeCameraAimFromTip,
  computeDefaultMapPlacement,
  computeFloorplanSnapVertices,
  computeFovConeTipSvgPoint,
  computeRoomMetrics,
  computeWallMetrics,
  deleteSelectedFloorplanEntity,
  extractRoomBoundaryWalls,
  mapClientPointToFloorplan,
  normalizeFloorplanRoomRect,
  resizeRoomByMeters,
  resizeWallToLengthMeters,
  rotateWallToAngleDegrees,
  sanitizeFloorplanRoom,
  sanitizeFloorplanUrl,
  sanitizeFloorplanWall,
  sanitizeMapPlacement,
  snapFloorplanCoord,
  snapWallEndpoint,
} from "../utils/floorplanCadEngine";
import {
  HudPosition,
  LiveLayoutMode,
  MatrixDensity,
  SpotlightFilter,
  formatTs,
  resolveEffectiveTheme,
} from "../utils/format";
import { resolveCanvasThemePalette } from "../utils/themeTokens";
import { LiveMsePlayer, PushToTalkButton, VodHlsPlayer } from "../components/VideoPlayers";
import {
  CAMERA_HUD_CONTROL_DESCRIPTORS,
  applyDeletedPresetToPtzInfo,
  applySavedPresetToPtzInfo,
  cameraHardwareCache,
  isCameraHardwareControlsSupported,
  isCameraPtzEnabled,
  mergeOptimisticCameraHardwareInfo,
  probeCameraHardwareAndPtz,
  resolveCameraCapabilitySnapshot,
  resolveCameraPtzPresets,
  resolveCameraPushToTalkCapability,
  resolveCameraZoomSupported,
  setCachedCameraHardware,
  setCachedCameraPtzInfo,
  subscribeCameraHardwareCache,
} from "../utils/cameraCapabilities";

export {
  isCameraHardwareControlsSupported,
  isCameraPtzEnabled,
  resolveCameraPtzPresets,
  resolveCameraPushToTalkCapability,
  resolveCameraZoomSupported,
};
import { TimelineScrubber, type TimelineClipRequest } from "../components/TimelineScrubber";
import { ReviewFlipbook } from "../components/ReviewFlipbook";
import { TacticalCameraPicker } from "../components/TacticalCameraPicker";
import { CameraDetailsSheet } from "../components/CameraDetailsSheet";
import {
  buildCameraSnapshotUrl,
  filterCompactStageDescriptors,
  formatCameraTelemetryRows,
  resolveCameraRuntimeFlags,
  resolveCompactStageContainerStyle,
  resolveTileException,
  resolveTileIdentityTone,
  sanitizeCompactGridColumns,
  type CameraRuntimeFlag,
  type CompactGridColumns,
} from "../utils/liveTileHud";
import { useCompactViewport } from "../utils/useCompactViewport";
import {
  computeLiveStageDescriptors,
  formatRelativeAgo,
  resolveEffectiveSpotlightHeroCam,
  resolveAspectMatrixGridStyle,
  resolveMatrixDensityOptions,
  selectRecentEvents,
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
  const safeCams = Array.isArray(cameras) ? cameras : [];
  const pageSize = getMatrixPageSize(density);
  const totalPages = Math.max(1, Math.ceil(safeCams.length / pageSize));
  const normalizedPage =
    typeof page === "number" && Number.isFinite(page) ? Math.floor(page) : 0;
  const safePage = Math.max(0, Math.min(normalizedPage, totalPages - 1));
  const visibleMatrixCams = safeCams.slice(
    safePage * pageSize,
    (safePage + 1) * pageSize
  );
  return { visibleMatrixCams, pageSize, totalPages, safePage };
}

export function nextMatrixTourPage(
  currentPage: number,
  totalPages: number
): number {
  const safeTotal =
    typeof totalPages === "number" && Number.isFinite(totalPages)
      ? Math.floor(totalPages)
      : 1;
  if (safeTotal <= 1) return 0;
  const safeCurrent =
    typeof currentPage === "number" && Number.isFinite(currentPage)
      ? Math.max(0, Math.floor(currentPage))
      : 0;
  return (safeCurrent + 1) % safeTotal;
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

export function resolveSafeAlertThumbSrc(
  thumbPath: string | null | undefined,
  camera: string
): string {
  const rawCam = (camera || "").trim();
  const cleanCam =
    !rawCam || rawCam === "." || rawCam === ".." || rawCam.includes("..")
      ? "camera"
      : rawCam;
  const encodedCam = encodeURIComponent(cleanCam);
  const fallback = `/api/${encodedCam}/latest.webp`;
  if (typeof thumbPath !== "string") return fallback;
  const trimmed = thumbPath.trim();
  if (!trimmed || /[\u0000-\u001F\u007F\\?#]/.test(trimmed)) return fallback;
  const stripped = trimmed.replace(/^\/media\/[^/]+/, "").trim();
  if (
    !stripped ||
    !stripped.startsWith("/") ||
    stripped.startsWith("//") ||
    /^\/%2f/i.test(stripped) ||
    stripped.includes("..") ||
    /%2e%2e|%5c|%252e|%255c|%00/i.test(stripped)
  ) {
    return fallback;
  }
  try {
    const decoded = decodeURIComponent(stripped);
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
      return fallback;
    }
  } catch {
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
  let currentCamera = (options.camera || "").trim();
  const speed = options.speed ?? 0.5;
  const fetchFn: typeof fetch =
    options.fetchImpl ?? ((input, init) => fetch(input, init));
  let moving = false;
  let activeDirection: PtzMoveDirection | null = null;
  let disposed = false;
  let pendingOp: Promise<void> = Promise.resolve();

  const enqueueOp = (task: () => Promise<void>): Promise<void> => {
    const next = pendingOp.then(task, task);
    pendingOp = next.catch(() => {});
    return next;
  };

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
    await enqueueOp(() => sendStopForCamera(camToStop));
  };

  const startMove = async (direction: PtzMoveDirection) => {
    if (disposed || !currentCamera) return;
    if (moving && activeDirection === direction) {
      await pendingOp;
      return;
    }
    const targetCam = currentCamera;
    moving = true;
    activeDirection = direction;
    const payload = buildPtzMovePayload(direction, speed, "continuous");
    options.onMoveStarted?.(targetCam, direction);
    await enqueueOp(async () => {
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
    });
  };

  const setCamera = async (nextCamera: string) => {
    const cleanNext = (nextCamera || "").trim();
    if (cleanNext === currentCamera) return;
    if (moving) {
      const prevCam = currentCamera;
      moving = false;
      activeDirection = null;
      currentCamera = cleanNext;
      await enqueueOp(() => sendStopForCamera(prevCam));
      return;
    }
    currentCamera = cleanNext;
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
  hardwareInfo?: CameraHardwareInfoResponse | null;
  onPtzUpdate?: (camera: string, nextInfo: CameraPtzInfoResponse) => void;
  onHardwareUpdate?: (camera: string, nextHw: CameraHardwareInfoResponse) => void;
}> = ({
  camera,
  cameraConfig,
  ptzInfo,
  hardwareInfo,
  onPtzUpdate,
  onHardwareUpdate,
}) => {
  const capabilitySnapshot = resolveCameraCapabilitySnapshot({
    camera,
    cameraConfig,
    ptzInfo,
    hardwareInfo,
  });
  const ptzEnabled = capabilitySnapshot.ptzEnabled;
  const hwControlsSupported = capabilitySnapshot.hardwareControlsSupported;
  const [showPresetInput, setShowPresetInput] = useState(false);
  const [newPresetName, setNewPresetName] = useState("");
  const [showHwDrawer, setShowHwDrawer] = useState(false);
  const [hwBusy, setHwBusy] = useState(false);
  const onPtzUpdateRef = useRef(onPtzUpdate);
  onPtzUpdateRef.current = onPtzUpdate;
  const onHardwareUpdateRef = useRef(onHardwareUpdate);
  onHardwareUpdateRef.current = onHardwareUpdate;
  const ptzInfoRef = useRef(ptzInfo);
  ptzInfoRef.current = ptzInfo;
  const hardwareInfoRef = useRef(hardwareInfo);
  hardwareInfoRef.current = hardwareInfo;
  const activeCameraRef = useRef<string | null>((camera || "").trim());
  activeCameraRef.current = (camera || "").trim();
  const lifecycleRef = useRef<PtzMoveLifecycleController | null>(null);

  useEffect(() => {
    setShowPresetInput(false);
    setNewPresetName("");
    setShowHwDrawer(false);
    setHwBusy(false);
  }, [camera]);

  useEffect(() => {
    const cleanCam = (camera || "").trim();
    activeCameraRef.current = cleanCam;
    if (!ptzEnabled) {
      if (lifecycleRef.current) {
        void lifecycleRef.current.dispose();
        lifecycleRef.current = null;
      }
      return;
    }
    const ctrl = createPtzMoveLifecycle({
      camera: cleanCam,
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
      if (activeCameraRef.current === cleanCam) {
        activeCameraRef.current = null;
      }
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

  if (!ptzEnabled && !hwControlsSupported) {
    return null;
  }

  const { presets, activePreset, canManagePresets, zoomSupported } =
    capabilitySnapshot;

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
    const targetCam = (camera || "").trim();
    if (!clean || !targetCam) return;
    await lifecycleRef.current?.stopMove(false);
    try {
      const res = await fetch(`/api/${encodeURIComponent(targetCam)}/ptz/preset`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ preset: clean }),
      });
      if (
        res.ok &&
        activeCameraRef.current === targetCam &&
        onPtzUpdateRef.current
      ) {
        onPtzUpdateRef.current(targetCam, {
          ...(ptzInfoRef.current || { enabled: true }),
          active_preset: clean,
        });
      }
    } catch {
      // ignore
    }
  };

  const handleSaveCurrentPreset = async () => {
    const cleanName = newPresetName.trim();
    const targetCam = (camera || "").trim();
    if (!cleanName || !targetCam) return;
    try {
      const res = await fetch(
        `/api/${encodeURIComponent(targetCam)}/ptz/preset/save`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ name: cleanName }),
        }
      );
      const data = await res.json().catch(() => null);
      if (
        res.ok &&
        data?.success &&
        activeCameraRef.current === targetCam &&
        onPtzUpdateRef.current
      ) {
        const nextPtz = applySavedPresetToPtzInfo(
          ptzInfoRef.current || { enabled: true },
          String(data.token || cleanName),
          cleanName,
          Array.isArray(data.presets) ? data.presets : undefined,
          data.preset_names &&
            typeof data.preset_names === "object" &&
            !Array.isArray(data.preset_names)
            ? data.preset_names
            : undefined
        );
        if (nextPtz) {
          setCachedCameraPtzInfo(targetCam, nextPtz);
          onPtzUpdateRef.current(targetCam, nextPtz);
        }
        setNewPresetName("");
        setShowPresetInput(false);
      }
    } catch {
      // ignore
    }
  };

  const handleDeleteActivePreset = async () => {
    const targetCam = (camera || "").trim();
    if (!activePreset || !targetCam) return;
    const matchItem = (ptzInfo?.presets || []).find(
      (p) => p.name === activePreset || p.token === activePreset
    );
    const tokToDelete = matchItem?.token || activePreset;
    try {
      const res = await fetch(
        `/api/${encodeURIComponent(targetCam)}/ptz/preset/${encodeURIComponent(tokToDelete)}`,
        {
          method: "DELETE",
          credentials: "include",
        }
      );
      const data = await res.json().catch(() => null);
      if (
        res.ok &&
        activeCameraRef.current === targetCam &&
        onPtzUpdateRef.current
      ) {
        const nextPtz = applyDeletedPresetToPtzInfo(
          ptzInfoRef.current || { enabled: true },
          tokToDelete,
          Array.isArray(data?.presets) ? data.presets : undefined,
          data?.preset_names &&
            typeof data.preset_names === "object" &&
            !Array.isArray(data.preset_names)
            ? data.preset_names
            : undefined
        );
        if (nextPtz) {
          setCachedCameraPtzInfo(targetCam, nextPtz);
          onPtzUpdateRef.current(targetCam, nextPtz);
        }
      }
    } catch {
      // ignore
    }
  };

  const handleApplyHardwareField = async (patch: Record<string, unknown>) => {
    const targetCam = (camera || "").trim();
    if (!targetCam || hwBusy) return;
    setHwBusy(true);
    try {
      const res = await fetch(`/api/${encodeURIComponent(targetCam)}/hardware`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(patch),
      });
      const data = await res.json().catch(() => null);
      const curHw = hardwareInfoRef.current;
      if (
        res.ok &&
        activeCameraRef.current === targetCam &&
        curHw &&
        onHardwareUpdateRef.current
      ) {
        const nextHw = mergeOptimisticCameraHardwareInfo(curHw, patch, data);
        if (nextHw) {
          setCachedCameraHardware(targetCam, nextHw);
          onHardwareUpdateRef.current(targetCam, nextHw);
        }
      }
    } catch {
      // ignore
    } finally {
      if (activeCameraRef.current === targetCam) {
        setHwBusy(false);
      }
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
        flexWrap: "wrap",
        gap: 3,
        padding: "2px 5px",
        background: "rgba(7, 10, 15, 0.88)",
        border: "1px solid var(--border-strong)",
        borderRadius: 3,
        pointerEvents: "auto",
      }}
    >
      {ptzEnabled && (
        <>
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
          <button
            {...bindDirectionButtonProps("left", `PTZ Pan Left ${camera}`, "Pan Left")}
          >
            ◀
          </button>
          <button
            {...bindDirectionButtonProps("up", `PTZ Tilt Up ${camera}`, "Tilt Up")}
          >
            ▲
          </button>
          <button
            {...bindDirectionButtonProps("down", `PTZ Tilt Down ${camera}`, "Tilt Down")}
          >
            ▼
          </button>
          <button
            {...bindDirectionButtonProps("right", `PTZ Pan Right ${camera}`, "Pan Right")}
          >
            ▶
          </button>
          {zoomSupported && (
            <>
              <button
                {...bindDirectionButtonProps("zoom_in", `PTZ Zoom In ${camera}`, "Zoom In")}
              >
                Z+
              </button>
              <button
                {...bindDirectionButtonProps("zoom_out", `PTZ Zoom Out ${camera}`, "Zoom Out")}
              >
                Z-
              </button>
            </>
          )}
          <button
            type="button"
            className="nvr-btn danger nvr-tile-action mono sm"
            aria-label={`PTZ Stop ${camera}`}
            title="Stop PTZ Movement"
            onClick={() => stopContinuousMove(true)}
          >
            ■
          </button>
          {presets.length > 0 && (
            <select
              className="nvr-select mono sm"
              aria-label={`PTZ Preset Selector ${camera}`}
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
          {canManagePresets && (
            <>
              <button
                type="button"
                className="nvr-btn nvr-tile-action mono sm"
                style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
                aria-label={`Save PTZ Preset ${camera}`}
                title="Save current camera position as preset"
                onClick={() => setShowPresetInput((v) => !v)}
              >
                +P
              </button>
              {activePreset && (
                <button
                  type="button"
                  className="nvr-btn danger nvr-tile-action mono sm"
                  style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
                  aria-label={`Delete PTZ Preset ${camera}`}
                  title={`Delete preset '${activePreset}'`}
                  onClick={() => void handleDeleteActivePreset()}
                >
                  ×
                </button>
              )}
              {showPresetInput && (
                <span style={{ display: "inline-flex", alignItems: "center", gap: 2 }}>
                  <input
                    type="text"
                    className="nvr-input mono sm"
                    style={{ width: 78, height: 20, fontSize: "var(--text-xs)", padding: "0 4px" }}
                    placeholder="Preset..."
                    aria-label={`New Preset Name ${camera}`}
                    value={newPresetName}
                    onChange={(e) => setNewPresetName(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") {
                        e.preventDefault();
                        void handleSaveCurrentPreset();
                      }
                    }}
                  />
                  <button
                    type="button"
                    className="nvr-btn primary mono sm"
                    style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
                    aria-label={`Confirm Save Preset ${camera}`}
                    onClick={() => void handleSaveCurrentPreset()}
                  >
                    OK
                  </button>
                </span>
              )}
            </>
          )}
        </>
      )}
      {hwControlsSupported && hardwareInfo && (
        <>
          <button
            type="button"
            className={`nvr-btn nvr-tile-action mono sm ${showHwDrawer ? "active" : ""}`}
            style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
            aria-label={`Hardware Controls ${camera}`}
            aria-pressed={showHwDrawer}
            title="Camera Hardware Controls (IR, SmartTrack, LED, Flip/Mirror, Image)"
            onClick={() => setShowHwDrawer((v) => !v)}
          >
            HW
          </button>
          {showHwDrawer && (
            <span
              style={{
                display: "inline-flex",
                alignItems: "center",
                gap: 3,
                paddingLeft: 3,
                borderLeft: "1px solid var(--border-strong)",
              }}
            >
              {CAMERA_HUD_CONTROL_DESCRIPTORS.map((desc) => {
                if (!hardwareInfo.capabilities[desc.capability]) {
                  return null;
                }
                if (desc.kind === "select") {
                  const options = desc.resolveSelectOptions
                    ? desc.resolveSelectOptions(hardwareInfo.state)
                    : [];
                  const curVal = desc.resolveSelectValue
                    ? desc.resolveSelectValue(hardwareInfo.state)
                    : String(hardwareInfo.state[desc.id] ?? "");
                  return (
                    <select
                      key={desc.id}
                      className="nvr-select mono sm"
                      style={{ height: 20, fontSize: "var(--text-xs)", padding: "0 4px" }}
                      aria-label={desc.ariaLabel(camera)}
                      title={desc.title}
                      disabled={hwBusy}
                      value={curVal}
                      onChange={(e) => {
                        const patch = desc.parseSelectPatch
                          ? desc.parseSelectPatch(e.target.value)
                          : { [desc.id]: e.target.value };
                        void handleApplyHardwareField(patch);
                      }}
                    >
                      {options.map((opt) => (
                        <option key={String(opt.value)} value={opt.value}>
                          {opt.label}
                        </option>
                      ))}
                    </select>
                  );
                }
                const isActive = Boolean(hardwareInfo.state[desc.id]);
                return (
                  <button
                    key={desc.id}
                    type="button"
                    disabled={hwBusy}
                    aria-label={desc.ariaLabel(camera)}
                    aria-pressed={isActive}
                    className={`nvr-btn nvr-tile-action mono sm ${
                      isActive ? "active" : ""
                    }`}
                    style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
                    title={desc.title}
                    onClick={() =>
                      void handleApplyHardwareField({
                        [desc.id]: !isActive,
                      })
                    }
                  >
                    {desc.shortLabel || desc.id.toUpperCase()}
                  </button>
                );
              })}
              {hardwareInfo.capabilities.manual_siren && (
                <button
                  type="button"
                  disabled={hwBusy}
                  aria-label={`Trigger Siren ${camera}`}
                  className="nvr-btn danger nvr-tile-action mono sm"
                  style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
                  title="Trigger 3.5s audible siren on camera speaker"
                  onClick={() =>
                    void handleApplyHardwareField({
                      trigger_siren: true,
                    })
                  }
                >
                  ALM
                </button>
              )}
            </span>
          )}
        </>
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
  const { t, locale } = useI18n();
  const [hoveredAlertId, setHoveredAlertId] = useState<string | null>(null);
  const setActiveTab = useNvrStore((s) => s.setActiveTab);
  const [stripNowSec, setStripNowSec] = useState<number>(() => Math.floor(Date.now() / 1000));
  const [clipRequest, setClipRequest] = useState<TimelineClipRequest | null>(null);
  const recentEvents = useMemo(() => selectRecentEvents(unreviewedAlerts, 10), [unreviewedAlerts]);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const [stageSize, setStageSize] = useState<{ width: number; height: number } | null>(null);


  // Relative event ages ("4m ago") only need a coarse clock.
  useEffect(() => {
    const timer = window.setInterval(() => setStripNowSec(Math.floor(Date.now() / 1000)), 15000);
    return () => window.clearInterval(timer);
  }, []);
  const [hoveredDockCam, setHoveredDockCam] = useState<string | null>(null);

  const liveLayout = useNvrStore((s) => s.liveLayout);
  const setLiveLayout = useNvrStore((s) => s.setLiveLayout);

  // Measure the matrix stage so 16:9 tiles can be centred inside it.
  useEffect(() => {
    const el = stageRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect;
      if (!rect) return;
      const width = Math.round(rect.width);
      const height = Math.round(rect.height);
      setStageSize((prev) =>
        prev && prev.width === width && prev.height === height ? prev : { width, height }
      );
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [liveLayout]);
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
  const [ptzInfoByCam, setPtzInfoByCam] = useState<Record<string, CameraPtzInfoResponse>>(
    () => cameraHardwareCache.getSnapshot().ptzByCam
  );
  const [hardwareInfoByCam, setHardwareInfoByCam] = useState<
    Record<string, CameraHardwareInfoResponse>
  >(() => cameraHardwareCache.getSnapshot().hardwareByCam);
  const [pttResetEpoch, setPttResetEpoch] = useState<number>(0);
  const activePttCamerasRef = useRef<Set<string>>(new Set());
  const isMountedRef = useRef<boolean>(true);

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    const releaseActivePtt = () => {
      setHoveredAlertId(null);
      setHoveredDockCam(null);
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
  }, [
    liveLayout,
    selectedCamera,
    matrixPage,
    matrixDensity,
    activeCameraGroup,
    timeShiftTs,
    simulatedFleetCount,
  ]);

  useEffect(() => {
    return subscribeCameraHardwareCache(() => {
      if (!isMountedRef.current) return;
      const snap = cameraHardwareCache.getSnapshot();
      setHardwareInfoByCam(snap.hardwareByCam);
      setPtzInfoByCam(snap.ptzByCam);
    });
  }, []);

  useEffect(() => {
    const cams = config?.cameras || {};
    for (const [camName, camCfg] of Object.entries(cams)) {
      void probeCameraHardwareAndPtz(camName, {
        cameraConfig: camCfg,
        timeoutMs: 3500,
      }).then((snap) => {
        if (!isMountedRef.current) return;
        if (snap.hardware) {
          setHardwareInfoByCam((prev) => ({
            ...prev,
            [camName]: snap.hardware!,
          }));
        }
        if (snap.ptz) {
          setPtzInfoByCam((prev) => ({
            ...prev,
            [camName]: snap.ptz!,
          }));
        }
      });
    }
  }, [config?.cameras]);

  const handlePtzUpdate = (camName: string, nextInfo: CameraPtzInfoResponse) => {
    setPtzInfoByCam((prev) => {
      const merged = { ...(prev[camName] || {}), ...nextInfo };
      setCachedCameraPtzInfo(camName, merged);
      return {
        ...prev,
        [camName]: merged,
      };
    });
  };

  const handleHardwareUpdate = (
    camName: string,
    nextHw: CameraHardwareInfoResponse
  ) => {
    setCachedCameraHardware(camName, nextHw);
    setHardwareInfoByCam((prev) => ({
      ...prev,
      [camName]: nextHw,
    }));
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

  const matrixDensityOptions = useMemo(
    () => resolveMatrixDensityOptions(groupCams.length, matrixDensity),
    [groupCams.length, matrixDensity]
  );

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

  /** Opens the timeline's Export clip mode on `camera` with the given range. */
  const requestClip = (camera: string, startSec: number, endSec: number) => {
    if (!Number.isFinite(startSec) || !Number.isFinite(endSec)) return;
    selectCameraAndRevealOnStage(camera);
    setClipRequest({
      camera,
      start: Math.floor(Math.min(startSec, endSec)),
      end: Math.ceil(Math.max(startSec, endSec)),
      nonce: Date.now(),
    });
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
  const isCompact = useCompactViewport();
  const [detailsCam, setDetailsCam] = useState<string | null>(null);
  const [ptzOpenCam, setPtzOpenCam] = useState<string | null>(null);
  const [compactColumns, setCompactColumns] = useState<CompactGridColumns>(() => {
    try {
      return sanitizeCompactGridColumns(
        typeof window !== "undefined"
          ? window.localStorage.getItem("ninjanvr.live.compactColumns")
          : null
      );
    } catch {
      return 1;
    }
  });
  const updateCompactColumns = (next: CompactGridColumns) => {
    setCompactColumns(next);
    try {
      window.localStorage.setItem("ninjanvr.live.compactColumns", String(next));
    } catch {
      // storage unavailable (private mode) — preference stays in memory
    }
  };
  const toggleCameraRuntimeFlag = (realCam: string, flag: CameraRuntimeFlag, next: boolean) => {
    fetch(`/api/${encodeURIComponent(realCam)}/state`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ [flag]: next }),
    }).catch(() => {});
  };

  const stageGroupCams =
    groupCams.length > MAX_DOCK_LIVE_DECODERS + 1 &&
    visibleMatrixCams.length > 0
      ? [
          ...visibleMatrixCams,
          ...groupCams.filter((c) => !visibleMatrixCams.includes(c)),
        ]
      : groupCams;

  const allStageDescriptors = computeLiveStageDescriptors({
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
  const stageDescriptors = isCompact
    ? filterCompactStageDescriptors(liveLayout, allStageDescriptors)
    : allStageDescriptors;

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <div className="nvr-segmented" role="group" aria-label={t.live.viewMode}>
            {(
              [
                ["2x2", t.live.matrixMode, undefined],
                ["focus", t.live.focusStage, undefined],
                ["single", t.live.singleFull, undefined],
                [
                  "spotlight",
                  t.live.spotlightMode,
                  "Zero-CPU Dynamic Birdseye: automatically enlarges cameras with active motion or tracked objects",
                ],
                [
                  "map",
                  t.live.tacticalMap,
                  "Interactive Tactical Map / Floorplan with FOV cones and real-time motion/object pulses",
                ],
              ] as Array<[LiveLayoutMode, string, string | undefined]>
            )
              .filter(([mode]) => !isCompact || mode === "2x2" || mode === "focus" || mode === "spotlight")
              .map(([mode, label, hint]) => (
              <button
                key={mode}
                type="button"
                className={`nvr-btn ${liveLayout === mode ? "active" : ""}`}
                aria-pressed={liveLayout === mode}
                title={hint}
                onClick={() => {
                  setLiveLayout(mode);
                  if (mode !== "focus" && mode !== "single") setTimeShiftTs(null);
                }}
              >
                {isCompact
                  ? mode === "2x2"
                    ? t.live.modeShortGrid
                    : mode === "focus"
                    ? t.live.modeShortCamera
                    : t.live.modeShortSpotlight
                  : label}
              </button>
            ))}
          </div>

          {liveLayout === "2x2" && isCompact && (
            <div className="nvr-segmented nvr-compact-cols" role="group" aria-label={t.live.gridColumns}>
              {([1, 2] as CompactGridColumns[]).map((cols) => (
                <button
                  key={cols}
                  type="button"
                  data-testid={`compact-columns-${cols}`}
                  className={`nvr-btn ${compactColumns === cols ? "active" : ""}`}
                  aria-pressed={compactColumns === cols}
                  aria-label={`${t.live.gridColumns}: ${cols}`}
                  onClick={() => updateCompactColumns(cols)}
                >
                  {cols === 1 ? (
                    <Rows2 size={18} aria-hidden="true" />
                  ) : (
                    <Grid2x2 size={18} aria-hidden="true" />
                  )}
                </button>
              ))}
            </div>
          )}

          {liveLayout === "2x2" && !isCompact && (
            <>
              <span className="nvr-subbar-label mono">{t.live.density}</span>
              <div className="nvr-segmented mono" role="group" aria-label={t.live.density}>
                {matrixDensityOptions.options.map((opt) => (
                  <button
                    key={opt.density}
                    type="button"
                    className={`nvr-btn ${opt.active ? "active" : ""}`}
                    aria-pressed={opt.active}
                    disabled={!opt.enabled}
                    title={
                      opt.enabled
                        ? `${opt.density.replace("x", "×")} · ${getMatrixPageSize(opt.density)} ${
                            t.live.allCameras.toLowerCase()
                          }`
                        : t.live.densityNeedsMore.replace("{n}", String(opt.needsMoreThan))
                    }
                    onClick={() => {
                      setMatrixDensity(opt.density);
                      setTimeShiftTs(null);
                    }}
                  >
                    {opt.density.replace("x", "×")}
                  </button>
                ))}
              </div>
            </>
          )}

          {cameraGroupEntries.length > 0 && (
            <>
              <span className="nvr-divider" aria-hidden="true" />
              <button
                type="button"
                className={`nvr-btn mono ${
                  activeCameraGroup === "all" ? "active" : ""
                } sm`}
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
                  } sm`} style={{ textTransform: "uppercase" }}
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
                className="nvr-btn mono sm"
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
                className="nvr-btn mono sm"
                aria-label="Next matrix page"
                onClick={() =>
                  setMatrixPage((p) => nextMatrixTourPage(p, totalPages))
                }
              >
                <ChevronRight size={12} />
              </button>
              <button
                type="button"
                className={`nvr-btn mono ${matrixTourEnabled ? "active" : ""} sm`}
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
                  className={`nvr-btn mono ${spotlightFilter === flt ? "active" : ""} sm`}
                  onClick={() => setSpotlightFilter(flt)}
                >
                  {lbl}
                </button>
              ))}
              <select
                className="nvr-select mono sm"
                aria-label="Spotlight hold duration"
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
                } sm`}
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
                } sm`}
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
                    className="nvr-btn success mono sm"
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
                        className={`nvr-btn mono ${isCurrent ? "active" : ""} sm`}
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
          {!isCompact && (liveLayout === "focus" || liveLayout === "single") && (
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

        <div className="nvr-desktop-only" style={{ display: "flex", alignItems: "center", gap: 6 }}>
          {simulatedFleetCount > 0 && (
            <button
              type="button"
              className="nvr-btn danger mono sm"
              aria-label={`Exit Simulation (Restore ${cameraNames.filter((c) => !/_sim_\d+$/.test(c)).length || 4} Cameras)`}
              onClick={() => setSimulatedFleetCount(0)}
            >
              Exit Simulation ({simulatedFleetCount} Cams → {cameraNames.filter((c) => !/_sim_\d+$/.test(c)).length || 4})
            </button>
          )}
          <select
            className="nvr-select mono"
            aria-label="Camera HUD badge position"
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
        </div>
      </div>

      {/* Recent events strip: each card loops its still frames as a flipbook (<img> swaps,
          no video decoder). The full MP4 preview still mounts ONLY on pointer hover. */}
      {isCompact &&
        liveLayout === "2x2" &&
        showAlertRibbon &&
        recentEvents.length > 0 &&
        (() => {
          const latest = recentEvents[0];
          const label = ((latest.data?.objects || [])[0] || latest.severity).toString();
          return (
            <div className="nvr-event-summary" data-testid="compact-event-summary">
              <button
                type="button"
                className="nvr-event-summary-main"
                aria-label={`${t.live.eventTimeShift}: ${latest.camera} ${label}`}
                onClick={() => {
                  setSelectedCamera(latest.camera);
                  setLiveLayout("focus");
                  setTimeShiftTs(sanitizeTimeShiftSeek(latest.start_time));
                }}
              >
                <img
                  src={resolveSafeAlertThumbSrc(latest.thumb_path, latest.camera)}
                  alt=""
                  className={`nvr-event-summary-thumb severity-${latest.severity}`}
                />
                <span className="nvr-event-summary-text">
                  <span className="mono nvr-event-summary-count">
                    {unreviewedAlerts.length} {t.live.unreviewedShort}
                  </span>
                  <span className="nvr-event-summary-detail">
                    <span className="nvr-event-summary-label">{label}</span> · {latest.camera} ·{" "}
                    {formatRelativeAgo(stripNowSec, latest.start_time, locale === "pt")}
                  </span>
                </span>
              </button>
              <button
                type="button"
                className="nvr-btn primary nvr-event-summary-cta"
                onClick={() => setActiveTab("review")}
              >
                {t.live.openReview}
              </button>
            </div>
          );
        })()}

      {!isCompact &&
        shouldRenderLiveAlertRibbon(
        liveLayout,
        showAlertRibbon,
        unreviewedAlerts.length
      ) && (
        <div className="nvr-alert-ribbon" aria-label={t.live.recentEvents}>
          <div className="nvr-event-strip-head">
            <span className="mono nvr-event-strip-count">
              {unreviewedAlerts.length} {t.live.unreviewedShort}
            </span>
            <button
              type="button"
              className="nvr-btn sm"
              onClick={() => setActiveTab("review")}
            >
              {t.live.openReview} →
            </button>
            <button
              type="button"
              className="nvr-btn sm success"
              onClick={() =>
                handleMarkReviews(
                  unreviewedAlerts.map((r) => r.id),
                  true
                )
              }
            >
              <Check size={12} />
              {t.live.markAllReviewed}
            </button>
          </div>

          {recentEvents.map((item) => {
            const isHovered = hoveredAlertId === item.id;
            const thumbSrc = resolveSafeAlertThumbSrc(
              item.thumb_path,
              item.camera
            );
            const label = (item.data?.objects || [])[0] || item.severity;
            const timeShiftToEvent = () => {
              setSelectedCamera(item.camera);
              setLiveLayout("focus");
              setTimeShiftTs(sanitizeTimeShiftSeek(item.start_time));
            };

            return (
              <div
                key={item.id}
                role="button"
                tabIndex={0}
                className={`nvr-alert-card nvr-event-card severity-${item.severity}`}
                title={`${item.camera.toUpperCase()} · ${formatTs(item.start_time)} · ${t.live.eventTimeShift}`}
                onPointerEnter={() => setHoveredAlertId(item.id)}
                onPointerLeave={() =>
                  setHoveredAlertId((curr) => (curr === item.id ? null : curr))
                }
                onPointerCancel={() =>
                  setHoveredAlertId((curr) => (curr === item.id ? null : curr))
                }
                onBlur={() =>
                  setHoveredAlertId((curr) => (curr === item.id ? null : curr))
                }
                onClick={timeShiftToEvent}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    timeShiftToEvent();
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
                    onError={() =>
                      setHoveredAlertId((curr) => (curr === item.id ? null : curr))
                    }
                  />
                ) : (
                  <ReviewFlipbook
                    reviewId={item.id}
                    fallbackSrc={thumbSrc}
                    alt={`${item.camera} ${label} event`}
                  />
                )}
                <div className="nvr-alert-card-hud">
                  <div className="nvr-event-card-row">
                    <span className="mono nvr-event-card-cam">{item.camera}</span>
                    <span className="mono nvr-event-card-label">{label}</span>
                  </div>
                  <div className="nvr-event-card-row">
                    <span className="mono nvr-event-card-ago">
                      {formatRelativeAgo(stripNowSec, item.start_time, locale === "pt")}
                    </span>
                    <span className="nvr-event-card-actions">
                      <button
                        type="button"
                        className="nvr-btn sm"
                        title={t.live.exportThisEvent}
                        aria-label={`${t.live.exportThisEvent}: ${item.camera} ${formatTs(item.start_time)}`}
                        onClick={(e) => {
                          e.stopPropagation();
                          // Park the playhead on the event so "This event" and Preview line up.
                          setTimeShiftTs(sanitizeTimeShiftSeek(item.start_time));
                          requestClip(item.camera, item.start_time - 5, (item.end_time ?? item.start_time + 15) + 5);
                        }}
                      >
                        <Scissors size={11} />
                      </button>
                      <button
                        type="button"
                        className="nvr-btn sm success"
                        title={t.live.markReviewed}
                        aria-label={`${t.live.markReviewed}: ${item.camera} ${formatTs(item.start_time)}`}
                        onClick={(e) => {
                          e.stopPropagation();
                          handleMarkReviews([item.id], true);
                        }}
                      >
                        <Check size={11} />
                      </button>
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
          ref={stageRef}
          style={
            (isCompact
              ? resolveCompactStageContainerStyle(liveLayout, compactColumns)
              : undefined) ??
            (liveLayout === "2x2"
              ? resolveAspectMatrixGridStyle(stageSize, visibleMatrixCams.length)
              : undefined) ??
            resolvePersistentStageContainerStyle(
              liveLayout,
              stageDescriptors,
              matrixDensity,
              resolveAdaptiveMatrixGridStyle(
                visibleMatrixCams.length,
                matrixDensity
              )
            )
          }
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
            const roiBadges = resolveCameraRoiStateBadges(camCfg, act, camStat);
            const tileException = resolveTileException({
              tamper: Boolean(act?.tamper),
              objectLabels: activeLabels,
              roiAnomalies: roiBadges.filter((b) => b.isAnomaly).map((b) => b.label),
              recordEnabled: act?.config?.record,
              motion: hasMotion,
            });
            const identityTone = resolveTileIdentityTone({
              timeShifted: isTimeShifted,
              cameraFps: camStat?.camera_fps,
              hasStats: Boolean(camStat),
            });
            const ptzCapable =
              isCameraPtzEnabled(camCfg, ptzInfoByCam[realCam], hardwareInfoByCam[realCam]) ||
              isCameraHardwareControlsSupported(hardwareInfoByCam[realCam]);
            const exceptionAtBottom = !isCompact && hudPosition.startsWith("top");

            const handleTileActivate = (isDoubleClick = false) => {
              const nowMs = Date.now();
              if (nowMs - lastLayoutSwitchMsRef.current < 320) {
                return;
              }
              if (isCompact && isGrid) {
                // Phones: one tap opens the camera screen (no hover, no double-tap).
                lastLayoutSwitchMsRef.current = nowMs;
                triggerMainStreamPrewarmIfNeeded(realCam, true);
                setSelectedCamera(cam);
                setLiveLayout("focus");
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
                style={isCompact ? undefined : resolvePersistentTileStyle(liveLayout, descriptor)}
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
                    pushToTalkSupported={
                      resolveCameraPushToTalkCapability(
                        camCfg,
                        hardwareInfoByCam[realCam],
                        config?.go2rtc?.streams,
                        realCam
                      ).supported
                    }
                    pushToTalkMode={
                      resolveCameraPushToTalkCapability(
                        camCfg,
                        hardwareInfoByCam[realCam],
                        config?.go2rtc?.streams,
                        realCam
                      ).mode
                    }
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
                  </div>
                ) : (
                  <>
                    {tileException && (
                      <div
                        className={`nvr-tile-exception ${exceptionAtBottom ? "at-bottom" : ""}`}
                      >
                        <span
                          data-testid={`tile-exception-${cam}`}
                          className={`nvr-badge ${tileException.tone} mono`}
                          title={tileException.hiddenCount > 0 ? "More alerts in Camera Details" : undefined}
                        >
                          {tileException.label}
                          {tileException.hiddenCount > 0 ? ` +${tileException.hiddenCount}` : ""}
                        </span>
                      </div>
                    )}
                    <div
                      className={`nvr-cam-hud nvr-tile-bar pos-${
                        isCompact && hudPosition !== "hidden" ? "bottom-left" : hudPosition
                      }`}
                    >
                      <span
                        data-testid={`tile-identity-${cam}`}
                        className={`nvr-badge nvr-tile-identity ${
                          identityTone === "rewind"
                            ? "rewind"
                            : descriptor.isSpotlightHeld || identityTone === "offline"
                            ? "alert"
                            : "live"
                        }`}
                      >
                        <span
                          className={`nvr-dot ${
                            identityTone === "live" && !descriptor.isSpotlightHeld
                              ? "live"
                              : identityTone === "offline"
                              ? "alert"
                              : "motion"
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
                      <div
                        className="nvr-tile-actions"
                        onClick={(e) => e.stopPropagation()}
                        onDoubleClick={(e) => e.stopPropagation()}
                      >
                        {isTimeShifted &&
                          (isHero ? (
                            <button
                              type="button"
                              className="nvr-btn success sm"
                              onClick={() => setTimeShiftTs(null)}
                            >
                              <Play size={12} />
                              {t.live.returnToLive}
                            </button>
                          ) : (
                            <button
                              type="button"
                              className="nvr-btn success mono sm"
                              onClick={() => setTimeShiftTs(null)}
                            >
                              LIVE
                            </button>
                          ))}
                        {isHero && liveLayout === "spotlight" && !isTimeShifted && (
                          <>
                            <button
                              type="button"
                              data-testid="spotlight-hero-rewind30s"
                              className="nvr-btn mono sm"
                              onClick={() => {
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
                              className="nvr-btn mono sm"
                              onClick={() => {
                                triggerMainStreamPrewarmIfNeeded(realCam, true);
                                setSelectedCamera(cam);
                                setLiveLayout("focus");
                              }}
                            >
                              {t.live.focusCameraBtn}
                            </button>
                          </>
                        )}
                        {isGrid && (
                          <button
                            type="button"
                            className="nvr-tile-icon"
                            title={t.live.focusCameraBtn}
                            aria-label={`${t.live.focusCameraBtn}: ${cam}`}
                            onClick={() => {
                              triggerMainStreamPrewarmIfNeeded(realCam, true);
                              setSelectedCamera(cam);
                              setLiveLayout("focus");
                            }}
                          >
                            <Maximize2 size={15} />
                          </button>
                        )}
                        {!isTimeShifted && liveLayout !== "spotlight" && (
                          <button
                            type="button"
                            className="nvr-tile-icon"
                            title={t.live.rewind30s}
                            aria-label={`${t.live.rewind30s}: ${cam}`}
                            onClick={() => {
                              setSelectedCamera(cam);
                              setTimeShiftTs(Math.floor(Date.now() / 1000) - 30);
                            }}
                          >
                            <RotateCcw size={15} />
                          </button>
                        )}
                        <a
                          href={buildCameraSnapshotUrl(realCam)}
                          download
                          className="nvr-tile-icon"
                          data-testid={`tile-snapshot-${cam}`}
                          title={t.live.snapshotHint}
                          aria-label={`${t.live.snapshot}: ${cam}`}
                        >
                          <Camera size={15} />
                        </a>
                        <button
                          type="button"
                          className="nvr-tile-icon"
                          title={t.live.clipLast30sHint}
                          aria-label={`${t.live.clipLast30s}: ${cam}`}
                          onClick={() => {
                            const nowSec = Math.floor(Date.now() / 1000);
                            requestClip(cam, nowSec - 30, nowSec);
                          }}
                        >
                          <Scissors size={15} />
                        </button>
                        {!isTimeShifted && (
                          <PushToTalkButton
                            key={`ptt-${cam}-${realCam}-${pttResetEpoch}`}
                            camera={realCam}
                            height={28}
                            supported={
                              resolveCameraPushToTalkCapability(
                                camCfg,
                                hardwareInfoByCam[realCam],
                                config?.go2rtc?.streams,
                                realCam
                              ).supported
                            }
                            mode={
                              resolveCameraPushToTalkCapability(
                                camCfg,
                                hardwareInfoByCam[realCam],
                                config?.go2rtc?.streams,
                                realCam
                              ).mode
                            }
                            onStateChange={(state) => {
                              if (state === "connecting" || state === "talking") {
                                activePttCamerasRef.current.add(cam);
                              } else {
                                activePttCamerasRef.current.delete(cam);
                              }
                            }}
                          />
                        )}
                        {ptzCapable && (
                          <button
                            type="button"
                            className={`nvr-tile-icon ${ptzOpenCam === cam ? "active" : ""}`}
                            aria-pressed={ptzOpenCam === cam}
                            data-testid={`tile-ptz-toggle-${cam}`}
                            title="PTZ"
                            aria-label={`PTZ: ${cam}`}
                            onClick={() => setPtzOpenCam((c) => (c === cam ? null : cam))}
                          >
                            <Move size={15} />
                          </button>
                        )}
                        <button
                          type="button"
                          className="nvr-tile-icon"
                          data-testid={`tile-details-${cam}`}
                          title={t.live.cameraDetails}
                          aria-label={`${t.live.cameraDetails}: ${cam}`}
                          onClick={() => setDetailsCam(cam)}
                        >
                          <MoreHorizontal size={15} />
                        </button>
                      </div>
                    </div>
                    {ptzCapable && ptzOpenCam === cam && (
                      <div className="nvr-tile-ptz">
                        <TacticalPtzHudOverlay
                          camera={realCam}
                          cameraConfig={camCfg}
                          ptzInfo={ptzInfoByCam[realCam]}
                          hardwareInfo={hardwareInfoByCam[realCam]}
                          onPtzUpdate={handlePtzUpdate}
                          onHardwareUpdate={handleHardwareUpdate}
                        />
                      </div>
                    )}
                  </>
                )}
              </div>
            );
          })}
        </div>
      )}

      {isCompact &&
        liveLayout !== "2x2" &&
        liveLayout !== "map" &&
        (() => {
          const cam = liveLayout === "spotlight" ? spotlightHeroCam : selectedCamera;
          if (!cam) return null;
          const realCam = resolveRealCameraSource(cam, config?.cameras || cameraNames);
          const camCfg = effectiveCamerasConfig?.[cam] ?? config?.cameras?.[realCam];
          const ptt = resolveCameraPushToTalkCapability(
            camCfg,
            hardwareInfoByCam[realCam],
            config?.go2rtc?.streams,
            realCam
          );
          const ptzCapable =
            isCameraPtzEnabled(camCfg, ptzInfoByCam[realCam], hardwareInfoByCam[realCam]) ||
            isCameraHardwareControlsSupported(hardwareInfoByCam[realCam]);
          return (
            <div className="nvr-cam-actionbar" role="toolbar" aria-label={`${cam} actions`}>
              {timeShiftTs !== null ? (
                <button
                  type="button"
                  className="nvr-cam-action is-live"
                  onClick={() => setTimeShiftTs(null)}
                >
                  <Play size={20} aria-hidden="true" />
                  <span>{t.live.returnToLive}</span>
                </button>
              ) : (
                <button
                  type="button"
                  className="nvr-cam-action"
                  onClick={() => setTimeShiftTs(Math.floor(Date.now() / 1000) - 30)}
                >
                  <RotateCcw size={20} aria-hidden="true" />
                  <span>{t.live.rewind30s}</span>
                </button>
              )}
              <a
                href={buildCameraSnapshotUrl(realCam)}
                download
                className="nvr-cam-action"
                data-testid={`action-snapshot-${cam}`}
              >
                <Camera size={20} aria-hidden="true" />
                <span>{t.live.snapshot}</span>
              </a>
              <button
                type="button"
                className="nvr-cam-action"
                onClick={() => {
                  const nowSec = Math.floor(Date.now() / 1000);
                  requestClip(cam, nowSec - 30, nowSec);
                }}
              >
                <Scissors size={20} aria-hidden="true" />
                <span>{t.live.clipLast30s}</span>
              </button>
              {ptt.supported && timeShiftTs === null && (
                <div className="nvr-cam-action is-ptt">
                  <PushToTalkButton
                    key={`ptt-bar-${cam}-${realCam}-${pttResetEpoch}`}
                    camera={realCam}
                    height={44}
                    supported={ptt.supported}
                    mode={ptt.mode}
                  />
                </div>
              )}
              {ptzCapable && (
                <button
                  type="button"
                  className={`nvr-cam-action ${ptzOpenCam === cam ? "active" : ""}`}
                  aria-pressed={ptzOpenCam === cam}
                  onClick={() => setPtzOpenCam((c) => (c === cam ? null : cam))}
                >
                  <Move size={20} aria-hidden="true" />
                  <span>PTZ</span>
                </button>
              )}
              <button
                type="button"
                className="nvr-cam-action"
                data-testid={`action-details-${cam}`}
                onClick={() => setDetailsCam(cam)}
              >
                <MoreHorizontal size={20} aria-hidden="true" />
                <span>{t.live.detailsShort}</span>
              </button>
            </div>
          );
        })()}

      {liveLayout === "map" && (
        <FloorplanStudioCanvas
          cameraNames={cameraNames}
          effectiveCameraNames={effectiveCameraNames}
          effectiveCamerasConfig={effectiveCamerasConfig}
          effectiveCameraGroups={effectiveCameraGroups}
          groupCams={groupCams}
          selectedCamera={selectedCamera}
          onSelectCamera={setSelectedCamera}
          onOpenFocus={() => setLiveLayout("focus")}
          config={config}
          onConfigSaved={setConfig}
          stats={stats}
          cameraActivity={cameraActivity}
          hardwareInfoByCam={hardwareInfoByCam}
          showVectors={showVectors}
          mapTheme={mapTheme}
        />
      )}

      {detailsCam &&
        (() => {
          const realCam = resolveRealCameraSource(detailsCam, config?.cameras || cameraNames);
          const camCfg = effectiveCamerasConfig?.[detailsCam] ?? config?.cameras?.[realCam];
          const camStat = stats?.cameras?.[detailsCam] ?? stats?.cameras?.[realCam];
          const act = cameraActivity[detailsCam] ?? cameraActivity[realCam];
          return (
            <CameraDetailsSheet
              camera={detailsCam}
              isPt={locale === "pt"}
              telemetry={formatCameraTelemetryRows({
                detectWidth: camCfg?.detect?.width,
                detectHeight: camCfg?.detect?.height,
                cameraFps: camStat?.camera_fps,
                detectionFps: camStat?.detection_fps,
                nightMode: Boolean(act?.night_mode || camStat?.night_mode),
                isPt: locale === "pt",
              })}
              flags={resolveCameraRuntimeFlags(act?.config, act?.enabled)}
              onToggleFlag={(flag, next) => toggleCameraRuntimeFlag(realCam, flag, next)}
              useMainStream={useMainStream}
              onSetMainStream={setUseMainStream}
              showVectors={showVectors}
              onSetShowVectors={setShowVectors}
              onClose={() => setDetailsCam(null)}
            />
          );
        })()}

      {!(isCompact && liveLayout === "2x2") &&
        shouldRenderSpotlightTimeline(
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
          clipRequest={clipRequest}
          onOpenExports={() => setActiveTab("exports")}
          titlePrefix="TIMELINE"
        />
      )}
    </>
  );
};
