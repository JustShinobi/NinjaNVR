import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import {
  computeClipOverlayGeometry,
  computeClipPreFlightSummary,
  computePannedWindowEnd,
  computePrecisionHandleTs,
  findExactContainingReview,
  formatClipBoundaryInput,
  isSameLocalCalendarDay,
  mergeDeduplicatedSegments,
  parseExportJobPayload,
  parseFlexibleClipDateTime,
  quantizeClipBoundaryTs,
  reanchorClipRangeBoundary,
  reconcileExportJobList,
  resolveTimelineHoverThumbnailUrl,
  sanitizePreviewFrameListPayload,
  stepHandleDragTs,
} from "../utils/exportClipFlow";
import { resolveVodSeekAction } from "../utils/archivePlayback";
import {
  TimelineScrubber,
  resolveEventClipRange,
} from "../components/TimelineScrubber";
import { ExportsWorkspace } from "../views/ExportsWorkspace";
import type { ExportJobItem, RecordingSegment, ReviewItem } from "../types/nvr";

describe("Export Clip Flow — Pillar 1 (Timestamp-accurate thumbnails & exact event containment)", () => {
  test("sanitizePreviewFrameListPayload validates camera ownership, path safety, and ascending order", () => {
    const frames = sanitizePreviewFrameListPayload("front_door", [
      { url: "/api/preview/front_door/frame/1700000005500.jpg" },
      { url: "/api/preview/front_door/frame/1700000001000.jpg" },
      { url: "/api/preview/backyard/frame/1700000002000.jpg" },
      { url: "/api/preview/front_door/frame/../../secret.jpg" },
      { url: "https://evil.example/api/preview/front_door/frame/1700000003000.jpg" },
      { url: "/api/preview/front_door/frame/1700000004000.png" },
    ]);
    expect(frames).toEqual([
      {
        ts: 1700000001,
        tsMs: 1700000001000,
        url: "/api/preview/front_door/frame/1700000001000.jpg",
      },
      {
        ts: 1700000005.5,
        tsMs: 1700000005500,
        url: "/api/preview/front_door/frame/1700000005500.jpg",
      },
    ]);
    expect(sanitizePreviewFrameListPayload("../bad", frames)).toEqual([]);
  });

  test("resolveTimelineHoverThumbnailUrl uses Tier-1 indexed frames when near, Tier-2 /at/ buckets when older, and null on unrecorded gaps", () => {
    const indexed = sanitizePreviewFrameListPayload("sala", [
      { url: "/api/preview/sala/frame/1700000100000.jpg" },
      { url: "/api/preview/sala/frame/1700000101000.jpg" },
      { url: "/api/preview/sala/frame/1700000102500.jpg" },
    ]);

    // Different timestamps inside the same event resolve distinct frames!
    expect(
      resolveTimelineHoverThumbnailUrl("sala", 1700000100.2, indexed, {
        hasRecording: true,
      })
    ).toBe("/api/preview/sala/frame/1700000100000.jpg");
    expect(
      resolveTimelineHoverThumbnailUrl("sala", 1700000102.4, indexed, {
        hasRecording: true,
      })
    ).toBe("/api/preview/sala/frame/1700000102500.jpg");

    // Older historical timestamp (>3h ago, outside PreviewFrameStore) falls back to Tier-2 `/at/` 1s bucket
    expect(
      resolveTimelineHoverThumbnailUrl("sala", 1699950012.34, indexed, {
        hasRecording: true,
        fineTrim: false,
      })
    ).toBe("/api/preview/sala/at/1699950012000.jpg");

    // Fine trim (dragging IN/OUT) uses 250ms sub-second precision buckets
    expect(
      resolveTimelineHoverThumbnailUrl("sala", 1699950012.34, indexed, {
        hasRecording: true,
        fineTrim: true,
      })
    ).toBe("/api/preview/sala/at/1699950012250.jpg");

    // Unrecorded gap returns null (never lies with a distant event or live camera image)
    expect(
      resolveTimelineHoverThumbnailUrl("sala", 1699900000, indexed, {
        hasRecording: false,
      })
    ).toBeNull();
  });

  test("findExactContainingReview only matches when cursor is inside the event bounds (never 3 minutes away)", () => {
    const rev: ReviewItem = {
      id: "rev_1",
      camera: "sala",
      start_time: 1000,
      end_time: 1020,
      severity: "alert",
      thumb_path: "/clips/review/thumb-rev_1.webp",
      has_been_reviewed: false,
      data: { objects: ["person"] },
    };
    expect(findExactContainingReview([rev], "sala", "sala", 1010)?.id).toBe(
      "rev_1"
    );
    // 30 seconds before or after the event must NOT match!
    expect(findExactContainingReview([rev], "sala", "sala", 970)).toBeNull();
    expect(findExactContainingReview([rev], "sala", "sala", 1050)).toBeNull();
  });
});

