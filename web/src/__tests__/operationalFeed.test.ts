import { describe, expect, test } from "bun:test";
import type {
  DatasetSampleItem,
  EnrolledFaceProfileItem,
  EventItem,
  ReviewItem,
} from "../types/nvr";
import { upsertDatasetSampleInList } from "../utils/datasetStudio";
import {
  applyFencedSampleMutationOutcome,
  buildEventsFeedUrl,
  buildReviewFeedUrl,
  createSampleMutationEpochTracker,
  matchesOperationalDateBounds,
  parseOperationalDateBoundsSec,
  reconcileDeletedFaceProfile,
  reconcileEventMutation,
  reconcileReviewMutation,
} from "../utils/operationalFeed";

function makeEvent(overrides: Partial<EventItem> = {}): EventItem {
  return {
    id: "ev_1",
    camera: "escritorio",
    label: "car",
    sub_label: null,
    zones: ["driveway"],
    start_time: 1_700_000_100,
    end_time: 1_700_000_115,
    top_score: 0.91,
    score: 0.88,
    has_clip: true,
    has_snapshot: true,
    retain_indefinitely: false,
    false_positive: false,
    box: [10, 20, 200, 180],
    data: {},
    ...overrides,
  };
}

function makeReview(overrides: Partial<ReviewItem> = {}): ReviewItem {
  return {
    id: "rev_1",
    camera: "escritorio",
    start_time: 1_700_000_100,
    end_time: 1_700_000_125,
    severity: "alert",
    thumb_path: "/clips/rev_1.webp",
    has_been_reviewed: false,
    data: {
      objects: ["person", "car"],
      detections: ["ev_1"],
    },
    ...overrides,
  };
}

function makeSample(overrides: Partial<DatasetSampleItem> = {}): DatasetSampleItem {
  return {
    id: "smp_1",
    camera: "escritorio",
    split: "train",
    sample_kind: "true_positive",
    image_path: "/dataset/smp_1.jpg",
    width: 1920,
    height: 1080,
    annotations: [],
    verified: false,
    night_mode: false,
    created_at: 1_700_000_000,
    verified_by: null,
    ...overrides,
  };
}

