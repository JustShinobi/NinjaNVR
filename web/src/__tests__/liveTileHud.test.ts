import { describe, expect, test } from "bun:test";
import {
  buildCameraSnapshotUrl,
  formatBadgeCount,
  formatCameraTelemetryRows,
  isCompactViewport,
  resolveCameraRuntimeFlags,
  resolveMobileNavTabs,
  resolveTileException,
  resolveTileIdentityTone,
} from "../utils/liveTileHud";

describe("resolveTileException (HUD by exception)", () => {
  test("normal state renders no chip at all", () => {
    expect(resolveTileException({})).toBeNull();
    expect(
      resolveTileException({ recordEnabled: true, motion: false, objectLabels: [] })
    ).toBeNull();
  });

  test("tamper outranks everything and counts hidden exceptions", () => {
    const ex = resolveTileException({
      tamper: true,
      objectLabels: ["person"],
      recordEnabled: false,
      motion: true,
    });
    expect(ex?.kind).toBe("tamper");
    expect(ex?.tone).toBe("alert");
    expect(ex?.hiddenCount).toBe(2);
  });

  test("objects dedupe, uppercase and cap the label list", () => {
    const ex = resolveTileException({
      objectLabels: ["person", "Person", "car", "dog"],
      motion: true,
    });
    expect(ex).toEqual({ kind: "object", label: "PERSON, CAR +1", tone: "alert", hiddenCount: 0 });
  });

  test("anomalous ROI beats record-off, record-off beats motion", () => {
    expect(
      resolveTileException({ roiAnomalies: ["gate open"], recordEnabled: false })?.kind
    ).toBe("roi");
    const rec = resolveTileException({ recordEnabled: false, motion: true });
    expect(rec?.kind).toBe("record-off");
    expect(rec?.label).toBe("REC OFF");
    expect(rec?.tone).toBe("motion");
    expect(rec?.hiddenCount).toBe(1);
  });

  test("motion alone surfaces as an amber chip", () => {
    expect(resolveTileException({ motion: true })).toEqual({
      kind: "motion",
      label: "MOTION",
      tone: "motion",
      hiddenCount: 0,
    });
  });

  test("record undefined is not treated as off", () => {
    expect(resolveTileException({ recordEnabled: undefined })).toBeNull();
  });
});

describe("resolveTileIdentityTone", () => {
  test("time-shift wins over fps state", () => {
    expect(resolveTileIdentityTone({ timeShifted: true, cameraFps: 0, hasStats: true })).toBe(
      "rewind"
    );
  });
  test("zero fps with stats is offline, missing stats is optimistic", () => {
    expect(resolveTileIdentityTone({ timeShifted: false, cameraFps: 0, hasStats: true })).toBe(
      "offline"
    );
    expect(resolveTileIdentityTone({ timeShifted: false, hasStats: false })).toBe("live");
    expect(resolveTileIdentityTone({ timeShifted: false, cameraFps: 5, hasStats: true })).toBe(
      "live"
    );
  });
});

describe("snapshot + telemetry helpers", () => {
  test("snapshot url targets the unified endpoint and encodes the id", () => {
    expect(buildCameraSnapshotUrl("escritorio")).toBe("/api/escritorio/snapshot.jpg");
    expect(buildCameraSnapshotUrl("a b/c")).toBe("/api/a%20b%2Fc/snapshot.jpg");
  });

  test("telemetry rows format resolution, fps and image mode", () => {
    const rows = formatCameraTelemetryRows({
      detectWidth: 1280,
      detectHeight: 720,
      cameraFps: 5,
      detectionFps: 7.63,
      nightMode: true,
    });
    expect(rows.map((r) => r.value)).toEqual(["1280×720", "5.0 FPS", "7.6 FPS", "IR night"]);
    const pt = formatCameraTelemetryRows({ isPt: true });
    expect(pt.map((r) => r.value)).toEqual(["—", "—", "—", "Dia"]);
    expect(pt[0].label).toBe("Resolução");
  });

  test("runtime flags default to on and respect explicit config", () => {
    expect(resolveCameraRuntimeFlags()).toEqual({
      enabled: true,
      detect: true,
      record: true,
      alerts: true,
    });
    expect(resolveCameraRuntimeFlags({ record: false }, false)).toEqual({
      enabled: false,
      detect: true,
      record: false,
      alerts: true,
    });
  });
});

