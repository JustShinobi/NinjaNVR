import type {
  AiVerificationResult,
  DetectedAttribute,
  EnrolledFaceProfileItem,
  EventItem,
  ReviewItem,
} from "../types/nvr";
import {
  DEFAULT_LPR_FORMAT_REGEX,
  formatLicensePlateBadge,
  normalizeLicensePlate,
  resolveKnownPlateLabel,
  validateLicensePlateFormat,
} from "./configYaml";
import { shouldRunFallbackAutoAnnotate } from "./datasetStudio";

export {
  DEFAULT_LPR_FORMAT_REGEX,
  formatLicensePlateBadge,
  normalizeLicensePlate,
  resolveKnownPlateLabel,
  validateLicensePlateFormat,
};

export function isSafeDatasetSampleId(id: unknown): id is string {
  return (
    typeof id === "string" &&
    id.length > 0 &&
    id.length <= 128 &&
    id !== "." &&
    !id.includes("..") &&
    /^[A-Za-z0-9_.-]+$/.test(id)
  );
}

export const MAX_FORENSIC_EXPORT_WINDOW_SEC = 86400;
export const REVIEW_PRESETS_STORAGE_KEY = "ninjanvr_saved_filters";
export const EXPLORE_PRESETS_STORAGE_KEY = "ninjanvr_explore_saved_filters";

const FORENSIC_RESERVED_KEYS = new Set(["__proto__", "constructor", "prototype"]);

export interface SavedReviewFilterPreset {
  name: string;
  camera: string;
  severity: string;
  object: string;
  excludeObject: string;
  zone: string;
  status: "unreviewed" | "reviewed" | "all";
}

export interface SavedExploreFilterPreset {
  name: string;
  camera: string;
  label: string;
  excludeLabel: string;
  zone: string;
  minScore: number;
  stationary: "all" | "moving" | "stationary";
  retainedOnly: boolean;
}

export type ForensicPresetKind = "review" | "explore";

export interface BatchFaceProgress {
  active: boolean;
  completed: number;
  total: number;
  name: string;
}

export interface PlateFilterableItem {
  sub_label?: string | null;
  data?: {
    recognized_license_plate?: string | null;
    recognized_license_plates?: string[];
    sub_label?: string | null;
    sub_labels?: string[];
  };
}

/**
 * Normalizes a raw license plate string for forensic storage and filtering.
 */
export function normalizeForensicLicensePlate(raw: string | null | undefined): string {
  return normalizeLicensePlate(raw || "");
}

/**
 * Normalizes a face profile name, rejecting path traversal, reserved keys, and "unknown".
 */
export function normalizeFaceProfileName(raw: string): string {
  const trimmed = (raw || "").trim();
  if (!trimmed || trimmed.includes("..") || /[\\/]/.test(trimmed)) {
    return "";
  }
  const cleaned = trimmed.replace(/[^a-zA-Z0-9_\- ]/g, "").trim();
  if (
    !cleaned ||
    !/[a-zA-Z0-9]/.test(cleaned) ||
    cleaned.toLowerCase() === "unknown" ||
    FORENSIC_RESERVED_KEYS.has(cleaned.toLowerCase())
  ) {
    return "";
  }
  return cleaned;
}

/**
 * Parses `/api/faces` payload into normalized `EnrolledFaceProfileItem[]` and deduplicates by name.
 */
export function parseFacesApiResponse(payload: unknown): EnrolledFaceProfileItem[] {
  if (!payload || typeof payload !== "object") return [];
  const obj = payload as Record<string, unknown>;
  let rawList: EnrolledFaceProfileItem[] = [];

  if (Array.isArray(obj.profiles)) {
    rawList = obj.profiles
      .filter(
        (item): item is Record<string, unknown> =>
          item !== null &&
          typeof item === "object" &&
          typeof (item as Record<string, unknown>).name === "string"
      )
      .map((item) => ({
        name: String(item.name),
        sample_count:
          typeof item.sample_count === "number"
            ? item.sample_count
            : Array.isArray(item.sample_files)
            ? item.sample_files.length
            : 1,
        sample_files: Array.isArray(item.sample_files)
          ? item.sample_files.map(String)
          : [],
        updated_at:
          typeof item.updated_at === "number" ? item.updated_at : undefined,
      }));
  } else {
    const facesMap =
      obj.faces && typeof obj.faces === "object" && !Array.isArray(obj.faces)
        ? (obj.faces as Record<string, unknown>)
        : obj;
    for (const [key, val] of Object.entries(facesMap)) {
      if (key === "profiles" || key === "faces") continue;
      if (Array.isArray(val)) {
        rawList.push({
          name: key,
          sample_count: val.length,
          sample_files: val.map(String),
        });
      }
    }
  }

  const seen = new Set<string>();
  const deduped: EnrolledFaceProfileItem[] = [];
  for (const item of rawList) {
    const trimmedName = item.name.trim();
    if (!trimmedName) continue;
    const key = trimmedName.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    deduped.push({
      ...item,
      name: trimmedName,
    });
  }
  return deduped;
}

export function formatAiVerificationBadge(verdict: AiVerificationResult): string {
  const rawConf =
    typeof verdict.confidence === "number" && Number.isFinite(verdict.confidence)
      ? verdict.confidence
      : 0;
  const pct = Math.max(0, Math.min(100, Math.round(rawConf * 100)));
  const status = verdict.is_true_positive ? "TRUE POSITIVE" : "FALSE POSITIVE";
  return `AI JUDGE: ${status} (${pct}%)`;
}

export function resolveFiniteEventScore(
  ev: Pick<EventItem, "top_score" | "score" | "data">,
  fallback: number = 0.85
): number {
  const candidates = [ev.top_score, ev.data?.top_score, ev.score];
  for (const val of candidates) {
    if (typeof val === "number" && Number.isFinite(val)) {
      return Math.max(0, Math.min(1, val));
    }
  }
  return typeof fallback === "number" && Number.isFinite(fallback)
    ? Math.max(0, Math.min(1, fallback))
    : 0.85;
}

export function shouldApplyEventMutationResult(
  selectedEventId: string | null | undefined,
  requestEventId: string,
  responseOk: boolean
): boolean {
  return Boolean(
    responseOk &&
      requestEventId &&
      selectedEventId &&
      selectedEventId === requestEventId
  );
}

export function shouldApplyReviewMutationResult(
  activeReviewId: string | null | undefined,
  requestReviewId: string,
  responseOk: boolean
): boolean {
  return Boolean(
    responseOk &&
      requestReviewId &&
      activeReviewId &&
      activeReviewId === requestReviewId
  );
}

export function getVisibleReviewPageIds(
  visibleReviews: ReadonlyArray<Pick<ReviewItem, "id">>,
  pageSize: number = 40
): string[] {
  const limit =
    Number.isFinite(pageSize) && pageSize > 0 ? Math.floor(pageSize) : 40;
  return visibleReviews
    .slice(0, limit)
    .map((r) => r.id)
    .filter((id): id is string => typeof id === "string" && id.length > 0);
}

