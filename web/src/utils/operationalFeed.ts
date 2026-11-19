import type {
  AiVerificationResult,
  EnrolledFaceProfileItem,
  EventItem,
  ReviewItem,
} from "../types/nvr";
import {
  applyEventSubLabelUpdate,
  applyReviewSubLabelUpdate,
  clearFaceProfileFromEvents,
  clearFaceProfileFromReviews,
} from "./forensicTriage";

export const DEFAULT_REVIEW_FEED_LIMIT = 200;
export const DEFAULT_DATE_FILTERED_REVIEW_FEED_LIMIT = 300;
export const DEFAULT_EVENTS_FEED_LIMIT = 120;
export const DEFAULT_DATE_FILTERED_EVENTS_FEED_LIMIT = 250;

export function parseOperationalDateBoundsSec(
  dateFilter: string | null | undefined
): { dayStart: number; dayEnd: number } | null {
  const clean = (dateFilter || "").trim();
  if (!clean) return null;
  const parts = clean.split("-");
  if (parts.length !== 3 || parts.some((p) => !/^\d+$/.test(p))) {
    return null;
  }
  const [year, month, day] = parts.map(Number);
  if (
    !Number.isFinite(year) ||
    !Number.isFinite(month) ||
    !Number.isFinite(day) ||
    month < 1 ||
    month > 12 ||
    day < 1 ||
    day > 31
  ) {
    return null;
  }
  const localMidnight = new Date(year, month - 1, day, 0, 0, 0);
  if (
    Number.isNaN(localMidnight.getTime()) ||
    localMidnight.getFullYear() !== year ||
    localMidnight.getMonth() !== month - 1 ||
    localMidnight.getDate() !== day
  ) {
    return null;
  }
  const dayStart = Math.floor(localMidnight.getTime() / 1000);
  if (!Number.isFinite(dayStart)) {
    return null;
  }
  return {
    dayStart,
    dayEnd: dayStart + 86400,
  };
}

export function matchesOperationalDateBounds(
  item: { start_time: number; end_time?: number | null },
  bounds: { dayStart: number; dayEnd: number } | null
): boolean {
  if (!bounds) return true;
  if (typeof item.start_time !== "number" || !Number.isFinite(item.start_time)) {
    return false;
  }
  const effectiveEnd =
    typeof item.end_time === "number" && Number.isFinite(item.end_time)
      ? Math.max(item.start_time, item.end_time)
      : item.start_time;
  return item.start_time < bounds.dayEnd && effectiveEnd >= bounds.dayStart;
}

function normalizePositiveLimit(limit: number | undefined, fallback: number): number {
  if (typeof limit === "number" && Number.isFinite(limit) && limit > 0) {
    return Math.max(1, Math.floor(limit));
  }
  return fallback;
}

export interface ReviewFeedQueryOptions {
  camera?: string | null;
  severity?: string | null;
  label?: string | null;
  excludeLabel?: string | null;
  zone?: string | null;
  status?: "unreviewed" | "reviewed" | "all";
  before?: number | null;
}

export interface EventsFeedQueryOptions {
  camera?: string | null;
  label?: string | null;
  excludeLabel?: string | null;
  zone?: string | null;
  minScore?: number | null;
  stationary?: "all" | "moving" | "stationary";
  retainedOnly?: boolean;
  plate?: string | null;
  before?: number | null;
}

/**
 * Builds the `/api/review` URL while honoring `reviewDateFilter` and optional
 * server-side filter / cursor parameters.
 */
export function buildReviewFeedUrl(
  reviewDateFilter?: string | null,
  limit?: number,
  options?: ReviewFeedQueryOptions
): string {
  const bounds = parseOperationalDateBoundsSec(reviewDateFilter);
  const resolvedLimit = normalizePositiveLimit(
    limit,
    bounds ? DEFAULT_DATE_FILTERED_REVIEW_FEED_LIMIT : DEFAULT_REVIEW_FEED_LIMIT
  );
  const params: string[] = [];
  if (bounds) {
    params.push(`after=${bounds.dayStart}`);
    const effBefore =
      typeof options?.before === "number" && Number.isFinite(options.before)
        ? Math.min(bounds.dayEnd, options.before)
        : bounds.dayEnd;
    params.push(`before=${effBefore}`);
  } else if (typeof options?.before === "number" && Number.isFinite(options.before)) {
    params.push(`before=${options.before}`);
  }
  if (options?.camera && options.camera !== "all") {
    params.push(`cameras=${encodeURIComponent(options.camera)}`);
  }
  if (options?.severity && options.severity !== "all") {
    params.push(`severity=${encodeURIComponent(options.severity)}`);
  }
  if (options?.label && options.label !== "all") {
    params.push(`labels=${encodeURIComponent(options.label)}`);
  }
  if (options?.excludeLabel && options.excludeLabel !== "none") {
    params.push(`exclude_labels=${encodeURIComponent(options.excludeLabel)}`);
  }
  if (options?.zone && options.zone !== "all") {
    params.push(`zones=${encodeURIComponent(options.zone)}`);
  }
  if (options?.status === "unreviewed") {
    params.push("reviewed=0");
  } else if (options?.status === "reviewed") {
    params.push("reviewed=1");
  }
  params.push(`limit=${resolvedLimit}`);
  return `/api/review?${params.join("&")}`;
}

