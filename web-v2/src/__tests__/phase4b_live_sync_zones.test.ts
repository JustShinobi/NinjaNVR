import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import {
  TacticalPtzHudOverlay,
  buildFloorplanSavePayload,
  buildFovConeSvgPath,
  buildPtzMovePayload,
  computeCameraAimFromTip,
  computeDefaultMapPlacement,
  computeFovConeTipSvgPoint,
  computeRoomMetrics,
  computeWallMetrics,
  createPtzMoveLifecycle,
  extractRoomBoundaryWalls,
  mapClientPointToFloorplan,
  normalizeFloorplanRoomRect,
  resizeRoomByMeters,
  resizeWallToLengthMeters,
  resolveSafeAlertThumbSrc,
  rotateWallToAngleDegrees,
  sanitizeFloorplanRoom,
  sanitizeFloorplanUrl,
  sanitizeFloorplanWall,
  sanitizeMapPlacement,
  sanitizeTimeShiftSeek,
  snapFloorplanCoord,
  snapWallEndpoint,
} from "../views/LiveWorkspace";
import {
  assignSyncQuadSlot,
  autoPickActiveCamerasAtTimestamp,
  clampSyncTimestamp,
  computeSyncVodWindow,
  resolveSafeSyncThumbSrc,
} from "../views/SyncWorkspace";
import {
  computePolygonSignedArea,
  deduplicatePolygonVertices,
  hasSelfIntersectingEdges,
  isNonDegeneratePolygon,
  normalizeZonePoint,
  sanitizeCameraGeometryDraft,
  sanitizeConfigGeometryDraft,
  sanitizeMaskPolygonsValue,
  sanitizePolygonCoordString,
  sanitizeStateRoiConfig,
  sanitizeTripwireLine,
} from "../views/ZonesWorkspace";
import { CameraConfig, NinjaConfigResponse, ReviewItem } from "../types/nvr";

function makeBaseCameraConfig(): CameraConfig {
  return {
    enabled: true,
    detect: { enabled: true, width: 1920, height: 1080, fps: 5 },
    motion: { enabled: true, threshold: 30, contour_area: 10, mask: [] },
    zones: {},
    record: { enabled: true },
    snapshots: { enabled: true },
  };
}

describe("Phase 4B LiveWorkspace — ONVIF PTZ Continuous Move/Stop Lifecycle & URL Encoding", () => {
  test("createPtzMoveLifecycle stops camera on pointer release, window blur, visibilitychange, key blur, camera switch, and unmount", async () => {
    const calls: Array<{ url: string; body: string; keepalive?: boolean }> = [];
    const mockFetch: typeof fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      calls.push({
        url: String(input),
        body: typeof init?.body === "string" ? init.body : "",
        keepalive: init?.keepalive,
      });
      return new Response(JSON.stringify({ success: true }), { status: 200 });
    }) as typeof fetch;

    const lifecycle = createPtzMoveLifecycle({
      camera: "front/gate #1",
      speed: 0.5,
      fetchImpl: mockFetch,
    });

    // Idle blur or stopMove(false) does not spam /ptz/stop
    await lifecycle.handleWindowBlur();
    expect(calls.length).toBe(0);

    // 1. Start move -> window blur sends encoded /ptz/stop with keepalive
    await lifecycle.startMove("left");
    expect(lifecycle.isMoving()).toBe(true);
    expect(lifecycle.getActiveDirection()).toBe("left");
    expect(calls.length).toBe(1);
    expect(calls[0].url).toBe("/api/front%2Fgate%20%231/ptz/move");
    expect(JSON.parse(calls[0].body)).toEqual({
      mode: "continuous",
      action: "left",
      pan: -0.5,
      tilt: 0,
      zoom: 0,
      speed: 0.5,
    });

    await lifecycle.handleWindowBlur();
    expect(lifecycle.isMoving()).toBe(false);
    expect(lifecycle.getActiveDirection()).toBeNull();
    expect(calls.length).toBe(2);
    expect(calls[1].url).toBe("/api/front%2Fgate%20%231/ptz/stop");
    expect(calls[1].keepalive).toBe(true);
    expect(JSON.parse(calls[1].body)).toEqual({ pan_tilt: true, zoom: true });

    // 2. Start move -> document visibilitychange ('hidden') sends /ptz/stop
    await lifecycle.startMove("up");
    expect(lifecycle.isMoving()).toBe(true);
    await lifecycle.handleVisibilityChange("hidden");
    expect(lifecycle.isMoving()).toBe(false);
    expect(calls.length).toBe(4);
    expect(calls[3].url).toBe("/api/front%2Fgate%20%231/ptz/stop");

    // 3. Start move -> keyboard focus loss (handleKeyBlur) sends /ptz/stop
    await lifecycle.startMove("zoom_in");
    expect(lifecycle.isMoving()).toBe(true);
    await lifecycle.handleKeyBlur();
    expect(lifecycle.isMoving()).toBe(false);
    expect(calls.length).toBe(6);
    expect(calls[5].url).toBe("/api/front%2Fgate%20%231/ptz/stop");

    // 4. Start move -> switching camera sends /ptz/stop to the PREVIOUS camera before switching
    await lifecycle.startMove("right");
    expect(lifecycle.isMoving()).toBe(true);
    await lifecycle.setCamera("backyard_ptz");
    expect(lifecycle.isMoving()).toBe(false);
    expect(lifecycle.getCamera()).toBe("backyard_ptz");
    expect(calls.length).toBe(8);
    expect(calls[7].url).toBe("/api/front%2Fgate%20%231/ptz/stop");

    // 5. Start move on new camera -> dispose (unmount) sends /ptz/stop to backyard_ptz
    await lifecycle.startMove("down");
    expect(lifecycle.isMoving()).toBe(true);
    await lifecycle.dispose();
    expect(lifecycle.isMoving()).toBe(false);
    expect(calls.length).toBe(10);
    expect(calls[9].url).toBe("/api/backyard_ptz/ptz/stop");

    // Further startMove calls after dispose are ignored
    await lifecycle.startMove("left");
    expect(calls.length).toBe(10);
  });

  test("buildPtzMovePayload sanitizes NaN and Infinity speeds safely", () => {
    const nanPayload = buildPtzMovePayload("left", Number.NaN, "continuous");
    expect(Number.isFinite(nanPayload.pan)).toBe(true);
    expect(nanPayload.pan).toBe(-0.5);
    expect(nanPayload.speed).toBe(0.5);

    const infPayload = buildPtzMovePayload("zoom_in", Number.POSITIVE_INFINITY, "continuous");
    expect(infPayload.zoom).toBe(0.5);
    expect(infPayload.speed).toBe(0.5);
  });

  test("TacticalPtzHudOverlay safely transitions between non-PTZ and PTZ configs without hook order violation", () => {
    const baseCam = makeBaseCameraConfig();
    const htmlDisabled = renderToStaticMarkup(
      React.createElement(TacticalPtzHudOverlay, {
        camera: "cam_1",
        cameraConfig: baseCam,
        ptzInfo: null,
      })
    );
    expect(htmlDisabled).toBe("");

    const htmlEnabled = renderToStaticMarkup(
      React.createElement(TacticalPtzHudOverlay, {
        camera: "cam_1",
        cameraConfig: { ...baseCam, onvif: { host: "192.0.2.55", port: 80 } },
        ptzInfo: { enabled: true, active_preset: "gate" },
      })
    );
    expect(htmlEnabled).toContain("PTZ:gate");
  });
});

