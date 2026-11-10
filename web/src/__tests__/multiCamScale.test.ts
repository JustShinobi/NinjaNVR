import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import {
  DEFAULT_PICKER_THRESHOLD,
  TacticalCameraPicker,
  filterPickerCameras,
  getCameraPickerStatus,
  resolvePickerMode,
  stepPickerCamera,
} from "../components/TacticalCameraPicker";
import {
  MAX_DOCK_LIVE_DECODERS,
  computeCameraAimFromTip,
  computeDefaultMapPlacement,
  computeFovConeTipSvgPoint,
  computeRoomMetrics,
  computeWallMetrics,
  extractRoomBoundaryWalls,
  getMatrixPageSize,
  mapClientPointToFloorplan,
  nextMatrixTourPage,
  normalizeFloorplanRoomRect,
  resizeRoomByMeters,
  resizeWallToLengthMeters,
  resolveAdaptiveMatrixGridStyle,
  resolveDockTileMode,
  resolveOrderedDashboardCameras,
  rotateWallToAngleDegrees,
  sliceMatrixPage,
  snapFloorplanCoord,
  snapWallEndpoint,
} from "../views/LiveWorkspace";
import {
  MAX_SYNC_QUAD_SLOTS,
  assignSyncQuadSlot,
  autoPickActiveCamerasAtTimestamp,
  resolveSyncQuadCameras,
} from "../views/SyncWorkspace";
import { filterAndSortCameraHealth } from "../views/SystemWorkspace";
import {
  CameraConfig,
  CameraGroupConfig,
  CameraLiveActivity,
  CameraRuntimeStats,
  ReviewItem,
} from "../types/nvr";

function makeFleet(count: number): string[] {
  return Array.from(
    { length: count },
    (_, i) => `cam_${String(i + 1).padStart(3, "0")}`
  );
}

