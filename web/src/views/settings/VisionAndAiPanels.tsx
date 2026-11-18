import React, { useEffect, useState } from "react";
import { Crosshair, Trash2 } from "lucide-react";
import { SettingsScopeDivider } from "../../components/SettingsScopeDivider";
import { useI18n } from "../../i18n";
import { useNvrStore } from "../../store/useNvrStore";
import { NinjaConfigResponse } from "../../types/nvr";
import {
  type ConfigWorkbenchSession,
  useConfigWorkbenchSession,
} from "../../utils/configWorkbench";
import {
  normalizeFaceProfileName,
  parseFacesApiResponse,
} from "../../utils/forensicTriage";
import {
  createScopedConfigLens,
  DEFAULT_ATTRIBUTE_LABELS,
  toggleAttributeLabelList,
  toggleSubLabelRuleList,
} from "../../utils/scopedConfigDraft";
import { DetectorModelsSection } from "./vision/DetectorModelsSection";
import { FaceRecognitionSection } from "./vision/FaceRecognitionSection";
import { GenAiJudgeSection } from "./vision/GenAiJudgeSection";
import { LprSettingsSection } from "./vision/LprSettingsSection";
import { ObjectConfidenceSection } from "./vision/ObjectConfidenceSection";
import { useTwoStepConfirm } from "./vision/useTwoStepConfirm";
import {
  AI_SETTINGS_SUBSECTIONS,
  type AiSettingsSubSectionId,
  type AiSettingsSubSectionMeta,
  applySecurity15ClassPreset,
  type BatchAutoAnnotateProgress,
  buildLocalFinetuneCliCommand,
  buildModelUploadPayload,
  buildStateRoiConfig,
  COLAB_MODEL_FACTORY_URL,
  computeBatchProgressMetrics,
  computeDatasetReadinessMetrics,
  type DatasetReadinessMetrics,
  DEFAULT_AI_SUBSECTION,
  filterFalsePositiveSignaturesByScope,
  formatDhash64Hex,
  formatFalsePositiveReason,
  formatSignatureBbox,
  formatYoloAnnotationLine,
  isAiSubSectionVisible,
  isBundledCocoSsdModel,
  isSafeDatasetSampleId,
  isSafeGenAiCustomBaseUrl,
  NINJANVR_SECURITY_15CL_LABELMAP_PATH,
  NINJANVR_SECURITY_CLASSES,
  parseRoiClassesInput,
  parseRoiCropInput,
  removeKnownPlateEntry,
  resolveSettingsScopeBadge,
  ROI_ALERT_DURATION_PILLS,
  selectBatchAutoAnnotateCandidates,
  slugifyRoiLocationName,
  STANDARD_LABELS,
  STATE_ROI_SCENARIO_PRESETS,
  type StateRoiPresetId,
  type StateRoiScenarioPreset,
  upsertKnownPlateEntry,
  xyxyToYoloNormalized,
  yoloNormalizedToXyxy,
} from "./vision/visionAiHelpers";

export {
  AI_SETTINGS_SUBSECTIONS,
  applySecurity15ClassPreset,
  buildLocalFinetuneCliCommand,
  buildModelUploadPayload,
  buildStateRoiConfig,
  COLAB_MODEL_FACTORY_URL,
  computeBatchProgressMetrics,
  computeDatasetReadinessMetrics,
  DEFAULT_AI_SUBSECTION,
  DEFAULT_ATTRIBUTE_LABELS,
  filterFalsePositiveSignaturesByScope,
  formatDhash64Hex,
  formatFalsePositiveReason,
  formatSignatureBbox,
  formatYoloAnnotationLine,
  isAiSubSectionVisible,
  isBundledCocoSsdModel,
  isSafeDatasetSampleId,
  isSafeGenAiCustomBaseUrl,
  NINJANVR_SECURITY_15CL_LABELMAP_PATH,
  NINJANVR_SECURITY_CLASSES,
  normalizeFaceProfileName,
  parseFacesApiResponse,
  parseRoiClassesInput,
  parseRoiCropInput,
  removeKnownPlateEntry,
  resolveSettingsScopeBadge,
  ROI_ALERT_DURATION_PILLS,
  selectBatchAutoAnnotateCandidates,
  slugifyRoiLocationName,
  STANDARD_LABELS,
  STATE_ROI_SCENARIO_PRESETS,
  toggleAttributeLabelList,
  toggleSubLabelRuleList,
  upsertKnownPlateEntry,
  xyxyToYoloNormalized,
  yoloNormalizedToXyxy,
};

