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
  SpotlightFilter,
  WorkspaceTab,
  resolveInitialBool,
  resolveInitialFirstWeekday,
  resolveInitialHudPosition,
  resolveInitialLiveLayout,
  resolveInitialPlaybackSpeed,
  resolveInitialTab,
} from "../utils/format";

interface NvrRealtimeState {
  activeTab: WorkspaceTab;
  liveLayout: LiveLayoutMode;
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
  timeShiftTs: number | null;
  playbackSpeed: number;
  wsOnline: boolean;
  config: NinjaConfigResponse | null;
  stats: NinjaStatsResponse | null;
  statsHistory: NinjaStatsResponse[];
  cameraActivity: Record<string, CameraLiveActivity>;

  setActiveTab: (tab: WorkspaceTab) => void;
  setLiveLayout: (layout: LiveLayoutMode) => void;
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
  setTimeShiftTs: (ts: number | null) => void;
  setPlaybackSpeed: (spd: number) => void;
  resetUiPreferences: () => void;
  setWsOnline: (online: boolean) => void;
  setConfig: (config: NinjaConfigResponse | null) => void;
  setStats: (stats: NinjaStatsResponse | null) => void;
  setStatsHistory: (history: NinjaStatsResponse[]) => void;
  setCameraActivity: (activity: Record<string, CameraLiveActivity>) => void;
}

export const useNvrStore = create<NvrRealtimeState>((set) => ({
  activeTab: resolveInitialTab(),
  liveLayout: resolveInitialLiveLayout(),
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
        "ninjanvr_hud_position",
        "ninjanvr_use_main_stream",
        "ninjanvr_show_vectors",
        "ninjanvr_show_alert_ribbon",
        "ninjanvr_first_weekday",
        "ninjanvr_playback_speed",
      ].forEach((k) => window.localStorage?.removeItem(k));
    }
    set({
      liveLayout: "2x2",
      hudPosition: "bottom-right",
      useMainStream: false,
      showVectors: false,
      showAlertRibbon: true,
      firstWeekday: "sunday",
      playbackSpeed: 1,
    });
  },
  setWsOnline: (wsOnline) => set({ wsOnline }),
  setConfig: (config) => set({ config }),
  setStats: (stats) => set({ stats }),
  setStatsHistory: (statsHistory) => set({ statsHistory }),
  setCameraActivity: (cameraActivity) => set({ cameraActivity }),
}));
