import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import { I18nProvider } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import type { EventItem, NinjaConfigResponse, ReviewItem } from "../types/nvr";
import { resolveReviewArchivePlayback } from "../utils/archivePlayback";
import {
  createConfigWorkbenchSession,
  createInMemoryConfigTransport,
} from "../utils/configWorkbench";
import {
  computeReviewPlaybackWindow,
  createForensicTriageController,
  createTwoStepConfirmController,
  isOngoingIncident,
  resolveAdjacentReviewAfterAck,
} from "../utils/forensicTriage";
import { resolveCanvasThemePalette, wcagContrastRatio } from "../utils/themeTokens";
import { FloorplanStudioCanvas } from "../components/FloorplanStudioCanvas";
import { ExploreWorkspace } from "../views/ExploreWorkspace";
import { ReviewWorkspace } from "../views/ReviewWorkspace";
import {
  SystemWorkspace,
  formatCameraTelemetryValue,
} from "../views/SystemWorkspace";
import { AiDetectionObjectsSection } from "../views/settings/VisionAndAiPanels";
import {
  resolveVisionTagStyle,
  shouldGuardDraftPolygonSwitch,
  ZonesStudio,
} from "../views/settings/ZonesStudio";

async function readSrcText(relativeFromTests: string): Promise<string> {
  const fileUrl = new URL(relativeFromTests, import.meta.url);
  const bunRuntime = (
    globalThis as unknown as {
      Bun: { file(url: URL): { text(): Promise<string> } };
    }
  ).Bun;
  return bunRuntime.file(fileUrl).text();
}

function makeReview(
  id: string,
  overrides: Partial<ReviewItem> = {}
): ReviewItem {
  return {
    id,
    camera: "front_gate",
    start_time: 1_700_000_000,
    end_time: 1_700_000_018,
    severity: "alert",
    thumb_path: `/clips/review/${id}.webp`,
    has_been_reviewed: false,
    data: {
      objects: ["person"],
      detections: [`ev_${id}`],
    },
    ...overrides,
  };
}

function makeEvent(
  id: string,
  overrides: Partial<EventItem> = {}
): EventItem {
  return {
    id,
    camera: "front_gate",
    label: "person",
    sub_label: null,
    zones: ["entry"],
    start_time: 1_700_000_000,
    end_time: 1_700_000_015,
    top_score: 0.91,
    score: 0.89,
    has_clip: true,
    has_snapshot: true,
    retain_indefinitely: false,
    false_positive: false,
    box: [0.1, 0.1, 0.4, 0.8],
    ...overrides,
  };
}

function makeSampleConfig(): NinjaConfigResponse {
  return {
    version: "0.18-0",
    safe_mode: false,
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
    cameras: {
      front_gate: {
        enabled: true,
        detect: { enabled: true, width: 1280, height: 720, fps: 5 },
        motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
        objects: { track: ["person"], filters: {} },
        zones: {},
        state_rois: {
          main_gate: {
            crop: [0.2, 0.3, 0.7, 0.85],
            classes: ["closed", "open"],
            interval_sec: 5,
            hysteresis: 3,
            anomaly_state: "open",
            anomaly_timeout_sec: 300,
          },
        },
        record: { enabled: true },
        snapshots: { enabled: true },
      },
    },
  };
}