export function resolveReviewThumbUrl(
  item: Pick<ReviewItem, "camera" | "thumb_path">
): string {
  const rawCam = (item.camera || "unknown").trim();
  const cleanCam =
    !rawCam || rawCam === "." || rawCam === ".." || rawCam.includes("..")
      ? "unknown"
      : rawCam;
  const safeCamera = encodeURIComponent(cleanCam);
  const fallback = `/api/${safeCamera}/latest.webp`;
  const raw = (item.thumb_path || "").trim();
  if (!raw || /[\u0000-\u001f\u007f\\?#]/.test(raw)) {
    return fallback;
  }
  const stripped = raw.replace(/^\/media\/[^/]+/, "");
  let decoded = stripped;
  try {
    decoded = decodeURIComponent(stripped);
  } catch {
    return fallback;
  }
  const segments = decoded.split("/");
  const isAllowedMediaPath = /^\/(?:clips|previews|exports)\//.test(decoded);
  const isAllowedApiThumbPath =
    /^\/api\/(?:events\/[^/]+\/(?:thumbnail\.webp|snapshot\.jpg)|preview\/[^/]+\/thumbnail\.jpg|[^/]+\/latest\.(?:webp|jpg))$/i.test(
      decoded
    ) && !/^\/api\/(?:auth|logout|users|tokens|config|restart)\b/i.test(decoded);
  if (
    !stripped.startsWith("/") ||
    stripped.startsWith("//") ||
    stripped.includes("..") ||
    decoded.startsWith("//") ||
    decoded.includes("..") ||
    decoded.includes("%") ||
    segments.some((seg) => seg === "." || seg === "..") ||
    /[\u0000-\u001f\u007f\\?#]/.test(decoded) ||
    (!isAllowedMediaPath && !isAllowedApiThumbPath)
  ) {
    return fallback;
  }
  return stripped;
}

export function resolveNextBriefingStep(
  queueIds: ReadonlyArray<string>,
  currentIdx: number,
  availableReviews: ReadonlyArray<ReviewItem>,
  activeReviewId?: string | null
): { nextIdx: number; nextReview: ReviewItem } | null {
  if (!Array.isArray(queueIds) || queueIds.length === 0) return null;
  const activeIdx =
    typeof activeReviewId === "string" && activeReviewId.length > 0
      ? queueIds.indexOf(activeReviewId)
      : -1;
  const baseIdx =
    activeIdx >= 0
      ? activeIdx
      : typeof currentIdx === "number" && Number.isFinite(currentIdx)
      ? Math.floor(currentIdx)
      : -1;
  const startIdx = Math.max(0, baseIdx + 1);
  for (let idx = startIdx; idx < queueIds.length; idx++) {
    const candidateId = queueIds[idx];
    if (!candidateId) continue;
    const found = availableReviews.find((r) => r.id === candidateId);
    if (found) {
      return { nextIdx: idx, nextReview: found };
    }
  }
  return null;
}

export function computeReviewPlaybackWindow(
  startTime: number,
  endTime: number | null | undefined,
  preRollSec: number = 4,
  postRollSec: number = 6,
  fallbackDurationSec: number = 20
): { startSec: number; endSec: number } {
  const safeStart =
    typeof startTime === "number" && Number.isFinite(startTime) && startTime > 0
      ? startTime
      : Math.floor(Date.now() / 1000) - fallbackDurationSec;
  const safePre =
    typeof preRollSec === "number" && Number.isFinite(preRollSec) && preRollSec >= 0
      ? preRollSec
      : 0;
  const safePost =
    typeof postRollSec === "number" && Number.isFinite(postRollSec) && postRollSec >= 0
      ? postRollSec
      : 0;
  const safeFallbackDur =
    typeof fallbackDurationSec === "number" &&
    Number.isFinite(fallbackDurationSec) &&
    fallbackDurationSec > 0
      ? fallbackDurationSec
      : 20;
  const baseEnd =
    typeof endTime === "number" && Number.isFinite(endTime) && endTime > safeStart
      ? endTime
      : safeStart + safeFallbackDur;
  const startSec = Math.max(0, safeStart - safePre);
  const endSec = Math.min(
    startSec + MAX_FORENSIC_EXPORT_WINDOW_SEC,
    Math.max(startSec + 1, baseEnd + safePost)
  );
  return { startSec, endSec };
}

export function parseSavedReviewPresets(
  raw: string | null | undefined
): SavedReviewFilterPreset[] {
  if (!raw || typeof raw !== "string") return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    const out: SavedReviewFilterPreset[] = [];
    for (const item of parsed) {
      if (!item || typeof item !== "object") continue;
      const rec = item as Record<string, unknown>;
      if (typeof rec.name !== "string" || !rec.name.trim()) continue;
      const status =
        rec.status === "unreviewed" ||
        rec.status === "reviewed" ||
        rec.status === "all"
          ? rec.status
          : "unreviewed";
      out.push({
        name: rec.name.trim(),
        camera: typeof rec.camera === "string" && rec.camera ? rec.camera : "all",
        severity:
          typeof rec.severity === "string" && rec.severity ? rec.severity : "all",
        object: typeof rec.object === "string" && rec.object ? rec.object : "all",
        excludeObject:
          typeof rec.excludeObject === "string" && rec.excludeObject
            ? rec.excludeObject
            : "none",
        zone: typeof rec.zone === "string" && rec.zone ? rec.zone : "all",
        status,
      });
    }
    return out;
  } catch {
    return [];
  }
}

export function parseSavedExplorePresets(
  raw: string | null | undefined
): SavedExploreFilterPreset[] {
  if (!raw || typeof raw !== "string") return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    const out: SavedExploreFilterPreset[] = [];
    for (const item of parsed) {
      if (!item || typeof item !== "object") continue;
      const rec = item as Record<string, unknown>;
      if (typeof rec.name !== "string" || !rec.name.trim()) continue;
      const stationary =
        rec.stationary === "all" ||
        rec.stationary === "moving" ||
        rec.stationary === "stationary"
          ? rec.stationary
          : "all";
      const minScore =
        typeof rec.minScore === "number" &&
        Number.isFinite(rec.minScore) &&
        rec.minScore >= 0 &&
        rec.minScore <= 100
          ? rec.minScore
          : 0;
      out.push({
        name: rec.name.trim(),
        camera: typeof rec.camera === "string" && rec.camera ? rec.camera : "all",
        label: typeof rec.label === "string" && rec.label ? rec.label : "all",
        excludeLabel:
          typeof rec.excludeLabel === "string" && rec.excludeLabel
            ? rec.excludeLabel
            : "none",
        zone: typeof rec.zone === "string" && rec.zone ? rec.zone : "all",
        minScore,
        stationary,
        retainedOnly: Boolean(rec.retainedOnly),
      });
    }
    return out;
  } catch {
    return [];
  }
}

export function matchesLicensePlateFilter(
  item: PlateFilterableItem,
  plateQuery: string
): boolean {
  const normQuery = normalizeForensicLicensePlate(plateQuery);
  if (!normQuery) return true;
  const candidates: string[] = [];
  if (item.data?.recognized_license_plate) {
    candidates.push(item.data.recognized_license_plate);
  }
  if (Array.isArray(item.data?.recognized_license_plates)) {
    candidates.push(...item.data.recognized_license_plates);
  }
  if (item.sub_label) {
    candidates.push(item.sub_label);
  }
  if (item.data?.sub_label) {
    candidates.push(item.data.sub_label);
  }
  if (Array.isArray(item.data?.sub_labels)) {
    candidates.push(...item.data.sub_labels);
  }
  for (const raw of candidates) {
    const norm = normalizeForensicLicensePlate(raw || "");
    if (norm.length > 0 && norm.includes(normQuery)) {
      return true;
    }
  }
  return false;
}

export function formatEventLabelWithSubLabel(
  label: string,
  subLabel?: string | null,
  detectedAttributes?: DetectedAttribute[]
): string {
  const primary = (label || "object").toUpperCase();
  const effectiveSub =
    subLabel && subLabel.trim().length > 0
      ? subLabel.trim().toUpperCase()
      : detectedAttributes && detectedAttributes.length > 0
      ? detectedAttributes[0].label.toUpperCase()
      : null;
  return effectiveSub ? `${primary} · ${effectiveSub}` : primary;
}

