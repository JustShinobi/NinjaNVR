import { create } from "zustand";
import {
  CameraLiveActivity,
  NinjaConfigResponse,
  NinjaStatsResponse,
} from "../types/nvr";
import {
  HudPosition,
  LiveLayoutMode,
  WorkspaceTab,
  resolveInitialHudPosition,
  resolveInitialTab,
} from "../utils/format";

interface NvrRealtimeState {
  activeTab: WorkspaceTab;
  liveLayout: LiveLayoutMode;
  selectedCamera: string;
  hudPosition: HudPosition;
  useMainStream: boolean;
  showVectors: boolean;
  timeShiftTs: number | null;
  playbackSpeed: number;
  wsOnline: boolean;
  config: NinjaConfigResponse | null;
  stats: NinjaStatsResponse | null;
  statsHistory: NinjaStatsResponse[];
  cameraActivity: Record<string, CameraLiveActivity>;

  setActiveTab: (tab: WorkspaceTab) => void;
  setLiveLayout: (layout: LiveLayoutMode) => void;
  setSelectedCamera: (camera: string) => void;
  setHudPosition: (pos: HudPosition) => void;
  setUseMainStream: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setShowVectors: (fnOrVal: boolean | ((prev: boolean) => boolean)) => void;
  setTimeShiftTs: (ts: number | null) => void;
  setPlaybackSpeed: (spd: number) => void;
  setWsOnline: (online: boolean) => void;
  setConfig: (config: NinjaConfigResponse | null) => void;
  setStats: (stats: NinjaStatsResponse | null) => void;
  setStatsHistory: (history: NinjaStatsResponse[]) => void;
  setCameraActivity: (activity: Record<string, CameraLiveActivity>) => void;
}

export const useNvrStore = create<NvrRealtimeState>((set) => ({
  activeTab: resolveInitialTab(),
  liveLayout: "2x2",
  selectedCamera: "escritorio",
  hudPosition: resolveInitialHudPosition(),
  useMainStream: false,
  showVectors: false,
  timeShiftTs: null,
  playbackSpeed: 1,
  wsOnline: false,
  config: null,
  stats: null,
  statsHistory: [],
  cameraActivity: {},

  setActiveTab: (activeTab) => set({ activeTab }),
  setLiveLayout: (liveLayout) => set({ liveLayout }),
  setSelectedCamera: (selectedCamera) => set({ selectedCamera }),
  setHudPosition: (hudPosition) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_hud_position", hudPosition);
    }
    set({ hudPosition });
  },
  setUseMainStream: (fnOrVal) =>
    set((s) => ({
      useMainStream: typeof fnOrVal === "function" ? fnOrVal(s.useMainStream) : fnOrVal,
    })),
  setShowVectors: (fnOrVal) =>
    set((s) => ({
      showVectors: typeof fnOrVal === "function" ? fnOrVal(s.showVectors) : fnOrVal,
    })),
  setTimeShiftTs: (timeShiftTs) => set({ timeShiftTs }),
  setPlaybackSpeed: (playbackSpeed) => set({ playbackSpeed }),
  setWsOnline: (wsOnline) => set({ wsOnline }),
  setConfig: (config) => set({ config }),
  setStats: (stats) => set({ stats }),
  setStatsHistory: (statsHistory) => set({ statsHistory }),
  setCameraActivity: (cameraActivity) => set({ cameraActivity }),
}));
