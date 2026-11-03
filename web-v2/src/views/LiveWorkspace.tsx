import React, { useEffect, useMemo, useRef, useState } from "react";
import { Camera, Check, ChevronLeft, ChevronRight, Eye, Play, RefreshCw } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import {
  CameraConfig,
  CameraGroupConfig,
  CameraMapPlacement,
  CameraPtzInfoResponse,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import { HudPosition, MatrixDensity, SpotlightFilter, formatTs } from "../utils/format";
import { LiveMsePlayer, VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";
import { TacticalCameraPicker } from "../components/TacticalCameraPicker";

export const MAX_DOCK_LIVE_DECODERS = 6;

export function getMatrixPageSize(density: MatrixDensity): number {
  if (density === "3x3") return 9;
  if (density === "4x4") return 16;
  return 4;
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

export function computeDefaultMapPlacement(
  index: number,
  totalCameras: number
): CameraMapPlacement {
  const n = Math.max(1, totalCameras);
  const cols = Math.max(1, Math.ceil(Math.sqrt(n)));
  const rows = Math.max(1, Math.ceil(n / cols));
  const safeIdx = Math.max(0, Math.min(index, n - 1));
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

function buildFovConeSvgPath(
  cx: number,
  cy: number,
  angleDeg: number,
  fovDeg: number,
  radius: number
): string {
  const half = Math.max(10, Math.min(89, fovDeg / 2));
  const startRad = ((angleDeg - half) * Math.PI) / 180;
  const endRad = ((angleDeg + half) * Math.PI) / 180;
  const x1 = cx + radius * Math.cos(startRad);
  const y1 = cy + radius * Math.sin(startRad);
  const x2 = cx + radius * Math.cos(endRad);
  const y2 = cy + radius * Math.sin(endRad);
  const largeArc = fovDeg > 180 ? 1 : 0;
  return `M ${cx.toFixed(1)} ${cy.toFixed(1)} L ${x1.toFixed(1)} ${y1.toFixed(
    1
  )} A ${radius.toFixed(1)} ${radius.toFixed(1)} 0 ${largeArc} 1 ${x2.toFixed(
    1
  )} ${y2.toFixed(1)} Z`;
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

export function buildPtzMovePayload(
  direction: "left" | "right" | "up" | "down" | "zoom_in" | "zoom_out",
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
  const spd = Math.max(0.05, Math.min(1.0, speed));
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

export const TacticalPtzHudOverlay: React.FC<{
  camera: string;
  cameraConfig?: CameraConfig;
  ptzInfo?: CameraPtzInfoResponse | null;
  onPtzUpdate?: (camera: string, nextInfo: CameraPtzInfoResponse) => void;
}> = ({ camera, cameraConfig, ptzInfo, onPtzUpdate }) => {
  if (!isCameraPtzEnabled(cameraConfig, ptzInfo)) {
    return null;
  }

  const presets = resolveCameraPtzPresets(cameraConfig, ptzInfo);
  const activePreset =
    ptzInfo?.active_preset !== undefined
      ? ptzInfo.active_preset
      : cameraConfig?.onvif?.return_preset || null;

  const sendMove = async (
    dir: "left" | "right" | "up" | "down" | "zoom_in" | "zoom_out"
  ) => {
    const payload = buildPtzMovePayload(dir, 0.5, "continuous");
    try {
      await fetch(`/api/${encodeURIComponent(camera)}/ptz/move`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(payload),
      });
      if (onPtzUpdate) {
        onPtzUpdate(camera, {
          ...(ptzInfo || { enabled: true }),
          active_preset: null,
        });
      }
    } catch {
      // ignore transient network errors
    }
  };

  const sendStop = async () => {
    try {
      await fetch(`/api/${encodeURIComponent(camera)}/ptz/stop`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ pan_tilt: true, zoom: true }),
      });
    } catch {
      // ignore
    }
  };

  const sendPreset = async (preset: string) => {
    const clean = preset.trim();
    if (!clean) return;
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
      <button
        type="button"
        className="nvr-btn nvr-tile-action mono"
        style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
        aria-label={`PTZ Pan Left ${camera}`}
        title="Pan Left"
        onClick={() => sendMove("left")}
      >
        ◀
      </button>
      <button
        type="button"
        className="nvr-btn nvr-tile-action mono"
        style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
        aria-label={`PTZ Tilt Up ${camera}`}
        title="Tilt Up"
        onClick={() => sendMove("up")}
      >
        ▲
      </button>
      <button
        type="button"
        className="nvr-btn nvr-tile-action mono"
        style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
        aria-label={`PTZ Tilt Down ${camera}`}
        title="Tilt Down"
        onClick={() => sendMove("down")}
      >
        ▼
      </button>
      <button
        type="button"
        className="nvr-btn nvr-tile-action mono"
        style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
        aria-label={`PTZ Pan Right ${camera}`}
        title="Pan Right"
        onClick={() => sendMove("right")}
      >
        ▶
      </button>
      <button
        type="button"
        className="nvr-btn nvr-tile-action mono"
        style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
        aria-label={`PTZ Zoom In ${camera}`}
        title="Zoom In"
        onClick={() => sendMove("zoom_in")}
      >
        Z+
      </button>
      <button
        type="button"
        className="nvr-btn nvr-tile-action mono"
        style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
        aria-label={`PTZ Zoom Out ${camera}`}
        title="Zoom Out"
        onClick={() => sendMove("zoom_out")}
      >
        Z-
      </button>
      <button
        type="button"
        className="nvr-btn danger nvr-tile-action mono"
        style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
        aria-label={`PTZ Stop ${camera}`}
        title="Stop PTZ Movement"
        onClick={() => sendStop()}
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
              sendPreset(e.target.value);
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
  const config = useNvrStore((s) => s.config);
  const setConfig = useNvrStore((s) => s.setConfig);
  const stats = useNvrStore((s) => s.stats);
  const cameraActivity = useNvrStore((s) => s.cameraActivity);

  const lastActiveRef = useRef<Record<string, number>>({});
  const lastSurgeRef = useRef<Record<string, number>>({});
  const prevMatchRef = useRef<Record<string, boolean>>({});
  const [spotlightTrail, setSpotlightTrail] = useState<Array<{ cam: string; ts: number }>>([]);
  const [nowTick, setNowTick] = useState<number>(() => Math.floor(Date.now() / 1000));
  const [mapEditMode, setMapEditMode] = useState(false);
  const [draggingCam, setDraggingCam] = useState<string | null>(null);
  const [mapFloorplanUrl, setMapFloorplanUrl] = useState<string>(() => {
    try {
      return window.localStorage?.getItem("ninjanvr_floorplan_url") || "";
    } catch {
      return "";
    }
  });
  const [mapPlacements, setMapPlacements] = useState<Record<string, CameraMapPlacement>>({});
  const [mapSaveStatus, setMapSaveStatus] = useState<string | null>(null);
  const [ptzInfoByCam, setPtzInfoByCam] = useState<Record<string, CameraPtzInfoResponse>>({});
  const mapSvgRef = useRef<SVGSVGElement | null>(null);

  useEffect(() => {
    const cams = config?.cameras || {};
    for (const [camName, camCfg] of Object.entries(cams)) {
      if (isCameraPtzEnabled(camCfg) && !ptzInfoByCam[camName]) {
        fetch(`/api/${encodeURIComponent(camName)}/ptz/info`, {
          credentials: "include",
        })
          .then((res) => (res.ok ? res.json() : null))
          .then((data: CameraPtzInfoResponse | null) => {
            if (data) {
              setPtzInfoByCam((prev) => ({ ...prev, [camName]: data }));
            }
          })
          .catch(() => {});
      }
    }
  }, [config?.cameras, ptzInfoByCam]);

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
      cameraNames.forEach((cam, idx) => {
        const cfgMap = config?.cameras?.[cam]?.map;
        const fallbackPlacement = computeDefaultMapPlacement(
          idx,
          cameraNames.length
        );
        const isUnconfiguredDefault =
          cfgMap &&
          Math.abs((cfgMap.x ?? 0.5) - 0.5) < 0.001 &&
          Math.abs((cfgMap.y ?? 0.5) - 0.5) < 0.001 &&
          Math.abs(cfgMap.angle_deg ?? 0) < 0.001;
        if (cfgMap && !isUnconfiguredDefault) {
          next[cam] = {
            x: cfgMap.x ?? fallbackPlacement.x,
            y: cfgMap.y ?? fallbackPlacement.y,
            angle_deg: cfgMap.angle_deg ?? fallbackPlacement.angle_deg,
            fov_deg: cfgMap.fov_deg ?? fallbackPlacement.fov_deg,
            range: cfgMap.range ?? fallbackPlacement.range,
          };
        } else if (!next[cam]) {
          next[cam] = fallbackPlacement;
        }
      });
      return next;
    });
  }, [cameraNames, config, mapEditMode]);

  const handleSaveMapPlacements = async () => {
    try {
      const cfgRes = await fetch("/api/config", { credentials: "include" });
      if (!cfgRes.ok) return;
      const currentCfg = await cfgRes.json();
      if (currentCfg && currentCfg.cameras) {
        for (const [cam, placement] of Object.entries(mapPlacements)) {
          if (currentCfg.cameras[cam]) {
            currentCfg.cameras[cam].map = placement;
          }
        }
        const saveRes = await fetch("/api/config/structured?save_option=saveonly", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ cameras: currentCfg.cameras }),
        });
        if (saveRes.ok) {
          const savedData = await saveRes.json().catch(() => ({}));
          if (savedData.config) {
            setConfig(savedData.config);
          }
          setMapSaveStatus("Saved to config.yml");
          window.setTimeout(() => setMapSaveStatus(null), 2500);
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
    if (liveLayout !== "spotlight") return;
    const tmr = window.setInterval(() => {
      setNowTick(Math.floor(Date.now() / 1000));
    }, 1000);
    return () => window.clearInterval(tmr);
  }, [liveLayout]);

  const groupCams = useMemo(() => {
    const resolved = resolveOrderedDashboardCameras(
      cameraNames,
      config?.cameras,
      activeCameraGroup,
      config?.camera_groups
    );
    return resolved.length > 0 || activeCameraGroup !== "all"
      ? resolved
      : cameraNames;
  }, [cameraNames, config?.cameras, activeCameraGroup, config?.camera_groups]);

  const { visibleMatrixCams, totalPages, safePage } = useMemo(
    () => sliceMatrixPage(groupCams, matrixPage, matrixDensity),
    [groupCams, matrixPage, matrixDensity]
  );

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
    if (!config?.camera_groups) return [];
    return Object.entries(config.camera_groups).sort(
      ([aKey, aVal], [bKey, bVal]) =>
        ((aVal.order ?? 0) - (bVal.order ?? 0)) || aKey.localeCompare(bKey)
    );
  }, [config?.camera_groups]);

  const spotlightPromotedCams = [...groupCams]
    .filter((cam) => nowTick - (lastActiveRef.current[cam] || 0) <= spotlightHoldSec)
    .sort(
      (a, b) =>
        (lastSurgeRef.current[b] || lastActiveRef.current[b] || 0) -
        (lastSurgeRef.current[a] || lastActiveRef.current[a] || 0)
    );
  const spotlightHeroCam =
    spotlightPromotedCams[0] ||
    (spotlightTrail.length > 0
      ? spotlightTrail[spotlightTrail.length - 1].cam
      : selectedCamera);
  const spotlightActiveCams = [spotlightHeroCam];
  const spotlightIdleCams = groupCams.filter((c) => c !== spotlightHeroCam);
  const focusDockCams = groupCams.filter((c) => c !== selectedCamera);

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
                ALL ({cameraNames.length})
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
                          setSelectedCamera(item.cam);
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
                cameraGroups={config?.camera_groups}
                cameraStats={stats?.cameras}
                cameraActivity={cameraActivity}
                ariaLabel="Focus camera selector"
              />
            </>
          )}
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
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
          {unreviewedAlerts.length > 0 && (
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
      {showAlertRibbon && unreviewedAlerts.length > 0 && (
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
            const thumbSrc = item.thumb_path
              ? item.thumb_path.replace(/^\/media\/[^/]+/, "")
              : `/api/${item.camera}/latest.webp`;
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

      {liveLayout === "2x2" && (
        <div className={`nvr-matrix-${matrixDensity}`}>
          {visibleMatrixCams.map((cam) => {
            const camCfg = config?.cameras?.[cam];
            const camStat = stats?.cameras?.[cam];
            const act = cameraActivity[cam];
            const activeLabels = Array.from(
              new Set((act?.objects || []).map((o) => o.label.toUpperCase()))
            );
            const hasObjects = activeLabels.length > 0;
            const hasMotion = Boolean(act?.motion && (act?.max_Area ?? 5000) >= 3500);
            const isTimelineCam = selectedCamera === cam;
            const isTimeShifted = isTimelineCam && timeShiftTs !== null;

            return (
              <div
                key={cam}
                className={`nvr-cam-tile ${isTimeShifted ? "has-native-controls" : ""}`}
                style={{
                  outline: isTimelineCam ? "1px solid var(--signal-accent)" : "none",
                  outlineOffset: -1,
                }}
              >
                {isTimeShifted && timeShiftTs !== null ? (
                  <VodHlsPlayer
                    camera={cam}
                    startTime={timeShiftTs}
                    endTime={timeShiftTs + 180}
                    playbackRate={playbackSpeed}
                  />
                ) : (
                  <LiveMsePlayer
                    camera={cam}
                    useMainStream={useMainStream}
                    cameraConfig={camCfg}
                    showVectors={showVectors}
                    onClick={() => {
                      if (selectedCamera === cam) {
                        setLiveLayout("focus");
                      } else {
                        setSelectedCamera(cam);
                      }
                    }}
                  />
                )}
                <div className={`nvr-cam-hud pos-${hudPosition}`}>
                  <span className={`nvr-badge ${isTimeShifted ? "rewind" : "live"}`}>
                    <span className={`nvr-dot ${isTimeShifted ? "motion" : "live"}`} />
                    {cam.toUpperCase()}
                    {isTimeShifted && timeShiftTs !== null
                      ? ` @ ${formatTs(timeShiftTs)}`
                      : ""}
                  </span>
                  <span className="nvr-badge">
                    {camCfg?.detect?.width || 1280}×{camCfg?.detect?.height || 720} ·{" "}
                    {camStat?.camera_fps?.toFixed(1) || "5.0"} FPS
                  </span>
                  {hasObjects && (
                    <span className="nvr-badge alert">{activeLabels.join(", ")}</span>
                  )}
                  {hasMotion && !hasObjects && (
                    <span className="nvr-badge motion">{t.live.motionActive}</span>
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
                  <span className="nvr-badge">
                    AI {camStat?.detection_fps?.toFixed(1) || "0.0"} FPS
                  </span>
                  {(
                    [
                      ["enabled", "EN", act?.config?.enabled ?? act?.enabled ?? true],
                      ["detect", "DET", act?.config?.detect ?? true],
                      ["record", "REC", act?.config?.record ?? true],
                      ["alerts", "ALR", act?.config?.alerts ?? true],
                    ] as Array<["enabled" | "detect" | "record" | "alerts", string, boolean]>
                  ).map(([flagKey, shortLbl, isOn]) => (
                    <button
                      key={flagKey}
                      type="button"
                      className={`nvr-btn nvr-tile-action mono ${isOn ? "active" : ""}`}
                      style={{ height: 22, padding: "0 5px", fontSize: "var(--text-xs)" }}
                      title={`Toggle ${flagKey} at runtime without server restart`}
                      onClick={(e) => {
                        e.stopPropagation();
                        fetch(`/api/${encodeURIComponent(cam)}/state`, {
                          method: "POST",
                          headers: { "Content-Type": "application/json" },
                          credentials: "include",
                          body: JSON.stringify({ [flagKey]: !isOn }),
                        }).catch(() => {});
                      }}
                    >
                      {shortLbl}:{isOn ? "ON" : "OFF"}
                    </button>
                  ))}
                  <button
                    className="nvr-btn nvr-tile-action mono"
                    style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
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
                      style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
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
                    style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                    onClick={(e) => {
                      e.stopPropagation();
                      setSelectedCamera(cam);
                      setLiveLayout("focus");
                    }}
                  >
                    Expand
                  </button>
                  <a
                    href={`/api/${cam}/latest.jpg`}
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
                  <TacticalPtzHudOverlay
                    camera={cam}
                    cameraConfig={camCfg}
                    ptzInfo={ptzInfoByCam[cam]}
                    onPtzUpdate={handlePtzUpdate}
                  />
                </div>
              </div>
            );
          })}
        </div>
      )}

      {liveLayout === "spotlight" && (
        <div className="nvr-focus-layout">
          <div
            className={
              spotlightActiveCams.length > 1 ? "nvr-matrix-2x2" : "nvr-focus-stage"
            }
            style={{ flex: 1, minHeight: 0 }}
          >
            {spotlightActiveCams.map((cam) => {
              const camCfg = config?.cameras?.[cam];
              const camStat = stats?.cameras?.[cam];
              const act = cameraActivity[cam];
              const activeLabels = (act?.objects || [])
                .filter((o) => !o.stationary)
                .map((o) => o.label.toUpperCase());
              const isHeld = spotlightPromotedCams.includes(cam);
              return (
                <div
                  key={cam}
                  className="nvr-cam-tile"
                  style={{
                    flex: 1,
                    outline: isHeld
                      ? "1px solid var(--signal-alert)"
                      : "1px solid var(--border-strong)",
                    outlineOffset: -1,
                  }}
                >
                  <LiveMsePlayer
                    camera={cam}
                    useMainStream={spotlightActiveCams.length === 1 || useMainStream}
                    cameraConfig={camCfg}
                    showVectors={showVectors}
                    onClick={() => setSelectedCamera(cam)}
                  />
                  <div className={`nvr-cam-hud pos-${hudPosition}`}>
                    <span className={`nvr-badge ${isHeld ? "alert" : "live"}`}>
                      <span className={`nvr-dot ${isHeld ? "motion" : "live"}`} />
                      SPOTLIGHT // {cam.toUpperCase()}
                    </span>
                    <span className="nvr-badge">
                      {camStat?.camera_fps?.toFixed(1) || "5.0"} FPS
                    </span>
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
                        title="Infrared (IR) monochrome night profile active"
                      >
                        IR NIGHT
                      </span>
                    )}
                    {activeLabels.length > 0 && (
                      <span className="nvr-badge alert">
                        {activeLabels.join(", ")}
                      </span>
                    )}
                    <TacticalPtzHudOverlay
                      camera={cam}
                      cameraConfig={camCfg}
                      ptzInfo={ptzInfoByCam[cam]}
                      onPtzUpdate={handlePtzUpdate}
                    />
                  </div>
                </div>
              );
            })}
          </div>

          {spotlightIdleCams.length > 0 && (
            <div className="nvr-focus-dock">
              {spotlightIdleCams.map((cam, idx) => {
                const tileMode = resolveDockTileMode(
                  idx,
                  cam,
                  hoveredDockCam,
                  MAX_DOCK_LIVE_DECODERS,
                  spotlightIdleCams
                );
                const promoteCam = () => {
                  const ts = Math.floor(Date.now() / 1000);
                  lastActiveRef.current[cam] = ts;
                  lastSurgeRef.current[cam] = ts;
                  setSelectedCamera(cam);
                  setNowTick(ts);
                };
                return (
                  <div
                    key={cam}
                    className="nvr-cam-tile"
                    style={{ height: "33.33%", minHeight: 140, opacity: 0.78, cursor: "pointer" }}
                    onPointerEnter={() => setHoveredDockCam(cam)}
                    onPointerLeave={() =>
                      setHoveredDockCam((c) => (c === cam ? null : c))
                    }
                    onClick={promoteCam}
                  >
                    {tileMode === "mse" ? (
                      <LiveMsePlayer
                        camera={cam}
                        useMainStream={false}
                        cameraConfig={config?.cameras?.[cam]}
                        showVectors={showVectors}
                        onClick={promoteCam}
                      />
                    ) : (
                      <img
                        className="nvr-cam-poster"
                        src={`/api/${encodeURIComponent(cam)}/latest.webp`}
                        alt={`${cam} standby snapshot`}
                        loading="lazy"
                      />
                    )}
                    <div className={`nvr-cam-hud pos-${hudPosition}`}>
                      <span className="nvr-badge mono">
                        {cam.toUpperCase()} · {tileMode === "mse" ? "STANDBY" : "SNAPSHOT"}
                      </span>
                      {Boolean(
                        cameraActivity[cam]?.night_mode || stats?.cameras?.[cam]?.night_mode
                      ) && <span className="nvr-badge mono">IR NIGHT</span>}
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      )}

      {(liveLayout === "focus" || liveLayout === "single") && (
        <div
          className={
            liveLayout === "focus" ? "nvr-focus-layout" : "nvr-matrix-single"
          }
        >
          <div className="nvr-focus-stage">
            <div
              className={`nvr-cam-tile ${timeShiftTs !== null ? "has-native-controls" : ""}`}
              style={{ flex: 1 }}
            >
              {timeShiftTs === null ? (
                <LiveMsePlayer
                  camera={selectedCamera}
                  useMainStream={true}
                  cameraConfig={config?.cameras?.[selectedCamera]}
                  showVectors={showVectors}
                />
              ) : (
                <VodHlsPlayer
                  camera={selectedCamera}
                  startTime={timeShiftTs}
                  endTime={timeShiftTs + 180}
                  playbackRate={playbackSpeed}
                />
              )}
              <div className={`nvr-cam-hud pos-${hudPosition}`}>
                <span className={`nvr-badge ${timeShiftTs === null ? "live" : "rewind"}`}>
                  <span
                    className={`nvr-dot ${timeShiftTs === null ? "live" : "motion"}`}
                  />
                  {selectedCamera.toUpperCase()} //{" "}
                  {timeShiftTs === null
                    ? t.live.liveBadge
                    : `${t.live.timeShiftBadge} @ ${formatTs(timeShiftTs)}`}
                </span>
                <span className="nvr-badge">
                  {stats?.cameras?.[selectedCamera]?.camera_fps?.toFixed(1) || "5.0"} FPS ·
                  AI{" "}
                  {stats?.cameras?.[selectedCamera]?.detection_fps?.toFixed(1) || "0.0"} FPS
                </span>
                {(cameraActivity[selectedCamera]?.objects || []).length > 0 ? (
                  <span className="nvr-badge alert">
                    {Array.from(
                      new Set(
                        (cameraActivity[selectedCamera]?.objects || []).map((o) =>
                          o.label.toUpperCase()
                        )
                      )
                    ).join(", ")}
                  </span>
                ) : Boolean(
                    cameraActivity[selectedCamera]?.motion &&
                      (cameraActivity[selectedCamera]?.max_Area ?? 5000) >= 3500
                  ) ? (
                  <span className="nvr-badge motion">{t.live.motionActive}</span>
                ) : null}
                {Boolean(cameraActivity[selectedCamera]?.tamper) && (
                  <span className="nvr-badge alert mono">TAMPER</span>
                )}
                {Boolean(
                  cameraActivity[selectedCamera]?.night_mode ||
                    stats?.cameras?.[selectedCamera]?.night_mode
                ) && <span className="nvr-badge mono">IR NIGHT</span>}
                {timeShiftTs !== null && (
                  <button
                    className="nvr-btn success"
                    onClick={() => setTimeShiftTs(null)}
                  >
                    <Play size={12} />
                    {t.live.returnToLive}
                  </button>
                )}
                <TacticalPtzHudOverlay
                  camera={selectedCamera}
                  cameraConfig={config?.cameras?.[selectedCamera]}
                  ptzInfo={ptzInfoByCam[selectedCamera]}
                  onPtzUpdate={handlePtzUpdate}
                />
              </div>
            </div>
          </div>

          {liveLayout === "focus" && (
            <div className="nvr-focus-dock">
              {focusDockCams.map((cam, idx) => {
                const tileMode = resolveDockTileMode(
                  idx,
                  cam,
                  hoveredDockCam,
                  MAX_DOCK_LIVE_DECODERS,
                  focusDockCams
                );
                return (
                  <div
                    key={cam}
                    className="nvr-cam-tile"
                    style={{ height: "33.33%", minHeight: 160, cursor: "pointer" }}
                    onPointerEnter={() => setHoveredDockCam(cam)}
                    onPointerLeave={() =>
                      setHoveredDockCam((c) => (c === cam ? null : c))
                    }
                    onClick={() => setSelectedCamera(cam)}
                  >
                    {tileMode === "mse" ? (
                      <LiveMsePlayer
                        camera={cam}
                        useMainStream={false}
                        cameraConfig={config?.cameras?.[cam]}
                        showVectors={showVectors}
                        onClick={() => setSelectedCamera(cam)}
                      />
                    ) : (
                      <img
                        className="nvr-cam-poster"
                        src={`/api/${encodeURIComponent(cam)}/latest.webp`}
                        alt={`${cam} dock snapshot`}
                        loading="lazy"
                      />
                    )}
                    <div className={`nvr-cam-hud pos-${hudPosition}`}>
                      <span className="nvr-badge live">{cam.toUpperCase()}</span>
                      <span className="nvr-badge">
                        {tileMode === "mse"
                          ? `${stats?.cameras?.[cam]?.camera_fps?.toFixed(1) || "5.0"} FPS`
                          : "WEBP"}
                      </span>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      )}

      {liveLayout === "map" && (
        <div
          className="nvr-focus-layout"
          style={{ display: "grid", gridTemplateColumns: "1fr 340px", gap: 8, flex: 1, minHeight: 0 }}
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
              }}
            >
              <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700 }}>
                TACTICAL FLOORPLAN // SECTOR FOV CONES & LIVE ACTIVITY PULSE
              </span>
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <input
                  className="nvr-input mono"
                  style={{ height: 24, width: 190, fontSize: "var(--text-xs)" }}
                  placeholder="Floorplan SVG/PNG URL (optional)"
                  value={mapFloorplanUrl}
                  onChange={(e) => {
                    setMapFloorplanUrl(e.target.value);
                    window.localStorage?.setItem("ninjanvr_floorplan_url", e.target.value);
                  }}
                />
                <button
                  type="button"
                  className={`nvr-btn mono ${mapEditMode ? "active" : ""}`}
                  style={{ height: 24, fontSize: "var(--text-xs)" }}
                  onClick={() => setMapEditMode((v) => !v)}
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

            <svg
              ref={mapSvgRef}
              viewBox="0 0 1000 600"
              style={{
                width: "100%",
                height: "100%",
                flex: 1,
                userSelect: "none",
                cursor: draggingCam ? "grabbing" : "default",
                background:
                  "radial-gradient(circle at 50% 50%, rgba(14, 165, 233, 0.06), rgba(7, 10, 15, 0.98))",
              }}
              onPointerMove={(e) => {
                if (!mapEditMode || !draggingCam || !mapSvgRef.current) return;
                const rect = mapSvgRef.current.getBoundingClientRect();
                if (rect.width <= 0 || rect.height <= 0) return;
                const nx = Math.max(0.05, Math.min(0.95, (e.clientX - rect.left) / rect.width));
                const ny = Math.max(0.05, Math.min(0.95, (e.clientY - rect.top) / rect.height));
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
              }}
              onPointerUp={() => setDraggingCam(null)}
              onPointerLeave={() => setDraggingCam(null)}
            >
              <defs>
                <pattern id="tacticalGrid" width="50" height="50" patternUnits="userSpaceOnUse">
                  <path
                    d="M 50 0 L 0 0 0 50"
                    fill="none"
                    stroke="rgba(148, 163, 184, 0.12)"
                    strokeWidth="1"
                  />
                </pattern>
              </defs>
              <rect width="1000" height="600" fill="url(#tacticalGrid)" />
              {mapFloorplanUrl && (
                <image
                  href={mapFloorplanUrl}
                  x="40"
                  y="30"
                  width="920"
                  height="540"
                  preserveAspectRatio="xMidYMid meet"
                  opacity="0.45"
                />
              )}
              <rect
                x="60"
                y="45"
                width="880"
                height="510"
                fill="none"
                stroke="rgba(56, 189, 248, 0.25)"
                strokeWidth="1.5"
                strokeDasharray="6 4"
              />

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
                const act = cameraActivity[cam];
                const objs = act?.objects || [];
                const hasObjects = objs.length > 0;
                const hasMotion = Boolean(act?.motion && (act?.max_Area ?? 5000) >= 3500);
                const isSelected = selectedCamera === cam;

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
                    style={{ cursor: mapEditMode ? "grab" : "pointer" }}
                    onPointerDown={(e) => {
                      e.stopPropagation();
                      setSelectedCamera(cam);
                      setMapEditMode(true);
                      setDraggingCam(cam);
                    }}
                  >
                    <path
                      d={buildFovConeSvgPath(cx, cy, place.angle_deg, place.fov_deg, radius)}
                      fill={coneFill}
                      stroke={coneStroke}
                      strokeWidth={isSelected ? 2 : 1.2}
                    />
                    {(hasObjects || hasMotion) && (
                      <circle
                        cx={cx}
                        cy={cy}
                        r={20}
                        fill="none"
                        stroke={coneStroke}
                        strokeWidth="2"
                        opacity="0.75"
                      />
                    )}
                    <circle
                      cx={cx}
                      cy={cy}
                      r={isSelected ? 9 : 7}
                      fill={hasObjects ? "#ef4444" : hasMotion ? "#f59e0b" : "#0ea5e9"}
                      stroke="#070a0f"
                      strokeWidth="2"
                    />
                    <text
                      x={cx + 12}
                      y={cy + 4}
                      fill="#f8fafc"
                      fontSize="12"
                      fontFamily="JetBrains Mono, monospace"
                      fontWeight="700"
                    >
                      {cam.toUpperCase()}
                    </text>
                    {objs.slice(0, 2).map((obj, oIdx) => (
                      <text
                        key={`${obj.id}-${oIdx}`}
                        x={cx + 12}
                        y={cy + 19 + oIdx * 14}
                        fill="#fca5a5"
                        fontSize="11"
                        fontFamily="JetBrains Mono, monospace"
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

            <div className="nvr-cam-tile" style={{ height: 210, flexShrink: 0 }}>
              <LiveMsePlayer
                camera={selectedCamera}
                useMainStream={false}
                cameraConfig={config?.cameras?.[selectedCamera]}
                showVectors={showVectors}
              />
            </div>

            {mapPlacements[selectedCamera] && (
              <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  HEADING ANGLE: {Math.round(mapPlacements[selectedCamera].angle_deg)}°
                </div>
                <input
                  type="range"
                  min={0}
                  max={360}
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
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  FOV CONE ANGLE: {Math.round(mapPlacements[selectedCamera].fov_deg)}°
                </div>
                <input
                  type="range"
                  min={30}
                  max={180}
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
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  SECTOR RANGE: {Math.round(mapPlacements[selectedCamera].range * 100)}%
                </div>
                <input
                  type="range"
                  min={5}
                  max={50}
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
              </div>
            )}
          </div>
        </div>
      )}

      <TimelineScrubber
        camera={selectedCamera}
        cameras={cameraNames}
        onSelectCamera={(cam) => setSelectedCamera(cam)}
        segments={focusSegments}
        reviews={reviews}
        currentTs={timeShiftTs}
        onSeek={(ts) => setTimeShiftTs(ts)}
        onExportRange={handleQuickExport}
        titlePrefix="TIMELINE"
      />
    </>
  );
};