/**
 * Builds the `/api/events` URL while honoring `reviewDateFilter` and optional
 * server-side filter / cursor parameters.
 */
export function buildEventsFeedUrl(
  reviewDateFilter?: string | null,
  limit?: number,
  options?: EventsFeedQueryOptions
): string {
  const bounds = parseOperationalDateBoundsSec(reviewDateFilter);
  const resolvedLimit = normalizePositiveLimit(
    limit,
    bounds ? DEFAULT_DATE_FILTERED_EVENTS_FEED_LIMIT : DEFAULT_EVENTS_FEED_LIMIT
  );
  const params: string[] = [];
  if (bounds) {
    params.push(`after=${bounds.dayStart}`);
    const effBefore =
      typeof options?.before === "number" && Number.isFinite(options.before)
        ? Math.min(bounds.dayEnd, options.before)
        : bounds.dayEnd;
    params.push(`before=${effBefore}`);
  } else if (typeof options?.before === "number" && Number.isFinite(options.before)) {
    params.push(`before=${options.before}`);
  }
  if (options?.camera && options.camera !== "all") {
    params.push(`cameras=${encodeURIComponent(options.camera)}`);
  }
  if (options?.label && options.label !== "all") {
    params.push(`labels=${encodeURIComponent(options.label)}`);
  }
  if (options?.excludeLabel && options.excludeLabel !== "none") {
    params.push(`exclude_labels=${encodeURIComponent(options.excludeLabel)}`);
  }
  if (options?.zone && options.zone !== "all") {
    params.push(`zones=${encodeURIComponent(options.zone)}`);
  }
  if (
    typeof options?.minScore === "number" &&
    Number.isFinite(options.minScore) &&
    options.minScore > 0
  ) {
    const normalizedScore =
      options.minScore > 1
        ? Math.min(1, options.minScore / 100)
        : options.minScore;
    params.push(`min_score=${Number(normalizedScore.toFixed(4))}`);
  }
  if (options?.stationary === "stationary") {
    params.push("stationary=1");
  } else if (options?.stationary === "moving") {
    params.push("stationary=0");
  }
  if (options?.retainedOnly) {
    params.push("favorites=1");
  }
  if (options?.plate && options.plate.trim()) {
    params.push(`plate=${encodeURIComponent(options.plate.trim())}`);
  }
  params.push(`limit=${resolvedLimit}`);
  return `/api/events?${params.join("&")}`;
}

export interface EventMutationPatch {
  subLabel?: string | null;
  subLabelScore?: number | null;
  recognizedLicensePlate?: string | null;
  notes?: string;
  falsePositive?: boolean;
  aiVerification?: AiVerificationResult | null;
  retainIndefinitely?: boolean;
}

export interface ReconcileEventState {
  events: EventItem[];
  selectedEvent: EventItem | null;
}

function applyPatchToSingleEvent(
  event: EventItem,
  patch: EventMutationPatch
): EventItem {
  let next: EventItem = event;

  if (patch.subLabel !== undefined) {
    const [updated] = applyEventSubLabelUpdate(
      [next],
      next.id,
      patch.subLabel,
      patch.subLabelScore
    );
    if (updated) {
      next = updated;
    }
  }

  if (patch.recognizedLicensePlate !== undefined) {
    next = {
      ...next,
      data: {
        ...(next.data || {}),
        recognized_license_plate: patch.recognizedLicensePlate,
      },
    };
  }

  if (patch.notes !== undefined) {
    next = {
      ...next,
      data: {
        ...(next.data || {}),
        notes: patch.notes,
      },
    };
  }

  if (patch.falsePositive !== undefined) {
    next = {
      ...next,
      false_positive: patch.falsePositive,
    };
  }

  if (patch.aiVerification !== undefined) {
    next = {
      ...next,
      data: {
        ...(next.data || {}),
        ai_verification: patch.aiVerification,
      },
    };
  }

  if (patch.retainIndefinitely !== undefined) {
    next = {
      ...next,
      retain_indefinitely: patch.retainIndefinitely,
    };
  }

  return next;
}

