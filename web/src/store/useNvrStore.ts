import { create } from "zustand";
import {
  CameraConfig,
  CameraGroupConfig,
  CameraLiveActivity,
  NinjaConfigResponse,
  NinjaStatsResponse,
  StorageCleanupJobSnapshot,
} from "../types/nvr";
import {
  FirstWeekday,
  HudPosition,
  LiveLayoutMode,
  LiveStreamQualityMode,
  MatrixDensity,
  SpotlightFilter,
  SpotlightTimelineMode,
  ThemeMode,
  WorkspaceTab,
  applyThemeToDocument,
  resolveInitialBool,
  resolveInitialFirstWeekday,
  resolveInitialHudPosition,
  resolveInitialLiveLayout,
  resolveInitialMatrixDensity,
  resolveInitialPlaybackSpeed,
  resolveInitialSpotlightTimelineMode,
  resolveInitialTab,
  resolveInitialThemeMode,
  sanitizeLiveStreamQualityMode,
} from "../utils/format";

export type SimulatedFleetCount = 0 | 12 | 36 | 100;

export const MAX_STATS_HISTORY_SAMPLES = 120;
export const MAX_CAMERA_ACTIVITY_ENTRIES = 256;

function safeStorageGet(key: string): string | null {
  try {
    if (typeof window !== "undefined") {
      return window.localStorage?.getItem(key) ?? null;
    }
  } catch {
    // ignore storage access errors
  }
  return null;
}

function safeStorageSet(key: string, val: string): void {
  try {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem(key, val);
    }
  } catch {
    // ignore storage quota/access errors
  }
}

function safeStorageRemove(key: string): void {
  try {
    if (typeof window !== "undefined") {
      window.localStorage?.removeItem(key);
    }
  } catch {
    // ignore storage access errors
  }
}

export function stableSerializeConfigValue(val: unknown): string {
  if (val === null || val === undefined) return "null";
  if (typeof val !== "object") return JSON.stringify(val);
  if (Array.isArray(val)) {
    return `[${val.map((item) => stableSerializeConfigValue(item)).join(",")}]`;
  }
  const obj = val as Record<string, unknown>;
  const keys = Object.keys(obj).sort();
  return `{${keys
    .map((k) => `${JSON.stringify(k)}:${stableSerializeConfigValue(obj[k])}`)
    .join(",")}}`;
}

function sanitizeCameraActivityMap(
  raw: Record<string, CameraLiveActivity> | null | undefined
): Record<string, CameraLiveActivity> {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    return {};
  }
  const out: Record<string, CameraLiveActivity> = {};
  const entries = Object.entries(raw);
  const limit = Math.min(entries.length, MAX_CAMERA_ACTIVITY_ENTRIES);
  for (let i = 0; i < limit; i++) {
    const [k, v] = entries[i];
    if (
      !k ||
      k === "__proto__" ||
      k === "constructor" ||
      k === "prototype" ||
      !v ||
      typeof v !== "object"
    ) {
      continue;
    }
    out[k] = v;
  }
  return out;
}

export const SIMULATED_FLEET_GROUPS = [
  "Interno",
  "Perímetro",
  "Andar 1",
  "Estacionamento",
] as const;

export const SIMULATED_FLEET_GROUPS_EN = [
  "Indoor",
  "Perimeter",
  "Floor 1",
  "Parking",
] as const;

export const SIMULATED_EMPTY_BASE_CAMERAS = [
  "cam_01",
  "cam_02",
  "cam_03",
  "cam_04",
] as const;

export function resolveInitialSimulatedFleetCount(): SimulatedFleetCount {
  const raw = Number(safeStorageGet("ninjanvr_simulated_fleet_count") || "0");
  if (raw === 12 || raw === 36 || raw === 100) {
    return raw;
  }
  return 0;
}

export function resolveInitialStreamQualityMode(): LiveStreamQualityMode {
  return sanitizeLiveStreamQualityMode(
    safeStorageGet("ninjanvr_stream_quality_mode"),
    safeStorageGet("ninjanvr_use_main_stream")
  );
}