describe("mobile shell", () => {
  test("compact viewport boundary is 768px exclusive", () => {
    expect(isCompactViewport(390)).toBe(true);
    expect(isCompactViewport(767)).toBe(true);
    expect(isCompactViewport(768)).toBe(false);
    expect(isCompactViewport(0)).toBe(false);
    expect(isCompactViewport(Number.NaN)).toBe(false);
  });

  test("touch phones in landscape stay compact, short desktop windows do not", () => {
    // iPhone 17 Pro Max landscape: 956x440 CSS px, coarse pointer
    expect(isCompactViewport(956, 440, true)).toBe(true);
    expect(isCompactViewport(956, 440, false)).toBe(false);
    // Tablet landscape is tall enough for the desktop console
    expect(isCompactViewport(1180, 820, true)).toBe(false);
  });

  test("admin-only tabs never leak to viewers", () => {
    expect(resolveMobileNavTabs("viewer")).toEqual({
      primary: ["live", "sync", "review", "exports"],
      overflow: ["explore"],
    });
    expect(resolveMobileNavTabs("admin").overflow).toEqual(["explore", "zones", "system"]);
  });

  test("badge counts cap at 99+", () => {
    expect(formatBadgeCount(0)).toBe("");
    expect(formatBadgeCount(7)).toBe("7");
    expect(formatBadgeCount(200)).toBe("99+");
  });
});

import {
  filterCompactStageDescriptors,
  resolveCompactStageContainerStyle,
  sanitizeCompactGridColumns,
} from "../utils/liveTileHud";

describe("compact live stage", () => {
  test("matrix becomes a scrolling 1- or 2-column list", () => {
    expect(resolveCompactStageContainerStyle("2x2", 1).gridTemplateColumns).toBe("minmax(0, 1fr)");
    expect(resolveCompactStageContainerStyle("2x2", 2).gridTemplateColumns).toBe(
      "repeat(2, minmax(0, 1fr))"
    );
    expect(resolveCompactStageContainerStyle("2x2", 1).overflowY).toBe("auto");
    // Regression: "auto" rows squeezed 16:9 tiles to the viewport and overlapped them.
    expect(resolveCompactStageContainerStyle("2x2", 1).gridAutoRows).toBe("max-content");
    expect(resolveCompactStageContainerStyle("focus", 1).gridAutoRows).toBe("max-content");
    expect(resolveCompactStageContainerStyle("focus", 1).flex).toBe("0 0 auto");
    expect(resolveCompactStageContainerStyle("focus", 2).gridTemplateColumns).toBe(
      "minmax(0, 1fr)"
    );
  });

  test("focus and spotlight drop dock tiles so phones decode one stream", () => {
    const d = [{ role: "hero" }, { role: "dock" }, { role: "dock" }];
    expect(filterCompactStageDescriptors("focus", d)).toEqual([{ role: "hero" }]);
    expect(filterCompactStageDescriptors("spotlight", d)).toEqual([{ role: "hero" }]);
    const g = [{ role: "grid" }, { role: "grid" }];
    expect(filterCompactStageDescriptors("2x2", g)).toEqual(g);
  });

  test("column preference sanitizes persisted values", () => {
    expect(sanitizeCompactGridColumns("2")).toBe(2);
    expect(sanitizeCompactGridColumns(2)).toBe(2);
    expect(sanitizeCompactGridColumns("3")).toBe(1);
    expect(sanitizeCompactGridColumns(null)).toBe(1);
  });
});

import { countActiveReviewFilters } from "../utils/liveTileHud";

