import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, test } from "bun:test";
import {
  WORKSPACE_TAB_URL_MAP,
  WS_FALLBACK_POLL_MS,
  WS_HEALTHY_HEARTBEAT_MS,
  isWorkspaceTab,
  normalizeWsTopic,
  resolveCoreDataPollIntervalMs,
  upsertBoundedReviewList,
} from "../App";
import {
  SIMULATED_FLEET_GROUPS,
  SIMULATED_FLEET_GROUPS_EN,
  expandSimulatedFleet,
  resolveRealCameraSource,
  resolveValidCameraSelection,
  useNvrStore,
} from "../store/useNvrStore";
import {
  I18nProvider,
  applyLocaleToDocument,
  locales,
  resolveHtmlLang,
} from "../i18n";
import {
  captureVodStillFrame,
  quantizeVodWindowBounds,
  resolveVodFrameStepDurationSec,
  syncDualSlotVideoMuteState,
} from "../components/VideoPlayers";
import {
  TimelineScrubber,
  computeTimelineSliderKeyStep,
} from "../components/TimelineScrubber";
import {
  LiveWorkspace,
  TacticalPtzHudOverlay,
  createTwoStepConfirmController,
} from "../views/LiveWorkspace";
import {
  SYNC_DRIFT_THRESHOLD_SEC,
  SyncWorkspace,
  advanceSyncClockTick,
  clampSyncPlayheadTs,
  shouldReconcileSyncDrift,
} from "../views/SyncWorkspace";
import { ExportsWorkspace } from "../views/ExportsWorkspace";
import {
  getFocusableSheetElements,
  trapSheetTabNavigation,
} from "../utils/useSheetDismiss";
import {
  extractSelectorCssVariables,
  wcagContrastRatio,
} from "../utils/themeTokens";
import type {
  CameraConfig,
  CameraHardwareInfoResponse,
  ExportItem,
  NinjaConfigResponse,
  ReviewItem,
} from "../types/nvr";

async function readSrcText(relativePathFromTests: string): Promise<string> {
  const fileUrl = new URL(relativePathFromTests, import.meta.url);
  const bunGlobal = (
    globalThis as unknown as {
      Bun: { file(url: URL): { text(): Promise<string> } };
    }
  ).Bun;
  return bunGlobal.file(fileUrl).text();
}

function withSyncedServerStoreSnapshot<T>(fn: () => T): T {
  const initial = useNvrStore.getInitialState();
  const backup = { ...initial };
  Object.assign(initial, useNvrStore.getState());
  try {
    return fn();
  } finally {
    Object.assign(initial, backup);
  }
}

function makeSampleCameraConfig(fps = 25): CameraConfig {
  return {
    enabled: true,
    detect: { enabled: true, width: 1920, height: 1080, fps },
    motion: { enabled: true, threshold: 30, contour_area: 10, mask: [] },
    zones: {},
    record: { enabled: true },
    snapshots: { enabled: true },
  };
}

