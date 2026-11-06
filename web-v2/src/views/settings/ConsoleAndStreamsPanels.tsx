import React, { useState } from "react";
import { Plus, RefreshCw, RotateCcw, Trash2 } from "lucide-react";
import { SupportedLocale, useI18n } from "../../i18n";
import { useNvrStore } from "../../store/useNvrStore";
import {
  CameraConfig,
  FfprobeItem,
  NinjaConfigResponse,
} from "../../types/nvr";
import {
  FirstWeekday,
  HudPosition,
  LiveLayoutMode,
  ThemeMode,
} from "../../utils/format";
import { SettingsScopeDivider } from "../../components/SettingsScopeDivider";

const RESERVED_OBJECT_KEYS = new Set(["__proto__", "constructor", "prototype"]);

export function sanitizeConfigEntrySlug(raw: string): string {
  const slug = (raw || "")
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_-]/g, "_");
  if (!slug || !/[a-z0-9]/.test(slug) || RESERVED_OBJECT_KEYS.has(slug)) {
    return "";
  }
  return slug;
}

export function buildOnvifPasswordPatch(
  rawInput: string,
  hasExistingPassword?: boolean,
  clearRequested?: boolean
): { password: string; has_password: boolean; clear_password: boolean } {
  if (clearRequested) {
    return {
      password: "",
      has_password: false,
      clear_password: true,
    };
  }
  const isMaskSentinel = rawInput === "***" || rawInput === "*";
  if (rawInput.length > 0 && !isMaskSentinel) {
    return {
      password: rawInput,
      has_password: Boolean(hasExistingPassword),
      clear_password: false,
    };
  }
  return {
    password: hasExistingPassword ? "***" : "",
    has_password: Boolean(hasExistingPassword),
    clear_password: false,
  };
}

export const ToggleSwitch: React.FC<{
  checked: boolean;
  onChange: (next: boolean) => void;
  label?: string;
}> = ({ checked, onChange, label }) => (
  <button
    type="button"
    role="switch"
    aria-checked={checked}
    aria-label={label}
    className={`nvr-switch ${checked ? "is-on" : ""}`}
    onClick={() => onChange(!checked)}
  >
    <span className="nvr-switch-thumb" />
  </button>
);