describe("LiveWorkspace paginated matrix, Auto-Tour, camera groups, and dock virtualization", () => {
  test("paginates 100 cameras into 25 (2x2), 12 (3x3), and 7 (4x4) pages with strict decoder cap <= 16", () => {
    const fleet100 = makeFleet(100);

    expect(getMatrixPageSize("2x2")).toBe(4);
    expect(getMatrixPageSize("3x3")).toBe(9);
    expect(getMatrixPageSize("4x4")).toBe(16);

    const p2x2 = sliceMatrixPage(fleet100, 0, "2x2");
    expect(p2x2.totalPages).toBe(25);
    expect(p2x2.visibleMatrixCams.length).toBe(4);
    expect(p2x2.visibleMatrixCams).toEqual([
      "cam_001",
      "cam_002",
      "cam_003",
      "cam_004",
    ]);

    const p3x3 = sliceMatrixPage(fleet100, 0, "3x3");
    expect(p3x3.totalPages).toBe(12);
    expect(p3x3.visibleMatrixCams.length).toBe(9);

    const p4x4First = sliceMatrixPage(fleet100, 0, "4x4");
    expect(p4x4First.totalPages).toBe(7);
    expect(p4x4First.visibleMatrixCams.length).toBe(16);

    const p4x4Last = sliceMatrixPage(fleet100, 6, "4x4");
    expect(p4x4Last.safePage).toBe(6);
    expect(p4x4Last.visibleMatrixCams.length).toBe(4);
    expect(p4x4Last.visibleMatrixCams[0]).toBe("cam_097");
    expect(p4x4Last.visibleMatrixCams[3]).toBe("cam_100");

    // Verify every single page across all densities never exceeds 16 decoders
    for (const density of ["2x2", "3x3", "4x4"] as const) {
      const { totalPages } = sliceMatrixPage(fleet100, 0, density);
      for (let page = 0; page < totalPages; page++) {
        const slice = sliceMatrixPage(fleet100, page, density);
        expect(slice.visibleMatrixCams.length).toBeLessThanOrEqual(16);
        expect(slice.visibleMatrixCams.length).toBeGreaterThan(0);
      }
    }
  });

  test("Auto-Tour wraps matrix page index cleanly at totalPages boundary", () => {
    expect(nextMatrixTourPage(0, 1)).toBe(0);
    expect(nextMatrixTourPage(0, 25)).toBe(1);
    expect(nextMatrixTourPage(23, 25)).toBe(24);
    expect(nextMatrixTourPage(24, 25)).toBe(0);
  });

  test("filters cameras by activeCameraGroup and respects camera.ui.dashboard and camera.ui.order", () => {
    const cams = ["cam_a", "cam_b", "cam_c", "cam_d"];
    const baseCam: CameraConfig = {
      enabled: true,
      detect: { enabled: true, width: 1280, height: 720, fps: 5 },
      motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
      zones: {},
      record: { enabled: true },
      snapshots: { enabled: true },
    };
    const camerasConfig: Record<string, CameraConfig> = {
      cam_a: { ...baseCam, ui: { order: 30, dashboard: true } },
      cam_b: { ...baseCam, ui: { order: 10, dashboard: true } },
      cam_c: { ...baseCam, ui: { order: 20, dashboard: false } }, // hidden from dashboard
      cam_d: { ...baseCam, ui: { order: 5, dashboard: true } },
    };
    const groups: Record<string, CameraGroupConfig> = {
      exterior: { cameras: ["cam_a", "cam_b", "cam_c"], order: 1 },
    };

    const allOrdered = resolveOrderedDashboardCameras(
      cams,
      camerasConfig,
      "all",
      groups
    );
    expect(allOrdered).toEqual(["cam_d", "cam_b", "cam_a"]);

    const groupOrdered = resolveOrderedDashboardCameras(
      cams,
      camerasConfig,
      "exterior",
      groups
    );
    expect(groupOrdered).toEqual(["cam_b", "cam_a"]);
  });

  test("caps concurrent live MSE decoders in .nvr-focus-dock to at most 6 and upgrades hovered tile", () => {
    const dockCams = makeFleet(99);
    const idleModes = dockCams.map((cam, idx) =>
      resolveDockTileMode(idx, cam, null, MAX_DOCK_LIVE_DECODERS, dockCams)
    );
    const idleMseCount = idleModes.filter((m) => m === "mse").length;
    expect(idleMseCount).toBe(6);
    expect(idleModes[0]).toBe("mse");
    expect(idleModes[5]).toBe("mse");
    expect(idleModes[6]).toBe("snapshot");
    expect(idleModes[98]).toBe("snapshot");

    // Hovering a snapshot tile at index 40 upgrades it to MSE while capping total MSE tiles at 6
    const hoveredModes = dockCams.map((cam, idx) =>
      resolveDockTileMode(idx, cam, "cam_041", MAX_DOCK_LIVE_DECODERS, dockCams)
    );
    expect(hoveredModes[40]).toBe("mse");
    const hoveredMseCount = hoveredModes.filter((m) => m === "mse").length;
    expect(hoveredMseCount).toBe(6);
  });
});