describe("operationalFeed — reviewDateFilter preservation, collection reconciliation & epoch fencing", () => {
  test("buildReviewFeedUrl and buildEventsFeedUrl preserve reviewDateFilter across 5-second tab refreshes instead of clobbering to limit=80", () => {
    expect(buildReviewFeedUrl("")).toBe("/api/review?limit=200");
    expect(buildEventsFeedUrl("")).toBe("/api/events?limit=120");

    // Date-filtered review refresh retains exact day bounds and limit=300
    const expectedStart = Math.floor(
      new Date(2026, 10, 14, 0, 0, 0).getTime() / 1000
    );
    const expectedEnd = expectedStart + 86400;
    expect(buildReviewFeedUrl("2026-11-14")).toBe(
      `/api/review?after=${expectedStart}&before=${expectedEnd}&limit=300`
    );
    expect(buildEventsFeedUrl("2026-11-14")).toBe(
      `/api/events?after=${expectedStart}&before=${expectedEnd}&limit=250`
    );

    // Malformed or calendar-rollover dates fall back cleanly to unfiltered feed URL
    expect(buildReviewFeedUrl("2026-02-31")).toBe("/api/review?limit=200");
    expect(buildReviewFeedUrl("2026-13-01")).toBe("/api/review?limit=200");
    expect(buildEventsFeedUrl("not-a-date")).toBe("/api/events?limit=120");
  });

  test("reconcileEventMutation propagates plate, sub-label, note, AI Judge verdict & false-positive updates to both master list and selectedEvent", () => {
    const ev1 = makeEvent({ id: "ev_1" });
    const ev2 = makeEvent({ id: "ev_2", label: "person" });

    // 1. License plate + sub-label + note on selected event updates both master list and detail view
    const afterPlateAndNote = reconcileEventMutation(
      { events: [ev1, ev2], selectedEvent: ev1 },
      "ev_1",
      {
        recognizedLicensePlate: "BRA2E19",
        subLabel: "Fleet Van",
        subLabelScore: 0.99,
        notes: "Authorized delivery vehicle",
      }
    );
    expect(afterPlateAndNote.events[0].sub_label).toBe("Fleet Van");
    expect(afterPlateAndNote.events[0].data?.recognized_license_plate).toBe(
      "BRA2E19"
    );
    expect(afterPlateAndNote.events[0].data?.notes).toBe(
      "Authorized delivery vehicle"
    );
    expect(afterPlateAndNote.selectedEvent?.sub_label).toBe("Fleet Van");
    expect(afterPlateAndNote.selectedEvent?.data?.recognized_license_plate).toBe(
      "BRA2E19"
    );
    expect(afterPlateAndNote.selectedEvent?.data?.notes).toBe(
      "Authorized delivery vehicle"
    );
    // Unrelated event is untouched
    expect(afterPlateAndNote.events[1]).toBe(ev2);

    // 2. AI Judge verdict + false-positive update on ev_1 while operator is inspecting ev_2 updates master list without clobbering selectedEvent (ev_2)
    const verdict = {
      is_true_positive: false,
      confidence: 0.94,
      reason: "Tree shadow oscillation",
    };
    const afterAiJudge = reconcileEventMutation(
      { events: afterPlateAndNote.events, selectedEvent: ev2 },
      "ev_1",
      {
        aiVerification: verdict,
        falsePositive: true,
      }
    );
    expect(afterAiJudge.events[0].false_positive).toBe(true);
    expect(afterAiJudge.events[0].data?.ai_verification).toEqual(verdict);
    expect(afterAiJudge.selectedEvent).toBe(ev2);
  });

  test("reconcileReviewMutation updates master list and activeReview while preserving incident start_time and end_time", () => {
    const rev1 = makeReview({
      id: "rev_1",
      start_time: 1_700_000_100,
      end_time: 1_700_000_125,
    });
    const rev2 = makeReview({ id: "rev_2" });

    const updated = reconcileReviewMutation(
      { reviews: [rev1, rev2], activeReview: rev1 },
      "rev_1",
      {
        subLabel: "Bruno",
        recognizedLicensePlate: "ABC1D23",
        notes: "Confirmed resident arrival",
        hasBeenReviewed: true,
      }
    );

    expect(updated.reviews[0].data.sub_labels).toEqual(["Bruno"]);
    expect(updated.reviews[0].data.recognized_license_plates).toEqual([
      "ABC1D23",
    ]);
    expect(updated.reviews[0].data.notes).toBe("Confirmed resident arrival");
    expect(updated.reviews[0].has_been_reviewed).toBe(true);
    expect(updated.reviews[0].start_time).toBe(1_700_000_100);
    expect(updated.reviews[0].end_time).toBe(1_700_000_125);

    expect(updated.activeReview?.data.sub_labels).toEqual(["Bruno"]);
    expect(updated.activeReview?.data.recognized_license_plates).toEqual([
      "ABC1D23",
    ]);
    expect(updated.activeReview?.data.notes).toBe("Confirmed resident arrival");
    expect(updated.activeReview?.has_been_reviewed).toBe(true);
    expect(updated.reviews[1]).toBe(rev2);
  });

  test("reconcileDeletedFaceProfile scrubs deleted profile case-insensitively across events, reviews, enrolledFaces & localSubLabels", () => {
    const evBruno = makeEvent({
      id: "ev_bruno",
      sub_label: "Bruno",
      sub_label_score: 0.96,
      data: { sub_label: "Bruno", sub_label_score: 0.96 },
    });
    const evAna = makeEvent({
      id: "ev_ana",
      sub_label: "Ana",
      sub_label_score: 0.92,
      data: { sub_label: "Ana", sub_label_score: 0.92 },
    });
    const revBrunoAndAna = makeReview({
      id: "rev_mixed",
      data: { sub_labels: ["bruno", "Ana"], sub_label_score: 0.95 },
    });
    const enrolledFaces: EnrolledFaceProfileItem[] = [
      { name: "Bruno", sample_count: 4 },
      { name: "Ana", sample_count: 2 },
    ];

    const res = reconcileDeletedFaceProfile(
      {
        events: [evBruno, evAna],
        selectedEvent: evBruno,
        reviews: [revBrunoAndAna],
        activeReview: revBrunoAndAna,
        enrolledFaces,
        localSubLabels: { ev_bruno: "BRUNO", ev_ana: "Ana" },
      },
      "bruno"
    );

    expect(res.clearedSelectedEvent).toBe(true);
    expect(res.clearedActiveReview).toBe(true);
    expect(res.events[0].sub_label).toBeNull();
    expect(res.selectedEvent?.sub_label).toBeNull();
    expect(res.events[1].sub_label).toBe("Ana");
    expect(res.reviews[0].data.sub_labels).toEqual(["Ana"]);
    expect(res.activeReview?.data.sub_labels).toEqual(["Ana"]);
    expect(res.enrolledFaces.map((f) => f.name)).toEqual(["Ana"]);
    expect(res.localSubLabels).toEqual({ ev_bruno: null, ev_ana: "Ana" });
  });

  test("per-sample mutation epoch fencing prevents stale PUT responses or rollbacks from overwriting newer edits", () => {
    const backingEpochs: Record<string, number> = {};
    const tracker = createSampleMutationEpochTracker(backingEpochs);

    const v0 = makeSample({ id: "smp_1", annotations: [] });
    const boxA = {
      class_id: 0,
      label: "person",
      cx: 0.5,
      cy: 0.5,
      w: 0.2,
      h: 0.4,
    };
    const boxB = {
      class_id: 2,
      label: "car",
      cx: 0.3,
      cy: 0.6,
      w: 0.25,
      h: 0.2,
    };

    // Edit #1 begins (epoch 1) -> optimistic [boxA]
    const epoch1 = tracker.begin("smp_1");
    const v1Optimistic = { ...v0, annotations: [boxA] };
    let list = upsertDatasetSampleInList([v0], v1Optimistic);

    // Edit #2 begins on same sample before PUT #1 settles (epoch 2) -> optimistic [boxA, boxB]
    const epoch2 = tracker.begin("smp_1");
    const v2Optimistic = { ...v1Optimistic, annotations: [boxA, boxB] };
    list = upsertDatasetSampleInList(list, v2Optimistic);

    expect(epoch1).toBe(1);
    expect(epoch2).toBe(2);
    expect(tracker.isCurrent("smp_1", epoch1)).toBe(false);
    expect(tracker.isCurrent("smp_1", epoch2)).toBe(true);

    // Stale PUT #1 fails (500) -> MUST NOT roll back list to v0!
    const staleFailure = applyFencedSampleMutationOutcome(list, {
      tracker,
      sampleId: "smp_1",
      epoch: epoch1,
      ok: false,
      rollbackItem: v0,
      upsert: upsertDatasetSampleInList,
    });
    expect(staleFailure.stale).toBe(true);
    expect(staleFailure.applied).toBe(false);
    expect(staleFailure.items[0].annotations).toEqual([boxA, boxB]);

    // Stale PUT #1 resolves 200 OK late -> MUST NOT overwrite [boxA, boxB] with [boxA]!
    const staleSuccess = applyFencedSampleMutationOutcome(list, {
      tracker,
      sampleId: "smp_1",
      epoch: epoch1,
      ok: true,
      serverItem: v1Optimistic,
      rollbackItem: v0,
      upsert: upsertDatasetSampleInList,
    });
    expect(staleSuccess.stale).toBe(true);
    expect(staleSuccess.applied).toBe(false);
    expect(staleSuccess.items[0].annotations).toEqual([boxA, boxB]);

    // Current PUT #2 resolves 200 OK -> applies cleanly
    const v2Server = { ...v2Optimistic, verified: true, verified_by: "manual" };
    const currentSuccess = applyFencedSampleMutationOutcome(list, {
      tracker,
      sampleId: "smp_1",
      epoch: epoch2,
      ok: true,
      serverItem: v2Server,
      rollbackItem: v1Optimistic,
      upsert: upsertDatasetSampleInList,
    });
    expect(currentSuccess.stale).toBe(false);
    expect(currentSuccess.applied).toBe(true);
    expect(currentSuccess.items[0].verified).toBe(true);
    expect(currentSuccess.items[0].annotations).toEqual([boxA, boxB]);

    // Edit #3 begins (epoch 3) and fails with no newer edit -> rolls back to v2Server cleanly
    const epoch3 = tracker.begin("smp_1");
    const v3Optimistic = { ...v2Server, annotations: [] };
    list = upsertDatasetSampleInList(currentSuccess.items, v3Optimistic);
    const currentRollback = applyFencedSampleMutationOutcome(list, {
      tracker,
      sampleId: "smp_1",
      epoch: epoch3,
      ok: false,
      rollbackItem: v2Server,
      upsert: upsertDatasetSampleInList,
    });
    expect(currentRollback.stale).toBe(false);
    expect(currentRollback.applied).toBe(true);
    expect(currentRollback.items[0].annotations).toEqual([boxA, boxB]);
  });

  test("matchesOperationalDateBounds filters out live /ws items from today when a historical reviewDateFilter is active", () => {
    const bounds = parseOperationalDateBoundsSec("2026-11-10");
    expect(bounds).not.toBeNull();

    const historicalRev = makeReview({
      id: "rev_hist",
      start_time: bounds!.dayStart + 3600,
      end_time: bounds!.dayStart + 3620,
    });
    const midnightSpanningRev = makeReview({
      id: "rev_span",
      start_time: bounds!.dayStart - 10,
      end_time: bounds!.dayStart + 15,
    });
    const liveTodayRev = makeReview({
      id: "rev_today_ws",
      start_time: bounds!.dayEnd + 86400 * 4,
      end_time: null,
    });
    const nextMidnightBoundaryEv = makeEvent({
      id: "ev_next_day",
      start_time: bounds!.dayEnd,
      end_time: bounds!.dayEnd + 5,
    });

    expect(matchesOperationalDateBounds(historicalRev, bounds)).toBe(true);
    expect(matchesOperationalDateBounds(midnightSpanningRev, bounds)).toBe(true);
    expect(matchesOperationalDateBounds(liveTodayRev, bounds)).toBe(false);
    expect(matchesOperationalDateBounds(nextMidnightBoundaryEv, bounds)).toBe(false);
    expect(matchesOperationalDateBounds(liveTodayRev, null)).toBe(true);
  });
});