/**
 * Resolves a simulated camera clone name (e.g. `cam_01_sim_01`) back to its underlying
 * real physical camera so `<LiveMsePlayer>` and snapshot URLs (`/api/:cam/latest.webp`)
 * stream real live MSE video and snapshots without touching `config.yml`.
 */
export function resolveRealCameraSource(
  cameraName: string,
  configuredCameras?: Record<string, unknown> | string[] | null
): string {
  const realList: string[] = Array.isArray(configuredCameras)
    ? configuredCameras.filter(Boolean)
    : configuredCameras && typeof configuredCameras === "object"
    ? Object.keys(configuredCameras)
    : [];

  const clean = (cameraName || "").trim();
  if (!clean) {
    return realList[0] || "";
  }
  if (realList.includes(clean)) {
    return clean;
  }

  const simMatch = clean.match(/^(.+)_sim_(\d+)$/);
  if (simMatch) {
    const baseName = simMatch[1];
    const simIdx = Math.max(0, parseInt(simMatch[2], 10) - 1);
    if (realList.length === 0 || realList.includes(baseName)) {
      return baseName;
    }
    return realList[simIdx % realList.length];
  }

  if (realList.length > 0) {
    const prefixHit = realList.find((rc) => clean.startsWith(`${rc}_sim_`));
    if (prefixHit) return prefixHit;
    return realList[0];
  }

  return clean;
}

export interface ExpandedSimulatedFleet {
  cameraNames: string[];
  camerasConfig: Record<string, CameraConfig>;
  cameraGroups: Record<string, CameraGroupConfig>;
}

/**
 * Dynamically expands the real configured cameras into `simulatedFleetCount` virtual channels
 * and populates localized tactical `camera_groups` in memory without inventing cameras when
 * simulation is disabled (`simulatedFleetCount === 0`).
 */
export function expandSimulatedFleet(
  realCameraNames: string[],
  camerasConfig?: Record<string, CameraConfig> | null,
  cameraGroups?: Record<string, CameraGroupConfig> | null,
  simulatedFleetCount: SimulatedFleetCount = 0,
  locale?: "en" | "pt"
): ExpandedSimulatedFleet {
  const nonSimCams = realCameraNames.filter((c) => Boolean(c) && !/_sim_\d+$/.test(c));
  const baseCfgMap: Record<string, CameraConfig> = { ...(camerasConfig || {}) };
  const baseGroups: Record<string, CameraGroupConfig> = {
    ...(cameraGroups || {}),
  };

  if (!simulatedFleetCount || simulatedFleetCount <= 0) {
    return {
      cameraNames: nonSimCams,
      camerasConfig: baseCfgMap,
      cameraGroups: baseGroups,
    };
  }

  const baseCams =
    nonSimCams.length > 0 ? nonSimCams : [...SIMULATED_EMPTY_BASE_CAMERAS];
  const expandedNames: string[] = [];
  const expandedCfg: Record<string, CameraConfig> = { ...baseCfgMap };

  for (let i = 0; i < simulatedFleetCount; i++) {
    const sourceCam = baseCams[i % baseCams.length];
    const camName =
      i < baseCams.length
        ? sourceCam
        : `${sourceCam}_sim_${String(i - baseCams.length + 1).padStart(2, "0")}`;
    expandedNames.push(camName);

    const srcCfg = baseCfgMap[sourceCam];
    if (srcCfg) {
      expandedCfg[camName] = {
        ...srcCfg,
        name: camName,
        enabled: true,
        ui: {
          ...(srcCfg.ui || {}),
          order: i + 1,
          dashboard: true,
        },
      };
    }
  }

  const groupLabels =
    locale === "en" ? SIMULATED_FLEET_GROUPS_EN : SIMULATED_FLEET_GROUPS;
  const expandedGroups: Record<string, CameraGroupConfig> = { ...baseGroups };
  groupLabels.forEach((groupName, gIdx) => {
    const groupCams = expandedNames.filter(
      (_, idx) => idx % groupLabels.length === gIdx
    );
    expandedGroups[groupName] = {
      order: gIdx + 1,
      icon: "camera",
      cameras: groupCams,
    };
  });

  return {
    cameraNames: expandedNames,
    camerasConfig: expandedCfg,
    cameraGroups: expandedGroups,
  };
}

