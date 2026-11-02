import React from "react";
import { Copy, Eye } from "lucide-react";
import { useI18n } from "../i18n";
import { NinjaConfigResponse } from "../types/nvr";
import { LiveMsePlayer, extractMaskPolygons } from "../components/VideoPlayers";

interface ZonesWorkspaceProps {
  cameraNames: string[];
  config: NinjaConfigResponse | null;
  zonesSubTab: "visual" | "yaml";
  setZonesSubTab: (tab: "visual" | "yaml") => void;
  inspectorCam: string;
  setInspectorCam: (cam: string) => void;
  rawYaml: string;
  setRawYaml: (yaml: string) => void;
  draftPoints: Array<[number, number]>;
  setDraftPoints: React.Dispatch<React.SetStateAction<Array<[number, number]>>>;
  ffprobeInfo: unknown[] | null;
  handleSaveConfig: (saveOption: "saveonly" | "restart") => Promise<void>;
  notify: (msg: string) => void;
}

export const ZonesWorkspace: React.FC<ZonesWorkspaceProps> = ({
  cameraNames,
  config,
  zonesSubTab,
  setZonesSubTab,
  inspectorCam,
  setInspectorCam,
  rawYaml,
  setRawYaml,
  draftPoints,
  setDraftPoints,
  ffprobeInfo,
  handleSaveConfig,
  notify,
}) => {
  const { t } = useI18n();

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <button
            className={`nvr-btn ${zonesSubTab === "visual" ? "active" : ""}`}
            onClick={() => setZonesSubTab("visual")}
          >
            <Eye size={13} />
            {t.zones.visualTab}
          </button>
          <button
            className={`nvr-btn ${zonesSubTab === "yaml" ? "active" : ""}`}
            onClick={() => setZonesSubTab("yaml")}
          >
            {t.zones.yamlTab}
          </button>

          {zonesSubTab === "visual" && (
            <>
              <span className="nvr-divider" aria-hidden="true" />
              {cameraNames.map((cam) => (
                <button
                  key={cam}
                  className={`nvr-btn mono ${inspectorCam === cam ? "active" : ""}`}
                  style={{ textTransform: "uppercase" }}
                  onClick={() => setInspectorCam(cam)}
                >
                  {cam}
                </button>
              ))}
            </>
          )}
        </div>

        {zonesSubTab === "yaml" && (
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <button
              className="nvr-btn"
              onClick={() => {
                navigator.clipboard?.writeText(rawYaml);
                notify("Copied config.yml to clipboard");
              }}
            >
              <Copy size={12} />
              {t.zones.copyYaml}
            </button>
            <button
              className="nvr-btn primary"
              onClick={() => handleSaveConfig("saveonly")}
            >
              {t.zones.saveOnly}
            </button>
            <button
              className="nvr-btn success"
              onClick={() => handleSaveConfig("restart")}
            >
              {t.zones.saveAndRestart}
            </button>
          </div>
        )}
      </div>

      {zonesSubTab === "visual" ? (
        <div className="nvr-workbench">
          <div className="nvr-panel">
            <div className="nvr-panel-header">
              <span>{inspectorCam.toUpperCase()} // POLYGON VECTORS</span>
            </div>
            <div className="nvr-scroll-y" style={{ padding: 12 }}>
              <div
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-motion-text)",
                  marginBottom: 6,
                }}
              >
                INTERACTIVE POLYGON BUILDER ({draftPoints.length} vertices)
              </div>
              <div
                style={{
                  padding: 8,
                  background: "var(--bg-canvas)",
                  border: "1px solid rgba(245, 158, 11, 0.4)",
                  borderRadius: 3,
                  marginBottom: 12,
                  display: "flex",
                  flexDirection: "column",
                  gap: 6,
                }}
              >
                <div style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  Click anywhere on the camera preview to plot normalized polygon vertices.
                </div>
                <div
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    color: "var(--signal-motion-text)",
                    wordBreak: "break-all",
                    padding: "4px 6px",
                    background: "var(--bg-deep)",
                    borderRadius: 2,
                  }}
                >
                  {draftPoints.length > 0
                    ? draftPoints.map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`).join(",")
                    : "0.010,0.010,0.340,0.010,0.340,0.080,0.010,0.080"}
                </div>
                <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
                  <button
                    className="nvr-btn"
                    style={{ height: 22, fontSize: "var(--text-xs)" }}
                    onClick={() =>
                      setDraftPoints([
                        [0.01, 0.01],
                        [0.34, 0.01],
                        [0.34, 0.08],
                        [0.01, 0.08],
                      ])
                    }
                  >
                    Preset: OSD Clock Mask
                  </button>
                  <button
                    className="nvr-btn primary"
                    style={{ height: 22, fontSize: "var(--text-xs)" }}
                    onClick={() => {
                      const coords =
                        draftPoints.length > 0
                          ? draftPoints
                              .map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`)
                              .join(",")
                          : "0.010,0.010,0.340,0.010,0.340,0.080,0.010,0.080";
                      navigator.clipboard?.writeText(coords);
                      notify("Copied normalized polygon coordinates to clipboard");
                    }}
                  >
                    <Copy size={10} />
                    Copy Coords
                  </button>
                  <button
                    className="nvr-btn"
                    style={{ height: 22, fontSize: "var(--text-xs)" }}
                    disabled={draftPoints.length === 0}
                    onClick={() => setDraftPoints((prev) => prev.slice(0, -1))}
                  >
                    Undo Point
                  </button>
                  <button
                    className="nvr-btn danger"
                    style={{ height: 22, fontSize: "var(--text-xs)" }}
                    disabled={draftPoints.length === 0}
                    onClick={() => setDraftPoints([])}
                  >
                    Clear
                  </button>
                </div>
              </div>

              <div
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-alert-text)",
                  marginBottom: 6,
                }}
              >
                {t.zones.motionMasks} (
                {extractMaskPolygons(config?.cameras?.[inspectorCam]?.motion?.mask).length})
              </div>
              {extractMaskPolygons(config?.cameras?.[inspectorCam]?.motion?.mask).map(
                (m, idx) => (
                  <div
                    key={idx}
                    className="mono"
                    style={{
                      padding: 8,
                      background: "var(--bg-canvas)",
                      border: "1px solid rgba(239, 68, 68, 0.35)",
                      borderRadius: 3,
                      fontSize: "var(--text-xs)",
                      marginBottom: 8,
                      wordBreak: "break-all",
                    }}
                  >
                    MASK #{idx + 1}: {m}
                  </div>
                )
              )}

              <div
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-accent-text)",
                  marginTop: 14,
                  marginBottom: 6,
                }}
              >
                {t.zones.detectionZones} (
                {Object.keys(config?.cameras?.[inspectorCam]?.zones || {}).length})
              </div>
              {Object.entries(config?.cameras?.[inspectorCam]?.zones || {}).map(
                ([zName, zCfg]) => (
                  <div
                    key={zName}
                    className="mono"
                    style={{
                      padding: 8,
                      background: "var(--bg-canvas)",
                      border: "1px solid rgba(14, 165, 233, 0.35)",
                      borderRadius: 3,
                      fontSize: "var(--text-xs)",
                      marginBottom: 8,
                      wordBreak: "break-all",
                    }}
                  >
                    <strong>{zName.toUpperCase()}</strong>: {zCfg.coordinates}
                  </div>
                )
              )}

              <div
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-live-text)",
                  marginTop: 14,
                  marginBottom: 6,
                }}
              >
                {t.zones.streamProbe}
              </div>
              <pre
                className="mono"
                style={{
                  padding: 8,
                  background: "var(--bg-canvas)",
                  border: "1px solid var(--border-hairline)",
                  borderRadius: 3,
                  fontSize: "var(--text-xs)",
                  overflowX: "auto",
                }}
              >
                {JSON.stringify(ffprobeInfo, null, 2)}
              </pre>
            </div>
          </div>

          <div className="nvr-focus-stage">
            <div className="nvr-cam-tile" style={{ flex: 1 }}>
              <LiveMsePlayer
                camera={inspectorCam}
                useMainStream={false}
                cameraConfig={config?.cameras?.[inspectorCam]}
                showVectors={true}
                draftPoints={draftPoints}
                onPlotPoint={(nx, ny) => setDraftPoints((prev) => [...prev, [nx, ny]])}
              />
              <div className="nvr-cam-hud pos-top-right" style={{ pointerEvents: "none" }}>
                <span className="nvr-badge live">
                  {inspectorCam.toUpperCase()} // VECTORS ACTIVE
                </span>
                <span className="nvr-badge">
                  CLICK VIDEO TO PLOT · RED MASK · CYAN ZONE · AMBER DRAFT
                </span>
              </div>
            </div>
          </div>
        </div>
      ) : (
        <div style={{ flex: 1, display: "flex", flexDirection: "column" }}>
          <textarea
            className="nvr-code-editor"
            value={rawYaml}
            onChange={(e) => setRawYaml(e.target.value)}
            spellCheck={false}
            aria-label="config.yml editor"
          />
        </div>
      )}
    </>
  );
};