export function formatDetectedAttributePill(attr: DetectedAttribute): string {
  const safeScore =
    typeof attr.score === "number" && Number.isFinite(attr.score) ? attr.score : 0;
  const pct = Math.max(0, Math.min(100, Math.round(safeScore * 100)));
  const safeArea =
    typeof attr.area_px === "number" && Number.isFinite(attr.area_px)
      ? Math.max(0, Math.round(attr.area_px))
      : 0;
  return `${(attr.label || "attribute").toUpperCase()} ${pct}% · ${safeArea}px²`;
}

export function formatSubLabelBadge(
  subLabel?: string | null,
  subLabelScore?: number | null
): { text: string; isUnknown: boolean; isKnown: boolean } | null {
  const clean = subLabel?.trim();
  if (!clean) return null;
  const isUnknown = clean.toLowerCase() === "unknown";
  const scorePct =
    typeof subLabelScore === "number" &&
    Number.isFinite(subLabelScore) &&
    subLabelScore > 0
      ? ` ${Math.round(subLabelScore * 100)}%`
      : "";
  return {
    text: isUnknown
      ? `UNKNOWN VISITOR${scorePct}`
      : `ID: ${clean.toUpperCase()}${scorePct}`,
    isUnknown,
    isKnown: !isUnknown,
  };
}

export function applyEventSubLabelUpdate(
  events: EventItem[],
  eventId: string,
  subLabel: string | null,
  score?: number | null
): EventItem[] {
  const clean = subLabel && subLabel.trim().length > 0 ? subLabel.trim() : null;
  return events.map((ev) => {
    if (ev.id !== eventId) return ev;
    return {
      ...ev,
      sub_label: clean,
      sub_label_score: clean ? score ?? ev.sub_label_score ?? null : null,
      data: {
        ...(ev.data || {}),
        sub_label: clean,
        sub_label_score: clean ? score ?? ev.data?.sub_label_score ?? null : null,
      },
    };
  });
}

export function applyReviewSubLabelUpdate(
  reviews: ReviewItem[],
  reviewId: string,
  subLabel: string | null
): ReviewItem[] {
  const clean = subLabel && subLabel.trim().length > 0 ? subLabel.trim() : null;
  return reviews.map((rev) => {
    if (rev.id !== reviewId) return rev;
    return {
      ...rev,
      data: {
        ...(rev.data || {}),
        sub_labels: clean ? [clean] : [],
        ...(clean ? {} : { sub_label_score: null }),
      },
    };
  });
}

export function clearFaceProfileFromEvents(
  events: EventItem[],
  profileName: string
): EventItem[] {
  const target = profileName.trim().toLowerCase();
  if (!target) return events;
  return events.map((ev) => {
    const evSub = (ev.sub_label || ev.data?.sub_label || "").trim().toLowerCase();
    if (evSub !== target) return ev;
    return {
      ...ev,
      sub_label: null,
      sub_label_score: null,
      data: {
        ...(ev.data || {}),
        sub_label: null,
        sub_label_score: null,
      },
    };
  });
}

export function clearFaceProfileFromReviews(
  reviews: ReviewItem[],
  profileName: string
): ReviewItem[] {
  const target = profileName.trim().toLowerCase();
  if (!target) return reviews;
  return reviews.map((rev) => {
    const subs = rev.data?.sub_labels || [];
    if (!subs.some((s) => s.trim().toLowerCase() === target)) return rev;
    const remaining = subs.filter((s) => s.trim().toLowerCase() !== target);
    return {
      ...rev,
      data: {
        ...(rev.data || {}),
        sub_labels: remaining,
        ...(remaining.length === 0 ? { sub_label_score: null } : {}),
      },
    };
  });
}

// ---------------------------------------------------------------------------
// Deep Forensic Triage & Identity Controller (Ports & Adapters Seam)
// ---------------------------------------------------------------------------

export interface ForensicTriageDeps {
  fetchImpl?: typeof fetch;
  storage?: Pick<Storage, "getItem" | "setItem">;
}

export interface FetchEnrolledFacesOptions {
  excludeDeletedProfile?: string | { name?: string | null } | null;
  signal?: AbortSignal;
}

export interface AssignFaceSubLabelOptions {
  primaryEventId: string;
  detectionEventIds?: ReadonlyArray<string>;
  rawName: string | null | undefined;
  enrollInGallery?: boolean;
  backfillEvents?: boolean;
  targetKind?: "event" | "review";
  targetId?: string;
  getActiveTargetId?: () => string | null | undefined;
  isCancelled?: () => boolean;
}

export interface AssignFaceSubLabelResult {
  ok: boolean;
  invalidName: boolean;
  cleanName: string | null;
  usedFallbackSubLabel: boolean;
  backfilledEvents: number;
  sampleCount: number;
  clearedEventIds: string[];
  message?: string;
  stale: boolean;
  shouldApply: boolean;
}

export interface BatchAssignFaceItem {
  id: string;
  primaryEventId?: string;
  detectionEventIds?: ReadonlyArray<string>;
}

export interface BatchAssignFaceSubLabelOptions {
  items: ReadonlyArray<string | BatchAssignFaceItem>;
  rawName: string | null | undefined;
  enrollInGallery?: boolean;
  onProgress?: (progress: BatchFaceProgress | null) => void;
  onItemSuccess?: (itemId: string, cleanName: string | null) => void;
  isCancelled?: () => boolean;
}

export interface BatchAssignFaceSubLabelResult {
  ok: boolean;
  invalidName: boolean;
  cleanName: string | null;
  total: number;
  succeededIds: string[];
  failedIds: string[];
  totalBackfilled: number;
}

export interface SaveLicensePlateOptions {
  eventId: string;
  rawPlate: string | null | undefined;
  targetKind?: "event" | "review";
  targetId?: string;
  getActiveTargetId?: () => string | null | undefined;
  isCancelled?: () => boolean;
}

export interface SaveLicensePlateResult {
  ok: boolean;
  normalizedPlate: string | null;
  subLabel?: string | null;
  message?: string;
  stale: boolean;
  shouldApply: boolean;
}

export interface SaveForensicNoteOptions {
  targetKind: "event" | "review";
  targetId: string;
  notes: string;
  getActiveTargetId?: () => string | null | undefined;
  isCancelled?: () => boolean;
}

export interface SaveForensicNoteResult {
  ok: boolean;
  targetKind: "event" | "review";
  targetId: string;
  notes: string;
  message?: string;
  stale: boolean;
  shouldApply: boolean;
}

export interface ToggleFalsePositiveOptions {
  getActiveTargetId?: () => string | null | undefined;
  isCancelled?: () => boolean;
}

export interface ToggleFalsePositiveResult {
  stale: boolean;
  shouldApply: boolean;
  ok: boolean;
  nextFp: boolean;
  message?: string;
}

export interface VerifyWithAiOptions {
  getActiveTargetId?: () => string | null | undefined;
  isCancelled?: () => boolean;
}

export interface VerifyWithAiResult {
  stale: boolean;
  shouldApply: boolean;
  ok: boolean;
  verified: boolean;
  verifiedBy?: string;
  reason?: string;
  error?: string;
  falsePositive?: boolean;
  aiVerification?: AiVerificationResult;
}

export interface AddEventToDatasetOptions {
  eventId: string;
  camera: string;
  label: string;
  timestamp?: number;
  autoAnnotate?: boolean;
  getActiveTargetId?: () => string | null | undefined;
  isCancelled?: () => boolean;
}

export interface AddEventToDatasetResult {
  stale: boolean;
  shouldApply: boolean;
  ok: boolean;
  id?: string;
  split?: string;
  autoAnnotated: boolean;
  annotationCount: number;
  error?: string;
}

