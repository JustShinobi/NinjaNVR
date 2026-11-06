import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import {
  computeAspectFitTiles,
  formatRelativeAgo,
  resolveAspectMatrixGridStyle,
  resolveMatrixDensityOptions,
  resolveMatrixGridShape,
  selectRecentEvents,
} from "../utils/liveStageLayout";
import {
  ReviewFlipbook,
  isSafeReviewFlipbookId,
  nextFlipbookIndex,
  sanitizeReviewFrameUrls,
} from "../components/ReviewFlipbook";
import {
  parseClockOnDay,
  resolveClipZoom,
  resolveEventClipRange,
  resolveReviewLane,
  sanitizeTimelineThumbUrl,
} from "../components/TimelineScrubber";
import { sanitizeExportVideoUrl } from "../App";
import { computeContainedBox } from "../components/VideoPlayers";
import {
  nextMatrixTourPage,
  resolveSafeAlertThumbSrc,
  sanitizeFloorplanUrl,
  sliceMatrixPage,
} from "../views/LiveWorkspace";
import { resolveSafeSyncThumbSrc } from "../views/SyncWorkspace";
import type { ReviewItem } from "../types/nvr";

describe("resolveMatrixDensityOptions", () => {
  test("disables densities that would render the same grid", () => {
    const { options, effective } = resolveMatrixDensityOptions(4, "3x3");
    expect(options.map((o) => [o.density, o.enabled])).toEqual([
      ["2x2", true],
      ["3x3", false],
      ["4x4", false],
    ]);
    expect(effective).toBe("2x2");
    expect(options.find((o) => o.active)?.density).toBe("2x2");
    expect(options[1].needsMoreThan).toBe(4);
    expect(options[2].needsMoreThan).toBe(9);
  });

  test("keeps the selection when it makes a difference", () => {
    expect(resolveMatrixDensityOptions(6, "3x3").effective).toBe("3x3");
    expect(resolveMatrixDensityOptions(12, "4x4").effective).toBe("4x4");
  });

  test("falls back to the densest meaningful step below the selection", () => {
    expect(resolveMatrixDensityOptions(6, "4x4").effective).toBe("3x3");
    expect(resolveMatrixDensityOptions(0, "4x4").effective).toBe("2x2");
    expect(resolveMatrixDensityOptions(Number.NaN, "3x3").effective).toBe("2x2");
  });
});

describe("formatRelativeAgo", () => {
  test("uses seconds, minutes, hours and days in both locales", () => {
    expect(formatRelativeAgo(1000, 945)).toBe("55s ago");
    expect(formatRelativeAgo(1000, 760)).toBe("4m ago");
    expect(formatRelativeAgo(10_000, 2_800)).toBe("2h ago");
    expect(formatRelativeAgo(400_000, 200_000)).toBe("2d ago");
    expect(formatRelativeAgo(1000, 760, true)).toBe("há 4 min");
    expect(formatRelativeAgo(1000, 945, true)).toBe("há 55s");
  });

  test("clamps future timestamps and rejects non-finite input", () => {
    expect(formatRelativeAgo(1000, 1200)).toBe("0s ago");
    expect(formatRelativeAgo(Number.NaN, 1)).toBe("");
  });
});

describe("selectRecentEvents", () => {
  test("returns the newest first, capped, skipping invalid times", () => {
    const items = [
      { id: "a", start_time: 10 },
      { id: "b", start_time: 30 },
      { id: "c", start_time: Number.NaN },
      { id: "d", start_time: 20 },
    ];
    expect(selectRecentEvents(items, 2).map((i) => i.id)).toEqual(["b", "d"]);
    expect(selectRecentEvents(items, 0)).toEqual([]);
    expect(items[0].id).toBe("a"); // input is not mutated
  });
});

describe("16:9 matrix grid", () => {
  test("shape mirrors the adaptive grid", () => {
    expect(resolveMatrixGridShape(1)).toEqual({ cols: 1, rows: 1 });
    expect(resolveMatrixGridShape(2)).toEqual({ cols: 2, rows: 1 });
    expect(resolveMatrixGridShape(4)).toEqual({ cols: 2, rows: 2 });
    expect(resolveMatrixGridShape(5)).toEqual({ cols: 3, rows: 2 });
    expect(resolveMatrixGridShape(9)).toEqual({ cols: 3, rows: 3 });
    expect(resolveMatrixGridShape(11)).toEqual({ cols: 4, rows: 3 });
    expect(resolveMatrixGridShape(16)).toEqual({ cols: 4, rows: 4 });
  });

  test("fits the largest 16:9 tile, limited by height on a wide stage", () => {
    // Production stage from the audit: 2000×810 with four cameras.
    const fit = computeAspectFitTiles(2000, 810, 2, 2, 2);
    expect(fit).toEqual({ tileWidth: 718, tileHeight: 403 });
    // A tall stage is limited by width instead.
    expect(computeAspectFitTiles(800, 2000, 2, 2, 2)).toEqual({ tileWidth: 399, tileHeight: 224 });
  });

  test("rejects unusable stages", () => {
    expect(computeAspectFitTiles(0, 500, 2, 2)).toBeNull();
    expect(computeAspectFitTiles(500, 500, 0, 2)).toBeNull();
    expect(computeAspectFitTiles(20, 10, 4, 4)).toBeNull();
    expect(computeAspectFitTiles(Number.NaN, 500, 2, 2)).toBeNull();
  });

  test("grid style centres pixel tracks once the stage is measured", () => {
    expect(resolveAspectMatrixGridStyle(null, 4)).toBeUndefined();
    const style = resolveAspectMatrixGridStyle({ width: 2000, height: 810 }, 4);
    expect(style?.gridTemplateColumns).toBe("repeat(2, 718px)");
    expect(style?.gridTemplateRows).toBe("repeat(2, 403px)");
    expect(style?.justifyContent).toBe("center");
    expect(style?.alignContent).toBe("center");
  });
});