describe("Phase 4B LiveWorkspace — Tactical Floorplan CAD Math, XSS URL Sanitization & Config Save Payload", () => {
  test("floorplan CAD math functions guard against NaN, Infinity, and zero dimensions", () => {
    const placement = computeDefaultMapPlacement(Number.NaN, Number.NaN);
    expect(Number.isFinite(placement.x)).toBe(true);
    expect(Number.isFinite(placement.y)).toBe(true);
    expect(Number.isFinite(placement.range)).toBe(true);

    const aim = computeCameraAimFromTip(Number.NaN, 0.5, Number.POSITIVE_INFINITY, 0.5);
    expect(Number.isFinite(aim.angle_deg)).toBe(true);
    expect(Number.isFinite(aim.range)).toBe(true);

    const tip = computeFovConeTipSvgPoint(Number.NaN, Number.NaN, Number.NaN, Number.NaN);
    expect(Number.isFinite(tip.x)).toBe(true);
    expect(Number.isFinite(tip.y)).toBe(true);

    const conePath = buildFovConeSvgPath(500, 300, Number.NaN, 220, Number.NaN);
    expect(conePath).not.toContain("NaN");
    // Even if fovDeg > 180, half is clamped to 89 deg (178 deg total < 180), so large-arc-flag must be 0
    expect(conePath).toContain(" 0 0 1 ");

    const mappedPt = mapClientPointToFloorplan(
      Number.NaN,
      200,
      { left: 0, top: 0, width: Number.NaN, height: 0 },
      0,
      0
    );
    expect(mappedPt).toEqual({ x: 0.5, y: 0.5 });

    expect(snapFloorplanCoord(Number.NaN, Number.NaN)).toBe(0);
    expect(snapFloorplanCoord(0.5, 0)).toBe(0.5);

    const rect = normalizeFloorplanRoomRect(
      Number.NaN,
      Number.NEGATIVE_INFINITY,
      Number.POSITIVE_INFINITY,
      Number.NaN
    );
    expect(Number.isFinite(rect.x)).toBe(true);
    expect(Number.isFinite(rect.y)).toBe(true);
    expect(Number.isFinite(rect.w)).toBe(true);
    expect(Number.isFinite(rect.h)).toBe(true);

    const rmMetrics = computeRoomMetrics(
      { x: Number.NaN, y: 0.1, w: Number.NaN, h: 0.3 },
      Number.NaN,
      0
    );
    expect(Number.isFinite(rmMetrics.areaSqM)).toBe(true);

    const resizedRoom = resizeRoomByMeters(
      { id: "r1", label: "SALA", x: 0.1, y: 0.1, w: 0.2, h: 0.2 },
      Number.NaN,
      Number.POSITIVE_INFINITY,
      0,
      Number.NaN
    );
    expect(Number.isFinite(resizedRoom.w)).toBe(true);
    expect(Number.isFinite(resizedRoom.h)).toBe(true);

    const wallMetrics = computeWallMetrics(
      [Number.NaN, 0.2, Number.POSITIVE_INFINITY, 0.2],
      Number.NaN,
      0
    );
    expect(Number.isFinite(wallMetrics.lengthM)).toBe(true);
    expect(Number.isFinite(wallMetrics.angleDeg)).toBe(true);

    const resizedWall = resizeWallToLengthMeters(
      [0.2, 0.2, 0.2, 0.2],
      Number.NaN,
      0,
      Number.NaN
    );
    expect(resizedWall.every((n) => Number.isFinite(n) && n >= 0 && n <= 1)).toBe(true);

    const rotatedWall = rotateWallToAngleDegrees(
      [0.2, 0.2, 0.4, 0.2],
      Number.NaN,
      0,
      Number.NaN
    );
    expect(rotatedWall.every((n) => Number.isFinite(n) && n >= 0 && n <= 1)).toBe(true);

    const snappedEnd = snapWallEndpoint(0.1, 0.1, Number.NaN, Number.POSITIVE_INFINITY, {
      snapStep: Number.NaN,
      floorplanWidthM: 0,
      floorplanHeightM: Number.NaN,
    });
    expect(Number.isFinite(snappedEnd.x)).toBe(true);
    expect(Number.isFinite(snappedEnd.y)).toBe(true);

    const boundaryWalls = extractRoomBoundaryWalls({
      id: "r1",
      label: "SALA",
      x: Number.NaN,
      y: 0.1,
      w: Number.NaN,
      h: 0.2,
    });
    expect(boundaryWalls.length).toBe(4);
    expect(
      boundaryWalls.every((w) => w.points.every((n) => Number.isFinite(n)))
    ).toBe(true);
  });

  test("sanitizeFloorplanUrl blocks javascript:, data:text/html, protocol-relative, and scripted SVG data URLs", () => {
    expect(sanitizeFloorplanUrl("javascript:alert(1)")).toBe("");
    expect(sanitizeFloorplanUrl("JAVASCRIPT:fetch('//evil')")).toBe("");
    expect(sanitizeFloorplanUrl("data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==")).toBe("");
    expect(sanitizeFloorplanUrl("//evil.example/floorplan.svg")).toBe("");

    // SVG data URL with embedded <script> is rejected
    const maliciousSvgB64 = globalThis.btoa(
      '<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>'
    );
    expect(sanitizeFloorplanUrl(`data:image/svg+xml;base64,${maliciousSvgB64}`)).toBe("");

    // Clean SVG data URL and PNG data URL and relative/https URLs are allowed
    const cleanSvgB64 = globalThis.btoa(
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><rect width="100" height="100"/></svg>'
    );
    expect(sanitizeFloorplanUrl(`data:image/svg+xml;base64,${cleanSvgB64}`)).toBe(
      `data:image/svg+xml;base64,${cleanSvgB64}`
    );
    expect(sanitizeFloorplanUrl("data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==")).toBe(
      "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="
    );
    expect(sanitizeFloorplanUrl("/assets/floorplan.png")).toBe("/assets/floorplan.png");
    expect(sanitizeFloorplanUrl("https://example.com/map.png")).toBe("https://example.com/map.png");
  });

  test("buildFloorplanSavePayload emits map-only camera patches without clobbering zones/masks and strips simulated cameras", () => {
    const existingCameras: Record<string, CameraConfig> = {
      front_door: {
        ...makeBaseCameraConfig(),
        privacy_mask: ["0.100,0.100,0.400,0.100,0.400,0.400"],
        zones: {
          porch: { coordinates: "0.200,0.200,0.600,0.200,0.600,0.600" },
        },
      },
    };
    const mapPlacements = {
      front_door: {
        x: 0.35,
        y: 0.45,
        angle_deg: 135,
        fov_deg: 95,
        range: 0.3,
      },
      front_door_sim_01: {
        x: 0.8,
        y: 0.8,
        angle_deg: 0,
        fov_deg: 90,
        range: 0.25,
      },
    };

    const payload = buildFloorplanSavePayload({
      existingCameras,
      mapPlacements,
      existingUi: { theme: "dark", high_contrast: true },
      floorplanUrl: "javascript:alert(1)",
      floorplanOpacity: Number.NaN,
      floorplanWidthM: 25,
      floorplanHeightM: 15,
      floorplanRooms: [
        { id: "r1", label: "SALA", x: -0.2, y: 0.1, w: 0.5, h: 0.4 },
      ],
      floorplanWalls: [
        { points: [-0.5, 0.2, 1.5, 0.2], kind: "wall" },
      ],
    });

    // Simulated camera is not injected; real camera patch contains ONLY `map` so zones/privacy_mask are never clobbered
    expect(Object.keys(payload.cameras)).toEqual(["front_door"]);
    expect(Object.keys(payload.cameras.front_door)).toEqual(["map"]);
    expect(payload.cameras.front_door.map).toEqual({
      x: 0.35,
      y: 0.45,
      angle_deg: 135,
      fov_deg: 95,
      range: 0.3,
    });

    const fp = payload.ui.floorplan as {
      background_data_url: string | null;
      opacity: number;
      rooms: Array<{ x: number; y: number; w: number; h: number }>;
      walls: Array<{ points: [number, number, number, number] }>;
    };
    expect(fp.background_data_url).toBeNull();
    expect(fp.opacity).toBe(0.45);
    expect(fp.rooms[0].x).toBe(0);
    expect(fp.walls[0].points).toEqual([0, 0.2, 1, 0.2]);
  });

  test("resolveSafeAlertThumbSrc and sanitizeTimeShiftSeek encode camera names and bound seeks", () => {
    expect(resolveSafeAlertThumbSrc("", "front door#1")).toBe(
      "/api/front%20door%231/latest.webp"
    );
    expect(resolveSafeAlertThumbSrc("javascript:alert(1)", "cam_a")).toBe(
      "/api/cam_a/latest.webp"
    );
    expect(resolveSafeAlertThumbSrc("//evil.com/x.webp", "cam_a")).toBe(
      "/api/cam_a/latest.webp"
    );
    expect(
      resolveSafeAlertThumbSrc("/media/nvr/clips/review/thumb-1.webp", "cam_a")
    ).toBe("/clips/review/thumb-1.webp");

    const now = 1700001000;
    expect(sanitizeTimeShiftSeek(null, now)).toBeNull();
    expect(sanitizeTimeShiftSeek(Number.NaN, now)).toBeNull();
    expect(sanitizeTimeShiftSeek(-50, now)).toBeNull();
    // Seeking into the future or within 2s of live edge returns to live (null)
    expect(sanitizeTimeShiftSeek(now + 100, now)).toBeNull();
    expect(sanitizeTimeShiftSeek(now - 1, now)).toBeNull();
    expect(sanitizeTimeShiftSeek(now - 30.8, now)).toBe(now - 31);
  });
});

