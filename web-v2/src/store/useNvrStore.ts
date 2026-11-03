import { create } from "zustand";
import {
  CameraConfig,
  CameraGroupConfig,
  CameraLiveActivity,
  NinjaConfigResponse,
  NinjaStatsResponse,
} from "../types/nvr";
import {
  FirstWeekday,
  HudPosition,
  LiveLayoutMode,
  MatrixDensity,
  SpotlightFilter,
  ThemeMode,
  WorkspaceTab,
  applyThemeToDocument,
  resolveInitialBool,
  resolveInitialFirstWeekday,
  resolveInitialHudPosition,
  resolveInitialLiveLayout,
  resolveInitialMatrixDensity,
  resolveInitialPlaybackSpeed,
  resolveInitialTab,
  resolveInitialThemeMode,
} from "../utils/format";

export type SimulatedFleetCount = 0 | 12 | 36 | 100;

export const SIMULATED_FLEET_GROUPS = [
  "Interno",
  "Perímetro",
  "Andar 1",
  "Estacionamento",
] as const;

export function resolveInitialSimulatedFleetCount(): SimulatedFleetCount {
  try {
    if (typeof window !== "undefined") {
      const raw = Number(
        window.localStorage?.getItem("ninjanvr_simulated_fleet_count") || "0"
      );
      if (raw === 12 || raw === 36 || raw === 100) {
        return raw;
      }
    }
  } catch {
    // ignore
  }
  return 0;
}

/**
 * Resolves a simulated camera clone name (e.g. `escritorio_sim_01`, `sala_sim_02`)
 * back to its underlying real physical camera so `<LiveMsePlayer>` and snapshot URLs
 * (`/api/:cam/latest.webp`) stream real live MSE video and snapshots without touching `config.yml`.
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
    return realList[0] || "escritorio";
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
 * (e.g. `escritorio_sim_01`, `sala_sim_02`, ...) and populates tactical `camera_groups`
 * (`Interno`, `Perímetro`, `Andar 1`, `Estacionamento`) in memory.
 */