describe("Adaptive Tactical Map ceil(sqrt(N)) coordinate placement", () => {
  test("places all N=1, 4, 6, 25, 100, 150 cameras strictly within x:[0.10..0.90] and y:[0.12..0.88]", () => {
    for (const total of [1, 4, 6, 25, 100, 150]) {
      const coords = new Set<string>();
      for (let i = 0; i < total; i++) {
        const p = computeDefaultMapPlacement(i, total);
        expect(p.x).toBeGreaterThanOrEqual(0.1);
        expect(p.x).toBeLessThanOrEqual(0.9);
        expect(p.y).toBeGreaterThanOrEqual(0.12);
        expect(p.y).toBeLessThanOrEqual(0.88);
        expect(p.fov_deg).toBe(90);
        coords.add(`${p.x},${p.y}`);
      }
      // Every camera gets a unique grid cell
      expect(coords.size).toBe(total);
    }
  });

  test("computeCameraAimFromTip computes normalized angle_deg in [0, 360) and clamped range in [0.05, 0.50]", () => {
    // East (0 deg)
    const east = computeCameraAimFromTip(0.5, 0.5, 0.75, 0.5);
    expect(east.angle_deg).toBe(0);
    expect(east.range).toBe(0.25);

    // South (90 deg)
    const south = computeCameraAimFromTip(0.5, 0.5, 0.5, 0.8);
    expect(south.angle_deg).toBe(90);
    expect(south.range).toBe(0.3);

    // West (180 deg)
    const west = computeCameraAimFromTip(0.5, 0.5, 0.2, 0.5);
    expect(west.angle_deg).toBe(180);
    expect(west.range).toBe(0.3);

    // North (-90 deg -> normalized to 270 deg)
    const north = computeCameraAimFromTip(0.5, 0.5, 0.5, 0.2);
    expect(north.angle_deg).toBe(270);
    expect(north.range).toBe(0.3);

    // North-East (-45 deg -> normalized to 315 deg)
    const ne = computeCameraAimFromTip(0.5, 0.5, 0.7, 0.3);
    expect(ne.angle_deg).toBe(315);
    expect(ne.range).toBeGreaterThanOrEqual(0.05);
    expect(ne.range).toBeLessThanOrEqual(0.5);

    // Clamping range minimum (0.05) and maximum (0.50)
    const tooClose = computeCameraAimFromTip(0.5, 0.5, 0.51, 0.5);
    expect(tooClose.range).toBe(0.05);
    expect(tooClose.angle_deg).toBeGreaterThanOrEqual(0);
    expect(tooClose.angle_deg).toBeLessThan(360);

    const tooFar = computeCameraAimFromTip(0.1, 0.1, 0.95, 0.95);
    expect(tooFar.range).toBe(0.5);
    expect(tooFar.angle_deg).toBe(45);

    const svgTip = computeFovConeTipSvgPoint(500, 300, 90, 200);
    expect(svgTip).toEqual({ x: 500, y: 500 });
  });

  test("normalizeFloorplanRoomRect normalizes drags from any corner into positive (x, y, w, h) within [0, 1]", () => {
    // Top-left to bottom-right
    const tlToBr = normalizeFloorplanRoomRect(0.08, 0.1, 0.46, 0.46);
    expect(tlToBr).toEqual({ x: 0.08, y: 0.1, w: 0.38, h: 0.36 });

    // Bottom-right to top-left (reversed drag)
    const brToTl = normalizeFloorplanRoomRect(0.46, 0.46, 0.08, 0.1);
    expect(brToTl).toEqual({ x: 0.08, y: 0.1, w: 0.38, h: 0.36 });

    // Bottom-left to top-right
    const blToTr = normalizeFloorplanRoomRect(0.1, 0.8, 0.6, 0.2);
    expect(blToTr).toEqual({ x: 0.1, y: 0.2, w: 0.5, h: 0.6 });

    // Out-of-bounds coordinates clamped strictly to [0, 1]
    const clamped = normalizeFloorplanRoomRect(-0.25, 1.3, 0.4, 0.2);
    expect(clamped.x).toBe(0);
    expect(clamped.y).toBe(0.2);
    expect(clamped.w).toBe(0.4);
    expect(clamped.h).toBe(0.8);
    expect(clamped.x + clamped.w).toBeLessThanOrEqual(1);
    expect(clamped.y + clamped.h).toBeLessThanOrEqual(1);

    // Zero-area click enforces minimum room dimensions
    const zeroClick = normalizeFloorplanRoomRect(0.5, 0.5, 0.5, 0.5, 0.05);
    expect(zeroClick.w).toBe(0.05);
    expect(zeroClick.h).toBe(0.05);

    expect(snapFloorplanCoord(0.113, 0.02)).toBe(0.12);
    expect(snapFloorplanCoord(-0.5, 0.02)).toBe(0);
    expect(snapFloorplanCoord(1.4, 0.02)).toBe(1);
  });

  test("mapClientPointToFloorplan accurately converts mouse coordinates across letterboxed/pillarboxed SVG viewports", () => {
    // Wide container (1600x600) with 1000x600 viewBox -> scale = 1.0, drawnW = 1000, offsetX = 300
    const wideRect = { left: 100, top: 50, width: 1600, height: 600 };
    // Clicking at left edge of rendered 1000x600 viewBox (clientX = 100 + 300 = 400) -> x = 0
    expect(mapClientPointToFloorplan(400, 50, wideRect, 1000, 600)).toEqual({
      x: 0,
      y: 0,
    });
    // Clicking at center of rendered viewBox (clientX = 100 + 300 + 500 = 900, clientY = 50 + 300 = 350) -> (0.5, 0.5)
    expect(mapClientPointToFloorplan(900, 350, wideRect, 1000, 600)).toEqual({
      x: 0.5,
      y: 0.5,
    });
    // Clicking at right edge of rendered viewBox (clientX = 100 + 300 + 1000 = 1400) -> x = 1
    expect(mapClientPointToFloorplan(1400, 650, wideRect, 1000, 600)).toEqual({
      x: 1,
      y: 1,
    });
  });

  test("computeRoomMetrics, resizeRoomByMeters, computeWallMetrics, resizeWallToLengthMeters, rotateWallToAngleDegrees, and snapWallEndpoint support exact metric editing", () => {
    const room = {
      id: "sala",
      label: "SALA",
      x: 0.1,
      y: 0.25,
      w: 0.25,
      h: 0.5,
      color: "#0ea5e9",
    };
    const rmMetrics = computeRoomMetrics(room, 20, 12);
    expect(rmMetrics.widthM).toBe(5);
    expect(rmMetrics.heightM).toBe(6);
    expect(rmMetrics.areaSqM).toBe(30);
    expect(rmMetrics.xM).toBe(2);
    expect(rmMetrics.yM).toBe(3);

    const resizedRoom = resizeRoomByMeters(room, 4.0, 3.0, 20, 12);
    expect(resizedRoom.w).toBe(0.2);
    expect(resizedRoom.h).toBe(0.25);

    const boundaryWalls = extractRoomBoundaryWalls(resizedRoom);
    expect(boundaryWalls.length).toBe(4);
    expect(boundaryWalls[0].points).toEqual([0.1, 0.25, 0.3, 0.25]);

    // Horizontal 4.0m wall on a 20m x 12m floorplan -> dx = 0.20
    const wallPts: [number, number, number, number] = [0.1, 0.2, 0.3, 0.2];
    const wMetrics = computeWallMetrics(wallPts, 20, 12);
    expect(wMetrics.lengthM).toBe(4);
    expect(wMetrics.angleDeg).toBe(0);

    // Resize wall to 6.5m horizontally -> x2 = 0.1 + 6.5 / 20 = 0.425
    const resizedWall = resizeWallToLengthMeters(wallPts, 6.5, 20, 12);
    expect(resizedWall).toEqual([0.1, 0.2, 0.425, 0.2]);
    expect(computeWallMetrics(resizedWall, 20, 12).lengthM).toBe(6.5);

    // Rotate 6.0m wall to 90 degrees (vertical) -> dy = 6.0 / 12 = 0.5
    const wall6m: [number, number, number, number] = [0.2, 0.1, 0.5, 0.1];
    const rotated90 = rotateWallToAngleDegrees(wall6m, 90, 20, 12);
    expect(rotated90).toEqual([0.2, 0.1, 0.2, 0.6]);
    expect(computeWallMetrics(rotated90, 20, 12).lengthM).toBe(6);
    expect(computeWallMetrics(rotated90, 20, 12).angleDeg).toBe(90);

    // Magnetic vertex snap & orthogonal snap
    const vSnap = snapWallEndpoint(0.1, 0.1, 0.304, 0.252, {
      vertices: [{ x: 0.3, y: 0.25 }],
      vertexSnapRadius: 0.015,
    });
    expect(vSnap.snappedToVertex).toBe(true);
    expect(vSnap.x).toBe(0.3);
    expect(vSnap.y).toBe(0.25);

    const orthoHoriz = snapWallEndpoint(0.2, 0.3, 0.5, 0.306, {
      orthoSnap: true,
      snapStep: 0.005,
    });
    expect(orthoHoriz.y).toBe(0.3);
  });

  test("adapts partial last-page matrix grid dimensions so remaining cameras fill the stage", () => {
    expect(resolveAdaptiveMatrixGridStyle(16, "4x4")).toBeUndefined();
    expect(resolveAdaptiveMatrixGridStyle(9, "3x3")).toBeUndefined();
    expect(resolveAdaptiveMatrixGridStyle(4, "2x2")).toBeUndefined();

    // 4 remaining cameras on Page 7/7 of a 100-camera 4x4 grid fill the stage as 2x2
    expect(resolveAdaptiveMatrixGridStyle(4, "4x4")).toEqual({
      gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
      gridTemplateRows: "repeat(2, minmax(0, 1fr))",
    });
    expect(resolveAdaptiveMatrixGridStyle(1, "4x4")).toEqual({
      gridTemplateColumns: "minmax(0, 1fr)",
      gridTemplateRows: "minmax(0, 1fr)",
    });
    expect(resolveAdaptiveMatrixGridStyle(2, "3x3")).toEqual({
      gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
      gridTemplateRows: "minmax(0, 1fr)",
    });
    expect(resolveAdaptiveMatrixGridStyle(6, "4x4")).toEqual({
      gridTemplateColumns: "repeat(3, minmax(0, 1fr))",
      gridTemplateRows: "repeat(2, minmax(0, 1fr))",
    });
  });
});

