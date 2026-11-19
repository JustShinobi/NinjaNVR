import { describe, expect, test } from "bun:test";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  formatDateTime,
  formatRelTime,
  formatTs,
  resolveHeaderTelemetryMeta,
} from "../utils/format";
import {
  sanitizeFloorplanRoom,
  sanitizeFloorplanWall,
} from "../utils/floorplanCadEngine";
import { detectProcessRestartSections } from "../utils/settingsUx";
import {
  buildEventsFeedUrl,
  buildReviewFeedUrl,
} from "../utils/operationalFeed";
import { createConfigWorkbenchSession } from "../utils/configWorkbench";
import { resolveVodFrameStepDurationSec } from "../components/VideoPlayers";
import { CameraDetailsSheet } from "../components/CameraDetailsSheet";
import { TacticalCameraPicker } from "../components/TacticalCameraPicker";
import { createArchiveMetadataCache } from "../utils/archivePlayback";
import type { NinjaConfigResponse } from "../types/nvr";

describe("UI/UX Remediation Waves — Formatters, Floorplan, Workbench, RBAC & VOD Step", () => {
  test("formatRelTime and resolveHeaderTelemetryMeta localize PT-BR and EN cleanly", () => {
    const nowSec = Math.floor(Date.now() / 1000);
    expect(formatRelTime(nowSec - 180, "en")).toBe("3m ago");
    expect(formatRelTime(nowSec - 180, "pt")).toBe("há 3m");
    expect(formatRelTime(Number.NaN, "pt")).toBe("há 0s");

    const enMeta = resolveHeaderTelemetryMeta(true, 6, "en");
    const ptMeta = resolveHeaderTelemetryMeta(false, 6, "pt");
    expect(enMeta.wsStatusLabel).toContain("Real-time WebSocket");
    expect(ptMeta.wsStatusLabel).toContain("WebSocket reconectando");
  });

  test("formatTs and formatDateTime honor ui.time_format (12h vs 24h)", () => {
    const sampleEpoch = 1_763_568_900; // deterministic timestamp
    const s24 = formatTs(sampleEpoch, { locale: "en", timeFormat: "24h" });
    const s12 = formatTs(sampleEpoch, { locale: "en", timeFormat: "12h" });
    expect(s24).not.toMatch(/AM|PM/i);
    expect(s12).toMatch(/AM|PM/i);
    expect(formatDateTime(Number.NaN, { locale: "pt" })).toBe("--:--:--");
  });

  test("sanitizeFloorplanRoom accepts YAML `name` alias and sanitizeFloorplanWall accepts `{x1,y1,x2,y2}`", () => {
    const room = sanitizeFloorplanRoom({
      id: "r_lobby",
      label: "",
      name: "Main Lobby",
      x: 0.1,
      y: 0.2,
      w: 0.3,
      h: 0.4,
    });
    expect(room.label).toBe("Main Lobby");

    const wall = sanitizeFloorplanWall({
      x1: 0.08,
      y1: 0.12,
      x2: 0.92,
      y2: 0.12,
      kind: "door",
    });
    expect(wall.points).toEqual([0.08, 0.12, 0.92, 0.12]);
    expect(wall.kind).toBe("door");
  });

  test("detectProcessRestartSections flags tls, database, and detectors but NOT hot-reloaded mqtt", () => {
    const base = {
      mqtt: { enabled: false, port: 1883 },
      tls: { enabled: true },
      database: { path: "/config/ninjanvr.db" },
    };
    const onlyMqttChanged = {
      ...base,
      mqtt: { enabled: true, port: 1884 },
    };
    expect(detectProcessRestartSections(base, onlyMqttChanged)).toEqual([]);

    const tlsAndDbChanged = {
      ...base,
      tls: { enabled: false },
      database: { path: "/config/other.db" },
    };
    expect(detectProcessRestartSections(base, tlsAndDbChanged)).toEqual([
      "tls",
      "database",
    ]);
  });

  test("ConfigWorkbenchSession surfaces hasRemoteConflict on concurrent WS config update during Raw YAML edits and checks triggerRestart failure", async () => {
    const initialCfg: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      cameras: {},
    };
    let restartShouldFail = true;
    const session = createConfigWorkbenchSession(
      {
        fetchStructuredConfig: async () => initialCfg,
        fetchRawYaml: async () => 'version: "0.18-0"\ncameras: {}\n',
        commitStructured: async () => ({
          ok: true,
          status: 200,
          config: initialCfg,
          rawYaml: 'version: "0.18-0"\ncameras: {}\n',
        }),
        commitStructuredPatch: async () => ({
          ok: true,
          status: 200,
          config: initialCfg,
          rawYaml: 'version: "0.18-0"\ncameras: {}\n',
        }),
        commitRawYaml: async (yaml) => ({
          ok: true,
          status: 200,
          config: initialCfg,
          rawYaml: yaml,
        }),
        triggerRestart: async () => !restartShouldFail,
      },
      {
        initialConfig: initialCfg,
        initialRawYaml: 'version: "0.18-0"\ncameras: {}\n',
      }
    );

    session.setRawYamlDraft('version: "0.18-0"\n# local comment\ncameras: {}\n');
    expect(session.getSnapshot().hasUnsavedRawEdits).toBe(true);
    expect(session.getSnapshot().hasRemoteConflict).toBe(false);

    // Concurrent server update arrives via WebSocket
    session.hydrateFromServer({
      version: "0.18-0",
      safe_mode: false,
      cameras: {
        front_gate: {
          enabled: true,
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    });
    expect(session.getSnapshot().hasRemoteConflict).toBe(true);

    // Failed triggerRestart reports ok: false in saveOutcome message instead of lying
    const commitRes = await session.commitRaw({ restart: true });
    expect(commitRes.message).toContain("pipeline reload failed");
    expect(session.getSnapshot().saveOutcome?.ok).toBe(false);
    expect(session.getSnapshot().hasRemoteConflict).toBe(false);
  });

  test("buildReviewFeedUrl and buildEventsFeedUrl encode server-side filter & cursor params", () => {
    const revUrl = buildReviewFeedUrl(null, 150, {
      camera: "front_gate",
      severity: "alert",
      status: "unreviewed",
      before: 1700000500,
    });
    expect(revUrl).toBe(
      "/api/review?before=1700000500&cameras=front_gate&severity=alert&reviewed=0&limit=150"
    );

    const evUrl = buildEventsFeedUrl(null, 100, {
      camera: "loading_dock",
      label: "truck",
      minScore: 80,
      plate: "ABC1D23",
      retainedOnly: true,
      before: 1700000900,
    });
    expect(evUrl).toBe(
      "/api/events?before=1700000900&cameras=loading_dock&labels=truck&min_score=0.8&favorites=1&plate=ABC1D23&limit=100"
    );
  });

  test("resolveVodFrameStepDurationSec uses recorded main-stream 25 FPS instead of 5 FPS substream AI detect.fps", () => {
    expect(
      resolveVodFrameStepDurationSec({
        enabled: true,
        detect: { enabled: true, width: 1280, height: 720, fps: 5 },
        motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
        zones: {},
        record: { enabled: true },
        snapshots: { enabled: true },
      })
    ).toBeCloseTo(1 / 25, 5);
    expect(resolveVodFrameStepDurationSec(null, 30)).toBeCloseTo(1 / 30, 5);
  });

  test("CameraDetailsSheet disables pipeline switches when canEdit is false and enables when true (RBAC)", () => {
    const readOnlyHtml = renderToStaticMarkup(
      React.createElement(CameraDetailsSheet, {
        camera: "front_gate",
        telemetry: [{ key: "camera_fps", label: "Camera", value: "0.0 FPS" }],
        flags: { enabled: true, detect: true, record: true, alerts: true },
        onToggleFlag: () => {},
        useMainStream: false,
        onSetMainStream: () => {},
        showVectors: false,
        onSetShowVectors: () => {},
        onClose: () => {},
        canEdit: false,
      })
    );
    expect(readOnlyHtml).toMatch(/<button[^>]*data-testid="flag-record"[^>]*disabled/);

    const adminHtml = renderToStaticMarkup(
      React.createElement(CameraDetailsSheet, {
        camera: "front_gate",
        telemetry: [{ key: "camera_fps", label: "Camera", value: "8.0 FPS" }],
        flags: { enabled: true, detect: true, record: true, alerts: true },
        onToggleFlag: () => {},
        useMainStream: false,
        onSetMainStream: () => {},
        showVectors: false,
        onSetShowVectors: () => {},
        onClose: () => {},
        canEdit: true,
      })
    );
    expect(adminHtml).toContain('data-testid="flag-record"');
    expect(adminHtml).not.toMatch(/<button[^>]*data-testid="flag-record"[^>]*disabled/);
  });

  test("TacticalCameraPicker renders live and offline status dots accurately in inline and combobox modes", () => {
    const sampleStats = {
      cam_live: {
        camera_fps: 15,
        process_fps: 5,
        skipped_fps: 0,
        detection_fps: 0,
        detection_enabled: true,
        pid: 1,
        capture_pid: 2,
        ffmpeg_pid: 3,
      },
      cam_offline: {
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

    const inlineHtml = renderToStaticMarkup(
      React.createElement(TacticalCameraPicker, {
        cameras: ["cam_live", "cam_offline"],
        selectedCamera: "cam_offline",
        cameraStats: sampleStats,
        onSelect: () => {},
      })
    );
    expect(inlineHtml).toContain("nvr-dot live");
    expect(inlineHtml).toContain("nvr-dot alert");

    const comboboxHtml = renderToStaticMarkup(
      React.createElement(TacticalCameraPicker, {
        cameras: [
          "cam_offline",
          "cam_live",
          "cam_3",
          "cam_4",
          "cam_5",
          "cam_6",
          "cam_7",
        ],
        selectedCamera: "cam_offline",
        cameraStats: sampleStats,
        onSelect: () => {},
      })
    );
    expect(comboboxHtml).toContain("nvr-dot alert");
  });

  test("ArchiveMetadataCache.getSummary appends tz_offset_min when provided and preserves default URL when omitted", async () => {
    const requestedUrls: string[] = [];
    const cache = createArchiveMetadataCache(async (input) => {
      requestedUrls.push(String(input));
      return new Response(
        JSON.stringify([{ day: "2026-11-18", events: 1, hours: [] }]),
        { status: 200, headers: { "Content-Type": "application/json" } }
      );
    });

    await cache.getSummary("front_gate");
    await cache.getSummary("front_gate", { tzOffsetMin: 180 });
    // Second call with same tzOffsetMin hits cache; after invalidate("front_gate"), it refetches!
    await cache.getSummary("front_gate", { tzOffsetMin: 180 });
    expect(requestedUrls).toEqual([
      "/api/front_gate/recordings/summary",
      "/api/front_gate/recordings/summary?tz_offset_min=180",
    ]);

    cache.invalidate("front_gate");
    await cache.getSummary("front_gate", { tzOffsetMin: 180 });
    expect(requestedUrls).toEqual([
      "/api/front_gate/recordings/summary",
      "/api/front_gate/recordings/summary?tz_offset_min=180",
      "/api/front_gate/recordings/summary?tz_offset_min=180",
    ]);
  });
});
