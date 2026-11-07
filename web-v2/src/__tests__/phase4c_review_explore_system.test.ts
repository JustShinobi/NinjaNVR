import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import {
  computeReviewPlaybackWindow,
  getVisibleReviewPageIds,
  parseSavedReviewPresets,
  resolveNextBriefingStep,
  resolveReviewThumbUrl,
  ReviewWorkspace,
  shouldApplyReviewMutationResult,
} from "../views/ReviewWorkspace";
import {
  ExploreWorkspace,
  formatAiVerificationBadge,
  formatDetectedAttributePill,
  formatSubLabelBadge,
  parseSavedExplorePresets,
  resolveFiniteEventScore,
  shouldApplyEventMutationResult,
} from "../views/ExploreWorkspace";
import {
  ExportsWorkspace,
  MAX_EXPORT_WINDOW_SEC,
  resolveSafeExportVideoUrl,
  validateExportTimeWindow,
} from "../views/ExportsWorkspace";
import {
  buildClientFallbackHeuristics,
  cameraUsesSharedMainLoopback,
  filterAndSortCameraHealth,
  normalizeRtspProducerEndpoint,
  SystemWorkspace,
  toFiniteNumber,
} from "../views/SystemWorkspace";
import { resolveDesiredLiveStreamSrc } from "../components/VideoPlayers";
import {
  buildOnvifPasswordPatch,
  sanitizeConfigEntrySlug,
} from "../views/settings/ConsoleAndStreamsPanels";
import {
  normalizeAllowedCamerasScope,
  resolveEffectiveRecordConfig,
  validateFederationPeerUrl,
} from "../views/settings/RecordingAndSecurityPanels";
import {
  AI_SETTINGS_SUBSECTIONS,
  DEFAULT_AI_SUBSECTION,
  AiDetectionObjectsSection,
  buildLocalFinetuneCliCommand,
  buildModelUploadPayload,
  computeDatasetReadinessMetrics,
  filterFalsePositiveSignaturesByScope,
  formatDhash64Hex,
  formatFalsePositiveReason,
  formatSignatureBbox,
  formatYoloAnnotationLine,
  isAiSubSectionVisible,
  isBundledCocoSsdModel,
  isSafeGenAiCustomBaseUrl,
  normalizeFaceProfileName,
  resolveSettingsScopeBadge,
  slugifyRoiLocationName,
  xyxyToYoloNormalized,
} from "../views/settings/VisionAndAiPanels";
import { useNvrStore } from "../store/useNvrStore";
import type { EventItem, ExportItem, ReviewItem } from "../types/nvr";

describe("Phase 4C — ReviewWorkspace security & regression suite", () => {
  test("resolveReviewThumbUrl encodes camera path segments and rejects unsafe or traversal thumb_path values", () => {
    // Strips /media/<mount> prefix on valid relative paths
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/media/ninjanvr/clips/review/thumb-front_door-100.webp",
      })
    ).toBe("/clips/review/thumb-front_door-100.webp");

    // Encodes special characters / slashes in fallback camera name
    expect(
      resolveReviewThumbUrl({
        camera: "node1/cam#1?x=1",
        thumb_path: "",
      })
    ).toBe("/api/node1%2Fcam%231%3Fx%3D1/latest.webp");

    // Rejects protocol-relative URLs, external URLs, javascript: URIs, path traversal, and control characters
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "//evil.example/pixel.webp",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "https://evil.example/pixel.webp",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "javascript:alert(1)",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/clips/../../api/logout",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/clips/review\\evil.webp",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/clips/\u0000bad.webp",
      })
    ).toBe("/api/front_door/latest.webp");
  });

  test("shouldApplyReviewMutationResult guards against stale activeReview switches and failed HTTP responses", () => {
    expect(shouldApplyReviewMutationResult("rev_1", "rev_1", true)).toBe(true);
    expect(shouldApplyReviewMutationResult("rev_2", "rev_1", true)).toBe(false);
    expect(shouldApplyReviewMutationResult(null, "rev_1", true)).toBe(false);
    expect(shouldApplyReviewMutationResult("rev_1", "rev_1", false)).toBe(false);
    expect(shouldApplyReviewMutationResult("", "rev_1", true)).toBe(false);
  });

  test("getVisibleReviewPageIds returns only visible filtered review IDs up to pageSize", () => {
    const items = Array.from({ length: 50 }, (_, idx) => ({
      id: `rev_${idx + 1}`,
    }));
    const pageIds = getVisibleReviewPageIds(items, 40);
    expect(pageIds.length).toBe(40);
    expect(pageIds[0]).toBe("rev_1");
    expect(pageIds[39]).toBe("rev_40");
    expect(getVisibleReviewPageIds([], 40)).toEqual([]);
  });

  test("computeReviewPlaybackWindow guarantees finite positive duration even on inverted or NaN timestamps", () => {
    expect(computeReviewPlaybackWindow(1000, 1020, 4, 6, 20)).toEqual({
      startSec: 996,
      endSec: 1026,
    });

    // Inverted end_time (end_time <= start_time) falls back to start_time + fallbackDurationSec
    expect(computeReviewPlaybackWindow(1000, 950, 5, 5, 20)).toEqual({
      startSec: 995,
      endSec: 1025,
    });

    // Null or NaN end_time falls back cleanly
    expect(computeReviewPlaybackWindow(1000, null, 4, 6, 20)).toEqual({
      startSec: 996,
      endSec: 1026,
    });
    expect(computeReviewPlaybackWindow(1000, Number.NaN, 4, 6, 20)).toEqual({
      startSec: 996,
      endSec: 1026,
    });

    // NaN start_time still produces valid positive window where endSec > startSec
    const nanWin = computeReviewPlaybackWindow(Number.NaN, Number.NaN, 4, 6, 20);
    expect(Number.isFinite(nanWin.startSec)).toBe(true);
    expect(Number.isFinite(nanWin.endSec)).toBe(true);
    expect(nanWin.endSec).toBeGreaterThan(nanWin.startSec);
  });

  test("parseSavedReviewPresets safely handles corrupted, non-array, and partial localStorage JSON", () => {
    expect(parseSavedReviewPresets(null)).toEqual([]);
    expect(parseSavedReviewPresets("")).toEqual([]);
    expect(parseSavedReviewPresets("{bad json")).toEqual([]);
    expect(parseSavedReviewPresets('{"not":"an array"}')).toEqual([]);
    expect(parseSavedReviewPresets("null")).toEqual([]);
    expect(parseSavedReviewPresets("42")).toEqual([]);

    const valid = parseSavedReviewPresets(
      JSON.stringify([
        null,
        { name: "   " },
        {
          name: " Driveway Alerts ",
          camera: "driveway",
          severity: "alert",
          object: "person",
          excludeObject: "cat",
          zone: "gate",
          status: "unreviewed",
        },
        {
          name: "Fallback Fields",
          status: "invalid_status",
        },
      ])
    );
    expect(valid).toEqual([
      {
        name: "Driveway Alerts",
        camera: "driveway",
        severity: "alert",
        object: "person",
        excludeObject: "cat",
        zone: "gate",
        status: "unreviewed",
      },
      {
        name: "Fallback Fields",
        camera: "all",
        severity: "all",
        object: "all",
        excludeObject: "none",
        zone: "all",
        status: "unreviewed",
      },
    ]);
  });

  test("ReviewWorkspace renders safely with corrupted provenance bbox/scores", () => {
    const rev: ReviewItem = {
      id: "rev_test_1",
      camera: "cam/test",
      start_time: 1700000000,
      end_time: 1699999900, // inverted end_time
      severity: "alert",
      thumb_path: "//evil.example/bad.webp",
      has_been_reviewed: false,
      data: {
        objects: ["person"],
        sub_labels: ["Bruno"],
        provenance: {
          top_score: Number.NaN,
          score: Number.NaN,
          min_score_cfg: Number.NaN,
          threshold_cfg: Number.NaN,
          area_px: Number.NaN,
          area_pct: Number.NaN,
          aspect_ratio: Number.NaN,
          bbox: [10, 0, 0, 0],
          norm_bbox: [0, 0, 0, 0],
          zones: ["entry"],
          frames_tracked: 1,
          stationary: false,
          trigger_ts: Number.NaN,
        },
      },
    };
    const html = renderToStaticMarkup(
      React.createElement(ReviewWorkspace, {
        cameraNames: ["cam/test"],
        reviews: [rev],
        filteredReviews: [rev],
        reviewCameraFilter: "all",
        setReviewCameraFilter: () => {},
        reviewSeverityFilter: "all",
        setReviewSeverityFilter: () => {},
        reviewObjectFilter: "all",
        setReviewObjectFilter: () => {},
        reviewStatusFilter: "unreviewed",
        setReviewStatusFilter: () => {},
        reviewDateFilter: "",
        setReviewDateFilter: () => {},
        globalDaysSummary: [],
        activeReview: rev,
        setActiveReview: () => {},
        focusSegments: [],
        handleMarkReviews: async () => {},
        handleDeleteReviews: async () => {},
        handleQuickExport: async () => {},
        setSyncMasterTs: () => {},
        switchTab: () => {},
      })
    );
    expect(html).toContain("/api/cam%2Ftest/latest.webp");
    expect(html).not.toContain("//evil.example/bad.webp");
    expect(html).not.toContain("NaN");
  });
});