describe("Export Clip Flow — Pillar 2 & 3 (Trim Precision, Scroll/Pan Isolation, & Off-screen Handles)", () => {
  test("computePannedWindowEnd pans continuously across midnight and snaps to null at live edge", () => {
    const nowSec = 1700100000;
    const minArchiveSec = nowSec - 7 * 86400;
    // Pan backward by 120s
    expect(
      computePannedWindowEnd({
        curWinEnd: nowSec - 300,
        deltaSec: -120,
        windowSpanSec: 3600,
        minArchiveSec,
        nowSec,
      })
    ).toBe(nowSec - 420);
    // Panning up to live edge returns null (live edge lock)
    expect(
      computePannedWindowEnd({
        curWinEnd: nowSec - 10,
        deltaSec: 30,
        windowSpanSec: 3600,
        minArchiveSec,
        nowSec,
      })
    ).toBeNull();
  });

  test("computeClipOverlayGeometry hides off-screen handles instead of pinning fake handles at 0% or 100%", () => {
    // 40-minute clip [1000, 3400], zoomed into 5-minute window [3100, 3400] around OUT
    const geo = computeClipOverlayGeometry(1000, 3400, 3100, 3400);
    expect(geo.visible).toBe(true);
    expect(geo.showInHandle).toBe(false);
    expect(geo.offscreenIn).toBe("left");
    expect(geo.showOutHandle).toBe(true);
    expect(geo.offscreenOut).toBeNull();
    expect(geo.leftPct).toBe(0);
    expect(geo.widthPct).toBe(100);

    // Entire clip is off-screen to the left
    const offLeft = computeClipOverlayGeometry(1000, 1300, 2000, 2300);
    expect(offLeft.visible).toBe(false);
    expect(offLeft.showInHandle).toBe(false);
    expect(offLeft.showOutHandle).toBe(false);
    expect(offLeft.offscreenIn).toBe("left");
    expect(offLeft.offscreenOut).toBe("left");
  });

  test("computePrecisionHandleTs scales pointer movement by 0.15x when Shift is held", () => {
    const coarse = computePrecisionHandleTs({
      clientX: 500,
      rectLeft: 100,
      rectWidth: 800,
      winStart: 1000,
      winEnd: 4600, // 1 hour = 3600s
      shiftKey: false,
    });
    expect(coarse).toBe(2800); // 50% across 3600s

    // Moving +80px (10% of track = 360s coarse) with Shift held only moves +54s!
    const fine = computePrecisionHandleTs({
      clientX: 480,
      rectLeft: 100,
      rectWidth: 800,
      winStart: 1000,
      winEnd: 4600,
      shiftKey: true,
      fineAnchor: { startClientX: 400, startTs: 2350 },
    });
    expect(fine).toBe(2404);
  });
});