describe("Task T3 Audit — [UX-P0-1] Single-Channel Live Audio & Bidirectional PTT Return", () => {
  beforeEach(() => {
    useNvrStore.setState({
      selectedCamera: "",
      unmutedLiveCamera: null,
      pttAudioOverride: null,
      timeShiftTs: null,
      liveLayout: "2x2",
      simulatedFleetCount: 0,
      config: null,
    });
  });

  test("enforces single-channel live audio across the matrix and restores prior mute state after Push-to-Talk unless manually overridden", () => {
    const store = useNvrStore.getState();
    expect(useNvrStore.getState().unmutedLiveCamera).toBeNull();

    // Unmute cam_a, then toggle cam_b -> only cam_b remains unmuted (zero multi-tile echo)
    store.toggleLiveCameraAudio("cam_a");
    expect(useNvrStore.getState().unmutedLiveCamera).toBe("cam_a");
    store.toggleLiveCameraAudio("cam_b");
    expect(useNvrStore.getState().unmutedLiveCamera).toBe("cam_b");

    // Holding PTT on cam_c temporarily switches audio return to cam_c and restores cam_b on release
    store.beginPttAudioReturn("cam_c");
    expect(useNvrStore.getState().unmutedLiveCamera).toBe("cam_c");
    store.endPttAudioReturn("cam_c");
    expect(useNvrStore.getState().unmutedLiveCamera).toBe("cam_b");

    // When all cameras start muted, PTT unmutes cam_a while talking and re-mutes on release
    store.setUnmutedLiveCamera(null);
    expect(useNvrStore.getState().unmutedLiveCamera).toBeNull();
    store.beginPttAudioReturn("cam_a");
    expect(useNvrStore.getState().unmutedLiveCamera).toBe("cam_a");
    store.endPttAudioReturn("cam_a");
    expect(useNvrStore.getState().unmutedLiveCamera).toBeNull();

    // Manual mute/unmute during active PTT overrides auto-restore on release
    store.beginPttAudioReturn("cam_a");
    expect(useNvrStore.getState().unmutedLiveCamera).toBe("cam_a");
    store.setUnmutedLiveCamera("cam_a"); // operator explicitly locked unmute
    store.endPttAudioReturn("cam_a");
    expect(useNvrStore.getState().unmutedLiveCamera).toBe("cam_a");
  });

  test("syncDualSlotVideoMuteState unmutes only the active MSE slot and keeps standby slot muted", () => {
    let playCalls0 = 0;
    let playCalls1 = 0;
    const slot0 = {
      muted: true,
      paused: true,
      play: () => {
        playCalls0 += 1;
        return Promise.resolve();
      },
    };
    const slot1 = {
      muted: false,
      paused: true,
      play: () => {
        playCalls1 += 1;
        return Promise.resolve();
      },
    };

    // Unmuted on slot 0 -> slot0.muted = false, slot1.muted = true, resumes paused active video
    syncDualSlotVideoMuteState(slot0, slot1, 0, false, true);
    expect(slot0.muted).toBe(false);
    expect(slot1.muted).toBe(true);
    expect(playCalls0).toBe(1);
    expect(playCalls1).toBe(0);

    // Promote to slot 1 while still unmuted -> slot0.muted = true, slot1.muted = false
    syncDualSlotVideoMuteState(slot0, slot1, 1, false, true);
    expect(slot0.muted).toBe(true);
    expect(slot1.muted).toBe(false);
    expect(playCalls1).toBe(1);

    // Muted -> both slots strictly muted
    syncDualSlotVideoMuteState(slot0, slot1, 1, true, true);
    expect(slot0.muted).toBe(true);
    expect(slot1.muted).toBe(true);
  });

  test("LiveWorkspace renders accessible Mute/Unmute tile button reflecting unmutedLiveCamera state", () => {
    const cfg: NinjaConfigResponse = {
      version: "1.0.0",
      safe_mode: false,
      cameras: {
        front_gate: makeSampleCameraConfig(15),
        courtyard: makeSampleCameraConfig(15),
      },
    };
    useNvrStore.setState({
      config: cfg,
      selectedCamera: "front_gate",
      unmutedLiveCamera: "front_gate",
      liveLayout: "2x2",
    });

    const html = withSyncedServerStoreSnapshot(() =>
      renderToStaticMarkup(
        React.createElement(LiveWorkspace, {
          cameraNames: ["front_gate", "courtyard"],
          reviews: [],
          unreviewedAlerts: [],
          focusSegments: [],
          handleMarkReviews: async () => {},
          handleQuickExport: async () => {},
        })
      )
    );

    expect(html).toContain('data-testid="tile-audio-front_gate"');
    expect(html).toContain('aria-label="Mute live audio: front_gate"');
    expect(html).toContain('data-testid="tile-audio-courtyard"');
    expect(html).toContain('aria-label="Unmute live audio: courtyard"');
  });
});