describe("TacticalCameraPicker adaptive mode switching, filtering, and stepping", () => {
  test("switches between inline mode (<= 6) and combobox mode (> 6)", () => {
    expect(resolvePickerMode(4, DEFAULT_PICKER_THRESHOLD)).toBe("inline");
    expect(resolvePickerMode(6, DEFAULT_PICKER_THRESHOLD)).toBe("inline");
    expect(resolvePickerMode(7, DEFAULT_PICKER_THRESHOLD)).toBe("combobox");
    expect(resolvePickerMode(100, DEFAULT_PICKER_THRESHOLD)).toBe("combobox");

    const smallHtml = renderToStaticMarkup(
      React.createElement(TacticalCameraPicker, {
        cameras: ["cam_01", "cam_02", "cam_03", "cam_04"],
        selectedCamera: "cam_02",
        onSelect: () => {},
      })
    );
    expect(smallHtml).toContain("nvr-picker-inline");
    expect(smallHtml).not.toContain("nvr-picker-combobox");

    const largeFleet = makeFleet(100);
    const largeHtml = renderToStaticMarkup(
      React.createElement(TacticalCameraPicker, {
        cameras: largeFleet,
        selectedCamera: "cam_001",
        onSelect: () => {},
      })
    );
    expect(largeHtml).toContain("nvr-picker-combobox");
    expect(largeHtml).toContain("CAM_001 (1 / 100)");
  });

  test("steps prev/next with wrap-around and filters by search query, status, and group", () => {
    const fleet = ["cam_01", "cam_02", "cam_03", "cam_04"];
    expect(stepPickerCamera(fleet, "cam_01", "prev")).toBe("cam_04");
    expect(stepPickerCamera(fleet, "cam_04", "next")).toBe("cam_01");
    expect(stepPickerCamera(fleet, "cam_02", "next")).toBe("cam_03");

    const stats: Record<string, CameraRuntimeStats> = {
      cam_01: {
        camera_fps: 15,
        process_fps: 5,
        skipped_fps: 0,
        detection_fps: 0,
        detection_enabled: true,
        pid: 101,
        capture_pid: 102,
        ffmpeg_pid: 103,
      },
      cam_02: {
        camera_fps: 0,
        process_fps: 0,
        skipped_fps: 0,
        detection_fps: 0,
        detection_enabled: true,
        pid: 0,
        capture_pid: 0,
        ffmpeg_pid: 0,
      },
    };
    const activity: Record<string, CameraLiveActivity> = {
      cam_03: {
        motion: true,
        max_Area: 8000,
        objects: [{ id: "1", label: "person", score: 0.9, stationary: false }],
      },
    };
    const groups: Record<string, CameraGroupConfig> = {
      gates: { cameras: ["cam_01", "cam_03"] },
    };

    expect(getCameraPickerStatus("cam_01", stats, activity)).toBe("live");
    expect(getCameraPickerStatus("cam_02", stats, activity)).toBe("offline");
    expect(getCameraPickerStatus("cam_03", stats, activity)).toBe("alert");

    expect(
      filterPickerCameras(fleet, {
        statusFilter: "offline",
        cameraStats: stats,
        cameraActivity: activity,
      })
    ).toEqual(["cam_02"]);

    expect(
      filterPickerCameras(fleet, {
        statusFilter: "alert",
        cameraStats: stats,
        cameraActivity: activity,
      })
    ).toEqual(["cam_03"]);

    expect(
      filterPickerCameras(fleet, {
        groupFilter: "gates",
        cameraGroups: groups,
        query: "03",
      })
    ).toEqual(["cam_03"]);
  });
});