describe("Phase 4C — ExploreWorkspace security & regression suite", () => {
  test("shouldApplyEventMutationResult guards against stale selectedEvent overwrites", () => {
    expect(shouldApplyEventMutationResult("ev_10", "ev_10", true)).toBe(true);
    expect(shouldApplyEventMutationResult("ev_20", "ev_10", true)).toBe(false);
    expect(shouldApplyEventMutationResult(null, "ev_10", true)).toBe(false);
    expect(shouldApplyEventMutationResult("ev_10", "ev_10", false)).toBe(false);
  });

  test("parseSavedExplorePresets validates array structure and sanitizes malformed entries", () => {
    expect(parseSavedExplorePresets(null)).toEqual([]);
    expect(parseSavedExplorePresets("false")).toEqual([]);
    expect(parseSavedExplorePresets('{"bad":true}')).toEqual([]);

    const parsed = parseSavedExplorePresets(
      JSON.stringify([
        {
          name: " High Confidence Cars ",
          camera: "street",
          label: "car",
          excludeLabel: "cat",
          zone: "road",
          minScore: 80,
          stationary: "moving",
          retainedOnly: true,
        },
        {
          name: "Defaults",
          minScore: -10,
          stationary: "invalid",
        },
      ])
    );
    expect(parsed).toEqual([
      {
        name: "High Confidence Cars",
        camera: "street",
        label: "car",
        excludeLabel: "cat",
        zone: "road",
        minScore: 80,
        stationary: "moving",
        retainedOnly: true,
      },
      {
        name: "Defaults",
        camera: "all",
        label: "all",
        excludeLabel: "none",
        zone: "all",
        minScore: 0,
        stationary: "all",
        retainedOnly: false,
      },
    ]);
  });

  test("formatAiVerificationBadge, formatDetectedAttributePill, and formatSubLabelBadge guard against NaN and Infinity", () => {
    expect(
      formatAiVerificationBadge({
        is_true_positive: true,
        confidence: Number.NaN,
        reason: "ok",
      })
    ).toBe("AI JUDGE: TRUE POSITIVE (0%)");

    expect(
      formatDetectedAttributePill({
        label: "face",
        score: Number.NaN,
        norm_bbox: [0, 0, 1, 1],
        area_px: Number.NaN,
      })
    ).toBe("FACE 0% · 0px²");

    expect(formatSubLabelBadge("Bruno", Number.NaN)).toEqual({
      text: "ID: BRUNO",
      isUnknown: false,
      isKnown: true,
    });
  });

  test("ExploreWorkspace renders encoded event URLs and handles empty visibleEvents state", () => {
    const ev: EventItem = {
      id: "ev/special#1",
      camera: "front_door",
      label: "person",
      sub_label: "Bruno",
      zones: ["porch"],
      start_time: 1700000000,
      end_time: 1700000010,
      top_score: 0.92,
      score: 0.92,
      has_clip: true,
      has_snapshot: true,
      retain_indefinitely: false,
      false_positive: false,
      box: null,
    };
    const html = renderToStaticMarkup(
      React.createElement(ExploreWorkspace, {
        cameraNames: ["front_door"],
        filteredEvents: [ev],
        exploreQuery: "",
        setExploreQuery: () => {},
        exploreCamera: "all",
        setExploreCamera: () => {},
        exploreLabel: "all",
        setExploreLabel: () => {},
        exploreRetainedOnly: false,
        setExploreRetainedOnly: () => {},
        globalDaysSummary: [],
        reviewDateFilter: "",
        setReviewDateFilter: () => {},
        selectedEvent: ev,
        setSelectedEvent: () => {},
        eventViewMode: "snapshot",
        setEventViewMode: () => {},
        handleEventRetain: async () => {},
        handleEventDelete: async () => {},
        setSyncMasterTs: () => {},
        switchTab: () => {},
        notify: () => {},
      })
    );
    expect(html).toContain("/api/events/ev%2Fspecial%231/thumbnail.webp");
    expect(html).toContain("/api/events/ev%2Fspecial%231/snapshot.jpg?bbox=1");
  });
});

