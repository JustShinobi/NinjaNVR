import { describe, expect, test } from "bun:test";
import {
  BatchFaceProgress,
  EXPLORE_PRESETS_STORAGE_KEY,
  REVIEW_PRESETS_STORAGE_KEY,
  applyEventSubLabelUpdate,
  applyReviewSubLabelUpdate,
  clearFaceProfileFromEvents,
  clearFaceProfileFromReviews,
  computeReviewPlaybackWindow,
  createForensicTriageController,
  matchesLicensePlateFilter,
  normalizeForensicLicensePlate,
  parseSavedExplorePresets,
  parseSavedReviewPresets,
  resolveNextBriefingStep,
  resolveReviewThumbUrl,
  shouldApplyEventMutationResult,
  shouldApplyReviewMutationResult,
} from "../utils/forensicTriage";
import type { EventItem, ReviewItem } from "../types/nvr";

interface RecordedFetchCall {
  url: string;
  method: string;
  body: Record<string, unknown> | null;
}

function createInMemoryStorage(
  initial: Record<string, string> = {}
): Pick<Storage, "getItem" | "setItem"> & { dump: () => Record<string, string> } {
  const map = new Map<string, string>(Object.entries(initial));
  return {
    getItem(key: string): string | null {
      return map.has(key) ? (map.get(key) ?? null) : null;
    },
    setItem(key: string, value: string): void {
      map.set(key, String(value));
    },
    dump(): Record<string, string> {
      return Object.fromEntries(map.entries());
    },
  };
}

function jsonResponse(
  status: number,
  payload: unknown
): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