describe("countActiveReviewFilters", () => {
  test("defaults count as zero", () => {
    expect(
      countActiveReviewFilters({
        camera: "all",
        severity: "all",
        object: "all",
        status: "unreviewed",
        date: "",
        excludeObject: "none",
        zone: "all",
        plate: "  ",
      })
    ).toBe(0);
  });
  test("each non-default filter counts once", () => {
    expect(
      countActiveReviewFilters({
        camera: "sala",
        severity: "alert",
        status: "all",
        date: "2026-11-08",
        plate: "ABC1D23",
      })
    ).toBe(5);
  });
});

import { computePinchScale } from "../components/VideoPlayers";

describe("computePinchScale", () => {
  test("spreading fingers scales proportionally from the start scale", () => {
    expect(computePinchScale(100, 200, 1)).toBe(2);
    expect(computePinchScale(100, 150, 2)).toBe(3);
  });
  test("clamps to the 1x–5x range", () => {
    expect(computePinchScale(100, 50, 1)).toBe(1);
    expect(computePinchScale(100, 1000, 2)).toBe(5);
  });
  test("degenerate distances keep the current scale", () => {
    expect(computePinchScale(0, 100, 2)).toBe(2);
    expect(computePinchScale(100, Number.NaN, 3)).toBe(3);
    expect(computePinchScale(100, 120, Number.NaN)).toBe(1.2);
  });
});

import { countActiveExploreFilters } from "../utils/liveTileHud";

describe("countActiveExploreFilters", () => {
  test("defaults count as zero", () => {
    expect(
      countActiveExploreFilters({
        camera: "all",
        label: "all",
        excludeLabel: "none",
        zone: "all",
        minScore: 0,
        stationary: "all",
        retainedOnly: false,
        plate: "",
        date: "",
      })
    ).toBe(0);
  });
  test("counts every non-default filter once", () => {
    expect(
      countActiveExploreFilters({
        camera: "sala",
        minScore: 60,
        stationary: "moving",
        retainedOnly: true,
        plate: "ABC",
      })
    ).toBe(5);
  });
});

import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { CameraDetailsSheet } from "../components/CameraDetailsSheet";
import {
  buildLiveDisplayedMediaInfo,
  cameraHasIndependentSubstream,
  isSubstreamIdentifierForCamera,
  shouldHoverPrewarmMainStream,
} from "../components/VideoPlayers";
import {
  formatDisplayedQualityBadge,
  formatResolutionShorthand,
  resolveEffectiveDisplayedMedia,
} from "../utils/liveTileHud";
import { sanitizeLiveStreamQualityMode } from "../utils/format";

