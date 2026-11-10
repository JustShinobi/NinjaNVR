import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import { FloorplanStudioCanvas } from "../components/FloorplanStudioCanvas";
import { CameraConfig, FloorplanRoom, FloorplanWall } from "../types/nvr";
import {
  DEFAULT_FLOORPLAN_HEIGHT_M,
  DEFAULT_FLOORPLAN_WIDTH_M,
  FloorplanCadStateReducers,
  applyFloorplanCameraDrag,
  applyFloorplanFovTipDrag,
  applyFloorplanRoomDrag,
  applyFloorplanRoomDrawCommit,
  applyFloorplanRoomResize,
  applyFloorplanWallEndpointDrag,
  buildFloorplanSavePayload,
  buildFovConeSvgPath,
  computeCameraAimFromTip,
  computeDefaultMapPlacement,
  computeFloorplanSnapVertices,
  computeFovConeTipSvgPoint,
  computeRoomMetrics,
  computeWallMetrics,
  deleteSelectedFloorplanEntity,
  extractRoomBoundaryWalls,
  mapClientPointToFloorplan,
  normalizeFloorplanRoomRect,
  resizeRoomByMeters,
  resizeWallToLengthMeters,
  rotateWallToAngleDegrees,
  sanitizeFloorplanRoom,
  sanitizeFloorplanUrl,
  sanitizeFloorplanWall,
  sanitizeMapPlacement,
  snapFloorplanCoord,
  snapWallEndpoint,
} from "../utils/floorplanCadEngine";
import { resolveCanvasThemePalette } from "../utils/themeTokens";

function makeMockCameraConfig(): CameraConfig {
  return {
    enabled: true,
    detect: { enabled: true, width: 1280, height: 720, fps: 5 },
    motion: { enabled: true, threshold: 25, contour_area: 100, mask: [] },
    zones: {},
    record: { enabled: true },
    snapshots: { enabled: true },
  };
}