describe("Task T3 Audit — [UX-P0-2 & UX-P2-13] Dark Surface Tile Tokens in Light/HC Themes & Contrast", () => {
  test(".nvr-cam-tile re-specifies color-scheme: only dark, inherited color/accent-color, dark surface tokens, and High-Contrast overrides", async () => {
    const css = await readSrcText("../styles.css");
    const tileVars = extractSelectorCssVariables(css, ".nvr-cam-tile");

    expect(css).toMatch(/\.nvr-cam-tile\s*\{[\s\S]*?color-scheme:\s*only dark;/);
    expect(css).toMatch(/\.nvr-cam-tile\s*\{[\s\S]*?color:\s*var\(--text-primary\);/);
    expect(css).toMatch(
      /\.nvr-cam-tile\s*\{[\s\S]*?accent-color:\s*var\(--signal-accent-text\);/
    );

    expect(tileVars["--bg-panel"]).toBe("#192231");
    expect(tileVars["--bg-subcard"]).toBe("#141c29");
    expect(tileVars["--bg-input"]).toBe("#0d131d");
    expect(tileVars["--border-input"]).toBe("#4f658a");
    expect(tileVars["--border-divider"]).toBe("rgba(148, 163, 184, 0.16)");
    expect(tileVars["--signal-accent-tint"]).toBe("rgba(56, 189, 248, 0.16)");
    expect(tileVars["--signal-accent-border"]).toBe("rgba(56, 189, 248, 0.45)");
    expect(tileVars["--signal-alert-tint"]).toBe("rgba(239, 68, 68, 0.2)");
    expect(tileVars["--signal-alert-border"]).toBe("rgba(248, 113, 113, 0.5)");

    // Verify controls inside .nvr-cam-tile maintain >= 12:1 text contrast instead of 1.10:1 white-on-white
    expect(
      wcagContrastRatio(tileVars["--text-primary"], tileVars["--bg-input"])
    ).toBeGreaterThanOrEqual(14);
    expect(
      wcagContrastRatio(tileVars["--text-primary"], tileVars["--bg-panel"])
    ).toBeGreaterThanOrEqual(12);

    // High-contrast tile overrides
    const hcTileVars = extractSelectorCssVariables(
      css,
      '[data-high-contrast="true"] .nvr-cam-tile'
    );
    expect(hcTileVars["--text-primary"]).toBe("#ffffff");
    expect(hcTileVars["--text-secondary"]).toBe("#e2e8f0");
    expect(hcTileVars["--border-hairline"]).toBe("#94a3b8");
    expect(hcTileVars["--border-strong"]).toBe("#ffffff");
    expect(hcTileVars["--border-input"]).toBe("#ffffff");
    expect(hcTileVars["--bg-input"]).toBe("#000000");
    expect(hcTileVars["--badge-surface"]).toBe("rgba(0, 0, 0, 0.94)");
    expect(
      wcagContrastRatio(hcTileVars["--text-primary"], hcTileVars["--bg-input"])
    ).toBeCloseTo(21, 1);

    // WCAG AA >= 4.5:1 on .nvr-tabbar-badge (#ffffff on #dc2626 = 4.83:1)
    expect(wcagContrastRatio("#ffffff", "#dc2626")).toBeGreaterThanOrEqual(4.5);
    expect(css).toMatch(/\.nvr-tabbar-badge\s*\{[\s\S]*?background:\s*#dc2626;/);

    // Active settings nav subtitle promoted to --text-secondary and light-surface #38bdf8 eliminated
    expect(css).toMatch(
      /\.nvr-settings-nav-item\.active\s*\{[\s\S]*?--text-muted:\s*var\(--text-secondary\);/
    );
    expect(css).toMatch(
      /\.nvr-zs-tag-ruler\s*\{[^}]*color:\s*var\(--signal-accent-text\);/
    );
    expect(css).toMatch(
      /\.nvr-zs-readout-text strong\s*\{[^}]*color:\s*var\(--signal-accent-text\);/
    );
  });
});

describe("Task T3 Audit — [UX-P0-4 & ARCH-P2-10] SyncWorkspace Master Transport & VodHlsPlayer Improvements", () => {
  test("SyncWorkspace renders Play/Pause All, Step -1f/+1f All, Audio Mute/Unmute, and reconciles >0.45s drift", () => {
    const now = 1_700_010_000;
    expect(clampSyncPlayheadTs(now - 30.125, now)).toBe(now - 30.125);
    expect(advanceSyncClockTick(now - 50, 500, 2, now)).toBe(now - 49);
    expect(shouldReconcileSyncDrift(100.2, 100.0, SYNC_DRIFT_THRESHOLD_SEC)).toBe(
      false
    );
    expect(shouldReconcileSyncDrift(100.6, 100.0, SYNC_DRIFT_THRESHOLD_SEC)).toBe(
      true
    );

    useNvrStore.setState({ selectedCamera: "north_gate", syncQuadCameras: [] });
    const html = renderToStaticMarkup(
      React.createElement(SyncWorkspace, {
        cameraNames: ["north_gate", "south_gate", "lobby", "dock"],
        syncMasterTs: now - 90,
        setSyncMasterTs: () => {},
        syncSpeed: 1,
        setSyncSpeed: () => {},
        reviews: [],
        focusSegments: [],
        handleQuickExport: async () => {},
      })
    );

    expect(html).toContain('data-testid="sync-play-pause"');
    expect(html).toContain('data-testid="sync-step-prev"');
    expect(html).toContain('data-testid="sync-step-next"');
    expect(html).toContain('data-testid="sync-audio-toggle"');
    expect(html).toContain('data-testid="sync-master-clock"');
  });

  test("VodHlsPlayer quantizes non-clip HLS windows to 60s boundaries, steps by actual camera FPS, and captures server-side frames", async () => {
    // 1. 60s window quantization avoids tearing down Hls() on small nudges
    const w1 = quantizeVodWindowBounds(1_700_000_015, 1_700_000_135);
    const w2 = quantizeVodWindowBounds(1_700_000_025, 1_700_000_145);
    expect(w1.safeStart).toBe(w2.safeStart);
    expect(w1.safeEnd).toBe(w2.safeEnd);
    expect(w1.safeStart % 60).toBe(0);
    expect(w1.safeEnd % 60).toBe(0);

    // Explicit tight clip bounds bypass 60s rounding
    const tight = quantizeVodWindowBounds(1_700_000_015, 1_700_000_045, {
      isTightClip: true,
    });
    expect(tight.safeStart).toBe(1_700_000_015);
    expect(tight.safeEnd).toBe(1_700_000_045);

    // 2. Actual camera FPS frame step duration
    expect(resolveVodFrameStepDurationSec(makeSampleCameraConfig(25))).toBeCloseTo(
      0.04,
      5
    );
    expect(resolveVodFrameStepDurationSec(null)).toBeCloseTo(1 / 15, 5);

    // 3. Server-side frame capture via GET /api/:cam/frame?ts=:sec (privacy-mask safe) + offline canvas fallback
    const fetchedUrls: string[] = [];
    let clickedDownload = "";
    const mockDoc = {
      createElement: (tag: string) => {
        if (tag === "a") {
          const anchor = {
            download: "",
            href: "",
            click() {
              clickedDownload = anchor.download;
            },
          };
          return anchor;
        }
        return {
          width: 0,
          height: 0,
          getContext: () => ({ drawImage() {} }),
          toDataURL: () => "data:image/png;base64,AAA",
        };
      },
    } as unknown as Document;

    const serverRes = await captureVodStillFrame({
      camera: "front gate",
      realCamera: "front gate",
      captureEpochSec: 1700000123.456,
      doc: mockDoc,
      fetchImpl: (async (input: RequestInfo | URL) => {
        fetchedUrls.push(String(input));
        return new Response(new Uint8Array([0xff, 0xd8, 0xff]), {
          status: 200,
          headers: { "Content-Type": "image/jpeg" },
        });
      }) as typeof fetch,
    });
    expect(serverRes.source).toBe("server");
    expect(fetchedUrls).toEqual(["/api/front%20gate/frame?ts=1700000123.456"]);
    expect(clickedDownload).toBe("front gate_1700000123.jpg");

    // Offline fallback to canvas only when fetch fails
    const fallbackRes = await captureVodStillFrame({
      camera: "cam_offline",
      realCamera: "cam_offline",
      captureEpochSec: 1700000200,
      videoEl: { videoWidth: 1280, videoHeight: 720 } as HTMLVideoElement,
      doc: mockDoc,
      fetchImpl: (async () => {
        throw new Error("network offline");
      }) as typeof fetch,
    });
    expect(fallbackRes.source).toBe("canvas");
    expect(fallbackRes.filename).toBe("cam_offline_1700000200.png");
  });
});

describe("Task T3 Audit — [ARCH-P1-7 & ARCH-P2-9] Reactive /ws, Config Stability, No 'escritorio', Lazy Chunks & History", () => {
  test("eliminates 5s polling storm, reconciles review new/update/end over /ws, and preserves setConfig referential stability", () => {
    expect(resolveCoreDataPollIntervalMs(true, false)).toBe(WS_HEALTHY_HEARTBEAT_MS);
    expect(WS_HEALTHY_HEARTBEAT_MS).toBe(60_000);
    expect(resolveCoreDataPollIntervalMs(false, false)).toBe(WS_FALLBACK_POLL_MS);
    expect(resolveCoreDataPollIntervalMs(true, true)).toBe(WS_FALLBACK_POLL_MS);
    expect(WS_FALLBACK_POLL_MS).toBe(10_000);

    expect(normalizeWsTopic("ninjanvr/stats")).toBe("stats");
    expect(normalizeWsTopic("reviews")).toBe("reviews");

    const openAlert: ReviewItem = {
      id: "rev_live_1",
      camera: "driveway",
      start_time: 1700000100,
      end_time: null,
      severity: "alert",
      thumb_path: "/clips/driveway.webp",
      has_been_reviewed: false,
      data: { objects: ["person"] },
    };
    const list1 = upsertBoundedReviewList([], openAlert);
    expect(list1.length).toBe(1);
    expect(list1[0].end_time).toBeNull();

    // Update & close ("end") mutate the existing review in-place without duplicating
    const closedAlert: ReviewItem = {
      ...openAlert,
      end_time: 1700000118,
      data: { objects: ["person", "car"], sub_labels: ["Alice"] },
    };
    const list2 = upsertBoundedReviewList(list1, closedAlert);
    expect(list2.length).toBe(1);
    expect(list2[0].end_time).toBe(1700000118);
    expect(list2[0].data.sub_labels).toEqual(["Alice"]);

    // Referential stability in setConfig
    const cfgA: NinjaConfigResponse = {
      version: "1.0.0",
      safe_mode: false,
      ui: { theme: "dark" },
      cameras: { driveway: makeSampleCameraConfig(15) },
    };
    const cfgAClone: NinjaConfigResponse = JSON.parse(JSON.stringify(cfgA));
    useNvrStore.getState().setConfig(cfgA);
    const ref1 = useNvrStore.getState().config;
    const camsRef1 = ref1?.cameras;

    // Identical payload keeps exact same object reference
    useNvrStore.getState().setConfig(cfgAClone);
    expect(useNvrStore.getState().config).toBe(ref1);

    // Changing only ui.theme preserves config.cameras reference so ONVIF/ffprobe effects do not re-fire
    useNvrStore.getState().setConfig({ ...cfgAClone, ui: { theme: "light" } });
    expect(useNvrStore.getState().config?.cameras).toBe(camsRef1);
  });

  test("removes hardcoded 'escritorio' defaults, localizes English simulated groups, keeps persistent stage in map mode, and enforces 2-step confirm", async () => {
    useNvrStore.setState({
      selectedCamera: "",
      simulatedFleetCount: 0,
      config: null,
    });
    expect(useNvrStore.getState().selectedCamera).toBe("");
    expect(resolveRealCameraSource("", null)).toBe("");
    expect(resolveValidCameraSelection("", null, 0)).toBe("");
    expect(expandSimulatedFleet([], null, null, 0).cameraNames).toEqual([]);

    // English locale simulated fleet uses English group names ("Indoor", "Perimeter", ...)
    const simEn = expandSimulatedFleet(["gate"], null, null, 12, "en");
    expect(Object.keys(simEn.cameraGroups)).toEqual([...SIMULATED_FLEET_GROUPS_EN]);
    const simPt = expandSimulatedFleet(["gate"], null, null, 12, "pt");
    expect(Object.keys(simPt.cameraGroups)).toEqual([...SIMULATED_FLEET_GROUPS]);

    // Empty state when config has not loaded yet (zero bogus /live/mse/api/ws?src=escritorio_sub)
    const emptyLiveHtml = renderToStaticMarkup(
      React.createElement(LiveWorkspace, {
        cameraNames: [],
        reviews: [],
        unreviewedAlerts: [],
        focusSegments: [],
        handleMarkReviews: async () => {},
        handleQuickExport: async () => {},
      })
    );
    expect(emptyLiveHtml).toContain('data-testid="live-empty-state"');
    expect(emptyLiveHtml).not.toContain("escritorio");

    // In Tactical Map mode, .nvr-persistent-stage stays mounted in DOM with `hidden` + `display:none`
    const cfg: NinjaConfigResponse = {
      version: "1.0.0",
      safe_mode: false,
      cameras: { front_door: makeSampleCameraConfig(15) },
    };
    useNvrStore.getState().setConfig(cfg);
    expect(useNvrStore.getState().selectedCamera).toBe("front_door");
    useNvrStore.setState({ liveLayout: "map" });

    const mapModeHtml = withSyncedServerStoreSnapshot(() =>
      renderToStaticMarkup(
        React.createElement(LiveWorkspace, {
          cameraNames: ["front_door"],
          reviews: [],
          unreviewedAlerts: [],
          focusSegments: [],
          handleMarkReviews: async () => {},
          handleQuickExport: async () => {},
        })
      )
    );
    expect(mapModeHtml).toContain('data-testid="live-persistent-stage"');
    expect(mapModeHtml).toContain("display:none");

    // 2-step confirmation controller arms on 1st click, confirms on 2nd click, and expires on timeout
    let timeoutCb: (() => void) | null = null;
    const confirmCtrl = createTwoStepConfirmController(3000, undefined, {
      setTimeout: (cb) => {
        timeoutCb = cb;
        return 1;
      },
      clearTimeout: () => {
        timeoutCb = null;
      },
    });
    expect(confirmCtrl.triggerOrArm("trigger_siren")).toBe(false);
    expect(confirmCtrl.isArmed("trigger_siren")).toBe(true);
    expect(confirmCtrl.triggerOrArm("trigger_siren")).toBe(true);
    expect(confirmCtrl.isArmed("trigger_siren")).toBe(false);

    expect(confirmCtrl.triggerOrArm("reboot_camera")).toBe(false);
    expect(timeoutCb).not.toBeNull();
    const fireTimeout = timeoutCb as (() => void) | null;
    fireTimeout?.();
    expect(confirmCtrl.isArmed("reboot_camera")).toBe(false);
    expect(confirmCtrl.triggerOrArm("reboot_camera")).toBe(false);

    // App.tsx uses React.lazy for secondary workspaces, pushState/popstate, and removes Review key collision
    const appSrc = await readSrcText("../App.tsx");
    expect(appSrc).toContain('React.lazy(() =>');
    expect(appSrc).toContain("window.history.pushState");
    expect(appSrc).toContain('window.addEventListener("popstate"');
    expect(appSrc).not.toContain('if (activeTab === "review") {\n        const key');
    expect(Object.keys(WORKSPACE_TAB_URL_MAP).every(isWorkspaceTab)).toBe(true);
  });

  test("TacticalPtzHudOverlay exposes Delete Preset with 2-step confirmation attributes", () => {
    const mockHw: CameraHardwareInfoResponse = {
      camera: "ptz_1",
      online: true,
      protocol_family: "onvif",
      onvif_configured: true,
      device_info: {},
      capabilities: {
        ptz: true,
        ptz_zoom: true,
        ptz_presets: true,
        ptz_presets_manage: true,
        push_to_talk: false,
        imaging: false,
        hardware_autotracking: false,
        day_night_mode: false,
        ir_lighting: false,
        status_led: false,
        flip_mirror: false,
        wdr: false,
        siren_on_event: false,
        manual_siren: true,
        audio_anomaly_detect: false,
        audio_volume: false,
        reboot: true,
      },
      missing_requirements: [],
      state: {},
    };
    const html = renderToStaticMarkup(
      React.createElement(TacticalPtzHudOverlay, {
        camera: "ptz_1",
        ptzInfo: {
          enabled: true,
          online: true,
          active_preset: "gate",
          presets: [{ token: "1", name: "gate" }],
          features: ["pt", "zoom", "presets_manage"],
        },
        hardwareInfo: mockHw,
      })
    );
    expect(html).toContain('data-confirming="false"');
    expect(html).toContain('aria-label="Delete PTZ Preset ptz_1"');
  });
});

describe("Task T3 Audit — [UX-P1-10, UX-P1-11, UX-P1-12] Sheet Focus Trap, Slider A11y, Un-nested Buttons & <html lang>", () => {
  test("useSheetDismiss traps Tab and Shift+Tab inside focusable sheet elements", () => {
    let focusedId = "btn_first";
    const makeNode = (id: string, opts?: { disabled?: boolean; hidden?: boolean }) => ({
      id,
      hasAttribute: (attr: string) => (attr === "hidden" ? Boolean(opts?.hidden) : false),
      getAttribute: (attr: string) =>
        attr === "disabled" && opts?.disabled ? "true" : null,
      closest: () => null,
      focus: () => {
        focusedId = id;
      },
    });

    const first = makeNode("btn_first") as unknown as HTMLElement;
    const middle = makeNode("btn_mid") as unknown as HTMLElement;
    const hiddenNode = makeNode("btn_hidden", { hidden: true }) as unknown as HTMLElement;
    const last = makeNode("btn_last") as unknown as HTMLElement;

    const container = {
      querySelectorAll: () => [first, middle, hiddenNode, last],
      contains: (el: unknown) => el === first || el === middle || el === last,
      focus: () => {
        focusedId = "container";
      },
    } as unknown as HTMLElement;

    const focusables = getFocusableSheetElements(container);
    expect(focusables).toEqual([first, middle, last]);

    // Forward Tab from last element wraps to first element
    let prevented = false;
    const wrappedFwd = trapSheetTabNavigation(
      {
        key: "Tab",
        shiftKey: false,
        preventDefault: () => {
          prevented = true;
        },
      },
      container,
      last
    );
    expect(wrappedFwd).toBe(true);
    expect(prevented).toBe(true);
    expect(focusedId).toBe("btn_first");

    // Shift+Tab from first element wraps to last element
    prevented = false;
    const wrappedBack = trapSheetTabNavigation(
      {
        key: "Tab",
        shiftKey: true,
        preventDefault: () => {
          prevented = true;
        },
      },
      container,
      first
    );
    expect(wrappedBack).toBe(true);
    expect(prevented).toBe(true);
    expect(focusedId).toBe("btn_last");

    // Tab from middle element lets browser advance naturally
    expect(
      trapSheetTabNavigation(
        { key: "Tab", shiftKey: false, preventDefault: () => {} },
        container,
        middle
      )
    ).toBe(false);
  });

  test("TimelineScrubber exposes keyboard ARIA slider semantics on playhead track and IN/OUT trim handles", () => {
    expect(computeTimelineSliderKeyStep("track", "ArrowLeft", false)).toBe(-5);
    expect(computeTimelineSliderKeyStep("track", "ArrowRight", false)).toBe(5);
    expect(computeTimelineSliderKeyStep("track", "ArrowLeft", true)).toBe(-30);
    expect(computeTimelineSliderKeyStep("track", "ArrowRight", true)).toBe(30);
    expect(computeTimelineSliderKeyStep("in", "ArrowLeft", false)).toBe(-1);
    expect(computeTimelineSliderKeyStep("in", "ArrowRight", true)).toBe(0.25);
    expect(computeTimelineSliderKeyStep("out", "ArrowLeft", true)).toBe(-0.25);
    expect(computeTimelineSliderKeyStep("out", "Enter", false)).toBeNull();

    const nowSec = Math.floor(Date.now() / 1000);
    const html = renderToStaticMarkup(
      React.createElement(TimelineScrubber, {
        camera: "front_door",
        cameras: ["front_door"],
        segments: [],
        reviews: [],
        currentTs: nowSec - 60,
        onSeek: () => {},
        onExportRange: async () => {},
        defaultRangeMode: true,
      })
    );

    const sliderMatches = html.match(/role="slider"/g) || [];
    expect(sliderMatches.length).toBe(3); // Playhead track + IN handle + OUT handle
    expect(html).toContain('aria-label="Timeline playhead for front_door"');
    expect(html).toContain('aria-label="Clip start (IN) trim handle for front_door"');
    expect(html).toContain('aria-label="Clip end (OUT) trim handle for front_door"');
    expect(html).toContain("aria-valuemin=");
    expect(html).toContain("aria-valuemax=");
    expect(html).toContain("aria-valuenow=");
    expect(html).toContain("aria-valuetext=");
  });

  test("LiveWorkspace and ExportsWorkspace un-nest <div role='button'> containers and sync <html lang>", async () => {
    const prevDoc = (globalThis as unknown as { document?: unknown }).document;
    const prevWin = (globalThis as unknown as { window?: unknown }).window;
    const attrs = new Map<string, string>();
    const rootEl = {
      lang: "",
      setAttribute: (k: string, v: string) => attrs.set(k, v),
    };
    const storage = new Map<string, string>([["ninjanvr_locale", "pt"]]);
    (globalThis as unknown as { document: unknown }).document = {
      documentElement: rootEl,
    };
    (globalThis as unknown as { window: unknown }).window = {
      localStorage: {
        getItem: (k: string) => storage.get(k) ?? null,
        setItem: (k: string, v: string) => storage.set(k, v),
      },
    };
    try {
      expect(resolveHtmlLang("en")).toBe("en");
      expect(resolveHtmlLang("pt")).toBe("pt-BR");
      expect(applyLocaleToDocument("en")).toBe("en");
      expect(rootEl.lang).toBe("en");
      expect(attrs.get("lang")).toBe("en");

      // Mounting I18nProvider with saved "pt" locale sets <html lang="pt-BR">
      renderToStaticMarkup(
        React.createElement(I18nProvider, null, React.createElement("span", null, "ok"))
      );
      expect(rootEl.lang).toBe("pt-BR");
      expect(attrs.get("lang")).toBe("pt-BR");
    } finally {
      (globalThis as unknown as { document?: unknown }).document = prevDoc;
      (globalThis as unknown as { window?: unknown }).window = prevWin;
    }

    // Verify 100% dictionary key parity across all sections
    for (const sec of Object.keys(locales.en) as Array<keyof typeof locales.en>) {
      expect(Object.keys(locales.en[sec]).sort()).toEqual(
        Object.keys(locales.pt[sec]).sort()
      );
    }

    // Verify ExportsWorkspace completed export rows have zero <div role="button"> wrapping child buttons
    const expItem: ExportItem = {
      id: "front_door_1000_1060",
      camera: "front_door",
      name: "Gate Clip",
      date: 1700001000,
      video_path: "/exports/front_door_1000_1060.mp4",
      thumb_path: "/clips/front_door_1000_1060.webp",
      in_progress: false,
    };
    const expHtml = renderToStaticMarkup(
      React.createElement(ExportsWorkspace, {
        cameraNames: ["front_door"],
        config: null,
        exportsList: [expItem],
        setExportsList: () => {},
        exportCam: "front_door",
        setExportCam: () => {},
        activeExportPreview: null,
        setActiveExportPreview: () => {},
        timeShiftTs: 1700001000,
        setTimeShiftTs: () => {},
        focusSegments: [],
        reviews: [],
        handleQuickExport: async () => {},
        notify: () => {},
      })
    );
    expect(expHtml).not.toContain('role="button"');
    expect(expHtml).toContain("nvr-row-primary-btn");

    // Verify mobile CSS flexible .nvr-btn height & >= 44px coarse/mobile targets + horizontal settings rail
    const css = await readSrcText("../styles.css");
    expect(css).toMatch(/\.nvr-btn\s*\{[\s\S]*?min-block-size:\s*28px;[\s\S]*?height:\s*auto;/);
    expect(css).toMatch(/@media \(pointer: coarse\)[\s\S]*?min-block-size:\s*44px;/);
    expect(css).toMatch(/@media \(max-width: 767px\)[\s\S]*?\.nvr-settings-rail\s*\{[\s\S]*?flex-direction:\s*row;/);
  });
});