describe("Phase 4C — ExportsWorkspace security & regression suite", () => {
  test("validateExportTimeWindow rejects non-finite, inverted (end <= start), zero-length, and > 24h windows", () => {
    // Valid window
    const valid = validateExportTimeWindow(1700000000, 1700000300);
    expect(valid).toEqual({
      valid: true,
      startSec: 1700000000,
      endSec: 1700000300,
      durationSec: 300,
    });

    // Exact 24h boundary is valid
    const exact24h = validateExportTimeWindow(
      1700000000,
      1700000000 + MAX_EXPORT_WINDOW_SEC
    );
    expect(exact24h.valid).toBe(true);
    expect(exact24h.durationSec).toBe(86400);

    // Exceeding 24h is rejected
    const over24h = validateExportTimeWindow(
      1700000000,
      1700000000 + MAX_EXPORT_WINDOW_SEC + 1
    );
    expect(over24h.valid).toBe(false);
    expect(over24h.error).toContain("exceeds maximum allowed duration (24h)");

    // Inverted window (end < start) and zero-length (end === start) are rejected
    expect(validateExportTimeWindow(1700000500, 1700000100).valid).toBe(false);
    expect(validateExportTimeWindow(1700000500, 1700000500).valid).toBe(false);

    // NaN, Infinity, and negative timestamps are rejected
    expect(validateExportTimeWindow(Number.NaN, 1700000500).valid).toBe(false);
    expect(validateExportTimeWindow(1700000000, Number.POSITIVE_INFINITY).valid).toBe(
      false
    );
    expect(validateExportTimeWindow(-10, 100).valid).toBe(false);
  });

  test("resolveSafeExportVideoUrl blocks javascript:, data:, cross-origin, protocol-relative, and path-traversal URLs", () => {
    expect(
      resolveSafeExportVideoUrl(
        "/media/ninjanvr/exports/front_door_100_200.mp4",
        "front_door_100_200"
      )
    ).toBe("/exports/front_door_100_200.mp4");

    expect(
      resolveSafeExportVideoUrl(
        "/exports/front_door_100_200.mp4",
        "front_door_100_200"
      )
    ).toBe("/exports/front_door_100_200.mp4");

    // Dangerous schemes fall back to encoded /exports/{id}.mp4
    expect(
      resolveSafeExportVideoUrl("javascript:alert(document.cookie)", "exp_1")
    ).toBe("/exports/exp_1.mp4");
    expect(
      resolveSafeExportVideoUrl("data:text/html,<script>alert(1)</script>", "exp_1")
    ).toBe("/exports/exp_1.mp4");
    expect(
      resolveSafeExportVideoUrl("https://evil.example/clip.mp4", "exp_1")
    ).toBe("/exports/exp_1.mp4");
    expect(resolveSafeExportVideoUrl("//evil.example/clip.mp4", "exp_1")).toBe(
      "/exports/exp_1.mp4"
    );
    expect(
      resolveSafeExportVideoUrl("/exports/../../api/logout", "exp_1")
    ).toBe("/exports/exp_1.mp4");

    // Unsafe exportId with path traversal returns empty string if videoPath is also unsafe
    expect(
      resolveSafeExportVideoUrl("javascript:alert(1)", "../evil")
    ).toBe("");
  });

  test("ExportsWorkspace sanitizes malicious video_path in download link and preview video element", () => {
    const maliciousExport: ExportItem = {
      id: "safe_export_id",
      camera: "front_door",
      name: "Test Clip",
      date: 1700000000,
      video_path: "javascript:alert(1)",
      thumb_path: "",
      in_progress: false,
    };
    const html = renderToStaticMarkup(
      React.createElement(ExportsWorkspace, {
        cameraNames: ["front_door"],
        config: null,
        exportsList: [maliciousExport],
        setExportsList: () => {},
        exportCam: "front_door",
        setExportCam: () => {},
        exportPresetMin: 5,
        setExportPresetMin: () => {},
        exportLabel: "",
        setExportLabel: () => {},
        activeExportPreview: maliciousExport,
        setActiveExportPreview: () => {},
        timeShiftTs: null,
        setTimeShiftTs: () => {},
        focusSegments: [],
        reviews: [],
        handleQuickExport: async () => {},
        notify: () => {},
      })
    );
    expect(html).not.toContain("javascript:alert(1)");
    expect(html).toContain('href="/exports/safe_export_id.mp4"');
    expect(html).toContain('src="/exports/safe_export_id.mp4"');
  });
});