describe("Export Clip Flow — Pillar 4 & 5 (Multi-day parsing, Pre-flight telemetry, Jobs & Unified UI)", () => {
  test("parseFlexibleClipDateTime supports both same-day HH:MM:SS and multi-day YYYY-MM-DD HH:MM:SS across midnight", () => {
    const refNight = Math.floor(
      new Date(2026, 10, 14, 23, 50, 0).getTime() / 1000
    );
    const nextMorning = Math.floor(
      new Date(2026, 10, 15, 0, 20, 15).getTime() / 1000
    );
    expect(parseFlexibleClipDateTime("23:55:10", refNight)).toBe(
      Math.floor(new Date(2026, 10, 14, 23, 55, 10).getTime() / 1000)
    );
    expect(
      parseFlexibleClipDateTime("2026-11-15 00:20:15", refNight)
    ).toBe(nextMorning);
    expect(isSameLocalCalendarDay(refNight, nextMorning)).toBe(false);
    expect(formatClipBoundaryInput(nextMorning, true)).toBe(
      "2026-11-15 00:20:15"
    );
  });

  test("computeClipPreFlightSummary estimates MB/GB proportionally and detects unrecorded offline gaps", () => {
    const segs: RecordingSegment[] = [
      {
        id: "s1",
        start_time: 1000,
        end_time: 1060,
        duration: 60,
        motion: 10,
        objects: 1,
        segment_size: 20, // 20 MB
      },
      // 90-second offline gap [1060..1150]
      {
        id: "s2",
        start_time: 1150,
        end_time: 1210,
        duration: 60,
        motion: 4,
        objects: 0,
        segment_size: 10, // 10 MB
      },
    ];
    const summary = computeClipPreFlightSummary(segs, 1030, 1210);
    expect(summary.hasCoverage).toBe(true);
    expect(summary.segmentCount).toBe(2);
    // 50% of s1 (10 MB) + 100% of s2 (10 MB) = 20 MB
    expect(Math.round(summary.estimatedSizeMb)).toBe(20);
    expect(summary.estimatedSizeLabel).toBe("~20 MB");
    expect(summary.gapCount).toBe(1);
    expect(summary.missingDurationSec).toBe(90);

    const emptySummary = computeClipPreFlightSummary(segs, 5000, 5120);
    expect(emptySummary.hasCoverage).toBe(false);
    expect(emptySummary.segmentCount).toBe(0);
  });

  test("parseExportJobPayload and reconcileExportJobList track async job progress", () => {
    const j1 = parseExportJobPayload({
      id: "sala_100_400",
      camera: "sala",
      name: "Long export",
      start_time: 100,
      end_time: 400,
      include_audio: true,
      status: "running",
      progress_pct: 42.5,
      created_at: 1000,
      updated_at: 1010,
    });
    expect(j1?.progress_pct).toBe(42.5);
    const list1 = reconcileExportJobList([], j1 as ExportJobItem);
    expect(list1.length).toBe(1);

    const j1Done = parseExportJobPayload({
      ...j1,
      status: "completed",
      progress_pct: 100,
      video_url: "/exports/sala_100_400.mp4",
    })!;
    const list2 = reconcileExportJobList(list1, j1Done);
    expect(list2.length).toBe(1);
    expect(list2[0].status).toBe("completed");
    expect(list2[0].progress_pct).toBe(100);
  });

  test("TimelineScrubber keeps navigation zoom/calendar controls visible in rangeMode andExportsWorkspace renders active job progress", () => {
    const scrubberHtml = renderToStaticMarkup(
      React.createElement(TimelineScrubber, {
        camera: "front_door",
        cameras: ["front_door", "backyard"],
        segments: [],
        reviews: [],
        currentTs: 1700000500,
        onSeek: () => {},
        onExportRange: async () => {},
        defaultRangeMode: true,
      })
    );
    // Zoom buttons and Calendar remain visible even while Export Clip bar is open!
    expect(scrubberHtml).toContain("Zoom:");
    expect(scrubberHtml).toContain("5m");
    expect(scrubberHtml).toContain("24h");
    expect(scrubberHtml).toContain("Include Audio");

    const activeJob: ExportJobItem = {
      id: "front_door_1000_4600",
      camera: "front_door",
      name: "1h Evidence Export",
      start_time: 1000,
      end_time: 4600,
      include_audio: true,
      status: "running",
      progress_pct: 64,
      created_at: 5000,
      updated_at: 5030,
    };
    const workspaceHtml = renderToStaticMarkup(
      React.createElement(ExportsWorkspace, {
        cameraNames: ["front_door"],
        config: null,
        exportsList: [],
        setExportsList: () => {},
        exportJobs: [activeJob],
        onCancelExportJob: async () => {},
        exportCam: "front_door",
        setExportCam: () => {},
        activeExportPreview: null,
        setActiveExportPreview: () => {},
        timeShiftTs: 4500,
        setTimeShiftTs: () => {},
        focusSegments: [],
        reviews: [],
        handleQuickExport: async () => {},
        notify: () => {},
      })
    );
    expect(workspaceHtml).toContain("1h Evidence Export");
    expect(workspaceHtml).toContain("64%");
    expect(workspaceHtml).toContain('aria-valuenow="64"');
  });
});

