import React, { useEffect, useState } from "react";
import {
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  Copy,
  Cpu,
  Edit3,
  Plus,
  Sparkles,
  Trash2,
} from "lucide-react";
import {
  coordStringToPoints,
  extractMaskPolygons,
  LiveMsePlayer,
} from "../../components/VideoPlayers";
import { useI18n } from "../../i18n";
import { useNvrStore } from "../../store/useNvrStore";
import {
  CameraDetectConfig,
  CameraMotionConfig,
  NinjaConfigResponse,
  ObjectFilterConfig,
} from "../../types/nvr";
import { ToggleSwitch } from "./ConsoleAndStreamsPanels";

const STANDARD_LABELS = [
  "person",
  "car",
  "bicycle",
  "motorcycle",
  "bus",
  "dog",
  "cat",
  "bird",
];

export const VisionZonesMotionSection: React.FC<{
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  scope: "global" | string;
  inspectorCam: string;
  draftPoints: Array<[number, number]>;
  setDraftPoints: React.Dispatch<React.SetStateAction<Array<[number, number]>>>;
  notify: (msg: string) => void;
}> = ({
  draftConfig,
  updateDraft,
  inspectorCam,
  draftPoints,
  setDraftPoints,
  notify,
}) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const [polyType, setPolyType] = useState<
    "zone" | "tripwire" | "motion_mask" | "object_mask" | "measure_box"
  >("zone");
  const [zoneNameInput, setZoneNameInput] = useState("entrada");
  const [zoneInertia, setZoneInertia] = useState(3);
  const [zoneLoitering, setZoneLoitering] = useState(0);
  const [zoneObjects, setZoneObjects] = useState<string[]>(["person", "car"]);
  const [lineDirection, setLineDirection] = useState<"any" | "a_to_b" | "b_to_a">("any");

  const camCfg = draftConfig.cameras?.[inspectorCam];
  const detW = camCfg?.detect?.width || 1280;
  const detH = camCfg?.detect?.height || 720;

  const motionCfg: CameraMotionConfig = camCfg?.motion || {
    enabled: true,
    threshold: 30,
    lightning_threshold: 0.8,
    improve_contrast: true,
    contour_area: 10,
    delta_alpha: 0.2,
    frame_alpha: 0.01,
    frame_height: 100,
    mask: {},
  };

  const updateMotionField = <K extends keyof CameraMotionConfig>(
    key: K,
    val: CameraMotionConfig[K]
  ) => {
    updateDraft((next) => {
      if (next.cameras[inspectorCam]) {
        next.cameras[inspectorCam].motion[key] = val;
      }
    });
  };

  const applyMotionPreset = (
    threshold: number,
    contourArea: number,
    lightning: number,
    label: string
  ) => {
    updateDraft((next) => {
      const target = next.cameras[inspectorCam]?.motion;
      if (!target) return;
      target.threshold = threshold;
      target.contour_area = contourArea;
      target.lightning_threshold = lightning;
      target.improve_contrast = true;
    });
    notify(`Applied '${label}' motion sensitivity preset`);
  };

  const currentCoordStr =
    draftPoints.length >= 3
      ? draftPoints.map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`).join(",")
      : "";

  const handleCommitPolygon = () => {
    if (polyType === "tripwire") {
      if (draftPoints.length < 2) {
        notify("Plot at least 2 points on the camera preview for a directional tripwire");
        return;
      }
      const lSlug = zoneNameInput
        .trim()
        .toLowerCase()
        .replace(/[^a-z0-9_]/g, "_");
      if (!lSlug || lSlug.length < 2) {
        notify("Enter a tripwire ID with at least 2 characters");
        return;
      }
      const [p1, p2] = draftPoints;
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        if (!cam.lines) cam.lines = {};
        cam.lines[lSlug] = {
          points: [
            Number(p1[0].toFixed(4)),
            Number(p1[1].toFixed(4)),
            Number(p2[0].toFixed(4)),
            Number(p2[1].toFixed(4)),
          ],
          direction: lineDirection,
          objects: zoneObjects,
        };
      });
      setDraftPoints([]);
      notify(`Staged tripwire line '${lSlug}' (${lineDirection}) on ${inspectorCam}`);
      return;
    }

    if (draftPoints.length < 3) {
      notify(
        isPt
          ? "Marque pelo menos 3 pontos no vídeo antes de adicionar o polígono"
          : "Plot at least 3 vertices on the camera preview before adding a polygon"
      );
      return;
    }
    const coordStr = draftPoints
      .map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`)
      .join(",");

    if (polyType === "zone") {
      const zSlug = zoneNameInput
        .trim()
        .toLowerCase()
        .replace(/[^a-z0-9_]/g, "_");
      if (!zSlug || zSlug.length < 2) {
        notify("Enter a zone name with at least 2 characters");
        return;
      }
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        if (!cam.zones) cam.zones = {};
        cam.zones[zSlug] = {
          coordinates: coordStr,
          objects: zoneObjects,
          inertia: zoneInertia,
          loitering_time: zoneLoitering,
        };
      });
      setDraftPoints([]);
      notify(`Staged zone '${zSlug}' on ${inspectorCam}`);
    } else if (polyType === "motion_mask") {
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        const existing = extractMaskPolygons(cam.motion.mask);
        cam.motion.mask = [...existing, coordStr];
      });
      setDraftPoints([]);
      notify(`Staged motion mask polygon on ${inspectorCam}`);
    } else {
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        if (!cam.objects) cam.objects = { track: ["person"], filters: {}, mask: [] };
        const existing = extractMaskPolygons(cam.objects.mask);
        cam.objects.mask = [...existing, coordStr];
      });
      setDraftPoints([]);
      notify(`Staged object mask polygon on ${inspectorCam}`);
    }
  };

  const motionMasks = extractMaskPolygons(camCfg?.motion?.mask);
  const objectMasks = extractMaskPolygons(camCfg?.objects?.mask);
  const zonesMap = camCfg?.zones || {};

  const isIndoorMotion =
    (motionCfg.threshold ?? 30) === 22 && (motionCfg.contour_area ?? 10) === 8;
  const isBalancedMotion =
    (motionCfg.threshold ?? 30) === 30 && (motionCfg.contour_area ?? 10) === 10;
  const isOutdoorMotion =
    (motionCfg.threshold ?? 30) === 45 && (motionCfg.contour_area ?? 10) === 25;

  return (
    <div
      className="nvr-workbench"
      style={{
        flex: 1,
        minHeight: 0,
        gridTemplateColumns: "390px minmax(0, 1fr)",
      }}
    >
      {/* Left Interactive Polygon Builder & Vector Inventory */}
      <div className="nvr-panel">
        <div className="nvr-panel-header">
          <span>
            {inspectorCam.toUpperCase()} //{" "}
            {isPt ? "CONSTRUTOR DE ZONAS & MÁSCARAS" : "POLYGON VECTOR STUDIO"}
          </span>
          <span className="mono">{draftPoints.length} PTS</span>
        </div>
        <div
          className="nvr-scroll-y"
          style={{ padding: 10, display: "flex", flexDirection: "column", gap: 10 }}
        >
          {/* Polygon Plotter Controls */}
          <div
            style={{
              padding: 10,
              background: "var(--bg-canvas)",
              border: "1px solid rgba(245, 158, 11, 0.45)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <div
              className="mono"
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                color: "var(--signal-motion-text)",
                display: "flex",
                justifyContent: "space-between",
              }}
            >
              <span>
                {isPt ? "1. ESCOLHA O TIPO DE POLÍGONO" : "1. SELECT POLYGON TYPE"}
              </span>
              <span>
                {isPt ? "CLIQUE NO VÍDEO" : "CLICK VIDEO TO PLOT"}
              </span>
            </div>

            <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
              {(
                [
                  ["zone", isPt ? "Zona (Ciano)" : "Zone (Cyan)"],
                  ["tripwire", isPt ? "Tripwire (Verde)" : "Tripwire (Green)"],
                  ["motion_mask", isPt ? "Másc. Mov (Verm)" : "Motion Mask (Red)"],
                  ["object_mask", isPt ? "Másc. Obj (Viol)" : "Object Mask (Violet)"],
                  ["measure_box", isPt ? "Medir Caixa" : "Measure Box"],
                ] as const
              ).map(([id, label]) => (
                <button
                  key={id}
                  type="button"
                  className={`nvr-btn ${polyType === id ? "active" : ""}`}
                  style={{ flex: 1, height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
                  onClick={() => setPolyType(id)}
                >
                  {label}
                </button>
              ))}
            </div>

            {polyType === "measure_box" && (
              <div
                className="mono"
                style={{
                  padding: "6px 8px",
                  background: "var(--bg-deep)",
                  border: "1px solid var(--signal-accent)",
                  borderRadius: 2,
                  fontSize: "var(--text-xs)",
                  fontVariantNumeric: "tabular-nums",
                  color: "var(--signal-accent-text)",
                }}
              >
                {draftPoints.length >= 2 ? (
                  (() => {
                    const [a, b] = draftPoints.slice(-2);
                    const wPx = Math.max(1, Math.round(Math.abs(b[0] - a[0]) * detW));
                    const hPx = Math.max(1, Math.round(Math.abs(b[1] - a[1]) * detH));
                    const areaPx = wPx * hPx;
                    const areaPct = ((areaPx / Math.max(1, detW * detH)) * 100).toFixed(2);
                    const ratio = (wPx / hPx).toFixed(2);
                    return `Area: ${areaPx} px² (${areaPct}%) | Ratio: ${ratio} (${wPx}×${hPx})`;
                  })()
                ) : (
                  <span>
                    {isPt
                      ? "Clique 2 cantos opostos no vídeo para medir Área (px & %) e Proporção..."
                      : "Click 2 opposite corners on the video frame to measure Area (px & %) and Ratio..."}
                  </span>
                )}
              </div>
            )}

            {polyType === "tripwire" && (
              <div style={{ display: "flex", gap: 6, alignItems: "center" }}>
                <input
                  className="nvr-input mono"
                  style={{ flex: 1, height: 24, fontSize: "var(--text-xs)" }}
                  placeholder="Tripwire ID (e.g. driveway_line)"
                  aria-label="Tripwire Name"
                  value={zoneNameInput}
                  onChange={(e) => setZoneNameInput(e.target.value)}
                />
                <select
                  className="nvr-select mono"
                  style={{ height: 24, fontSize: "var(--text-xs)" }}
                  aria-label="Tripwire Crossing Direction"
                  value={lineDirection}
                  onChange={(e) =>
                    setLineDirection(e.target.value as "any" | "a_to_b" | "b_to_a")
                  }
                >
                  <option value="any">Direction: ANY</option>
                  <option value="a_to_b">Direction: A &rarr; B</option>
                  <option value="b_to_a">Direction: B &rarr; A</option>
                </select>
              </div>
            )}

            {polyType === "zone" && (
              <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <div style={{ display: "grid", gridTemplateColumns: "1.4fr 1fr 1fr", gap: 6 }}>
                  <div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        marginBottom: 2,
                        whiteSpace: "nowrap",
                      }}
                    >
                      {isPt ? "ID da Zona" : "Zone ID"}
                    </div>
                    <input
                      className="nvr-input mono"
                      style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                      placeholder="e.g. portao"
                      aria-label="Zone Name"
                      value={zoneNameInput}
                      onChange={(e) => setZoneNameInput(e.target.value)}
                    />
                  </div>
                  <div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        marginBottom: 2,
                        whiteSpace: "nowrap",
                      }}
                    >
                      {isPt ? "Inércia (fr)" : "Inertia (fr)"}
                    </div>
                    <input
                      type="number"
                      min={1}
                      max={60}
                      className="nvr-input mono"
                      style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                      aria-label="Zone Inertia Frames"
                      value={zoneInertia}
                      onChange={(e) => setZoneInertia(Math.max(1, Number(e.target.value)))}
                    />
                  </div>
                  <div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        marginBottom: 2,
                        whiteSpace: "nowrap",
                      }}
                    >
                      {isPt ? "Loiter (seg)" : "Loiter (sec)"}
                    </div>
                    <input
                      type="number"
                      min={0}
                      max={300}
                      className="nvr-input mono"
                      style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                      aria-label="Zone Loitering Seconds"
                      value={zoneLoitering}
                      onChange={(e) => setZoneLoitering(Math.max(0, Number(e.target.value)))}
                    />
                  </div>
                </div>
                <div className="nvr-chip-group">
                  {STANDARD_LABELS.slice(0, 6).map((lbl) => {
                    const active = zoneObjects.includes(lbl);
                    return (
                      <button
                        key={lbl}
                        type="button"
                        className={`nvr-btn mono ${active ? "active" : ""}`}
                        style={{ height: 20, padding: "0 6px", fontSize: "var(--text-xs)" }}
                        onClick={() =>
                          setZoneObjects((prev) =>
                            prev.includes(lbl)
                              ? prev.filter((x) => x !== lbl)
                              : [...prev, lbl]
                          )
                        }
                      >
                        {active ? `✓ ${lbl}` : lbl}
                      </button>
                    );
                  })}
                </div>
              </div>
            )}

            <div
              className="mono"
              style={{
                fontSize: "var(--text-xs)",
                color: "var(--signal-motion-text)",
                wordBreak: currentCoordStr ? "break-all" : "normal",
                padding: "4px 6px",
                background: "var(--bg-deep)",
                borderRadius: 2,
              }}
            >
              {currentCoordStr ||
                (isPt
                  ? "Clique 3+ pontos no vídeo ao lado ou use o preset Relógio OSD..."
                  : "Click 3+ points on the live preview or use OSD Clock preset...")}
            </div>

            <div style={{ display: "flex", gap: 5, flexWrap: "wrap" }}>
              <button
                type="button"
                className="nvr-btn success"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                onClick={handleCommitPolygon}
              >
                <Plus size={11} />
                {isPt
                  ? `Adicionar ${polyType === "zone" ? "Zona" : "Máscara"}`
                  : `Stage ${polyType === "zone" ? "Zone" : "Mask"}`}
              </button>
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                onClick={() =>
                  setDraftPoints([
                    [0.01, 0.01],
                    [0.34, 0.01],
                    [0.34, 0.08],
                    [0.01, 0.08],
                  ])
                }
              >
                {isPt ? "Preset Relógio OSD" : "OSD Clock Preset"}
              </button>
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                onClick={() => {
                  const text =
                    currentCoordStr || "0.010,0.010,0.340,0.010,0.340,0.080,0.010,0.080";
                  navigator.clipboard?.writeText(text);
                  notify("Copied polygon coordinates to clipboard");
                }}
              >
                <Copy size={10} />
                Copy
              </button>
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                disabled={draftPoints.length === 0}
                onClick={() => setDraftPoints((p) => p.slice(0, -1))}
              >
                Undo
              </button>
              <button
                type="button"
                className="nvr-btn danger"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                disabled={draftPoints.length === 0}
                onClick={() => setDraftPoints([])}
              >
                Clear
              </button>
            </div>
          </div>

          {/* Unified Active Zones & Masks List */}
          <div className="nvr-cfg-card">
            <div className="nvr-cfg-card-header">
              <span>
                {isPt ? "ZONAS" : "ZONES"} ({Object.keys(zonesMap).length}) ·{" "}
                {isPt ? "MÁSCARAS" : "MASKS"} ({motionMasks.length + objectMasks.length})
              </span>
              <span style={{ color: "var(--signal-accent-text)" }}>
                CYAN / RED / VIOLET
              </span>
            </div>
            <div className="nvr-cfg-card-body" style={{ padding: 8, gap: 6 }}>
              {Object.keys(zonesMap).length === 0 &&
                motionMasks.length === 0 &&
                objectMasks.length === 0 && (
                  <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                    {isPt
                      ? `Nenhuma zona ou máscara configurada em ${inspectorCam}. Clique no vídeo ao lado para desenhar.`
                      : `No zones or masks configured on ${inspectorCam}. Click on the live video to draw one.`}
                  </div>
                )}

              {Object.entries(zonesMap).map(([zName, zCfg]) => (
                <div
                  key={zName}
                  style={{
                    padding: 6,
                    background: "var(--bg-canvas)",
                    border: "1px solid rgba(14, 165, 233, 0.35)",
                    borderRadius: 2,
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div style={{ minWidth: 0, flex: 1 }}>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        color: "var(--signal-accent-text)",
                      }}
                    >
                      ZONE: {zName.toUpperCase()} · inertia={zCfg.inertia ?? 3} · loiter=
                      {zCfg.loitering_time ?? 0}s
                    </div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {(zCfg.objects || ["all"]).join(", ")} · {String(zCfg.coordinates)}
                    </div>
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn"
                      style={{ height: 22, padding: "0 6px" }}
                      title="Load zone vertices into canvas"
                      onClick={() => {
                        setPolyType("zone");
                        setZoneNameInput(zName);
                        setZoneInertia(zCfg.inertia ?? 3);
                        setZoneLoitering(zCfg.loitering_time ?? 0);
                        setZoneObjects(zCfg.objects || ["person"]);
                        setDraftPoints(
                          coordStringToPoints(String(zCfg.coordinates), detW, detH)
                        );
                      }}
                    >
                      <Edit3 size={11} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 22, padding: "0 6px" }}
                      title="Delete zone"
                      onClick={() =>
                        updateDraft((next) => {
                          delete next.cameras[inspectorCam].zones[zName];
                        })
                      }
                    >
                      <Trash2 size={11} />
                    </button>
                  </div>
                </div>
              ))}

              {motionMasks.map((m, idx) => (
                <div
                  key={`mm-${idx}`}
                  style={{
                    padding: 6,
                    background: "var(--bg-canvas)",
                    border: "1px solid rgba(239, 68, 68, 0.35)",
                    borderRadius: 2,
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    <strong style={{ color: "var(--signal-alert-text)" }}>
                      MOTION MASK #{idx + 1}:
                    </strong>{" "}
                    {m}
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() => {
                        setPolyType("motion_mask");
                        setDraftPoints(coordStringToPoints(m, detW, detH));
                      }}
                    >
                      <Edit3 size={10} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() =>
                        updateDraft((next) => {
                          const list = extractMaskPolygons(
                            next.cameras[inspectorCam].motion.mask
                          );
                          list.splice(idx, 1);
                          next.cameras[inspectorCam].motion.mask = list;
                        })
                      }
                    >
                      <Trash2 size={10} />
                    </button>
                  </div>
                </div>
              ))}

              {objectMasks.map((m, idx) => (
                <div
                  key={`om-${idx}`}
                  style={{
                    padding: 6,
                    background: "var(--bg-canvas)",
                    border: "1px solid rgba(168, 85, 247, 0.4)",
                    borderRadius: 2,
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    <strong style={{ color: "#d8b4fe" }}>
                      OBJECT MASK #{idx + 1}:
                    </strong>{" "}
                    {m}
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() => {
                        setPolyType("object_mask");
                        setDraftPoints(coordStringToPoints(m, detW, detH));
                      }}
                    >
                      <Edit3 size={10} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() =>
                        updateDraft((next) => {
                          const list = extractMaskPolygons(
                            next.cameras[inspectorCam].objects?.mask
                          );
                          list.splice(idx, 1);
                          if (next.cameras[inspectorCam].objects) {
                            next.cameras[inspectorCam].objects!.mask = list;
                          }
                        })
                      }
                    >
                      <Trash2 size={10} />
                    </button>
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      </div>

      {/* Right Interactive Camera Video Stage + Docked SIMD Motion Tuner Bar */}
      <div className="nvr-focus-stage">
        <div className="nvr-cam-tile" style={{ flex: 1 }}>
          <LiveMsePlayer
            camera={inspectorCam}
            useMainStream={false}
            cameraConfig={camCfg}
            showVectors={true}
            draftPoints={draftPoints}
            onPlotPoint={(nx, ny) => setDraftPoints((prev) => [...prev, [nx, ny]])}
          />
          <div className="nvr-cam-hud pos-top-right" style={{ pointerEvents: "none" }}>
            <span className="nvr-badge live">
              {inspectorCam.toUpperCase()} // VECTORS ACTIVE
            </span>
            <span className="nvr-badge">
              RED MOTION · VIOLET OBJECT · CYAN ZONE · AMBER DRAFT
            </span>
          </div>
        </div>

        {/* Docked SIMD Motion Tuner Deck Below Live Preview */}
        <div
          style={{
            background: "var(--bg-panel)",
            borderTop: "1px solid var(--border-hairline)",
            padding: "8px 12px",
            display: "flex",
            flexDirection: "column",
            gap: 8,
          }}
        >
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: 8,
              flexWrap: "wrap",
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--text-secondary)",
                }}
              >
                SIMD MOTION TUNER ({inspectorCam.toUpperCase()})
              </span>
              <ToggleSwitch
                checked={motionCfg.enabled ?? true}
                label="Enable Motion Detector"
                onChange={(v) => updateMotionField("enabled", v)}
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 5 }}>
              <button
                type="button"
                className={`nvr-btn ${isIndoorMotion ? "active" : ""}`}
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => applyMotionPreset(22, 8, 0.85, "Indoor / High Sensitivity")}
              >
                {isPt ? "Interno (Sensível)" : "Indoor (Sensitive)"}
              </button>
              <button
                type="button"
                className={`nvr-btn ${isBalancedMotion ? "active" : ""}`}
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => applyMotionPreset(30, 10, 0.8, "Balanced Default")}
              >
                {isPt ? "Equilibrado (Padrão)" : "Balanced (Default)"}
              </button>
              <button
                type="button"
                className={`nvr-btn ${isOutdoorMotion ? "active" : ""}`}
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => applyMotionPreset(45, 25, 0.65, "Outdoor / Trees & Wind")}
              >
                {isPt ? "Externo (Vento/Sol)" : "Outdoor (Trees/Wind)"}
              </button>
              <span className="nvr-divider" aria-hidden="true" />
              <span
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                Contrast:
              </span>
              <ToggleSwitch
                checked={motionCfg.improve_contrast ?? true}
                label="Improve Contrast"
                onChange={(v) => updateMotionField("improve_contrast", v)}
              />
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  updateDraft((next) => {
                    Object.values(next.cameras).forEach((cam) => {
                      cam.motion = {
                        ...cam.motion,
                        enabled: motionCfg.enabled ?? true,
                        threshold: motionCfg.threshold ?? 30,
                        contour_area: motionCfg.contour_area ?? 10,
                        lightning_threshold: motionCfg.lightning_threshold ?? 0.8,
                        improve_contrast: motionCfg.improve_contrast ?? true,
                        frame_height: motionCfg.frame_height ?? 100,
                      };
                    });
                  });
                  notify("Copied motion sensitivity parameters to all cameras");
                }}
              >
                {isPt ? "Aplicar a Todas" : "Apply to All"}
              </button>
            </div>
          </div>

          <div
            style={{
              display: "grid",
              gridTemplateColumns: "minmax(0, 1fr) minmax(0, 1fr) auto auto",
              gap: 10,
              alignItems: "center",
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 6, minWidth: 0 }}>
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                  whiteSpace: "nowrap",
                }}
              >
                {isPt ? "Luminância:" : "Threshold:"}{" "}
                <strong style={{ color: "var(--text-primary)" }}>
                  {motionCfg.threshold ?? 30}
                </strong>
              </span>
              <input
                type="range"
                min={5}
                max={120}
                style={{ flex: 1, minWidth: 0, width: "100%" }}
                aria-label="Motion Threshold"
                value={motionCfg.threshold ?? 30}
                onChange={(e) => updateMotionField("threshold", Number(e.target.value))}
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 6, minWidth: 0 }}>
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                  whiteSpace: "nowrap",
                }}
              >
                {isPt ? "Contorno:" : "Contour:"}{" "}
                <strong style={{ color: "var(--text-primary)" }}>
                  {motionCfg.contour_area ?? 10}px
                </strong>
              </span>
              <input
                type="range"
                min={3}
                max={150}
                style={{ flex: 1, minWidth: 0, width: "100%" }}
                aria-label="Contour Area"
                value={motionCfg.contour_area ?? 10}
                onChange={(e) =>
                  updateMotionField("contour_area", Number(e.target.value))
                }
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 5, whiteSpace: "nowrap" }}>
              <span
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                Lightning:
              </span>
              <input
                type="number"
                step={0.05}
                min={0.2}
                max={1.0}
                className="nvr-input mono"
                style={{ width: 54, height: 24 }}
                value={Number((motionCfg.lightning_threshold ?? 0.8).toFixed(2))}
                onChange={(e) =>
                  updateMotionField("lightning_threshold", Number(e.target.value))
                }
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 5, whiteSpace: "nowrap" }}>
              <span
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                Scale H:
              </span>
              <input
                type="number"
                min={32}
                max={360}
                className="nvr-input mono"
                style={{ width: 54, height: 24 }}
                value={motionCfg.frame_height ?? 100}
                onChange={(e) =>
                  updateMotionField("frame_height", Number(e.target.value))
                }
              />
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