describe("ReviewFlipbook", () => {
  test("keeps only same-origin frame URLs for the same review", () => {
    const urls = sanitizeReviewFrameUrls("rev_1", {
      frames: [
        { url: "/api/review/rev_1/frames/1793984364123.jpg" },
        { url: "/api/review/rev_1/frames/1793984365123.jpg" },
        { url: "/api/review/rev_1/frames/12345678901234567.jpg" },
        { url: "/api/review/other/frames/2.jpg" },
        { url: "https://evil.example/frames/3.jpg" },
        { url: "/api/review/rev_1/frames/../../x.jpg" },
        { url: "/api/review/rev_1/frames/4.png" },
        { url: 5 },
      ],
    });
    // Frames are named by capture time in ms; absurdly long names are rejected.
    expect(urls).toEqual([
      "/api/review/rev_1/frames/1793984364123.jpg",
      "/api/review/rev_1/frames/1793984365123.jpg",
    ]);
    expect(sanitizeReviewFrameUrls("../x", { frames: [] })).toEqual([]);
    expect(sanitizeReviewFrameUrls("rev_1", null)).toEqual([]);
  });

  test("loops frame indices and holds still for a single frame", () => {
    expect(nextFlipbookIndex(0, 8)).toBe(1);
    expect(nextFlipbookIndex(7, 8)).toBe(0);
    expect(nextFlipbookIndex(3, 1)).toBe(0);
    expect(nextFlipbookIndex(Number.NaN, 4)).toBe(1);
  });

  test("renders the static thumbnail until frames arrive (no <video>)", () => {
    const html = renderToStaticMarkup(
      React.createElement(ReviewFlipbook, {
        reviewId: "rev_1",
        fallbackSrc: "/api/review/rev_1/thumb.webp",
        alt: "sala person event",
      })
    );
    expect(html).toContain('src="/api/review/rev_1/thumb.webp"');
    expect(html).not.toContain("<video");
  });
});