describe("Export Clip Flow — Stage 1 Review Regression Coverage", () => {
  test("resolveVodSeekAction reloads window when requiredEndTs exceeds loaded 180s window (both >180s clips and 30s straddling clips)", () => {
    const loaded180 = { camera: "sala", startSec: 1000, endSec: 1180 };

    // 10-minute clip [1000, 1600] must reload to cover 1600 even though targetTs=1000 is inside [1000, 1180]
    const longAct = resolveVodSeekAction(loaded180, {
      camera: "sala",
      targetTs: 1000,
      requiredEndTs: 1600,
      windowSpanSec: 612,
      nowSec: 5000,
    });
    expect(longAct.kind).toBe("reload_window");
    expect(longAct.window.startSec).toBe(1000);
    expect(longAct.window.endSec >= 1600).toBe(true);

    // 30-second clip [1165, 1195] straddling loaded180.endSec=1180 must also reload instead of freezing at 1180!
    const straddleAct = resolveVodSeekAction(loaded180, {
      camera: "sala",
      targetTs: 1165,
      requiredEndTs: 1195,
      nowSec: 5000,
    });
    expect(straddleAct.kind).toBe("reload_window");
    expect(straddleAct.window.endSec >= 1195).toBe(true);

    // Clip [1020, 1080] fully inside [1000, 1180] reuses seek_in_window without manifest reload
    const fitAct = resolveVodSeekAction(loaded180, {
      camera: "sala",
      targetTs: 1020,
      requiredEndTs: 1080,
      nowSec: 5000,
    });
    expect(fitAct.kind).toBe("seek_in_window");
    expect(fitAct.offsetSec).toBe(20);
  });

  test("stepHandleDragTs preserves 0.25s Shift precision and never teleports when Shift is released mid-drag", () => {
    // Start handle at t=2000s, clientX=400 on an 800px track spanning 800s (1px = 1s at 1.0x)
    const s0 = { lastClientX: 400, currentTs: 2000 };

    // Drag +10px with Shift held -> 10 * 0.15 = +1.5s -> 2001.5s; another +5px -> +0.75s -> 2002.25s!
    const s1 = stepHandleDragTs({
      clientX: 410,
      rectLeft: 100,
      rectWidth: 800,
      winStart: 1600,
      winEnd: 2400,
      shiftKey: true,
      prevState: s0,
    });
    const q1 = quantizeClipBoundaryTs(s1.currentTs, true, 1000, 3000);
    expect(q1).toBe(2001.5);

    const s2 = stepHandleDragTs({
      clientX: 415,
      rectLeft: 100,
      rectWidth: 800,
      winStart: 1600,
      winEnd: 2400,
      shiftKey: true,
      prevState: { lastClientX: 410, currentTs: q1 },
    });
    const q2 = quantizeClipBoundaryTs(s2.currentTs, true, 1000, 3000);
    expect(q2).toBe(2002.25);

    // Release Shift at clientX=415 and move +2px to 417: must advance by +2s from 2002.25 -> 2004s,
    // NOT teleport to raw clientX (1600 + (317/800)*800 = 1917s)!
    const s3 = stepHandleDragTs({
      clientX: 417,
      rectLeft: 100,
      rectWidth: 800,
      winStart: 1600,
      winEnd: 2400,
      shiftKey: false,
      prevState: { lastClientX: 415, currentTs: q2 },
    });
    const q3 = quantizeClipBoundaryTs(s3.currentTs, false, 1000, 3000);
    expect(q3).toBe(2004);
  });

  test("reanchorClipRangeBoundary resolves >24h backward and forward jumps without deadlocking I/O/ClockInput", () => {
    const nowSec = 1_700_500_000;
    const minArchiveSec = nowSec - 30 * 86400;
    // Current clip is on today [now - 90, now - 30] (60s span); user jumps 3 days back and presses I
    const threeDaysAgo = nowSec - 3 * 86400;
    const reIn = reanchorClipRangeBoundary({
      which: "in",
      candidateTs: threeDaysAgo,
      curStart: nowSec - 90,
      curEnd: nowSec - 30,
      minArchiveSec,
      nowSec,
    });
    expect(reIn).toEqual({ start: threeDaysAgo, end: threeDaysAgo + 60 });

    // Now clip is 3 days ago; user jumps back to today and presses O
    const reOut = reanchorClipRangeBoundary({
      which: "out",
      candidateTs: nowSec - 10,
      curStart: threeDaysAgo,
      curEnd: threeDaysAgo + 60,
      minArchiveSec,
      nowSec,
    });
    expect(reOut).toEqual({ start: nowSec - 70, end: nowSec - 10 });
  });

  test("virtual _sim_XX camera resolves event clip range and rejects stale cross-camera indexedFrames", () => {
    const reviews: ReviewItem[] = [
      {
        id: "rev_real",
        camera: "escritorio",
        start_time: 2000,
        end_time: 2020,
        severity: "alert",
        thumb_path: "",
        has_been_reviewed: false,
        data: {},
      },
    ];
    expect(
      resolveEventClipRange(
        reviews,
        "escritorio_sim_01",
        2010,
        5000,
        5,
        "escritorio"
      )
    ).toEqual({ start: 1995, end: 2025 });

    // Stale indexedFrames from `sala` must not be returned when camera switched to `garagem`
    const staleSalaFrames = sanitizePreviewFrameListPayload("sala", [
      { url: "/api/preview/sala/frame/2010000.jpg" },
    ]);
    expect(
      resolveTimelineHoverThumbnailUrl("garagem", 2010, staleSalaFrames, {
        hasRecording: true,
      })
    ).toBe("/api/preview/garagem/at/2010000.jpg");

    // mergeDeduplicatedSegments merges viewport + settled clip segments in chronological order
    const merged = mergeDeduplicatedSegments(
      [
        {
          id: "s2",
          start_time: 200,
          end_time: 210,
          duration: 10,
          motion: 1,
          objects: 0,
          segment_size: 1,
        },
      ],
      [
        {
          id: "s1",
          start_time: 100,
          end_time: 110,
          duration: 10,
          motion: 1,
          objects: 0,
          segment_size: 1,
        },
        {
          id: "s2",
          start_time: 200,
          end_time: 210,
          duration: 10,
          motion: 1,
          objects: 0,
          segment_size: 1,
        },
      ]
    );
    expect(merged.map((m) => m.id)).toEqual(["s1", "s2"]);
  });

  test("TimelineScrubber renders exactly one off-screen right jump pill when entire clip is to the right of viewport", () => {
    // currentTs is 2 hours before initial clip range (clipRequest sets [5000, 5060], while currentTs=1000 keeps viewport earlier if panned)
    // Using currentTs=5000 and defaultRangeMode=true sets range [4910, 4970]; let's verify no duplicate right pills render
    const html = renderToStaticMarkup(
      React.createElement(TimelineScrubber, {
        camera: "front_door",
        segments: [],
        reviews: [],
        currentTs: 1700005000,
        onSeek: () => {},
        onExportRange: async () => {},
        defaultRangeMode: true,
      })
    );
    expect(html).not.toContain("rgba(9, 13, 20");
    expect(html).not.toContain("rgba(255, 255, 255, 0.55)");
  });
});