describe("Phase 4B SyncWorkspace — Bounded Time Scrubbing, Safe Thumbnails & Quad Slot Validation", () => {
  test("clampSyncTimestamp and computeSyncVodWindow prevent future or NaN VOD manifest requests", () => {
    const now = 1700005000;
    expect(clampSyncTimestamp(Number.NaN, now)).toBe(now - 30);
    expect(clampSyncTimestamp(-100, now)).toBe(now - 30);
    expect(clampSyncTimestamp(now + 600, now)).toBe(now - 5);
    expect(clampSyncTimestamp(now - 60, now)).toBe(now - 60);

    // Near live edge, endTime is capped at `now` while guaranteeing at least 12s window
    const nearEdge = computeSyncVodWindow(now - 20, 120, now);
    expect(nearEdge.startTime).toBe(now - 20);
    expect(nearEdge.endTime).toBe(now);

    // Historical timestamp gets full 120s window
    const historical = computeSyncVodWindow(now - 500, 120, now);
    expect(historical.startTime).toBe(now - 500);
    expect(historical.endTime).toBe(now - 380);
  });

  test("assignSyncQuadSlot and autoPickActiveCamerasAtTimestamp reject unknown cameras and NaN inputs", () => {
    const cams = ["cam_1", "cam_2", "cam_3", "cam_4"];
    expect(assignSyncQuadSlot(cams, Number.NaN, "cam_3", cams)).toEqual([
      "cam_3",
      "cam_2",
      "cam_1",
      "cam_4",
    ]);
    // Unknown camera is rejected
    expect(assignSyncQuadSlot(cams, 0, "unknown_cam", cams)).toEqual(cams);

    const reviews: ReviewItem[] = [
      {
        id: "rev_nan",
        camera: "cam_2",
        start_time: Number.NaN,
        end_time: Number.NaN,
        severity: "alert",
        thumb_path: "",
        has_been_reviewed: false,
        data: { objects: ["person"] },
      },
      {
        id: "rev_ok",
        camera: "cam_3",
        start_time: 1700000000,
        end_time: 1700000010,
        severity: "alert",
        thumb_path: "",
        has_been_reviewed: false,
        data: { objects: ["person"] },
      },
    ];
    expect(
      autoPickActiveCamerasAtTimestamp(cams, reviews, 1700000000, 60)[0]
    ).toBe("cam_3");
    expect(
      autoPickActiveCamerasAtTimestamp(cams, reviews, Number.NaN, Number.NaN)
    ).toEqual(cams);

    expect(resolveSafeSyncThumbSrc("", "cam/1?x=2")).toBe(
      "/api/cam%2F1%3Fx%3D2/latest.webp"
    );
  });
});