describe("Export clip helpers", () => {
  const ref = Math.floor(new Date(2026, 10, 6, 3, 18, 45).getTime() / 1000);

  test("parseClockOnDay reads HH:MM[:SS] on the reference day", () => {
    expect(parseClockOnDay("03:16:50", ref)).toBe(
      Math.floor(new Date(2026, 10, 6, 3, 16, 50).getTime() / 1000)
    );
    expect(parseClockOnDay(" 3:17 ", ref)).toBe(
      Math.floor(new Date(2026, 10, 6, 3, 17, 0).getTime() / 1000)
    );
    for (const bad of ["24:00", "03:60", "03:16:61", "abc", "", "03-16-50"]) {
      expect(parseClockOnDay(bad, ref)).toBeNull();
    }
    expect(parseClockOnDay("03:16", Number.NaN)).toBeNull();
  });

  test("resolveClipZoom keeps the clip easy to adjust", () => {
    expect(resolveClipZoom(30)).toBe(5);
    expect(resolveClipZoom(300)).toBe(15);
    expect(resolveClipZoom(1800)).toBe(60);
    expect(resolveClipZoom(7200)).toBe(360);
    expect(resolveClipZoom(Number.NaN)).toBe(5);
  });

  const review = (id: string, camera: string, start: number, end: number | null): ReviewItem =>
    ({
      id,
      camera,
      start_time: start,
      end_time: end,
      severity: "alert",
      thumb_path: "",
      has_been_reviewed: false,
      data: {},
    }) as ReviewItem;

  test("This event picks the review under the playhead, padded", () => {
    const reviews = [review("a", "sala", 1000, 1020), review("b", "quarto", 1005, 1030)];
    expect(resolveEventClipRange(reviews, "sala", 1010, 5000)).toEqual({ start: 995, end: 1025 });
  });

  test("falls back to the latest review in the previous 10 minutes", () => {
    const reviews = [review("a", "sala", 1000, 1020), review("b", "sala", 1200, 1210)];
    expect(resolveEventClipRange(reviews, "sala", 1500, 5000)).toEqual({ start: 1195, end: 1215 });
    expect(resolveEventClipRange(reviews, "sala", 2000, 5000)).toBeNull();
  });

  test("open-ended reviews use 15 s and never pass the live edge", () => {
    const reviews = [review("a", "sala", 1000, null)];
    expect(resolveEventClipRange(reviews, "sala", 1005, 1012)).toEqual({ start: 995, end: 1012 });
    expect(resolveEventClipRange([], "sala", 1005, 2000)).toBeNull();
    expect(resolveEventClipRange(reviews, "sala", Number.NaN, 2000)).toBeNull();
  });

  test("alerts and detections get separate timeline lanes", () => {
    const alert = resolveReviewLane("alert");
    const detection = resolveReviewLane("detection");
    expect(alert.y + alert.h).toBeLessThanOrEqual(detection.y);
  });

  test("hardens URL sanitizers, SVG floorplan decoder, and geometry/pagination helpers against NaN and traversal", () => {
    // 1. ReviewFlipbook ID validation
    expect(isSafeReviewFlipbookId("rev_1")).toBe(true);
    expect(isSafeReviewFlipbookId(".")).toBe(false);
    expect(isSafeReviewFlipbookId("..")).toBe(false);
    expect(sanitizeReviewFrameUrls(".", { frames: [{ url: "/api/review/./frames/1.jpg" }] })).toEqual([]);

    // 2. Double-encoded traversal, query/fragment injection, dot camera, and sensitive /api/* blocks
    expect(
      sanitizeExportVideoUrl("/exports/%252e%252e/config.yml", "..", 100, 120)
    ).toBe("/exports/camera_100_120.mp4");
    expect(
      sanitizeExportVideoUrl("/api/auth/logout", "cam1", 100, 120)
    ).toBe("/exports/cam1_100_120.mp4");
    expect(
      sanitizeExportVideoUrl("/exports/clip.mp4?token=secret", "cam1", 100, 120)
    ).toBe("/exports/cam1_100_120.mp4");

    expect(
      sanitizeTimelineThumbUrl("/clips/%252e%252e/secret.webp", "..")
    ).toBe("/api/camera/latest.webp");
    expect(
      sanitizeTimelineThumbUrl("/api/config/raw", "cam1")
    ).toBe("/api/cam1/latest.webp");
    expect(
      sanitizeTimelineThumbUrl("/clips/thumb.webp?x=1", "cam1")
    ).toBe("/api/cam1/latest.webp");

    expect(resolveSafeAlertThumbSrc("/api/logout", "..")).toBe(
      "/api/camera/latest.webp"
    );
    expect(resolveSafeAlertThumbSrc("/clips/a.webp?evil=1", "cam1")).toBe(
      "/api/cam1/latest.webp"
    );
    expect(resolveSafeSyncThumbSrc("/api/tokens", ".")).toBe(
      "/api/camera/latest.webp"
    );

    // 3. Floorplan SVG null-byte / external href / sensitive API route blocking
    const nullByteSvgB64 = globalThis.btoa(
      "<svg xmlns=\"http://www.w3.org/2000/svg\">\u0000<script>alert(1)</script></svg>"
    );
    expect(sanitizeFloorplanUrl(`data:image/svg+xml;base64,${nullByteSvgB64}`)).toBe("");
    const extHrefSvgB64 = globalThis.btoa(
      '<svg xmlns="http://www.w3.org/2000/svg"><image href="https://evil.example/track.png"/></svg>'
    );
    expect(sanitizeFloorplanUrl(`data:image/svg+xml;base64,${extHrefSvgB64}`)).toBe("");
    expect(sanitizeFloorplanUrl("/api/config/raw")).toBe("");

    // 4. NaN / Infinity guards in computeContainedBox, sliceMatrixPage, nextMatrixTourPage, resolveEventClipRange
    expect(computeContainedBox(Number.NaN, 360, 1280, 720)).toEqual({
      left: 0,
      top: 0,
      width: 0,
      height: 360,
    });
    expect(computeContainedBox(640, 360, Number.NaN, 720)).toEqual({
      left: 0,
      top: 0,
      width: 640,
      height: 360,
    });
    expect(sliceMatrixPage(["c1", "c2", "c3"], Number.NaN, "2x2").safePage).toBe(0);
    expect(sliceMatrixPage(["c1", "c2", "c3"], Number.NaN, "2x2").visibleMatrixCams).toEqual([
      "c1",
      "c2",
      "c3",
    ]);
    expect(nextMatrixTourPage(Number.NaN, 3)).toBe(1);
    expect(nextMatrixTourPage(0, Number.NaN)).toBe(0);
    expect(
      resolveEventClipRange([review("a", "sala", 1000, 1020)], "sala", 1010, 5000, Number.NaN)
    ).toEqual({ start: 995, end: 1025 });
  });
});