describe("Web Triage, Config Workbench Guard, Honest Telemetry & Settings Decomposition", () => {
  test("configWorkbench blocks overwriting unsaved Raw config.yml edits during structured commits until resolved", async () => {
    const initialConfig = makeSampleConfig();
    const transport = createInMemoryConfigTransport({ initialConfig });
    const session = createConfigWorkbenchSession(transport, {
      initialConfig,
      initialRawYaml: await transport.fetchRawYaml(),
    });

    session.setRawYamlDraft("# custom operator yaml edit\nversion: 0.18-0\nsafe_mode: true\ncameras: {}\n");
    expect(session.getSnapshot().hasUnsavedRawEdits).toBe(true);
    expect(session.getSnapshot().isDirty).toBe(true);

    // Attempting to save structured config from a visual tab must fail closed without touching transport
    const blocked = await session.commitStructured({ restart: false });
    expect(blocked.ok).toBe(false);
    expect(blocked.status).toBe(409);
    expect(transport.committedStructuredDrafts.length).toBe(0);
    expect(session.getSnapshot().hasUnsavedRawEdits).toBe(true);
    expect(session.getSnapshot().rawYaml).toContain("# custom operator yaml edit");

    // Explicitly discarding Raw YAML edits restores clean state and unblocks structured saves
    session.discardRawEdits();
    expect(session.getSnapshot().hasUnsavedRawEdits).toBe(false);
    expect(session.getSnapshot().isDirty).toBe(false);

    session.mutateDraft((next) => {
      next.cameras.front_gate.motion.threshold = 42;
    });
    const saved = await session.commitStructured({ restart: false });
    expect(saved.ok).toBe(true);
    expect(transport.committedStructuredDrafts.length).toBe(1);
    expect(session.getSnapshot().baselineConfig?.cameras.front_gate.motion.threshold).toBe(42);
  });

  test("ongoing incidents (end_time: null) expand playback and export windows to nowSec instead of truncating at 25s", () => {
    const startSec = 1_700_100_000;
    const nowSec = startSec + 140; // 2m20s ongoing incident

    expect(isOngoingIncident(null)).toBe(true);
    expect(isOngoingIncident(undefined)).toBe(true);
    expect(isOngoingIncident(startSec + 15)).toBe(false);

    const win = computeReviewPlaybackWindow(startSec, null, 5, 5, 20, nowSec);
    expect(win).toEqual({
      startSec: startSec - 5,
      endSec: nowSec,
    });
    expect(win.endSec - startSec).toBe(140);

    // Fresh ongoing incident (< 20s old) still guarantees at least start_time + 20s
    const freshOngoing = computeReviewPlaybackWindow(
      startSec,
      null,
      4,
      6,
      20,
      startSec + 5
    );
    expect(freshOngoing).toEqual({
      startSec: startSec - 4,
      endSec: startSec + 20,
    });

    const ongoingReview = makeReview("rev_ongoing", {
      start_time: startSec,
      end_time: null,
    });
    const resolved = resolveReviewArchivePlayback(ongoingReview, null, {
      nowSec,
      configuredCameras: ["front_gate"],
    });
    expect(resolved.vodWindow.endSec).toBe(nowSec);
    expect(resolved.exportWindow).toEqual({
      startSec: startSec - 5,
      endSec: nowSec,
    });
  });

  test("resolveAdjacentReviewAfterAck advances to the adjacent next item on [R] instead of teleporting to index 0", () => {
    const queue = [
      makeReview("rev_0"),
      makeReview("rev_1"),
      makeReview("rev_2"),
      makeReview("rev_3"),
      makeReview("rev_4"),
    ];

    // Acknowledging index 2 in unreviewed filter selects rev_3 (adjacent next), never rev_0
    const nextFromMiddle = resolveAdjacentReviewAfterAck(
      queue,
      ["rev_2"],
      "rev_2",
      "unreviewed"
    );
    expect(nextFromMiddle?.id).toBe("rev_3");

    // Acknowledging the last item falls back to the preceding item (rev_3)
    const nextFromTail = resolveAdjacentReviewAfterAck(
      queue,
      ["rev_4"],
      "rev_4",
      "unreviewed"
    );
    expect(nextFromTail?.id).toBe("rev_3");

    // Acknowledging every remaining item clears active selection cleanly
    const emptyAfterBatch = resolveAdjacentReviewAfterAck(
      [queue[0]],
      ["rev_0"],
      "rev_0",
      "unreviewed"
    );
    expect(emptyAfterBatch).toBeNull();

    // When viewing "all" statuses, the item remains in the list so activeReview stays put
    const staysInAll = resolveAdjacentReviewAfterAck(
      queue,
      ["rev_2"],
      "rev_2",
      "all"
    );
    expect(staysInAll?.id).toBe("rev_2");
  });

  test("SystemWorkspace formats missing camera telemetry as honest '—' without fake 5.0 FPS or 1.2% CPU defaults", () => {
    expect(formatCameraTelemetryValue(undefined, 1)).toBe("—");
    expect(formatCameraTelemetryValue(null, 1, "%")).toBe("—");
    expect(formatCameraTelemetryValue("", 1, "%")).toBe("—");
    expect(formatCameraTelemetryValue("N/A", 1, "%")).toBe("—");
    expect(formatCameraTelemetryValue(Number.NaN, 1)).toBe("—");
    expect(formatCameraTelemetryValue(0, 1)).toBe("0.0");
    expect(formatCameraTelemetryValue("2.4%", 1, "%")).toBe("2.4%");

    const prevStats = useNvrStore.getState().stats;
    useNvrStore.setState({ stats: null });
    try {
      const html = renderToStaticMarkup(
        React.createElement(SystemWorkspace, {
          cameraNames: ["lobby_cam"],
          logService: "ninjanvr",
          setLogService: () => {},
          logLines: ["2026-11-17 INFO Ready"],
          logSearch: "",
          setLogSearch: () => {},
          logPaused: false,
          setLogPaused: () => {},
          notify: () => {},
          initialRightMode: "logs",
        })
      );
      expect(html).toContain("lobby_cam");
      expect(html).toContain("<td>—</td>");
      expect(html).not.toContain("5.0");
      expect(html).not.toContain("1.2%");
      expect(html).not.toContain("1.0%");
      expect(html).toContain("INFERENCE");
      expect(html).not.toContain("INFERÊNCIA");
      expect(html).toContain("var(--border-divider)");
      expect(html).not.toContain("rgba(255,255,255,0.03)");
    } finally {
      useNvrStore.setState({ stats: prevStats });
    }
  });

  test("ReviewWorkspace & ExploreWorkspace render pagination, Clear All Filters, ONGOING badge, AI triage actions, and non-nested rows", async () => {
    const sixtyReviews = Array.from({ length: 62 }, (_, i) =>
      makeReview(`rev_${i + 1}`, {
        end_time: i === 0 ? null : 1_700_000_020,
      })
    );
    const reviewHtml = renderToStaticMarkup(
      React.createElement(ReviewWorkspace, {
        cameraNames: ["front_gate"],
        reviews: sixtyReviews,
        filteredReviews: sixtyReviews,
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
        activeReview: sixtyReviews[0],
        setActiveReview: () => {},
        focusSegments: [],
        handleMarkReviews: async () => {},
        handleDeleteReviews: async () => {},
        handleQuickExport: async () => {},
        setSyncMasterTs: () => {},
        switchTab: () => {},
      })
    );

    expect(reviewHtml).toContain("ONGOING");
    expect(reviewHtml).toContain("Show 50 more (12 remaining)");
    expect(reviewHtml).toContain("Mark False Positive");
    expect(reviewHtml).toContain("Verify with AI");
    expect(reviewHtml).toContain("Retain Indefinitely");
    expect(reviewHtml).not.toContain('role="button"');

    // Empty filtered Review state renders Clear All Filters button
    const emptyReviewHtml = renderToStaticMarkup(
      React.createElement(ReviewWorkspace, {
        cameraNames: ["front_gate"],
        reviews: sixtyReviews,
        filteredReviews: [],
        reviewCameraFilter: "front_gate",
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
        activeReview: null,
        setActiveReview: () => {},
        focusSegments: [],
        handleMarkReviews: async () => {},
        handleDeleteReviews: async () => {},
        handleQuickExport: async () => {},
        setSyncMasterTs: () => {},
        switchTab: () => {},
      })
    );
    expect(emptyReviewHtml).toContain("Clear All Filters");

    // ExploreWorkspace renders 50-item pagination and non-nested rows
    const sixtyEvents = Array.from({ length: 58 }, (_, i) =>
      makeEvent(`ev_${i + 1}`)
    );
    const exploreHtml = renderToStaticMarkup(
      React.createElement(ExploreWorkspace, {
        cameraNames: ["front_gate"],
        filteredEvents: sixtyEvents,
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
        selectedEvent: sixtyEvents[0],
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
    expect(exploreHtml).toContain("Show 50 more (8 remaining)");
    expect(exploreHtml).not.toContain('role="button"');

    // ForensicTriageController supports review-scoped toggleEventRetain
    const calls: Array<{ url: string; method: string }> = [];
    const ctrl = createForensicTriageController({
      fetchImpl: (async (input, init) => {
        calls.push({
          url: String(input),
          method: init?.method || "GET",
        });
        return new Response(JSON.stringify({ retain_indefinitely: true }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }) as typeof fetch,
    });
    ctrl.setActiveTarget("review", "rev_1");
    const retainRes = await ctrl.toggleEventRetain("ev_rev_1", false, {
      targetKind: "review",
      targetId: "rev_1",
    });
    expect(retainRes.ok).toBe(true);
    expect(retainRes.nextRetained).toBe(true);
    expect(retainRes.shouldApply).toBe(true);
    expect(calls).toEqual([{ url: "/api/events/ev_rev_1/retain", method: "POST" }]);
  });

  test("Settings decomposition, ROI deduplication, Floorplan contrast/i18n, ZonesStudio badge contrast, and 2-step confirmation", async () => {
    // 1. Privacy-mask badge contrast meets WCAG AA (>= 4.5:1; actual ~7.58:1)
    const privacyTag = resolveVisionTagStyle("privacy_mask");
    expect(privacyTag).toEqual({ background: "#475569", color: "#ffffff" });
    expect(
      wcagContrastRatio(privacyTag.color, privacyTag.background)
    ).toBeGreaterThanOrEqual(7.0);

    // 2. Draft polygon vertex guard helper
    expect(shouldGuardDraftPolygonSwitch([])).toBe(false);
    expect(shouldGuardDraftPolygonSwitch([[0.2, 0.3]])).toBe(true);

    // 3. Deterministic 3s 2-step confirmation controller
    let nowMs = 1_000;
    const confirmCtrl = createTwoStepConfirmController(3000, () => nowMs);
    expect(confirmCtrl.trigger("delete:cam_a")).toBe("armed");
    expect(confirmCtrl.isArmed("delete:cam_a")).toBe(true);
    expect(confirmCtrl.trigger("delete:cam_a")).toBe("confirmed");
    expect(confirmCtrl.isArmed("delete:cam_a")).toBe(false);

    // Expires after 3000ms and requires arming again
    expect(confirmCtrl.trigger("revoke:tok_1")).toBe("armed");
    nowMs += 3001;
    expect(confirmCtrl.isArmed("revoke:tok_1")).toBe(false);
    expect(confirmCtrl.trigger("revoke:tok_1")).toBe("armed");

    // 4. Deduplicated State Monitor (ROI) card links directly to Tab 03 without raw float form
    const cfg = makeSampleConfig();
    const roiHtml = renderToStaticMarkup(
      React.createElement(AiDetectionObjectsSection, {
        draftConfig: cfg,
        updateDraft: () => {},
        scope: "front_gate",
        inspectorCam: "front_gate",
        notify: () => {},
        onOpenVectorStudio: () => {},
        initialSubSection: "state_rois",
      })
    );
    expect(roiHtml).toContain("STATE MONITORS (ROI)");
    expect(roiHtml).toContain("MAIN_GATE");
    expect(roiHtml).toContain("Edit ROIs in Visual Studio (Tab 03)");
    expect(roiHtml).not.toContain("Normalized Crop [x1,y1,x2,y2]");

    // 5. ZonesStudio renders cleanly with configured state_rois
    const zsHtml = renderToStaticMarkup(
      React.createElement(ZonesStudio, {
        draftConfig: cfg,
        savedConfig: cfg,
        updateDraft: () => {},
        inspectorCam: "front_gate",
        draftPoints: [],
        setDraftPoints: () => {},
        notify: () => {},
        reviewOpen: false,
        onOpenReview: () => {},
        onCloseReview: () => {},
        onSave: () => {},
        onDiscard: () => {},
        isSaving: false,
        isDirty: false,
        saveOutcome: null,
      })
    );
    expect(zsHtml).toContain("Areas and lines");

    // 6. FloorplanStudioCanvas localizes labels in both English and Portuguese modes and uses var(--signal-accent-text)
    const winRef = (
      globalThis as unknown as {
        window?: { localStorage?: { getItem(k: string): string | null; setItem(k: string, v: string): void; removeItem(k: string): void } };
      }
    ).window;
    const prevSavedLocale = winRef?.localStorage?.getItem("ninjanvr_locale") ?? null;
    try {
      winRef?.localStorage?.setItem("ninjanvr_locale", "en");
      const fpHtmlEn = renderToStaticMarkup(
        React.createElement(
          I18nProvider,
          null,
          React.createElement(FloorplanStudioCanvas, {
            cameraNames: ["front_gate"],
            effectiveCameraNames: ["front_gate"],
            effectiveCamerasConfig: cfg.cameras,
            groupCams: ["front_gate"],
            selectedCamera: "front_gate",
            onSelectCamera: () => {},
            onOpenFocus: () => {},
            config: cfg,
            stats: null,
            cameraActivity: {},
            hardwareInfoByCam: {},
            showVectors: true,
            mapTheme: resolveCanvasThemePalette("light"),
          })
        )
      );
      expect(fpHtmlEn).toContain("Dimensions (m)");
      expect(fpHtmlEn).toContain("Range (m)");
      expect(fpHtmlEn).toContain("ROOMS (0) // DIMENSIONS IN METERS");
      expect(fpHtmlEn).not.toContain("MEDIDAS EM METROS");
      expect(fpHtmlEn).toContain("var(--signal-accent-text)");

      winRef?.localStorage?.setItem("ninjanvr_locale", "pt");
      const fpHtmlPt = renderToStaticMarkup(
        React.createElement(
          I18nProvider,
          null,
          React.createElement(FloorplanStudioCanvas, {
            cameraNames: ["front_gate"],
            effectiveCameraNames: ["front_gate"],
            effectiveCamerasConfig: cfg.cameras,
            groupCams: ["front_gate"],
            selectedCamera: "front_gate",
            onSelectCamera: () => {},
            onOpenFocus: () => {},
            config: cfg,
            stats: null,
            cameraActivity: {},
            hardwareInfoByCam: {},
            showVectors: true,
            mapTheme: resolveCanvasThemePalette("light"),
          })
        )
      );
      expect(fpHtmlPt).toContain("Medidas (m)");
      expect(fpHtmlPt).toContain("Alcance (m)");
      expect(fpHtmlPt).toContain("ROOMS (0) // MEDIDAS EM METROS");
    } finally {
      if (prevSavedLocale === null) {
        winRef?.localStorage?.removeItem("ninjanvr_locale");
      } else {
        winRef?.localStorage?.setItem("ninjanvr_locale", prevSavedLocale);
      }
    }

    // 7. Source-level hygiene: no window.confirm in DatasetStudio, no hardcoded "escritorio" in VisionAndAiPanels, no forced setLocale("en") on Reset Defaults
    const datasetSrc = await readSrcText("../views/settings/DatasetStudio.tsx");
    expect(datasetSrc).not.toContain("window.confirm");

    const visionSrc = await readSrcText("../views/settings/VisionAndAiPanels.tsx");
    expect(visionSrc).not.toContain('"escritorio"');
    expect(visionSrc.split("\n").length).toBeLessThan(1000);

    const lprSrc = await readSrcText("../views/settings/vision/LprSettingsSection.tsx");
    expect(lprSrc).not.toContain('"escritorio"');
    expect(lprSrc).toContain("Silver SUV (BRA2E19)");

    const consoleSrc = await readSrcText("../views/settings/ConsoleAndStreamsPanels.tsx");
    expect(consoleSrc).not.toContain('setLocale("en")');
  });
});
