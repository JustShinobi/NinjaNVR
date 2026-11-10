import React, { useEffect, useMemo, useRef, useState } from "react";
import { resolveRealCameraSource } from "../store/useNvrStore";
import {
  CameraConfig,
  CameraGroupConfig,
  CameraHardwareInfoResponse,
  CameraLiveActivity,
  CameraMapPlacement,
  FloorplanRoom,
  FloorplanWall,
  NinjaConfigResponse,
  NinjaStatsResponse,
} from "../types/nvr";
import { resolveCameraPushToTalkCapability } from "../utils/cameraCapabilities";
import {
  DEFAULT_FLOORPLAN_HEIGHT_M,
  DEFAULT_FLOORPLAN_WIDTH_M,
  DEFAULT_ROOM_LABELS,
  FloorplanRoomDragState,
  FloorplanRoomDraft,
  FloorplanRoomResizeHandle,
  FloorplanRoomResizeState,
  FloorplanStudioTool,
  FloorplanWallDragState,
  ROOM_COLOR_PALETTE,
  applyFloorplanCameraDrag,
  applyFloorplanFovTipDrag,
  applyFloorplanRoomDrag,
  applyFloorplanRoomDrawCommit,
  applyFloorplanRoomResize,
  applyFloorplanWallEndpointDrag,
  buildFloorplanSavePayload,
  buildFovConeSvgPath,
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
import { CanvasThemePalette } from "../utils/themeTokens";
import { TacticalCameraPicker } from "./TacticalCameraPicker";
import { LiveMsePlayer } from "./VideoPlayers";

export interface FloorplanStudioCanvasProps {
  cameraNames: string[];
  effectiveCameraNames: string[];
  effectiveCamerasConfig?: Record<string, CameraConfig>;
  effectiveCameraGroups?: Record<string, CameraGroupConfig>;
  groupCams: string[];
  selectedCamera: string;
  onSelectCamera: (cam: string) => void;
  onOpenFocus: () => void;
  config?: NinjaConfigResponse | null;
  onConfigSaved?: (nextConfig: NinjaConfigResponse) => void;
  stats?: NinjaStatsResponse | null;
  cameraActivity: Record<string, CameraLiveActivity>;
  hardwareInfoByCam: Record<string, CameraHardwareInfoResponse>;
  showVectors: boolean;
  mapTheme: CanvasThemePalette;
}

function resolveInitialMapPlacements(
  effectiveCameraNames: string[],
  effectiveCamerasConfig?: Record<string, CameraConfig>,
  prev: Record<string, CameraMapPlacement> = {}
): Record<string, CameraMapPlacement> {
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
}

export const FloorplanStudioCanvas: React.FC<FloorplanStudioCanvasProps> = ({
  cameraNames,
  effectiveCameraNames,
  effectiveCamerasConfig,
  effectiveCameraGroups,
  groupCams,
  selectedCamera,
  onSelectCamera,
  onOpenFocus,
  config,
  onConfigSaved,
  stats,
  cameraActivity,
  hardwareInfoByCam,
  showVectors,
  mapTheme,
}) => {
  const [mapEditMode, setMapEditMode] = useState(false);
  const [mapTool, setMapTool] = useState<FloorplanStudioTool>("select");
  const [draggingCam, setDraggingCam] = useState<string | null>(null);
  const [draggingAimCam, setDraggingAimCam] = useState<string | null>(null);
  const [mapFloorplanUrl, setMapFloorplanUrl] = useState<string>(() => {
    const cfgUrl = config?.ui?.floorplan?.background_data_url;
    if (typeof cfgUrl === "string" && cfgUrl.trim()) {
      return sanitizeFloorplanUrl(cfgUrl);
    }
    try {
      return sanitizeFloorplanUrl(
        typeof window !== "undefined"
          ? window.localStorage?.getItem("ninjanvr_floorplan_url") || ""
          : ""
      );
    } catch {
      return "";
    }
  });
  const [floorplanOpacity, setFloorplanOpacity] = useState<number>(() => {
    const op = config?.ui?.floorplan?.opacity;
    return typeof op === "number" && Number.isFinite(op)
      ? Math.max(0.1, Math.min(1, op))
      : 0.45;
  });
  const [floorplanWidthM, setFloorplanWidthM] = useState<number>(() => {
    const w = config?.ui?.floorplan?.width_m;
    return typeof w === "number" && Number.isFinite(w) && w >= 2
      ? Math.min(500, w)
      : DEFAULT_FLOORPLAN_WIDTH_M;
  });
  const [floorplanHeightM, setFloorplanHeightM] = useState<number>(() => {
    const h = config?.ui?.floorplan?.height_m;
    return typeof h === "number" && Number.isFinite(h) && h >= 2
      ? Math.min(500, h)
      : DEFAULT_FLOORPLAN_HEIGHT_M;
  });
  const [floorplanRooms, setFloorplanRooms] = useState<FloorplanRoom[]>(() =>
    Array.isArray(config?.ui?.floorplan?.rooms)
      ? config!.ui!.floorplan!.rooms!.map((r, idx) =>
          sanitizeFloorplanRoom(r, idx)
        )
      : []
  );
  const [floorplanWalls, setFloorplanWalls] = useState<FloorplanWall[]>(() =>
    Array.isArray(config?.ui?.floorplan?.walls)
      ? config!.ui!.floorplan!.walls!.map((w) => sanitizeFloorplanWall(w))
      : []
  );
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
  const [roomDraft, setRoomDraft] = useState<FloorplanRoomDraft | null>(null);
  const [draggingRoom, setDraggingRoom] = useState<FloorplanRoomDragState | null>(null);
  const [resizingRoom, setResizingRoom] = useState<FloorplanRoomResizeState | null>(null);
  const [wallStartPoint, setWallStartPoint] = useState<{
    x: number;
    y: number;
  } | null>(null);
  const [wallPreviewPoint, setWallPreviewPoint] = useState<{
    x: number;
    y: number;
    snappedToVertex?: boolean;
  } | null>(null);
  const [draggingWall, setDraggingWall] = useState<FloorplanWallDragState | null>(null);
  const [mapPlacements, setMapPlacements] = useState<Record<string, CameraMapPlacement>>(() =>
    resolveInitialMapPlacements(effectiveCameraNames, effectiveCamerasConfig)
  );
  const [mapSaveStatus, setMapSaveStatus] = useState<string | null>(null);

  const isMountedRef = useRef<boolean>(true);
  const mapSaveTimerRef = useRef<number | null>(null);
  const mapSaveSeqRef = useRef<number>(0);
  const latestFloorplanDraftRef = useRef<{
    mapPlacements: Record<string, CameraMapPlacement>;
    mapFloorplanUrl: string;
    floorplanOpacity: number;
    floorplanWidthM: number;
    floorplanHeightM: number;
    floorplanRooms: FloorplanRoom[];
    floorplanWalls: FloorplanWall[];
  }>({
    mapPlacements,
    mapFloorplanUrl,
    floorplanOpacity,
    floorplanWidthM,
    floorplanHeightM,
    floorplanRooms,
    floorplanWalls,
  });
  latestFloorplanDraftRef.current = {
    mapPlacements,
    mapFloorplanUrl,
    floorplanOpacity,
    floorplanWidthM,
    floorplanHeightM,
    floorplanRooms,
    floorplanWalls,
  };
  const activeFileReaderRef = useRef<FileReader | null>(null);
  const mapSvgRef = useRef<SVGSVGElement | null>(null);
  const floorplanFileInputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
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

  const snapVertices = useMemo(
    () => computeFloorplanSnapVertices(floorplanWalls, floorplanRooms),
    [floorplanWalls, floorplanRooms]
  );

  useEffect(() => {
    if (!mapEditMode) return;
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
        setDraggingCam(null);
        setDraggingAimCam(null);
        setDraggingRoom(null);
        setResizingRoom(null);
        setDraggingWall(null);
      } else if (e.key === "Delete" || e.key === "Backspace") {
        const next = deleteSelectedFloorplanEntity({
          rooms: floorplanRooms,
          walls: floorplanWalls,
          selectedRoomId,
          selectedWallIdx,
        });
        if (next.deleted === "room") {
          e.preventDefault();
          setFloorplanRooms(next.rooms);
          setSelectedRoomId(null);
          setDraggingRoom(null);
          setResizingRoom(null);
        } else if (next.deleted === "wall") {
          e.preventDefault();
          setFloorplanWalls(next.walls);
          setSelectedWallIdx(null);
          setDraggingWall(null);
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [mapEditMode, floorplanRooms, floorplanWalls, selectedRoomId, selectedWallIdx]);

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
        setFloorplanRooms(
          fp.rooms.map((r, idx) => sanitizeFloorplanRoom(r, idx))
        );
      }
      if (Array.isArray(fp.walls)) {
        setFloorplanWalls(fp.walls.map((w) => sanitizeFloorplanWall(w)));
      }
      if (typeof fp.background_data_url === "string") {
        setMapFloorplanUrl(sanitizeFloorplanUrl(fp.background_data_url));
      }
    }
  }, [config?.ui?.floorplan, mapEditMode]);

  useEffect(() => {
    if (mapEditMode) return;
    setMapPlacements((prev) =>
      resolveInitialMapPlacements(
        effectiveCameraNames,
        effectiveCamerasConfig,
        prev
      )
    );
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
    const mySaveSeq = ++mapSaveSeqRef.current;
    try {
      const cfgRes = await fetch("/api/config", { credentials: "include" });
      if (
        !cfgRes.ok ||
        !isMountedRef.current ||
        mySaveSeq !== mapSaveSeqRef.current
      ) {
        return;
      }
      const currentCfg = await cfgRes.json();
      if (!isMountedRef.current || mySaveSeq !== mapSaveSeqRef.current) return;
      if (currentCfg && currentCfg.cameras) {
        const latestDraft = latestFloorplanDraftRef.current;
        const payload = buildFloorplanSavePayload({
          existingCameras: currentCfg.cameras,
          mapPlacements: latestDraft.mapPlacements,
          existingUi: currentCfg.ui,
          floorplanUrl: latestDraft.mapFloorplanUrl,
          floorplanOpacity: latestDraft.floorplanOpacity,
          floorplanWidthM: latestDraft.floorplanWidthM,
          floorplanHeightM: latestDraft.floorplanHeightM,
          floorplanRooms: latestDraft.floorplanRooms,
          floorplanWalls: latestDraft.floorplanWalls,
        });
        const saveRes = await fetch("/api/config/structured?save_option=saveonly", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify(payload),
        });
        if (
          saveRes.ok &&
          isMountedRef.current &&
          mySaveSeq === mapSaveSeqRef.current
        ) {
          const savedData = await saveRes.json().catch(() => ({}));
          if (!isMountedRef.current || mySaveSeq !== mapSaveSeqRef.current) {
            return;
          }
          if (savedData.success === false) {
            return;
          }
          if (savedData.config && onConfigSaved) {
            onConfigSaved(savedData.config);
          }
          setMapSaveStatus("Saved to config.yml");
          if (mapSaveTimerRef.current !== null) {
            window.clearTimeout(mapSaveTimerRef.current);
          }
          mapSaveTimerRef.current = window.setTimeout(() => {
            mapSaveTimerRef.current = null;
            if (isMountedRef.current) {
              setMapSaveStatus(null);
            }
          }, 2500);
        }
      }
    } catch {
      // ignore
    }
  };

  return (
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
                className="nvr-input mono sm"
                aria-label="Floorplan total width in meters"
                min={2}
                max={500}
                step={1}
                style={{ width: 52 }}
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
                className="nvr-input mono sm"
                aria-label="Floorplan total height in meters"
                min={2}
                max={500}
                step={1}
                style={{ width: 52 }}
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
              className={`nvr-btn mono ${showMeasurements ? "active" : ""} sm`}
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
              className="nvr-btn mono sm"
              onClick={() => floorplanFileInputRef.current?.click()}
              title="Upload PNG, SVG, WebP, or JPEG floorplan background"
            >
              Upload Floorplan
            </button>
            {mapFloorplanUrl && (
              <button
                type="button"
                className="nvr-btn mono sm"
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
              className={`nvr-btn mono ${mapEditMode ? "active" : ""} sm`}
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
                className="nvr-btn primary mono sm"
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
                  className={`nvr-btn mono ${mapTool === "select" ? "active" : ""} sm`}
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
                  className={`nvr-btn mono ${mapTool === "draw_room" ? "active" : ""} sm`}
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
                  } sm`}
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
                  } sm`}
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
                  } sm`}
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
                    className="nvr-select mono sm"
                    aria-label="Floorplan grid snap step"
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
                  className={`nvr-btn mono ${orthoSnap ? "active" : ""} sm`}
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
                      className="nvr-input mono sm"
                      aria-label="Selected room label"
                      style={{ width: 115 }}
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
                        className="nvr-input mono sm"
                        aria-label="Selected room width in meters"
                        step={0.1}
                        min={0.3}
                        max={floorplanWidthM}
                        style={{ width: 64 }}
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
                        className="nvr-input mono sm"
                        aria-label="Selected room height in meters"
                        step={0.1}
                        min={0.3}
                        max={floorplanHeightM}
                        style={{ width: 64 }}
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
                      className="nvr-btn mono sm"
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
                      className="nvr-btn mono sm"
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
                      className="nvr-btn danger mono sm"
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
                      className="nvr-input mono sm"
                      placeholder={`Wall #${wallIdx + 1}`}
                      aria-label="Selected wall label"
                      style={{ width: 100 }}
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
                      className="nvr-select mono sm"
                      aria-label="Selected wall kind"
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
                        className="nvr-input mono sm"
                        aria-label="Selected wall length in meters"
                        step={0.1}
                        min={0.1}
                        max={Math.hypot(floorplanWidthM, floorplanHeightM)}
                        style={{ width: 68 }}
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
                        className="nvr-btn mono sm"
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
                        className="nvr-input mono sm"
                        aria-label="Selected wall angle in degrees"
                        step={15}
                        min={0}
                        max={360}
                        style={{ width: 58 }}
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
                      className="nvr-btn mono sm"
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
                      className="nvr-btn mono sm"
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
                      className="nvr-btn danger mono sm"
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
                      className="nvr-input mono sm"
                      aria-label="Quick insert wall length in meters"
                      step={0.1}
                      min={0.2}
                      max={floorplanWidthM}
                      style={{ width: 64 }}
                      value={quickWallLenM}
                      onChange={(e) =>
                        setQuickWallLenM(Math.max(0.2, Number(e.target.value) || 1))
                      }
                    />
                    <span>m</span>
                    <button
                      type="button"
                      className="nvr-btn mono sm"
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
                      className="nvr-btn mono sm"
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
                        className="nvr-btn mono sm"
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
              setMapPlacements((prev) =>
                applyFloorplanFovTipDrag(prev, draggingAimCam, rawX, rawY)
              );
              return;
            }

            if (draggingCam) {
              setMapPlacements((prev) =>
                applyFloorplanCameraDrag(prev, draggingCam, rawX, rawY)
              );
              return;
            }

            if (draggingWall) {
              setFloorplanWalls((prev) =>
                applyFloorplanWallEndpointDrag(prev, draggingWall, rawX, rawY, {
                  snapStep,
                  orthoSnap,
                  vertices: snapVertices,
                  floorplanWidthM,
                  floorplanHeightM,
                })
              );
              return;
            }

            if (resizingRoom) {
              setFloorplanRooms((prev) =>
                applyFloorplanRoomResize(prev, resizingRoom, rawX, rawY, snapStep)
              );
              return;
            }

            if (draggingRoom) {
              setFloorplanRooms((prev) =>
                applyFloorplanRoomDrag(prev, draggingRoom, rawX, rawY, snapStep)
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
              const committed = applyFloorplanRoomDrawCommit(
                floorplanRooms,
                roomDraft,
                {
                  floorplanWidthM,
                  floorplanHeightM,
                }
              );
              setFloorplanRooms(committed.rooms);
              setSelectedRoomId(committed.selectedRoomId);
              setSelectedWallIdx(null);
              setRoomDraft(null);
            } else if (
              mapTool === "draw_wall" &&
              wallStartPoint &&
              wallPreviewPoint
            ) {
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
          onPointerCancel={() => {
            setCursorNorm(null);
            setRoomDraft(null);
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
                        { key: "nw", cx: rx, cy: ry, cursor: "nwse-resize" },
                        { key: "ne", cx: rx + rw, cy: ry, cursor: "nesw-resize" },
                        { key: "sw", cx: rx, cy: ry + rh, cursor: "nesw-resize" },
                        { key: "se", cx: rx + rw, cy: ry + rh, cursor: "nwse-resize" },
                        { key: "n", cx: rx + rw / 2, cy: ry, cursor: "ns-resize" },
                        { key: "s", cx: rx + rw / 2, cy: ry + rh, cursor: "ns-resize" },
                        { key: "w", cx: rx, cy: ry + rh / 2, cursor: "ew-resize" },
                        { key: "e", cx: rx + rw, cy: ry + rh / 2, cursor: "ew-resize" },
                      ] as Array<{
                        key: FloorplanRoomResizeHandle;
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

                <circle
                  cx={sx1}
                  cy={sy1}
                  r={isWallSelected && mapEditMode ? 6.5 : 3.5}
                  fill={
                    isWallSelected
                      ? "#38bdf8"
                      : mapTheme.floorplanWallNodeFill
                  }
                  stroke={isWallSelected ? "#f8fafc" : "none"}
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
                  stroke={isWallSelected ? "#f8fafc" : "none"}
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
                <path
                  d={buildFovConeSvgPath(cx, cy, place.angle_deg, place.fov_deg, radius)}
                  fill={coneFill}
                  stroke={coneStroke}
                  strokeWidth={isSelected ? 2 : 1.2}
                  style={{ pointerEvents: "none" }}
                />
                {mapEditMode && (
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
                        onSelectCamera(cam);
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
                    onSelectCamera(cam);
                    if (mapEditMode) {
                      setMapTool("select");
                      setDraggingCam(cam);
                    }
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
                    onSelectCamera(cam);
                    if (mapEditMode) {
                      setMapTool("select");
                      setDraggingCam(cam);
                    }
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
            className="nvr-btn mono sm"
            onClick={onOpenFocus}
          >
            Open Focus
          </button>
        </div>
        <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
          <TacticalCameraPicker
            cameras={groupCams}
            selectedCamera={selectedCamera}
            onSelect={onSelectCamera}
            cameraGroups={effectiveCameraGroups ?? config?.camera_groups}
            cameraStats={stats?.cameras}
            cameraActivity={cameraActivity}
            ariaLabel="Map sector camera selector"
          />
        </div>

        <div className="nvr-cam-tile" style={{ height: 190, flexShrink: 0 }}>
          {(() => {
            const mapRealCam = resolveRealCameraSource(
              selectedCamera,
              config?.cameras || cameraNames
            );
            const mapCamCfg =
              effectiveCamerasConfig?.[selectedCamera] ??
              config?.cameras?.[selectedCamera] ??
              config?.cameras?.[mapRealCam];
            const mapPttCap = resolveCameraPushToTalkCapability(
              mapCamCfg,
              hardwareInfoByCam[mapRealCam],
              config?.go2rtc?.streams,
              mapRealCam
            );
            return (
              <LiveMsePlayer
                camera={selectedCamera}
                useMainStream={false}
                cameraConfig={mapCamCfg}
                showVectors={showVectors}
                pushToTalkSupported={mapPttCap.supported}
                pushToTalkMode={mapPttCap.mode}
              />
            );
          })()}
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
              className="nvr-btn mono sm"
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
                    className="nvr-input mono sm"
                    aria-label={`Room label ${room.id}`}
                    style={{ flex: 1 }}
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
                    className="nvr-btn mono sm"
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
                    className="nvr-input mono sm"
                    aria-label={`Room width meters ${room.id}`}
                    step={0.1}
                    min={0.2}
                    max={floorplanWidthM}
                    style={{ width: 58 }}
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
                    className="nvr-input mono sm"
                    aria-label={`Room height meters ${room.id}`}
                    step={0.1}
                    min={0.2}
                    max={floorplanHeightM}
                    style={{ width: 58 }}
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
                className="nvr-btn mono sm"
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
                  className="nvr-btn mono sm"
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
                  className="nvr-input mono sm"
                  aria-label={`Wall ${wIdx + 1} length in meters`}
                  step={0.1}
                  min={0.1}
                  max={Math.hypot(floorplanWidthM, floorplanHeightM)}
                  style={{ width: 62 }}
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
                  className="nvr-btn mono sm"
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
                  className="nvr-btn mono sm"
                  style={{ marginLeft: "auto" }}
                  onClick={(e) => {
                    e.stopPropagation();
                    setMapEditMode(true);
                    setFloorplanWalls((prev) =>
                      prev.filter((_, idx) => idx !== wIdx)
                    );
                    setDraggingWall(null);
                    setSelectedWallIdx((prevIdx) => {
                      if (prevIdx === null) return null;
                      if (prevIdx === wIdx) return null;
                      return prevIdx > wIdx ? prevIdx - 1 : prevIdx;
                    });
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
  );
};