describe("SyncWorkspace 4-camera slot capping and Auto-Pick @ Timestamp", () => {
  test("caps concurrent Sync Quad cameras at 4 and swaps slots cleanly", () => {
    const fleet100 = makeFleet(100);
    const quad = resolveSyncQuadCameras(fleet100, []);
    expect(quad.length).toBe(MAX_SYNC_QUAD_SLOTS);
    expect(quad).toEqual(["cam_001", "cam_002", "cam_003", "cam_004"]);

    // Assigning slot 1 to a new camera updates that slot
    const updated = assignSyncQuadSlot(quad, 1, "cam_050", fleet100);
    expect(updated).toEqual(["cam_001", "cam_050", "cam_003", "cam_004"]);

    // Assigning slot 0 to an already-present camera swaps the two slots without duplicates
    const swapped = assignSyncQuadSlot(updated, 0, "cam_050", fleet100);
    expect(swapped).toEqual(["cam_050", "cam_001", "cam_003", "cam_004"]);
  });

  test("autoPickActiveCamerasAtTimestamp selects the 4 most active cameras near timestamp", () => {
    const fleet100 = makeFleet(100);
    const ts = 1700001000;
    const reviews: ReviewItem[] = [
      {
        id: "r1",
        camera: "cam_042",
        start_time: ts - 5,
        end_time: ts + 10,
        severity: "alert",
        thumb_path: "",
        has_been_reviewed: false,
        data: { objects: ["person"] },
      },
      {
        id: "r2",
        camera: "cam_088",
        start_time: ts + 2,
        end_time: ts + 20,
        severity: "alert",
        thumb_path: "",
        has_been_reviewed: false,
        data: { objects: ["car"] },
      },
      {
        id: "r3",
        camera: "cam_019",
        start_time: ts - 15,
        end_time: ts,
        severity: "detection",
        thumb_path: "",
        has_been_reviewed: false,
        data: { objects: ["person"] },
      },
    ];

    const picked = autoPickActiveCamerasAtTimestamp(fleet100, reviews, ts);
    expect(picked.length).toBe(4);
    expect(picked.slice(0, 3)).toEqual(["cam_088", "cam_042", "cam_019"]);
    expect(picked[3]).toBe("cam_001");
  });
});