describe("ForensicTriageModule — createForensicTriageController", () => {
  test("fetchEnrolledFaces deduplicates profiles case-insensitively and filters out excludeDeletedProfile", async () => {
    const calls: string[] = [];
    const fetchImpl: typeof fetch = (async (input) => {
      const url = typeof input === "string" ? input : input.toString();
      calls.push(url);
      return jsonResponse(200, {
        profiles: [
          {
            name: " Bruno ",
            sample_count: 3,
            sample_files: ["1.jpg", "2.jpg", "3.jpg"],
            updated_at: 1700000100,
          },
          {
            name: "bruno",
            sample_count: 1,
            sample_files: ["dup.jpg"],
          },
          {
            name: "Alice",
            sample_count: 2,
            sample_files: ["a1.jpg", "a2.jpg"],
          },
          {
            name: "Eve",
            sample_count: 1,
            sample_files: ["e1.jpg"],
          },
        ],
      });
    }) as typeof fetch;

    const controller = createForensicTriageController({ fetchImpl });
    const faces = await controller.fetchEnrolledFaces({
      excludeDeletedProfile: { name: "ALICE" },
    });

    expect(calls).toEqual(["/api/faces"]);
    expect(faces.map((f) => f.name)).toEqual(["Bruno", "Eve"]);
    expect(faces[0].sample_count).toBe(3);
  });

  test("assignFaceSubLabel enrolls directly and falls back to /api/events/{id}/sub_label when /api/faces/{name}/enroll returns 422", async () => {
    const calls: RecordedFetchCall[] = [];
    let failEnroll = false;

    const fetchImpl: typeof fetch = (async (input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const method = init?.method || "GET";
      const body =
        typeof init?.body === "string"
          ? (JSON.parse(init.body) as Record<string, unknown>)
          : null;
      calls.push({ url, method, body });

      if (url.startsWith("/api/faces/") && failEnroll) {
        return jsonResponse(422, {
          message: "No detectable face crop in event snapshot",
        });
      }
      if (url.startsWith("/api/faces/")) {
        return jsonResponse(200, {
          sample_count: 4,
          backfilled_events: 2,
        });
      }
      if (url.endsWith("/sub_label")) {
        return jsonResponse(200, {
          status: "ok",
          sub_label: body?.sub_label ?? null,
        });
      }
      return jsonResponse(404, {});
    }) as typeof fetch;

    const controller = createForensicTriageController({ fetchImpl });

    // 1. Direct gallery enrollment succeeds
    const directRes = await controller.assignFaceSubLabel({
      primaryEventId: "ev_101",
      rawName: "  Bruno Silva ",
    });
    expect(directRes.ok).toBe(true);
    expect(directRes.cleanName).toBe("Bruno Silva");
    expect(directRes.usedFallbackSubLabel).toBe(false);
    expect(directRes.sampleCount).toBe(4);
    expect(directRes.backfilledEvents).toBe(2);
    expect(directRes.shouldApply).toBe(true);
    expect(calls.length).toBe(1);
    expect(calls[0]).toEqual({
      url: "/api/faces/Bruno%20Silva/enroll",
      method: "POST",
      body: {
        event_id: "ev_101",
        assign_sub_label: true,
        backfill_events: true,
      },
    });

    // 2. Fallback to /api/events/{id}/sub_label when /api/faces/{name}/enroll returns 422
    calls.length = 0;
    failEnroll = true;
    const fallbackRes = await controller.assignFaceSubLabel({
      primaryEventId: "ev_202",
      rawName: "Erik",
    });
    expect(fallbackRes.ok).toBe(true);
    expect(fallbackRes.cleanName).toBe("Erik");
    expect(fallbackRes.usedFallbackSubLabel).toBe(true);
    expect(fallbackRes.shouldApply).toBe(true);
    expect(calls).toEqual([
      {
        url: "/api/faces/Erik/enroll",
        method: "POST",
        body: {
          event_id: "ev_202",
          assign_sub_label: true,
          backfill_events: true,
        },
      },
      {
        url: "/api/events/ev_202/sub_label",
        method: "POST",
        body: {
          sub_label: "Erik",
          sub_label_score: 1.0,
          enroll_face: true,
        },
      },
    ]);

    // 3. Rejects invalid / path-traversal names before issuing network calls
    calls.length = 0;
    const invalidRes = await controller.assignFaceSubLabel({
      primaryEventId: "ev_202",
      rawName: "../etc/passwd",
    });
    expect(invalidRes.ok).toBe(false);
    expect(invalidRes.invalidName).toBe(true);
    expect(calls.length).toBe(0);
  });

  test("batchAssignFaceSubLabel sends backfill_events: false on intermediate items, true on the final item, and emits progress transitions", async () => {
    const calls: RecordedFetchCall[] = [];
    const progressLog: Array<BatchFaceProgress | null> = [];
    const itemSuccessLog: Array<{ id: string; name: string | null }> = [];

    const fetchImpl: typeof fetch = (async (input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const method = init?.method || "GET";
      const body =
        typeof init?.body === "string"
          ? (JSON.parse(init.body) as Record<string, unknown>)
          : null;
      calls.push({ url, method, body });
      const isLast = Boolean(body?.backfill_events);
      return jsonResponse(200, {
        sample_count: calls.length,
        backfilled_events: isLast ? 5 : 0,
      });
    }) as typeof fetch;

    const controller = createForensicTriageController({ fetchImpl });
    const result = await controller.batchAssignFaceSubLabel({
      items: [
        { id: "rev_1", primaryEventId: "det_1" },
        { id: "rev_2", primaryEventId: "det_2" },
        { id: "rev_3", primaryEventId: "det_3" },
      ],
      rawName: "Erik",
      onProgress: (p) => progressLog.push(p ? { ...p } : null),
      onItemSuccess: (id, name) => itemSuccessLog.push({ id, name }),
    });

    expect(result).toEqual({
      ok: true,
      invalidName: false,
      cleanName: "Erik",
      total: 3,
      succeededIds: ["rev_1", "rev_2", "rev_3"],
      failedIds: [],
      totalBackfilled: 5,
    });

    expect(calls).toEqual([
      {
        url: "/api/faces/Erik/enroll",
        method: "POST",
        body: {
          event_id: "det_1",
          assign_sub_label: true,
          backfill_events: false,
        },
      },
      {
        url: "/api/faces/Erik/enroll",
        method: "POST",
        body: {
          event_id: "det_2",
          assign_sub_label: true,
          backfill_events: false,
        },
      },
      {
        url: "/api/faces/Erik/enroll",
        method: "POST",
        body: {
          event_id: "det_3",
          assign_sub_label: true,
          backfill_events: true,
        },
      },
    ]);

    expect(progressLog).toEqual([
      { active: true, completed: 0, total: 3, name: "Erik" },
      { active: true, completed: 1, total: 3, name: "Erik" },
      { active: true, completed: 2, total: 3, name: "Erik" },
      { active: true, completed: 3, total: 3, name: "Erik" },
      null,
    ]);

    expect(itemSuccessLog).toEqual([
      { id: "rev_1", name: "Erik" },
      { id: "rev_2", name: "Erik" },
      { id: "rev_3", name: "Erik" },
    ]);
  });

  test("assignFaceSubLabel clears sub_label across all detectionEventIds when rawName is null or empty", async () => {
    const calls: RecordedFetchCall[] = [];
    const fetchImpl: typeof fetch = (async (input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const method = init?.method || "GET";
      const body =
        typeof init?.body === "string"
          ? (JSON.parse(init.body) as Record<string, unknown>)
          : null;
      calls.push({ url, method, body });
      return jsonResponse(200, { status: "cleared" });
    }) as typeof fetch;

    const controller = createForensicTriageController({ fetchImpl });

    // Clear with rawName: null across multiple detectionEventIds of a review segment
    const resNull = await controller.assignFaceSubLabel({
      primaryEventId: "det_a",
      detectionEventIds: ["det_a", "det_b", "det_c", "det_a"],
      rawName: null,
      targetKind: "review",
      targetId: "rev_10",
    });

    expect(resNull.ok).toBe(true);
    expect(resNull.cleanName).toBeNull();
    expect(resNull.clearedEventIds).toEqual(["det_a", "det_b", "det_c"]);
    expect(calls).toEqual([
      {
        url: "/api/events/det_a/sub_label",
        method: "POST",
        body: { sub_label: null },
      },
      {
        url: "/api/events/det_b/sub_label",
        method: "POST",
        body: { sub_label: null },
      },
      {
        url: "/api/events/det_c/sub_label",
        method: "POST",
        body: { sub_label: null },
      },
    ]);

    // Clear with rawName: ""
    calls.length = 0;
    const resEmpty = await controller.assignFaceSubLabel({
      primaryEventId: "det_single",
      rawName: "   ",
    });
    expect(resEmpty.ok).toBe(true);
    expect(resEmpty.cleanName).toBeNull();
    expect(resEmpty.clearedEventIds).toEqual(["det_single"]);
    expect(calls).toEqual([
      {
        url: "/api/events/det_single/sub_label",
        method: "POST",
        body: { sub_label: null },
      },
    ]);

    // Batch clear across multiple review segments with multi-detection arrays
    calls.length = 0;
    const clearedItemIds: string[] = [];
    const batchClearRes = await controller.batchAssignFaceSubLabel({
      items: [
        {
          id: "rev_1",
          primaryEventId: "det_1a",
          detectionEventIds: ["det_1a", "det_1b"],
        },
        {
          id: "rev_2",
          primaryEventId: "det_2a",
          detectionEventIds: ["det_2a"],
        },
      ],
      rawName: null,
      onItemSuccess: (id, name) => {
        if (name === null) clearedItemIds.push(id);
      },
    });
    expect(batchClearRes.ok).toBe(true);
    expect(batchClearRes.cleanName).toBeNull();
    expect(batchClearRes.succeededIds).toEqual(["rev_1", "rev_2"]);
    expect(clearedItemIds).toEqual(["rev_1", "rev_2"]);
    expect(calls.map((c) => c.url)).toEqual([
      "/api/events/det_1a/sub_label",
      "/api/events/det_1b/sub_label",
      "/api/events/det_2a/sub_label",
    ]);
  });

  test("sequence-guarded saveLicensePlate and saveForensicNote reject stale in-flight responses when selection changes", async () => {
    const pendingResolvers: Array<() => void> = [];
    const calls: RecordedFetchCall[] = [];

    const fetchImpl: typeof fetch = ((input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const method = init?.method || "GET";
      const body =
        typeof init?.body === "string"
          ? (JSON.parse(init.body) as Record<string, unknown>)
          : null;
      calls.push({ url, method, body });
      return new Promise<Response>((resolve) => {
        pendingResolvers.push(() => {
          resolve(
            jsonResponse(200, {
              status: "ok",
              ...(body || {}),
            })
          );
        });
      });
    }) as typeof fetch;

    const controller = createForensicTriageController({ fetchImpl });

    // Select event ev_1 and start saving license plate
    let activeEventId = "ev_1";
    controller.setActiveTarget("event", activeEventId);

    const platePromise1 = controller.saveLicensePlate({
      eventId: "ev_1",
      rawPlate: "abc-1d23",
      targetKind: "event",
      targetId: "ev_1",
      getActiveTargetId: () => activeEventId,
    });

    // Operator switches selected event to ev_2 before ev_1's request resolves
    activeEventId = "ev_2";
    controller.setActiveTarget("event", activeEventId);

    const platePromise2 = controller.saveLicensePlate({
      eventId: "ev_2",
      rawPlate: "xyz-9876",
      targetKind: "event",
      targetId: "ev_2",
      getActiveTargetId: () => activeEventId,
    });

    // Resolve both in-flight requests
    expect(pendingResolvers.length).toBe(2);
    pendingResolvers[0]();
    pendingResolvers[1]();

    const [plateRes1, plateRes2] = await Promise.all([
      platePromise1,
      platePromise2,
    ]);

    expect(plateRes1.ok).toBe(true);
    expect(plateRes1.normalizedPlate).toBe("ABC1D23");
    expect(plateRes1.stale).toBe(true);
    expect(plateRes1.shouldApply).toBe(false);

    expect(plateRes2.ok).toBe(true);
    expect(plateRes2.normalizedPlate).toBe("XYZ9876");
    expect(plateRes2.stale).toBe(false);
    expect(plateRes2.shouldApply).toBe(true);

    // Now test sequence-guarded forensic note mutation on review segments
    pendingResolvers.length = 0;
    calls.length = 0;
    let activeReviewId = "rev_100";
    controller.setActiveTarget("review", activeReviewId);

    const notePromise1 = controller.saveForensicNote({
      targetKind: "review",
      targetId: "rev_100",
      notes: "Stale note for rev_100",
      getActiveTargetId: () => activeReviewId,
    });

    // Switch active review while notePromise1 is in flight
    activeReviewId = "rev_200";
    controller.setActiveTarget("review", activeReviewId);

    pendingResolvers[0]();
    const noteRes1 = await notePromise1;

    expect(calls[0]).toEqual({
      url: "/api/review/rev_100/notes",
      method: "PUT",
      body: { notes: "Stale note for rev_100" },
    });
    expect(noteRes1.ok).toBe(true);
    expect(noteRes1.stale).toBe(true);
    expect(noteRes1.shouldApply).toBe(false);

    // Saving note on the currently active review succeeds with shouldApply: true
    const notePromise2 = controller.saveForensicNote({
      targetKind: "review",
      targetId: "rev_200",
      notes: "Fresh note for rev_200",
      getActiveTargetId: () => activeReviewId,
    });
    // Calling setActiveTarget idempotently with the SAME active target ID must NOT invalidate notePromise2
    controller.setActiveTarget("review", "rev_200");
    // Concurrent plate save on the SAME active review must NOT invalidate notePromise2
    const plateOnSameReview = controller.saveLicensePlate({
      eventId: "det_200",
      rawPlate: "BRA2E19",
      targetKind: "review",
      targetId: "rev_200",
      getActiveTargetId: () => activeReviewId,
    });
    pendingResolvers[1]();
    pendingResolvers[2]();
    const [noteRes2, concurrentPlateRes] = await Promise.all([
      notePromise2,
      plateOnSameReview,
    ]);
    expect(noteRes2.ok).toBe(true);
    expect(noteRes2.stale).toBe(false);
    expect(noteRes2.shouldApply).toBe(true);
    expect(noteRes2.notes).toBe("Fresh note for rev_200");
    expect(concurrentPlateRes.ok).toBe(true);
    expect(concurrentPlateRes.stale).toBe(false);
    expect(concurrentPlateRes.shouldApply).toBe(true);
    expect(concurrentPlateRes.normalizedPlate).toBe("BRA2E19");
  });

  test("loadFilterPresets, saveFilterPreset, and deleteFilterPreset round-trip and recover from corrupted storage", () => {
    const storage = createInMemoryStorage({
      [REVIEW_PRESETS_STORAGE_KEY]: "{corrupted json",
      [EXPLORE_PRESETS_STORAGE_KEY]: '{"not":"an array"}',
    });
    const controller = createForensicTriageController({ storage });

    // Corrupted JSON recovers cleanly to empty array
    expect(controller.loadFilterPresets("review")).toEqual([]);
    expect(controller.loadFilterPresets("explore")).toEqual([]);

    // Save review preset with explicit name and default fallback name
    let revPresets = controller.saveFilterPreset("review", [], "Night Gate", {
      camera: "gate_cam",
      severity: "alert",
      object: "person",
      excludeObject: "cat",
      zone: "driveway",
      status: "unreviewed",
    });
    expect(revPresets).toEqual([
      {
        name: "Night Gate",
        camera: "gate_cam",
        severity: "alert",
        object: "person",
        excludeObject: "cat",
        zone: "driveway",
        status: "unreviewed",
      },
    ]);

    revPresets = controller.saveFilterPreset("review", revPresets, "   ", {
      camera: "all",
      severity: "alert",
      object: "car",
      excludeObject: "none",
      zone: "all",
      status: "all",
    });
    expect(revPresets.map((p) => p.name)).toEqual(["Night Gate", "ALL · ALERT"]);
    expect(controller.loadFilterPresets("review")).toEqual(revPresets);

    // Delete review preset
    revPresets = controller.deleteFilterPreset(
      "review",
      revPresets,
      "Night Gate"
    );
    expect(revPresets.map((p) => p.name)).toEqual(["ALL · ALERT"]);
    expect(controller.loadFilterPresets("review")).toEqual(revPresets);

    // Save & delete explore preset round-trip
    let expPresets = controller.saveFilterPreset(
      "explore",
      [],
      "Moving Vehicles",
      {
        camera: "street_cam",
        label: "car",
        excludeLabel: "person",
        zone: "lane_1",
        minScore: 80,
        stationary: "moving",
        retainedOnly: true,
      }
    );
    expect(controller.loadFilterPresets("explore")).toEqual(expPresets);

    expPresets = controller.deleteFilterPreset(
      "explore",
      expPresets,
      "Moving Vehicles"
    );
    expect(expPresets).toEqual([]);
    expect(controller.loadFilterPresets("explore")).toEqual([]);
  });

  test("pure domain helpers transform events/reviews immutably and match license plates across both item shapes", () => {
    const events: EventItem[] = [
      {
        id: "ev_1",
        camera: "front",
        label: "person",
        sub_label: "Bruno",
        sub_label_score: 0.94,
        zones: ["porch"],
        start_time: 1000,
        end_time: 1010,
        top_score: 0.9,
        score: 0.9,
        has_clip: true,
        has_snapshot: true,
        retain_indefinitely: false,
        false_positive: false,
        box: null,
        data: {
          sub_label: "Bruno",
          sub_label_score: 0.94,
          recognized_license_plate: "ABC1D23",
        },
      },
    ];

    const reviews: ReviewItem[] = [
      {
        id: "rev_1",
        camera: "front",
        start_time: 1000,
        end_time: 1020,
        severity: "alert",
        thumb_path: "/media/ninjanvr/clips/review/thumb-front.webp",
        has_been_reviewed: false,
        data: {
          objects: ["person", "car"],
          sub_labels: ["Bruno"],
          sub_label_score: 0.95,
          recognized_license_plates: ["ABC1D23"],
        },
      },
    ];

    expect(normalizeForensicLicensePlate(" abc-1d23 ")).toBe("ABC1D23");
    expect(matchesLicensePlateFilter(events[0], "abc-1d")).toBe(true);
    expect(matchesLicensePlateFilter(reviews[0], "abc-1d")).toBe(true);
    expect(matchesLicensePlateFilter(reviews[0], "bruno")).toBe(true);
    expect(matchesLicensePlateFilter(reviews[0], "xyz999")).toBe(false);

    const clearedEvents = clearFaceProfileFromEvents(events, "bruno");
    expect(clearedEvents[0].sub_label).toBeNull();
    expect(clearedEvents[0].data?.sub_label).toBeNull();

    const clearedReviews = clearFaceProfileFromReviews(reviews, "BRUNO");
    expect(clearedReviews[0].data.sub_labels).toEqual([]);
    expect(clearedReviews[0].data.sub_label_score).toBeNull();

    const reassignedEvents = applyEventSubLabelUpdate(
      clearedEvents,
      "ev_1",
      "Erik",
      1.0
    );
    expect(reassignedEvents[0].sub_label).toBe("Erik");
    expect(reassignedEvents[0].sub_label_score).toBe(1.0);

    const reassignedReviews = applyReviewSubLabelUpdate(
      clearedReviews,
      "rev_1",
      "Erik"
    );
    expect(reassignedReviews[0].data.sub_labels).toEqual(["Erik"]);

    expect(shouldApplyEventMutationResult("ev_1", "ev_1", true)).toBe(true);
    expect(shouldApplyReviewMutationResult("rev_1", "rev_2", true)).toBe(false);
    expect(parseSavedReviewPresets("invalid")).toEqual([]);
    expect(parseSavedExplorePresets("invalid")).toEqual([]);
    expect(computeReviewPlaybackWindow(100, 120, 4, 6, 20)).toEqual({
      startSec: 96,
      endSec: 126,
    });
    expect(resolveReviewThumbUrl(reviews[0])).toBe(
      "/clips/review/thumb-front.webp"
    );
    expect(resolveNextBriefingStep(["rev_1"], -1, reviews)).toEqual({
      nextIdx: 0,
      nextReview: reviews[0],
    });
  });

  test("toggleFalsePositive, verifyWithAi, addEventToDataset (with fallback auto-annotate), and fetchEnrolledFaceNames enforce sequence freshness", async () => {
    const calls: RecordedFetchCall[] = [];
    let pendingVerifyResolver: (() => void) | null = null;
    let triggerFallbackAutoAnnotate = false;

    const fetchImpl: typeof fetch = (async (input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const method = init?.method || "GET";
      const body =
        typeof init?.body === "string"
          ? (JSON.parse(init.body) as Record<string, unknown>)
          : null;
      calls.push({ url, method, body });

      if (url === "/api/faces") {
        return jsonResponse(200, {
          faces: {
            "Zoe ": ["z1.jpg"],
            " alice": ["a1.jpg", "a2.jpg"],
            Alice: ["dup.jpg"],
            "  ": ["blank.jpg"],
          },
        });
      }

      if (url.endsWith("/false_positive") && method === "PUT") {
        if (url.includes("ev_fp_fallback")) {
          return jsonResponse(404, { error: "Not found" });
        }
        return jsonResponse(200, {
          status: "ok",
          false_positive: Boolean(body?.false_positive),
        });
      }

      if (url.endsWith("/false-positive") && method === "POST") {
        return jsonResponse(200, {
          status: "ok",
          false_positive: Boolean(body?.false_positive),
        });
      }

      if (url.endsWith("/verify-ai")) {
        if (url.includes("ev_stale")) {
          return new Promise<Response>((resolve) => {
            pendingVerifyResolver = () =>
              resolve(
                jsonResponse(200, {
                  verified: true,
                  verified_by: "gemini-2.5-flash",
                  confidence: 0.96,
                  reason: "Person clearly standing on porch",
                })
              );
          });
        }
        return jsonResponse(200, {
          is_true_positive: true,
          provider: "openai",
          model: "gpt-4o",
          confidence: 0.91,
          reason: "Confirmed delivery driver",
        });
      }

      if (url.startsWith("/api/dataset/samples/from-event")) {
        if (triggerFallbackAutoAnnotate) {
          return jsonResponse(200, {
            id: "ds_ev_fallback",
            sample_kind: "true_positive",
            split: "train",
            auto_annotate_attempted: false,
            annotations: [],
          });
        }
        return jsonResponse(200, {
          id: "ds_ev_direct",
          sample_kind: "ai_annotated",
          split: "val",
          annotations: [
            { class_id: 0, label: "person", cx: 0.5, cy: 0.5, w: 0.2, h: 0.4 },
            { class_id: 2, label: "car", cx: 0.2, cy: 0.7, w: 0.3, h: 0.2 },
          ],
        });
      }

      if (url === "/api/dataset/samples/ds_ev_fallback/auto-annotate") {
        return jsonResponse(200, {
          id: "ds_ev_fallback",
          sample_kind: "ai_annotated",
          split: "train",
          annotations: [
            { class_id: 0, label: "person", cx: 0.4, cy: 0.5, w: 0.2, h: 0.5 },
            { class_id: 1, label: "face", cx: 0.4, cy: 0.3, w: 0.08, h: 0.1 },
            { class_id: 11, label: "package", cx: 0.45, cy: 0.75, w: 0.1, h: 0.1 },
          ],
        });
      }

      return jsonResponse(404, { error: "Not found" });
    }) as typeof fetch;

    const controller = createForensicTriageController({ fetchImpl });

    // 1. fetchEnrolledFaceNames returns sorted unique non-empty names
    const names = await controller.fetchEnrolledFaceNames();
    expect(names).toEqual(["alice", "Zoe"]);

    // 2. toggleFalsePositive tries canonical PUT /false_positive first and falls back to POST /false-positive on 404/405
    controller.setActiveTarget("event", "ev_fp_1");
    const fpRes = await controller.toggleFalsePositive("ev_fp_1", false);
    expect(fpRes).toEqual({
      stale: false,
      shouldApply: true,
      ok: true,
      nextFp: true,
    });
    expect(calls.find((c) => c.url === "/api/events/ev_fp_1/false_positive")).toEqual({
      url: "/api/events/ev_fp_1/false_positive",
      method: "PUT",
      body: { false_positive: true },
    });

    controller.setActiveTarget("event", "ev_fp_fallback");
    const fpFallbackRes = await controller.toggleFalsePositive(
      "ev_fp_fallback",
      true
    );
    expect(fpFallbackRes).toEqual({
      stale: false,
      shouldApply: true,
      ok: true,
      nextFp: false,
    });
    expect(
      calls.find((c) => c.url === "/api/events/ev_fp_fallback/false_positive")
    ).toEqual({
      url: "/api/events/ev_fp_fallback/false_positive",
      method: "PUT",
      body: { false_positive: false },
    });
    expect(
      calls.find((c) => c.url === "/api/events/ev_fp_fallback/false-positive")
    ).toEqual({
      url: "/api/events/ev_fp_fallback/false-positive",
      method: "POST",
      body: { false_positive: false },
    });

    // 3. verifyWithAi marks stale when active event changes while request is in flight
    controller.setActiveTarget("event", "ev_stale");
    const staleVerifyPromise = controller.verifyWithAi("ev_stale");
    controller.setActiveTarget("event", "ev_fresh");
    const freshVerifyRes = await controller.verifyWithAi("ev_fresh");

    expect(freshVerifyRes.ok).toBe(true);
    expect(freshVerifyRes.stale).toBe(false);
    expect(freshVerifyRes.shouldApply).toBe(true);
    expect(freshVerifyRes.verified).toBe(true);
    expect(freshVerifyRes.verifiedBy).toBe("openai");
    expect(freshVerifyRes.reason).toBe("Confirmed delivery driver");

    expect(pendingVerifyResolver).not.toBeNull();
    pendingVerifyResolver!();
    const staleVerifyRes = await staleVerifyPromise;
    expect(staleVerifyRes.ok).toBe(true);
    expect(staleVerifyRes.stale).toBe(true);
    expect(staleVerifyRes.shouldApply).toBe(false);
    expect(staleVerifyRes.verified).toBe(true);
    expect(staleVerifyRes.verifiedBy).toBe("gemini-2.5-flash");

    // 4. addEventToDataset: direct AI annotation vs fallback /auto-annotate
    controller.setActiveTarget("event", "ev_direct");
    const dsDirect = await controller.addEventToDataset({
      eventId: "ev_direct",
      camera: "front_door",
      label: "person",
      timestamp: 1700000100,
      autoAnnotate: true,
    });
    expect(dsDirect).toEqual({
      stale: false,
      shouldApply: true,
      ok: true,
      id: "ds_ev_direct",
      split: "val",
      autoAnnotated: true,
      annotationCount: 2,
    });

    triggerFallbackAutoAnnotate = true;
    controller.setActiveTarget("event", "ev_fallback");
    const dsFallback = await controller.addEventToDataset({
      eventId: "ev_fallback",
      camera: "front_door",
      label: "person",
      autoAnnotate: true,
    });
    expect(dsFallback).toEqual({
      stale: false,
      shouldApply: true,
      ok: true,
      id: "ds_ev_fallback",
      split: "train",
      autoAnnotated: true,
      annotationCount: 3,
    });
    expect(
      calls.some(
        (c) =>
          c.url === "/api/dataset/samples/ds_ev_fallback/auto-annotate" &&
          c.method === "POST"
      )
    ).toBe(true);
  });

  test("batchAssignFaceSubLabel falls back to /api/events/{id}/sub_label on 422 and guarantees backfill_events: true runs when final item falls back", async () => {
    const calls: RecordedFetchCall[] = [];
    const itemSuccessLog: Array<{ id: string; name: string | null }> = [];

    const fetchImpl: typeof fetch = (async (input, init) => {
      const url = typeof input === "string" ? input : input.toString();
      const method = init?.method || "GET";
      const body =
        typeof init?.body === "string"
          ? (JSON.parse(init.body) as Record<string, unknown>)
          : null;
      calls.push({ url, method, body });

      if (url === "/api/faces/Erik/enroll") {
        // Simulate det_2 (the final item in the batch) having no detectable face crop (422)
        if (body?.event_id === "det_2") {
          return jsonResponse(422, {
            message: "No detectable face crop in event snapshot",
          });
        }
        return jsonResponse(200, {
          sample_count: 2,
          backfilled_events: body?.backfill_events ? 4 : 0,
        });
      }

      if (url === "/api/events/det_2/sub_label") {
        return jsonResponse(200, {
          status: "ok",
          sub_label: body?.sub_label ?? null,
        });
      }

      return jsonResponse(404, {});
    }) as typeof fetch;

    const controller = createForensicTriageController({ fetchImpl });
    const res = await controller.batchAssignFaceSubLabel({
      items: [
        { id: "rev_1", primaryEventId: "det_1" },
        { id: "rev_2", primaryEventId: "det_2" },
      ],
      rawName: "Erik",
      onItemSuccess: (id, name) => itemSuccessLog.push({ id, name }),
    });

    expect(res).toEqual({
      ok: true,
      invalidName: false,
      cleanName: "Erik",
      total: 2,
      succeededIds: ["rev_1", "rev_2"],
      failedIds: [],
      totalBackfilled: 4,
    });
    expect(itemSuccessLog).toEqual([
      { id: "rev_1", name: "Erik" },
      { id: "rev_2", name: "Erik" },
    ]);

    // 1. det_1 enrolled with backfill_events: false
    // 2. det_2 attempted /enroll with backfill_events: true -> 422 -> fell back to /api/events/det_2/sub_label
    // 3. Because det_1 succeeded /enroll without backfill_events: true and det_2's /enroll failed, backfill_events: true was triggered on det_1
    expect(calls).toEqual([
      {
        url: "/api/faces/Erik/enroll",
        method: "POST",
        body: {
          event_id: "det_1",
          assign_sub_label: true,
          backfill_events: false,
        },
      },
      {
        url: "/api/faces/Erik/enroll",
        method: "POST",
        body: {
          event_id: "det_2",
          assign_sub_label: true,
          backfill_events: true,
        },
      },
      {
        url: "/api/events/det_2/sub_label",
        method: "POST",
        body: {
          sub_label: "Erik",
          sub_label_score: 1.0,
          enroll_face: true,
        },
      },
      {
        url: "/api/faces/Erik/enroll",
        method: "POST",
        body: {
          event_id: "det_1",
          assign_sub_label: true,
          backfill_events: true,
        },
      },
    ]);
  });
});
