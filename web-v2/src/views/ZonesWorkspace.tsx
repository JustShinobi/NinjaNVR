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
import { buildCleanYamlFromDraft } from "../utils/configYaml";
import {
  applyStructuredDraftMutation,
  synthesizeDraftYamlIfNeeded,
} from "../utils/scopedConfigDraft";
import { TacticalCameraPicker } from "../components/TacticalCameraPicker";
import { useNvrStore } from "../store/useNvrStore";
import { countChangedSections, formatUnsavedLabel } from "../utils/settingsUx";
import {
  ChannelsStreamsSection,
  ConsoleUiSection,
} from "./settings/ConsoleAndStreamsPanels";
import { AiDetectionObjectsSection } from "./settings/VisionAndAiPanels";
import { ZonesStudio, type SaveOutcome } from "./settings/ZonesStudio";
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
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const [activeCategory, setActiveCategory] = useState<SettingsCategory>(() =>
    zonesSubTab === "yaml" ? "yaml" : "vision"
  );
  const [scope, setScope] = useState<"global" | string>("camera");
  const [draftConfig, setDraftConfig] = useState<NinjaConfigResponse | null>(() =>
    config ? JSON.parse(JSON.stringify(config)) : null
  );
  const [baselineYaml, setBaselineYaml] = useState<string>(() => rawYaml || "");
  const [isDirty, setIsDirty] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [visionReviewOpen, setVisionReviewOpen] = useState(false);
  const [saveOutcome, setSaveOutcome] = useState<SaveOutcome | null>(null);
  const draftRevisionRef = useRef<number>(0);
  const sanitizedDraftRef = useRef<NinjaConfigResponse | null>(null);
  const latestDraftRef = useRef<NinjaConfigResponse | null>(draftConfig);
  latestDraftRef.current = sanitizedDraftRef.current ?? draftConfig;
  const yamlStaleRef = useRef<boolean>(false);
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
    const subTabChanged = prevZonesSubTabRef.current !== zonesSubTab;
    prevZonesSubTabRef.current = zonesSubTab;
    if (subTabChanged) {
      if (zonesSubTab === "yaml" && activeCategory !== "yaml") {
        const currentDraft = latestDraftRef.current;
        if (yamlStaleRef.current && currentDraft) {
          const synced = synthesizeDraftYamlIfNeeded(
            currentDraft,
            baselineYaml,
            rawYaml,
            yamlStaleRef.current
          );
          yamlStaleRef.current = synced.yamlStale;
          setRawYaml(synced.rawYaml);
        }
        setActiveCategory("yaml");
      } else if (zonesSubTab === "visual" && activeCategory === "yaml") {
        setActiveCategory("vision");
      }
      return;
    }
    if (activeCategory === "yaml" && yamlStaleRef.current && latestDraftRef.current) {
      const synced = synthesizeDraftYamlIfNeeded(
        latestDraftRef.current,
        baselineYaml,
        rawYaml,
        yamlStaleRef.current
      );
      yamlStaleRef.current = synced.yamlStale;
      setRawYaml(synced.rawYaml);
    }
  }, [zonesSubTab, activeCategory, draftConfig, baselineYaml, rawYaml, setRawYaml]);

  // Sync incoming config into draftConfig when no local unsaved changes exist
  useEffect(() => {
    if (config && !isDirty && !isSaving) {
      const cloned = JSON.parse(JSON.stringify(config)) as NinjaConfigResponse;
      sanitizedDraftRef.current = null;
      latestDraftRef.current = cloned;
      setDraftConfig(cloned);
    }
  }, [config, isDirty, isSaving]);

  useEffect(() => {
    if (rawYaml && !isDirty && !isSaving && baselineYaml !== rawYaml) {
      setBaselineYaml(rawYaml);
    }
  }, [rawYaml, baselineYaml, isDirty, isSaving]);

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
    const baseDraft = latestDraftRef.current;
    if (!baseDraft) return;
    const syncYamlImmediately =
      activeCategory === "yaml" || zonesSubTab === "yaml";
    const mutation = applyStructuredDraftMutation(baseDraft, updater, {
      syncYamlImmediately,
      baselineYaml: baselineYaml || rawYaml,
    });
    if (mutation.rejectedGeometryCount > 0) {
      notify(
        isPt
          ? "Polígono ou linha degenerada (colinear / área zero) ignorada para proteger máscaras de privacidade"
          : "Rejected degenerate (collinear / zero-area) polygon or line to protect backend masks"
      );
    }
    sanitizedDraftRef.current = mutation.draft;
    latestDraftRef.current = mutation.draft;
    draftRevisionRef.current += 1;
    setDraftConfig(mutation.draft);
    yamlStaleRef.current = mutation.yamlStale;
    if (mutation.rawYaml !== null) {
      setRawYaml(mutation.rawYaml);
    }
    setIsDirty(true);
  };

  const handleDiscardDraft = () => {
    draftRevisionRef.current += 1;
    sanitizedDraftRef.current = null;
    yamlStaleRef.current = false;
    if (config) {
      const cloned = JSON.parse(JSON.stringify(config)) as NinjaConfigResponse;
      latestDraftRef.current = cloned;
      setDraftConfig(cloned);
    }
    if (baselineYaml) {
      setRawYaml(baselineYaml);
    }
    setDraftPoints([]);
    setIsDirty(false);
    notify(
      isPt
        ? "Alterações descartadas e restauradas ao estado salvo"
        : "Reverted unsaved configuration changes"
    );
  };

  const handleSaveStructured = async (restartAfter = false) => {
    const activeDraft = latestDraftRef.current;
    if (!activeDraft || isSaving) return;
    const report = (ok: boolean, message: string) => {
      notify(message);
      setSaveOutcome({ ok, message, at: Date.now() });
    };
    setSaveOutcome(null);
    const saveRev = draftRevisionRef.current;
    const { cleaned: payloadConfig, rejectedCount } =
      sanitizedDraftRef.current === activeDraft
        ? { cleaned: activeDraft, rejectedCount: 0 }
        : sanitizeConfigGeometryDraft(activeDraft);
    sanitizedDraftRef.current = payloadConfig;
    latestDraftRef.current = payloadConfig;
    if (rejectedCount > 0) {
      notify(
        isPt
          ? "Polígono degenerado removido antes de salvar"
          : "Stripped degenerate polygon before saving configuration"
      );
    }
    setIsSaving(true);
    try {
      const res = await fetch("/api/config/structured", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(payloadConfig),
      });

      if (res.status === 404) {
        // Fallback for pods running an older backend binary without /api/config/structured
        const generatedYaml = buildCleanYamlFromDraft(
          payloadConfig,
          baselineYaml || rawYaml
        );
        const saveRes = await fetch(
          `/api/config/save?save_option=${restartAfter ? "restart" : "saveonly"}`,
          {
            method: "POST",
            headers: { "Content-Type": "text/plain" },
            credentials: "include",
            body: generatedYaml,
          }
        );
        const saveData = await saveRes.json().catch(() => ({}));
        if (!isMountedRef.current) return;
        if (saveRes.ok && saveData.success !== false) {
          setConfig(JSON.parse(JSON.stringify(payloadConfig)));
          setBaselineYaml(generatedYaml);
          if (draftRevisionRef.current === saveRev) {
            yamlStaleRef.current = false;
            setRawYaml(generatedYaml);
            setIsDirty(false);
          }
          if (restartAfter) {
            await fetch("/api/restart", { method: "POST", credentials: "include" });
            if (!isMountedRef.current) return;
          }
          report(
            true,
            restartAfter
              ? isPt
                ? "Configuração salva e NVR reiniciado"
                : "Configuration saved and NVR restart triggered"
              : isPt
              ? "Configuração salva em config.yml"
              : "Configuration saved to config.yml"
          );
        } else {
          report(false, saveData.message || (isPt ? "Falha ao salvar a configuração" : "Failed to save configuration"));
        }
        return;
      }

      const data = await res.json().catch(() => ({}));
      if (!isMountedRef.current) return;
      if (res.ok && data.success !== false) {
        const nextSavedConfig: NinjaConfigResponse =
          data.config || payloadConfig;
        setConfig(JSON.parse(JSON.stringify(nextSavedConfig)));
        if (draftRevisionRef.current === saveRev) {
          const clonedSaved = JSON.parse(
            JSON.stringify(nextSavedConfig)
          ) as NinjaConfigResponse;
          sanitizedDraftRef.current = clonedSaved;
          latestDraftRef.current = clonedSaved;
          setDraftConfig(clonedSaved);
        }
        const nextSavedYaml =
          typeof data.raw_yaml === "string" && data.raw_yaml.trim()
            ? data.raw_yaml
            : buildCleanYamlFromDraft(
                nextSavedConfig,
                baselineYaml || rawYaml
              );
        setBaselineYaml(nextSavedYaml);
        if (draftRevisionRef.current === saveRev) {
          yamlStaleRef.current = false;
          setRawYaml(nextSavedYaml);
          setIsDirty(false);
        }
        if (restartAfter) {
          await fetch("/api/restart", { method: "POST", credentials: "include" });
          if (!isMountedRef.current) return;
          report(
            true,
            isPt
              ? "Configuração salva e pipeline reiniciado"
              : "Structured configuration saved and pipeline restart triggered"
          );
        } else {
          report(
            true,
            isPt
              ? "Configuração salva em config.yml"
              : "Structured configuration saved to config.yml"
          );
        }
      } else {
        report(
          false,
          data.message ||
            (isPt
              ? `Falha ao salvar (HTTP ${res.status})`
              : `Failed to save structured configuration (HTTP ${res.status})`)
        );
      }
    } catch {
      if (isMountedRef.current) {
        report(
          false,
          isPt
            ? "Erro de rede ao salvar. Nada foi gravado."
            : "Network error while saving. Nothing was written."
        );
      }
    } finally {
      if (isMountedRef.current) {
        setIsSaving(false);
      }
    }
  };

  const handleSaveRawYaml = async (saveOption: "saveonly" | "restart") => {
    if (isSaving) return;
    const saveRev = draftRevisionRef.current;
    let savedYamlSnapshot = rawYaml;
    if (yamlStaleRef.current && latestDraftRef.current) {
      const synced = synthesizeDraftYamlIfNeeded(
        latestDraftRef.current,
        baselineYaml,
        rawYaml,
        yamlStaleRef.current
      );
      yamlStaleRef.current = synced.yamlStale;
      savedYamlSnapshot = synced.rawYaml;
      setRawYaml(synced.rawYaml);
    }
    setIsSaving(true);
    try {
      await handleSaveConfig(saveOption);
      if (!isMountedRef.current) return;
      setBaselineYaml(savedYamlSnapshot);
      if (draftRevisionRef.current === saveRev) {
        setIsDirty(false);
      }
    } finally {
      if (isMountedRef.current) {
        setIsSaving(false);
      }
    }
  };

  const switchInspectorCamera = (cam: string) => {
    setInspectorCam(cam);
    setDraftPoints([]);
  };

  const selectCategory = (cat: SettingsCategory) => {
    const currentDraft = latestDraftRef.current;
    if (cat === "yaml" && yamlStaleRef.current && currentDraft) {
      const synced = synthesizeDraftYamlIfNeeded(
        currentDraft,
        baselineYaml,
        rawYaml,
        yamlStaleRef.current
      );
      yamlStaleRef.current = synced.yamlStale;
      setRawYaml(synced.rawYaml);
    }
    const nextSubTab = cat === "yaml" ? "yaml" : "visual";
    prevZonesSubTabRef.current = nextSubTab;
    setActiveCategory(cat);
    setVisionReviewOpen(false);
    setZonesSubTab(nextSubTab);
    setDraftPoints([]);
  };

  const unsavedCount = useMemo(
    () => (isDirty && config && draftConfig ? countChangedSections(config, draftConfig) : 0),
    [isDirty, config, draftConfig]
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
                    navigator.clipboard?.writeText(rawYaml);
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
              value={rawYaml}
              onChange={(e) => {
                draftRevisionRef.current += 1;
                yamlStaleRef.current = false;
                setRawYaml(e.target.value);
                setIsDirty(true);
              }}
              spellCheck={false}
              aria-label="config.yml editor"
            />
          </div>
        ) : activeCategory === "vision" && draftConfig ? (
          <ZonesStudio
            draftConfig={draftConfig}
            savedConfig={config}
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
              />
            )}

            {activeCategory === "dataset" && (
              <DatasetStudioSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                notify={notify}
                onModelDeployed={(model) => {
                  // The server already wrote config.yml; mirror it in the saved config and the
                  // draft so a later "Save Config" does not send the previous model back.
                  const patch = (cfg: NinjaConfigResponse | null): NinjaConfigResponse | null =>
                    cfg && cfg.model ? { ...cfg, model: mergeDeployedModel(cfg.model, model) } : cfg;
                  if (config) setConfig(patch(config));
                  setDraftConfig((prev) => {
                    const next = patch(prev);
                    sanitizedDraftRef.current = next;
                    latestDraftRef.current = next;
                    return next;
                  });
                  yamlStaleRef.current = true;
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
