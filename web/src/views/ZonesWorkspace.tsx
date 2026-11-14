import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Camera,
  CheckCircle2,
  Copy,
  Cpu,
  Crosshair,
  FileCode,
  HardDrive,
  RotateCcw,
  Save,
  Shield,
  Sliders,
} from "lucide-react";
import { useI18n } from "../i18n";
import { FfprobeItem, NinjaConfigResponse } from "../types/nvr";
import {
  type ConfigWorkbenchSession,
  createConfigWorkbenchSession,
  createFetchConfigTransport,
  useConfigWorkbenchSession,
  useConfigWorkbenchSnapshot,
} from "../utils/configWorkbench";
import { TacticalCameraPicker } from "../components/TacticalCameraPicker";
import { useNvrStore } from "../store/useNvrStore";
import { countChangedSections, formatUnsavedLabel } from "../utils/settingsUx";
import {
  ChannelsStreamsSection,
  ConsoleUiSection,
} from "./settings/ConsoleAndStreamsPanels";
import { AiDetectionObjectsSection } from "./settings/VisionAndAiPanels";
import { ZonesStudio } from "./settings/ZonesStudio";
import { DatasetStudioSection } from "./settings/DatasetStudio";
import { mergeDeployedModel } from "../utils/datasetStudio";
import {
  CommsAuthUsersSection,
  RecordingReviewSection,
} from "./settings/RecordingAndSecurityPanels";

export type SettingsCategory =
  | "console"
  | "channels"
  | "vision"
  | "ai"
  | "dataset"
  | "recording"
  | "security"
  | "yaml";

import {
  normalizeZonePoint,
  deduplicatePolygonVertices,
  computePolygonSignedArea,
  hasSelfIntersectingEdges,
  isNonDegeneratePolygon,
  sanitizePolygonCoordString,
  sanitizeMaskPolygonsValue,
  sanitizeTripwireLine,
  sanitizeStateRoiConfig,
  sanitizeCameraGeometryDraft,
  sanitizeConfigGeometryDraft,
} from "../utils/visionGeometry";

export {
  normalizeZonePoint,
  deduplicatePolygonVertices,
  computePolygonSignedArea,
  hasSelfIntersectingEdges,
  isNonDegeneratePolygon,
  sanitizePolygonCoordString,
  sanitizeMaskPolygonsValue,
  sanitizeTripwireLine,
  sanitizeStateRoiConfig,
  sanitizeCameraGeometryDraft,
  sanitizeConfigGeometryDraft,
};

interface ZonesWorkspaceProps {
  cameraNames: string[];
  config: NinjaConfigResponse | null;
  setConfig: (cfg: NinjaConfigResponse | null) => void;
  zonesSubTab: "visual" | "yaml";
  setZonesSubTab: (tab: "visual" | "yaml") => void;
  inspectorCam: string;
  setInspectorCam: (cam: string) => void;
  rawYaml: string;
  setRawYaml: (yaml: string) => void;
  draftPoints: Array<[number, number]>;
  setDraftPoints: React.Dispatch<React.SetStateAction<Array<[number, number]>>>;
  ffprobeInfo: FfprobeItem[] | null;
  handleSaveConfig: (saveOption: "saveonly" | "restart") => Promise<void>;
  notify: (msg: string) => void;
  workbenchSession?: ConfigWorkbenchSession;
}