export type {
  AiSettingsSubSectionId,
  AiSettingsSubSectionMeta,
  BatchAutoAnnotateProgress,
  DatasetReadinessMetrics,
  StateRoiPresetId,
  StateRoiScenarioPreset,
};

export const AiDetectionObjectsSection: React.FC<{
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  scope: "global" | string;
  inspectorCam: string;
  notify: (msg: string) => void;
  onOpenVectorStudio?: (targetCam?: string) => void;
  initialSubSection?: AiSettingsSubSectionId;
  initialDataCardsOpen?: boolean;
  workbenchSession?: ConfigWorkbenchSession;
}> = ({
  draftConfig,
  updateDraft,
  scope,
  inspectorCam,
  notify,
  onOpenVectorStudio,
  initialSubSection = DEFAULT_AI_SUBSECTION,
  initialDataCardsOpen = false,
  workbenchSession,
}) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const contextWorkbench = useConfigWorkbenchSession();
  const activeWorkbench = workbenchSession ?? contextWorkbench;
  const stats = useNvrStore((s) => s.stats);
  const { isArmed, confirmOrArm } = useTwoStepConfirm(3000);
  const [activeSubSection, setActiveSubSection] =
    useState<AiSettingsSubSectionId>(initialSubSection);

  const cameraKeys = Object.keys(draftConfig.cameras || {});
  const [roiTargetCam, setRoiTargetCam] = useState<string>(
    () => inspectorCam || cameraKeys[0] || ""
  );

  useEffect(() => {
    const fallbackCam = inspectorCam || cameraKeys[0] || "";
    if (scope !== "global" && inspectorCam) {
      setRoiTargetCam(inspectorCam);
    } else if (
      !roiTargetCam ||
      (cameraKeys.length > 0 && !cameraKeys.includes(roiTargetCam))
    ) {
      setRoiTargetCam(fallbackCam);
    }
  }, [inspectorCam, scope, cameraKeys.join("\0"), roiTargetCam]);

  const effectiveRoiCam =
    scope === "global"
      ? roiTargetCam || inspectorCam || cameraKeys[0] || ""
      : inspectorCam || cameraKeys[0] || "";
  const scopeCameraCount = cameraKeys.length;
  const configLens = createScopedConfigLens(draftConfig, scope, inspectorCam);
  const isCocoSsdActive = isBundledCocoSsdModel(draftConfig.model);

  const scopeDivider = (group: "global" | "camera") => (
    <SettingsScopeDivider
      group={group}
      scope={scope}
      inspectorCam={inspectorCam}
      cameraCount={scopeCameraCount}
      isPt={isPt}
    />
  );

  const configuredRoiEntries = Object.entries(
    draftConfig.cameras?.[effectiveRoiCam]?.state_rois || {}
  );

  return (
    <div className="nvr-card-grid" style={{ gap: 10 }}>
      <div
        className="nvr-settings-subnav"
        role="navigation"
        aria-label={
          isPt
            ? "Sub-navegação de módulos de IA e Filtros"
            : "AI & Object Filters Module Navigation"
        }
      >
        <div className="nvr-settings-subnav-pills">
          {AI_SETTINGS_SUBSECTIONS.map((sec) => {
            const isActive = activeSubSection === sec.id;
            return (
              <button
                key={sec.id}
                type="button"
                className={`nvr-btn mono ${isActive ? "active" : ""} sm`}
                onClick={() => setActiveSubSection(sec.id)}
              >
                {isPt ? sec.labelPt : sec.labelEn}
              </button>
            );
          })}
        </div>
      </div>

      {isAiSubSectionVisible(activeSubSection, "engine") && (
        <DetectorModelsSection
          draftConfig={draftConfig}
          updateDraft={updateDraft}
          configLens={configLens}
          stats={stats}
          isPt={isPt}
          notify={notify}
          scopeDivider={scopeDivider}
        />
      )}

      {isAiSubSectionVisible(activeSubSection, "objects") && (
        <ObjectConfidenceSection
          draftConfig={draftConfig}
          updateDraft={updateDraft}
          configLens={configLens}
          standardLabels={STANDARD_LABELS}
          isCocoSsdActive={isCocoSsdActive}
          isPt={isPt}
          notify={notify}
          scopeDivider={scopeDivider}
        />
      )}

      {isAiSubSectionVisible(activeSubSection, "genai") && (
        <GenAiJudgeSection
          draftConfig={draftConfig}
          updateDraft={updateDraft}
          scope={scope}
          inspectorCam={inspectorCam}
          isPt={isPt}
          notify={notify}
          initialDataCardsOpen={initialDataCardsOpen}
          scopeDivider={scopeDivider}
          isSafeGenAiCustomBaseUrl={isSafeGenAiCustomBaseUrl}
          filterFalsePositiveSignaturesByScope={filterFalsePositiveSignaturesByScope}
          formatFalsePositiveReason={formatFalsePositiveReason}
          formatSignatureBbox={formatSignatureBbox}
          formatDhash64Hex={formatDhash64Hex}
        />
      )}

      {isAiSubSectionVisible(activeSubSection, "faces") && (
        <FaceRecognitionSection
          draftConfig={draftConfig}
          updateDraft={updateDraft}
          configLens={configLens}
          isPt={isPt}
          notify={notify}
          initialDataCardsOpen={initialDataCardsOpen}
          scopeDivider={scopeDivider}
        />
      )}

      {isAiSubSectionVisible(activeSubSection, "lpr") && (
        <LprSettingsSection
          draftConfig={draftConfig}
          updateDraft={updateDraft}
          inspectorCam={inspectorCam}
          isPt={isPt}
          notify={notify}
          initialDataCardsOpen={initialDataCardsOpen}
          activeWorkbench={activeWorkbench}
          scopeDivider={scopeDivider}
          upsertKnownPlateEntry={upsertKnownPlateEntry}
          removeKnownPlateEntry={removeKnownPlateEntry}
        />
      )}

      {isAiSubSectionVisible(activeSubSection, "state_rois") && (
        <>
          <SettingsScopeDivider
            group="camera"
            scope={effectiveRoiCam}
            inspectorCam={effectiveRoiCam}
            cameraCount={scopeCameraCount}
            isPt={isPt}
          />
          <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
            <div className="nvr-cfg-card-header">
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <span>
                  {isPt ? "MONITORES DE ESTADO (ROI)" : "STATE MONITORS (ROI)"}
                </span>
                {effectiveRoiCam && (
                  <span className="nvr-yaml-key-tag">
                    cameras.{effectiveRoiCam}.state_rois
                  </span>
                )}
                <span className="nvr-badge">{configuredRoiEntries.length}</span>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                {scope === "global" && cameraKeys.length > 0 && (
                  <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
                    <span
                      className="mono"
                      style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                    >
                      {isPt ? "Câmera:" : "Camera:"}
                    </span>
                    <select
                      className="nvr-select mono sm"
                      aria-label="ROI Target Camera"
                      value={effectiveRoiCam}
                      onChange={(e) => setRoiTargetCam(e.target.value)}
                    >
                      {cameraKeys.map((camKey) => (
                        <option key={camKey} value={camKey}>
                          {camKey.toUpperCase()}
                        </option>
                      ))}
                    </select>
                  </div>
                )}
                {onOpenVectorStudio && (
                  <button
                    type="button"
                    className="nvr-btn primary mono sm"
                    data-testid="open-zones-studio-roi"
                    onClick={() => onOpenVectorStudio(effectiveRoiCam)}
                  >
                    <Crosshair size={11} />
                    {isPt
                      ? "Editar ROIs no Vídeo Ao Vivo (Aba 03) →"
                      : "Edit ROIs in Visual Studio (Tab 03) →"}
                  </button>
                )}
              </div>
            </div>
            <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
              <div
                className="nvr-subcard"
                style={{
                  padding: "8px 10px",
                  flexDirection: "row",
                  alignItems: "center",
                  justifyContent: "space-between",
                  flexWrap: "wrap",
                  gap: 10,
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 10, minWidth: 0, flex: 1 }}>
                  <svg
                    width="64"
                    height="36"
                    viewBox="0 0 64 36"
                    aria-label="ROI Crop Viewport Preview"
                    style={{
                      background: "var(--bg-input)",
                      border: "1px solid var(--border-strong)",
                      borderRadius: 2,
                      flexShrink: 0,
                    }}
                  >
                    {configuredRoiEntries.map(([rName, rCfg]) => {
                      const c = rCfg.crop || [0.25, 0.25, 0.75, 0.75];
                      return (
                        <rect
                          key={rName}
                          x={c[0] * 64}
                          y={c[1] * 36}
                          width={Math.max(3, (c[2] - c[0]) * 64)}
                          height={Math.max(3, (c[3] - c[1]) * 36)}
                          fill="rgba(245, 158, 11, 0.28)"
                          stroke="var(--signal-motion)"
                          strokeWidth="1.2"
                        />
                      );
                    })}
                  </svg>
                  <div style={{ display: "flex", flexDirection: "column", gap: 2, minWidth: 0 }}>
                    <span style={{ fontSize: "var(--text-sm)", fontWeight: 600 }}>
                      {isPt
                        ? "Desenhe e ajuste monitores de estado diretamente sobre o vídeo da câmera"
                        : "Draw and configure state monitors directly on the live camera feed"}
                    </span>
                    <span className="nvr-field-desc">
                      {isPt
                        ? "Portões, vagas, entregas e equipamentos fixos são demarcados visualmente na Aba 03 (Zonas, Máscaras & Mov.), sem digitar coordenadas decimais."
                        : "Gates, parking spots, delivery zones, and fixed equipment are framed visually in Tab 03 (ZonesStudio) without manual float coordinates."}
                    </span>
                  </div>
                </div>
                {onOpenVectorStudio && (
                  <button
                    type="button"
                    className="nvr-btn mono sm"
                    onClick={() => onOpenVectorStudio(effectiveRoiCam)}
                  >
                    {isPt ? "Abrir Estúdio Vetorial (Aba 03) ↗" : "Open ZonesStudio (Tab 03) ↗"}
                  </button>
                )}
              </div>

              {configuredRoiEntries.length === 0 ? (
                <div className="nvr-field-desc">
                  {isPt
                    ? "Nenhum monitor de estado configurado nesta câmera. Use o botão acima para demarcá-lo no vídeo ao vivo."
                    : "No state monitors configured on this camera yet. Click the button above to frame a spot on the live video."}
                </div>
              ) : (
                <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                  {configuredRoiEntries.map(([rName, rCfg]) => {
                    const rmKey = `delete-roi:${effectiveRoiCam}:${rName}`;
                    const rmArmed = isArmed(rmKey);
                    return (
                      <div
                        key={rName}
                        className="nvr-subcard"
                        style={{
                          padding: "6px 8px",
                          flexDirection: "row",
                          alignItems: "center",
                          justifyContent: "space-between",
                          flexWrap: "wrap",
                          gap: 8,
                        }}
                      >
                        <div className="mono" style={{ fontSize: "var(--text-xs)" }}>
                          <strong style={{ color: "var(--signal-motion-text)" }}>
                            {rName.toUpperCase()}
                          </strong>{" "}
                          · states=[{(rCfg.classes || ["closed", "open"]).join(", ")}] · interval=
                          {rCfg.interval_sec ?? 5}s · hysteresis={rCfg.hysteresis ?? 3}
                          {rCfg.anomaly_state
                            ? ` · anomaly=${rCfg.anomaly_state} (${
                                rCfg.anomaly_timeout_sec ?? 300
                              }s)`
                            : ""}
                        </div>
                        <div style={{ display: "flex", gap: 4 }}>
                          {onOpenVectorStudio && (
                            <button
                              type="button"
                              className="nvr-btn mono sm"
                              onClick={() => onOpenVectorStudio(effectiveRoiCam)}
                            >
                              {isPt ? "Editar na Aba 03" : "Edit in Tab 03"}
                            </button>
                          )}
                          <button
                            type="button"
                            className="nvr-btn danger mono sm"
                            title={`Remove ROI ${rName}`}
                            onClick={() =>
                              confirmOrArm(rmKey, () =>
                                updateDraft((next) => {
                                  if (next.cameras[effectiveRoiCam]?.state_rois) {
                                    delete next.cameras[effectiveRoiCam].state_rois![rName];
                                  }
                                })
                              )
                            }
                          >
                            <Trash2 size={11} />
                            {rmArmed ? (isPt ? "Confirmar?" : "Confirm?") : null}
                          </button>
                        </div>
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          </div>
        </>
      )}
    </div>
  );
};