export interface ForensicTriageController {
  setActiveTarget: (
    targetKind: "event" | "review",
    targetId: string | null | undefined
  ) => number;
  invalidatePendingMutations: () => number;
  getMutationSequence: () => number;
  fetchEnrolledFaces: (
    options?: FetchEnrolledFacesOptions
  ) => Promise<EnrolledFaceProfileItem[]>;
  fetchEnrolledFaceNames: (
    options?: FetchEnrolledFacesOptions
  ) => Promise<string[]>;
  assignFaceSubLabel: (
    options: AssignFaceSubLabelOptions
  ) => Promise<AssignFaceSubLabelResult>;
  batchAssignFaceSubLabel: (
    options: BatchAssignFaceSubLabelOptions
  ) => Promise<BatchAssignFaceSubLabelResult>;
  batchAssignFace: (
    options: BatchAssignFaceSubLabelOptions
  ) => Promise<BatchAssignFaceSubLabelResult>;
  saveLicensePlate: (
    options: SaveLicensePlateOptions
  ) => Promise<SaveLicensePlateResult>;
  saveForensicNote: (
    options: SaveForensicNoteOptions
  ) => Promise<SaveForensicNoteResult>;
  toggleFalsePositive: (
    eventId: string,
    currentFp: boolean,
    options?: ToggleFalsePositiveOptions
  ) => Promise<ToggleFalsePositiveResult>;
  verifyWithAi: (
    eventId: string,
    options?: VerifyWithAiOptions
  ) => Promise<VerifyWithAiResult>;
  addEventToDataset: (
    params: AddEventToDatasetOptions
  ) => Promise<AddEventToDatasetResult>;
  loadFilterPresets: {
    (kind: "review"): SavedReviewFilterPreset[];
    (kind: "explore"): SavedExploreFilterPreset[];
    (
      kind: ForensicPresetKind
    ): SavedReviewFilterPreset[] | SavedExploreFilterPreset[];
  };
  saveFilterPreset: {
    (
      kind: "review",
      presets: ReadonlyArray<SavedReviewFilterPreset>,
      name: string,
      payload: Omit<SavedReviewFilterPreset, "name"> & { name?: string }
    ): SavedReviewFilterPreset[];
    (
      kind: "explore",
      presets: ReadonlyArray<SavedExploreFilterPreset>,
      name: string,
      payload: Omit<SavedExploreFilterPreset, "name"> & { name?: string }
    ): SavedExploreFilterPreset[];
  };
  deleteFilterPreset: {
    (
      kind: "review",
      presets: ReadonlyArray<SavedReviewFilterPreset>,
      name: string
    ): SavedReviewFilterPreset[];
    (
      kind: "explore",
      presets: ReadonlyArray<SavedExploreFilterPreset>,
      name: string
    ): SavedExploreFilterPreset[];
  };
}

function resolveDefaultStorage(): Pick<Storage, "getItem" | "setItem"> | null {
  try {
    if (typeof window !== "undefined" && window.localStorage) {
      return window.localStorage;
    }
  } catch {
    // localStorage may be unavailable in SSR or restricted sandboxes
  }
  return null;
}

