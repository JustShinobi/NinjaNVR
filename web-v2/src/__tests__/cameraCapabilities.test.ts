import { beforeEach, describe, expect, test } from "bun:test";
import {
  HARDWARE_CONFIG_ONLY_PATCH_KEYS,
  applyDeletedPresetToPtzInfo,
  applyPresetDeletedToPtzInfo,
  applyPresetSavedToPtzInfo,
  applySavedPresetToPtzInfo,
  cameraHardwareCache,
  createCameraHardwareCacheController,
  getCachedCameraHardware,
  getCachedCameraPtzInfo,
  invalidateCameraHardwareCache,
  isConfigPersistenceHardwarePatch,
  mergeOptimisticCameraHardwareInfo,
  probeCameraHardwareAndPtz,
  resolveCameraCapabilitySnapshot,
  setCachedCameraHardware,
  setCachedCameraPtzInfo,
  subscribeCameraHardwareCache,
} from "../utils/cameraCapabilities";
import type {
  CameraConfig,
  CameraHardwareCapabilities,
  CameraHardwareInfoResponse,
  CameraPtzInfoResponse,
} from "../types/nvr";

function makeBaseCapabilities(
  overrides: Partial<CameraHardwareCapabilities> = {}
): CameraHardwareCapabilities {
  return {
    ptz: true,
    ptz_zoom: false,
    ptz_presets: true,
    ptz_presets_manage: true,
    push_to_talk: false,
    push_to_talk_mode: null,
    imaging: true,
    hardware_autotracking: true,
    day_night_mode: true,
    ir_lighting: true,
    status_led: true,
    flip_mirror: true,
    wdr: true,
    siren_on_event: true,
    manual_siren: true,
    audio_anomaly_detect: false,
    audio_volume: true,
    reboot: true,
    ...overrides,
  };
}

function makeBaseHardwareInfo(
  overrides: Partial<CameraHardwareInfoResponse> = {}
): CameraHardwareInfoResponse {
  return {
    camera: "front_porch",
    online: true,
    protocol_family: "tapo",
    onvif_configured: false,
    device_info: {
      manufacturer: "TP-Link",
      model: "Tapo C210",
      firmware: "1.3.0",
    },
    capabilities: makeBaseCapabilities(),
    missing_requirements: ["onvif_not_configured", "tapo_cloud_password"],
    state: {
      brightness: 50,
      contrast: 50,
      status_led: true,
      day_night_mode: "auto",
      speaker_volume: 80,
      flip: false,
      mirror: false,
    },
    ...overrides,
  };
}