export function expandSimulatedFleet(
  realCameraNames: string[],
  camerasConfig?: Record<string, CameraConfig> | null,
  cameraGroups?: Record<string, CameraGroupConfig> | null,
  simulatedFleetCount: SimulatedFleetCount = 0
): ExpandedSimulatedFleet {
  const nonSimCams = realCameraNames.filter((c) => !/_sim_\d+$/.test(c));
  const baseCams =
    nonSimCams.length > 0
      ? nonSimCams
      : ["escritorio", "sala", "quarto", "cozinha"];
  const baseCfgMap: Record<string, CameraConfig> = { ...(camerasConfig || {}) };
  const baseGroups: Record<string, CameraGroupConfig> = {
    ...(cameraGroups || {}),
  };

  if (!simulatedFleetCount || simulatedFleetCount <= 0) {
    return {
      cameraNames: baseCams,
      camerasConfig: baseCfgMap,
      cameraGroups: baseGroups,
    };
  }

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

  const expandedGroups: Record<string, CameraGroupConfig> = { ...baseGroups };
  SIMULATED_FLEET_GROUPS.forEach((groupName, gIdx) => {
    const groupCams = expandedNames.filter(
      (_, idx) => idx % SIMULATED_FLEET_GROUPS.length === gIdx
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
  securityProfile: "armed" | "home" | "disarmed";
  selectedCamera: string;
  hudPosition: HudPosition;
  useMainStream: boolean;
  showVectors: boolean;
  showAlertRibbon: boolean;
  audibleAlertChime: boolean;
  browserNotifications: boolean;
  firstWeekday: FirstWeekday;
  themeMode: ThemeMode;
  highContrast: boolean;
  timeShiftTs: number | null;
  playbackSpeed: number;
  simulatedFleetCount: SimulatedFleetCount;
  lastDeletedFaceProfile: { name: string; ts: number } | null;
  subLabelsRefreshTick: number;
  wsOnline: boolean;
  config: NinjaConfigResponse | null;
  stats: NinjaStatsResponse | null;
  statsHistory: NinjaStatsResponse[];
  cameraActivity: Record<string, CameraLiveActivity>;

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
  setSecurityProfile: (profile: "armed" | "home" | "disarmed") => void;
  setSelectedCamera: (camera: string) => void;
  setHudPosition: (pos: HudPosition) => void;
  setUseMainStream: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setShowVectors: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setShowAlertRibbon: (val: boolean) => void;
  setAudibleAlertChime: (val: boolean) => void;
  setBrowserNotifications: (val: boolean) => void;
  setFirstWeekday: (day: FirstWeekday) => void;
  setThemeMode: (mode: ThemeMode) => void;
  setHighContrast: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setTimeShiftTs: (ts: number | null) => void;
  setPlaybackSpeed: (spd: number) => void;
  setSimulatedFleetCount: (count: SimulatedFleetCount) => void;
  recordDeletedFaceProfile: (name: string) => void;
  bumpSubLabelsRefresh: () => void;
  resetUiPreferences: () => void;
  setWsOnline: (online: boolean) => void;
  setConfig: (config: NinjaConfigResponse | null) => void;
  setStats: (stats: NinjaStatsResponse | null) => void;
  setStatsHistory: (history: NinjaStatsResponse[]) => void;
  setCameraActivity: (activity: Record<string, CameraLiveActivity>) => void;
}

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
  securityProfile: "armed",
  selectedCamera: "escritorio",
  hudPosition: resolveInitialHudPosition(),
  useMainStream: resolveInitialBool("ninjanvr_use_main_stream", false),
  showVectors: resolveInitialBool("ninjanvr_show_vectors", false),
  showAlertRibbon: resolveInitialBool("ninjanvr_show_alert_ribbon", true),
  audibleAlertChime: resolveInitialBool("ninjanvr_audible_chime", false),
  browserNotifications: resolveInitialBool("ninjanvr_browser_notifications", false),
  firstWeekday: resolveInitialFirstWeekday(),
  themeMode: resolveInitialThemeMode(),
  highContrast: resolveInitialBool("ninjanvr_high_contrast", false),
  timeShiftTs: null,
  playbackSpeed: resolveInitialPlaybackSpeed(),
  simulatedFleetCount: resolveInitialSimulatedFleetCount(),
  lastDeletedFaceProfile: null,
  subLabelsRefreshTick: 0,
  wsOnline: false,
  config: null,
  stats: null,
  statsHistory: [],
  cameraActivity: {},

  setActiveTab: (activeTab) => set({ activeTab }),
  setLiveLayout: (liveLayout) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_live_layout", liveLayout);
    }
    set({ liveLayout });
  },
  setActiveCameraGroup: (activeCameraGroup) =>
    set({ activeCameraGroup, matrixPage: 0 }),
  setMatrixDensity: (matrixDensity) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_matrix_density", matrixDensity);
    }
    set({ matrixDensity, matrixPage: 0 });
  },
  setMatrixPage: (pageOrFn) =>
    set((s) => {
      const next =
        typeof pageOrFn === "function" ? pageOrFn(s.matrixPage) : pageOrFn;
      return { matrixPage: Math.max(0, next) };
    }),
  setMatrixTourEnabled: (fnOrVal) =>
    set((s) => ({
      matrixTourEnabled:
        typeof fnOrVal === "function" ? fnOrVal(s.matrixTourEnabled) : fnOrVal,
    })),
  setMatrixTourIntervalSec: (matrixTourIntervalSec) =>
    set({ matrixTourIntervalSec: Math.max(3, matrixTourIntervalSec) }),
  setSyncQuadCameras: (camsOrFn) =>
    set((s) => {
      const next =
        typeof camsOrFn === "function" ? camsOrFn(s.syncQuadCameras) : camsOrFn;
      return { syncQuadCameras: Array.from(new Set(next)).slice(0, 4) };
    }),
  setSpotlightFilter: (spotlightFilter) => set({ spotlightFilter }),
  setSpotlightHoldSec: (spotlightHoldSec) => set({ spotlightHoldSec }),
  setSecurityProfile: (securityProfile) => set({ securityProfile }),
  setSelectedCamera: (selectedCamera) => set({ selectedCamera }),
  setHudPosition: (hudPosition) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_hud_position", hudPosition);
    }
    set({ hudPosition });
  },
  setUseMainStream: (fnOrVal) =>
    set((s) => {
      const next = typeof fnOrVal === "function" ? fnOrVal(s.useMainStream) : fnOrVal;
      if (typeof window !== "undefined") {
        window.localStorage?.setItem("ninjanvr_use_main_stream", String(next));
      }
      return { useMainStream: next };
    }),
  setShowVectors: (fnOrVal) =>
    set((s) => {
      const next = typeof fnOrVal === "function" ? fnOrVal(s.showVectors) : fnOrVal;
      if (typeof window !== "undefined") {
        window.localStorage?.setItem("ninjanvr_show_vectors", String(next));
      }
      return { showVectors: next };
    }),
  setShowAlertRibbon: (showAlertRibbon) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_show_alert_ribbon", String(showAlertRibbon));
    }
    set({ showAlertRibbon });
  },
  setAudibleAlertChime: (audibleAlertChime) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_audible_chime", String(audibleAlertChime));
    }
    set({ audibleAlertChime });
  },
  setBrowserNotifications: (browserNotifications) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem(
        "ninjanvr_browser_notifications",
        String(browserNotifications)
      );
    }
    set({ browserNotifications });
  },
  setFirstWeekday: (firstWeekday) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_first_weekday", firstWeekday);
    }
    set({ firstWeekday });
  },
  setThemeMode: (themeMode) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_theme_mode", themeMode);
    }
    applyThemeToDocument(themeMode, get().highContrast);
    set({ themeMode });
  },
  setHighContrast: (fnOrVal) =>
    set((s) => {
      const next =
        typeof fnOrVal === "function" ? fnOrVal(s.highContrast) : fnOrVal;
      if (typeof window !== "undefined") {
        window.localStorage?.setItem("ninjanvr_high_contrast", String(next));
      }
      applyThemeToDocument(s.themeMode, next);
      return { highContrast: next };
    }),
  setTimeShiftTs: (timeShiftTs) => set({ timeShiftTs }),
  setPlaybackSpeed: (playbackSpeed) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_playback_speed", String(playbackSpeed));
    }
    set({ playbackSpeed });
  },
  setSimulatedFleetCount: (simulatedFleetCount) => {
    if (typeof window !== "undefined") {
      if (simulatedFleetCount > 0) {
        window.localStorage?.setItem(
          "ninjanvr_simulated_fleet_count",
          String(simulatedFleetCount)
        );
      } else {
        window.localStorage?.removeItem("ninjanvr_simulated_fleet_count");
      }
    }
    set((s) => {
      const nextSelected =
        simulatedFleetCount === 0
          ? resolveRealCameraSource(s.selectedCamera, s.config?.cameras)
          : s.selectedCamera;
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
    if (typeof window !== "undefined") {
      [
        "ninjanvr_live_layout",
        "ninjanvr_matrix_density",
        "ninjanvr_hud_position",
        "ninjanvr_use_main_stream",
        "ninjanvr_show_vectors",
        "ninjanvr_show_alert_ribbon",
        "ninjanvr_audible_chime",
        "ninjanvr_browser_notifications",
        "ninjanvr_first_weekday",
        "ninjanvr_theme_mode",
        "ninjanvr_high_contrast",
        "ninjanvr_playback_speed",
        "ninjanvr_simulated_fleet_count",
      ].forEach((k) => window.localStorage?.removeItem(k));
    }
    applyThemeToDocument("dark", false);
    set({
      liveLayout: "2x2",
      activeCameraGroup: "all",
      matrixDensity: "2x2",
      matrixPage: 0,
      matrixTourEnabled: false,
      matrixTourIntervalSec: 15,
      syncQuadCameras: [],
      hudPosition: "bottom-right",
      useMainStream: false,
      showVectors: false,
      showAlertRibbon: true,
      audibleAlertChime: false,
      browserNotifications: false,
      firstWeekday: "sunday",
      themeMode: "dark",
      highContrast: false,
      playbackSpeed: 1,
      simulatedFleetCount: 0,
    });
  },
  setWsOnline: (wsOnline) => set({ wsOnline }),
  setConfig: (config) =>
    set((s) => {
      let nextThemeMode = s.themeMode;
      let nextHighContrast = s.highContrast;
      const hasLocalTheme =
        typeof window !== "undefined" &&
        window.localStorage?.getItem("ninjanvr_theme_mode") !== null;
      const hasLocalHc =
        typeof window !== "undefined" &&
        window.localStorage?.getItem("ninjanvr_high_contrast") !== null;

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
      return {
        config,
        themeMode: nextThemeMode,
        highContrast: nextHighContrast,
      };
    }),
  setStats: (stats) => set({ stats }),
  setStatsHistory: (statsHistory) => set({ statsHistory }),
  setCameraActivity: (cameraActivity) => set({ cameraActivity }),
}));