describe("Phase 4C — SystemWorkspace security & regression suite", () => {
  test("normalizeRtspProducerEndpoint scrubs embedded credentials and accurately filters loopback hosts vs path substrings", () => {
    // Scrubs credentials so two streams with different or masked credentials match the same physical endpoint
    expect(
      normalizeRtspProducerEndpoint(
        "rtsp://admin:supersecret@192.0.2.10:554/h264Preview_01_main#backchannel=0"
      )
    ).toBe("rtsp://192.0.2.10:554/h264Preview_01_main");
    expect(
      normalizeRtspProducerEndpoint(
        "rtsp://*:*@192.0.2.10:554/h264Preview_01_main"
      )
    ).toBe("rtsp://192.0.2.10:554/h264Preview_01_main");

    // Loopback addresses (127.0.0.1, 127.0.0.2, localhost, [::1]) return null
    expect(
      normalizeRtspProducerEndpoint("rtsp://127.0.0.1:8554/front_door")
    ).toBeNull();
    expect(
      normalizeRtspProducerEndpoint("rtsp://127.0.0.2:8554/front_door")
    ).toBeNull();
    expect(
      normalizeRtspProducerEndpoint("rtsp://localhost:8554/front_door")
    ).toBeNull();
    expect(
      normalizeRtspProducerEndpoint("rtsp://[::1]:8554/front_door")
    ).toBeNull();

    // External camera whose stream path happens to contain "localhost" is NOT falsely treated as loopback
    expect(
      normalizeRtspProducerEndpoint(
        "rtsp://admin:pw@192.0.2.55:554/live/localhost_feed"
      )
    ).toBe("rtsp://192.0.2.55:554/live/localhost_feed");
  });

  test("filterAndSortCameraHealth handles NaN and non-numeric CPU/FPS values deterministically without comparator corruption", () => {
    const cameras = ["cam_b", "cam_a", "cam_c"];
    const statsMap = {
      cam_a: {
        camera_fps: Number.NaN,
        process_fps: 5,
        skipped_fps: 0,
        detection_fps: Number.NaN,
        detection_enabled: true,
        pid: 101,
        capture_pid: 102,
        ffmpeg_pid: 103,
        ffmpeg_cpu: "N/A",
        capture_cpu: "1.0",
      },
      cam_b: {
        camera_fps: 15,
        process_fps: 15,
        skipped_fps: 0,
        detection_fps: 4.5,
        detection_enabled: true,
        pid: 201,
        capture_pid: 202,
        ffmpeg_pid: 203,
        ffmpeg_cpu: "8.4",
        capture_cpu: "2.0",
      },
      cam_c: {
        camera_fps: 5,
        process_fps: 5,
        skipped_fps: 0,
        detection_fps: 0,
        detection_enabled: false,
        pid: 301,
        capture_pid: 302,
        ffmpeg_pid: 303,
        ffmpeg_cpu: "2.1",
        capture_cpu: "1.1",
      },
    };

    expect(
      filterAndSortCameraHealth(cameras, statsMap, {
        sortColumn: "ffmpeg_cpu",
        sortDirection: "desc",
      })
    ).toEqual(["cam_b", "cam_c", "cam_a"]);

    expect(
      filterAndSortCameraHealth(cameras, statsMap, {
        sortColumn: "camera_fps",
        sortDirection: "desc",
      })
    ).toEqual(["cam_b", "cam_c", "cam_a"]);

    expect(
      filterAndSortCameraHealth(cameras, statsMap, {
        statusFilter: "offline",
      })
    ).toEqual(["cam_a"]);

    expect(
      filterAndSortCameraHealth(cameras, statsMap, {
        statusFilter: "active_ai",
      })
    ).toEqual(["cam_b"]);
  });

  test("buildClientFallbackHeuristics detects duplicate RTSP streams across scrubbed/unscrubbed URLs and handles missing storage.free without NaN", () => {
    const alerts = buildClientFallbackHeuristics(
      {
        version: "0.18-0",
        safe_mode: false,
        go2rtc: {
          streams: {
            cam_main: "rtsp://admin:secret@192.0.2.10:554/live/main",
            cam_dup: "rtsp://*:*@192.0.2.10:554/live/main#audio=aac",
          },
        },
        cameras: {},
      },
      {
        cameras: {},
        detectors: {},
        camera_fps: 5,
        process_fps: 5,
        skipped_fps: 0,
        detection_fps: 0,
        cpu_usages: {},
        processes: {},
        service: {
          uptime: 100,
          version: "0.9.0",
          latest_version: "0.9.0",
          last_updated: 1700000000,
          storage: {
            "/media/ninjanvr/recordings": {
              total: 102400,
              used: 92160,
              free: Number.NaN,
              mount_type: "ext4",
            },
          },
        },
      }
    );

    const dupAlert = alerts.find((a) => a.id.startsWith("duplicate_rtsp_producer_"));
    expect(dupAlert).toBeDefined();
    expect(dupAlert?.target).toBe("cam_main, cam_dup");

    const storageAlert = alerts.find((a) => a.id === "storage_capacity_unbounded");
    expect(storageAlert).toBeDefined();
    expect(storageAlert?.detail).not.toContain("NaN");
    expect(storageAlert?.detail).toContain("10.0 GB free");
  });

  test("cameraUsesSharedMainLoopback and resolveDesiredLiveStreamSrc suppress false-positive detect_ffmpeg_cpu alerts and alias _sub to main stream", () => {
    const cfg = {
      version: "0.18-0",
      safe_mode: false,
      go2rtc: {
        streams: {
          cozinha: ["rtsp://*:*@192.0.2.10:554/stream1"],
          cozinha_sub: ["rtsp://*:*@192.0.2.10:554/stream2"],
          sala: ["rtsp://*:*@192.0.2.11:554/cam/realmonitor?channel=1&subtype=0"],
        },
      },
      cameras: {
        cozinha: {
          enabled: true,
          ffmpeg: {
            inputs: [
              { path: "rtsp://127.0.0.1:8554/cozinha_sub", roles: ["detect"] },
              { path: "rtsp://127.0.0.1:8554/cozinha", roles: ["record"] },
            ],
          },
          live: {
            streams: { "Stream 1": "cozinha_sub", "Stream 2": "cozinha" },
            height: 1080,
            quality: 10,
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          record: { enabled: true },
          snapshots: { enabled: true },
          motion: { enabled: true, threshold: 25, contour_area: 10, mask: {} },
          objects: { track: ["person"] },
          zones: {},
        },
        sala: {
          enabled: true,
          ffmpeg: {
            inputs: [
              { path: "rtsp://127.0.0.1:8554/sala", roles: ["detect", "record", "audio"] },
            ],
          },
          live: {
            streams: { "Stream 1": "sala", "Stream 2": "sala" },
            height: 1080,
            quality: 10,
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          record: { enabled: true },
          snapshots: { enabled: true },
          motion: { enabled: true, threshold: 25, contour_area: 10, mask: {} },
          objects: { track: ["person"] },
          zones: {},
        },
      },
    };

    expect(cameraUsesSharedMainLoopback(cfg, "cozinha")).toBe(false);
    expect(cameraUsesSharedMainLoopback(cfg, "sala")).toBe(true);
    expect(
      resolveDesiredLiveStreamSrc("cozinha", false, cfg.cameras, cfg.go2rtc.streams)
    ).toBe("cozinha_sub");
    expect(
      resolveDesiredLiveStreamSrc("sala", false, cfg.cameras, cfg.go2rtc.streams)
    ).toBe("sala");

    const alerts = buildClientFallbackHeuristics(cfg, {
      cameras: {
        cozinha: {
          camera_fps: 5,
          process_fps: 5,
          skipped_fps: 0,
          detection_fps: 0,
          detection_enabled: true,
          pid: 1,
          capture_pid: 1,
          ffmpeg_pid: 47,
          ffmpeg_cpu: "0.9",
        },
        sala: {
          camera_fps: 5,
          process_fps: 5,
          skipped_fps: 0,
          detection_fps: 0,
          detection_enabled: true,
          pid: 1,
          capture_pid: 1,
          ffmpeg_pid: 46,
          ffmpeg_cpu: "2.5",
        },
      },
      detectors: {},
      camera_fps: 10,
      process_fps: 10,
      skipped_fps: 0,
      detection_fps: 0,
      cpu_usages: {},
      processes: {},
      service: {
        uptime: 600,
        version: "3.0.6",
        latest_version: "3.0.6",
        last_updated: 1700000000,
        storage: {},
        heuristics: [
          {
            id: "detect_ffmpeg_cpu_sala",
            severity: "optimization",
            category: "vision",
            target: "sala",
            title: "Elevated FFmpeg decode/scale CPU on detect pipeline",
            detail: "Camera 'sala' detect decoder uses 2.5% CPU",
            recommendation: "Share a single go2rtc loopback producer",
          },
        ],
      },
    });
    expect(alerts.find((a) => a.id === "detect_ffmpeg_cpu_sala")).toBeUndefined();
  });

  test("toFiniteNumber normalizes numbers, strings, NaN, and Infinity", () => {
    expect(toFiniteNumber(12.5, 0)).toBe(12.5);
    expect(toFiniteNumber("14.2%", 0)).toBe(14.2);
    expect(toFiniteNumber("N/A", 6.5)).toBe(6.5);
    expect(toFiniteNumber(Number.NaN, 7.0)).toBe(7.0);
    expect(toFiniteNumber(Number.POSITIVE_INFINITY, 1.0)).toBe(1.0);
  });

  test("SystemWorkspace renders telemetry and storage tables without NaN on malformed metrics", () => {
    const prevStats = useNvrStore.getState().stats;
    useNvrStore.setState({
      stats: {
        cameras: {
          front_door: {
            camera_fps: Number.NaN,
            process_fps: Number.NaN,
            skipped_fps: Number.NaN,
            detection_fps: Number.NaN,
            detection_enabled: true,
            pid: 101,
            capture_pid: 102,
            ffmpeg_pid: 103,
            ffmpeg_cpu: "NaN",
            capture_cpu: "NaN",
          },
        },
        detectors: {},
        camera_fps: Number.NaN,
        process_fps: Number.NaN,
        skipped_fps: 0,
        detection_fps: Number.NaN,
        cpu_usages: {},
        processes: {},
        service: {
          uptime: Number.NaN,
          version: "0.9.0",
          latest_version: "0.9.0",
          last_updated: 1700000000,
          storage: {},
        },
      },
    });
    try {
      const html = renderToStaticMarkup(
        React.createElement(SystemWorkspace, {
          cameraNames: ["front_door"],
          logService: "ninjanvr",
          setLogService: () => {},
          logLines: ["2026-11-05 INFO Engine started"],
          logSearch: "",
          setLogSearch: () => {},
          logPaused: false,
          setLogPaused: () => {},
          notify: () => {},
        })
      );
      expect(html).not.toContain("NaN");
      expect(html).toContain("HARDWARE &amp; PIPELINE TELEMETRY");
    } finally {
      useNvrStore.setState({ stats: prevStats });
    }
  });
});

describe("Phase 4C Cycle-2 — Review, Explore, Exports, System & Settings panels hardening suite", () => {
  test("resolveReviewThumbUrl blocks percent-encoded traversal and sensitive API routes while computeReviewPlaybackWindow caps duration at 24h", () => {
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/clips/%2e%2e/%2e%2e/api/logout",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/api/auth/logout",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/api/users/admin",
      })
    ).toBe("/api/front_door/latest.webp");

    // Unbounded end_time is capped to MAX_EXPORT_WINDOW_SEC (86400s)
    const cappedWin = computeReviewPlaybackWindow(1000, 1000 + 500000, 4, 6, 20);
    expect(cappedWin.startSec).toBe(996);
    expect(cappedWin.endSec - cappedWin.startSec).toBe(MAX_EXPORT_WINDOW_SEC);
  });

  test("resolveNextBriefingStep skips deleted review items in briefingQueueIds instead of aborting early", () => {
    const rev1: ReviewItem = {
      id: "rev_1",
      camera: "cam1",
      start_time: 100,
      end_time: 110,
      severity: "alert",
      thumb_path: "",
      has_been_reviewed: false,
      data: { objects: ["person"] },
    };
    const rev3: ReviewItem = {
      id: "rev_3",
      camera: "cam1",
      start_time: 200,
      end_time: 210,
      severity: "alert",
      thumb_path: "",
      has_been_reviewed: false,
      data: { objects: ["person"] },
    };
    // rev_2 was deleted while briefing was running
    const step = resolveNextBriefingStep(
      ["rev_1", "rev_2", "rev_3"],
      0,
      [rev1, rev3]
    );
    expect(step).toEqual({ nextIdx: 2, nextReview: rev3 });
    expect(
      resolveNextBriefingStep(["rev_1", "rev_2", "rev_3"], 2, [rev1, rev3])
    ).toBeNull();
  });

  test("resolveFiniteEventScore and ExploreWorkspace prevent NaN% and NaNs when event scores or timestamps are NaN", () => {
    const nanEvent: EventItem = {
      id: "ev_nan_1",
      camera: "front_door",
      label: "person",
      sub_label: "Bruno",
      zones: ["porch"],
      start_time: Number.NaN,
      end_time: Number.NaN,
      top_score: Number.NaN,
      score: Number.NaN,
      has_clip: true,
      has_snapshot: true,
      retain_indefinitely: false,
      false_positive: false,
      box: null,
      data: {
        top_score: Number.NaN,
        detected_attributes: [
          {
            label: "face",
            score: Number.NaN,
            norm_bbox: [Number.NaN, 0.1, 0.2, 0.3],
            area_px: Number.NaN,
          },
        ],
      },
    };
    expect(resolveFiniteEventScore(nanEvent, 0.85)).toBe(0.85);

    const html = renderToStaticMarkup(
      React.createElement(ExploreWorkspace, {
        cameraNames: ["front_door"],
        filteredEvents: [nanEvent],
        exploreQuery: "",
        setExploreQuery: () => {},
        exploreCamera: "all",
        setExploreCamera: () => {},
        exploreLabel: "all",
        setExploreLabel: () => {},
        exploreRetainedOnly: false,
        setExploreRetainedOnly: () => {},
        globalDaysSummary: [],
        reviewDateFilter: "",
        setReviewDateFilter: () => {},
        selectedEvent: nanEvent,
        setSelectedEvent: () => {},
        eventViewMode: "snapshot",
        setEventViewMode: () => {},
        handleEventRetain: async () => {},
        handleEventDelete: async () => {},
        setSyncMasterTs: () => {},
        switchTab: () => {},
        notify: () => {},
      })
    );
    expect(html).not.toContain("NaN");
    expect(html).toContain("85%");
    expect(html).toContain("88%");
  });

  test("resolveSafeExportVideoUrl rejects percent-encoded traversal and non-export API routes", () => {
    expect(
      resolveSafeExportVideoUrl(
        "/exports/%2e%2e/%2e%2e/api/logout",
        "exp_1"
      )
    ).toBe("/exports/exp_1.mp4");
    expect(
      resolveSafeExportVideoUrl("/api/auth/logout", "exp_1")
    ).toBe("/exports/exp_1.mp4");
    expect(
      resolveSafeExportVideoUrl("javascript:alert(1)", "%2e%2e")
    ).toBe("");
  });

  test("normalizeRtspProducerEndpoint rejects 0.0.0.0, [::], and trailing-dot localhost", () => {
    expect(
      normalizeRtspProducerEndpoint("rtsp://0.0.0.0:8554/front_door")
    ).toBeNull();
    expect(
      normalizeRtspProducerEndpoint("rtsp://[::]:8554/front_door")
    ).toBeNull();
    expect(
      normalizeRtspProducerEndpoint("rtsp://localhost.:8554/front_door")
    ).toBeNull();
  });

  test("ConsoleAndStreamsPanels: buildOnvifPasswordPatch and sanitizeConfigEntrySlug preserve secret masks and block prototype pollution", () => {
    expect(sanitizeConfigEntrySlug("__proto__")).toBe("");
    expect(sanitizeConfigEntrySlug("constructor")).toBe("");
    expect(sanitizeConfigEntrySlug("prototype")).toBe("");
    expect(sanitizeConfigEntrySlug("  Front Gate #1 ")).toBe("front_gate__1");

    // When no password was saved, empty input stays empty instead of falsely setting "***"
    expect(buildOnvifPasswordPatch("", false, false)).toEqual({
      password: "",
      has_password: false,
      clear_password: false,
    });
    // When a password was previously saved, empty input preserves "***"
    expect(buildOnvifPasswordPatch("", true, false)).toEqual({
      password: "***",
      has_password: true,
      clear_password: false,
    });
    // Explicit clear request sets clear_password: true and clears password
    expect(buildOnvifPasswordPatch("", true, true)).toEqual({
      password: "",
      has_password: false,
      clear_password: true,
    });
  });

  test("RecordingAndSecurityPanels: resolveEffectiveRecordConfig deep-merges partial alerts/detections and validateFederationPeerUrl blocks SSRF", () => {
    const baseRec = {
      enabled: true,
      expire_interval: 60,
      continuous: { days: 14 },
      motion: { days: 0 },
      alerts: {
        pre_capture: 5,
        post_capture: 5,
        retain: { days: 14, mode: "motion" },
      },
      detections: {
        pre_capture: 5,
        post_capture: 5,
        retain: { days: 14, mode: "motion" },
      },
      preview: { quality: "medium" },
    };
    const merged = resolveEffectiveRecordConfig(baseRec, {
      enabled: true,
      alerts: { pre_capture: 8, post_capture: 10 },
    });
    expect(merged.alerts?.pre_capture).toBe(8);
    expect(merged.alerts?.retain).toEqual({ days: 14, mode: "motion" });
    expect(merged.detections?.retain).toEqual({ days: 14, mode: "motion" });

    expect(normalizeAllowedCamerasScope(" front_door , garagem , front_door , ../bad ")).toBe(
      "front_door,garagem"
    );
    expect(normalizeAllowedCamerasScope("   ")).toBe("*");

    // Valid remote NVR peer
    expect(
      validateFederationPeerUrl("https://nvr-remote.example.test:8971/")
    ).toEqual({
      valid: true,
      normalizedUrl: "https://nvr-remote.example.test:8971",
    });

    // SSRF / unsafe peer URLs are rejected
    expect(validateFederationPeerUrl("javascript:alert(1)").valid).toBe(false);
    expect(
      validateFederationPeerUrl("http://admin:secret@192.0.2.10:8971").valid
    ).toBe(false);
    expect(
      validateFederationPeerUrl("http://169.254.169.254/latest/meta-data").valid
    ).toBe(false);
    expect(
      validateFederationPeerUrl("http://metadata.google.internal.").valid
    ).toBe(false);
    expect(validateFederationPeerUrl("http://127.0.0.1:5000").valid).toBe(false);
    expect(validateFederationPeerUrl("http://localhost:8971").valid).toBe(false);
    expect(validateFederationPeerUrl("http://0.0.0.0:5000").valid).toBe(false);
    expect(validateFederationPeerUrl("http://[::1]:5000").valid).toBe(false);
    expect(validateFederationPeerUrl("http://[fe80::1]:5000").valid).toBe(false);
  });

  test("VisionAndAiPanels: formatDhash64Hex, computeDatasetReadinessMetrics, buildModelUploadPayload, buildLocalFinetuneCliCommand, and isSafeGenAiCustomBaseUrl guard against Infinity, NaN, path traversal, shell injection, and SSRF", () => {
    // Infinity does not throw RangeError in BigInt conversion
    expect(formatDhash64Hex(Number.POSITIVE_INFINITY)).toBe("—");
    expect(formatDhash64Hex(Number.NEGATIVE_INFINITY)).toBe("—");
    expect(formatDhash64Hex(Number.NaN)).toBe("—");
    expect(formatSignatureBbox([Number.NaN, Number.POSITIVE_INFINITY, 0.5, 0.75])).toBe(
      "[0.00, 0.00, 0.50, 0.75]"
    );

    // NaN box / annotation values do not produce "NaN"
    expect(
      xyxyToYoloNormalized([Number.NaN, Number.NaN, 0.5, 0.5])
    ).toEqual({
      cx: 0.25,
      cy: 0.25,
      w: 0.5,
      h: 0.5,
    });
    expect(
      formatYoloAnnotationLine({
        class_id: Number.NaN,
        label: "person",
        cx: Number.NaN,
        cy: 0.5,
        w: Number.NaN,
        h: 0.4,
      })
    ).toBe("0 0.000000 0.500000 0.001000 0.400000");

    // Readiness metrics clamp and sanitize NaN or inconsistent counts
    const nanMetrics = computeDatasetReadinessMetrics({
      total_samples: Number.NaN,
      true_positive_count: Number.NaN,
      ai_annotated_count: 10,
      hard_negative_count: 5,
      train_count: 12,
      val_count: 3,
      verified_count: 20, // greater than positive+hn
    });
    expect(Number.isFinite(nanMetrics.totalSamples)).toBe(true);
    expect(nanMetrics.verifiedPct).toBeLessThanOrEqual(100);
    expect(nanMetrics.positivePct).toBeLessThanOrEqual(100);

    // Model upload sanitizes traversal filenames and strips data:*;base64, prefix
    const upload = buildModelUploadPayload({
      filename: "../../etc/cron.d/evil.onnx",
      modelBase64: "data:application/octet-stream;base64,U1lOVEhFVElDX09OTlg=",
      width: Number.NaN,
      height: -100,
    });
    expect(upload.filename).toBe("evil.onnx");
    expect(upload.model_base64).toBe("U1lOVEhFVElDX09OTlg=");
    expect(upload.width).toBe(640);
    expect(upload.height).toBe(640);

    // Local finetune CLI command strips shell metacharacters and handles NaN epochs/freeze
    const safeCmd = buildLocalFinetuneCliCommand({
      nvrUrl: "https://nvr.example.com:8971; rm -rf /",
      datasetPath: "data.zip && whoami",
      modelArch: "yolo11s.pt | cat /etc/passwd",
      epochs: Number.NaN,
      freeze: Number.NaN,
      nvrToken: "bad;token",
    });
    expect(safeCmd).not.toContain(";");
    expect(safeCmd).not.toContain("&&");
    expect(safeCmd).not.toContain("|");
    expect(safeCmd).not.toContain("NaN");
    expect(safeCmd).toContain("--epochs 30 --freeze 10");
    expect(safeCmd).toContain("--nvr-token $NINJANVR_API_TOKEN");

    // GenAI custom base URL SSRF validation
    expect(isSafeGenAiCustomBaseUrl("https://openrouter.ai/api/v1")).toBe(true);
    expect(isSafeGenAiCustomBaseUrl("http://127.0.0.1:11434")).toBe(true);
    expect(isSafeGenAiCustomBaseUrl("http://127.0.0.1:5000")).toBe(false);
    expect(isSafeGenAiCustomBaseUrl("http://169.254.169.254/latest")).toBe(false);
    expect(isSafeGenAiCustomBaseUrl("http://user:pass@openrouter.ai")).toBe(false);
    expect(isSafeGenAiCustomBaseUrl("javascript:alert(1)")).toBe(false);
  });

  test("VisionAndAiPanels Settings UI/UX: standardizes scope badges, sub-navigation, SSD attribute warnings, FP filtering, disabled zones, and i18n strings", () => {
    // 1. Sub-navigation pills & visibility helper
    // The section opens on a single module; "All Modules" is the last pill.
    expect(AI_SETTINGS_SUBSECTIONS.map((s) => s.id)).toEqual([
      "engine",
      "objects",
      "genai",
      "faces",
      "lpr",
      "state_rois",
      "all",
    ]);
    expect(DEFAULT_AI_SUBSECTION).toBe("engine");
    expect(isAiSubSectionVisible("all", "engine")).toBe(true);
    expect(isAiSubSectionVisible("all", "state_rois")).toBe(true);
    expect(isAiSubSectionVisible("faces", "faces")).toBe(true);
    expect(isAiSubSectionVisible("faces", "lpr")).toBe(false);

    // 2. Scope badge standardization (Global vs Camera vs Camera-Only)
    expect(
      resolveSettingsScopeBadge("global_only", "escritorio", "escritorio", false)
    ).toEqual({
      label: "SCOPE: GLOBAL",
      isCameraScope: false,
      effectiveCamera: "escritorio",
    });
    expect(
      resolveSettingsScopeBadge("camera_or_global", "global", "escritorio", false)
    ).toEqual({
      label: "SCOPE: GLOBAL DEFAULTS",
      isCameraScope: false,
      effectiveCamera: "escritorio",
    });
    expect(
      resolveSettingsScopeBadge("camera_or_global", "escritorio", "escritorio", true)
    ).toEqual({
      label: "ESCOPO: ESCRITORIO",
      isCameraScope: true,
      effectiveCamera: "escritorio",
    });
    expect(
      resolveSettingsScopeBadge("camera_only", "global", "sala", false)
    ).toEqual({
      label: "SCOPE: CAMERA (SALA)",
      isCameraScope: true,
      effectiveCamera: "sala",
    });

    // 3. Bundled COCO SSD model detection for child attribute warning
    expect(
      isBundledCocoSsdModel({
        model_type: "ssd",
        path: "/openvino-model/ssdlite_mobilenet_v2.xml",
      })
    ).toBe(true);
    expect(
      isBundledCocoSsdModel({
        model_type: "yolo-generic",
        path: "/config/models/ninjanvr_security_15cl.onnx",
      })
    ).toBe(false);

    // 4. False-positive signature camera scope filtering & reason formatting
    const sampleSigs = [
      {
        id: 1,
        camera: "sala",
        label: "person",
        norm_bbox: [0.58, 0.08, 0.42, 0.95] as [number, number, number, number],
        dhash64: 12345,
        hit_count: 1,
        created_at: 1700000000,
        last_seen_at: 1700000000,
        reason: "The room is empty",
      },
      {
        id: 2,
        camera: "escritorio",
        label: "person",
        norm_bbox: [0.73, 0.37, 0.19, 0.49] as [number, number, number, number],
        dhash64: 67890,
        hit_count: 2,
        created_at: 1700000100,
        last_seen_at: 1700000100,
        reason: "color:gray",
      },
    ];
    expect(
      filterFalsePositiveSignaturesByScope(
        sampleSigs,
        "escritorio",
        "escritorio",
        "scope"
      ).map((s) => s.id)
    ).toEqual([2]);
    expect(
      filterFalsePositiveSignaturesByScope(
        sampleSigs,
        "escritorio",
        "escritorio",
        "all"
      ).map((s) => s.id)
    ).toEqual([1, 2]);
    expect(
      filterFalsePositiveSignaturesByScope(
        sampleSigs,
        "global",
        "escritorio",
        "scope"
      ).map((s) => s.id)
    ).toEqual([1, 2]);

    expect(formatFalsePositiveReason("color:gray", null, false)).toEqual({
      text: "Static region signature (dominant color: gray)",
      isTechnicalFallback: true,
    });
    expect(formatFalsePositiveReason("operator_marked", null, true)).toEqual({
      text: "Marcado manualmente pelo operador",
      isTechnicalFallback: true,
    });
    expect(
      formatFalsePositiveReason("No person is visible in the kitchen scene.", null, false)
    ).toEqual({
      text: "No person is visible in the kitchen scene.",
      isTechnicalFallback: false,
    });

    // 5. SSR render verification of AiDetectionObjectsSection
    const html = renderToStaticMarkup(
      React.createElement(AiDetectionObjectsSection, {
        draftConfig: {
          version: "2.6.7",
          safe_mode: false,
          mqtt: {
            enabled: false,
            host: "localhost",
            port: 1883,
            topic_prefix: "ninjanvr",
            client_id: "ninjanvr",
            stats_interval: 60,
          },
          record: {
            enabled: true,
            expire_interval: 60,
            continuous: { days: 7 },
            motion: { days: 14 },
            alerts: { pre_capture: 5, post_capture: 5, retain: { days: 30, mode: "motion" } },
            detections: { pre_capture: 5, post_capture: 5, retain: { days: 14, mode: "motion" } },
            preview: { quality: "medium" },
          },
          snapshots: { enabled: true, selection: "best_in_zone" },
          go2rtc: { streams: {} },
          detectors: { detector01: { type: "openvino", device: "CPU" } },
          model: {
            path: "/openvino-model/ssdlite_mobilenet_v2.xml",
            labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
            width: 300,
            height: 300,
            input_tensor: "nhwc",
            input_pixel_format: "bgr",
            model_type: "ssd",
          },
          detect: {
            enabled: true,
            width: 1280,
            height: 720,
            fps: 5,
            min_initialized: 2,
            max_disappeared: 25,
          },
          motion: {
            enabled: true,
            threshold: 30,
            contour_area: 10,
            mask: [],
          },
          objects: { track: ["person"], filters: {} },
          genai: {
            enabled: true,
            provider: "gemini",
            base_url: null,
            api_key: null,
            has_api_key: true,
            api_key_env: "GEMINI_API_KEY",
            model: "gemini-3.8-flash",
            verify_ambiguous_alerts: false,
            ambiguous_score_min: 0.45,
            ambiguous_score_max: 0.72,
            auto_suppress_false_positives: true,
            timeout_ms: 30000,
          },
          face_recognition: {
            enabled: false,
            model_path: "/config/models/arcface_r50.onnx",
            similarity_threshold: 0.68,
            min_face_area: 750,
            unknown_alert: false,
          },
          lpr: {
            enabled: false,
            min_confidence: 0.75,
            format_regex: "^[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$",
            known_plates: {},
          },
          cameras: {
            escritorio: {
              enabled: true,
              ffmpeg: { inputs: [] },
              detect: { enabled: true, width: 1280, height: 720, fps: 5 },
              motion: { enabled: true, threshold: 30, contour_area: 10, mask: [] },
              record: {
                enabled: true,
                expire_interval: 60,
                continuous: { days: 7 },
                motion: { days: 14 },
                alerts: {
                  pre_capture: 5,
                  post_capture: 5,
                  retain: { days: 30, mode: "motion" },
                },
                detections: {
                  pre_capture: 5,
                  post_capture: 5,
                  retain: { days: 14, mode: "motion" },
                },
                preview: { quality: "medium" },
              },
              snapshots: { enabled: true },
              objects: { track: ["person"], filters: {} },
              zones: {},
              state_rois: {},
            },
          },
        },
        updateDraft: () => {},
        scope: "escritorio",
        inspectorCam: "escritorio",
        notify: () => {},
        initialSubSection: "all",
        initialDataCardsOpen: true,
      })
    );

    expect(html).toContain("nvr-settings-subnav");
    // Scope is shown once per group by dividers, not repeated as per-card pills.
    expect(html).toContain("nvr-scope-divider");
    expect(html).toContain("GLOBAL · 1 CAMERA");
    expect(html).toContain("ESCRITORIO · THIS CAMERA ONLY");
    expect(html).not.toContain("nvr-scope-pill");
    expect(html).toContain("nvr-yaml-key-tag");
    expect(html).toContain("nvr-cfg-disabled-zone");
    expect(html).toContain("nvr-file-picker");
    expect(html).toContain("nvr-diag-drawer");
    expect(html).toContain("REQUIRES 15-CLASS YOLO MODEL");
    expect(html).toContain("ROI Crop Viewport Preview");
    expect(html).not.toContain("Advanced Technical Tuning / Coordenadas");
    expect(html).not.toContain("e.g. Porta do Escritório");
  });
});

