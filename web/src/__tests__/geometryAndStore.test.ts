import { beforeEach, describe, expect, test } from "bun:test";
import {
  computeContainedBox,
  coordStringToPoints,
  extractMaskPolygons,
  parsePolygonPoints,
} from "../components/VideoPlayers";
import { en } from "../i18n/en";
import { locales, pt, resolveInitialLocale } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";

describe("polygon and geometry helpers", () => {
  test("extractMaskPolygons handles strings, arrays, and nested mask objects", () => {
    expect(extractMaskPolygons(null)).toEqual([]);
    expect(extractMaskPolygons(" 0.1,0.1,0.5,0.5 ")).toEqual(["0.1,0.1,0.5,0.5"]);
    expect(
      extractMaskPolygons([
        "0,0,1,0,1,1",
        { coordinates: "0.2,0.2,0.8,0.8" },
        { invalid: true },
      ])
    ).toEqual(["0,0,1,0,1,1", "0.2,0.2,0.8,0.8"]);
    expect(
      extractMaskPolygons({
        mask_a: "0.1,0.2,0.3,0.4",
        mask_b: { coordinates: "0.5,0.5,0.9,0.9", enabled: true },
      })
    ).toEqual(["0.1,0.2,0.3,0.4", "0.5,0.5,0.9,0.9"]);
  });

  test("coordStringToPoints and parsePolygonPoints normalize both 0..1 and pixel coordinates", () => {
    const normPts = coordStringToPoints("0.1, 0.2, 0.5, 0.8", 1280, 720);
    expect(normPts).toEqual([
      [0.1, 0.2],
      [0.5, 0.8],
    ]);

    const pxPts = coordStringToPoints("640,360,1280,720", 1280, 720);
    expect(pxPts).toEqual([
      [0.5, 0.5],
      [1, 1],
    ]);

    const svgPoints = parsePolygonPoints("0.25,0.5,640,720", 1280, 720);
    expect(svgPoints).toBe("250,500 500,1000");
  });

  test("computeContainedBox preserves aspect ratio for pillarboxed and letterboxed stages", () => {
    const pillar = computeContainedBox(1000, 500, 1280, 720);
    expect(pillar.height).toBe(500);
    expect(Math.round(pillar.width)).toBe(889);
    expect(Math.round(pillar.left)).toBe(56);
    expect(pillar.top).toBe(0);

    const letter = computeContainedBox(1280, 1000, 1280, 720);
    expect(letter.width).toBe(1280);
    expect(letter.height).toBe(720);
    expect(letter.left).toBe(0);
    expect(letter.top).toBe(140);

    const zero = computeContainedBox(0, 0, 1280, 720);
    expect(zero).toEqual({ left: 0, top: 0, width: 0, height: 0 });
  });
});

