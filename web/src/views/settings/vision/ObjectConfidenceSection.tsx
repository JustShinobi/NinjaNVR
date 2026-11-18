import React, { useEffect, useState } from "react";
import { ChevronDown, ChevronUp, Plus, Sparkles } from "lucide-react";
import {
  ModelConfig,
  NinjaConfigResponse,
  ObjectFilterConfig,
} from "../../../types/nvr";
import {
  DEFAULT_ATTRIBUTE_LABELS,
  type ScopedConfigLens,
} from "../../../utils/scopedConfigDraft";

export interface ObjectConfidenceSectionProps {
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  configLens: ScopedConfigLens;
  standardLabels: readonly string[];
  isCocoSsdActive: boolean;
  isPt: boolean;
  notify: (msg: string) => void;
  scopeDivider: (group: "global" | "camera") => React.ReactNode;
}

export const ObjectConfidenceSection: React.FC<ObjectConfidenceSectionProps> = ({
  updateDraft,
  configLens,
  standardLabels,
  isCocoSsdActive,
  isPt,
  notify,
  scopeDivider,
}) => {
  const [customLabel, setCustomLabel] = useState("");
  const [selectedFilterLabel, setSelectedFilterLabel] = useState("person");
  const [showAdvancedFilterBox, setShowAdvancedFilterBox] = useState(false);

  const trackedLabels: string[] = configLens.readTrackedLabels();
  const attributeLabels: string[] = configLens.readAttributeLabels();
  const activeSnapshotSelection = configLens.readSnapshotSelection();

  useEffect(() => {
    if (trackedLabels.length > 0 && !trackedLabels.includes(selectedFilterLabel)) {
      setSelectedFilterLabel(trackedLabels[0]);
    }
  }, [trackedLabels, selectedFilterLabel]);

  const activeFilter: ObjectFilterConfig =
    configLens.readObjectFilter(selectedFilterLabel);

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
      configLens.toggleTrackedLabel(next, lbl);
    });
  };

  const toggleAttributeLabel = (lbl: string) => {
    updateDraft((next) => {
      configLens.toggleAttributeLabel(next, lbl);
    });
  };

  const updateSnapshotSelection = (
    selection: "best_overall" | "best_in_zone" | "first_trigger" | "best_attribute"
  ) => {
    updateDraft((next) => {
      configLens.mutateSnapshotSelection(next, selection);
    });
  };

  const updateObjectFilter = (patch: Partial<ObjectFilterConfig>) => {
    updateDraft((next) => {
      configLens.updateObjectFilter(next, selectedFilterLabel, patch);
    });
  };

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

  return (
    <>
      {scopeDivider("camera")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "OBJETOS A DETECTAR" : "OBJECTS TO DETECT"}</span>
            <span className="nvr-yaml-key-tag">objects.track</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span className="nvr-badge live">
              {trackedLabels.length} {isPt ? "CLASSES ATIVAS" : "ACTIVE CLASSES"}
            </span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
            <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", marginRight: 4 }}>
              CLASSES:
            </span>
            {Array.from(new Set([...standardLabels, ...trackedLabels])).map((lbl) => {
              const isTracked = trackedLabels.includes(lbl);
              return (
                <button
                  key={lbl}
                  type="button"
                  className={`nvr-btn mono ${isTracked ? "active" : ""} sm`}
                  onClick={() => toggleTrackLabel(lbl)}
                >
                  {isTracked ? `✓ ${lbl}` : `+ ${lbl}`}
                </button>
              );
            })}
            <input
              className="nvr-input mono sm"
              style={{ width: 155 }}
              placeholder={isPt ? "Outra classe COCO..." : "Custom COCO label..."}
              value={customLabel}
              onChange={(e) => setCustomLabel(e.target.value)}
            />
            <button
              type="button"
              className="nvr-btn sm"
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

          <div className="nvr-subcard" style={{ padding: "8px 10px" }}>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                flexWrap: "wrap",
                gap: 8,
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                  <span
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      fontWeight: 700,
                      color: "var(--signal-accent-text)",
                    }}
                  >
                    {isPt
                      ? "ATRIBUTOS FILHOS EM PASSE ÚNICO (FACE, PLACA & LOGOS DE ENTREGA)"
                      : "ATTRIBUTE LABELS (SINGLE-PASS CHILD BOXES)"}
                  </span>
                  <span className="nvr-yaml-key-tag">objects.attribute_labels</span>
                  {isCocoSsdActive && (
                    <span className="nvr-badge motion">
                      {isPt
                        ? "REQUER MODELO YOLO 15 CLASSES"
                        : "REQUIRES 15-CLASS YOLO MODEL"}
                    </span>
                  )}
                </div>
                <span style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isCocoSsdActive
                    ? isPt
                      ? "O modelo atual (MobileNetV2 SSD COCO) detecta objetos primários (person, car). Atributos filhos (face, placa e logos de entrega) entram em operação ao ativar o modelo YOLO de 15 classes."
                      : "Active model (MobileNetV2 SSD COCO) detects primary classes (person, car). Child attributes (face, license_plate, courier logos) activate when using a 15-Class Security YOLO model."
                    : isPt
                    ? "Vincula face → person e license_plate/logos → veículos sem criar trilhas avulsas."
                    : "Binds face → person and license_plate/delivery logos → vehicles without spawning standalone tracks."}
                </span>
              </div>

              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <span
                  className="mono"
                  style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", whiteSpace: "nowrap" }}
                >
                  {isPt ? "Política de Snapshot:" : "Snapshot Selection Policy:"}
                </span>
                <select
                  className="nvr-select mono sm"
                  aria-label="Snapshot Selection Policy"
                  style={{ width: 248, maxWidth: "100%" }}
                  value={activeSnapshotSelection}
                  onChange={(e) =>
                    updateSnapshotSelection(
                      e.target.value as
                        | "best_overall"
                        | "best_in_zone"
                        | "first_trigger"
                        | "best_attribute"
                    )
                  }
                >
                  <option value="best_attribute">
                    {isPt
                      ? "Melhor Atributo (Face / Placa) (best_attribute)"
                      : "Best Attribute (Face / Plate) (best_attribute)"}
                  </option>
                  <option value="best_in_zone">
                    {isPt ? "Melhor na Zona (best_in_zone)" : "Best in Zone (best_in_zone)"}
                  </option>
                  <option value="best_overall">
                    {isPt ? "Melhor Geral (best_overall)" : "Best Overall (best_overall)"}
                  </option>
                  <option value="first_trigger">
                    {isPt ? "Primeiro Gatilho (first_trigger)" : "First Trigger (first_trigger)"}
                  </option>
                </select>
              </div>
            </div>

            <div
              className={isCocoSsdActive ? "nvr-cfg-disabled-zone" : ""}
              style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}
            >
              {Array.from(
                new Set([...DEFAULT_ATTRIBUTE_LABELS, ...attributeLabels])
              ).map((attrLbl) => {
                const isActive = attributeLabels.includes(attrLbl);
                return (
                  <button
                    key={attrLbl}
                    type="button"
                    className={`nvr-btn mono ${isActive ? "active" : ""} sm`}
                    style={{ borderStyle: isCocoSsdActive ? "dashed" : "solid" }}
                    onClick={() => toggleAttributeLabel(attrLbl)}
                  >
                    {isActive ? `✓ ${attrLbl}` : `+ ${attrLbl}`}
                  </button>
                );
              })}
            </div>
          </div>
        </div>
      </div>

      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "SENSIBILIDADE DE CONFIANÇA" : "CONFIDENCE SENSITIVITY"}</span>
            <span className="nvr-yaml-key-tag">objects.filters</span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: 8 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-accent-text)", display: "flex", alignItems: "center", gap: 5 }}>
                  <Sparkles size={12} />
                  {isPt ? "CALIBRAR CLASSE:" : "TUNE CLASS:"}
                </span>
                {trackedLabels.map((lbl) => {
                  const clsFilter = configLens.readObjectFilter(lbl);
                  const pct = Math.round((clsFilter.threshold ?? 0.7) * 100);
                  return (
                    <button
                      key={lbl}
                      type="button"
                      className={`nvr-btn mono ${selectedFilterLabel === lbl ? "active" : ""} sm`}
                      onClick={() => setSelectedFilterLabel(lbl)}
                    >
                      {lbl} ({pct}%)
                    </button>
                  );
                })}
                {trackedLabels.length === 1 && (
                  <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                    {isPt
                      ? "(Ative mais classes ao lado para calibrá-las individualmente)"
                      : "(Enable more classes to tune them individually)"}
                  </span>
                )}
              </div>

              <div style={{ display: "flex", gap: 5, alignItems: "center", flexWrap: "wrap" }}>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "permissive" ? "active" : ""} sm`}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.4, threshold: 0.58 })
                  }
                >
                  {isPt ? "Permissivo (58%)" : "Permissive (58%)"}
                </button>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "balanced" ? "active" : ""} sm`}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.5, threshold: 0.7 })
                  }
                >
                  {isPt ? "Equilibrado (70%)" : "Balanced (70%)"}
                </button>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "strict" ? "active" : ""} sm`}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.65, threshold: 0.82 })
                  }
                >
                  {isPt ? "Rigoroso (82%)" : "Strict (82%)"}
                </button>
              </div>
            </div>

            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              <div className="nvr-field-row" style={{ padding: "4px 0" }}>
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
              className="nvr-btn mono sm"
              style={{ width: "100%", justifyContent: "space-between" }}
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
                  gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
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
    </>
  );
};

export function checkBundledCocoSsdModel(
  model?: Partial<ModelConfig> | null
): boolean {
  const modelType = (model?.model_type || "ssd").trim().toLowerCase();
  const modelPath = (
    model?.path || "/openvino-model/ssdlite_mobilenet_v2.xml"
  )
    .trim()
    .toLowerCase();
  return modelType === "ssd" && modelPath.includes("ssdlite_mobilenet_v2");
}