describe("real displayed stream quality telemetry and 3-way selector", () => {
  test("formatResolutionShorthand handles standard, portrait-rotated, and custom resolutions", () => {
    expect(formatResolutionShorthand(1920, 1080)).toBe("1080p");
    expect(formatResolutionShorthand(1080, 1920)).toBe("1080p");
    expect(formatResolutionShorthand(3840, 2160)).toBe("4K");
    expect(formatResolutionShorthand(2560, 1440)).toBe("1440p");
    expect(formatResolutionShorthand(1280, 720)).toBe("720p");
    expect(formatResolutionShorthand(640, 480)).toBe("480p");
    expect(formatResolutionShorthand(640, 360)).toBe("360p");
    expect(formatResolutionShorthand(2304, 1296)).toBe("2304×1296");
    expect(formatResolutionShorthand(0, 0)).toBe("");
  });

  test("buildLiveDisplayedMediaInfo and formatDisplayedQualityBadge report real decoded stream, pending keyframe transitions, and aliased substreams", () => {
    // 1. Live substream playing at native 640x360
    const subInfo = buildLiveDisplayedMediaInfo({
      realCamera: "escritorio",
      useMainStream: false,
      desiredStreamSrc: "escritorio_sub",
      mseReady: true,
      activeStreamSrc: "escritorio_sub",
      standbyConnectingStreamSrc: null,
      videoWidth: 640,
      videoHeight: 360,
      posterWidth: 640,
      posterHeight: 360,
    });
    expect(subInfo.variant).toBe("sub");
    expect(subInfo.substreamAliasedToMain).toBe(false);
    expect(formatDisplayedQualityBadge(subInfo, { detailed: false, isPt: true })?.label).toBe(
      "SUB 360p"
    );
    expect(formatDisplayedQualityBadge(subInfo, { detailed: true, isPt: true })?.label).toBe(
      "SUB · 640×360"
    );

    // 2. Double-buffered upgrade in flight (waiting for main-stream IDR keyframe)
    const upgradingInfo = buildLiveDisplayedMediaInfo({
      realCamera: "escritorio",
      useMainStream: true,
      desiredStreamSrc: "escritorio",
      mseReady: true,
      activeStreamSrc: "escritorio_sub",
      standbyConnectingStreamSrc: "escritorio",
      videoWidth: 640,
      videoHeight: 360,
      posterWidth: 640,
      posterHeight: 360,
    });
    const upgBadge = formatDisplayedQualityBadge(upgradingInfo, {
      detailed: false,
      isPt: true,
    });
    expect(upgBadge?.label).toBe("SUB 360p → MAIN…");
    expect(upgBadge?.isPendingSwitch).toBe(true);

    // 3. Promoted to main stream at 1920x1080
    const mainInfo = buildLiveDisplayedMediaInfo({
      realCamera: "escritorio",
      useMainStream: true,
      desiredStreamSrc: "escritorio",
      mseReady: true,
      activeStreamSrc: "escritorio",
      standbyConnectingStreamSrc: null,
      videoWidth: 1920,
      videoHeight: 1080,
      posterWidth: 1920,
      posterHeight: 1080,
    });
    expect(formatDisplayedQualityBadge(mainInfo, { detailed: false })?.label).toBe(
      "MAIN 1080p"
    );
    expect(formatDisplayedQualityBadge(mainInfo, { detailed: true })?.label).toBe(
      "MAIN · 1920×1080"
    );

    // 4. Camera without independent substream (e.g. "sala" aliased to main)
    const aliasedInfo = buildLiveDisplayedMediaInfo({
      realCamera: "sala",
      useMainStream: false,
      desiredStreamSrc: "sala",
      mseReady: true,
      activeStreamSrc: "sala",
      standbyConnectingStreamSrc: null,
      videoWidth: 1920,
      videoHeight: 1080,
      posterWidth: 1920,
      posterHeight: 1080,
    });
    expect(aliasedInfo.variant).toBe("main");
    expect(aliasedInfo.substreamAliasedToMain).toBe(true);
    const aliasedBadge = formatDisplayedQualityBadge(aliasedInfo, {
      detailed: false,
      isPt: true,
    });
    expect(aliasedBadge?.label).toBe("MAIN 1080p");
    expect(aliasedBadge?.title).toContain("não possui substream separado");

    // 5. Underscored camera key ("lobby_sub") is never misclassified as its own substream
    expect(isSubstreamIdentifierForCamera("lobby_sub", "lobby_sub")).toBe(false);
    expect(isSubstreamIdentifierForCamera("lobby_sub_sub", "lobby_sub")).toBe(true);

    // 6. Substream capability detection
    expect(
      cameraHasIndependentSubstream(
        "sala",
        undefined,
        { sala: ["rtsp://127.0.0.1:8554/sala"], sala_sub: ["rtsp://127.0.0.1:8554/sala"] }
      )
    ).toBe(false);
    expect(
      cameraHasIndependentSubstream(
        "escritorio",
        undefined,
        {
          escritorio: ["rtsp://cam.example/stream1"],
          escritorio_sub: ["rtsp://cam.example/stream2"],
        }
      )
    ).toBe(true);
  });

  test("formatCameraTelemetryRows and CameraDetailsSheet surface real displayed video dimensions alongside AI detect dims", () => {
    const rows = formatCameraTelemetryRows({
      detectWidth: 1280,
      detectHeight: 720,
      displayedMedia: {
        width: 1920,
        height: 1080,
        delivery: "mse",
        variant: "main",
        streamSrc: "sala",
        substreamAliasedToMain: true,
      },
      cameraFps: 5,
      detectionFps: 4.5,
      nightMode: false,
      isPt: true,
    });
    expect(rows[0]).toEqual({
      key: "resolution",
      label: "Vídeo exibido",
      value: "1920×1080",
      subValue: "Principal (sem sub) · IA 1280×720",
    });

    const html = renderToStaticMarkup(
      React.createElement(CameraDetailsSheet, {
        camera: "sala",
        telemetry: rows,
        flags: { enabled: true, detect: true, record: true, alerts: true },
        onToggleFlag: () => {},
        useMainStream: false,
        onSetMainStream: () => {},
        streamQualityMode: "sub",
        onSetStreamQualityMode: () => {},
        hasIndependentSubstream: false,
        showVectors: false,
        onSetShowVectors: () => {},
        onClose: () => {},
        isPt: true,
      })
    );
    expect(html).toContain("1920×1080");
    expect(html).toContain("Principal (sem sub) · IA 1280×720");
    expect(html).toContain('data-testid="sheet-quality-auto"');
    expect(html).toContain('data-testid="sheet-quality-sub"');
    expect(html).toContain('data-testid="sheet-quality-main"');
    expect(html).toContain('aria-label="Qualidade do stream"');
    expect(html).toContain("não possui substream separado");

    expect(sanitizeLiveStreamQualityMode(null, "true")).toBe("main");
    expect(sanitizeLiveStreamQualityMode(null, "false")).toBe("auto");
    expect(sanitizeLiveStreamQualityMode("sub", "true")).toBe("sub");
  });

  test("shouldHoverPrewarmMainStream blocks main-stream prewarm when streamQualityMode is sub", () => {
    expect(
      shouldHoverPrewarmMainStream({
        useMainStream: false,
        hasOnClick: true,
        streamQualityMode: "auto",
      })
    ).toBe(true);
    expect(
      shouldHoverPrewarmMainStream({
        useMainStream: false,
        hasOnClick: true,
        streamQualityMode: "sub",
      })
    ).toBe(false);
    expect(
      shouldHoverPrewarmMainStream({
        useMainStream: true,
        hasOnClick: true,
        streamQualityMode: "main",
      })
    ).toBe(false);
    expect(
      shouldHoverPrewarmMainStream({
        useMainStream: false,
        hasOnClick: false,
        streamQualityMode: "auto",
      })
    ).toBe(false);
  });

  test("resolveEffectiveDisplayedMedia clears stale MSE telemetry on mse->vod and mse->snapshot transitions", () => {
    const staleMse = {
      width: 1920,
      height: 1080,
      delivery: "mse" as const,
      variant: "main" as const,
      streamSrc: "escritorio",
    };
    const vodPending = resolveEffectiveDisplayedMedia("vod", staleMse);
    expect(vodPending).toEqual({
      width: 0,
      height: 0,
      delivery: "vod",
      variant: "vod",
    });
    expect(formatDisplayedQualityBadge(vodPending)?.label).toBe("VOD…");

    const snapPending = resolveEffectiveDisplayedMedia("snapshot", staleMse);
    expect(snapPending).toEqual({
      width: 0,
      height: 0,
      delivery: "snapshot",
      variant: "snapshot",
    });
    expect(formatDisplayedQualityBadge(snapPending)?.label).toBe("SNAP…");

    const loadedVod = {
      width: 1920,
      height: 1080,
      delivery: "vod" as const,
      variant: "vod" as const,
    };
    expect(resolveEffectiveDisplayedMedia("vod", loadedVod)).toEqual(loadedVod);
    expect(resolveEffectiveDisplayedMedia("mse", loadedVod)).toBeNull();
    expect(resolveEffectiveDisplayedMedia("mse", staleMse)).toEqual(staleMse);
  });
});
