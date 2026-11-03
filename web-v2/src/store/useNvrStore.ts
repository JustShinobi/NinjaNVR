import { create } from "zustand";
import {
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