/**
 * Validates and reconciles `selectedCamera` against `configuredCameras` (preferring enabled cameras)
 * and `simulatedFleetCount`, returning `""` when no camera is configured yet rather than a hardcoded
 * non-existent camera name.
 */
export function resolveValidCameraSelection(
  selectedCamera: string,
  configuredCameras?: Record<string, CameraConfig> | null,
  simulatedFleetCount: SimulatedFleetCount = 0
): string {
  const clean = (selectedCamera || "").trim();
  if (!configuredCameras || typeof configuredCameras !== "object") {
    if (simulatedFleetCount > 0) {
      const expanded = expandSimulatedFleet([], null, null, simulatedFleetCount).cameraNames;
      if (clean && expanded.includes(clean)) return clean;
      return expanded[0] || "";
    }
    return clean;
  }
  const allCams = Object.keys(configuredCameras);
  if (allCams.length === 0) {
    if (simulatedFleetCount > 0) {
      const expanded = expandSimulatedFleet([], configuredCameras, null, simulatedFleetCount).cameraNames;
      if (clean && expanded.includes(clean)) return clean;
      return expanded[0] || "";
    }
    return clean;
  }
  const enabledCams = allCams.filter((k) => configuredCameras[k]?.enabled !== false);
  const validRealCams = enabledCams.length > 0 ? enabledCams : allCams;

  if (simulatedFleetCount > 0) {
    const expanded = expandSimulatedFleet(
      validRealCams,
      configuredCameras,
      null,
      simulatedFleetCount
    ).cameraNames;
    if (clean && expanded.includes(clean)) {
      return clean;
    }
    return expanded[0] || validRealCams[0] || "";
  }

  if (clean && validRealCams.includes(clean)) {
    return clean;
  }
  return validRealCams[0] || "";
}

export interface PttAudioReturnOverride {
  pttCamera: string;
  priorCamera: string | null;
}

interface NvrRealtimeState {
  activeTab: WorkspaceTab;
  liveLayout: LiveLayoutMode;
  activeCameraGroup: string;
  matrixDensity: MatrixDensity;
  matrixPage: number;
  matrixTourEnabled: boolean;
  matrixTourIntervalSec: number;
  syncQuadCameras: string[];
  spotlightFilter: SpotlightFilter;
  spotlightHoldSec: number;
  spotlightTimelineMode: SpotlightTimelineMode;
  securityProfile: "armed" | "home" | "disarmed";
  selectedCamera: string;
  unmutedLiveCamera: string | null;
  pttAudioOverride: PttAudioReturnOverride | null;
  hudPosition: HudPosition;
  streamQualityMode: LiveStreamQualityMode;
  useMainStream: boolean;
  showVectors: boolean;
  showAlertRibbon: boolean;
  audibleAlertChime: boolean;
  browserNotifications: boolean;
  firstWeekday: FirstWeekday;
  themeMode: ThemeMode;
  highContrast: boolean;
  showYamlKeys: boolean;
  timeShiftTs: number | null;
  playbackSpeed: number;
  simulatedFleetCount: SimulatedFleetCount;
  lastDeletedFaceProfile: { name: string; ts: number } | null;
  subLabelsRefreshTick: number;
  wsOnline: boolean;
  coreDataError: string | null;
  config: NinjaConfigResponse | null;
  stats: NinjaStatsResponse | null;
  statsHistory: NinjaStatsResponse[];
  cameraActivity: Record<string, CameraLiveActivity>;
  cleanupJob: StorageCleanupJobSnapshot | null;
  cleanupDoneSeq: number;

