import { describe, expect, test } from "bun:test";
import type {
  RecordingDaySummary,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import {
  computeVodPlaybackWindow,
  createArchiveMetadataCache,
  resolveReviewArchivePlayback,
  resolveVodSeekAction,
  validateArchiveExportRange,
} from "../utils/archivePlayback";
import { validateExportTimeWindow } from "../views/ExportsWorkspace";

describe("archivePlayback — VOD window clamping, intra-window seeks & metadata cache", () => {
  test("computeVodPlaybackWindow clamps endSec <= nowSec on -30s rewinds and enforces >= 12s minimum span", () => {
    const nowSec = 1_700_001_000;

    // -30s rewind with default 180s duration clamps endSec to nowSec instead of requesting future segments
    const rewind30 = computeVodPlaybackWindow(nowSec - 30, {
      durationSec: 180,
      nowSec,
    });
    expect(rewind30).toEqual({
      startSec: nowSec - 30,
      endSec: nowSec,
    });
    expect(rewind30.endSec).toBeLessThanOrEqual(nowSec);
    expect(rewind30.endSec - rewind30.startSec).toBeGreaterThanOrEqual(12);

    // Near-live anchor (nowSec - 4) shifts startSec back to preserve 12s span without exceeding nowSec
    const nearEdge = computeVodPlaybackWindow(nowSec - 4, {
      durationSec: 180,
      nowSec,
    });
    expect(nearEdge).toEqual({
      startSec: nowSec - 12,
      endSec: nowSec,
    });

    // Future anchorTs clamps cleanly to [nowSec - 12, nowSec]
    const futureAnchor = computeVodPlaybackWindow(nowSec + 500, {
      durationSec: 180,
      nowSec,
    });
    expect(futureAnchor).toEqual({
      startSec: nowSec - 12,
      endSec: nowSec,
    });

    // Historical playback respects preRollSec and durationSec
    const historical = computeVodPlaybackWindow(nowSec - 600, {
      durationSec: 60,
      preRollSec: 4,
      nowSec,
    });
    expect(historical).toEqual({
      startSec: nowSec - 604,
      endSec: nowSec - 544,
    });

    // Non-finite anchor falls back safely without NaN
    const nanAnchor = computeVodPlaybackWindow(Number.NaN, {
      durationSec: 45,
      nowSec,
    });
    expect(nanAnchor).toEqual({
      startSec: nowSec - 45,
      endSec: nowSec,
    });
  });

  test("resolveVodSeekAction reuses current HLS window for intra-window seeks and reloads on cross-window or camera changes", () => {
    const nowSec = 1_700_005_000;
    const configuredCameras = { escritorio: {}, sala: {} };

    // Initial load (null currentWindow) resolves virtual _sim_01 to physical camera and loads window
    const initial = resolveVodSeekAction(null, {
      camera: "escritorio_sim_01",
      targetTs: nowSec - 200,
      nowSec,
      windowSpanSec: 120,
      configuredCameras,
    });
    expect(initial.kind).toBe("reload_window");
    expect(initial.window).toEqual({
      camera: "escritorio",
      startSec: nowSec - 200,
      endSec: nowSec - 80,
    });
    expect(initial.offsetSec).toBe(0);

    // Seeking inside [startSec, endSec - edgeMarginSec] on matching virtual channel reuses window in place
    const inWindow = resolveVodSeekAction(initial.window, {
      camera: "escritorio_sim_01",
      targetTs: nowSec - 155,
      nowSec,
      windowSpanSec: 120,
      edgeMarginSec: 2,
      configuredCameras,
    });
    expect(inWindow.kind).toBe("seek_in_window");
    expect(inWindow.offsetSec).toBe(45);
    expect(inWindow.window).toBe(initial.window);

    // Seeking within the 2s tail edgeMargin triggers reload_window so HLS buffer doesn't stall at EOF
    const nearTail = resolveVodSeekAction(initial.window, {
      camera: "escritorio_sim_01",
      targetTs: initial.window.endSec - 1,
      nowSec,
      windowSpanSec: 120,
      edgeMarginSec: 2,
      configuredCameras,
    });
    expect(nearTail.kind).toBe("reload_window");
    expect(nearTail.window.camera).toBe("escritorio");
    expect(nearTail.window.startSec).toBe(initial.window.endSec - 1);

    // Seeking before startSec triggers reload_window
    const beforeStart = resolveVodSeekAction(initial.window, {
      camera: "escritorio",
      targetTs: initial.window.startSec - 40,
      nowSec,
      windowSpanSec: 120,
      configuredCameras,
    });
    expect(beforeStart.kind).toBe("reload_window");
    expect(beforeStart.window.startSec).toBe(initial.window.startSec - 40);

    // Switching to another camera triggers reload_window even at an overlapping timestamp
    const otherCam = resolveVodSeekAction(initial.window, {
      camera: "sala_sim_02",
      targetTs: nowSec - 160,
      nowSec,
      windowSpanSec: 120,
      configuredCameras,
    });
    expect(otherCam.kind).toBe("reload_window");
    expect(otherCam.window.camera).toBe("sala");
  });

  test("resolveReviewArchivePlayback keeps ReviewItem bounds and exportWindow immutable across trigger jumps and timeline scrubs", () => {
    const nowSec = 1_700_010_000;
    const review: ReviewItem = {
      id: "rev_immutable_1",
      camera: "escritorio_sim_01",
      start_time: 1_700_009_000,
      end_time: 1_700_009_030,
      severity: "alert",
      thumb_path: "/clips/rev_immutable_1.webp",
      has_been_reviewed: false,
      data: { objects: ["person"] },
    };
    const originalSnapshot = structuredClone(review);

    // 1. Jump to trigger frame inside [start_time, end_time] reuses incident VOD window
    const triggerJump = resolveReviewArchivePlayback(review, 1_700_009_012, {
      nowSec,
      configuredCameras: ["escritorio", "sala"],
    });
    expect(triggerJump.realCamera).toBe("escritorio");
    expect(triggerJump.seekAction?.kind).toBe("seek_in_window");
    expect(triggerJump.vodWindow).toEqual({
      camera: "escritorio",
      startSec: 1_700_008_996, // 1700009000 - 4s pre-roll
      endSec: 1_700_009_036, // 1700009030 + 6s post-roll
    });
    expect(triggerJump.seekAction?.offsetSec).toBe(16);

    // 2. Scrubbing 5 minutes earlier reloads VOD window around playhead WITHOUT mutating review or exportWindow
    const farScrub = resolveReviewArchivePlayback(review, 1_700_008_700, {
      currentWindow: triggerJump.vodWindow,
      nowSec,
      configuredCameras: ["escritorio", "sala"],
    });
    expect(farScrub.seekAction?.kind).toBe("reload_window");
    expect(farScrub.vodWindow).toEqual({
      camera: "escritorio",
      startSec: 1_700_008_696, // targetTs - 4s pre-roll
      endSec: 1_700_008_756,
    });
    // True review export window [start_time - 5, end_time + 5] is completely preserved!
    expect(farScrub.exportWindow).toEqual({
      startSec: 1_700_008_995,
      endSec: 1_700_009_035,
    });
    expect(review).toEqual(originalSnapshot);
  });

  test("validateArchiveExportRange and validateExportTimeWindow share identical validation contracts", () => {
    expect(validateArchiveExportRange(1000.9, 1060.2)).toEqual({
      valid: true,
      startSec: 1000,
      endSec: 1061,
      durationSec: 61,
    });
    expect(validateExportTimeWindow(1000.9, 1060.2)).toEqual(
      validateArchiveExportRange(1000.9, 1060.2)
    );
    expect(validateArchiveExportRange(500, 500).valid).toBe(false);
    expect(validateArchiveExportRange(-5, 100).valid).toBe(false);
    expect(validateArchiveExportRange(0, 90000, 86400).valid).toBe(false);
  });

  test("createArchiveMetadataCache deduplicates concurrent/overlapping recordings & summary fetches and resolves _sim_01", async () => {
    let clockMs = 50_000;
    const requestedUrls: string[] = [];
    const sampleSegments: RecordingSegment[] = [
      {
        id: "seg_early",
        start_time: 1_700_000_100,
        end_time: 1_700_000_200,
        duration: 100,
        motion: 2,
        objects: 0,
        segment_size: 1024,
      },
      {
        id: "seg_recent",
        start_time: 1_700_050_000,
        end_time: 1_700_050_060,
        duration: 60,
        motion: 5,
        objects: 1,
        segment_size: 2048,
      },
    ];
    const sampleDays: RecordingDaySummary[] = [
      {
        day: "2026-11-14",
        events: 3,
        hours: [],
      },
    ];

    const mockFetch = (async (input: RequestInfo | URL) => {
      const url = String(input);
      requestedUrls.push(url);
      await new Promise((r) => setTimeout(r, 10));
      if (url.includes("/recordings/summary")) {
        return new Response(JSON.stringify(sampleDays), { status: 200 });
      }
      return new Response(JSON.stringify(sampleSegments), { status: 200 });
    }) as unknown as typeof fetch;

    const cache = createArchiveMetadataCache({
      fetchImpl: mockFetch,
      ttlMs: 3000,
      nowMs: () => clockMs,
      getConfiguredCameras: () => ({ escritorio: {}, cozinha: {} }),
    });

    // App.tsx 24h recordings fetch + TimelineScrubber sub-range fetch for `escritorio_sim_01` run concurrently
    const [appSegs, scrubberSegs, sumA, sumB] = await Promise.all([
      cache.getRecordings("escritorio", {
        after: 1_700_000_000,
        before: 1_700_086_400,
      }),
      cache.getRecordings("escritorio_sim_01", {
        after: 1_700_040_000,
        before: 1_700_060_000,
      }),
      cache.getSummary("escritorio"),
      cache.getSummary("escritorio_sim_01"),
    ]);

    // Only ONE recordings request and ONE summary request hit the network, both targeting physical `escritorio`
    expect(requestedUrls).toEqual([
      "/api/escritorio/recordings?after=1700000000&before=1700086400",
      "/api/escritorio/recordings/summary",
    ]);
    expect(appSegs.map((s) => s.id)).toEqual(["seg_early", "seg_recent"]);
    // Overlapping sub-window slice returns only segments intersecting [1700040000, 1700060000]
    expect(scrubberSegs.map((s) => s.id)).toEqual(["seg_recent"]);
    expect(sumA).toEqual(sampleDays);
    expect(sumB).toEqual(sampleDays);

    // Within TTL (clock +1000ms), another sub-range call reuses cached 24h window with zero network hits
    clockMs += 1000;
    const cachedSlice = await cache.getRecordings("escritorio_sim_01", {
      after: 1_700_000_050,
      before: 1_700_000_300,
    });
    expect(cachedSlice.map((s) => s.id)).toEqual(["seg_early"]);
    expect(requestedUrls.length).toBe(2);

    // After TTL expires (clock +4000ms), cache refetches cleanly
    clockMs += 4000;
    await cache.getSummary("escritorio_sim_01");
    expect(requestedUrls.length).toBe(3);
    expect(requestedUrls[2]).toBe("/api/escritorio/recordings/summary");
  });
});