export const ConsoleUiSection: React.FC<{
  notify: (msg: string) => void;
}> = ({ notify }) => {
  const { locale, setLocale } = useI18n();
  const isPt = locale === "pt";
  const liveLayout = useNvrStore((s) => s.liveLayout);
  const setLiveLayout = useNvrStore((s) => s.setLiveLayout);
  const hudPosition = useNvrStore((s) => s.hudPosition);
  const setHudPosition = useNvrStore((s) => s.setHudPosition);
  const useMainStream = useNvrStore((s) => s.useMainStream);
  const setUseMainStream = useNvrStore((s) => s.setUseMainStream);
  const showVectors = useNvrStore((s) => s.showVectors);
  const setShowVectors = useNvrStore((s) => s.setShowVectors);
  const showAlertRibbon = useNvrStore((s) => s.showAlertRibbon);
  const setShowAlertRibbon = useNvrStore((s) => s.setShowAlertRibbon);
  const audibleAlertChime = useNvrStore((s) => s.audibleAlertChime);
  const setAudibleAlertChime = useNvrStore((s) => s.setAudibleAlertChime);
  const browserNotifications = useNvrStore((s) => s.browserNotifications);
  const setBrowserNotifications = useNvrStore((s) => s.setBrowserNotifications);
  const playbackSpeed = useNvrStore((s) => s.playbackSpeed);
  const setPlaybackSpeed = useNvrStore((s) => s.setPlaybackSpeed);
  const firstWeekday = useNvrStore((s) => s.firstWeekday);
  const setFirstWeekday = useNvrStore((s) => s.setFirstWeekday);
  const themeMode = useNvrStore((s) => s.themeMode);
  const setThemeMode = useNvrStore((s) => s.setThemeMode);
  const highContrast = useNvrStore((s) => s.highContrast);
  const setHighContrast = useNvrStore((s) => s.setHighContrast);
  const simulatedFleetCount = useNvrStore((s) => s.simulatedFleetCount);
  const setSimulatedFleetCount = useNvrStore((s) => s.setSimulatedFleetCount);
  const realCameraCount = useNvrStore(
    (s) => Object.keys(s.config?.cameras || {}).length || 4
  );
  const resetUiPreferences = useNvrStore((s) => s.resetUiPreferences);

  return (
    <div className="nvr-card-grid">
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt ? "MATRIZ AO VIVO & PADRÕES DE STREAM" : "LIVE MATRIX & STREAM DEFAULTS"}
          </span>
          <span className="nvr-badge live">
            {isPt ? "SALVO NO NAVEGADOR" : "BROWSER LOCAL"}
          </span>
        </div>
        <div className="nvr-cfg-card-body">
          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Layout Padrão da Matriz Ao Vivo" : "Default Live Matrix Layout"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Arranjo inicial do video-wall ao abrir a aba Matriz Ao Vivo."
                  : "Initial video-wall arrangement when opening the Live Matrix tab."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              aria-label="Default Live Matrix Layout"
              value={liveLayout}
              onChange={(e) => {
                setLiveLayout(e.target.value as LiveLayoutMode);
                notify(
                  isPt
                    ? "Layout padrão da matriz atualizado"
                    : "Updated default live matrix layout"
                );
              }}
            >
              <option value="2x2">{isPt ? "Grade Tática 2x2" : "2x2 Tactical Grid"}</option>
              <option value="focus">{isPt ? "Foco + Timeline" : "Focus + Timeline"}</option>
              <option value="single">
                {isPt ? "Monitor Único Tela Cheia" : "Single Full Monitor"}
              </option>
            </select>
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Posição dos Badges HUD na Câmera" : "Camera HUD Telemetry Position"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Canto onde os indicadores aparecem para nunca cobrir o relógio OSD da câmera."
                  : "Corner placement for live badges so camera OSD timestamps are never obscured."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              aria-label="Camera HUD Telemetry Position"
              value={hudPosition}
              onChange={(e) => {
                setHudPosition(e.target.value as HudPosition);
                notify(
                  isPt ? "Posição do HUD atualizada" : "Updated camera HUD position"
                );
              }}
            >
              <option value="bottom-right">
                {isPt ? "Inferior Direito" : "Bottom-Right"}
              </option>
              <option value="top-right">{isPt ? "Superior Direito" : "Top-Right"}</option>
              <option value="top-left">{isPt ? "Superior Esquerdo" : "Top-Left"}</option>
              <option value="bottom-left">
                {isPt ? "Inferior Esquerdo" : "Bottom-Left"}
              </option>
              <option value="hidden">{isPt ? "Oculto" : "Hidden"}</option>
            </select>
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Preferir Stream Principal 1080p Ao Vivo"
                  : "Prefer 1080p Main Stream on Live"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Reproduzir stream de alta resolução em vez do substream 720p por padrão."
                  : "Stream full-resolution main feed instead of 720p substream by default."}
              </div>
            </div>
            <ToggleSwitch
              checked={useMainStream}
              onChange={(v) => {
                setUseMainStream(v);
                notify(
                  v
                    ? isPt
                      ? "Stream 1080p Principal ativado por padrão"
                      : "1080p Main Stream enabled by default"
                    : isPt
                    ? "Substream 720p ativado por padrão"
                    : "720p Substream enabled by default"
                );
              }}
              label="Prefer 1080p Main Stream"
            />
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Exibir Zonas e Máscaras Vetoriais Ao Vivo"
                  : "Overlay Vector Zones & Masks on Live"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Desenhar polígonos de zonas e máscaras sobre os vídeos ao vivo."
                  : "Render polygon boundaries over live camera tiles by default."}
              </div>
            </div>
            <ToggleSwitch
              checked={showVectors}
              onChange={(v) => {
                setShowVectors(v);
                notify(
                  v
                    ? isPt
                      ? "Sobreposição de vetores ativada"
                      : "Vector overlay enabled"
                    : isPt
                    ? "Sobreposição de vetores ocultada"
                    : "Vector overlay hidden"
                );
              }}
              label="Overlay Vector Zones on Live"
            />
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Fita de Alertas Pendentes" : "Unreviewed Alert Ribbon"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Exibir a faixa superior de alertas com preview MP4 ao passar o mouse."
                  : "Display the hover-to-preview alert strip above the Live Matrix."}
              </div>
            </div>
            <ToggleSwitch
              checked={showAlertRibbon}
              onChange={(v) => {
                setShowAlertRibbon(v);
                notify(
                  v
                    ? isPt
                      ? "Fita de alertas ativada"
                      : "Alert ribbon enabled"
                    : isPt
                    ? "Fita de alertas ocultada"
                    : "Alert ribbon hidden"
                );
              }}
              label="Unreviewed Alert Ribbon"
            />
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Sinal Sonoro Tático de Alerta" : "Audible Tactical Alert Chime"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Emitir sinal sonoro sintetizado (Web Audio API) ao receber novo alerta."
                  : "Emit a synthesized two-tone Web Audio API chime when a new alert arrives."}
              </div>
            </div>
            <ToggleSwitch
              checked={audibleAlertChime}
              onChange={(v) => {
                setAudibleAlertChime(v);
                notify(v ? "Audible alert chime enabled" : "Audible alert chime muted");
              }}
              label="Audible Tactical Alert Chime"
            />
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Notificações Web do Navegador" : "Browser Web Notifications"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Exibir notificação nativa do sistema com miniatura da câmera e testar entrega via WebSocket/MQTT."
                  : "Show desktop browser notifications with camera snapshot and test WebSocket/MQTT alert delivery."}
              </div>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <button
                type="button"
                className="nvr-btn mono sm"
                onClick={async () => {
                  try {
                    const res = await fetch("/api/notifications/test", {
                      method: "POST",
                      credentials: "include",
                    });
                    const data = await res.json().catch(() => ({}));
                    if (res.ok && data.success) {
                      notify(`Sent test alert notification (${data.camera})`);
                    } else {
                      notify("Failed to send test alert notification");
                    }
                  } catch {
                    notify("Failed to send test alert notification");
                  }
                }}
              >
                Send Test Notification
              </button>
              <ToggleSwitch
                checked={browserNotifications}
                onChange={async (v) => {
                  if (v && "Notification" in window && Notification.permission !== "granted") {
                    try {
                      const perm = await Notification.requestPermission();
                      if (perm !== "granted") {
                        notify("Browser notification permission was not granted");
                        return;
                      }
                    } catch {
                      notify("Browser notification permission was not granted");
                      return;
                    }
                  }
                  setBrowserNotifications(v);
                  notify(v ? "Browser notifications enabled" : "Browser notifications disabled");
                }}
                label="Browser Web Notifications"
              />
            </div>
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Simulador de Frota de Câmeras (12 / 36 / 100 Canais)"
                  : "Simulate Fleet (12 / 36 / 100 Cams)"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Clona as câmeras reais na memória do navegador (com grupos Interno, Perímetro, Andar 1, Estacionamento) sem alterar o config.yml."
                  : "Clones configured physical cameras in browser memory (with Interno, Perímetro, Andar 1, Estacionamento groups) without modifying config.yml."}
              </div>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
              <button
                type="button"
                className={`nvr-btn mono ${simulatedFleetCount === 0 ? "active" : ""} sm`}
                onClick={() => {
                  if (simulatedFleetCount !== 0) {
                    setSimulatedFleetCount(0);
                    notify(`Restored ${realCameraCount} physical cameras`);
                  }
                }}
              >
                {isPt ? "Desativado (Padrão)" : "Off (Default)"}
              </button>
              {([12, 36, 100] as const).map((cnt) => (
                <button
                  key={cnt}
                  type="button"
                  className={`nvr-btn mono ${simulatedFleetCount === cnt ? "active" : ""} sm`}
                  onClick={() => {
                    const next = simulatedFleetCount === cnt ? 0 : cnt;
                    setSimulatedFleetCount(next);
                    notify(
                      next > 0
                        ? `Simulated ${next}-camera fleet activated`
                        : `Restored ${realCameraCount} physical cameras`
                    );
                  }}
                >
                  {cnt} Cams
                </button>
              ))}
              {simulatedFleetCount > 0 && (
                <>
                  <button
                    type="button"
                    className="nvr-btn primary mono sm"
                    onClick={() => {
                      useNvrStore.getState().setActiveTab("live");
                      if (typeof window !== "undefined") {
                        window.history.replaceState(null, "", "/");
                      }
                    }}
                  >
                    {isPt
                      ? `Abrir Matriz Ao Vivo (${simulatedFleetCount} Cams) →`
                      : `Open Live Matrix (${simulatedFleetCount} Cams) →`}
                  </button>
                  <button
                    type="button"
                    className="nvr-btn danger mono sm"
                    onClick={() => {
                      setSimulatedFleetCount(0);
                      notify(`Restored ${realCameraCount} physical cameras`);
                    }}
                  >
                    Exit Simulation (Restore {realCameraCount} Cameras)
                  </button>
                </>
              )}
            </div>
          </div>
        </div>
      </div>

      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt ? "REPRODUÇÃO, CALENDÁRIO & IDIOMA" : "PLAYBACK, CALENDAR & REGIONAL"}
          </span>
          <button
            type="button"
            className="nvr-btn sm"
            onClick={() => {
              resetUiPreferences();
              setLocale("en");
              notify(
                isPt
                  ? "Preferências de interface restauradas para o padrão"
                  : "Console UI preferences restored to factory defaults"
              );
            }}
          >
            <RotateCcw size={11} />
            {isPt ? "Restaurar Padrões" : "Reset Defaults"}
          </button>
        </div>
        <div className="nvr-cfg-card-body">
          <div className="nvr-subcard">
            <div className="nvr-subcard-header">
              <span>
                {isPt
                  ? "TEMA DO CONSOLE & CONTRASTE DE MONITOR"
                  : "CONSOLE THEME & MONITOR CONTRAST"}
              </span>
              <span className="nvr-badge live">WCAG AA / 16-235 SAFE</span>
            </div>

            <div className="nvr-field-row">
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt ? "Modo de Tema do Console" : "Console Theme Mode"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Escolha entre Tático Escuro, Luz do Dia / Claro (seguro contra estourar branco RGB >= 235) ou Automático do Sistema."
                    : "Switch between Tactical Dark, Daylight / Light (anchored below RGB 235 white-crush ceiling), or System Auto."}
                </div>
              </div>
              <select
                className="nvr-select mono"
                aria-label="Console Theme Mode"
                data-testid="select-theme-mode"
                value={themeMode}
                onChange={(e) => {
                  const nextTheme = e.target.value as ThemeMode;
                  setThemeMode(nextTheme);
                  notify(
                    isPt
                      ? `Tema do console alterado para ${nextTheme.toUpperCase()}`
                      : `Console theme switched to ${nextTheme.toUpperCase()}`
                  );
                }}
              >
                <option value="dark">
                  {isPt ? "Tático Escuro" : "Tactical Dark"}
                </option>
                <option value="light">
                  {isPt ? "Luz do Dia / Claro" : "Daylight / Light"}
                </option>
                <option value="system">
                  {isPt ? "Automático do Sistema" : "System Auto"}
                </option>
              </select>
            </div>

            <div className="nvr-field-row">
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? "Reforço de Alto Contraste para Monitor"
                    : "High-Contrast Monitor Boost"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Reforça bordas estruturais em +15%, promove texto secundário para contraste AAA (>= 7:1) e aplica bordas de 1.5px."
                    : "Strengthens structural borders (+15%), promotes muted text to >= 7:1 AAA contrast, and applies 1.5px border weight."}
                </div>
              </div>
              <ToggleSwitch
                checked={highContrast}
                onChange={(v) => {
                  setHighContrast(v);
                  notify(
                    v
                      ? isPt
                        ? "Reforço de alto contraste ativado"
                        : "High-Contrast Monitor Boost enabled"
                      : isPt
                      ? "Reforço de alto contraste desativado"
                      : "High-Contrast Monitor Boost disabled"
                  );
                }}
                label="High-Contrast Monitor Boost"
              />
            </div>
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Idioma do Console" : "Console Language"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Idioma da interface do operador para navegação, menus e configurações."
                  : "Operator interface language for navigation, labels, and status indicators."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              aria-label="Console Language"
              value={locale}
              onChange={(e) => {
                const nextLoc = e.target.value as SupportedLocale;
                setLocale(nextLoc);
                notify(
                  nextLoc === "pt"
                    ? "Idioma alterado para Português (PT-BR)"
                    : "Language switched to English (EN)"
                );
              }}
            >
              <option value="en">English (EN)</option>
              <option value="pt">Português (PT-BR)</option>
            </select>
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Velocidade Padrão de Reprodução VOD" : "Default VOD Playback Speed"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Taxa inicial de reprodução ao revisar gravações e incidentes."
                  : "Initial playback rate when reviewing recorded segments and incidents."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              aria-label="Default VOD Playback Speed"
              value={String(playbackSpeed)}
              onChange={(e) => {
                const spd = Number(e.target.value);
                setPlaybackSpeed(spd);
                notify(
                  isPt
                    ? `Velocidade padrão definida para ${spd}x`
                    : `Default playback speed set to ${spd}x`
                );
              }}
            >
              <option value="0.5">{isPt ? "0.5x Lento" : "0.5x Slow"}</option>
              <option value="1">{isPt ? "1.0x Normal" : "1.0x Normal"}</option>
              <option value="2">{isPt ? "2.0x Rápido" : "2.0x Fast"}</option>
              <option value="4">{isPt ? "4.0x Busca Rápida" : "4.0x Scrub"}</option>
            </select>
          </div>

          <div className="nvr-field-row">
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Primeiro Dia da Semana no Calendário" : "Calendar First Day of Week"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Coluna inicial do calendário de gravações e triagem."
                  : "Starting column in the Recording & Triage Calendar popover."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              aria-label="Calendar First Day of Week"
              value={firstWeekday}
              onChange={(e) => {
                const day = e.target.value as FirstWeekday;
                setFirstWeekday(day);
                notify(
                  isPt
                    ? "Primeiro dia do calendário atualizado"
                    : "Updated calendar first weekday"
                );
              }}
            >
              <option value="sunday">
                {isPt ? "Domingo (Dom-Sáb)" : "Sunday (Su-Sa)"}
              </option>
              <option value="monday">
                {isPt ? "Segunda-feira (Seg-Dom)" : "Monday (Mo-Su)"}
              </option>
            </select>
          </div>
        </div>
      </div>
    </div>
  );
};