export const AiDetectionObjectsSection: React.FC<{
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  scope: "global" | string;
  inspectorCam: string;
  notify: (msg: string) => void;
}> = ({ draftConfig, updateDraft, scope, inspectorCam, notify }) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const stats = useNvrStore((s) => s.stats);
  const [customLabel, setCustomLabel] = useState("");
  const [selectedFilterLabel, setSelectedFilterLabel] = useState("person");
  const [showAdvancedModel, setShowAdvancedModel] = useState(false);
  const [showAdvancedByteTrack, setShowAdvancedByteTrack] = useState(false);
  const [showAdvancedFilterBox, setShowAdvancedFilterBox] = useState(false);
  const [isCustomResolution, setIsCustomResolution] = useState(false);

  const detKeys = Object.keys(draftConfig.detectors || { detector01: {} });
  const primaryDetKey = detKeys[0] || "detector01";
  const primaryDet = draftConfig.detectors?.[primaryDetKey] || {
    type: "openvino",
    device: "CPU",
  };
  const liveInferMs =
    stats?.detectors?.[primaryDetKey]?.inference_speed?.toFixed(1) || "7.0";

  const modelCfg = draftConfig.model || {
    path: "/openvino-model/ssdlite_mobilenet_v2.xml",
    labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
    width: 300,
    height: 300,
    input_tensor: "nhwc",
    input_pixel_format: "bgr",
    model_type: "ssd",
  };

  const activeDetect: CameraDetectConfig =
    scope === "global"
      ? draftConfig.detect || {
          enabled: true,
          width: 1280,
          height: 720,
          fps: 5,
          min_initialized: 2,
          max_disappeared: 25,
          stationary: { interval: 50, threshold: 50, classifier: true },
        }
      : draftConfig.cameras?.[inspectorCam]?.detect || {
          enabled: true,
          width: 1280,
          height: 720,
          fps: 5,
          min_initialized: 2,
          max_disappeared: 25,
          stationary: { interval: 50, threshold: 50, classifier: true },
        };

  const trackedLabels: string[] =
    scope === "global"
      ? draftConfig.objects?.track || ["person"]
      : draftConfig.cameras?.[inspectorCam]?.objects?.track ||
        draftConfig.objects?.track || ["person"];

  // Keep selectedFilterLabel pointed at an active tracked class whenever possible
  useEffect(() => {
    if (trackedLabels.length > 0 && !trackedLabels.includes(selectedFilterLabel)) {
      setSelectedFilterLabel(trackedLabels[0]);
    }
  }, [trackedLabels, selectedFilterLabel]);

  const activeFilter: ObjectFilterConfig =
    (scope === "global"
      ? draftConfig.objects?.filters?.[selectedFilterLabel]
      : draftConfig.cameras?.[inspectorCam]?.objects?.filters?.[selectedFilterLabel]) || {
      min_area: 0,
      max_area: 24000000,
      min_ratio: 0,
      max_ratio: 24000000,
      threshold: 0.7,
      min_score: 0.5,
    };

  const updateDetect = (patch: Partial<CameraDetectConfig>) => {
    updateDraft((next) => {
      if (scope === "global") {
        next.detect = { ...activeDetect, ...patch };
      } else if (next.cameras[inspectorCam]) {
        next.cameras[inspectorCam].detect = {
          ...next.cameras[inspectorCam].detect,
          ...patch,
        };
      }
    });
  };

  const selectModelProfile = (arch: "ssd" | "yolo-generic" | "yolox") => {
    if (arch !== "ssd") {
      setShowAdvancedModel(true);
    }
    updateDraft((next) => {
      if (arch === "ssd") {
        next.model = {
          ...modelCfg,
          model_type: "ssd",
          width: 300,
          height: 300,
          input_tensor: "nhwc",
          input_pixel_format: "bgr",
          path: "/openvino-model/ssdlite_mobilenet_v2.xml",
        };
      } else if (arch === "yolo-generic") {
        next.model = {
          ...modelCfg,
          model_type: "yolo-generic",
          width: 640,
          height: 640,
          input_tensor: "nchw",
          input_pixel_format: "rgb",
        };
      } else {
        next.model = {
          ...modelCfg,
          model_type: "yolox",
          width: 320,
          height: 320,
          input_tensor: "nchw",
          input_pixel_format: "bgr",
        };
      }
    });
  };

  const toggleTrackLabel = (lbl: string) => {
    const isCurrentlyTracked = trackedLabels.includes(lbl);
    if (isCurrentlyTracked && trackedLabels.length <= 1) {
      setSelectedFilterLabel(lbl);
      notify(
        isPt
          ? "Pelo menos 1 classe de objeto deve permanecer ativa"
          : "At least 1 object class must remain tracked"
      );
      return;
    }
    if (!isCurrentlyTracked) {
      setSelectedFilterLabel(lbl);
    }
    updateDraft((next) => {
      if (scope === "global") {
        if (!next.objects) next.objects = { track: ["person"], filters: {} };
        const curr = next.objects.track || [];
        next.objects.track = curr.includes(lbl)
          ? curr.filter((x) => x !== lbl)
          : [...curr, lbl];
      } else if (next.cameras[inspectorCam]) {
        if (!next.cameras[inspectorCam].objects) {
          next.cameras[inspectorCam].objects = {
            track: [...trackedLabels],
            filters: {},
          };
        }
        const curr = next.cameras[inspectorCam].objects!.track || [];
        next.cameras[inspectorCam].objects!.track = curr.includes(lbl)
          ? curr.filter((x) => x !== lbl)
          : [...curr, lbl];
      }
    });
  };

  const updateObjectFilter = (patch: Partial<ObjectFilterConfig>) => {
    updateDraft((next) => {
      const merged: ObjectFilterConfig = { ...activeFilter, ...patch };
      if (patch.min_score !== undefined && merged.min_score > merged.threshold) {
        merged.threshold = merged.min_score;
      }
      if (patch.threshold !== undefined && merged.threshold < merged.min_score) {
        merged.min_score = merged.threshold;
      }
      if (scope === "global") {
        if (!next.objects) next.objects = { track: ["person"], filters: {} };
        if (!next.objects.filters) next.objects.filters = {};
        next.objects.filters[selectedFilterLabel] = merged;
      } else if (next.cameras[inspectorCam]) {
        if (!next.cameras[inspectorCam].objects) {
          next.cameras[inspectorCam].objects = {
            track: [...trackedLabels],
            filters: {},
          };
        }
        if (!next.cameras[inspectorCam].objects!.filters) {
          next.cameras[inspectorCam].objects!.filters = {};
        }
        next.cameras[inspectorCam].objects!.filters![selectedFilterLabel] = merged;
      }
    });
  };

  const currentResPreset = `${activeDetect.width}x${activeDetect.height}`;
  const isStandardRes = ["1280x720", "640x480", "640x360", "1920x1080"].includes(
    currentResPreset
  );

  const detFps = activeDetect.fps ?? 5;
  const detHits = activeDetect.min_initialized ?? 2;
  const activeTrackPreset: "balanced" | "fast" | "strict" | "custom" =
    detFps === 5 && detHits === 2
      ? "balanced"
      : detFps === 8 && detHits === 1
      ? "fast"
      : detFps === 5 && detHits === 4
      ? "strict"
      : "custom";

  const confThresholdPct = Math.round((activeFilter.threshold ?? 0.7) * 100);
  const confMinScorePct = Math.round((activeFilter.min_score ?? 0.5) * 100);
  const activeConfPreset: "permissive" | "balanced" | "strict" | "custom" =
    confThresholdPct === 58 && confMinScorePct === 40
      ? "permissive"
      : confThresholdPct === 70 && confMinScorePct === 50
      ? "balanced"
      : confThresholdPct === 82 && confMinScorePct === 65
      ? "strict"
      : "custom";

  const modelProfiles: Array<{
    id: "ssd" | "yolo-generic" | "yolox";
    title: string;
    badge: string;
    badgeKind: "live" | "motion" | "";
    desc: string;
    specs: string;
  }> = [
    {
      id: "ssd",
      title: isPt
        ? "CPU Rápido Integrado — MobileNetV2 SSD"
        : "Built-In Fast CPU — MobileNetV2 SSD",
      badge: isPt ? "RECOMENDADO · INTEGRADO" : "RECOMMENDED · BUNDLED",
      badgeKind: "live",
      desc: isPt
        ? "Pré-instalado (~7 ms/frame). Ideal para servidores CPU com baixíssimo uso de recursos."
        : "Pre-installed (~7 ms/frame). Ideal for CPU-only servers with near-zero CPU overhead.",
      specs: "300×300 · NHWC · BGR · 91 COCO Classes",
    },
    {
      id: "yolo-generic",
      title: isPt
        ? "Alta Precisão Customizado — YOLOv8 / YOLOv11"
        : "High-Precision Custom — YOLOv8 / YOLOv11",
      badge: "CUSTOM .ONNX / .XML",
      badgeKind: "",
      desc: isPt
        ? "Maior alcance para objetos distantes ao montar um arquivo .onnx/.xml em /config/."
        : "Higher accuracy on distant objects when mounting a custom .onnx/.xml in /config/.",
      specs: "640×640 · NCHW · RGB · 80 COCO Classes",
    },
    {
      id: "yolox",
      title: isPt
        ? "Edge Equilibrado Customizado — YOLOX"
        : "Balanced Edge Custom — YOLOX",
      badge: "CUSTOM .ONNX / .XML",
      badgeKind: "",
      desc: isPt
        ? "Detector YOLO anchor-free em 320×320 para modelos customizados em /config/."
        : "Anchor-free YOLO detector at 320×320 for custom models in /config/.",
      specs: "320×320 · NCHW · BGR · 80 COCO Classes",
    },
  ];

  return (
    <div className="nvr-card-grid" style={{ gap: 10 }}>
      {/* Guided OpenVINO AI Detector & Model Card */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "MOTOR DE IA & PERFIL DO MODELO (GLOBAL)"
              : "AI DETECTION ENGINE & MODEL PROFILE (GLOBAL)"}
          </span>
          <span className="nvr-badge live">
            <CheckCircle2 size={11} />
            {primaryDet.device || "CPU"} ACTIVE · {liveInferMs}ms
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 7 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            {modelProfiles.map((prof) => {
              const isSelected = (modelCfg.model_type || "ssd") === prof.id;
              return (
                <div
                  key={prof.id}
                  role="button"
                  tabIndex={0}
                  onClick={() => selectModelProfile(prof.id)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      selectModelProfile(prof.id);
                    }
                  }}
                  style={{
                    padding: "7px 10px",
                    background: isSelected
                      ? "rgba(14, 165, 233, 0.12)"
                      : "var(--bg-canvas)",
                    border: `1px solid ${
                      isSelected ? "var(--signal-accent)" : "var(--border-hairline)"
                    }`,
                    borderRadius: 3,
                    cursor: "pointer",
                    display: "flex",
                    flexDirection: "column",
                    gap: 2,
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: 8,
                    }}
                  >
                    <span
                      style={{
                        fontSize: "var(--text-sm)",
                        fontWeight: 700,
                        color: isSelected
                          ? "var(--signal-accent-text)"
                          : "var(--text-primary)",
                      }}
                    >
                      {prof.title}
                    </span>
                    <span className={`nvr-badge ${prof.badgeKind}`}>{prof.badge}</span>
                  </div>
                  <div
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                    }}
                  >
                    {prof.desc}{" "}
                    <span className="mono" style={{ color: "var(--text-muted)" }}>
                      ({prof.specs})
                    </span>
                  </div>
                </div>
              );
            })}
          </div>

          {/* Step 2: Hardware Accelerator */}
          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                <Cpu size={13} />
                {isPt ? "Dispositivo de Hardware" : "Hardware Execution Device"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Fallback automático para CPU se GPU/NPU não estiver montada."
                  : "Auto-fallbacks to CPU if GPU/NPU is not mounted in container."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              aria-label="OpenVINO Execution Device"
              value={primaryDet.device || "CPU"}
              onChange={(e) => {
                const dev = e.target.value;
                updateDraft((next) => {
                  if (!next.detectors) next.detectors = {};
                  next.detectors[primaryDetKey] = {
                    ...primaryDet,
                    type: "openvino",
                    device: dev,
                  };
                });
              }}
            >
              <option value="CPU">
                {isPt ? "CPU — Pronto (Recomendado)" : "CPU — Ready (Recommended)"}
              </option>
              <option value="AUTO">
                {isPt ? "AUTO — Melhor Disponível" : "AUTO — Best Available"}
              </option>
              <option value="GPU">GPU — Intel iGPU / Arc</option>
              <option value="NPU">NPU — Intel AI Boost</option>
            </select>
          </div>

          {modelCfg.model_type !== "ssd" &&
            modelCfg.path.includes("ssdlite_mobilenet_v2") && (
              <div
                className="mono"
                style={{
                  padding: "6px 8px",
                  background: "rgba(245, 158, 11, 0.14)",
                  border: "1px solid rgba(245, 158, 11, 0.45)",
                  borderRadius: 3,
                  fontSize: "var(--text-xs)",
                  color: "var(--signal-motion-text)",
                }}
              >
                {isPt
                  ? "AVISO: Caminho ainda aponta para ssdlite_mobilenet_v2.xml (SSD). Informe o caminho do arquivo .onnx/.xml abaixo."
                  : "NOTE: Path still points to bundled ssdlite_mobilenet_v2.xml (SSD). Provide a custom YOLO .onnx/.xml path below."}
              </div>
            )}

          {/* Collapsible Advanced Tensor & Path Overrides */}
          <button
            type="button"
            className="nvr-btn mono"
            style={{
              width: "100%",
              justifyContent: "space-between",
              height: 24,
              fontSize: "var(--text-xs)",
            }}
            onClick={() => setShowAdvancedModel((v) => !v)}
          >
            <span>
              {isPt
                ? `AVANÇADO: TENSOR & CAMINHO (${modelCfg.width}×${modelCfg.height} · ${modelCfg.input_tensor.toUpperCase()} · ${modelCfg.input_pixel_format.toUpperCase()})`
                : `ADVANCED TENSOR & PATH OVERRIDES (${modelCfg.width}×${modelCfg.height} · ${modelCfg.input_tensor.toUpperCase()} · ${modelCfg.input_pixel_format.toUpperCase()})`}
            </span>
            {showAdvancedModel ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
          </button>

          {showAdvancedModel && (
            <div
              style={{
                padding: 10,
                background: "var(--bg-canvas)",
                border: "1px solid var(--border-hairline)",
                borderRadius: 3,
                display: "flex",
                flexDirection: "column",
                gap: 8,
              }}
            >
              <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr 1fr", gap: 6 }}>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Width
                  </div>
                  <input
                    type="number"
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={modelCfg.width}
                    onChange={(e) =>
                      updateDraft((next) => {
                        next.model = { ...modelCfg, width: Number(e.target.value) };
                      })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Height
                  </div>
                  <input
                    type="number"
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={modelCfg.height}
                    onChange={(e) =>
                      updateDraft((next) => {
                        next.model = { ...modelCfg, height: Number(e.target.value) };
                      })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Tensor
                  </div>
                  <select
                    className="nvr-select mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={modelCfg.input_tensor}
                    onChange={(e) =>
                      updateDraft((next) => {
                        next.model = { ...modelCfg, input_tensor: e.target.value };
                      })
                    }
                  >
                    <option value="nhwc">NHWC</option>
                    <option value="nchw">NCHW</option>
                  </select>
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Color
                  </div>
                  <select
                    className="nvr-select mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={modelCfg.input_pixel_format}
                    onChange={(e) =>
                      updateDraft((next) => {
                        next.model = {
                          ...modelCfg,
                          input_pixel_format: e.target.value,
                        };
                      })
                    }
                  >
                    <option value="bgr">BGR</option>
                    <option value="rgb">RGB</option>
                  </select>
                </div>
              </div>

              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  Model XML / ONNX File Path
                </div>
                <input
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={modelCfg.path}
                  onChange={(e) =>
                    updateDraft((next) => {
                      next.model = { ...modelCfg, path: e.target.value };
                    })
                  }
                />
              </div>
            </div>
          )}
        </div>
      </div>

      {/* Guided Detect & ByteTrack Behavior Card */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt ? "DETECÇÃO & RASTREAMENTO" : "CAMERA DETECTION & TRACKING"} (
            {scope === "global"
              ? isPt
                ? "PADRÃO GLOBAL"
                : "GLOBAL"
              : inspectorCam.toUpperCase()}
            )
          </span>
          <button
            type="button"
            className="nvr-btn"
            style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
            onClick={() => {
              updateDraft((next) => {
                Object.values(next.cameras).forEach((cam) => {
                  cam.detect = { ...activeDetect };
                });
              });
              notify("Applied detection & tracking settings to all cameras");
            }}
          >
            {isPt ? "Aplicar a Todas" : "Apply to All Cameras"}
          </button>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Ativar Detecção IA de Objetos" : "Enable AI Object Detection"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Analisa regiões com movimento no substream e rastreia objetos."
                  : "Analyze substream motion regions and track moving objects."}
              </div>
            </div>
            <ToggleSwitch
              checked={activeDetect.enabled}
              label="Enable AI Object Detection"
              onChange={(v) => updateDetect({ enabled: v })}
            />
          </div>

          <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <div className="nvr-field-label">
              {isPt
                ? "Resolução do Substream de Detecção"
                : "Substream Detection Resolution"}
            </div>
            <select
              className="nvr-select mono"
              style={{ width: "100%" }}
              aria-label="Substream Detection Resolution"
              value={isCustomResolution || !isStandardRes ? "custom" : currentResPreset}
              onChange={(e) => {
                const v = e.target.value;
                if (v === "custom") {
                  setIsCustomResolution(true);
                } else {
                  setIsCustomResolution(false);
                  if (v === "1280x720") updateDetect({ width: 1280, height: 720 });
                  else if (v === "640x480") updateDetect({ width: 640, height: 480 });
                  else if (v === "640x360") updateDetect({ width: 640, height: 360 });
                  else if (v === "1920x1080") updateDetect({ width: 1920, height: 1080 });
                }
              }}
            >
              <option value="1280x720">
                {isPt
                  ? "720p HD (1280×720 · 16:9 Recomendado)"
                  : "720p HD (1280×720 · 16:9 Recommended)"}
              </option>
              <option value="640x480">
                {isPt ? "480p SD (640×480 · 4:3 Baixa CPU)" : "480p SD (640×480 · 4:3 Low CPU)"}
              </option>
              <option value="640x360">
                {isPt
                  ? "360p Compacto (640×360 · 16:9 Ultra-Leve)"
                  : "360p Compact (640×360 · 16:9 Ultra-Low CPU)"}
              </option>
              <option value="1920x1080">
                {isPt ? "1080p Full HD (1920×1080 · Alta CPU)" : "1080p Full HD (1920×1080 · High CPU)"}
              </option>
              <option value="custom">
                {isPt
                  ? `Resolução Customizada (${activeDetect.width}×${activeDetect.height})...`
                  : `Custom Resolution (${activeDetect.width}×${activeDetect.height})...`}
              </option>
            </select>

            {(isCustomResolution || !isStandardRes) && (
              <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, marginTop: 4 }}>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {isPt ? "Largura (px)" : "Width (px)"}
                  </div>
                  <input
                    type="number"
                    min={160}
                    max={3840}
                    step={4}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeDetect.width}
                    onChange={(e) => updateDetect({ width: Number(e.target.value) })}
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {isPt ? "Altura (px)" : "Height (px)"}
                  </div>
                  <input
                    type="number"
                    min={120}
                    max={2160}
                    step={4}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeDetect.height}
                    onChange={(e) => updateDetect({ height: Number(e.target.value) })}
                  />
                </div>
              </div>
            )}
          </div>

          {/* 1-Click Tracking Sensitivity Presets */}
          <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", fontWeight: 700 }}>
            {isPt ? "PERFIL DE RESPOSTA DO RASTREADOR" : "TRACKING RESPONSIVENESS PRESET"}
          </div>
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr 1fr", gap: 6 }}>
            <button
              type="button"
              className={`nvr-btn ${activeTrackPreset === "balanced" ? "active" : ""}`}
              style={{
                height: "auto",
                padding: "6px 8px",
                flexDirection: "column",
                alignItems: "flex-start",
                gap: 2,
              }}
              onClick={() =>
                updateDetect({
                  fps: 5,
                  min_initialized: 2,
                  max_disappeared: 25,
                  stationary: { interval: 50, threshold: 50, classifier: true },
                })
              }
            >
              <span style={{ fontWeight: 700, fontSize: "var(--text-xs)" }}>
                {isPt ? "Equilibrado" : "Balanced (Default)"}
              </span>
              <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                5 FPS · 0.4s confirm
              </span>
            </button>

            <button
              type="button"
              className={`nvr-btn ${activeTrackPreset === "fast" ? "active" : ""}`}
              style={{
                height: "auto",
                padding: "6px 8px",
                flexDirection: "column",
                alignItems: "flex-start",
                gap: 2,
              }}
              onClick={() =>
                updateDetect({
                  fps: 8,
                  min_initialized: 1,
                  max_disappeared: 40,
                  stationary: { interval: 50, threshold: 50, classifier: true },
                })
              }
            >
              <span style={{ fontWeight: 700, fontSize: "var(--text-xs)" }}>
                {isPt ? "Entrada Rápida" : "Fast Entryway"}
              </span>
              <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                8 FPS · Instant hit
              </span>
            </button>

            <button
              type="button"
              className={`nvr-btn ${activeTrackPreset === "strict" ? "active" : ""}`}
              style={{
                height: "auto",
                padding: "6px 8px",
                flexDirection: "column",
                alignItems: "flex-start",
                gap: 2,
              }}
              onClick={() =>
                updateDetect({
                  fps: 5,
                  min_initialized: 4,
                  max_disappeared: 20,
                  stationary: { interval: 40, threshold: 40, classifier: true },
                })
              }
            >
              <span style={{ fontWeight: 700, fontSize: "var(--text-xs)" }}>
                {isPt ? "Externo Rigoroso" : "Strict Outdoor"}
              </span>
              <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                5 FPS · 4-hit filter
              </span>
            </button>
          </div>

          {/* Collapsible Advanced ByteTrack Frame Parameters */}
          <button
            type="button"
            className="nvr-btn mono"
            style={{
              width: "100%",
              justifyContent: "space-between",
              height: 24,
              fontSize: "var(--text-xs)",
            }}
            onClick={() => setShowAdvancedByteTrack((v) => !v)}
          >
            <span>
              {isPt
                ? `AVANÇADO: BYTETRACK (${detFps} FPS · ${detHits} HITS)`
                : `ADVANCED BYTETRACK PARAMETERS (${detFps} FPS · ${detHits} HITS)`}
            </span>
            {showAdvancedByteTrack ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
          </button>

          {showAdvancedByteTrack && (
            <div
              style={{
                padding: 8,
                background: "var(--bg-canvas)",
                border: "1px solid var(--border-hairline)",
                borderRadius: 3,
                display: "grid",
                gridTemplateColumns: "1fr 1fr 1fr 1fr",
                gap: 6,
              }}
            >
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  FPS
                </div>
                <input
                  type="number"
                  min={1}
                  max={30}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeDetect.fps}
                  onChange={(e) => updateDetect({ fps: Number(e.target.value) })}
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  Min Hits
                </div>
                <input
                  type="number"
                  min={1}
                  max={20}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeDetect.min_initialized ?? 2}
                  onChange={(e) =>
                    updateDetect({ min_initialized: Number(e.target.value) })
                  }
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  Max Gone
                </div>
                <input
                  type="number"
                  min={5}
                  max={200}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeDetect.max_disappeared ?? 25}
                  onChange={(e) =>
                    updateDetect({ max_disappeared: Number(e.target.value) })
                  }
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  Stat. Frames
                </div>
                <input
                  type="number"
                  min={10}
                  max={500}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={activeDetect.stationary?.threshold ?? 50}
                  onChange={(e) =>
                    updateDetect({
                      stationary: {
                        interval: activeDetect.stationary?.interval ?? 50,
                        threshold: Number(e.target.value),
                        classifier: activeDetect.stationary?.classifier ?? true,
                      },
                    })
                  }
                />
              </div>
            </div>
          )}
        </div>
      </div>

      {/* Tracked Objects & Intuitive Confidence Sliders */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "CLASSES DE OBJETOS MONITORADAS & SENSIBILIDADE DE CONFIANÇA"
              : "OBJECTS TO DETECT & CONFIDENCE SENSITIVITY"}{" "}
            ({scope === "global" ? "GLOBAL" : inspectorCam.toUpperCase()})
          </span>
          <span className="mono">
            {trackedLabels.length} {isPt ? "CLASSES ATIVAS" : "ACTIVE CLASSES"}
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
            <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", marginRight: 4 }}>
              {isPt ? "1. ATIVAR/DESATIVAR CLASSES:" : "1. ENABLE/DISABLE CLASSES:"}
            </span>
            {Array.from(new Set([...STANDARD_LABELS, ...trackedLabels])).map((lbl) => {
              const isTracked = trackedLabels.includes(lbl);
              return (
                <button
                  key={lbl}
                  type="button"
                  className={`nvr-btn mono ${isTracked ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() => toggleTrackLabel(lbl)}
                >
                  {isTracked ? `✓ ${lbl}` : `+ ${lbl}`}
                </button>
              );
            })}
            <input
              className="nvr-input mono"
              style={{ width: 140, height: 24, fontSize: "var(--text-xs)" }}
              placeholder={isPt ? "Outra classe COCO..." : "Custom COCO label..."}
              value={customLabel}
              onChange={(e) => setCustomLabel(e.target.value)}
            />
            <button
              type="button"
              className="nvr-btn"
              style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
              onClick={() => {
                const clean = customLabel.trim().toLowerCase();
                if (!clean) return;
                if (!trackedLabels.includes(clean)) toggleTrackLabel(clean);
                setSelectedFilterLabel(clean);
                setCustomLabel("");
              }}
            >
              <Plus size={11} />
              {isPt ? "Adicionar" : "Add"}
            </button>
          </div>

          <div
            style={{
              padding: "8px 10px",
              background: "var(--bg-canvas)",
              border: "1px solid var(--border-hairline)",
              borderRadius: 3,
              display: "flex",
              flexDirection: "column",
              gap: 8,
            }}
          >
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: 8 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-accent-text)", display: "flex", alignItems: "center", gap: 5 }}>
                  <Sparkles size={12} />
                  {isPt ? "2. CALIBRAR CLASSE ATIVA:" : "2. TUNE ACTIVE CLASS:"}
                </span>
                {trackedLabels.map((lbl) => (
                  <button
                    key={lbl}
                    type="button"
                    className={`nvr-btn mono ${selectedFilterLabel === lbl ? "active" : ""}`}
                    style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                    onClick={() => setSelectedFilterLabel(lbl)}
                  >
                    {lbl}
                  </button>
                ))}
              </div>

              <div style={{ display: "flex", gap: 5, alignItems: "center" }}>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "permissive" ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.4, threshold: 0.58 })
                  }
                >
                  {isPt ? "Permissivo (58%)" : "Permissive (58%)"}
                </button>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "balanced" ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.5, threshold: 0.7 })
                  }
                >
                  {isPt ? "Equilibrado (70%)" : "Balanced (70%)"}
                </button>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "strict" ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.65, threshold: 0.82 })
                  }
                >
                  {isPt ? "Rigoroso (82%)" : "Strict (82%)"}
                </button>
              </div>
            </div>

            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 12 }}>
              <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
                <div className="nvr-field-meta">
                  <div className="nvr-field-label">
                    {isPt
                      ? `Confiança p/ Confirmar '${selectedFilterLabel}' (${confThresholdPct}%)`
                      : `Confirmed '${selectedFilterLabel}' Confidence (${confThresholdPct}%)`}
                  </div>
                  <div className="nvr-field-desc">
                    {isPt
                      ? "Pontuação necessária para confirmar evento real e alerta."
                      : "Score required to confirm object as a real event and alert."}
                  </div>
                </div>
                <input
                  type="range"
                  min={35}
                  max={95}
                  aria-label="Confirmed Object Confidence"
                  value={confThresholdPct}
                  onChange={(e) =>
                    updateObjectFilter({
                      threshold: Number((Number(e.target.value) / 100).toFixed(2)),
                    })
                  }
                />
              </div>

              <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
                <div className="nvr-field-meta">
                  <div className="nvr-field-label">
                    {isPt
                      ? `Pontuação Mínima por Frame (${confMinScorePct}%)`
                      : `Single-Frame Minimum Score (${confMinScorePct}%)`}
                  </div>
                  <div className="nvr-field-desc">
                    {isPt
                      ? "Detecções abaixo deste valor em um frame isolado são ignoradas."
                      : "Detections below this score on a single frame are ignored."}
                  </div>
                </div>
                <input
                  type="range"
                  min={20}
                  max={90}
                  aria-label="Single-Frame Minimum Score"
                  value={confMinScorePct}
                  onChange={(e) =>
                    updateObjectFilter({
                      min_score: Number((Number(e.target.value) / 100).toFixed(2)),
                    })
                  }
                />
              </div>
            </div>

            <button
              type="button"
              className="nvr-btn mono"
              style={{
                width: "100%",
                justifyContent: "space-between",
                height: 22,
                fontSize: "var(--text-xs)",
              }}
              onClick={() => setShowAdvancedFilterBox((v) => !v)}
            >
              <span>
                {isPt
                  ? `AVANÇADO: LIMITES DE ÁREA & PROPORÇÃO (ÁREA MÍN: ${activeFilter.min_area}px)`
                  : `ADVANCED BOUNDING-BOX SIZE & ASPECT RATIO LIMITS (MIN AREA: ${activeFilter.min_area}px)`}
              </span>
              {showAdvancedFilterBox ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
            </button>

            {showAdvancedFilterBox && (
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(4, 1fr)",
                  gap: 8,
                  paddingTop: 2,
                }}
              >
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Min Area (px)
                  </div>
                  <input
                    type="number"
                    min={0}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeFilter.min_area}
                    onChange={(e) =>
                      updateObjectFilter({ min_area: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Max Area (px)
                  </div>
                  <input
                    type="number"
                    min={100}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeFilter.max_area}
                    onChange={(e) =>
                      updateObjectFilter({ max_area: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Min Ratio (W/H)
                  </div>
                  <input
                    type="number"
                    step={0.1}
                    min={0}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeFilter.min_ratio}
                    onChange={(e) =>
                      updateObjectFilter({ min_ratio: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Max Ratio (W/H)
                  </div>
                  <input
                    type="number"
                    step={0.1}
                    min={0.1}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeFilter.max_ratio}
                    onChange={(e) =>
                      updateObjectFilter({ max_ratio: Number(e.target.value) })
                    }
                  />
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
};