describe("useNvrStore state transitions and i18n parity", () => {
  beforeEach(() => {
    const store = new Map<string, string>();
    (globalThis as unknown as { window: unknown }).window = {
      location: { pathname: "/" },
      localStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => store.set(k, v),
        removeItem: (k: string) => store.delete(k),
      },
    };
    useNvrStore.getState().resetUiPreferences();
  });

  test("updates store slices, toggles functional setters, and resets preferences", () => {
    const s = useNvrStore.getState();
    s.setActiveTab("review");
    s.setLiveLayout("focus");
    s.setActiveCameraGroup("perimeter");
    s.setMatrixDensity("4x4");
    s.setMatrixPage(3);
    s.setMatrixTourEnabled((prev) => !prev);
    s.setMatrixTourIntervalSec(20);
    s.setSyncQuadCameras(["cam1", "cam2", "cam3", "cam4", "cam5"]);
    s.setSpotlightFilter("person");
    s.setSpotlightHoldSec(30);
    s.setSpotlightTimelineMode("wall");
    s.setSecurityProfile("home");
    s.setSelectedCamera("front_door");
    s.setHudPosition("top-right");
    s.setUseMainStream((prev) => !prev);
    s.setShowVectors((prev) => !prev);
    s.setShowAlertRibbon(false);
    s.setAudibleAlertChime(true);
    s.setBrowserNotifications(true);
    s.setFirstWeekday("monday");
    s.setTimeShiftTs(1700000100);
    s.setPlaybackSpeed(2);
    s.setWsOnline(true);
    const mockStats = {
      cameras: {},
      detectors: {},
      camera_fps: 5,
      process_fps: 5,
      skipped_fps: 0,
      detection_fps: 1,
      cpu_usages: {},
      service: {
        uptime: 120,
        version: "1.1.2",
        latest_version: "1.1.2",
        storage: {},
        last_updated: 1700000100,
      },
      processes: {},
    };
    s.setConfig({ version: "0.18-0", safe_mode: false, cameras: {} });
    s.setStats(mockStats);
    s.setStatsHistory([mockStats]);
    s.setCameraActivity({
      front_door: {
        motion: true,
        objects: [{ id: "1", label: "person", score: 0.91, stationary: false }],
      },
    });

    const updated = useNvrStore.getState();
    expect(updated.activeTab).toBe("review");
    expect(updated.liveLayout).toBe("focus");
    expect(updated.activeCameraGroup).toBe("perimeter");
    expect(updated.matrixDensity).toBe("4x4");
    expect(updated.matrixPage).toBe(3);
    expect(updated.matrixTourEnabled).toBe(true);
    expect(updated.matrixTourIntervalSec).toBe(20);
    expect(updated.syncQuadCameras).toEqual(["cam1", "cam2", "cam3", "cam4"]);
    expect(updated.spotlightFilter).toBe("person");
    expect(updated.spotlightHoldSec).toBe(30);
    expect(updated.spotlightTimelineMode).toBe("wall");
    expect(
      (
        globalThis as unknown as {
          window: { localStorage: { getItem: (k: string) => string | null } };
        }
      ).window.localStorage.getItem("ninjanvr_spotlight_timeline_mode")
    ).toBe("wall");
    expect(updated.securityProfile).toBe("home");
    expect(updated.selectedCamera).toBe("front_door");
    expect(updated.hudPosition).toBe("top-right");
    expect(updated.useMainStream).toBe(true);
    expect(updated.showVectors).toBe(true);
    expect(updated.showAlertRibbon).toBe(false);
    expect(updated.audibleAlertChime).toBe(true);
    expect(updated.browserNotifications).toBe(true);
    expect(updated.firstWeekday).toBe("monday");
    expect(updated.timeShiftTs).toBe(1700000100);
    expect(updated.playbackSpeed).toBe(2);
    expect(updated.wsOnline).toBe(true);
    expect(updated.config?.version).toBe("0.18-0");
    expect(updated.stats?.service?.uptime).toBe(120);
    expect(updated.statsHistory.length).toBe(1);
    expect(updated.cameraActivity.front_door?.motion).toBe(true);

    updated.resetUiPreferences();
    const reset = useNvrStore.getState();
    expect(reset.liveLayout).toBe("2x2");
    expect(reset.spotlightTimelineMode).toBe("reactive");
    expect(
      (
        globalThis as unknown as {
          window: { localStorage: { getItem: (k: string) => string | null } };
        }
      ).window.localStorage.getItem("ninjanvr_spotlight_timeline_mode")
    ).toBeNull();
    expect(reset.hudPosition).toBe("bottom-right");
    expect(reset.useMainStream).toBe(false);
    expect(reset.showVectors).toBe(false);
    expect(reset.showAlertRibbon).toBe(true);
    expect(reset.audibleAlertChime).toBe(false);
    expect(reset.browserNotifications).toBe(false);
    expect(reset.firstWeekday).toBe("sunday");
    expect(reset.playbackSpeed).toBe(1);
  });

  test("i18n locales maintain 100% section and key parity between en and pt", () => {
    expect(Object.keys(locales.en).sort()).toEqual(Object.keys(locales.pt).sort());
    for (const section of Object.keys(en) as Array<keyof typeof en>) {
      expect(Object.keys(en[section]).sort()).toEqual(Object.keys(pt[section]).sort());
    }

    expect(resolveInitialLocale()).toBe("en");
    (
      globalThis as unknown as {
        window: { localStorage: { setItem: (k: string, v: string) => void } };
      }
    ).window.localStorage.setItem("ninjanvr_locale", "pt");
    expect(resolveInitialLocale()).toBe("pt");
  });
});