describe("mergeOptimisticCameraHardwareInfo and HARDWARE_CONFIG_ONLY_PATCH_KEYS", () => {
  test("never leaks HARDWARE_CONFIG_ONLY_PATCH_KEYS into hardware state while merging imaging, LED, night-mode, and volume patches", () => {
    const prev = makeBaseHardwareInfo();
    const patch: Record<string, unknown> = {
      apply_suggested_onvif: true,
      apply_suggested_substream: true,
      cloud_password: "secret-cloud-pw",
      protocol: "tapo",
      profile_token: "profile_1",
      video_source_token: "vs_1",
      reboot: true,
      brightness: 75,
      status_led: false,
      day_night_mode: "night",
      speaker_volume: 95,
      wdr: true,
    };

    const merged = mergeOptimisticCameraHardwareInfo(prev, patch);
    expect(merged).not.toBeNull();
    expect(merged!.onvif_configured).toBe(true);
    expect(merged!.missing_requirements).toEqual([]);
    expect(merged!.state.brightness).toBe(75);
    expect(merged!.state.status_led).toBe(false);
    expect(merged!.state.day_night_mode).toBe("night");
    expect(merged!.state.speaker_volume).toBe(95);
    expect(merged!.state.wdr).toBe(true);
    expect(merged!.state.contrast).toBe(50);

    const stateRecord = merged!.state as Record<string, unknown>;
    for (const configKey of HARDWARE_CONFIG_ONLY_PATCH_KEYS) {
      expect(stateRecord[configKey]).toBeUndefined();
    }
  });

  test("merges server responseData overrides cleanly without polluting top-level response envelope keys", () => {
    const prev = makeBaseHardwareInfo();
    const merged = mergeOptimisticCameraHardwareInfo(
      prev,
      { brightness: 65, status_led: false },
      {
        // Simulate server returning authoritative clamped brightness and updated capabilities
        capabilities: makeBaseCapabilities({
          push_to_talk: true,
          push_to_talk_mode: "webrtc",
        }),
        state: {
          brightness: 60,
          sharpness: 70,
        },
      }
    );

    expect(merged).not.toBeNull();
    expect(merged!.state.brightness).toBe(60);
    expect(merged!.state.status_led).toBe(false);
    expect(merged!.state.sharpness).toBe(70);
    expect(merged!.capabilities.push_to_talk).toBe(true);
    expect(merged!.capabilities.push_to_talk_mode).toBe("webrtc");
  });

  test("returns null when prev is null/undefined and responseData is incomplete, and detects config persistence patches", () => {
    expect(
      mergeOptimisticCameraHardwareInfo(null, { brightness: 80 })
    ).toBeNull();
    expect(
      mergeOptimisticCameraHardwareInfo(undefined, { status_led: false })
    ).toBeNull();

    expect(
      isConfigPersistenceHardwarePatch({ apply_suggested_onvif: true })
    ).toBe(true);
    expect(
      isConfigPersistenceHardwarePatch({ apply_suggested_substream: true })
    ).toBe(true);
    expect(
      isConfigPersistenceHardwarePatch({ cloud_password: "secret" })
    ).toBe(true);
    expect(isConfigPersistenceHardwarePatch({ brightness: 50 })).toBe(false);
  });
});

describe("applySavedPresetToPtzInfo and applyDeletedPresetToPtzInfo", () => {
  test("saves new presets, deduplicates existing tokens/names, maps serverPresetNames, and sets active_preset", () => {
    const initial: CameraPtzInfoResponse = {
      enabled: true,
      online: true,
      presets: [
        { token: "1", name: "Home" },
        { token: "2", name: "Driveway" },
      ],
      preset_names: ["Home", "Driveway"],
      active_preset: "Home",
    };

    // Add a new preset
    const afterSave = applySavedPresetToPtzInfo(initial, "3", "Gate");
    expect(afterSave).not.toBeNull();
    expect(afterSave!.presets).toEqual([
      { token: "1", name: "Home" },
      { token: "2", name: "Driveway" },
      { token: "3", name: "Gate" },
    ]);
    expect(afterSave!.preset_names).toEqual(["Home", "Driveway", "Gate"]);
    expect(afterSave!.active_preset).toBe("Gate");

    // Saving again with same token or name deduplicates in-place
    const afterDuplicate = applyPresetSavedToPtzInfo(afterSave, "3", "Gate");
    expect(afterDuplicate!.presets?.length).toBe(3);
    expect(afterDuplicate!.preset_names).toEqual(["Home", "Driveway", "Gate"]);

    // Server-authoritative preset tokens + serverPresetNames map
    const fromServer = applySavedPresetToPtzInfo(
      afterDuplicate,
      "4",
      "Porch",
      ["1", "2", "3", "4", "4"],
      {
        "1": "Home",
        "2": "Driveway",
        "3": "Front Gate",
        "4": "Porch",
      }
    );
    expect(fromServer!.presets).toEqual([
      { token: "1", name: "Home" },
      { token: "2", name: "Driveway" },
      { token: "3", name: "Front Gate" },
      { token: "4", name: "Porch" },
    ]);
    expect(fromServer!.preset_names).toEqual([
      "Home",
      "Driveway",
      "Front Gate",
      "Porch",
    ]);
    expect(fromServer!.active_preset).toBe("Porch");
  });

  test("deletes presets by token or name, clears active_preset when active is deleted, and preserves active_preset otherwise", () => {
    const start: CameraPtzInfoResponse = {
      enabled: true,
      online: true,
      presets: [
        { token: "1", name: "Home" },
        { token: "2", name: "Driveway" },
        { token: "3", name: "Gate" },
      ],
      preset_names: ["Home", "Driveway", "Gate"],
      active_preset: "Driveway",
    };

    // Delete non-active preset "Gate" (by token "3") -> active_preset remains "Driveway"
    const afterNonActiveDelete = applyDeletedPresetToPtzInfo(start, "3");
    expect(afterNonActiveDelete!.presets).toEqual([
      { token: "1", name: "Home" },
      { token: "2", name: "Driveway" },
    ]);
    expect(afterNonActiveDelete!.preset_names).toEqual(["Home", "Driveway"]);
    expect(afterNonActiveDelete!.active_preset).toBe("Driveway");

    // Delete active preset "Driveway" by its token "2" -> active_preset is cleared to null
    const afterActiveDelete = applyPresetDeletedToPtzInfo(
      afterNonActiveDelete,
      "2"
    );
    expect(afterActiveDelete!.presets).toEqual([{ token: "1", name: "Home" }]);
    expect(afterActiveDelete!.preset_names).toEqual(["Home"]);
    expect(afterActiveDelete!.active_preset).toBeNull();

    // Server-provided remaining presets & preset_names on delete
    const withServerList = applyDeletedPresetToPtzInfo(
      start,
      "Driveway",
      ["1", "3"],
      { "1": "Home", "3": "Main Gate" }
    );
    expect(withServerList!.presets).toEqual([
      { token: "1", name: "Home" },
      { token: "3", name: "Main Gate" },
    ]);
    expect(withServerList!.preset_names).toEqual(["Home", "Main Gate"]);
    expect(withServerList!.active_preset).toBeNull();

    expect(applySavedPresetToPtzInfo(null, "1", "Home")).toBeNull();
    expect(applyDeletedPresetToPtzInfo(undefined, "1")).toBeNull();
  });
});