  setActiveTab: (tab: WorkspaceTab) => void;
  setLiveLayout: (layout: LiveLayoutMode) => void;
  setActiveCameraGroup: (group: string) => void;
  setMatrixDensity: (density: MatrixDensity) => void;
  setMatrixPage: (pageOrFn: number | ((prev: number) => number)) => void;
  setMatrixTourEnabled: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setMatrixTourIntervalSec: (sec: number) => void;
  setSyncQuadCameras: (
    camsOrFn: string[] | ((prev: string[]) => string[])
  ) => void;
  setSpotlightFilter: (filter: SpotlightFilter) => void;
  setSpotlightHoldSec: (sec: number) => void;
  setSpotlightTimelineMode: (mode: SpotlightTimelineMode) => void;
  setSecurityProfile: (profile: "armed" | "home" | "disarmed") => void;
  setSelectedCamera: (camera: string) => void;
  setUnmutedLiveCamera: (camera: string | null) => void;
  toggleLiveCameraAudio: (camera: string) => void;
  beginPttAudioReturn: (camera: string) => void;
  endPttAudioReturn: (camera: string) => void;
  setHudPosition: (pos: HudPosition) => void;
  setStreamQualityMode: (mode: LiveStreamQualityMode) => void;
  setUseMainStream: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setShowVectors: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setShowAlertRibbon: (val: boolean) => void;
  setAudibleAlertChime: (val: boolean) => void;
  setBrowserNotifications: (val: boolean) => void;
  setFirstWeekday: (day: FirstWeekday) => void;
  setThemeMode: (mode: ThemeMode) => void;
  setHighContrast: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setShowYamlKeys: (val: boolean) => void;
  setTimeShiftTs: (ts: number | null) => void;
  setPlaybackSpeed: (spd: number) => void;
  setSimulatedFleetCount: (count: SimulatedFleetCount) => void;
  recordDeletedFaceProfile: (name: string) => void;
  bumpSubLabelsRefresh: () => void;
  resetUiPreferences: () => void;
  setWsOnline: (online: boolean) => void;
  setCoreDataError: (err: string | null) => void;
  setConfig: (config: NinjaConfigResponse | null) => void;
  setStats: (stats: NinjaStatsResponse | null) => void;
  appendStatsSample: (sample: NinjaStatsResponse) => void;
  setStatsHistory: (history: NinjaStatsResponse[]) => void;
  setCameraActivity: (activity: Record<string, CameraLiveActivity>) => void;
  setCleanupJob: (job: StorageCleanupJobSnapshot | null) => void;
  dismissCleanupJob: () => void;
}

const initialThemeMode = resolveInitialThemeMode();
const initialHighContrast = resolveInitialBool("ninjanvr_high_contrast", false);
applyThemeToDocument(initialThemeMode, initialHighContrast);