/**
 * Propagates event mutations (plate, sub-label, note, AI Judge verdict,
 * false-positive, retention) consistently to both the master `events` list
 * and `selectedEvent` detail item.
 */
export function reconcileEventMutation(
  state: ReconcileEventState,
  eventIds: string | ReadonlyArray<string>,
  patch: EventMutationPatch
): ReconcileEventState {
  const idList = typeof eventIds === "string" ? [eventIds] : eventIds;
  const idSet = new Set(idList.filter(Boolean));
  if (idSet.size === 0) {
    return state;
  }

  const events = state.events.map((ev) =>
    idSet.has(ev.id) ? applyPatchToSingleEvent(ev, patch) : ev
  );
  const selectedEvent =
    state.selectedEvent && idSet.has(state.selectedEvent.id)
      ? applyPatchToSingleEvent(state.selectedEvent, patch)
      : state.selectedEvent;

  return { events, selectedEvent };
}

export interface ReviewMutationPatch {
  subLabel?: string | null;
  recognizedLicensePlate?: string | null;
  notes?: string;
  hasBeenReviewed?: boolean;
}

export interface ReconcileReviewState {
  reviews: ReviewItem[];
  activeReview: ReviewItem | null;
}

function applyPatchToSingleReview(
  review: ReviewItem,
  patch: ReviewMutationPatch
): ReviewItem {
  let next: ReviewItem = review;

  if (patch.subLabel !== undefined) {
    const [updated] = applyReviewSubLabelUpdate([next], next.id, patch.subLabel);
    if (updated) {
      next = updated;
    }
  }

  if (patch.recognizedLicensePlate !== undefined) {
    const cleanPlate = patch.recognizedLicensePlate
      ? patch.recognizedLicensePlate.trim()
      : "";
    next = {
      ...next,
      data: {
        ...next.data,
        recognized_license_plates: cleanPlate ? [cleanPlate] : [],
      },
    };
  }

  if (patch.notes !== undefined) {
    next = {
      ...next,
      data: {
        ...next.data,
        notes: patch.notes,
      },
    };
  }

  if (patch.hasBeenReviewed !== undefined) {
    next = {
      ...next,
      has_been_reviewed: patch.hasBeenReviewed,
    };
  }

  return next;
}

/**
 * Propagates review mutations (plate, sub-label, note, reviewed status)
 * consistently to both the master `reviews` list and `activeReview` detail item
 * without ever mutating incident `[start_time, end_time]` bounds.
 */
export function reconcileReviewMutation(
  state: ReconcileReviewState,
  reviewIds: string | ReadonlyArray<string>,
  patch: ReviewMutationPatch
): ReconcileReviewState {
  const idList = typeof reviewIds === "string" ? [reviewIds] : reviewIds;
  const idSet = new Set(idList.filter(Boolean));
  if (idSet.size === 0) {
    return state;
  }

  const reviews = state.reviews.map((rev) =>
    idSet.has(rev.id) ? applyPatchToSingleReview(rev, patch) : rev
  );
  const activeReview =
    state.activeReview && idSet.has(state.activeReview.id)
      ? applyPatchToSingleReview(state.activeReview, patch)
      : state.activeReview;

  return { reviews, activeReview };
}

export interface ReconcileDeletedFaceProfileInput {
  events?: EventItem[];
  selectedEvent?: EventItem | null;
  reviews?: ReviewItem[];
  activeReview?: ReviewItem | null;
  enrolledFaces?: EnrolledFaceProfileItem[];
  localSubLabels?: Record<string, string | null>;
}

export interface ReconcileDeletedFaceProfileResult {
  events: EventItem[];
  selectedEvent: EventItem | null;
  reviews: ReviewItem[];
  activeReview: ReviewItem | null;
  enrolledFaces: EnrolledFaceProfileItem[];
  localSubLabels: Record<string, string | null>;
  clearedSelectedEvent: boolean;
  clearedActiveReview: boolean;
}

/**
 * Scrubs a deleted face profile name across `events`, `selectedEvent`,
 * `reviews`, `activeReview`, `enrolledFaces`, and local optimistic sub-label maps.
 */