describe("probeCameraHardwareAndPtz and cameraHardwareCache (ports & adapters seam)", () => {
  beforeEach(() => {
    invalidateCameraHardwareCache();
  });

  test("probes hardware and PTZ via injected fetchImpl adapter, caches responses, deduplicates in-flight probes, and invalidates cleanly", async () => {
    const fetchCalls: string[] = [];
    const mockHw = makeBaseHardwareInfo({
      camera: "yard_cam",
      onvif_configured: true,
      capabilities: makeBaseCapabilities({ ptz: true, ptz_zoom: true }),
    });
    const mockPtz: CameraPtzInfoResponse = {
      enabled: true,
      online: true,
      camera: "yard_cam",
      features: ["pt", "zoom", "presets_manage"],
      presets: [{ token: "p1", name: "Garden" }],
      preset_names: ["Garden"],
      active_preset: "Garden",
    };

    const mockFetch = (async (input: RequestInfo | URL) => {
      const url = String(input);
      fetchCalls.push(url);
      if (url.includes("/hardware")) {
        return new Response(JSON.stringify(mockHw), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      if (url.includes("/ptz/info")) {
        return new Response(JSON.stringify(mockPtz), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      return new Response("not found", { status: 404 });
    }) as unknown as typeof fetch;

    const events: Array<string | undefined> = [];
    const unsubscribe = subscribeCameraHardwareCache((cam) => {
      events.push(cam);
    });

    // First probe hits network for both /hardware and /ptz/info
    const first = await probeCameraHardwareAndPtz(
      "yard_cam",
      { timeoutMs: 3000 },
      mockFetch
    );
    expect(first.fromCache).toBe(false);
    expect(first.hardware?.camera).toBe("yard_cam");
    expect(first.ptz?.active_preset).toBe("Garden");
    expect(fetchCalls).toEqual([
      "/api/yard_cam/hardware?timeout_ms=3000",
      "/api/yard_cam/ptz/info",
    ]);
    expect(getCachedCameraHardware("yard_cam")?.camera).toBe("yard_cam");
    expect(getCachedCameraPtzInfo("yard_cam")?.active_preset).toBe("Garden");

    // Second probe without force returns cached snapshot without invoking mockFetch
    const second = await probeCameraHardwareAndPtz(
      "yard_cam",
      { timeoutMs: 3000 },
      mockFetch
    );
    expect(second.fromCache).toBe(true);
    expect(second.hardware?.camera).toBe("yard_cam");
    expect(second.ptz?.active_preset).toBe("Garden");
    expect(fetchCalls.length).toBe(2);

    // Invalidating camera cache forces the next probe to hit the adapter seam again
    invalidateCameraHardwareCache("yard_cam");
    expect(getCachedCameraHardware("yard_cam")).toBeNull();
    expect(getCachedCameraPtzInfo("yard_cam")).toBeNull();

    const third = await probeCameraHardwareAndPtz(
      "yard_cam",
      { timeoutMs: 4000 },
      mockFetch
    );
    expect(third.fromCache).toBe(false);
    expect(fetchCalls).toEqual([
      "/api/yard_cam/hardware?timeout_ms=3000",
      "/api/yard_cam/ptz/info",
      "/api/yard_cam/hardware?timeout_ms=4000",
      "/api/yard_cam/ptz/info",
    ]);

    // Direct cache mutations notify subscribers and update snapshot
    const updatedHw = mergeOptimisticCameraHardwareInfo(mockHw, {
      brightness: 88,
    })!;
    setCachedCameraHardware("yard_cam", updatedHw);
    expect(getCachedCameraHardware("yard_cam")?.state.brightness).toBe(88);

    const updatedPtz = applySavedPresetToPtzInfo(mockPtz, "p2", "Gate")!;
    setCachedCameraPtzInfo("yard_cam", updatedPtz);
    expect(getCachedCameraPtzInfo("yard_cam")?.preset_names).toEqual([
      "Garden",
      "Gate",
    ]);
    expect(
      cameraHardwareCache.getSnapshot().hardwareByCam.yard_cam?.state.brightness
    ).toBe(88);
    expect(events.length).toBeGreaterThan(0);

    unsubscribe();
  });

  test("deduplicates concurrent in-flight probes and invalidates when cameraConfig ONVIF parameters change", async () => {
    const controller = createCameraHardwareCacheController();
    let hardwareHits = 0;
    const mockFetch = (async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/hardware")) {
        hardwareHits += 1;
        await new Promise((r) => setTimeout(r, 10));
        return new Response(
          JSON.stringify(
            makeBaseHardwareInfo({
              camera: "gate_cam",
              capabilities: makeBaseCapabilities({ ptz: false }),
            })
          ),
          { status: 200 }
        );
      }
      return new Response("{}", { status: 200 });
    }) as unknown as typeof fetch;

    const baseCfg = {
      enabled: true,
      onvif: { host: "192.0.2.10", port: 80, protocol: "dahua_cgi" },
      detect: { enabled: true, width: 1280, height: 720, fps: 5 },
      motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
      zones: {},
      record: { enabled: true },
      snapshots: { enabled: true },
    } as CameraConfig;

    const [resA, resB] = await Promise.all([
      controller.probeCameraHardwareAndPtz(
        "gate_cam",
        { cameraConfig: baseCfg, includePtz: false },
        mockFetch
      ),
      controller.probeCameraHardwareAndPtz(
        "gate_cam",
        { cameraConfig: baseCfg, includePtz: false },
        mockFetch
      ),
    ]);

    expect(hardwareHits).toBe(1);
    expect(resA.hardware?.camera).toBe("gate_cam");
    expect(resB.hardware?.camera).toBe("gate_cam");

    // Changing ONVIF port invalidates the cached entry for gate_cam automatically
    const updatedCfg: CameraConfig = {
      ...baseCfg,
      onvif: { ...baseCfg.onvif, port: 2020, protocol: "tapo" },
    };
    await controller.probeCameraHardwareAndPtz(
      "gate_cam",
      { cameraConfig: updatedCfg, includePtz: false },
      mockFetch
    );
    expect(hardwareHits).toBe(2);

    const snap = resolveCameraCapabilitySnapshot({
      camera: "gate_cam",
      cameraConfig: updatedCfg,
      hardwareInfo: resA.hardware,
      ptzInfo: { enabled: true, presets: [{ token: "1", name: "Entry" }] },
    });
    expect(snap.ptzEnabled).toBe(false);
    expect(snap.presets).toEqual(["Entry"]);
  });
});