describe("Phase 4B ZonesWorkspace — Pixel-to-Normalized Clamping & Degenerate Polygon Rejection", () => {
  test("normalizeZonePoint converts pixel coordinates to [0..1] and clamps out-of-bounds values", () => {
    // Normalized coordinates stay normalized
    expect(normalizeZonePoint([0.25, 0.75], 1920, 1080)).toEqual([0.25, 0.75]);
    // Pixel coordinates (960, 540) on 1920x1080 normalize to (0.5, 0.5)
    expect(normalizeZonePoint([960, 540], 1920, 1080)).toEqual([0.5, 0.5]);
    // Out-of-bounds and NaN values clamp safely into [0, 1]
    expect(normalizeZonePoint([-0.4, 2500], 1920, 1080)).toEqual([0, 1]);
    expect(normalizeZonePoint([Number.NaN, Number.POSITIVE_INFINITY], 1920, 1080)).toEqual([
      0, 0,
    ]);
  });

  test("isNonDegeneratePolygon and sanitizePolygonCoordString reject < 3 points, duplicate vertices, and collinear polygons", () => {
    // 3 identical points (user triple-clicked the same spot) -> degenerate!
    const samePoint: Array<[number, number]> = [
      [0.5, 0.5],
      [0.5, 0.5],
      [0.5, 0.5],
    ];
    expect(deduplicatePolygonVertices(samePoint).length).toBe(1);
    expect(isNonDegeneratePolygon(samePoint)).toBe(false);
    expect(sanitizePolygonCoordString("0.5,0.5,0.5,0.5,0.5,0.5")).toBeNull();

    // 3 collinear horizontal points -> zero shoelace area -> degenerate!
    const collinear: Array<[number, number]> = [
      [0.1, 0.3],
      [0.4, 0.3],
      [0.8, 0.3],
    ];
    expect(computePolygonSignedArea(collinear)).toBe(0);
    expect(isNonDegeneratePolygon(collinear)).toBe(false);
    expect(sanitizePolygonCoordString("0.1,0.3,0.4,0.3,0.8,0.3")).toBeNull();

    // 3 collinear diagonal points -> zero shoelace area -> degenerate!
    expect(sanitizePolygonCoordString("0.1,0.1,0.2,0.2,0.3,0.3")).toBeNull();

    // Valid triangle in normalized space
    expect(sanitizePolygonCoordString("0.1,0.1,0.6,0.1,0.6,0.6")).toBe(
      "0.100,0.100,0.600,0.100,0.600,0.600"
    );

    // Valid rectangle in pixel space (1280x720) -> normalized to [0..1]
    expect(
      sanitizePolygonCoordString("0,0,640,0,640,360,0,360", 1280, 720)
    ).toBe("0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500");
  });

  test("sanitizeCameraGeometryDraft and sanitizeConfigGeometryDraft strip degenerate privacy_mask, motion_mask, zones, and tripwires while normalizing pixel coordinates", () => {
    const camCfg: CameraConfig = {
      ...makeBaseCameraConfig(),
      detect: { enabled: true, width: 1280, height: 720, fps: 5 },
      privacy_mask: [
        "0.2,0.2,0.2,0.2,0.2,0.2", // degenerate same-point click -> must be stripped so backend FFmpeg drawbox does not fail closed!
        "0,0,640,0,640,360,0,360", // pixel coordinates -> normalized to 0..0.5
      ],
      motion: {
        enabled: true,
        threshold: 30,
        contour_area: 10,
        mask: ["0.1,0.1,0.2,0.2,0.3,0.3"], // collinear -> stripped
      },
      zones: {
        valid_zone: {
          coordinates: "0.1,0.1,0.5,0.1,0.5,0.5",
        },
        degenerate_zone: {
          coordinates: "0.4,0.4,0.4,0.4,0.4,0.4",
        },
      },
      lines: {
        zero_line: {
          points: [0.3, 0.3, 0.3, 0.3], // zero-length tripwire -> stripped
          direction: "any",
        },
        pixel_line: {
          points: [0, 360, 1280, 360], // pixel coordinates -> normalized
          direction: "a_to_b",
        },
      },
      state_rois: {
        gate_roi: {
          crop: [640, 360, 320, 180], // reversed pixel crop -> normalized & ordered [0.25, 0.25, 0.5, 0.5]
          classes: ["closed", "open"],
        },
      },
    };

    const { cleaned, rejectedCount } = sanitizeCameraGeometryDraft(camCfg);
    expect(rejectedCount).toBe(4);
    expect(cleaned.privacy_mask).toEqual([
      "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500",
    ]);
    expect(cleaned.motion.mask).toEqual([]);
    expect(Object.keys(cleaned.zones)).toEqual(["valid_zone"]);
    expect(Object.keys(cleaned.lines || {})).toEqual(["pixel_line"]);
    expect(cleaned.lines?.pixel_line.points).toEqual([0, 0.5, 1, 0.5]);
    expect(cleaned.state_rois?.gate_roi.crop).toEqual([0.25, 0.25, 0.5, 0.5]);

    // Also verify sanitizeConfigGeometryDraft across full NinjaConfigResponse
    const fullCfg: NinjaConfigResponse = {
      version: "0.9.0",
      safe_mode: false,
      mqtt: {
        enabled: false,
        host: "localhost",
        port: 1883,
        topic_prefix: "nvr",
        client_id: "ninjanvr",
        stats_interval: 60,
      },
      cameras: { front_door: camCfg },
    };
    const fullRes = sanitizeConfigGeometryDraft(fullCfg);
    expect(fullRes.rejectedCount).toBe(4);
    expect(fullRes.cleaned.cameras.front_door.privacy_mask).toEqual([
      "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500",
    ]);

    // Verify individual helpers
    expect(sanitizeMaskPolygonsValue("0.1,0.1,0.1,0.1,0.1,0.1")).toEqual({
      sanitized: [],
      rejectedCount: 1,
    });
    expect(
      sanitizeTripwireLine({ points: [0.5, 0.5, 0.5, 0.5], direction: "any" })
    ).toBeNull();
    expect(
      sanitizeStateRoiConfig({ crop: [0.5, 0.5, 0.5, 0.5], classes: ["closed", "open"] })
        .crop
    ).toEqual([0.5, 0.5, 0.51, 0.51]);
  });

  test("Cycle 2: createPtzMoveLifecycle serializes in-flight /ptz/move before /ptz/stop on rapid pointer release and deduplicates active direction", async () => {
    const completionOrder: string[] = [];
    const gate: { release?: () => void } = {};

    const delayedFetch: typeof fetch = (async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/ptz/move")) {
        await new Promise<void>((resolve) => {
          gate.release = resolve;
        });
        completionOrder.push(`DONE:${url}`);
      } else {
        completionOrder.push(`DONE:${url}`);
      }
      return new Response(JSON.stringify({ success: true }), { status: 200 });
    }) as typeof fetch;

    const lifecycle = createPtzMoveLifecycle({
      camera: "  ptz_cam_01  ",
      speed: 0.5,
      fetchImpl: delayedFetch,
    });

    // Fire unawaited startMove("left"), duplicate startMove("left"), and rapid stopMove(false) + window pointerup stopMove(false)
    const movePromise = lifecycle.startMove("left");
    const dupMovePromise = lifecycle.startMove("left");
    expect(lifecycle.isMoving()).toBe(true);

    const stopPromise1 = lifecycle.stopMove(false);
    const stopPromise2 = lifecycle.stopMove(false);
    // Synchronously marked stopped so UI/blur handlers see moving=false immediately
    expect(lifecycle.isMoving()).toBe(false);
    // /ptz/stop has NOT been sent yet because /ptz/move is still in flight
    expect(completionOrder).toEqual([]);

    // Release the in-flight /ptz/move network response
    await Promise.resolve();
    gate.release?.();
    await Promise.all([movePromise, dupMovePromise, stopPromise1, stopPromise2]);

    // /ptz/move strictly completes before a single /ptz/stop (no duplicate stop spam, no out-of-order stop-before-move)
    expect(completionOrder).toEqual([
      "DONE:/api/ptz_cam_01/ptz/move",
      "DONE:/api/ptz_cam_01/ptz/stop",
    ]);
  });

  test("Cycle 2: URL & thumbnail sanitizers block backslash authority-relative URLs, path traversal, whitespace cameras, and SVG on* / iframe / entity vectors", () => {
    expect(sanitizeFloorplanUrl("/\\evil.example/floorplan.svg")).toBe("");
    expect(sanitizeFloorplanUrl("/assets/../../api/logout")).toBe("");
    expect(sanitizeFloorplanUrl("/assets/%2e%2e/secret")).toBe("");
    expect(sanitizeFloorplanUrl("/assets/%5cevil")).toBe("");

    const onmouseoverSvgB64 = globalThis.btoa(
      '<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10" onmouseover="alert(1)"/></svg>'
    );
    expect(sanitizeFloorplanUrl(`data:image/svg+xml;base64,${onmouseoverSvgB64}`)).toBe("");

    const entitySvgB64 = globalThis.btoa(
      '<svg xmlns="http://www.w3.org/2000/svg"><a href="&#x6a;avascript:alert(1)"><rect width="10" height="10"/></a></svg>'
    );
    expect(sanitizeFloorplanUrl(`data:image/svg+xml;base64,${entitySvgB64}`)).toBe("");

    expect(resolveSafeAlertThumbSrc("/\\evil.example/pixel.webp", "cam_a")).toBe(
      "/api/cam_a/latest.webp"
    );
    expect(resolveSafeAlertThumbSrc("/clips/../../api/logout", "cam_a")).toBe(
      "/api/cam_a/latest.webp"
    );
    expect(resolveSafeAlertThumbSrc("/clips/%2e%2e/api/logout", "   ")).toBe(
      "/api/camera/latest.webp"
    );

    expect(resolveSafeSyncThumbSrc("/\\evil.example/pixel.webp", "cam_b")).toBe(
      "/api/cam_b/latest.webp"
    );
    expect(resolveSafeSyncThumbSrc("/clips/../../api/logout", "   ")).toBe(
      "/api/camera/latest.webp"
    );
  });

  test("Cycle 2: ZonesWorkspace clamps slight [0..1] canvas overshoots (1.0 < v <= 1.5), rejects empty tokens & bowties, and sanitizes object/filter/global masks", () => {
    // 1. Slight canvas pointer overshoot (1.02) clamps to 1.0 instead of dividing by 1920x1080
    expect(normalizeZonePoint([1.02, 0.5], 1920, 1080)).toEqual([1, 0.5]);
    expect(
      sanitizePolygonCoordString("0.2,0.2,1.02,0.2,1.02,0.8,0.2,0.8", 1920, 1080)
    ).toBe("0.200,0.200,1.000,0.200,1.000,0.800,0.200,0.800");
    expect(
      sanitizeTripwireLine(
        { points: [0.2, 0.5, 1.02, 0.5], direction: "a_to_b" },
        1920,
        1080
      )?.points
    ).toEqual([0.2, 0.5, 1, 0.5]);
    expect(
      sanitizeStateRoiConfig(
        { crop: [0.5, 0.5, 1.02, 0.9], classes: ["closed", "open"] },
        1920,
        1080
      ).crop
    ).toEqual([0.5, 0.5, 1, 0.9]);

    // 2. Empty comma tokens and non-numeric array items are rejected rather than coerced via Number("") === 0
    expect(sanitizePolygonCoordString(",,,,,")).toBeNull();
    expect(sanitizePolygonCoordString("0.1,0.1,0.5,0.1,0.5,")).toBeNull();
    expect(sanitizePolygonCoordString("0.1,0.1,0.5,0.1,,0.5")).toBeNull();
    expect(
      sanitizeMaskPolygonsValue([[null, null, true, false, "", 0.5]])
    ).toEqual({
      sanitized: [],
      rejectedCount: 1,
    });

    // 3. Asymmetric 4-vertex self-intersecting bowtie is rejected
    const asymmetricBowtie: Array<[number, number]> = [
      [0.1, 0.1],
      [0.9, 0.9],
      [0.9, 0.1],
      [0.1, 0.4],
    ];
    expect(hasSelfIntersectingEdges(asymmetricBowtie)).toBe(true);
    expect(isNonDegeneratePolygon(asymmetricBowtie)).toBe(false);
    expect(
      sanitizePolygonCoordString("0.1,0.1,0.9,0.9,0.9,0.1,0.1,0.4")
    ).toBeNull();

    // 4. Object-form masks ({ coordinates: string }, pixelate objects) and global/per-filter masks are sanitized
    const arrayOfCoordObjs = sanitizeMaskPolygonsValue(
      [
        { coordinates: "0,0,640,0,640,360,0,360" },
        { coordinates: "0.2,0.2,0.2,0.2,0.2,0.2" },
      ],
      1280,
      720
    );
    expect(arrayOfCoordObjs).toEqual({
      sanitized: [{ coordinates: "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500" }],
      rejectedCount: 1,
    });

    const pixelatedObjMask = sanitizeMaskPolygonsValue(
      {
        mode: "pixelate",
        block_size: 8,
        coordinates: "0,0,640,0,640,360,0,360",
        bad_poly: "0.1,0.1,0.1,0.1,0.1,0.1",
      },
      1280,
      720
    );
    expect(pixelatedObjMask).toEqual({
      sanitized: {
        mode: "pixelate",
        block_size: 8,
        coordinates: "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500",
      },
      rejectedCount: 1,
    });

    const cfgWithGlobalAndFilterMasks: NinjaConfigResponse = {
      version: "0.9.0",
      safe_mode: false,
      detect: { enabled: true, width: 1280, height: 720, fps: 5 },
      motion: {
        enabled: true,
        threshold: 30,
        contour_area: 10,
        mask: ["0.1,0.1,0.1,0.1,0.1,0.1"],
      },
      objects: {
        track: ["person"],
        mask: ["0,0,640,0,640,360,0,360"],
        filters: {
          person: {
            min_area: 500,
            max_area: 100000,
            min_ratio: 0.2,
            max_ratio: 5,
            threshold: 0.7,
            min_score: 0.5,
            mask: ["0.2,0.2,0.2,0.2,0.2,0.2"],
          },
        },
      },
      cameras: {
        front_door: {
          ...makeBaseCameraConfig(),
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          objects: {
            track: ["person"],
            filters: {
              person: {
                min_area: 500,
                max_area: 100000,
                min_ratio: 0.2,
                max_ratio: 5,
                threshold: 0.7,
                min_score: 0.5,
                mask: ["0.3,0.3,0.3,0.3,0.3,0.3", "0,0,640,0,640,360,0,360"],
              },
            },
          },
        },
      },
    };

    const fullSanitized = sanitizeConfigGeometryDraft(cfgWithGlobalAndFilterMasks);
    expect(fullSanitized.rejectedCount).toBe(3);
    expect(fullSanitized.cleaned.motion?.mask).toEqual([]);
    expect(fullSanitized.cleaned.objects?.mask).toEqual([
      "0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500",
    ]);
    expect(fullSanitized.cleaned.objects?.filters?.person.mask).toEqual([]);
    expect(
      fullSanitized.cleaned.cameras.front_door.objects?.filters?.person.mask
    ).toEqual(["0.000,0.000,0.500,0.000,0.500,0.500,0.000,0.500"]);
  });

  test("Cycle 3: sanitizeFloorplanRoom & sanitizeFloorplanWall preserve width/height when x/y is NaN or negative, sanitize CSS colors/wall kinds, and preserve wall angle on boundary clamping", () => {
    // 1. Negative or NaN x/y on a room preserves its width/height instead of collapsing w/h via r.x + r.w
    const negXRoom = sanitizeFloorplanRoom(
      {
        id: "r_neg",
        label: "GARAGEM",
        x: -0.2,
        y: 0.1,
        w: 0.5,
        h: 0.4,
        color: "#10b981",
      },
      0
    );
    expect(negXRoom.x).toBe(0);
    expect(negXRoom.w).toBe(0.5);
    expect(negXRoom.h).toBe(0.4);
    expect(negXRoom.color).toBe("#10b981");

    const nanXRoom = sanitizeFloorplanRoom(
      {
        id: "",
        label: "   ",
        x: Number.NaN,
        y: Number.NaN,
        w: 0.45,
        h: 0.35,
        color: "url(javascript:alert(1))",
      },
      2
    );
    expect(nanXRoom.id).toBe("room_3");
    expect(nanXRoom.label).toBe("ROOM");
    expect(nanXRoom.x).toBe(0);
    expect(nanXRoom.y).toBe(0);
    expect(nanXRoom.w).toBe(0.45);
    expect(nanXRoom.h).toBe(0.35);
    expect(nanXRoom.color).toBeNull();

    // 2. Malformed wall missing points array or with invalid kind is safely normalized
    const badWall = sanitizeFloorplanWall({
      points: undefined as unknown as [number, number, number, number],
      kind: "<script>",
    });
    expect(badWall.points).toEqual([0.2, 0.2, 0.4, 0.2]);
    expect(badWall.kind).toBe("wall");

    // 3. Directional box bound on resizeWallToLengthMeters & rotateWallToAngleDegrees preserves angle
    const horizontalOversize = resizeWallToLengthMeters(
      [0.2, 0.5, 0.4, 0.5],
      23, // > 20m width, < 23.32m diagonal
      20,
      12
    );
    expect(horizontalOversize).toEqual([0, 0.5, 1, 0.5]);

    const verticalRotated = rotateWallToAngleDegrees(
      [0.1, 0.2, 0.9, 0.2], // 16m horizontal wall rotated to 90 deg on a 20m x 12m floorplan
      90,
      20,
      12
    );
    expect(verticalRotated).toEqual([0.1, 0, 0.1, 1]);

    // 4. computeFovConeTipSvgPoint caps extreme radius to 1000 matching buildFovConeSvgPath
    expect(computeFovConeTipSvgPoint(500, 300, 0, 999999)).toEqual({
      x: 1500,
      y: 300,
    });
  });

  test("Cycle 3: URL & SVG sanitizers block SMIL <set>/<animate>, XXE <!ENTITY, obfuscated javascript:, credentialed URLs, and encoded // or double-encoded traversal", () => {
    // SVG SMIL <set> / <animate> / <style> / <!ENTITY> payloads blocked
    const smilSvgB64 = globalThis.btoa(
      '<svg xmlns="http://www.w3.org/2000/svg"><a id="a"><rect width="10" height="10"/></a><set href="#a" attributeName="href" to="https://evil.example"/></svg>'
    );
    expect(sanitizeFloorplanUrl(`data:image/svg+xml;base64,${smilSvgB64}`)).toBe("");

    const xxeSvgB64 = globalThis.btoa(
      '<!DOCTYPE svg [<!ENTITY xxe SYSTEM "file:///etc/passwd">]><svg xmlns="http://www.w3.org/2000/svg"><text>&xxe;</text></svg>'
    );
    expect(sanitizeFloorplanUrl(`data:image/svg+xml;base64,${xxeSvgB64}`)).toBe("");

    const obfuscatedJsSvgB64 = globalThis.btoa(
      '<svg xmlns="http://www.w3.org/2000/svg"><a href="java\tscript:alert(1)"><rect width="10" height="10"/></a></svg>'
    );
    expect(
      sanitizeFloorplanUrl(`data:image/svg+xml;base64,${obfuscatedJsSvgB64}`)
    ).toBe("");

    // Credentialed or quote-injected HTTP(S) URLs blocked
    expect(sanitizeFloorplanUrl("https://user:pass@example.com/map.png")).toBe("");
    expect(sanitizeFloorplanUrl('https://example.com/map.png" onload="alert(1)')).toBe("");

    // Encoded protocol-relative /%2fevil.example and double-encoded traversal blocked
    expect(sanitizeFloorplanUrl("/%2fevil.example/map.png")).toBe("");
    expect(sanitizeFloorplanUrl("/assets/%252e%252e/secret")).toBe("");
    expect(resolveSafeAlertThumbSrc("/%2fevil.example/x.webp", "cam_1")).toBe(
      "/api/cam_1/latest.webp"
    );
    expect(resolveSafeAlertThumbSrc("/clips/%252e%252e/secret.webp", "cam_1")).toBe(
      "/api/cam_1/latest.webp"
    );
    expect(resolveSafeSyncThumbSrc("/%2fevil.example/x.webp", "cam_1")).toBe(
      "/api/cam_1/latest.webp"
    );
    expect(resolveSafeSyncThumbSrc("/clips/%252e%252e/secret.webp", "cam_1")).toBe(
      "/api/cam_1/latest.webp"
    );
  });

  test("Cycle 3: SyncWorkspace caps oversized VOD windows to 3600s, deduplicates cameras, and handles inverted review end_time", () => {
    const now = 1700010000;
    const hugeWin = computeSyncVodWindow(now - 10000, 999999, now);
    expect(hugeWin.startTime).toBe(now - 10000);
    expect(hugeWin.endTime).toBe(now - 10000 + 3600);

    const dupCams = ["cam_1", "cam_1", "cam_2", "cam_3"];
    const invertedReviews: ReviewItem[] = [
      {
        id: "rev_inverted",
        camera: "cam_3",
        start_time: 1700000000,
        end_time: 1699990000, // inverted end_time < start_time
        severity: "alert",
        thumb_path: "",
        has_been_reviewed: false,
        data: { objects: ["person"] },
      },
    ];
    const picked = autoPickActiveCamerasAtTimestamp(
      dupCams,
      invertedReviews,
      1700000000,
      60
    );
    expect(picked).toEqual(["cam_3", "cam_1", "cam_2"]);
  });

  test("Cycle 3: ZonesWorkspace rejects pinched figure-8 polygons, T-junctions, and 180-degree folded spikes, and counts non-polygon mask entries", () => {
    // 1. Pinched figure-8 polygon sharing non-adjacent vertex [0.5, 0.5]
    const figureEight: Array<[number, number]> = [
      [0.1, 0.1],
      [0.5, 0.1],
      [0.5, 0.5],
      [0.9, 0.5],
      [0.9, 0.9],
      [0.5, 0.5],
      [0.1, 0.5],
    ];
    expect(hasSelfIntersectingEdges(figureEight)).toBe(true);
    expect(isNonDegeneratePolygon(figureEight)).toBe(false);
    expect(
      sanitizePolygonCoordString(
        "0.1,0.1,0.5,0.1,0.5,0.5,0.9,0.5,0.9,0.9,0.5,0.5,0.1,0.5"
      )
    ).toBeNull();

    // 2. T-junction where a vertex touches the interior of a non-adjacent edge
    const tJunction: Array<[number, number]> = [
      [0.1, 0.1],
      [0.9, 0.1],
      [0.9, 0.9],
      [0.5, 0.1], // touches top edge [0.1,0.1]->[0.9,0.1]
      [0.1, 0.9],
    ];
    expect(hasSelfIntersectingEdges(tJunction)).toBe(true);
    expect(isNonDegeneratePolygon(tJunction)).toBe(false);

    // 3. 180-degree folded-back collinear spike attached to a valid triangle
    const foldedSpike: Array<[number, number]> = [
      [0.1, 0.1],
      [0.6, 0.1],
      [0.9, 0.1], // extends along y=0.1
      [0.6, 0.1], // folds 180 deg back to [0.6, 0.1]
      [0.1, 0.6],
    ];
    expect(hasSelfIntersectingEdges(foldedSpike)).toBe(true);
    expect(isNonDegeneratePolygon(foldedSpike)).toBe(false);

    // 4. Non-polygon primitive items in mask array increment rejectedCount
    expect(
      sanitizeMaskPolygonsValue([
        null,
        123,
        true,
        {},
        "0.1,0.1,0.6,0.1,0.6,0.6",
      ])
    ).toEqual({
      sanitized: ["0.100,0.100,0.600,0.100,0.600,0.600"],
      rejectedCount: 4,
    });
  });
});