export function reconcileDeletedFaceProfile(
  state: ReconcileDeletedFaceProfileInput,
  deletedProfileName: string | null | undefined
): ReconcileDeletedFaceProfileResult {
  const rawEvents = state.events ?? [];
  const rawSelectedEvent = state.selectedEvent ?? null;
  const rawReviews = state.reviews ?? [];
  const rawActiveReview = state.activeReview ?? null;
  const rawEnrolledFaces = state.enrolledFaces ?? [];
  const rawLocalSubLabels = state.localSubLabels ?? {};

  const targetLower = (deletedProfileName || "").trim().toLowerCase();
  if (!targetLower) {
    return {
      events: rawEvents,
      selectedEvent: rawSelectedEvent,
      reviews: rawReviews,
      activeReview: rawActiveReview,
      enrolledFaces: rawEnrolledFaces,
      localSubLabels: rawLocalSubLabels,
      clearedSelectedEvent: false,
      clearedActiveReview: false,
    };
  }

  const events = clearFaceProfileFromEvents(rawEvents, deletedProfileName!);
  const prevSelectedSub = rawSelectedEvent
    ? (rawSelectedEvent.sub_label || rawSelectedEvent.data?.sub_label || "")
        .trim()
        .toLowerCase()
    : "";
  const clearedSelectedEvent = prevSelectedSub === targetLower;
  const selectedEvent = rawSelectedEvent
    ? clearFaceProfileFromEvents([rawSelectedEvent], deletedProfileName!)[0]
    : null;

  const reviews = clearFaceProfileFromReviews(rawReviews, deletedProfileName!);
  const clearedActiveReview = Boolean(
    rawActiveReview &&
      (rawActiveReview.data?.sub_labels || []).some(
        (s) => s.trim().toLowerCase() === targetLower
      )
  );
  const activeReview = rawActiveReview
    ? clearFaceProfileFromReviews([rawActiveReview], deletedProfileName!)[0]
    : null;

  const enrolledFaces = rawEnrolledFaces.filter(
    (p) => p.name.trim().toLowerCase() !== targetLower
  );

  let localChanged = false;
  const nextLocal: Record<string, string | null> = {};
  for (const [id, val] of Object.entries(rawLocalSubLabels)) {
    if (typeof val === "string" && val.trim().toLowerCase() === targetLower) {
      nextLocal[id] = null;
      localChanged = true;
    } else {
      nextLocal[id] = val;
    }
  }

  return {
    events,
    selectedEvent,
    reviews,
    activeReview,
    enrolledFaces,
    localSubLabels: localChanged ? nextLocal : rawLocalSubLabels,
    clearedSelectedEvent,
    clearedActiveReview,
  };
}

export interface SampleMutationEpochTracker {
  readonly epochs: Record<string, number>;
  begin(sampleId: string): number;
  current(sampleId: string): number;
  isCurrent(sampleId: string, epoch: number): boolean;
}

/**
 * Per-sample monotonic mutation epoch tracker used by `DatasetStudio` to prevent
 * out-of-order `PUT /api/dataset/samples/:id` responses or rollbacks from
 * clobbering newer edits on the same `sample.id`.
 */
export function createSampleMutationEpochTracker(
  backingStore: Record<string, number> = {}
): SampleMutationEpochTracker {
  return {
    epochs: backingStore,
    begin(sampleId: string): number {
      const prev =
        typeof backingStore[sampleId] === "number" &&
        Number.isFinite(backingStore[sampleId])
          ? backingStore[sampleId]
          : 0;
      const next = prev + 1;
      backingStore[sampleId] = next;
      return next;
    },
    current(sampleId: string): number {
      const cur = backingStore[sampleId];
      return typeof cur === "number" && Number.isFinite(cur) ? cur : 0;
    },
    isCurrent(sampleId: string, epoch: number): boolean {
      return (
        typeof epoch === "number" &&
        Number.isFinite(epoch) &&
        epoch > 0 &&
        backingStore[sampleId] === epoch
      );
    },
  };
}

export interface ApplyFencedSampleOutcomeOptions<T extends { id: string }> {
  tracker: SampleMutationEpochTracker;
  sampleId: string;
  epoch: number;
  ok: boolean;
  serverItem?: T;
  rollbackItem: T;
  upsert: (items: T[], item: T) => T[];
}

export function applyFencedSampleMutationOutcome<T extends { id: string }>(
  items: T[],
  options: ApplyFencedSampleOutcomeOptions<T>
): { items: T[]; stale: boolean; applied: boolean } {
  if (!options.tracker.isCurrent(options.sampleId, options.epoch)) {
    return { items, stale: true, applied: false };
  }
  if (options.ok && options.serverItem) {
    return {
      items: options.upsert(items, options.serverItem),
      stale: false,
      applied: true,
    };
  }
  return {
    items: options.upsert(items, options.rollbackItem),
    stale: false,
    applied: true,
  };
}