export const useNvrStore = create<NvrRealtimeState>((set, get) => ({
  activeTab: resolveInitialTab(),
  liveLayout: resolveInitialLiveLayout(),
  activeCameraGroup: "all",
  matrixDensity: resolveInitialMatrixDensity(),
  matrixPage: 0,
  matrixTourEnabled: false,
  matrixTourIntervalSec: 15,
  syncQuadCameras: [],
  spotlightFilter: "all_activity",
  spotlightHoldSec: 15,
  spotlightTimelineMode: resolveInitialSpotlightTimelineMode(),
  securityProfile: "armed",
  selectedCamera: "",
  unmutedLiveCamera: null,
  pttAudioOverride: null,
  hudPosition: resolveInitialHudPosition(),
  streamQualityMode: resolveInitialStreamQualityMode(),
  useMainStream: resolveInitialStreamQualityMode() === "main",
  showVectors: resolveInitialBool("ninjanvr_show_vectors", false),
  showAlertRibbon: resolveInitialBool("ninjanvr_show_alert_ribbon", true),
  audibleAlertChime: resolveInitialBool("ninjanvr_audible_chime", false),
  browserNotifications: resolveInitialBool("ninjanvr_browser_notifications", false),
  firstWeekday: resolveInitialFirstWeekday(),
  themeMode: initialThemeMode,
  highContrast: initialHighContrast,
  showYamlKeys: resolveInitialBool("ninjanvr_show_yaml_keys", false),
  timeShiftTs: null,
  playbackSpeed: resolveInitialPlaybackSpeed(),
  simulatedFleetCount: resolveInitialSimulatedFleetCount(),
  lastDeletedFaceProfile: null,
  subLabelsRefreshTick: 0,
  wsOnline: false,
  coreDataError: null,
  config: null,
  stats: null,
  statsHistory: [],
  cameraActivity: {},
  cleanupJob: null,
  cleanupDoneSeq: 0,

  setActiveTab: (activeTab) => set({ activeTab }),
  setLiveLayout: (liveLayout) => {
    safeStorageSet("ninjanvr_live_layout", liveLayout);
    set({ liveLayout });
  },
  setActiveCameraGroup: (activeCameraGroup) =>
    set({ activeCameraGroup, matrixPage: 0 }),
  setMatrixDensity: (matrixDensity) => {
    safeStorageSet("ninjanvr_matrix_density", matrixDensity);
    set({ matrixDensity, matrixPage: 0 });
  },
  setMatrixPage: (pageOrFn) =>
    set((s) => {
      const next =
        typeof pageOrFn === "function" ? pageOrFn(s.matrixPage) : pageOrFn;
      return { matrixPage: Math.max(0, Number.isFinite(next) ? Math.floor(next) : 0) };
    }),
  setMatrixTourEnabled: (fnOrVal) =>
    set((s) => ({
      matrixTourEnabled:
        typeof fnOrVal === "function" ? fnOrVal(s.matrixTourEnabled) : fnOrVal,
    })),
  setMatrixTourIntervalSec: (matrixTourIntervalSec) =>
    set({
      matrixTourIntervalSec: Math.max(
        3,
        Number.isFinite(matrixTourIntervalSec) ? matrixTourIntervalSec : 15
      ),
    }),
  setSyncQuadCameras: (camsOrFn) =>
    set((s) => {
      const next =
        typeof camsOrFn === "function" ? camsOrFn(s.syncQuadCameras) : camsOrFn;
      const clean = Array.isArray(next)
        ? next
            .filter((c): c is string => typeof c === "string")
            .map((c) => c.trim())
            .filter(Boolean)
        : [];
      return { syncQuadCameras: Array.from(new Set(clean)).slice(0, 4) };
    }),
  setSpotlightFilter: (spotlightFilter) => set({ spotlightFilter }),
  setSpotlightHoldSec: (spotlightHoldSec) =>
    set({
      spotlightHoldSec: Math.max(
        3,
        Number.isFinite(spotlightHoldSec) ? Math.floor(spotlightHoldSec) : 15
      ),
    }),
  setSpotlightTimelineMode: (spotlightTimelineMode) => {
    safeStorageSet("ninjanvr_spotlight_timeline_mode", spotlightTimelineMode);
    set({ spotlightTimelineMode });
  },
  setSecurityProfile: (securityProfile) => set({ securityProfile }),
  setSelectedCamera: (selectedCamera) =>
    set({ selectedCamera: (selectedCamera || "").trim() }),
  setUnmutedLiveCamera: (camera) =>
    set({
      unmutedLiveCamera: (camera || "").trim() || null,
      pttAudioOverride: null,
    }),
  toggleLiveCameraAudio: (camera) =>
    set((s) => {
      const clean = (camera || "").trim();
      if (!clean) {
        return { unmutedLiveCamera: null, pttAudioOverride: null };
      }
      const next = s.unmutedLiveCamera === clean ? null : clean;
      return {
        unmutedLiveCamera: next,
        pttAudioOverride: null,
      };
    }),
  beginPttAudioReturn: (camera) =>
    set((s) => {
      const clean = (camera || "").trim();
      if (!clean) return s;
      if (s.pttAudioOverride && s.pttAudioOverride.pttCamera === clean) {
        return s.unmutedLiveCamera === clean
          ? s
          : { unmutedLiveCamera: clean };
      }
      return {
        pttAudioOverride: {
          pttCamera: clean,
          priorCamera: s.unmutedLiveCamera,
        },
        unmutedLiveCamera: clean,
      };
    }),
  endPttAudioReturn: (camera) =>
    set((s) => {
      const clean = (camera || "").trim();
      if (!s.pttAudioOverride || s.pttAudioOverride.pttCamera !== clean) {
        return s;
      }
      return {
        unmutedLiveCamera: s.pttAudioOverride.priorCamera,
        pttAudioOverride: null,
      };
    }),
  setHudPosition: (hudPosition) => {
    safeStorageSet("ninjanvr_hud_position", hudPosition);
    set({ hudPosition });
  },
  setStreamQualityMode: (mode) => {
    const streamQualityMode = sanitizeLiveStreamQualityMode(mode);
    const useMainStream = streamQualityMode === "main";
    safeStorageSet("ninjanvr_stream_quality_mode", streamQualityMode);
    safeStorageSet("ninjanvr_use_main_stream", String(useMainStream));
    set({ streamQualityMode, useMainStream });
  },
  setUseMainStream: (fnOrVal) =>
    set((s) => {
      const next = typeof fnOrVal === "function" ? fnOrVal(s.useMainStream) : fnOrVal;
      const streamQualityMode: LiveStreamQualityMode = next ? "main" : "sub";
      safeStorageSet("ninjanvr_use_main_stream", String(next));
      safeStorageSet("ninjanvr_stream_quality_mode", streamQualityMode);
      return { useMainStream: next, streamQualityMode };
    }),
  setShowVectors: (fnOrVal) =>
    set((s) => {
      const next = typeof fnOrVal === "function" ? fnOrVal(s.showVectors) : fnOrVal;
      safeStorageSet("ninjanvr_show_vectors", String(next));
      return { showVectors: next };
    }),
  setShowAlertRibbon: (showAlertRibbon) => {
    safeStorageSet("ninjanvr_show_alert_ribbon", String(showAlertRibbon));
    set({ showAlertRibbon });
  },
  setAudibleAlertChime: (audibleAlertChime) => {
    safeStorageSet("ninjanvr_audible_chime", String(audibleAlertChime));
    set({ audibleAlertChime });
  },
  setBrowserNotifications: (browserNotifications) => {
    safeStorageSet(
      "ninjanvr_browser_notifications",
      String(browserNotifications)
    );
    set({ browserNotifications });
  },
  setFirstWeekday: (firstWeekday) => {
    safeStorageSet("ninjanvr_first_weekday", firstWeekday);
    set({ firstWeekday });
  },
  setThemeMode: (themeMode) => {
    safeStorageSet("ninjanvr_theme_mode", themeMode);
    applyThemeToDocument(themeMode, get().highContrast);
    set({ themeMode });
  },
  setHighContrast: (fnOrVal) =>
    set((s) => {
      const next =
        typeof fnOrVal === "function" ? fnOrVal(s.highContrast) : fnOrVal;
      safeStorageSet("ninjanvr_high_contrast", String(next));
      applyThemeToDocument(s.themeMode, next);
      return { highContrast: next };
    }),
  setShowYamlKeys: (showYamlKeys) => {
    safeStorageSet("ninjanvr_show_yaml_keys", String(showYamlKeys));
    set({ showYamlKeys });
  },
  setTimeShiftTs: (timeShiftTs) =>
    set({
      timeShiftTs:
        typeof timeShiftTs === "number" &&
        Number.isFinite(timeShiftTs) &&
        timeShiftTs > 0
          ? timeShiftTs
          : null,
    }),
  setPlaybackSpeed: (playbackSpeed) => {
    const safeSpeed =
      typeof playbackSpeed === "number" &&
      Number.isFinite(playbackSpeed) &&
      playbackSpeed > 0
        ? Math.min(16, Math.max(0.1, playbackSpeed))
        : 1;
    safeStorageSet("ninjanvr_playback_speed", String(safeSpeed));
    set({ playbackSpeed: safeSpeed });
  },
  setSimulatedFleetCount: (simulatedFleetCount) => {
    if (simulatedFleetCount > 0) {
      safeStorageSet(
        "ninjanvr_simulated_fleet_count",
        String(simulatedFleetCount)
      );
    } else {
      safeStorageRemove("ninjanvr_simulated_fleet_count");
    }
    set((s) => {
      const nextSelected =
        simulatedFleetCount === 0
          ? resolveRealCameraSource(s.selectedCamera, s.config?.cameras)
          : resolveValidCameraSelection(
              s.selectedCamera,
              s.config?.cameras,
              simulatedFleetCount
            );
      const nextGroup =
        simulatedFleetCount === 0 &&
        s.activeCameraGroup !== "all" &&
        !s.config?.camera_groups?.[s.activeCameraGroup]
          ? "all"
          : s.activeCameraGroup;
      const nextDensity: MatrixDensity =
        simulatedFleetCount >= 36
          ? "4x4"
          : simulatedFleetCount >= 12
          ? "3x3"
          : "2x2";
      const nextLayout: LiveLayoutMode =
        simulatedFleetCount > 0 ? "2x2" : s.liveLayout;
      return {
        simulatedFleetCount,
        liveLayout: nextLayout,
        matrixDensity: nextDensity,
        matrixPage: 0,
        selectedCamera: nextSelected,
        activeCameraGroup: nextGroup,
      };
    });
  },
  recordDeletedFaceProfile: (name) =>
    set((s) => ({
      lastDeletedFaceProfile: { name: name.trim(), ts: Date.now() },
      subLabelsRefreshTick: s.subLabelsRefreshTick + 1,
    })),
  bumpSubLabelsRefresh: () =>
    set((s) => ({ subLabelsRefreshTick: s.subLabelsRefreshTick + 1 })),
  resetUiPreferences: () => {
    [
      "ninjanvr_live_layout",
      "ninjanvr_matrix_density",
      "ninjanvr_spotlight_timeline_mode",
      "ninjanvr_hud_position",
      "ninjanvr_stream_quality_mode",
      "ninjanvr_use_main_stream",
      "ninjanvr_show_vectors",
      "ninjanvr_show_alert_ribbon",
      "ninjanvr_audible_chime",
      "ninjanvr_browser_notifications",
      "ninjanvr_first_weekday",
      "ninjanvr_theme_mode",
      "ninjanvr_high_contrast",
      "ninjanvr_show_yaml_keys",
      "ninjanvr_playback_speed",
      "ninjanvr_simulated_fleet_count",
    ].forEach((k) => safeStorageRemove(k));
    applyThemeToDocument("dark", false);
    set({
      liveLayout: "2x2",
      activeCameraGroup: "all",
      matrixDensity: "2x2",
      matrixPage: 0,
      matrixTourEnabled: false,
      matrixTourIntervalSec: 15,
      syncQuadCameras: [],
      spotlightTimelineMode: "reactive",
      unmutedLiveCamera: null,
      pttAudioOverride: null,
      hudPosition: "bottom-right",
      streamQualityMode: "auto",
      useMainStream: false,
      showVectors: false,
      showAlertRibbon: true,
      audibleAlertChime: false,
      browserNotifications: false,
      firstWeekday: "sunday",
      themeMode: "dark",
      highContrast: false,
      showYamlKeys: false,
      playbackSpeed: 1,
      simulatedFleetCount: 0,
    });
  },
  setWsOnline: (wsOnline) => set({ wsOnline }),
  setCoreDataError: (coreDataError) => set({ coreDataError }),
  setConfig: (config) =>
    set((s) => {
      let nextThemeMode = s.themeMode;
      let nextHighContrast = s.highContrast;
      const hasLocalTheme = safeStorageGet("ninjanvr_theme_mode") !== null;
      const hasLocalHc = safeStorageGet("ninjanvr_high_contrast") !== null;

      const cfgTheme = config?.ui?.theme;
      if (
        !hasLocalTheme &&
        (cfgTheme === "dark" || cfgTheme === "light" || cfgTheme === "system")
      ) {
        nextThemeMode = cfgTheme;
      }
      if (!hasLocalHc && typeof config?.ui?.high_contrast === "boolean") {
        nextHighContrast = config.ui.high_contrast;
      }
      applyThemeToDocument(nextThemeMode, nextHighContrast);
      const nextSelectedCamera = resolveValidCameraSelection(
        s.selectedCamera,
        config?.cameras,
        s.simulatedFleetCount
      );

      let nextConfig: NinjaConfigResponse | null = config;
      if (s.config && config) {
        if (stableSerializeConfigValue(s.config) === stableSerializeConfigValue(config)) {
          nextConfig = s.config;
        } else if (
          s.config.cameras &&
          config.cameras &&
          stableSerializeConfigValue(s.config.cameras) ===
            stableSerializeConfigValue(config.cameras)
        ) {
          nextConfig = { ...config, cameras: s.config.cameras };
        }
      }

      if (
        nextConfig === s.config &&
        nextSelectedCamera === s.selectedCamera &&
        nextThemeMode === s.themeMode &&
        nextHighContrast === s.highContrast
      ) {
        return s;
      }

      return {
        config: nextConfig,
        selectedCamera: nextSelectedCamera,
        themeMode: nextThemeMode,
        highContrast: nextHighContrast,
      };
    }),
  setStats: (stats) => set({ stats }),
  appendStatsSample: (sample) =>
    set((s) => ({
      stats: sample,
      statsHistory: [...s.statsHistory, sample].slice(-MAX_STATS_HISTORY_SAMPLES),
    })),
  setStatsHistory: (statsHistory) =>
    set({
      statsHistory: Array.isArray(statsHistory)
        ? statsHistory.slice(-MAX_STATS_HISTORY_SAMPLES)
        : [],
    }),
  setCameraActivity: (cameraActivity) =>
    set((s) => {
      const sanitized = sanitizeCameraActivityMap(cameraActivity);
      const prev = s.cameraActivity;
      const nextKeys = Object.keys(sanitized);
      const prevKeys = Object.keys(prev);
      let unchangedCount = 0;
      const merged: Record<string, CameraLiveActivity> = {};
      for (const key of nextKeys) {
        const prevEntry = prev[key];
        const nextEntry = sanitized[key];
        if (
          prevEntry !== undefined &&
          stableSerializeConfigValue(prevEntry) ===
            stableSerializeConfigValue(nextEntry)
        ) {
          merged[key] = prevEntry;
          unchangedCount += 1;
        } else {
          merged[key] = nextEntry;
        }
      }
      if (
        unchangedCount === nextKeys.length &&
        nextKeys.length === prevKeys.length
      ) {
        return s;
      }
      return { cameraActivity: merged };
    }),
  setCleanupJob: (nextJob) =>
    set((s) => {
      if (!nextJob) {
        return { cleanupJob: null };
      }
      const isTerm = (st: StorageCleanupJobSnapshot["status"]) =>
        st === "completed" || st === "cancelled" || st === "failed";
      const prev = s.cleanupJob;
      if (prev && prev.id === nextJob.id) {
        if (isTerm(prev.status) && !isTerm(nextJob.status)) {
          return s;
        }
        const mergedJob: StorageCleanupJobSnapshot = isTerm(nextJob.status)
          ? nextJob
          : {
              ...nextJob,
              progress_pct: Math.max(prev.progress_pct, nextJob.progress_pct),
            };
        const becameTerminal = !isTerm(prev.status) && isTerm(nextJob.status);
        return {
          cleanupJob: mergedJob,
          cleanupDoneSeq: becameTerminal ? s.cleanupDoneSeq + 1 : s.cleanupDoneSeq,
        };
      }
      const becameTerminal = isTerm(nextJob.status);
      return {
        cleanupJob: nextJob,
        cleanupDoneSeq: becameTerminal ? s.cleanupDoneSeq + 1 : s.cleanupDoneSeq,
      };
    }),
  dismissCleanupJob: () =>
    set((s) => {
      if (!s.cleanupJob) return s;
      const st = s.cleanupJob.status;
      if (st === "queued" || st === "running" || st === "cancelling") {
        return s;
      }
      return { cleanupJob: null };
    }),
}));