describe("FloorplanCadModule (floorplanCadEngine)", () => {
  test("computes room metrics and resizes rooms by meters while clamping inside floorplan bounds", () => {
    const room: FloorplanRoom = {
      id: "room_1",
      label: "SALA",
      x: 0.1,
      y: 0.25,
      w: 0.3,
      h: 0.5,
      color: "#0ea5e9",
    };

    const metrics = computeRoomMetrics(room, 20, 12);
    expect(metrics).toEqual({
      widthM: 6,
      heightM: 6,
      areaSqM: 36,
      xM: 2,
      yM: 3,
    });

    const resized = resizeRoomByMeters(room, 8, 4.8, 20, 12);
    expect(resized.w).toBe(0.4);
    expect(resized.h).toBe(0.4);
    expect(resized.x).toBe(0.1);
    expect(resized.y).toBe(0.25);

    // Near right/bottom edge, resizing shifts x/y inward so room stays within [0, 1]
    const edgeRoom: FloorplanRoom = {
      id: "room_edge",
      label: "VARANDA",
      x: 0.85,
      y: 0.85,
      w: 0.1,
      h: 0.1,
    };
    const clampedResize = resizeRoomByMeters(edgeRoom, 6, 3.6, 20, 12);
    expect(clampedResize.w).toBe(0.3);
    expect(clampedResize.h).toBe(0.3);
    expect(clampedResize.x).toBe(0.7);
    expect(clampedResize.y).toBe(0.7);
  });

  test("transforms wall length and angle in meters and degrees", () => {
    const horizPts: [number, number, number, number] = [0.1, 0.2, 0.35, 0.2];
    const metrics = computeWallMetrics(horizPts, 20, 12);
    expect(metrics.lengthM).toBe(5);
    expect(metrics.angleDeg).toBe(0);
    expect(metrics.midX).toBe(0.225);
    expect(metrics.midY).toBe(0.2);

    const resizedPts = resizeWallToLengthMeters(horizPts, 8, 20, 12);
    const resizedMetrics = computeWallMetrics(resizedPts, 20, 12);
    expect(resizedMetrics.lengthM).toBeCloseTo(8, 1);
    expect(resizedMetrics.angleDeg).toBe(0);

    const rotatedPts = rotateWallToAngleDegrees(resizedPts, 90, 20, 12);
    const rotatedMetrics = computeWallMetrics(rotatedPts, 20, 12);
    expect(rotatedMetrics.angleDeg).toBe(90);
    expect(rotatedMetrics.lengthM).toBeCloseTo(8, 1);
  });

  test("snaps wall endpoints to magnetic vertices and orthogonal/45-degree angles", () => {
    const rooms: FloorplanRoom[] = [
      { id: "r1", label: "SALA", x: 0.2, y: 0.2, w: 0.3, h: 0.25 },
    ];
    const walls: FloorplanWall[] = [
      { points: [0.6, 0.2, 0.8, 0.2], kind: "wall" },
    ];
    const vertices = computeFloorplanSnapVertices(walls, rooms);
    // 2 wall endpoints + 4 room corners = 6 vertices
    expect(vertices.length).toBe(6);

    // 1. Magnetic vertex snap within radius
    const vertexHit = snapWallEndpoint(0.1, 0.1, 0.504, 0.203, {
      snapStep: 0.005,
      orthoSnap: true,
      vertices,
      vertexSnapRadius: 0.015,
    });
    expect(vertexHit).toEqual({
      x: 0.5,
      y: 0.2,
      snappedToVertex: true,
    });

    // 2. Orthogonal horizontal snap when angle is within 7 degrees of 0 deg
    const orthoHoriz = snapWallEndpoint(0.2, 0.3, 0.45, 0.305, {
      snapStep: 0.005,
      orthoSnap: true,
      vertices: [],
      floorplanWidthM: DEFAULT_FLOORPLAN_WIDTH_M,
      floorplanHeightM: DEFAULT_FLOORPLAN_HEIGHT_M,
    });
    expect(orthoHoriz.snappedToVertex).toBe(false);
    expect(orthoHoriz.y).toBe(0.3);
    expect(orthoHoriz.x).toBe(0.45);
  });

  test("extracts 4 room boundary walls and round-trips camera aim from FOV cone tip", () => {
    const room: FloorplanRoom = {
      id: "r1",
      label: "COZINHA",
      x: 0.15,
      y: 0.2,
      w: 0.25,
      h: 0.3,
    };
    const boundary = extractRoomBoundaryWalls(room);
    expect(boundary).toEqual([
      { points: [0.15, 0.2, 0.4, 0.2], label: "COZINHA N", kind: "wall" },
      { points: [0.4, 0.2, 0.4, 0.5], label: "COZINHA E", kind: "wall" },
      { points: [0.4, 0.5, 0.15, 0.5], label: "COZINHA S", kind: "wall" },
      { points: [0.15, 0.5, 0.15, 0.2], label: "COZINHA W", kind: "wall" },
    ]);

    // Round-trip camera aim & FOV tip
    const aim = computeCameraAimFromTip(0.25, 0.25, 0.55, 0.25);
    expect(aim.angle_deg).toBe(0);
    expect(aim.range).toBe(0.3);

    const tipSvg = computeFovConeTipSvgPoint(250, 150, aim.angle_deg, aim.range * 800);
    expect(tipSvg).toEqual({ x: 490, y: 150 });

    const svgPath = buildFovConeSvgPath(250, 150, aim.angle_deg, 90, 200);
    expect(svgPath).toContain("M 250.0 150.0");
    expect(svgPath).toContain("A 200.0 200.0 0 0 1");

    const mapped = mapClientPointToFloorplan(
      550,
      350,
      { left: 50, top: 50, width: 1000, height: 600 },
      1000,
      600
    );
    expect(mapped).toEqual({ x: 0.5, y: 0.5 });
  });

  test("executes pure CAD draft reducer transitions for camera drag, FOV tip drag, room draw/drag/resize, wall drag, and entity deletion", () => {
    const initialPlacements = {
      cam_north: computeDefaultMapPlacement(0, 2),
    };

    // Camera drag clamps to [0.03, 0.97]
    const afterCamDrag = FloorplanCadStateReducers.applyFloorplanCameraDrag(
      initialPlacements,
      "cam_north",
      0.995,
      0.01
    );
    expect(afterCamDrag.cam_north.x).toBe(0.97);
    expect(afterCamDrag.cam_north.y).toBe(0.03);

    // FOV tip drag updates angle_deg and range
    const afterAimDrag = applyFloorplanFovTipDrag(
      afterCamDrag,
      "cam_north",
      0.97,
      0.33
    );
    expect(afterAimDrag.cam_north.angle_deg).toBe(90);
    expect(afterAimDrag.cam_north.range).toBe(0.3);

    // Room draw commit (dragged rectangle)
    const draftCommit = applyFloorplanRoomDrawCommit(
      [],
      { startX: 0.1, startY: 0.1, currentX: 0.4, currentY: 0.35 },
      { floorplanWidthM: 20, floorplanHeightM: 12, nowMs: 1700000000000 }
    );
    expect(draftCommit.rooms.length).toBe(1);
    expect(draftCommit.createdRoom).toEqual({
      id: "room_1700000000000_1",
      label: "SALA",
      x: 0.1,
      y: 0.1,
      w: 0.3,
      h: 0.25,
      color: "#0ea5e9",
    });

    // Room draw commit (single click creates default 3.0m x 2.4m room)
    const clickCommit = applyFloorplanRoomDrawCommit(
      draftCommit.rooms,
      { startX: 0.5, startY: 0.5, currentX: 0.5, currentY: 0.5 },
      { floorplanWidthM: 20, floorplanHeightM: 12, nowMs: 1700000000001 }
    );
    expect(clickCommit.rooms.length).toBe(2);
    expect(clickCommit.createdRoom.label).toBe("ESCRITÓRIO");
    expect(clickCommit.createdRoom.w).toBe(0.15);
    expect(clickCommit.createdRoom.h).toBe(0.2);

    // Room drag and resize
    const draggedRooms = applyFloorplanRoomDrag(
      clickCommit.rooms,
      {
        id: "room_1700000000000_1",
        startNormX: 0.2,
        startNormY: 0.2,
        origX: 0.1,
        origY: 0.1,
      },
      0.25,
      0.22,
      0.005
    );
    expect(draggedRooms[0].x).toBe(0.15);
    expect(draggedRooms[0].y).toBe(0.12);

    const resizedRooms = applyFloorplanRoomResize(
      draggedRooms,
      {
        id: "room_1700000000000_1",
        handle: "se",
        anchorX: draggedRooms[0].x,
        anchorY: draggedRooms[0].y,
        origRoom: draggedRooms[0],
      },
      0.55,
      0.42,
      0.005
    );
    expect(resizedRooms[0].w).toBe(0.4);
    expect(resizedRooms[0].h).toBe(0.3);

    // Wall endpoint & move drag
    const initialWalls: FloorplanWall[] = [
      { points: [0.2, 0.2, 0.4, 0.2], kind: "wall" },
    ];
    const movedWalls = applyFloorplanWallEndpointDrag(
      initialWalls,
      {
        index: 0,
        handle: "move",
        startNormX: 0.3,
        startNormY: 0.2,
        origPoints: [0.2, 0.2, 0.4, 0.2],
      },
      0.35,
      0.3,
      { snapStep: 0.005 }
    );
    expect(movedWalls[0].points).toEqual([0.25, 0.3, 0.45, 0.3]);

    const stretchedWalls = applyFloorplanWallEndpointDrag(
      movedWalls,
      {
        index: 0,
        handle: "end",
        startNormX: 0.45,
        startNormY: 0.3,
        origPoints: movedWalls[0].points,
      },
      0.65,
      0.3,
      { snapStep: 0.005, orthoSnap: true }
    );
    expect(stretchedWalls[0].points).toEqual([0.25, 0.3, 0.65, 0.3]);

    // Entity deletion
    const afterRoomDel = deleteSelectedFloorplanEntity({
      rooms: resizedRooms,
      walls: stretchedWalls,
      selectedRoomId: "room_1700000000000_1",
      selectedWallIdx: null,
    });
    expect(afterRoomDel.deleted).toBe("room");
    expect(afterRoomDel.rooms.length).toBe(1);
    expect(afterRoomDel.selectedRoomId).toBeNull();

    const afterWallDel = deleteSelectedFloorplanEntity({
      rooms: afterRoomDel.rooms,
      walls: afterRoomDel.walls,
      selectedRoomId: null,
      selectedWallIdx: 0,
    });
    expect(afterWallDel.deleted).toBe("wall");
    expect(afterWallDel.walls.length).toBe(0);
    expect(afterWallDel.selectedWallIdx).toBeNull();
  });

  test("sanitizes floorplan entities and builds config save payload without clobbering camera fields", () => {
    expect(sanitizeFloorplanCoordHelper()).toBe(0.24);
    expect(sanitizeFloorplanRoom({ x: -1, y: 2, w: 0.3, h: 0.2, label: "  HALL  " }, 0)).toEqual({
      id: "room_1",
      label: "HALL",
      x: 0,
      y: 0.8,
      w: 0.3,
      h: 0.2,
    });
    expect(
      sanitizeFloorplanWall({ points: [-0.2, 0.5, 1.4, 0.5], kind: "invalid_kind" })
    ).toEqual({
      points: [0, 0.5, 1, 0.5],
      kind: "wall",
    });
    expect(
      sanitizeMapPlacement({ x: 2, y: -1, angle_deg: -45, fov_deg: 200, range: 0.9 })
    ).toEqual({
      x: 1,
      y: 0,
      angle_deg: 315,
      fov_deg: 180,
      range: 0.5,
    });

    const payload = buildFloorplanSavePayload({
      existingCameras: { lobby: makeMockCameraConfig() },
      mapPlacements: {
        lobby: { x: 0.25, y: 0.75, angle_deg: 90, fov_deg: 110, range: 0.3 },
      },
      existingUi: { theme: "dark" },
      floorplanUrl: "https://example.com/floorplan.png",
      floorplanOpacity: 0.6,
      floorplanWidthM: 30,
      floorplanHeightM: 18,
      floorplanRooms: [{ id: "r1", label: "LOBBY", x: 0.1, y: 0.1, w: 0.4, h: 0.4 }],
      floorplanWalls: [{ points: [0.1, 0.1, 0.5, 0.1], kind: "door" }],
    });

    expect(payload.cameras.lobby.map).toEqual({
      x: 0.25,
      y: 0.75,
      angle_deg: 90,
      fov_deg: 110,
      range: 0.3,
    });
    expect(sanitizeFloorplanUrl("javascript:alert(1)")).toBe("");
    expect(normalizeFloorplanRoomRect(0.8, 0.7, 0.2, 0.3)).toEqual({
      x: 0.2,
      y: 0.3,
      w: 0.6,
      h: 0.4,
    });
  });

  test("renders extracted FloorplanStudioCanvas component cleanly", () => {
    const html = renderToStaticMarkup(
      React.createElement(FloorplanStudioCanvas, {
        cameraNames: ["lobby"],
        effectiveCameraNames: ["lobby"],
        effectiveCamerasConfig: { lobby: makeMockCameraConfig() },
        groupCams: ["lobby"],
        selectedCamera: "lobby",
        onSelectCamera: () => {},
        onOpenFocus: () => {},
        config: null,
        stats: null,
        cameraActivity: {},
        hardwareInfoByCam: {},
        showVectors: true,
        mapTheme: resolveCanvasThemePalette("dark"),
      })
    );
    expect(html).toContain("TACTICAL FLOORPLAN // SECTOR FOV CONES");
    expect(html).toContain("SECTOR FEED // LOBBY");
    expect(html).toContain("HEADING ANGLE:");
    expect(html).toContain("ROOMS (0) // MEDIDAS EM METROS");
  });
});

function sanitizeFloorplanCoordHelper(): number {
  return snapFloorplanCoord(0.243, 0.02);
}