describe("SystemWorkspace Camera Health search, status filtering, and sorting", () => {
  const cams = ["cam_01", "cam_02", "cam_03", "cam_04"];
  const statsMap: Record<string, CameraRuntimeStats> = {
    cam_01: {
      camera_fps: 15.0,
      process_fps: 5.0,
      skipped_fps: 0,
      detection_fps: 0.0,
      detection_enabled: true,
      pid: 1,
      capture_pid: 2,
      ffmpeg_pid: 3,
      ffmpeg_cpu: "2.1",
    },
    cam_02: {
      camera_fps: 0.0,
      process_fps: 0.0,
      skipped_fps: 0,
      detection_fps: 0.0,
      detection_enabled: true,
      pid: 0,
      capture_pid: 0,
      ffmpeg_pid: 0,
      ffmpeg_cpu: "0.0",
    },
    cam_03: {
      camera_fps: 20.0,
      process_fps: 5.0,
      skipped_fps: 0,
      detection_fps: 4.5,
      detection_enabled: true,
      pid: 4,
      capture_pid: 5,
      ffmpeg_pid: 6,
      ffmpeg_cpu: "8.4",
    },
    cam_04: {
      camera_fps: 10.0,
      process_fps: 5.0,
      skipped_fps: 0,
      detection_fps: 1.2,
      detection_enabled: true,
      pid: 7,
      capture_pid: 8,
      ffmpeg_pid: 9,
      ffmpeg_cpu: "4.0",
    },
  };

  test("filters by search query and offline / active_ai status", () => {
    expect(
      filterAndSortCameraHealth(cams, statsMap, { statusFilter: "offline" })
    ).toEqual(["cam_02"]);

    expect(
      filterAndSortCameraHealth(cams, statsMap, { statusFilter: "active_ai" })
    ).toEqual(["cam_03", "cam_04"]);

    expect(
      filterAndSortCameraHealth(cams, statsMap, { searchQuery: "03" })
    ).toEqual(["cam_03"]);
  });

  test("sorts by channel, camera_fps, detection_fps, and ffmpeg_cpu", () => {
    expect(
      filterAndSortCameraHealth(cams, statsMap, {
        sortColumn: "ffmpeg_cpu",
        sortDirection: "desc",
      })
    ).toEqual(["cam_03", "cam_04", "cam_01", "cam_02"]);

    expect(
      filterAndSortCameraHealth(cams, statsMap, {
        sortColumn: "camera_fps",
        sortDirection: "asc",
      })
    ).toEqual(["cam_02", "cam_04", "cam_01", "cam_03"]);

    expect(
      filterAndSortCameraHealth(cams, statsMap, {
        sortColumn: "detection_fps",
        sortDirection: "desc",
      })
    ).toEqual(["cam_03", "cam_04", "cam_01", "cam_02"]);
  });
});