export function createForensicTriageController(
  deps?: ForensicTriageDeps
): ForensicTriageController {
  const fetchFn: typeof fetch =
    deps?.fetchImpl ?? ((input, init) => fetch(input, init));
  const getStorage = (): Pick<Storage, "getItem" | "setItem"> | null =>
    deps?.storage ?? resolveDefaultStorage();

  let mutationSeq = 0;
  let targetEpoch = 0;
  const opSeqByKind: Record<
    "face" | "plate" | "note" | "falsePositive" | "verifyAi" | "dataset",
    number
  > = {
    face: 0,
    plate: 0,
    note: 0,
    falsePositive: 0,
    verifyAi: 0,
    dataset: 0,
  };
  const activeTargetByKind: Record<"event" | "review", string | null> = {
    event: null,
    review: null,
  };
  const hasExplicitTargetByKind: Record<"event" | "review", boolean> = {
    event: false,
    review: false,
  };

  const setActiveTarget = (
    targetKind: "event" | "review",
    targetId: string | null | undefined
  ): number => {
    const normalizedId = targetId ? targetId.trim() || null : null;
    const prevExplicit = hasExplicitTargetByKind[targetKind];
    const prevId = activeTargetByKind[targetKind];
    hasExplicitTargetByKind[targetKind] = true;
    activeTargetByKind[targetKind] = normalizedId;
    if (!prevExplicit || prevId !== normalizedId) {
      mutationSeq += 1;
      targetEpoch += 1;
    }
    return mutationSeq;
  };

  const invalidatePendingMutations = (): number => {
    mutationSeq += 1;
    targetEpoch += 1;
    return mutationSeq;
  };

  const getMutationSequence = (): number => mutationSeq;

  const evaluateMutationFreshness = (
    opKind: "face" | "plate" | "note" | "falsePositive" | "verifyAi" | "dataset",
    reqEpoch: number,
    reqOpSeq: number,
    targetKind: "event" | "review",
    requestTargetId: string,
    responseOk: boolean,
    getActiveTargetId?: () => string | null | undefined,
    isCancelled?: () => boolean
  ): { stale: boolean; shouldApply: boolean } => {
    const seqCurrent =
      reqEpoch === targetEpoch && reqOpSeq === opSeqByKind[opKind];
    const cancelled = Boolean(isCancelled?.());
    let targetMatches = true;
    if (getActiveTargetId) {
      const currentActiveId = getActiveTargetId();
      targetMatches =
        targetKind === "review"
          ? shouldApplyReviewMutationResult(
              currentActiveId,
              requestTargetId,
              true
            )
          : shouldApplyEventMutationResult(
              currentActiveId,
              requestTargetId,
              true
            );
    } else if (hasExplicitTargetByKind[targetKind]) {
      const currentActiveId = activeTargetByKind[targetKind];
      targetMatches =
        targetKind === "review"
          ? shouldApplyReviewMutationResult(
              currentActiveId,
              requestTargetId,
              true
            )
          : shouldApplyEventMutationResult(
              currentActiveId,
              requestTargetId,
              true
            );
    }
    const stale = !seqCurrent || !targetMatches || cancelled;
    return {
      stale,
      shouldApply: Boolean(responseOk && !stale),
    };
  };

  const fetchEnrolledFaces = async (
    options?: FetchEnrolledFacesOptions
  ): Promise<EnrolledFaceProfileItem[]> => {
    try {
      const res = await fetchFn("/api/faces", {
        credentials: "include",
        ...(options?.signal ? { signal: options.signal } : {}),
      });
      if (!res.ok) return [];
      const payload = await res.json().catch(() => null);
      if (!payload) return [];
      const parsed = parseFacesApiResponse(payload);
      const rawExclude =
        typeof options?.excludeDeletedProfile === "string"
          ? options.excludeDeletedProfile
          : options?.excludeDeletedProfile?.name || "";
      const deletedLower = rawExclude.trim().toLowerCase();
      if (!deletedLower) return parsed;
      return parsed.filter(
        (profile) => profile.name.trim().toLowerCase() !== deletedLower
      );
    } catch {
      return [];
    }
  };

  const assignFaceSubLabel = async (
    options: AssignFaceSubLabelOptions
  ): Promise<AssignFaceSubLabelResult> => {
    const hasRawInput =
      typeof options.rawName === "string" && options.rawName.trim().length > 0;
    const cleanName = hasRawInput
      ? normalizeFaceProfileName(options.rawName!) || null
      : null;

    if (hasRawInput && !cleanName) {
      return {
        ok: false,
        invalidName: true,
        cleanName: null,
        usedFallbackSubLabel: false,
        backfilledEvents: 0,
        sampleCount: 0,
        clearedEventIds: [],
        message: "Enter a valid face name (letters, numbers, spaces, _ or -)",
        stale: false,
        shouldApply: false,
      };
    }

    const allDets = (options.detectionEventIds || [])
      .map((d) => (d || "").trim())
      .filter((d) => d.length > 0);
    const primaryEventId = (
      options.primaryEventId ||
      allDets[0] ||
      ""
    ).trim();
    const targetKind = options.targetKind ?? "event";
    const targetId = (options.targetId || primaryEventId).trim();

    if (!primaryEventId && allDets.length === 0) {
      return {
        ok: false,
        invalidName: false,
        cleanName,
        usedFallbackSubLabel: false,
        backfilledEvents: 0,
        sampleCount: 0,
        clearedEventIds: [],
        stale: false,
        shouldApply: false,
      };
    }

    mutationSeq += 1;
    const reqOpSeq = ++opSeqByKind.face;
    const reqEpoch = targetEpoch;

    if (cleanName) {
      const enrollInGallery = options.enrollInGallery ?? true;
      const backfillEvents = options.backfillEvents ?? true;
      let res: Response;
      let usedFallbackSubLabel = false;

      if (enrollInGallery) {
        res = await fetchFn(
          `/api/faces/${encodeURIComponent(cleanName)}/enroll`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({
              event_id: primaryEventId,
              assign_sub_label: true,
              backfill_events: backfillEvents,
            }),
          }
        );
        if (!res.ok) {
          usedFallbackSubLabel = true;
          res = await fetchFn(
            `/api/events/${encodeURIComponent(primaryEventId)}/sub_label`,
            {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              credentials: "include",
              body: JSON.stringify({
                sub_label: cleanName,
                sub_label_score: 1.0,
                enroll_face: true,
              }),
            }
          );
        }
      } else {
        res = await fetchFn(
          `/api/events/${encodeURIComponent(primaryEventId)}/sub_label`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({
              sub_label: cleanName,
              sub_label_score: 1.0,
              enroll_face: false,
            }),
          }
        );
      }

      const data = await res.json().catch(() => null);
      const rawBackfilled = Number(
        (data as Record<string, unknown> | null)?.backfilled_events || 0
      );
      const rawSamples = Number(
        (data as Record<string, unknown> | null)?.sample_count || 0
      );
      const message =
        typeof (data as Record<string, unknown> | null)?.message === "string"
          ? String((data as Record<string, unknown>).message)
          : undefined;
      const { stale, shouldApply } = evaluateMutationFreshness(
        "face",
        reqEpoch,
        reqOpSeq,
        targetKind,
        targetId,
        res.ok,
        options.getActiveTargetId,
        options.isCancelled
      );
      return {
        ok: res.ok,
        invalidName: false,
        cleanName,
        usedFallbackSubLabel,
        backfilledEvents: Number.isFinite(rawBackfilled) ? rawBackfilled : 0,
        sampleCount: Number.isFinite(rawSamples) ? rawSamples : 0,
        clearedEventIds: [],
        message,
        stale,
        shouldApply,
      };
    }

    // Clear sub-label across all detectionEventIds (or primaryEventId)
    const targetDetIds =
      allDets.length > 0 ? Array.from(new Set(allDets)) : [primaryEventId];
    let anyOk = false;
    let lastErrorMessage: string | undefined;
    const clearedEventIds: string[] = [];

    for (const did of targetDetIds) {
      const res = await fetchFn(
        `/api/events/${encodeURIComponent(did)}/sub_label`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ sub_label: null }),
        }
      );
      if (res.ok) {
        anyOk = true;
        clearedEventIds.push(did);
      } else {
        const errData = await res.json().catch(() => null);
        if (
          typeof (errData as Record<string, unknown> | null)?.message === "string"
        ) {
          lastErrorMessage = String(
            (errData as Record<string, unknown>).message
          );
        }
      }
    }

    const { stale, shouldApply } = evaluateMutationFreshness(
      "face",
      reqEpoch,
      reqOpSeq,
      targetKind,
      targetId,
      anyOk,
      options.getActiveTargetId,
      options.isCancelled
    );

    return {
      ok: anyOk,
      invalidName: false,
      cleanName: null,
      usedFallbackSubLabel: false,
      backfilledEvents: 0,
      sampleCount: 0,
      clearedEventIds,
      message: lastErrorMessage,
      stale,
      shouldApply,
    };
  };

  const batchAssignFaceSubLabel = async (
    options: BatchAssignFaceSubLabelOptions
  ): Promise<BatchAssignFaceSubLabelResult> => {
    const normalizedItems = (options.items || [])
      .map((entry): BatchAssignFaceItem | null => {
        if (typeof entry === "string") {
          const cleanId = entry.trim();
          return cleanId
            ? {
                id: cleanId,
                primaryEventId: cleanId,
                detectionEventIds: [cleanId],
              }
            : null;
        }
        if (!entry || typeof entry !== "object") return null;
        const cleanId = (entry.id || "").trim();
        const dets = (entry.detectionEventIds || [])
          .map((d) => (d || "").trim())
          .filter((d) => d.length > 0);
        const primary = (entry.primaryEventId || dets[0] || cleanId).trim();
        if (!cleanId && !primary) return null;
        return {
          id: cleanId || primary,
          primaryEventId: primary,
          detectionEventIds: dets.length > 0 ? dets : [primary],
        };
      })
      .filter((x): x is BatchAssignFaceItem => x !== null);

    // Batch clear mode when rawName is explicitly null
    if (options.rawName === null) {
      const succeededIds: string[] = [];
      const failedIds: string[] = [];
      for (const item of normalizedItems) {
        if (options.isCancelled?.()) break;
        const detIds =
          item.detectionEventIds && item.detectionEventIds.length > 0
            ? item.detectionEventIds
            : item.primaryEventId
            ? [item.primaryEventId]
            : [item.id];
        let itemOk = false;
        for (const did of detIds) {
          const res = await fetchFn(
            `/api/events/${encodeURIComponent(did)}/sub_label`,
            {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              credentials: "include",
              body: JSON.stringify({ sub_label: null }),
            }
          ).catch(() => null);
          if (res?.ok) itemOk = true;
        }
        if (itemOk) {
          succeededIds.push(item.id);
        } else {
          failedIds.push(item.id);
        }
        if (!options.isCancelled?.()) {
          options.onItemSuccess?.(item.id, null);
        }
      }
      return {
        ok: succeededIds.length > 0,
        invalidName: false,
        cleanName: null,
        total: normalizedItems.length,
        succeededIds,
        failedIds,
        totalBackfilled: 0,
      };
    }

    const cleanName = normalizeFaceProfileName(options.rawName || "");
    if (!cleanName) {
      return {
        ok: false,
        invalidName: true,
        cleanName: null,
        total: normalizedItems.length,
        succeededIds: [],
        failedIds: [],
        totalBackfilled: 0,
      };
    }

    if (normalizedItems.length === 0) {
      return {
        ok: false,
        invalidName: false,
        cleanName,
        total: 0,
        succeededIds: [],
        failedIds: [],
        totalBackfilled: 0,
      };
    }

    const enrollInGallery = options.enrollInGallery ?? true;
    const succeededIds: string[] = [];
    const failedIds: string[] = [];
    let totalBackfilled = 0;
    let lastEnrolledEventId: string | null = null;
    let backfillRan = false;

    options.onProgress?.({
      active: true,
      completed: 0,
      total: normalizedItems.length,
      name: cleanName,
    });

    try {
      for (let i = 0; i < normalizedItems.length; i += 1) {
        if (options.isCancelled?.()) break;
        const item = normalizedItems[i];
        const primaryEventId = (item.primaryEventId || item.id).trim();
        const isLast = i === normalizedItems.length - 1;
        if (primaryEventId) {
          let res: Response;
          if (enrollInGallery) {
            res = await fetchFn(
              `/api/faces/${encodeURIComponent(cleanName)}/enroll`,
              {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                credentials: "include",
                body: JSON.stringify({
                  event_id: primaryEventId,
                  assign_sub_label: true,
                  backfill_events: isLast,
                }),
              }
            );
            if (res.ok) {
              lastEnrolledEventId = primaryEventId;
              if (isLast) {
                backfillRan = true;
              }
            } else {
              res = await fetchFn(
                `/api/events/${encodeURIComponent(primaryEventId)}/sub_label`,
                {
                  method: "POST",
                  headers: { "Content-Type": "application/json" },
                  credentials: "include",
                  body: JSON.stringify({
                    sub_label: cleanName,
                    sub_label_score: 1.0,
                    enroll_face: true,
                  }),
                }
              );
            }
          } else {
            res = await fetchFn(
              `/api/events/${encodeURIComponent(primaryEventId)}/sub_label`,
              {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                credentials: "include",
                body: JSON.stringify({
                  sub_label: cleanName,
                  sub_label_score: 1.0,
                  enroll_face: false,
                }),
              }
            );
          }
          if (res.ok) {
            succeededIds.push(item.id);
            const data = await res.json().catch(() => null);
            const bf = Number(
              (data as Record<string, unknown> | null)?.backfilled_events || 0
            );
            if (Number.isFinite(bf) && bf > 0) {
              totalBackfilled += bf;
            }
            if (!options.isCancelled?.()) {
              options.onItemSuccess?.(item.id, cleanName);
            }
          } else {
            failedIds.push(item.id);
          }
        } else {
          failedIds.push(item.id);
        }

        if (!options.isCancelled?.()) {
          options.onProgress?.({
            active: true,
            completed: i + 1,
            total: normalizedItems.length,
            name: cleanName,
          });
        }
      }

      if (
        enrollInGallery &&
        lastEnrolledEventId &&
        !backfillRan &&
        !options.isCancelled?.()
      ) {
        const bfRes = await fetchFn(
          `/api/faces/${encodeURIComponent(cleanName)}/enroll`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({
              event_id: lastEnrolledEventId,
              assign_sub_label: true,
              backfill_events: true,
            }),
          }
        ).catch(() => null);
        if (bfRes?.ok) {
          const bfData = await bfRes.json().catch(() => null);
          const bf = Number(
            (bfData as Record<string, unknown> | null)?.backfilled_events || 0
          );
          if (Number.isFinite(bf) && bf > 0) {
            totalBackfilled += bf;
          }
        }
      }

      return {
        ok: succeededIds.length > 0,
        invalidName: false,
        cleanName,
        total: normalizedItems.length,
        succeededIds,
        failedIds,
        totalBackfilled,
      };
    } finally {
      if (!options.isCancelled?.()) {
        options.onProgress?.(null);
      }
    }
  };

  const saveLicensePlate = async (
    options: SaveLicensePlateOptions
  ): Promise<SaveLicensePlateResult> => {
    const cleanEventId = (options.eventId || "").trim();
    const norm = normalizeForensicLicensePlate(options.rawPlate);
    const plateOrNull = norm.length > 0 ? norm : null;
    const targetKind = options.targetKind ?? "event";
    const targetId = (options.targetId || cleanEventId).trim();

    if (!cleanEventId) {
      return {
        ok: false,
        normalizedPlate: plateOrNull,
        stale: false,
        shouldApply: false,
      };
    }

    mutationSeq += 1;
    const reqOpSeq = ++opSeqByKind.plate;
    const reqEpoch = targetEpoch;
    const res = await fetchFn(
      `/api/events/${encodeURIComponent(cleanEventId)}/license_plate`,
      {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          recognized_license_plate: plateOrNull,
        }),
      }
    );

    const payload = await res.json().catch(() => null);
    const rec =
      payload && typeof payload === "object"
        ? (payload as Record<string, unknown>)
        : null;
    const resolvedPlate =
      rec && "recognized_license_plate" in rec
        ? typeof rec.recognized_license_plate === "string" &&
          rec.recognized_license_plate.trim().length > 0
          ? normalizeForensicLicensePlate(rec.recognized_license_plate)
          : null
        : plateOrNull;
    const resolvedSubLabel =
      rec && "sub_label" in rec
        ? (rec.sub_label as string | null | undefined)
        : undefined;
    const message =
      rec && typeof rec.message === "string" ? rec.message : undefined;

    const { stale, shouldApply } = evaluateMutationFreshness(
      "plate",
      reqEpoch,
      reqOpSeq,
      targetKind,
      targetId,
      res.ok,
      options.getActiveTargetId,
      options.isCancelled
    );

    return {
      ok: res.ok,
      normalizedPlate: resolvedPlate,
      subLabel: resolvedSubLabel,
      message,
      stale,
      shouldApply,
    };
  };

  const saveForensicNote = async (
    options: SaveForensicNoteOptions
  ): Promise<SaveForensicNoteResult> => {
    const cleanTargetId = (options.targetId || "").trim();
    const noteToSave = typeof options.notes === "string" ? options.notes : "";
    if (!cleanTargetId) {
      return {
        ok: false,
        targetKind: options.targetKind,
        targetId: "",
        notes: noteToSave,
        stale: false,
        shouldApply: false,
      };
    }

    mutationSeq += 1;
    const reqOpSeq = ++opSeqByKind.note;
    const reqEpoch = targetEpoch;
    const endpoint =
      options.targetKind === "review"
        ? `/api/review/${encodeURIComponent(cleanTargetId)}/notes`
        : `/api/events/${encodeURIComponent(cleanTargetId)}/notes`;

    const res = await fetchFn(endpoint, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ notes: noteToSave }),
    });

    let message: string | undefined;
    if (!res.ok) {
      const data = await res.json().catch(() => null);
      if (
        data &&
        typeof data === "object" &&
        typeof (data as Record<string, unknown>).message === "string"
      ) {
        message = String((data as Record<string, unknown>).message);
      }
    }

    const { stale, shouldApply } = evaluateMutationFreshness(
      "note",
      reqEpoch,
      reqOpSeq,
      options.targetKind,
      cleanTargetId,
      res.ok,
      options.getActiveTargetId,
      options.isCancelled
    );

    return {
      ok: res.ok,
      targetKind: options.targetKind,
      targetId: cleanTargetId,
      notes: noteToSave,
      message,
      stale,
      shouldApply,
    };
  };

  const fetchEnrolledFaceNames = async (
    options?: FetchEnrolledFacesOptions
  ): Promise<string[]> => {
    const profiles = await fetchEnrolledFaces(options);
    return Array.from(
      new Set(profiles.map((p) => p.name.trim()).filter(Boolean))
    ).sort((a, b) => a.localeCompare(b));
  };

  const toggleFalsePositive = async (
    eventId: string,
    currentFp: boolean,
    options?: ToggleFalsePositiveOptions
  ): Promise<ToggleFalsePositiveResult> => {
    const cleanEventId = (eventId || "").trim();
    const desiredFp = !currentFp;
    if (!cleanEventId) {
      return {
        stale: false,
        shouldApply: false,
        ok: false,
        nextFp: currentFp,
      };
    }

    mutationSeq += 1;
    const reqOpSeq = ++opSeqByKind.falsePositive;
    const reqEpoch = targetEpoch;

    let res = await fetchFn(
      `/api/events/${encodeURIComponent(cleanEventId)}/false_positive`,
      {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ false_positive: desiredFp }),
      }
    );
    if (res.status === 404 || res.status === 405) {
      res = await fetchFn(
        `/api/events/${encodeURIComponent(cleanEventId)}/false-positive`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ false_positive: desiredFp }),
        }
      );
    }

    const rawData = await res.json().catch(() => null);
    const rec =
      rawData && typeof rawData === "object"
        ? (rawData as Record<string, unknown>)
        : null;
    const resolvedFp =
      rec && typeof rec.false_positive === "boolean"
        ? rec.false_positive
        : res.ok
        ? desiredFp
        : currentFp;
    const message =
      rec && typeof rec.message === "string"
        ? rec.message
        : rec && typeof rec.error === "string"
        ? rec.error
        : undefined;

    const { stale, shouldApply } = evaluateMutationFreshness(
      "falsePositive",
      reqEpoch,
      reqOpSeq,
      "event",
      cleanEventId,
      res.ok,
      options?.getActiveTargetId,
      options?.isCancelled
    );

    return {
      stale,
      shouldApply,
      ok: res.ok,
      nextFp: resolvedFp,
      ...(message ? { message } : {}),
    };
  };

  const verifyWithAi = async (
    eventId: string,
    options?: VerifyWithAiOptions
  ): Promise<VerifyWithAiResult> => {
    const cleanEventId = (eventId || "").trim();
    if (!cleanEventId) {
      return {
        stale: false,
        shouldApply: false,
        ok: false,
        verified: false,
        error: "Missing event ID",
      };
    }

    mutationSeq += 1;
    const reqOpSeq = ++opSeqByKind.verifyAi;
    const reqEpoch = targetEpoch;

    const res = await fetchFn(
      `/api/events/${encodeURIComponent(cleanEventId)}/verify-ai`,
      {
        method: "POST",
        credentials: "include",
      }
    );

    const rawData = await res.json().catch(() => null);
    const rec =
      rawData && typeof rawData === "object"
        ? (rawData as Record<string, unknown>)
        : null;

    const { stale, shouldApply } = evaluateMutationFreshness(
      "verifyAi",
      reqEpoch,
      reqOpSeq,
      "event",
      cleanEventId,
      Boolean(res.ok && rec),
      options?.getActiveTargetId,
      options?.isCancelled
    );

    if (!res.ok || !rec) {
      const errMsg =
        (rec && typeof rec.error === "string" && rec.error) ||
        (rec && typeof rec.message === "string" && rec.message) ||
        "Vision AI Judge verification failed";
      return {
        stale,
        shouldApply: false,
        ok: false,
        verified: false,
        error: errMsg,
      };
    }

    const nestedVerdict =
      rec.ai_verification && typeof rec.ai_verification === "object"
        ? (rec.ai_verification as Partial<AiVerificationResult>)
        : null;
    const rawConf = Number(nestedVerdict?.confidence ?? rec.confidence ?? 0.9);
    const isTruePositive =
      typeof rec.verified === "boolean"
        ? rec.verified
        : typeof nestedVerdict?.is_true_positive === "boolean"
        ? nestedVerdict.is_true_positive
        : Boolean(rec.is_true_positive);
    const reason = String(
      nestedVerdict?.reason || rec.reason || "Verified by Vision AI Judge"
    );
    const verifiedBy =
      (typeof rec.verified_by === "string" && rec.verified_by) ||
      (typeof rec.verifiedBy === "string" && rec.verifiedBy) ||
      (typeof nestedVerdict?.provider === "string" && nestedVerdict.provider) ||
      (typeof rec.provider === "string" && rec.provider) ||
      (typeof nestedVerdict?.model === "string" && nestedVerdict.model) ||
      (typeof rec.model === "string" && rec.model) ||
      undefined;

    const aiVerification: AiVerificationResult = {
      is_true_positive: isTruePositive,
      confidence: Number.isFinite(rawConf) ? rawConf : 0.9,
      reason,
      provider:
        typeof nestedVerdict?.provider === "string"
          ? nestedVerdict.provider
          : typeof rec.provider === "string"
          ? rec.provider
          : undefined,
      model:
        typeof nestedVerdict?.model === "string"
          ? nestedVerdict.model
          : typeof rec.model === "string"
          ? rec.model
          : undefined,
      verified_at:
        typeof nestedVerdict?.verified_at === "number"
          ? nestedVerdict.verified_at
          : typeof rec.verified_at === "number"
          ? rec.verified_at
          : undefined,
    };

    return {
      stale,
      shouldApply,
      ok: true,
      verified: isTruePositive,
      ...(verifiedBy ? { verifiedBy } : {}),
      reason,
      ...(rec.false_positive !== undefined
        ? { falsePositive: Boolean(rec.false_positive) }
        : {}),
      aiVerification,
    };
  };

  const addEventToDataset = async (
    params: AddEventToDatasetOptions
  ): Promise<AddEventToDatasetResult> => {
    const cleanEventId = (params.eventId || "").trim();
    if (!cleanEventId) {
      return {
        stale: false,
        shouldApply: false,
        ok: false,
        autoAnnotated: false,
        annotationCount: 0,
        error: "Missing event ID",
      };
    }

    mutationSeq += 1;
    const reqOpSeq = ++opSeqByKind.dataset;
    const reqEpoch = targetEpoch;
    const autoAnnotate = params.autoAnnotate ?? true;

    const requestBody = JSON.stringify({
      event_id: cleanEventId,
      camera: params.camera,
      label: params.label,
      ...(params.timestamp !== undefined ? { timestamp: params.timestamp } : {}),
      auto_annotate: autoAnnotate,
    });

    let res = await fetchFn(
      `/api/dataset/samples/from-event/${encodeURIComponent(cleanEventId)}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: requestBody,
      }
    );
    if (res.status === 404 || res.status === 405) {
      res = await fetchFn("/api/dataset/samples/from-event", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: requestBody,
      });
    }

    const rawData = await res.json().catch(() => null);
    const data =
      rawData && typeof rawData === "object"
        ? (rawData as Record<string, unknown> & {
            sample?: Record<string, unknown> | null;
          })
        : null;

    if (!res.ok || !data) {
      const { stale } = evaluateMutationFreshness(
        "dataset",
        reqEpoch,
        reqOpSeq,
        "event",
        cleanEventId,
        false,
        params.getActiveTargetId,
        params.isCancelled
      );
      const errMsg =
        (data && typeof data.error === "string" && data.error) ||
        (data && typeof data.message === "string" && data.message) ||
        "Failed to add event snapshot to training dataset";
      return {
        stale,
        shouldApply: false,
        ok: false,
        autoAnnotated: false,
        annotationCount: 0,
        error: errMsg,
      };
    }

    const rawSampleId = String(
      data.id || data.sample?.id || `ds_${cleanEventId}`
    ).trim();
    const sampleKind = String(
      data.sample_kind || data.sample?.sample_kind || ""
    )
      .trim()
      .toLowerCase();
    let split = String(data.split || data.sample?.split || "train");
    const initialAnns = Array.isArray(data.annotations)
      ? data.annotations
      : Array.isArray(data.sample?.annotations)
      ? data.sample.annotations
      : null;

    let autoAnnotated = Boolean(
      data.auto_annotated === true ||
        sampleKind === "ai_annotated" ||
        (initialAnns && initialAnns.length > 0 && data.auto_annotate_attempted !== false)
    );
    let annotationCount = Array.isArray(initialAnns)
      ? initialAnns.length
      : typeof data.annotation_count === "number"
      ? data.annotation_count
      : 0;

    if (
      !autoAnnotated &&
      isSafeDatasetSampleId(rawSampleId) &&
      shouldRunFallbackAutoAnnotate(
        data as {
          sample_kind?: string;
          sample?: { sample_kind?: string } | null;
          auto_annotate_attempted?: boolean;
          auto_annotate_error?: string | null;
        },
        autoAnnotate
      )
    ) {
      try {
        const aiRes = await fetchFn(
          `/api/dataset/samples/${encodeURIComponent(
            rawSampleId
          )}/auto-annotate`,
          {
            method: "POST",
            credentials: "include",
          }
        );
        const aiRaw = await aiRes.json().catch(() => null);
        const aiData =
          aiRaw && typeof aiRaw === "object"
            ? (aiRaw as Record<string, unknown> & {
                sample?: Record<string, unknown> | null;
              })
            : null;
        if (aiRes.ok && aiData) {
          const anns = aiData.annotations ?? aiData.sample?.annotations;
          if (Array.isArray(anns)) {
            annotationCount = anns.length;
            autoAnnotated = true;
          } else if (typeof aiData.annotation_count === "number") {
            annotationCount = aiData.annotation_count;
            autoAnnotated = true;
          } else {
            autoAnnotated = true;
          }
          if (aiData.split || aiData.sample?.split) {
            split = String(aiData.split || aiData.sample?.split);
          }
        }
      } catch {
        // Keep sample even if AI teacher is offline
      }
    }

    const { stale, shouldApply } = evaluateMutationFreshness(
      "dataset",
      reqEpoch,
      reqOpSeq,
      "event",
      cleanEventId,
      true,
      params.getActiveTargetId,
      params.isCancelled
    );

    return {
      stale,
      shouldApply,
      ok: true,
      id: rawSampleId,
      split,
      autoAnnotated,
      annotationCount,
    };
  };

  function loadFilterPresets(kind: "review"): SavedReviewFilterPreset[];
  function loadFilterPresets(kind: "explore"): SavedExploreFilterPreset[];
  function loadFilterPresets(
    kind: ForensicPresetKind
  ): SavedReviewFilterPreset[] | SavedExploreFilterPreset[];
  function loadFilterPresets(
    kind: ForensicPresetKind
  ): SavedReviewFilterPreset[] | SavedExploreFilterPreset[] {
    const storage = getStorage();
    const storageKey =
      kind === "review"
        ? REVIEW_PRESETS_STORAGE_KEY
        : EXPLORE_PRESETS_STORAGE_KEY;
    try {
      const raw = storage?.getItem(storageKey) ?? null;
      return kind === "review"
        ? parseSavedReviewPresets(raw)
        : parseSavedExplorePresets(raw);
    } catch {
      return [];
    }
  }

  function saveFilterPreset(
    kind: "review",
    presets: ReadonlyArray<SavedReviewFilterPreset>,
    name: string,
    payload: Omit<SavedReviewFilterPreset, "name"> & { name?: string }
  ): SavedReviewFilterPreset[];
  function saveFilterPreset(
    kind: "explore",
    presets: ReadonlyArray<SavedExploreFilterPreset>,
    name: string,
    payload: Omit<SavedExploreFilterPreset, "name"> & { name?: string }
  ): SavedExploreFilterPreset[];
  function saveFilterPreset(
    kind: ForensicPresetKind,
    presets:
      | ReadonlyArray<SavedReviewFilterPreset>
      | ReadonlyArray<SavedExploreFilterPreset>,
    name: string,
    payload:
      | (Omit<SavedReviewFilterPreset, "name"> & { name?: string })
      | (Omit<SavedExploreFilterPreset, "name"> & { name?: string })
  ): SavedReviewFilterPreset[] | SavedExploreFilterPreset[] {
    const storage = getStorage();
    if (kind === "review") {
      const revPresets = presets as ReadonlyArray<SavedReviewFilterPreset>;
      const revPayload = payload as Omit<SavedReviewFilterPreset, "name"> & {
        name?: string;
      };
      const cam = revPayload.camera || "all";
      const sev = revPayload.severity || "all";
      const resolvedName =
        (name || "").trim() ||
        (revPayload.name || "").trim() ||
        `${cam === "all" ? "ALL" : cam.toUpperCase()} · ${sev.toUpperCase()}`;
      const status =
        revPayload.status === "unreviewed" ||
        revPayload.status === "reviewed" ||
        revPayload.status === "all"
          ? revPayload.status
          : "unreviewed";
      const next: SavedReviewFilterPreset[] = [
        ...revPresets.filter((p) => p.name !== resolvedName),
        {
          name: resolvedName,
          camera: cam,
          severity: sev,
          object: revPayload.object || "all",
          excludeObject: revPayload.excludeObject || "none",
          zone: revPayload.zone || "all",
          status,
        },
      ];
      try {
        storage?.setItem(REVIEW_PRESETS_STORAGE_KEY, JSON.stringify(next));
      } catch {
        // Ignore storage quota errors
      }
      return next;
    }

    const expPresets = presets as ReadonlyArray<SavedExploreFilterPreset>;
    const expPayload = payload as Omit<SavedExploreFilterPreset, "name"> & {
      name?: string;
    };
    const cam = expPayload.camera || "all";
    const lbl = expPayload.label || "all";
    const resolvedName =
      (name || "").trim() ||
      (expPayload.name || "").trim() ||
      `${cam === "all" ? "ALL" : cam.toUpperCase()} · ${lbl.toUpperCase()}`;
    const minScore =
      typeof expPayload.minScore === "number" &&
      Number.isFinite(expPayload.minScore) &&
      expPayload.minScore >= 0 &&
      expPayload.minScore <= 100
        ? expPayload.minScore
        : 0;
    const stationary =
      expPayload.stationary === "all" ||
      expPayload.stationary === "moving" ||
      expPayload.stationary === "stationary"
        ? expPayload.stationary
        : "all";
    const next: SavedExploreFilterPreset[] = [
      ...expPresets.filter((p) => p.name !== resolvedName),
      {
        name: resolvedName,
        camera: cam,
        label: lbl,
        excludeLabel: expPayload.excludeLabel || "none",
        zone: expPayload.zone || "all",
        minScore,
        stationary,
        retainedOnly: Boolean(expPayload.retainedOnly),
      },
    ];
    try {
      storage?.setItem(EXPLORE_PRESETS_STORAGE_KEY, JSON.stringify(next));
    } catch {
      // Ignore storage quota errors
    }
    return next;
  }

  function deleteFilterPreset(
    kind: "review",
    presets: ReadonlyArray<SavedReviewFilterPreset>,
    name: string
  ): SavedReviewFilterPreset[];
  function deleteFilterPreset(
    kind: "explore",
    presets: ReadonlyArray<SavedExploreFilterPreset>,
    name: string
  ): SavedExploreFilterPreset[];
  function deleteFilterPreset(
    kind: ForensicPresetKind,
    presets:
      | ReadonlyArray<SavedReviewFilterPreset>
      | ReadonlyArray<SavedExploreFilterPreset>,
    name: string
  ): SavedReviewFilterPreset[] | SavedExploreFilterPreset[] {
    const storage = getStorage();
    const trimmed = (name || "").trim();
    if (kind === "review") {
      const revPresets = presets as ReadonlyArray<SavedReviewFilterPreset>;
      const next = revPresets.filter(
        (p) => p.name !== name && p.name !== trimmed
      );
      try {
        storage?.setItem(REVIEW_PRESETS_STORAGE_KEY, JSON.stringify(next));
      } catch {
        // Ignore storage errors
      }
      return next;
    }
    const expPresets = presets as ReadonlyArray<SavedExploreFilterPreset>;
    const next = expPresets.filter(
      (p) => p.name !== name && p.name !== trimmed
    );
    try {
      storage?.setItem(EXPLORE_PRESETS_STORAGE_KEY, JSON.stringify(next));
    } catch {
      // Ignore storage errors
    }
    return next;
  }

  return {
    setActiveTarget,
    invalidatePendingMutations,
    getMutationSequence,
    fetchEnrolledFaces,
    fetchEnrolledFaceNames,
    assignFaceSubLabel,
    batchAssignFaceSubLabel,
    batchAssignFace: batchAssignFaceSubLabel,
    saveLicensePlate,
    saveForensicNote,
    toggleFalsePositive,
    verifyWithAi,
    addEventToDataset,
    loadFilterPresets,
    saveFilterPreset,
    deleteFilterPreset,
  };
}
