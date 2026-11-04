import { beforeEach, describe, expect, test } from "bun:test";
import type { ExportItem } from "../types/nvr";
import {
  findMatchingExport,
  formatClockTime,
  formatDateTime,
  formatDisplayVersion,
  formatDuration,
  formatRelTime,
  formatTs,
  formatUptime,
  resolveHeaderTelemetryMeta,
  resolveInitialBool,
  resolveInitialFirstWeekday,
  resolveInitialHudPosition,
  resolveInitialLiveLayout,
  resolveInitialPlaybackSpeed,
  resolveInitialSpotlightTimelineMode,
  resolveInitialTab,
} from "../utils/format";

describe("format utilities", () => {
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
  });

  test("formatTs, formatClockTime, and formatDateTime format unix seconds consistently", () => {
    const ts = 1700000000;
    const clock = formatClockTime(ts);
    expect(clock).toBe(formatTs(ts));
    expect(clock).toMatch(/^\d{2}:\d{2}:\d{2}$/);

    const dt = formatDateTime(ts);
    expect(dt.length).toBeGreaterThan(6);
    expect(dt).toMatch(/\d{2}:\d{2}:\d{2}/);
  });

  test("formatDuration formats seconds, minutes, and hours", () => {
    expect(formatDuration(0)).toBe("1s");
    expect(formatDuration(45)).toBe("45s");
    expect(formatDuration(65)).toBe("1m 05s");
    expect(formatDuration(3661)).toBe("1h 1m 1s");
  });

  test("formatRelTime and formatUptime format relative and elapsed durations", () => {
    const nowSec = Date.now() / 1000;
    expect(formatRelTime(nowSec - 12)).toMatch(/^\d+s ago$/);
    expect(formatRelTime(nowSec - 180)).toBe("3m ago");
    expect(formatRelTime(nowSec - 7200)).toBe("2h ago");
    expect(formatRelTime(nowSec - 172800)).toBe("2d ago");

    expect(formatUptime(3665)).toBe("1h 1m 5s");
    expect(formatUptime(59)).toBe("0h 0m 59s");
  });

  test("findMatchingExport matches overlapping export IDs and fallback names", () => {
    const exportsList: ExportItem[] = [
      {
        id: "front_door_1000_1060",
        camera: "front_door",
        name: "Front door clip",
        date: 1000,
        video_path: "/exports/front_door_1000_1060.mp4",
        thumb_path: "/clips/front_door_1000_1060.webp",
        in_progress: false,
      },
      {
        id: "custom_id",
        camera: "backyard",
        name: "Incident at 2500",
        date: 2500,
        video_path: "/exports/custom_id.mp4",
        thumb_path: "/clips/custom_id.webp",
        in_progress: false,
      },
    ];

    expect(findMatchingExport([], "front_door", 1000, 1010)).toBeNull();
    expect(findMatchingExport(exportsList, "front_door", 1010, 1020)?.id).toBe(
      "front_door_1000_1060"
    );
    expect(findMatchingExport(exportsList, "front_door", 2000, 2020)).toBeNull();
    expect(findMatchingExport(exportsList, "backyard", 2500, null)?.id).toBe("custom_id");
  });

  test("workspace tab and localStorage preference resolvers handle all routes and values", () => {
    const win = (globalThis as unknown as {
      window: {
        location: { pathname: string };
        localStorage: {
          setItem: (k: string, v: string) => void;
        };
      };
    }).window;

    const routeMap: Array<[string, string]> = [
      ["/", "live"],
      ["/sync", "sync"],
      ["/review", "review"],
      ["/explore", "explore"],
      ["/exports", "exports"],
      ["/settings", "zones"],
      ["/config", "zones"],
      ["/system", "system"],
      ["/logs", "system"],
    ];
    for (const [path, expected] of routeMap) {
      win.location.pathname = path;
      expect(resolveInitialTab()).toBe(expected as ReturnType<typeof resolveInitialTab>);
    }

    expect(resolveInitialHudPosition()).toBe("bottom-right");
    win.localStorage.setItem("ninjanvr_hud_position", "top-left");
    expect(resolveInitialHudPosition()).toBe("top-left");

    expect(resolveInitialLiveLayout()).toBe("2x2");
    win.localStorage.setItem("ninjanvr_live_layout", "spotlight");
    expect(resolveInitialLiveLayout()).toBe("spotlight");

    expect(resolveInitialBool("flag_x", true)).toBe(true);
    win.localStorage.setItem("flag_x", "false");
    expect(resolveInitialBool("flag_x", true)).toBe(false);
    win.localStorage.setItem("flag_x", "true");
    expect(resolveInitialBool("flag_x", false)).toBe(true);

    expect(resolveInitialPlaybackSpeed()).toBe(1);
    win.localStorage.setItem("ninjanvr_playback_speed", "2");
    expect(resolveInitialPlaybackSpeed()).toBe(2);

    expect(resolveInitialFirstWeekday()).toBe("sunday");
    win.localStorage.setItem("ninjanvr_first_weekday", "monday");
    expect(resolveInitialFirstWeekday()).toBe("monday");

    expect(resolveInitialSpotlightTimelineMode()).toBe("reactive");
    expect(resolveInitialSpotlightTimelineMode("wall")).toBe("wall");
    win.localStorage.setItem("ninjanvr_spotlight_timeline_mode", "wall");
    expect(resolveInitialSpotlightTimelineMode()).toBe("wall");
    win.localStorage.setItem("ninjanvr_spotlight_timeline_mode", "reactive");
    expect(resolveInitialSpotlightTimelineMode("wall")).toBe("reactive");
    win.localStorage.setItem("ninjanvr_spotlight_timeline_mode", "invalid_mode");
    expect(resolveInitialSpotlightTimelineMode()).toBe("reactive");
    expect(resolveInitialSpotlightTimelineMode("wall")).toBe("wall");
  });

  test("formatDisplayVersion and resolveHeaderTelemetryMeta format brand version and header telemetry status", () => {
    expect(formatDisplayVersion("1.9.9-rc.1", "CONSOLE")).toBe("v1.9.9-rc.1");
    expect(formatDisplayVersion("v1.9.9-rc.1", "CONSOLE")).toBe("v1.9.9-rc.1");
    expect(formatDisplayVersion("   ", "CONSOLE")).toBe("CONSOLE");
    expect(formatDisplayVersion(undefined, "CONSOLE")).toBe("CONSOLE");

    const online = resolveHeaderTelemetryMeta(true, 4);
    expect(online.wsDotClass).toBe("live");
    expect(online.channelBadge).toBe("4 CH");
    expect(online.tooltip).toContain("4 CH");
    expect(online.tooltip).toContain("Real-time WebSocket stream online");

    const reconnecting = resolveHeaderTelemetryMeta(false, 16);
    expect(reconnecting.wsDotClass).toBe("motion");
    expect(reconnecting.channelBadge).toBe("16 CH");
    expect(reconnecting.tooltip).toContain("16 CH");
    expect(reconnecting.tooltip).toContain("WebSocket reconnecting");
  });
});
