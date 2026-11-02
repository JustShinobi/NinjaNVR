import React, { useEffect, useState } from "react";
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
  ChannelsStreamsSection,
  ConsoleUiSection,
} from "./settings/ConsoleAndStreamsPanels";
import {
  AiDetectionObjectsSection,
  VisionZonesMotionSection,
} from "./settings/VisionAndAiPanels";
import {
  CommsAuthUsersSection,
  RecordingReviewSection,
} from "./settings/RecordingAndSecurityPanels";

export type SettingsCategory =
  | "console"
  | "channels"
  | "vision"
  | "ai"
  | "recording"
  | "security"
  | "yaml";

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
  const [draftConfig, setDraftConfig] = useState<NinjaConfigResponse | null>(null);
  const [baselineYaml, setBaselineYaml] = useState<string>("");
  const [isDirty, setIsDirty] = useState(false);
  const [isSaving, setIsSaving] = useState(false);

  // Sync incoming config into draftConfig when no local unsaved changes exist
  useEffect(() => {
    if (config && !isDirty) {
      setDraftConfig(JSON.parse(JSON.stringify(config)));
    }
  }, [config, isDirty]);

  useEffect(() => {
    if (rawYaml && !baselineYaml) {
      setBaselineYaml(rawYaml);
    }
  }, [rawYaml, baselineYaml]);

  const updateDraft = (updater: (next: NinjaConfigResponse) => void) => {
    setDraftConfig((prev) => {
      if (!prev) return prev;
      const cloned: NinjaConfigResponse = JSON.parse(JSON.stringify(prev));
      updater(cloned);
      const nextYaml = buildCleanYamlFromDraft(cloned, baselineYaml || rawYaml);
      setRawYaml(nextYaml);
      return cloned;
    });
    setIsDirty(true);
  };

  const handleDiscardDraft = () => {
    if (config) {
      setDraftConfig(JSON.parse(JSON.stringify(config)));
    }
    if (baselineYaml) {
      setRawYaml(baselineYaml);
    }
    setIsDirty(false);
    notify(
      isPt
        ? "Alterações descartadas e restauradas ao estado salvo"
        : "Reverted unsaved configuration changes"
    );
  };

  const handleSaveStructured = async (restartAfter = false) => {
    if (!draftConfig) return;
    setIsSaving(true);
    try {
      const res = await fetch("/api/config/structured", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(draftConfig),
      });

      if (res.status === 404) {
        // Fallback for pods running an older backend binary without /api/config/structured
        const generatedYaml = buildCleanYamlFromDraft(
          draftConfig,
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
        if (saveRes.ok && saveData.success !== false) {
          setConfig(JSON.parse(JSON.stringify(draftConfig)));
          setRawYaml(generatedYaml);
          setBaselineYaml(generatedYaml);
          setIsDirty(false);
          if (restartAfter) {
            await fetch("/api/restart", { method: "POST", credentials: "include" });
          }
          notify(
            restartAfter
              ? isPt
                ? "Configuração salva e NVR reiniciado"
                : "Configuration saved and NVR restart triggered"
              : isPt
              ? "Configuração salva em config.yml"
              : "Configuration saved to config.yml"
          );
        } else {
          notify(saveData.message || "Failed to save configuration");
        }
        return;
      }

      const data = await res.json().catch(() => ({}));
      if (res.ok && data.success !== false) {
        if (data.config) {
          setConfig(data.config);
          setDraftConfig(JSON.parse(JSON.stringify(data.config)));
        }
        if (typeof data.raw_yaml === "string") {
          setRawYaml(data.raw_yaml);
          setBaselineYaml(data.raw_yaml);
        }
        setIsDirty(false);
        if (restartAfter) {
          await fetch("/api/restart", { method: "POST", credentials: "include" });
          notify(
            isPt
              ? "Configuração salva e pipeline reiniciado"
              : "Structured configuration saved and pipeline restart triggered"
          );
        } else {
          notify(
            isPt
              ? "Configuração salva em config.yml"
              : "Structured configuration saved to config.yml"
          );
        }
      } else {
        notify(data.message || "Failed to save structured configuration");
      }
    } catch {
      notify("Network error while saving structured configuration");
    } finally {
      setIsSaving(false);
    }
  };

  const switchInspectorCamera = (cam: string) => {
    setInspectorCam(cam);
    setDraftPoints([]);
  };

  const selectCategory = (cat: SettingsCategory) => {
    setActiveCategory(cat);
    setZonesSubTab(cat === "yaml" ? "yaml" : "visual");
  };

  const allCamKeys = Object.keys(draftConfig?.cameras || {});
  const displayCams = allCamKeys.length > 0 ? allCamKeys : cameraNames;
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
      id: "recording",
      num: "05",
      title: isPt ? "Gravação & Revisão" : "Recording & Review",
      sub: isPt ? "Retenção & Regras de Alerta" : "Retention & Alert Rules",
      icon: <HardDrive size={14} />,
    },
    {
      id: "security",
      num: "06",
      title: isPt ? "Comms, Auth & Usuários" : "Comms, Auth & Users",
      sub: isPt ? "MQTT, TLS & Contas" : "MQTT, TLS & Accounts",
      icon: <Shield size={14} />,
    },
    {
      id: "yaml",
      num: "07",
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
          {isDirty && (
            <span
              className="nvr-badge motion"
              style={{ padding: "1px 5px", fontSize: "var(--text-xs)" }}
            >
              {isPt ? "NÃO SALVO" : "UNSAVED"}
            </span>
          )}
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
              <div style={{ display: "flex", alignItems: "center", gap: 9 }}>
                <span
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    color: isActive
                      ? "var(--signal-accent-text)"
                      : "var(--text-muted)",
                    fontWeight: 700,
                  }}
                >
                  {cat.num}
                </span>
                <div>
                  <div
                    style={{
                      fontSize: "var(--text-sm)",
                      fontWeight: 600,
                      color: isActive
                        ? "var(--text-primary)"
                        : "var(--text-secondary)",
                    }}
                  >
                    {cat.title}
                  </div>
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--text-muted)",
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
      <section className="nvr-settings-stage">
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
                  className={`nvr-btn mono ${scope === "global" ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() => setScope("global")}
                >
                  {isPt ? "PADRÃO GLOBAL" : "GLOBAL DEFAULTS"}
                </button>
                {displayCams.map((cam) => {
                  const isCamScope = scope !== "global" && inspectorCam === cam;
                  return (
                    <button
                      key={cam}
                      type="button"
                      className={`nvr-btn mono ${isCamScope ? "active" : ""}`}
                      style={{
                        height: 24,
                        padding: "0 8px",
                        fontSize: "var(--text-xs)",
                        textTransform: "uppercase",
                      }}
                      onClick={() => {
                        setScope("camera");
                        switchInspectorCamera(cam);
                      }}
                    >
                      {cam}
                    </button>
                  );
                })}
              </>
            )}

            {supportsCameraOnlyBar && (
              <>
                <span className="nvr-divider" aria-hidden="true" />
                {displayCams.map((cam) => (
                  <button
                    key={cam}
                    type="button"
                    className={`nvr-btn mono ${inspectorCam === cam ? "active" : ""}`}
                    style={{
                      height: 24,
                      padding: "0 8px",
                      fontSize: "var(--text-xs)",
                      textTransform: "uppercase",
                    }}
                    onClick={() => switchInspectorCamera(cam)}
                  >
                    {cam}
                  </button>
                ))}
                <a
                  href={`/api/camera-proxy/${encodeURIComponent(inspectorCam)}/`}
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
                  onClick={async () => {
                    setIsDirty(false);
                    setBaselineYaml(rawYaml);
                    await handleSaveConfig("saveonly");
                  }}
                >
                  {t.zones.saveOnly}
                </button>
                <button
                  type="button"
                  className="nvr-btn success"
                  onClick={async () => {
                    setIsDirty(false);
                    setBaselineYaml(rawYaml);
                    await handleSaveConfig("restart");
                  }}
                >
                  {t.zones.saveAndRestart}
                </button>
              </>
            ) : (
              <>
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
                setRawYaml(e.target.value);
                setIsDirty(true);
              }}
              spellCheck={false}
              aria-label="config.yml editor"
            />
          </div>
        ) : activeCategory === "vision" && draftConfig ? (
          <VisionZonesMotionSection
            draftConfig={draftConfig}
            updateDraft={updateDraft}
            scope={inspectorCam}
            inspectorCam={inspectorCam}
            draftPoints={draftPoints}
            setDraftPoints={setDraftPoints}
            notify={notify}
          />
        ) : (
          <div className="nvr-settings-body">
            {activeCategory === "console" && <ConsoleUiSection notify={notify} />}

            {draftConfig && activeCategory === "channels" && (
              <>
                <ChannelsStreamsSection
                  draftConfig={draftConfig}
                  updateDraft={updateDraft}
                  inspectorCam={inspectorCam}
                  setInspectorCam={switchInspectorCamera}
                  ffprobeInfo={ffprobeInfo}
                  notify={notify}
                />
                <div className="nvr-cfg-card" style={{ marginTop: 10 }}>
                  <div className="nvr-cfg-card-header">
                    <span>CAMERA TECHNICAL & MAINTENANCE NOTES // {inspectorCam.toUpperCase()}</span>
                    <span className="mono">cameras.{inspectorCam}.notes</span>
                  </div>
                  <div className="nvr-cfg-card-body">
                    <textarea
                      className="nvr-input mono"
                      rows={3}
                      style={{ width: "100%", height: "auto", padding: 8, resize: "vertical" }}
                      placeholder="PoE switch port, lens focal length, mounting height, or maintenance log..."
                      value={draftConfig.cameras?.[inspectorCam]?.notes || ""}
                      onChange={(e) => {
                        const val = e.target.value;
                        updateDraft((next) => {
                          if (next.cameras[inspectorCam]) {
                            next.cameras[inspectorCam].notes = val;
                          }
                        });
                      }}
                    />
                  </div>
                </div>
              </>
            )}

            {draftConfig && activeCategory === "ai" && (
              <AiDetectionObjectsSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                scope={scope === "global" ? "global" : inspectorCam}
                inspectorCam={inspectorCam}
                notify={notify}
              />
            )}

            {draftConfig && activeCategory === "recording" && (
              <RecordingReviewSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                scope={scope === "global" ? "global" : inspectorCam}
                inspectorCam={inspectorCam}
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