export const ChannelsStreamsSection: React.FC<{
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  inspectorCam: string;
  setInspectorCam: (cam: string) => void;
  ffprobeInfo: FfprobeItem[] | null;
  notify: (msg: string) => void;
}> = ({
  draftConfig,
  updateDraft,
  inspectorCam,
  setInspectorCam,
  ffprobeInfo,
  notify,
}) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const [newCamName, setNewCamName] = useState("");
  const [newCamMainUrl, setNewCamMainUrl] = useState("");
  const [newCamSubUrl, setNewCamSubUrl] = useState("");
  const [probeBusy, setProbeBusy] = useState(false);
  const [probeResult, setProbeResult] = useState<{
    ok: boolean;
    latency_ms: number;
    streams: Array<{
      codec_type?: string;
      codec_name?: string;
      codec_long_name?: string;
      width?: number;
      height?: number;
      avg_frame_rate?: string;
    }>;
    error?: string | null;
  } | null>(null);
  const probeSeqRef = React.useRef(0);
  const probeAbortRef = React.useRef<AbortController | null>(null);
  const inspectorCamRef = React.useRef(inspectorCam);
  inspectorCamRef.current = inspectorCam;

  React.useEffect(() => {
    inspectorCamRef.current = inspectorCam;
    probeSeqRef.current += 1;
    if (probeAbortRef.current) {
      probeAbortRef.current.abort();
      probeAbortRef.current = null;
    }
    setProbeBusy(false);
    setProbeResult(null);
  }, [inspectorCam]);

  React.useEffect(() => {
    return () => {
      probeSeqRef.current += 1;
      if (probeAbortRef.current) {
        probeAbortRef.current.abort();
        probeAbortRef.current = null;
      }
    };
  }, []);

  const handleRunStreamProbe = async (customUrl?: string) => {
    const targetCam = inspectorCam;
    if (!targetCam) return;
    if (probeAbortRef.current) {
      probeAbortRef.current.abort();
    }
    const controller = new AbortController();
    probeAbortRef.current = controller;
    const seq = ++probeSeqRef.current;
    setProbeBusy(true);
    try {
      const res = await fetch("/api/streams/probe", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        signal: controller.signal,
        body: JSON.stringify({
          camera: targetCam,
          url: customUrl || undefined,
        }),
      });
      const data = await res.json().catch(() => null);
      if (
        seq !== probeSeqRef.current ||
        inspectorCamRef.current !== targetCam
      ) {
        return;
      }
      if (data && typeof data === "object") {
        setProbeResult(data);
        if (data.ok) {
          notify(`Stream probe succeeded (${data.latency_ms}ms)`);
        } else {
          notify(data.error || "Stream probe failed");
        }
      } else {
        notify(`Stream probe failed (${res.status})`);
      }
    } catch (err) {
      if ((err as { name?: string })?.name !== "AbortError") {
        notify("Failed to reach stream probe endpoint");
      }
    } finally {
      if (seq === probeSeqRef.current) {
        setProbeBusy(false);
      }
    }
  };
  const [newStreamName, setNewStreamName] = useState("");
  const [newStreamUrl, setNewStreamUrl] = useState("");
  const [showAllGo2rtc, setShowAllGo2rtc] = useState(false);

  const cameras = draftConfig.cameras || {};
  const camKeys = Object.keys(cameras);
  const activeCamCfg: CameraConfig | undefined = cameras[inspectorCam];
  const go2rtcStreams = draftConfig.go2rtc?.streams || {};

  const filteredGo2rtcEntries = Object.entries(go2rtcStreams).filter(([sName]) =>
    showAllGo2rtc
      ? true
      : sName === inspectorCam || sName.startsWith(`${inspectorCam}_`)
  );

  const handleToggleInputRole = (inputIdx: number, role: "detect" | "record") => {
    updateDraft((next) => {
      const inputs = next.cameras[inspectorCam]?.ffmpeg?.inputs;
      if (!inputs || !inputs[inputIdx]) return;
      const currentRoles = inputs[inputIdx].roles || [];
      if (currentRoles.includes(role)) {
        // Prevent removing the last remaining role on an input if no other input has it
        if (currentRoles.length <= 1 && inputs.length === 1) {
          return;
        }
        if (currentRoles.length > 1) {
          inputs[inputIdx].roles = currentRoles.filter((r) => r !== role);
        } else if (inputs.length > 1) {
          // Swap with the otherrole so input is never empty
          const altRole = role === "detect" ? "record" : "detect";
          inputs[inputIdx].roles = [altRole];
        }
      } else {
        // Remove this role from other inputs on the same camera so each role has a unique source
        inputs.forEach((inp, idx) => {
          if (idx !== inputIdx && inp.roles.includes(role) && inp.roles.length > 1) {
            inp.roles = inp.roles.filter((r) => r !== role);
          }
        });
        inputs[inputIdx].roles = [...currentRoles, role];
      }
    });
  };

  const handleAddCamera = () => {
    const slug = sanitizeConfigEntrySlug(newCamName);
    if (!slug) {
      notify(
        isPt
          ? "Informe um ID válido para a câmera"
          : "Enter a valid camera ID (letters, numbers, _ or -)"
      );
      return;
    }
    if (cameras[slug]) {
      notify(`Camera '${slug}' already exists`);
      return;
    }
    const mainSrc = newCamMainUrl.trim() || `rtsp://127.0.0.1:8554/${slug}`;
    const subSrc = newCamSubUrl.trim() || `rtsp://127.0.0.1:8554/${slug}_sub`;

    updateDraft((next) => {
      if (!next.go2rtc) next.go2rtc = { streams: {} };
      if (!next.go2rtc.streams[slug]) next.go2rtc.streams[slug] = [mainSrc];
      if (!next.go2rtc.streams[`${slug}_sub`]) {
        next.go2rtc.streams[`${slug}_sub`] = [subSrc];
      }
      next.cameras[slug] = {
        name: slug,
        enabled: true,
        ffmpeg: {
          inputs: [
            { path: `rtsp://127.0.0.1:8554/${slug}_sub`, roles: ["detect"] },
            { path: `rtsp://127.0.0.1:8554/${slug}`, roles: ["record"] },
          ],
        },
        live: {
          streams: {
            "Stream 1": `${slug}_sub`,
            "Stream 2": slug,
          },
          height: 720,
          quality: 8,
        },
        detect: {
          enabled: true,
          width: 1280,
          height: 720,
          fps: 5,
          min_initialized: 2,
          max_disappeared: 25,
          stationary: { interval: 50, threshold: 50, classifier: true },
        },
        motion: {
          enabled: true,
          threshold: 30,
          lightning_threshold: 0.8,
          improve_contrast: true,
          contour_area: 10,
          delta_alpha: 0.2,
          frame_alpha: 0.01,
          frame_height: 100,
          mask: {},
        },
        objects: {
          track: next.objects?.track ? [...next.objects.track] : ["person"],
          filters: {},
          mask: {},
        },
        record: {
          enabled: true,
          expire_interval: 60,
          continuous: { days: 14 },
          motion: { days: 0 },
          alerts: {
            pre_capture: 5,
            post_capture: 5,
            retain: { days: 14, mode: "motion" },
          },
          detections: {
            pre_capture: 5,
            post_capture: 5,
            retain: { days: 14, mode: "motion" },
          },
          preview: { quality: "medium" },
        },
        snapshots: {
          enabled: true,
          timestamp: false,
          bounding_box: true,
          crop: false,
          quality: 70,
        },
        review: {
          alerts: {
            enabled: true,
            labels: ["person", "car"],
            required_zones: [],
            cutoff_time: 40,
          },
          detections: {
            enabled: true,
            required_zones: [],
            cutoff_time: 30,
          },
        },
        zones: {},
      };
    });
    setInspectorCam(slug);
    setNewCamName("");
    setNewCamMainUrl("");
    setNewCamSubUrl("");
    notify(`Staged new camera '${slug}' — click Save Config to persist`);
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 14 }}>
      {/* Camera Matrix State Table */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header" style={{ flexWrap: "wrap" }}>
          <span style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            {isPt
              ? `CANAIS DE CÂMERA & FUNÇÕES DE PIPELINE (${camKeys.length})`
              : `CAMERA CHANNELS & PIPELINE ROLES (${camKeys.length})`}
            <span className="nvr-yaml-key-tag">cameras</span>
          </span>
        </div>
        <div style={{ overflowX: "auto" }}>
          <table className="nvr-table mono">
            <thead>
              <tr>
                <th>{isPt ? "Canal" : "Channel"}</th>
                <th>{isPt ? "Ativa" : "Enabled"}</th>
                <th>{isPt ? "Detecção IA" : "AI Detect"}</th>
                <th>{isPt ? "Gravar MP4" : "Record MP4"}</th>
                <th>Snapshots</th>
                <th>{isPt ? "Alertas" : "Review Alerts"}</th>
                <th>{isPt ? "Resolução / FPS" : "Detect Dim / FPS"}</th>
                <th>Inputs</th>
                <th>{isPt ? "Ações" : "Actions"}</th>
              </tr>
            </thead>
            <tbody>
              {camKeys.map((camKey) => {
                const c = cameras[camKey];
                const isSel = camKey === inspectorCam;
                return (
                  <tr
                    key={camKey}
                    onClick={() => setInspectorCam(camKey)}
                    style={{
                      cursor: "pointer",
                      background: isSel ? "var(--bg-active)" : undefined,
                    }}
                  >
                    <td style={{ fontWeight: 700, textTransform: "uppercase" }}>
                      <span
                        className={`nvr-dot ${c.enabled ? "live" : "alert"}`}
                        style={{ marginRight: 7 }}
                      />
                      {camKey}
                    </td>
                    <td onClick={(e) => e.stopPropagation()}>
                      <ToggleSwitch
                        checked={c.enabled}
                        label={`Enable ${camKey}`}
                        onChange={(v) =>
                          updateDraft((next) => {
                            next.cameras[camKey].enabled = v;
                          })
                        }
                      />
                    </td>
                    <td onClick={(e) => e.stopPropagation()}>
                      <ToggleSwitch
                        checked={c.detect?.enabled ?? true}
                        label={`Enable detection on ${camKey}`}
                        onChange={(v) =>
                          updateDraft((next) => {
                            next.cameras[camKey].detect.enabled = v;
                          })
                        }
                      />
                    </td>
                    <td onClick={(e) => e.stopPropagation()}>
                      <ToggleSwitch
                        checked={c.record?.enabled ?? true}
                        label={`Enable recording on ${camKey}`}
                        onChange={(v) =>
                          updateDraft((next) => {
                            next.cameras[camKey].record.enabled = v;
                          })
                        }
                      />
                    </td>
                    <td onClick={(e) => e.stopPropagation()}>
                      <ToggleSwitch
                        checked={c.snapshots?.enabled ?? true}
                        label={`Enable snapshots on ${camKey}`}
                        onChange={(v) =>
                          updateDraft((next) => {
                            next.cameras[camKey].snapshots.enabled = v;
                          })
                        }
                      />
                    </td>
                    <td onClick={(e) => e.stopPropagation()}>
                      <ToggleSwitch
                        checked={c.review?.alerts?.enabled ?? true}
                        label={`Enable alerts on ${camKey}`}
                        onChange={(v) =>
                          updateDraft((next) => {
                            if (!next.cameras[camKey].review) {
                              next.cameras[camKey].review = {
                                alerts: {
                                  enabled: v,
                                  labels: ["person", "car"],
                                  required_zones: [],
                                  cutoff_time: 40,
                                },
                                detections: {
                                  enabled: true,
                                  required_zones: [],
                                  cutoff_time: 30,
                                },
                              };
                            } else {
                              next.cameras[camKey].review!.alerts.enabled = v;
                            }
                          })
                        }
                      />
                    </td>
                    <td>
                      {c.detect?.width || 1280}×{c.detect?.height || 720} @{" "}
                      {c.detect?.fps || 5}fps
                    </td>
                    <td>{c.ffmpeg?.inputs?.length || 0} stream(s)</td>
                    <td onClick={(e) => e.stopPropagation()}>
                      <button
                        type="button"
                        className="nvr-btn danger sm"
                        disabled={camKeys.length <= 1}
                        title="Remove camera from configuration"
                        onClick={() => {
                          updateDraft((next) => {
                            delete next.cameras[camKey];
                          });
                          const remaining = camKeys.filter((k) => k !== camKey);
                          if (inspectorCam === camKey && remaining.length > 0) {
                            setInspectorCam(remaining[0]);
                          }
                          notify(`Removed camera '${camKey}' from draft`);
                        }}
                      >
                        <Trash2 size={11} />
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>

      <div className="nvr-card-grid">
        <SettingsScopeDivider
          group="camera"
          scope={inspectorCam}
          inspectorCam={inspectorCam}
          cameraCount={camKeys.length}
          isPt={isPt}
        />
        {/* Selected Camera FFmpeg Inputs & Live Stream Mapping */}
        {activeCamCfg && (
          <div className="nvr-cfg-card">
            <div className="nvr-cfg-card-header">
              <span>
                {inspectorCam.toUpperCase()} //{" "}
                {isPt ? "STREAMS RTSP & FUNÇÕES" : "RTSP INPUTS & ROLES"}
              </span>
              <button
                type="button"
                className="nvr-btn primary sm"
                onClick={() =>
                  updateDraft((next) => {
                    const cam = next.cameras[inspectorCam];
                    if (!cam.ffmpeg) cam.ffmpeg = { inputs: [] };
                    cam.ffmpeg.inputs.push({
                      path: `rtsp://127.0.0.1:8554/${inspectorCam}`,
                      roles: ["record"],
                    });
                  })
                }
              >
                <Plus size={11} />
                {isPt ? "Adicionar Stream" : "Add Input"}
              </button>
            </div>
            <div className="nvr-cfg-card-body">
              {(activeCamCfg.ffmpeg?.inputs || []).map((inp, idx) => (
                <div key={idx} className="nvr-subcard">
                  <div className="nvr-subcard-header">
                    <span
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        color: "var(--signal-accent-text)",
                      }}
                    >
                      STREAM INPUT #{idx + 1} (
                      {inp.roles.includes("detect")
                        ? isPt
                          ? "Substream de Detecção"
                          : "Detection Substream"
                        : isPt
                        ? "Stream Principal de Gravação"
                        : "Main Recording Stream"}
                      )
                    </span>
                    <button
                      type="button"
                      className="nvr-btn danger sm"
                      disabled={(activeCamCfg.ffmpeg?.inputs?.length || 0) <= 1}
                      onClick={() =>
                        updateDraft((next) => {
                          next.cameras[inspectorCam].ffmpeg?.inputs.splice(idx, 1);
                        })
                      }
                    >
                      <Trash2 size={10} />
                    </button>
                  </div>
                  <input
                    className="nvr-input mono"
                    style={{ width: "100%" }}
                    aria-label={`Stream ${idx + 1} RTSP Path`}
                    value={inp.path}
                    placeholder="rtsp://127.0.0.1:8554/camera_stream"
                    onChange={(e) => {
                      const val = e.target.value;
                      updateDraft((next) => {
                        next.cameras[inspectorCam].ffmpeg!.inputs[idx].path = val;
                      });
                    }}
                  />
                  <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                    <span
                      className="mono"
                      style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                    >
                      {isPt ? "FUNÇÃO:" : "ROLES:"}
                    </span>
                    {(["detect", "record"] as const).map((role) => {
                      const hasRole = inp.roles.includes(role);
                      return (
                        <button
                          key={role}
                          type="button"
                          className={`nvr-btn mono ${hasRole ? "active" : ""} sm`}
                          onClick={() => handleToggleInputRole(idx, role)}
                        >
                          {role.toUpperCase()}
                        </button>
                      );
                    })}
                  </div>
                </div>
              ))}

              {/* Video Orientation / Corridor Mode Rotation Selector */}
              <div
                className="nvr-subcard"
                style={{
                  flexDirection: "row",
                  alignItems: "center",
                  justifyContent: "space-between",
                  padding: "6px 8px",
                }}
              >
                <span className="mono" style={{ fontSize: "var(--text-xs)" }}>
                  {isPt
                    ? "ROTAÇÃO DE VÍDEO (MODO CORREDOR):"
                    : "VIDEO ROTATION (CORRIDOR MODE):"}
                </span>
                <div style={{ display: "flex", gap: 4 }}>
                  {([0, 90, 180, 270] as const).map((deg) => {
                    const curRot = activeCamCfg.live?.rotate || 0;
                    return (
                      <button
                        key={deg}
                        type="button"
                        className={`nvr-btn mono ${curRot === deg ? "active" : ""} sm`}
                        onClick={() =>
                          updateDraft((next) => {
                            const cam = next.cameras[inspectorCam];
                            if (!cam) return;
                            if (!cam.live) {
                              cam.live = { streams: {}, height: 720, quality: 8, rotate: deg };
                            } else {
                              cam.live.rotate = deg;
                            }
                          })
                        }
                      >
                        {deg}°
                      </button>
                    );
                  })}
                </div>
              </div>

              {/* ONVIF PTZ & Resilient Auto-Tracking Configuration */}
              <div className="nvr-subcard">
                <div className="nvr-subcard-header">
                  <span
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      fontWeight: 700,
                      color: "var(--signal-accent-text)",
                    }}
                  >
                    {isPt
                      ? "ONVIF PTZ & AUTO-TRACKING RESILIENTE"
                      : "ONVIF PTZ & RESILIENT AUTO-TRACKING"}
                  </span>
                  <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                    <span
                      className="mono"
                      style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                    >
                      {isPt ? "Auto-Tracking:" : "Auto-Tracking:"}
                    </span>
                    <ToggleSwitch
                      checked={Boolean(activeCamCfg.onvif?.autotracking)}
                      label={`Enable ONVIF Auto-Tracking on ${inspectorCam}`}
                      onChange={(v) =>
                        updateDraft((next) => {
                          const cam = next.cameras[inspectorCam];
                          if (!cam) return;
                          cam.onvif = {
                            port: 80,
                            timeout_sec: 5,
                            ...(cam.onvif || {}),
                            autotracking: v,
                          };
                        })
                      }
                    />
                  </div>
                </div>

                <div
                  style={{
                    display: "grid",
                    gridTemplateColumns: "1.5fr 0.7fr 0.8fr",
                    gap: 6,
                  }}
                >
                  <input
                    className="nvr-input mono sm"
                    placeholder="ONVIF Host / IP (e.g. 192.0.2.50)"
                    aria-label="ONVIF Host"
                    value={activeCamCfg.onvif?.host || ""}
                    onChange={(e) => {
                      const val = e.target.value;
                      updateDraft((next) => {
                        const cam = next.cameras[inspectorCam];
                        if (!cam) return;
                        cam.onvif = {
                          port: 80,
                          timeout_sec: 5,
                          ...(cam.onvif || {}),
                          host: val,
                        };
                      });
                    }}
                  />
                  <input
                    type="number"
                    min={1}
                    max={65535}
                    className="nvr-input mono sm"
                    placeholder="Port (80)"
                    aria-label="ONVIF Port"
                    value={activeCamCfg.onvif?.port ?? 80}
                    onChange={(e) => {
                      const port = Math.max(1, Number(e.target.value) || 80);
                      updateDraft((next) => {
                        const cam = next.cameras[inspectorCam];
                        if (!cam) return;
                        cam.onvif = {
                          timeout_sec: 5,
                          ...(cam.onvif || {}),
                          port,
                        };
                      });
                    }}
                  />
                  <input
                    type="number"
                    min={1}
                    max={30}
                    className="nvr-input mono sm"
                    placeholder="Timeout (s)"
                    aria-label="ONVIF Timeout Seconds"
                    value={activeCamCfg.onvif?.timeout_sec ?? 5}
                    onChange={(e) => {
                      const timeout_sec = Math.max(1, Number(e.target.value) || 5);
                      updateDraft((next) => {
                        const cam = next.cameras[inspectorCam];
                        if (!cam) return;
                        cam.onvif = {
                          port: 80,
                          ...(cam.onvif || {}),
                          timeout_sec,
                        };
                      });
                    }}
                  />
                </div>

                <div
                  style={{
                    display: "grid",
                    gridTemplateColumns: "1fr 1fr 1fr",
                    gap: 6,
                  }}
                >
                  <input
                    className="nvr-input mono sm"
                    placeholder="ONVIF Username"
                    aria-label="ONVIF Username"
                    value={activeCamCfg.onvif?.user || ""}
                    onChange={(e) => {
                      const val = e.target.value;
                      updateDraft((next) => {
                        const cam = next.cameras[inspectorCam];
                        if (!cam) return;
                        cam.onvif = {
                          port: 80,
                          timeout_sec: 5,
                          ...(cam.onvif || {}),
                          user: val,
                        };
                      });
                    }}
                  />
                  <div style={{ display: "flex", gap: 4 }}>
                    <input
                      type="password"
                      className="nvr-input mono sm"
                      style={{ flex: 1, minWidth: 0 }}
                      placeholder={
                        activeCamCfg.onvif?.has_password
                          ? "*** (password saved)"
                          : "ONVIF Password"
                      }
                      aria-label="ONVIF Password"
                      value={
                        activeCamCfg.onvif?.password === "***" ||
                        activeCamCfg.onvif?.password === "*"
                          ? ""
                          : activeCamCfg.onvif?.password || ""
                      }
                      onChange={(e) => {
                        const val = e.target.value;
                        updateDraft((next) => {
                          const cam = next.cameras[inspectorCam];
                          if (!cam) return;
                          const patch = buildOnvifPasswordPatch(
                            val,
                            cam.onvif?.has_password,
                            false
                          );
                          cam.onvif = {
                            port: 80,
                            timeout_sec: 5,
                            ...(cam.onvif || {}),
                            ...patch,
                          };
                        });
                      }}
                    />
                    {(activeCamCfg.onvif?.has_password ||
                      (activeCamCfg.onvif?.password &&
                        activeCamCfg.onvif.password !== "")) && (
                      <button
                        type="button"
                        className="nvr-btn danger mono sm"
                        title="Clear saved ONVIF password"
                        aria-label="Clear ONVIF Password"
                        onClick={() => {
                          updateDraft((next) => {
                            const cam = next.cameras[inspectorCam];
                            if (!cam) return;
                            const patch = buildOnvifPasswordPatch("", false, true);
                            cam.onvif = {
                              port: 80,
                              timeout_sec: 5,
                              ...(cam.onvif || {}),
                              ...patch,
                            };
                          });
                        }}
                      >
                        ×
                      </button>
                    )}
                  </div>
                  <input
                    className="nvr-input mono sm"
                    placeholder="Return Preset (e.g. home)"
                    aria-label="ONVIF Return Preset"
                    value={activeCamCfg.onvif?.return_preset || ""}
                    onChange={(e) => {
                      const val = e.target.value;
                      updateDraft((next) => {
                        const cam = next.cameras[inspectorCam];
                        if (!cam) return;
                        cam.onvif = {
                          port: 80,
                          timeout_sec: 5,
                          ...(cam.onvif || {}),
                          return_preset: val,
                        };
                      });
                    }}
                  />
                </div>
              </div>

              {/* Visual Stream Probe Summary Cards */}
              <div
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-live-text)",
                  marginTop: 4,
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                }}
              >
                <span>
                  {isPt
                    ? `SONDA DE STREAM ATIVA (${inspectorCam.toUpperCase()})`
                    : `ACTIVE STREAM PROBE (${inspectorCam.toUpperCase()})`}
                </span>
                <button
                  type="button"
                  className="nvr-btn mono sm"
                  disabled={probeBusy}
                  onClick={() =>
                    handleRunStreamProbe(activeCamCfg.ffmpeg?.inputs?.[0]?.path)
                  }
                >
                  {probeBusy
                    ? isPt
                      ? "Testando..."
                      : "Probing..."
                    : isPt
                    ? "Testar Stream"
                    : "Test Stream"}
                </button>
              </div>
              {probeResult && (
                <div
                  className="nvr-subcard mono"
                  style={{
                    padding: 8,
                    borderColor: probeResult.ok
                      ? "var(--signal-live)"
                      : "var(--signal-alert)",
                    fontSize: "var(--text-xs)",
                    gap: 4,
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      justifyContent: "space-between",
                      fontWeight: 700,
                      color: probeResult.ok
                        ? "var(--signal-live-text)"
                        : "var(--signal-alert-text)",
                    }}
                  >
                    <span>
                      LIVE PROBE // {probeResult.ok ? "REACHABLE" : "UNREACHABLE"}
                    </span>
                    <span>{probeResult.latency_ms}ms</span>
                  </div>
                  {probeResult.error ? (
                    <div style={{ color: "var(--signal-alert-text)" }}>
                      {probeResult.error}
                    </div>
                  ) : (
                    <div style={{ color: "var(--text-primary)" }}>
                      {(probeResult.streams || [])
                        .map((s) =>
                          s.width && s.height
                            ? `${(s.codec_name || s.codec_long_name || "H.264").toUpperCase()} ${
                                s.width
                              }×${s.height} @ ${s.avg_frame_rate || "N/A"}`
                            : `${(s.codec_name || s.codec_long_name || "AAC").toUpperCase()} AUDIO`
                        )
                        .join(" · ") || "Stream connected"}
                    </div>
                  )}
                </div>
              )}
              <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
                {(ffprobeInfo || []).map((probe, pIdx) => {
                  const streams = probe.stdout?.streams || [];
                  const vStream = streams.find((s) => s.width && s.height);
                  const aStream = streams.find((s) => !s.width && s.codec_long_name);
                  return (
                    <div
                      key={pIdx}
                      className="nvr-subcard mono"
                      style={{
                        padding: 8,
                        fontSize: "var(--text-xs)",
                        gap: 3,
                      }}
                    >
                      <div
                        style={{
                          display: "flex",
                          justifyContent: "space-between",
                          color: "var(--signal-live-text)",
                          fontWeight: 700,
                        }}
                      >
                        <span>{pIdx === 0 ? "SUBSTREAM (DETECT)" : "MAIN (RECORD)"}</span>
                        <span>{probe.return_code === 0 ? "OK" : "ERR"}</span>
                      </div>
                      <div style={{ color: "var(--text-primary)" }}>
                        {vStream
                          ? `${vStream.width}×${vStream.height} · ${
                              vStream.avg_frame_rate || "N/A"
                            } fps`
                          : "No video track"}
                      </div>
                      <div style={{ color: "var(--text-secondary)" }}>
                        {vStream?.codec_long_name?.split("/")[0]?.trim() || "H.264"}
                        {aStream
                          ? ` + ${aStream.codec_long_name?.split("(")[0]?.trim()}`
                          : " (No Audio)"}
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
          </div>
        )}

        {/* Camera Technical & Maintenance Notes */}
        {activeCamCfg && (
          <div className="nvr-cfg-card" style={{ alignSelf: "start" }}>
            <div className="nvr-cfg-card-header">
              <span style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                {isPt ? "NOTAS TÉCNICAS & MANUTENÇÃO" : "TECHNICAL & MAINTENANCE NOTES"}
                <span className="nvr-yaml-key-tag">cameras.{inspectorCam}.notes</span>
              </span>
            </div>
            <div className="nvr-cfg-card-body">
              <textarea
                className="nvr-input mono"
                rows={4}
                style={{ width: "100%", height: "auto", padding: 8, resize: "vertical" }}
                aria-label={isPt ? "Notas da câmera" : "Camera notes"}
                placeholder={
                  isPt
                    ? "Porta do switch PoE, distância focal, altura de instalação ou log de manutenção..."
                    : "PoE switch port, lens focal length, mounting height, or maintenance log..."
                }
                value={activeCamCfg.notes || ""}
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
        )}

        <SettingsScopeDivider
          group="global"
          scope="global"
          inspectorCam={inspectorCam}
          cameraCount={camKeys.length}
          isPt={isPt}
        />
        {/* Add New Camera + go2rtc Restream Registry */}
        <div className="nvr-cfg-card" style={{ alignSelf: "start" }}>
            <div className="nvr-cfg-card-header">
              <span>
                {isPt ? "ADICIONAR NOVO CANAL DE CÂMERA" : "ADD NEW CAMERA CHANNEL"}
              </span>
              <span className="nvr-badge">
                {isPt ? "PROVISIONAMENTO RÁPIDO" : "QUICK PROVISION"}
              </span>
            </div>
            <div className="nvr-cfg-card-body">
              <div style={{ display: "grid", gridTemplateColumns: "1fr 1.4fr", gap: 8 }}>
                <input
                  className="nvr-input mono"
                  placeholder={isPt ? "ID da câmera (ex: garagem)" : "Camera ID (e.g. garagem)"}
                  aria-label="New Camera ID"
                  value={newCamName}
                  onChange={(e) => setNewCamName(e.target.value)}
                />
                <input
                  className="nvr-input mono"
                  placeholder="Main RTSP URL (1080p record)"
                  aria-label="New Camera Main RTSP URL"
                  value={newCamMainUrl}
                  onChange={(e) => setNewCamMainUrl(e.target.value)}
                />
              </div>
              <div style={{ display: "flex", gap: 8 }}>
                <input
                  className="nvr-input mono"
                  style={{ flex: 1 }}
                  placeholder="Substream RTSP URL (720p detect)"
                  aria-label="New Camera Substream RTSP URL"
                  value={newCamSubUrl}
                  onChange={(e) => setNewCamSubUrl(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn primary"
                  onClick={handleAddCamera}
                >
                  <Plus size={12} />
                  {isPt ? "Adicionar Câmera" : "Stage Camera"}
                </button>
              </div>
            </div>
          </div>

          <div className="nvr-cfg-card" style={{ alignSelf: "start" }}>
            <div className="nvr-cfg-card-header">
              <span>
                GO2RTC RESTREAM REGISTRY ({filteredGo2rtcEntries.length}/
                {Object.keys(go2rtcStreams).length})
              </span>
              <button
                type="button"
                className={`nvr-btn mono ${showAllGo2rtc ? "active" : ""} sm`}
                onClick={() => setShowAllGo2rtc((v) => !v)}
              >
                <RefreshCw size={11} />
                {showAllGo2rtc
                  ? isPt
                    ? `Filtrar: ${inspectorCam.toUpperCase()}`
                    : `Filter: ${inspectorCam.toUpperCase()}`
                  : isPt
                  ? `Mostrar Todos (${Object.keys(go2rtcStreams).length})`
                  : `Show All (${Object.keys(go2rtcStreams).length})`}
              </button>
            </div>
            <div className="nvr-cfg-card-body">
              {filteredGo2rtcEntries.map(([sName, sVal]) => {
                const urlStr = Array.isArray(sVal)
                  ? sVal.map(String).join(", ")
                  : typeof sVal === "string"
                  ? sVal
                  : JSON.stringify(sVal);
                return (
                  <div
                    key={sName}
                    style={{
                      display: "flex",
                      alignItems: "center",
                      gap: 8,
                      padding: "4px 0",
                      borderBottom: "1px solid var(--border-divider)",
                    }}
                  >
                    <span
                      className="mono"
                      style={{
                        minWidth: 105,
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        color: "var(--signal-accent-text)",
                      }}
                    >
                      {sName}
                    </span>
                    <input
                      className="nvr-input mono sm"
                      style={{ flex: 1 }}
                      aria-label={`go2rtc stream ${sName}`}
                      value={urlStr}
                      onChange={(e) => {
                        const val = e.target.value;
                        updateDraft((next) => {
                          if (!next.go2rtc) next.go2rtc = { streams: {} };
                          next.go2rtc.streams[sName] = [val];
                        });
                      }}
                    />
                    <button
                      type="button"
                      className="nvr-btn danger sm"
                      onClick={() =>
                        updateDraft((next) => {
                          if (next.go2rtc?.streams) {
                            delete next.go2rtc.streams[sName];
                          }
                        })
                      }
                    >
                      <Trash2 size={11} />
                    </button>
                  </div>
                );
              })}
              <div style={{ display: "flex", gap: 6, marginTop: 4 }}>
                <input
                  className="nvr-input mono"
                  style={{ width: 120 }}
                  placeholder="stream_id"
                  aria-label="New go2rtc stream name"
                  value={newStreamName}
                  onChange={(e) => setNewStreamName(e.target.value)}
                />
                <input
                  className="nvr-input mono"
                  style={{ flex: 1 }}
                  placeholder="rtsp://user:pass@ip:554/stream"
                  aria-label="New go2rtc stream source URL"
                  value={newStreamUrl}
                  onChange={(e) => setNewStreamUrl(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn primary"
                  onClick={() => {
                    const k = sanitizeConfigEntrySlug(newStreamName);
                    const u = newStreamUrl.trim();
                    if (!k || !u) return;
                    updateDraft((next) => {
                      if (!next.go2rtc) next.go2rtc = { streams: {} };
                      next.go2rtc.streams[k] = [u];
                    });
                    setNewStreamName("");
                    setNewStreamUrl("");
                  }}
                >
                  <Plus size={12} />
                  Add
                </button>
              </div>
            </div>
          </div>
      </div>
    </div>
  );
};