describe("Phase 4C Cycle-3 — Adversarial SSRF, traversal, secret-mask, CLI injection & state race regression suite", () => {
  test("resolveReviewThumbUrl blocks dot camera traversal, double percent-encoding, query strings, and non-image /api/* endpoints", () => {
    expect(
      resolveReviewThumbUrl({
        camera: "..",
        thumb_path: "",
      })
    ).toBe("/api/unknown/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: ".",
        thumb_path: "",
      })
    ).toBe("/api/unknown/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/clips/%252e%252e/%252e%252e/api/logout",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/api/ai/context?hours=24",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/api/dataset/export.zip",
      })
    ).toBe("/api/front_door/latest.webp");
    expect(
      resolveReviewThumbUrl({
        camera: "front_door",
        thumb_path: "/api/events/ev_123/thumbnail.webp",
      })
    ).toBe("/api/events/ev_123/thumbnail.webp");
  });

  test("resolveNextBriefingStep reconciles activeReviewId when operator manually selects a later queue item", () => {
    const rev1: ReviewItem = {
      id: "rev_1",
      camera: "cam1",
      start_time: 100,
      end_time: 110,
      severity: "alert",
      thumb_path: "",
      has_been_reviewed: false,
      data: { objects: ["person"] },
    };
    const rev2: ReviewItem = { ...rev1, id: "rev_2", start_time: 200 };
    const rev3: ReviewItem = { ...rev1, id: "rev_3", start_time: 300 };
    const step = resolveNextBriefingStep(
      ["rev_1", "rev_2", "rev_3"],
      0,
      [rev1, rev2, rev3],
      "rev_2"
    );
    expect(step).toEqual({ nextIdx: 2, nextReview: rev3 });
  });

  test("parseSavedExplorePresets rejects out-of-range minScore > 100 and resolveSafeExportVideoUrl blocks double-encoded traversal and dot exportIds", () => {
    const presets = parseSavedExplorePresets(
      JSON.stringify([
        { name: "Corrupted High Score", minScore: 999 },
        { name: "Valid Score", minScore: 85 },
      ])
    );
    expect(presets[0]?.minScore).toBe(0);
    expect(presets[1]?.minScore).toBe(85);

    expect(
      resolveSafeExportVideoUrl(
        "/exports/%252e%252e/%252e%252e/api/logout",
        "exp_1"
      )
    ).toBe("/exports/exp_1.mp4");
    expect(
      resolveSafeExportVideoUrl("/exports/clip.mp4?download=1", "exp_1")
    ).toBe("/exports/exp_1.mp4");
    expect(resolveSafeExportVideoUrl("javascript:alert(1)", ".")).toBe("");
    expect(
      resolveSafeExportVideoUrl("javascript:alert(1)", "%252e%252e")
    ).toBe("");
  });

  test("normalizeRtspProducerEndpoint scrubs query-string credentials, strips ffmpeg: prefix, and blocks IPv4-mapped IPv6 loopback", () => {
    expect(
      normalizeRtspProducerEndpoint(
        "ffmpeg:rtsp://192.0.2.10:554/live/main?user=admin&password=SuperSecret123&channel=1"
      )
    ).toBe("rtsp://192.0.2.10:554/live/main?user=admin&password=***&channel=1");
    expect(
      normalizeRtspProducerEndpoint(
        "rtsp://192.0.2.10:554/live/main?user=admin&password=***&channel=1"
      )
    ).toBe("rtsp://192.0.2.10:554/live/main?user=admin&password=***&channel=1");
    expect(
      normalizeRtspProducerEndpoint("rtsp://[::ffff:127.0.0.1]:8554/front_door")
    ).toBeNull();
    expect(
      normalizeRtspProducerEndpoint("rtsp://sub.localhost:8554/front_door")
    ).toBeNull();
  });

  test("sanitizeConfigEntrySlug, slugifyRoiLocationName, normalizeFaceProfileName, and buildOnvifPasswordPatch reject pure-punctuation slugs, prototype pollution, and mask sentinels", () => {
    expect(sanitizeConfigEntrySlug("___")).toBe("");
    expect(sanitizeConfigEntrySlug("---")).toBe("");
    expect(sanitizeConfigEntrySlug("!@#$%")).toBe("");

    expect(slugifyRoiLocationName("__proto__")).toBe("");
    expect(slugifyRoiLocationName("constructor")).toBe("");
    expect(slugifyRoiLocationName("prototype")).toBe("");
    expect(slugifyRoiLocationName("___")).toBe("");
    expect(slugifyRoiLocationName("Portão Principal")).toBe("portao_principal");

    expect(normalizeFaceProfileName("__proto__")).toBe("");
    expect(normalizeFaceProfileName("constructor")).toBe("");
    expect(normalizeFaceProfileName("prototype")).toBe("");
    expect(normalizeFaceProfileName("---")).toBe("");
    expect(normalizeFaceProfileName("Bruno Silva")).toBe("Bruno Silva");

    // Passing mask sentinel "***" or "*" when hasExistingPassword is false must not set password: "***"
    expect(buildOnvifPasswordPatch("***", false, false)).toEqual({
      password: "",
      has_password: false,
      clear_password: false,
    });
    expect(buildOnvifPasswordPatch("*", false, false)).toEqual({
      password: "",
      has_password: false,
      clear_password: false,
    });
    expect(buildOnvifPasswordPatch("***", true, false)).toEqual({
      password: "***",
      has_password: true,
      clear_password: false,
    });
  });

  test("validateFederationPeerUrl and isSafeGenAiCustomBaseUrl block IPv4-mapped IPv6, NAT64, *.localhost, cloud metadata, and non-Ollama localhost ports", () => {
    expect(validateFederationPeerUrl("http://[::ffff:127.0.0.1]:8971").valid).toBe(
      false
    );
    expect(
      validateFederationPeerUrl("http://[::ffff:169.254.169.254]/latest").valid
    ).toBe(false);
    expect(
      validateFederationPeerUrl("http://[64:ff9b::169.254.169.254]/latest").valid
    ).toBe(false);
    expect(validateFederationPeerUrl("http://[fd00:ec2::254]/latest").valid).toBe(
      false
    );
    expect(validateFederationPeerUrl("http://100.100.100.200/latest").valid).toBe(
      false
    );
    expect(validateFederationPeerUrl("http://node1.localhost:8971").valid).toBe(
      false
    );

    // GenAI custom base URL allows local Ollama (:11434) but rejects other localhost ports and IPv6-mapped metadata
    expect(isSafeGenAiCustomBaseUrl("http://127.0.0.1:11434")).toBe(true);
    expect(isSafeGenAiCustomBaseUrl("http://localhost:11434")).toBe(true);
    expect(isSafeGenAiCustomBaseUrl("http://127.0.0.1:6379")).toBe(false);
    expect(isSafeGenAiCustomBaseUrl("http://localhost")).toBe(false);
    expect(isSafeGenAiCustomBaseUrl("http://[::1]:8080")).toBe(false);
    expect(isSafeGenAiCustomBaseUrl("http://[::ffff:169.254.169.254]")).toBe(
      false
    );
    expect(isSafeGenAiCustomBaseUrl("http://[64:ff9b::169.254.169.254]")).toBe(
      false
    );
    expect(isSafeGenAiCustomBaseUrl("http://[fd00:ec2::254]")).toBe(false);
    expect(isSafeGenAiCustomBaseUrl("http://100.100.100.200")).toBe(false);
  });

  test("buildLocalFinetuneCliCommand and buildModelUploadPayload block CLI flag injection, subshells, and percent-encoded filename traversal", () => {
    const cli = buildLocalFinetuneCliCommand({
      datasetPath: "--evil-flag",
      modelArch: "-c",
      nvrToken: "--inject-arg",
      nvrUrl: "http://localhost:5000#(whoami)",
    });
    expect(cli).toContain("--dataset evil-flag");
    expect(cli).toContain("--model c");
    expect(cli).toContain("--nvr-token $NINJANVR_API_TOKEN");
    expect(cli).not.toContain("#");
    expect(cli).not.toContain("(whoami)");

    const upload = buildModelUploadPayload({
      filename: "..%2f..%2fetc%2fcron.d%2fbackdoor.onnx",
      modelBase64: "U1lOVEhFVElD",
      modelType: "invalid-arch",
      inputTensor: "invalid-tensor",
      inputPixelFormat: "invalid-color",
    });
    expect(upload.filename).toBe("backdoor.onnx");
    expect(upload.model_type).toBe("yolo-generic");
    expect(upload.input_tensor).toBe("nchw");
    expect(upload.input_pixel_format).toBe("rgb");
  });
});