export const ZonesWorkspace: React.FC<ZonesWorkspaceProps> = ({
  cameraNames,
  config,
  setConfig,
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
  workbenchSession,
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const [activeCategory, setActiveCategory] = useState<SettingsCategory>(() =>
    zonesSubTab === "yaml" ? "yaml" : "vision"
  );
  const [scope, setScope] = useState<"global" | string>("camera");
  const [visionReviewOpen, setVisionReviewOpen] = useState(false);

  const contextSession = useConfigWorkbenchSession();
  const sharedSession = workbenchSession ?? contextSession;
  const fallbackSessionRef = useRef<ConfigWorkbenchSession | null>(null);
  if (!sharedSession && !fallbackSessionRef.current) {
    fallbackSessionRef.current = createConfigWorkbenchSession(
      createFetchConfigTransport(),
      {
        initialConfig: config,
        initialRawYaml: rawYaml || "",
        onServerCommitted: (nextCfg, nextYaml) => {
          if (nextCfg) setConfig(nextCfg);
          if (nextYaml) setRawYaml(nextYaml);
        },
      }
    );
  }
  const session = sharedSession ?? fallbackSessionRef.current!;
  const {
    baselineConfig,
    draftConfig,
    rawYaml: sessionRawYaml,
    yamlStale,
    isDirty,
    isSaving,
    saveOutcome,
  } = useConfigWorkbenchSnapshot(session);
  const effectiveRawYaml = sharedSession
    ? sessionRawYaml
    : sessionRawYaml || rawYaml;

  const prevZonesSubTabRef = useRef<"visual" | "yaml">(zonesSubTab);
  const isMountedRef = useRef<boolean>(true);
  const showYamlKeys = useNvrStore((s) => s.showYamlKeys);
  const setShowYamlKeys = useNvrStore((s) => s.setShowYamlKeys);

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    if (!sharedSession && config) {
      session.hydrateFromServer(config);
    }
  }, [sharedSession, session, config]);

  useEffect(() => {
    if (!sharedSession && rawYaml && !isDirty && !isSaving) {
      session.hydrateRawYamlFromServer(rawYaml);
    }
  }, [sharedSession, session, rawYaml, isDirty, isSaving]);

  useEffect(() => {
    const subTabChanged = prevZonesSubTabRef.current !== zonesSubTab;
    prevZonesSubTabRef.current = zonesSubTab;
    if (subTabChanged) {
      if (zonesSubTab === "yaml" && activeCategory !== "yaml") {
        const syncedYaml = session.ensureDraftYamlSynced();
        if (!sharedSession && syncedYaml) {
          setRawYaml(syncedYaml);
        }
        setActiveCategory("yaml");
      } else if (zonesSubTab === "visual" && activeCategory === "yaml") {
        setActiveCategory("vision");
      }
      return;
    }
    if (activeCategory === "yaml" && yamlStale) {
      const syncedYaml = session.ensureDraftYamlSynced();
      if (!sharedSession && syncedYaml) {
        setRawYaml(syncedYaml);
      }
    }
  }, [
    zonesSubTab,
    activeCategory,
    yamlStale,
    session,
    sharedSession,
    setRawYaml,
  ]);

  const allCamKeys = Object.keys(draftConfig?.cameras || {});
  const displayCams = allCamKeys.length > 0 ? allCamKeys : cameraNames;
  const effectiveInspectorCam =
    displayCams.length > 0 && !displayCams.includes(inspectorCam)
      ? displayCams[0]
      : inspectorCam;

  useEffect(() => {
    if (
      displayCams.length > 0 &&
      inspectorCam &&
      !displayCams.includes(inspectorCam)
    ) {
      setInspectorCam(displayCams[0]);
      setDraftPoints([]);
    }
  }, [displayCams, inspectorCam, setInspectorCam, setDraftPoints]);

  const currentCamDetectW =
    draftConfig?.cameras?.[effectiveInspectorCam]?.detect?.width || 1280;
  const currentCamDetectH =
    draftConfig?.cameras?.[effectiveInspectorCam]?.detect?.height || 720;

  const setSanitizedDraftPoints: React.Dispatch<
    React.SetStateAction<Array<[number, number]>>
  > = useCallback(
    (action) => {
      setDraftPoints((prev) => {
        const rawNext = typeof action === "function" ? action(prev) : action;
        if (!Array.isArray(rawNext) || rawNext.length === 0) return [];
        const normalized = rawNext.map((pt) =>
          normalizeZonePoint(pt, currentCamDetectW, currentCamDetectH)
        );
        return deduplicatePolygonVertices(normalized, 0.0005);
      });
    },
    [setDraftPoints, currentCamDetectW, currentCamDetectH]
  );

  const updateDraft = (updater: (next: NinjaConfigResponse) => void) => {
    const syncYamlImmediately =
      activeCategory === "yaml" || zonesSubTab === "yaml";
    const mutation = session.mutateDraft(updater, { syncYamlImmediately });
    if (!mutation) return;
    if (mutation.rejectedGeometryCount > 0) {
      notify(
        isPt
          ? "Polígono ou linha degenerada (colinear / área zero) ignorada para proteger máscaras de privacidade"
          : "Rejected degenerate (collinear / zero-area) polygon or line to protect backend masks"
      );
    }
    if (!sharedSession && mutation.rawYaml !== null) {
      setRawYaml(mutation.rawYaml);
    }
  };

  const handleDiscardDraft = () => {
    session.discardDraft();
    if (!sharedSession) {
      const snap = session.getSnapshot();
      if (snap.rawYaml) {
        setRawYaml(snap.rawYaml);
      }
    }
    setDraftPoints([]);
    notify(
      isPt
        ? "Alterações descartadas e restauradas ao estado salvo"
        : "Reverted unsaved configuration changes"
    );
  };

  const handleSaveStructured = async (restartAfter = false) => {
    if (isSaving) return;
    const outcome = await session.commitStructured({
      restart: restartAfter,
      savedMessage: isPt
        ? "Configuração salva em config.yml"
        : "Structured configuration saved to config.yml",
      restartedMessage: isPt
        ? "Configuração salva e pipeline reiniciado"
        : "Structured configuration saved and pipeline restart triggered",
      failedMessage: isPt
        ? "Falha ao salvar a configuração"
        : "Failed to save configuration",
      networkErrorMessage: isPt
        ? "Erro de rede ao salvar. Nada foi gravado."
        : "Network error while saving. Nothing was written.",
    });
    if (!isMountedRef.current) return;
    if (outcome.rejectedGeometryCount > 0) {
      notify(
        isPt
          ? "Polígono degenerado removido antes de salvar"
          : "Stripped degenerate polygon before saving configuration"
      );
    }
    if (outcome.ok && outcome.config) {
      setConfig(outcome.config);
      if (!sharedSession && outcome.rawYaml) {
        setRawYaml(outcome.rawYaml);
      }
    }
    notify(outcome.message);
  };

  const handleSaveRawYaml = async (saveOption: "saveonly" | "restart") => {
    if (isSaving) return;
    const syncedYaml = session.ensureDraftYamlSynced();
    if (!sharedSession && syncedYaml) {
      setRawYaml(syncedYaml);
    }
    if (sharedSession) {
      await handleSaveConfig(saveOption);
      return;
    }
    await handleSaveConfig(saveOption);
    if (!isMountedRef.current) return;
    session.hydrateRawYamlFromServer(syncedYaml);
  };

  const switchInspectorCamera = (cam: string) => {
    setInspectorCam(cam);
    setDraftPoints([]);
  };

  const selectCategory = (cat: SettingsCategory) => {
    if (cat === "yaml") {
      const syncedYaml = session.ensureDraftYamlSynced();
      if (!sharedSession && syncedYaml) {
        setRawYaml(syncedYaml);
      }
    }
    const nextSubTab = cat === "yaml" ? "yaml" : "visual";
    prevZonesSubTabRef.current = nextSubTab;
    setActiveCategory(cat);
    setVisionReviewOpen(false);
    setZonesSubTab(nextSubTab);
    setDraftPoints([]);
  };

  const effectiveSavedConfig = baselineConfig ?? config;
  const unsavedCount = useMemo(
    () =>
      isDirty && effectiveSavedConfig && draftConfig
        ? countChangedSections(effectiveSavedConfig, draftConfig)
        : 0,
    [isDirty, effectiveSavedConfig, draftConfig]
  );
  const unsavedLabel = isDirty
    ? activeCategory === "yaml" || unsavedCount === 0
      ? isPt
        ? "NÃO SALVO"
        : "UNSAVED"
      : formatUnsavedLabel(unsavedCount, isPt)
    : "";

  const supportsGlobalAndCamScope =
    activeCategory === "ai" || activeCategory === "recording";
  const supportsCameraOnlyBar =
    activeCategory === "channels" || activeCategory === "vision";

  const categories: Array<{
    id: SettingsCategory;
    num: string;
    title: string;
    sub: string;
    icon: React.ReactNode;
  }> = [
    {
      id: "console",
      num: "01",
      title: isPt ? "Console & Interface" : "Console & UI",
      sub: isPt ? "Layout, HUD & Idioma" : "Layout, HUD & Regional",
      icon: <Sliders size={14} />,
    },
    {
      id: "channels",
      num: "02",
      title: isPt ? "Canais & Streams" : "Channels & Streams",
      sub: `${displayCams.length} ${isPt ? "Câmeras" : "Cameras"} · go2rtc`,
      icon: <Camera size={14} />,
    },
    {
      id: "vision",
      num: "03",
      title: isPt ? "Zonas, Máscaras & Mov." : "Zones, Masks & Motion",
      sub: isPt ? "Estúdio Vetorial & Tuner" : "Vector Studio & Tuner",
      icon: <Crosshair size={14} />,
    },
    {
      id: "ai",
      num: "04",
      title: isPt ? "IA & Filtros de Objeto" : "AI & Object Filters",
      sub: "OpenVINO & ByteTrack",
      icon: <Cpu size={14} />,
    },
    {
      id: "dataset",
      num: "05",
      title: isPt ? "Estúdio de Dataset IA" : "AI Dataset Studio",
      sub: isPt ? "Auto-Labeler YOLO & .onnx" : "YOLO Auto-Labeler & .onnx",
      icon: <Cpu size={14} />,
    },
    {
      id: "recording",
      num: "06",
      title: isPt ? "Gravação & Revisão" : "Recording & Review",
      sub: isPt ? "Retenção & Regras de Alerta" : "Retention & Alert Rules",
      icon: <HardDrive size={14} />,
    },
    {
      id: "security",
      num: "07",
      title: isPt ? "Comms, Auth & Usuários" : "Comms, Auth & Users",
      sub: isPt ? "MQTT, TLS & Contas" : "MQTT, TLS & Accounts",
      icon: <Shield size={14} />,
    },
    {
      id: "yaml",
      num: "08",
      title: "Raw config.yml",
      sub: isPt ? "Editor YAML Direto" : "Direct YAML Studio",
      icon: <FileCode size={14} />,
    },
  ];

  return (
    <div className="nvr-settings-shell">
      {/* Left Tactical Category Navigation Rail */}
      <aside className="nvr-settings-rail" aria-label="Settings Categories">
        <div className="nvr-panel-header">
          <span>{isPt ? "BANCADA DE CONFIGURAÇÕES" : "SETTINGS WORKBENCH"}</span>
        </div>
        {categories.map((cat) => {
          const isActive = activeCategory === cat.id;
          return (
            <button
              key={cat.id}
              type="button"
              className={`nvr-settings-nav-item ${isActive ? "active" : ""}`}
              onClick={() => selectCategory(cat.id)}
            >
              <div style={{ display: "flex", alignItems: "center", gap: 9, minWidth: 0, flex: 1 }}>
                <span
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    color: isActive
                      ? "var(--signal-accent-text)"
                      : "var(--text-muted)",
                    fontWeight: 700,
                    flexShrink: 0,
                  }}
                >
                  {cat.num}
                </span>
                <div style={{ minWidth: 0, flex: 1 }}>
                  <div
                    style={{
                      fontSize: "var(--text-sm)",
                      fontWeight: 600,
                      color: isActive
                        ? "var(--text-primary)"
                        : "var(--text-secondary)",
                      whiteSpace: "nowrap",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                    }}
                  >
                    {cat.title}
                  </div>
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--text-muted)",
                      whiteSpace: "nowrap",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                    }}
                  >
                    {cat.sub}
                  </div>
                </div>
              </div>
              {cat.icon}
            </button>
          );
        })}
      </aside>

      {/* Right Main Configuration Stage */}
      <section className={`nvr-settings-stage ${showYamlKeys ? "show-yaml-keys" : ""}`}>
        {/* Contextual Scope & Save Action Header */}
        <div className="nvr-settings-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span
              className="mono"
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                color: "var(--text-secondary)",
                textTransform: "uppercase",
              }}
            >
              {categories.find((c) => c.id === activeCategory)?.num} //{" "}
              {categories.find((c) => c.id === activeCategory)?.title}
            </span>

            {supportsGlobalAndCamScope && (
              <>
                <span className="nvr-divider" aria-hidden="true" />
                <button
                  type="button"
                  className={`nvr-btn mono ${scope === "global" ? "active" : ""} sm`}
                  onClick={() => {
                    setScope("global");
                    setDraftPoints([]);
                  }}
                >
                  {isPt ? "PADRÃO GLOBAL" : "GLOBAL DEFAULTS"}
                </button>
                <TacticalCameraPicker
                  cameras={displayCams}
                  selectedCamera={scope === "global" ? "" : effectiveInspectorCam}
                  onSelect={(cam) => {
                    setScope("camera");
                    switchInspectorCamera(cam);
                  }}
                  cameraGroups={draftConfig?.camera_groups}
                  ariaLabel="Settings camera scope selector"
                />
              </>
            )}

            {supportsCameraOnlyBar && (
              <>
                <span className="nvr-divider" aria-hidden="true" />
                <TacticalCameraPicker
                  cameras={displayCams}
                  selectedCamera={effectiveInspectorCam}
                  onSelect={(cam) => switchInspectorCamera(cam)}
                  cameraGroups={draftConfig?.camera_groups}
                  ariaLabel="Settings channel selector"
                />
                <a
                  href={`/api/camera-proxy/${encodeURIComponent(effectiveInspectorCam)}/`}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="nvr-btn mono"
                  style={{
                    height: 24,
                    padding: "0 8px",
                    fontSize: "var(--text-xs)",
                    textDecoration: "none",
                    display: "inline-flex",
                    alignItems: "center",
                  }}
                  title="Open camera's native Web UI via authenticated SSRF-safe reverse proxy"
                >
                  Open Camera Web UI ↗
                </a>
              </>
            )}
          </div>

          {/* Action Buttons */}
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            {activeCategory === "console" ? (
              <span className="nvr-badge live">
                <CheckCircle2 size={11} />
                {isPt ? "SALVAMENTO AUTOMÁTICO" : "AUTO-SAVED IN BROWSER"}
              </span>
            ) : activeCategory === "yaml" ? (
              <>
                {unsavedLabel && (
                  <span className="nvr-badge motion" role="status">
                    {unsavedLabel}
                  </span>
                )}
                {isDirty && (
                  <button
                    type="button"
                    className="nvr-btn"
                    onClick={handleDiscardDraft}
                    disabled={isSaving}
                  >
                    <RotateCcw size={12} />
                    {isPt ? "Descartar" : "Discard"}
                  </button>
                )}
                <button
                  type="button"
                  className="nvr-btn"
                  onClick={() => {
                    navigator.clipboard?.writeText(effectiveRawYaml);
                    notify("Copied config.yml to clipboard");
                  }}
                >
                  <Copy size={12} />
                  {t.zones.copyYaml}
                </button>
                <button
                  type="button"
                  className="nvr-btn primary"
                  disabled={isSaving}
                  onClick={() => handleSaveRawYaml("saveonly")}
                >
                  {t.zones.saveOnly}
                </button>
                <button
                  type="button"
                  className="nvr-btn success"
                  disabled={isSaving}
                  onClick={() => handleSaveRawYaml("restart")}
                >
                  {t.zones.saveAndRestart}
                </button>
              </>
            ) : activeCategory === "vision" ? (
              <>
                {unsavedLabel && (
                  <span className="nvr-badge motion" role="status">
                    {unsavedLabel}
                  </span>
                )}
                {isDirty || isSaving ? (
                  <button
                    type="button"
                    className="nvr-btn primary"
                    aria-pressed={visionReviewOpen}
                    onClick={() => setVisionReviewOpen(true)}
                    disabled={isSaving}
                  >
                    <Save size={12} />
                    {isSaving
                      ? isPt
                        ? "Salvando…"
                        : "Saving…"
                      : isPt
                      ? "Revisar e salvar"
                      : "Review and save"}
                  </button>
                ) : (
                  <span className="nvr-badge live" role="status">
                    <CheckCircle2 size={11} />
                    {isPt ? "TUDO SALVO" : "ALL SAVED"}
                  </span>
                )}
              </>
            ) : (
              <>
                <button
                  type="button"
                  className={`nvr-btn mono ${showYamlKeys ? "active" : ""}`}
                  aria-pressed={showYamlKeys}
                  onClick={() => setShowYamlKeys(!showYamlKeys)}
                  title={
                    isPt
                      ? "Mostrar as chaves do config.yml ao lado de cada configuração"
                      : "Show the config.yml key next to each setting"
                  }
                >
                  {isPt ? "Chaves YAML" : "YAML keys"}
                </button>
                {unsavedLabel && (
                  <span className="nvr-badge motion" role="status">
                    {unsavedLabel}
                  </span>
                )}
                {isDirty && (
                  <button
                    type="button"
                    className="nvr-btn"
                    onClick={handleDiscardDraft}
                    disabled={isSaving}
                  >
                    <RotateCcw size={12} />
                    {isPt ? "Descartar" : "Discard"}
                  </button>
                )}
                <button
                  type="button"
                  className="nvr-btn primary"
                  onClick={() => handleSaveStructured(false)}
                  disabled={isSaving}
                >
                  <Save size={12} />
                  {t.zones.saveOnly}
                </button>
                <button
                  type="button"
                  className="nvr-btn success"
                  onClick={() => handleSaveStructured(true)}
                  disabled={isSaving}
                >
                  {t.zones.saveAndRestart}
                </button>
              </>
            )}
          </div>
        </div>

        {/* Main Body Content */}
        {activeCategory === "yaml" ? (
          <div style={{ flex: 1, display: "flex", flexDirection: "column" }}>
            <textarea
              className="nvr-code-editor"
              value={effectiveRawYaml}
              onChange={(e) => {
                session.setRawYamlDraft(e.target.value);
                if (!sharedSession) {
                  setRawYaml(e.target.value);
                }
              }}
              spellCheck={false}
              aria-label="config.yml editor"
            />
          </div>
        ) : activeCategory === "vision" && draftConfig ? (
          <ZonesStudio
            draftConfig={draftConfig}
            savedConfig={effectiveSavedConfig}
            updateDraft={updateDraft}
            inspectorCam={effectiveInspectorCam}
            draftPoints={draftPoints}
            setDraftPoints={setSanitizedDraftPoints}
            notify={notify}
            reviewOpen={visionReviewOpen}
            onOpenReview={() => setVisionReviewOpen(true)}
            onCloseReview={() => setVisionReviewOpen(false)}
            onSave={() => handleSaveStructured(false)}
            onDiscard={handleDiscardDraft}
            isSaving={isSaving}
            isDirty={isDirty}
            saveOutcome={saveOutcome}
          />
        ) : (
          <div className="nvr-settings-body">
            {activeCategory === "console" && <ConsoleUiSection notify={notify} />}

            {draftConfig && activeCategory === "channels" && (
              <ChannelsStreamsSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                inspectorCam={effectiveInspectorCam}
                setInspectorCam={switchInspectorCamera}
                ffprobeInfo={ffprobeInfo}
                notify={notify}
                workbenchSession={session}
              />
            )}

            {draftConfig && activeCategory === "ai" && (
              <AiDetectionObjectsSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                scope={scope === "global" ? "global" : effectiveInspectorCam}
                inspectorCam={effectiveInspectorCam}
                notify={notify}
                onOpenVectorStudio={() => selectCategory("vision")}
                workbenchSession={session}
              />
            )}

            {activeCategory === "dataset" && (
              <DatasetStudioSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                notify={notify}
                onModelDeployed={(model) => {
                  // The server already wrote config.yml; rebase it into both baselineConfig and
                  // draftConfig via ConfigWorkbenchSession so a later "Save Only" preserves it.
                  const baseModel =
                    session.getSnapshot().baselineConfig?.model ??
                    draftConfig?.model ??
                    config?.model;
                  if (!baseModel) return;
                  const nextModel = mergeDeployedModel(baseModel, model);
                  void session.applyOutOfBandPatch(
                    { model: nextModel },
                    {
                      persistViaTransport: false,
                      syncYamlImmediately: zonesSubTab === "yaml",
                    }
                  );
                }}
              />
            )}

            {draftConfig && activeCategory === "recording" && (
              <RecordingReviewSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                scope={scope === "global" ? "global" : effectiveInspectorCam}
                inspectorCam={effectiveInspectorCam}
                notify={notify}
              />
            )}

            {draftConfig && activeCategory === "security" && (
              <CommsAuthUsersSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                notify={notify}
              />
            )}
          </div>
        )}
      </section>
    </div>
  );
};
