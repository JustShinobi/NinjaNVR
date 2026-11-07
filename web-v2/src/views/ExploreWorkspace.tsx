import React from "react";
import { Camera, Play, Sparkles, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import {
  AiVerificationResult,
  DetectedAttribute,
  EnrolledFaceProfileItem,
  EventItem,
  ExportItem,
  RecordingDaySummary,
  ReviewItem,
} from "../types/nvr";
import { WorkspaceTab, findMatchingExport, formatDateTime } from "../utils/format";
import {
  DEFAULT_LPR_FORMAT_REGEX,
  formatLicensePlateBadge,
  normalizeLicensePlate,
  resolveKnownPlateLabel,
  validateLicensePlateFormat,
} from "../utils/configYaml";
import { RecordingCalendarPopover } from "../components/RecordingCalendarPopover";
import {
  isSafeDatasetSampleId,
  normalizeFaceProfileName,
  parseFacesApiResponse,
} from "./settings/VisionAndAiPanels";

export function formatAiVerificationBadge(verdict: AiVerificationResult): string {
  const rawConf =
    typeof verdict.confidence === "number" && Number.isFinite(verdict.confidence)
      ? verdict.confidence
      : 0;
  const pct = Math.max(0, Math.min(100, Math.round(rawConf * 100)));
  const status = verdict.is_true_positive ? "TRUE POSITIVE" : "FALSE POSITIVE";
  return `AI JUDGE: ${status} (${pct}%)`;
}

export {
  DEFAULT_LPR_FORMAT_REGEX,
  formatLicensePlateBadge,
  normalizeLicensePlate,
  resolveKnownPlateLabel,
  validateLicensePlateFormat,
};

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
  ev: Pick<EventItem, "sub_label" | "data">,
  plateQuery: string
): boolean {
  const normQuery = normalizeLicensePlate(plateQuery);
  if (!normQuery) return true;
  const evPlate = normalizeLicensePlate(ev.data?.recognized_license_plate || "");
  const evSub = normalizeLicensePlate(ev.sub_label || "");
  return (
    (evPlate.length > 0 && evPlate.includes(normQuery)) ||
    (evSub.length > 0 && evSub.includes(normQuery))
  );
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

interface ExploreWorkspaceProps {
  cameraNames: string[];
  filteredEvents: EventItem[];
  exportsList?: ExportItem[];
  onOpenExport?: (exp: ExportItem) => void;
  exploreQuery: string;
  setExploreQuery: (q: string) => void;
  exploreCamera: string;
  setExploreCamera: (cam: string) => void;
  exploreLabel: string;
  setExploreLabel: (lbl: string) => void;
  exploreRetainedOnly: boolean;
  setExploreRetainedOnly: React.Dispatch<React.SetStateAction<boolean>>;
  globalDaysSummary: RecordingDaySummary[];
  reviewDateFilter: string;
  setReviewDateFilter: (date: string) => void;
  selectedEvent: EventItem | null;
  setSelectedEvent: (ev: EventItem | null) => void;
  eventViewMode: "snapshot" | "clip";
  setEventViewMode: (mode: "snapshot" | "clip") => void;
  handleEventRetain: (ev: EventItem) => Promise<void>;
  handleEventDelete: (id: string) => Promise<void>;
  setSyncMasterTs: (ts: number) => void;
  switchTab: (tab: WorkspaceTab) => void;
  notify: (msg: string) => void;
}

export const ExploreWorkspace: React.FC<ExploreWorkspaceProps> = ({
  cameraNames,
  filteredEvents,
  exportsList = [],
  onOpenExport,
  exploreQuery,
  setExploreQuery,
  exploreCamera,
  setExploreCamera,
  exploreLabel,
  setExploreLabel,
  exploreRetainedOnly,
  setExploreRetainedOnly,
  globalDaysSummary,
  reviewDateFilter,
  setReviewDateFilter,
  selectedEvent,
  setSelectedEvent,
  eventViewMode,
  setEventViewMode,
  handleEventRetain,
  handleEventDelete,
  setSyncMasterTs,
  switchTab,
  notify,
}) => {
  const { t } = useI18n();
  const [showForensic, setShowForensic] = React.useState(false);
  const [eventNoteDraft, setEventNoteDraft] = React.useState("");
  const [aiVerifyBusy, setAiVerifyBusy] = React.useState(false);
  const [datasetAddBusy, setDatasetAddBusy] = React.useState(false);
  const [addedDatasetIds, setAddedDatasetIds] = React.useState<Record<string, boolean>>({});
  const isMountedRef = React.useRef(true);
  const eventMutationSeqRef = React.useRef(0);
  const selectedEventIdRef = React.useRef<string | null>(selectedEvent?.id ?? null);
  selectedEventIdRef.current = selectedEvent?.id ?? null;
  const debugReplayAbortRef = React.useRef<AbortController | null>(null);
  const debugReplaySeqRef = React.useRef(0);

  const handleAddToDataset = async () => {
    if (!selectedEvent || datasetAddBusy) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    const seq = ++eventMutationSeqRef.current;
    setDatasetAddBusy(true);
    try {
      const res = await fetch(
        `/api/dataset/samples/from-event/${encodeURIComponent(targetId)}`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ auto_annotate: true }),
        }
      );
      const data = await res.json().catch(() => null);
      if (!isMountedRef.current) return;
      if (res.ok && data) {
        const rawSampleId = String(
          data.id || data.sample?.id || `ds_${targetId}`
        ).trim();
        const sampleKind = String(
          data.sample_kind || data.sample?.sample_kind || ""
        )
          .trim()
          .toLowerCase();
        let split = data.split || data.sample?.split || "train";
        let aiBoxCount: number | null =
          sampleKind === "ai_annotated" && Array.isArray(data.annotations)
            ? data.annotations.length
            : null;
        if (
          isSafeDatasetSampleId(rawSampleId) &&
          sampleKind !== "hard_negative" &&
          sampleKind !== "ai_annotated"
        ) {
          try {
            const aiRes = await fetch(
              `/api/dataset/samples/${encodeURIComponent(rawSampleId)}/auto-annotate`,
              {
                method: "POST",
                credentials: "include",
              }
            );
            const aiData = await aiRes.json().catch(() => null);
            if (aiRes.ok && aiData) {
              const anns = aiData.annotations || aiData.sample?.annotations;
              if (Array.isArray(anns)) {
                aiBoxCount = anns.length;
              }
              if (aiData.split || aiData.sample?.split) {
                split = aiData.split || aiData.sample?.split;
              }
            }
          } catch {
            // Keep sample even if AI teacher is offline
          }
        }
        if (!isMountedRef.current) return;
        setAddedDatasetIds((prev) => ({ ...prev, [targetId]: true }));
        notify(
          aiBoxCount !== null
            ? `Added event #${targetId.slice(-8)} & AI-annotated ${aiBoxCount} YOLO boxes (split: ${String(
                split
              ).toUpperCase()})`
            : `Added event #${targetId.slice(-8)} clean frame to Active Learning Dataset (split: ${String(
                split
              ).toUpperCase()})`
        );
      } else {
        notify(data?.message || "Failed to add event snapshot to training dataset");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error while adding event snapshot to training dataset");
      }
    } finally {
      if (isMountedRef.current && seq === eventMutationSeqRef.current) {
        setDatasetAddBusy(false);
      }
    }
  };

  const handleToggleFalsePositive = async () => {
    if (!selectedEvent) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    const nextFp = !targetEvent.false_positive;
    const seq = ++eventMutationSeqRef.current;
    try {
      const res = await fetch(
        `/api/events/${encodeURIComponent(targetId)}/false_positive`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ false_positive: nextFp }),
        }
      );
      if (!isMountedRef.current) return;
      if (res.ok) {
        if (
          seq === eventMutationSeqRef.current &&
          shouldApplyEventMutationResult(
            selectedEventIdRef.current,
            targetId,
            true
          )
        ) {
          setSelectedEvent({
            ...targetEvent,
            false_positive: nextFp,
          });
        }
        notify(
          nextFp
            ? "Marked as False Positive — spatial & dhash64 suppression signature saved"
            : "Cleared False Positive flag & removed suppression signature"
        );
      } else {
        const data = await res.json().catch(() => null);
        notify(data?.message || "Failed to update False Positive status");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error while updating False Positive status");
      }
    }
  };

  const handleVerifyWithAi = async () => {
    if (!selectedEvent || aiVerifyBusy) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    const seq = ++eventMutationSeqRef.current;
    setAiVerifyBusy(true);
    try {
      const res = await fetch(
        `/api/events/${encodeURIComponent(targetId)}/verify-ai`,
        {
          method: "POST",
          credentials: "include",
        }
      );
      const data = await res.json().catch(() => null);
      if (!isMountedRef.current) return;
      if (res.ok && data) {
        const rawConf = Number(data.confidence ?? 0.9);
        const aiVerdict: AiVerificationResult = data.ai_verification || {
          is_true_positive: Boolean(data.is_true_positive),
          confidence: Number.isFinite(rawConf) ? rawConf : 0.9,
          reason: String(data.reason || "Verified by Vision AI Judge"),
          provider: data.provider,
          model: data.model,
          verified_at: data.verified_at,
        };
        if (
          seq === eventMutationSeqRef.current &&
          shouldApplyEventMutationResult(
            selectedEventIdRef.current,
            targetId,
            true
          )
        ) {
          setSelectedEvent({
            ...targetEvent,
            false_positive:
              data.false_positive !== undefined
                ? Boolean(data.false_positive)
                : targetEvent.false_positive,
            data: {
              ...(targetEvent.data || {}),
              ai_verification: aiVerdict,
            },
          });
        }
        notify(
          `${formatAiVerificationBadge(aiVerdict)} — ${aiVerdict.reason}`
        );
      } else {
        notify(data?.message || "Vision AI Judge verification failed");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Vision AI Judge request failed");
      }
    } finally {
      if (isMountedRef.current && seq === eventMutationSeqRef.current) {
        setAiVerifyBusy(false);
      }
    }
  };
  const [faceNameDraft, setFaceNameDraft] = React.useState("");
  const [enrolledFaces, setEnrolledFaces] = React.useState<EnrolledFaceProfileItem[]>([]);
  const [selectedBatchEventIds, setSelectedBatchEventIds] = React.useState<string[]>([]);
  const [batchFaceProgress, setBatchFaceProgress] = React.useState<{
    active: boolean;
    completed: number;
    total: number;
    name: string;
  } | null>(null);
  const [eventPlateDraft, setEventPlateDraft] = React.useState("");
  const [plateFilter, setPlateFilter] = React.useState("");
  const lastDeletedFaceProfile = useNvrStore((s) => s.lastDeletedFaceProfile);
  const subLabelsRefreshTick = useNvrStore((s) => s.subLabelsRefreshTick);
  const bumpSubLabelsRefresh = useNvrStore((s) => s.bumpSubLabelsRefresh);

  React.useEffect(() => {
    setEventNoteDraft(selectedEvent?.data?.notes || "");
    setFaceNameDraft(
      selectedEvent?.sub_label && selectedEvent.sub_label !== "unknown"
        ? selectedEvent.sub_label
        : ""
    );
    setEventPlateDraft(selectedEvent?.data?.recognized_license_plate || "");
  }, [
    selectedEvent?.id,
    selectedEvent?.data?.notes,
    selectedEvent?.sub_label,
    selectedEvent?.data?.recognized_license_plate,
  ]);

  React.useEffect(() => {
    selectedEventIdRef.current = selectedEvent?.id ?? null;
    eventMutationSeqRef.current += 1;
    setAiVerifyBusy(false);
    setDatasetAddBusy(false);
    if (debugReplayAbortRef.current) {
      debugReplayAbortRef.current.abort();
      debugReplayAbortRef.current = null;
    }
    debugReplaySeqRef.current += 1;
    setDebugBusy(false);
    setDebugTrace(null);
  }, [selectedEvent?.id]);

  React.useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
      eventMutationSeqRef.current += 1;
      debugReplaySeqRef.current += 1;
      if (debugReplayAbortRef.current) {
        debugReplayAbortRef.current.abort();
        debugReplayAbortRef.current = null;
      }
    };
  }, []);

  React.useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    fetch("/api/faces", { credentials: "include", signal: controller.signal })
      .then((r) => (r.ok ? r.json() : null))
      .then((d) => {
        if (cancelled || !d) return;
        const parsed = parseFacesApiResponse(d);
        const deletedLower = lastDeletedFaceProfile?.name?.trim().toLowerCase();
        setEnrolledFaces(
          deletedLower
            ? parsed.filter((p) => p.name.trim().toLowerCase() !== deletedLower)
            : parsed
        );
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [subLabelsRefreshTick, lastDeletedFaceProfile]);

  React.useEffect(() => {
    if (!lastDeletedFaceProfile?.name) return;
    const deletedLower = lastDeletedFaceProfile.name.trim().toLowerCase();
    setEnrolledFaces((prev) =>
      prev.filter((p) => p.name.trim().toLowerCase() !== deletedLower)
    );
    if (
      selectedEvent &&
      (selectedEvent.sub_label || selectedEvent.data?.sub_label || "")
        .trim()
        .toLowerCase() === deletedLower
    ) {
      const [cleared] = clearFaceProfileFromEvents(
        [selectedEvent],
        lastDeletedFaceProfile.name
      );
      setSelectedEvent(cleared);
      setFaceNameDraft("");
    }
  }, [lastDeletedFaceProfile, selectedEvent, setSelectedEvent]);

  const handleAssignFaceSubLabel = async (overrideName?: string | null) => {
    if (!selectedEvent) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    const raw = overrideName !== undefined ? overrideName : faceNameDraft;
    const clean =
      raw && raw.trim().length > 0
        ? normalizeFaceProfileName(raw) || null
        : null;
    if (raw && raw.trim().length > 0 && !clean) {
      notify("Enter a valid face name (letters, numbers, spaces, _ or -)");
      return;
    }
    const seq = ++eventMutationSeqRef.current;
    try {
      let res: Response;
      if (clean) {
        res = await fetch(`/api/faces/${encodeURIComponent(clean)}/enroll`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({
            event_id: targetId,
            assign_sub_label: true,
            backfill_events: true,
          }),
        });
        if (!res.ok) {
          res = await fetch(
            `/api/events/${encodeURIComponent(targetId)}/sub_label`,
            {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              credentials: "include",
              body: JSON.stringify({
                sub_label: clean,
                sub_label_score: 1.0,
                enroll_face: true,
              }),
            }
          );
        }
      } else {
        res = await fetch(
          `/api/events/${encodeURIComponent(targetId)}/sub_label`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({ sub_label: null }),
          }
        );
      }
      if (!isMountedRef.current) return;
      if (res.ok) {
        const data = await res.json().catch(() => null);
        if (
          seq === eventMutationSeqRef.current &&
          shouldApplyEventMutationResult(
            selectedEventIdRef.current,
            targetId,
            true
          )
        ) {
          const [updated] = applyEventSubLabelUpdate(
            [targetEvent],
            targetId,
            clean,
            clean ? 1.0 : null
          );
          setSelectedEvent(updated);
        }
        bumpSubLabelsRefresh();
        const backfilled = Number(data?.backfilled_events || 0);
        const sampleCount = Number(data?.sample_count || 0);
        notify(
          clean
            ? `Assigned & enrolled '${clean}' into Face Gallery${
                sampleCount > 0 ? ` (${sampleCount} samples)` : ""
              }${backfilled > 0 ? ` · +${backfilled} similar events matched` : ""}`
            : "Cleared event sub-label"
        );
      } else {
        const data = await res.json().catch(() => null);
        notify(data?.message || "Failed to update event face sub-label");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error while updating event face sub-label");
      }
    }
  };

  const handleEnrollFaceFromEvent = async () => {
    await handleAssignFaceSubLabel();
  };

  const handleBatchAssignFace = async (overrideName?: string) => {
    const raw = overrideName !== undefined ? overrideName : faceNameDraft;
    const cleanName = normalizeFaceProfileName(raw || "");
    if (!cleanName) {
      notify("Enter or pick a person name (e.g. Erik) before batch face assignment");
      return;
    }
    const targetIds =
      selectedBatchEventIds.length > 0
        ? selectedBatchEventIds
        : selectedEvent
        ? [selectedEvent.id]
        : [];
    if (targetIds.length === 0) {
      notify("Select one or more events to assign & enroll in batch");
      return;
    }
    setFaceNameDraft(cleanName);
    setBatchFaceProgress({
      active: true,
      completed: 0,
      total: targetIds.length,
      name: cleanName,
    });
    let okCount = 0;
    let totalBackfilled = 0;
    try {
      for (let i = 0; i < targetIds.length; i += 1) {
        if (!isMountedRef.current) break;
        const eid = targetIds[i];
        const isLast = i === targetIds.length - 1;
        const res = await fetch(
          `/api/faces/${encodeURIComponent(cleanName)}/enroll`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({
              event_id: eid,
              assign_sub_label: true,
              backfill_events: isLast,
            }),
          }
        );
        if (res.ok) {
          okCount += 1;
          const data = await res.json().catch(() => null);
          totalBackfilled += Number(data?.backfilled_events || 0);
          if (selectedEvent && selectedEvent.id === eid) {
            const [updated] = applyEventSubLabelUpdate(
              [selectedEvent],
              eid,
              cleanName,
              1.0
            );
            setSelectedEvent(updated);
          }
        }
        if (isMountedRef.current) {
          setBatchFaceProgress({
            active: true,
            completed: i + 1,
            total: targetIds.length,
            name: cleanName,
          });
        }
      }
      if (isMountedRef.current) {
        bumpSubLabelsRefresh();
        setSelectedBatchEventIds([]);
        notify(
          `Batch assigned & enrolled '${cleanName}' on ${okCount}/${targetIds.length} events${
            totalBackfilled > 0 ? ` (+${totalBackfilled} auto-matched)` : ""
          }`
        );
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error during batch face assignment");
      }
    } finally {
      if (isMountedRef.current) {
        setBatchFaceProgress(null);
      }
    }
  };

  const handleSaveEventLicensePlate = async () => {
    if (!selectedEvent) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    const normalized = normalizeLicensePlate(eventPlateDraft);
    const seq = ++eventMutationSeqRef.current;
    try {
      const res = await fetch(
        `/api/events/${encodeURIComponent(targetId)}/license_plate`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({
            recognized_license_plate: normalized || null,
          }),
        }
      );
      if (!isMountedRef.current) return;
      if (res.ok) {
        const payload = await res.json().catch(() => ({}));
        if (!isMountedRef.current) return;
        const nextPlate =
          payload.recognized_license_plate !== undefined
            ? payload.recognized_license_plate
            : normalized || null;
        const nextSubLabel =
          payload.sub_label !== undefined
            ? payload.sub_label
            : targetEvent.sub_label;
        if (
          seq === eventMutationSeqRef.current &&
          shouldApplyEventMutationResult(
            selectedEventIdRef.current,
            targetId,
            true
          )
        ) {
          setSelectedEvent({
            ...targetEvent,
            sub_label: nextSubLabel,
            data: {
              ...(targetEvent.data || {}),
              recognized_license_plate: nextPlate,
            },
          });
          setEventPlateDraft(nextPlate || "");
        }
        notify(
          nextPlate
            ? `License plate set to ${nextPlate}`
            : "Cleared recognized license plate"
        );
      } else {
        const data = await res.json().catch(() => null);
        notify(data?.message || "Failed to save recognized license plate");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error while saving recognized license plate");
      }
    }
  };

  const handleSaveEventNote = async () => {
    if (!selectedEvent) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    const noteToSave = eventNoteDraft;
    const seq = ++eventMutationSeqRef.current;
    try {
      const res = await fetch(`/api/events/${encodeURIComponent(targetId)}/notes`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ notes: noteToSave }),
      });
      if (!isMountedRef.current) return;
      if (res.ok) {
        if (
          seq === eventMutationSeqRef.current &&
          shouldApplyEventMutationResult(
            selectedEventIdRef.current,
            targetId,
            true
          )
        ) {
          setSelectedEvent({
            ...targetEvent,
            data: { ...(targetEvent.data || {}), notes: noteToSave },
          });
        }
        notify("Saved forensic note on event");
      } else {
        const data = await res.json().catch(() => null);
        notify(data?.message || "Failed to save forensic note");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error while saving forensic note");
      }
    }
  };
  const [showDebugReplay, setShowDebugReplay] = React.useState(false);
  const [debugMinScore, setDebugMinScore] = React.useState(0.5);
  const [debugMinArea, setDebugMinArea] = React.useState(0);
  const [debugBusy, setDebugBusy] = React.useState(false);
  const [debugTrace, setDebugTrace] = React.useState<{
    segments_analyzed: number;
    frames: Array<{
      ts: number;
      motion_boxes: unknown[];
      raw_detections: Array<{ label: string; score: number; area_px: number }>;
      active_tracks: Array<{ label: string; score: number; area_px: number }>;
      filtered_reasons: Array<{ label: string; score: number; area_px: number; reason: string }>;
    }>;
  } | null>(null);

  const handleRunDebugReplay = async () => {
    if (!selectedEvent) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    if (debugReplayAbortRef.current) {
      debugReplayAbortRef.current.abort();
    }
    const controller = new AbortController();
    debugReplayAbortRef.current = controller;
    const seq = ++debugReplaySeqRef.current;
    setDebugBusy(true);
    try {
      const safeStart =
        typeof targetEvent.start_time === "number" &&
        Number.isFinite(targetEvent.start_time) &&
        targetEvent.start_time > 0
          ? Math.max(0, targetEvent.start_time - 2)
          : 0;
      const rawEnd =
        typeof targetEvent.end_time === "number" &&
        Number.isFinite(targetEvent.end_time) &&
        targetEvent.end_time > targetEvent.start_time
          ? targetEvent.end_time + 2
          : safeStart + 19;
      const safeEnd = Math.min(safeStart + 300, Math.max(safeStart + 1, rawEnd));
      const safeMinScore =
        typeof debugMinScore === "number" && Number.isFinite(debugMinScore)
          ? Math.max(0.05, Math.min(0.99, debugMinScore))
          : 0.5;
      const safeMinArea =
        typeof debugMinArea === "number" && Number.isFinite(debugMinArea)
          ? Math.max(0, Math.min(24000000, Math.round(debugMinArea)))
          : 0;
      const res = await fetch("/api/debug/reprocess", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        signal: controller.signal,
        body: JSON.stringify({
          camera: targetEvent.camera,
          start_time: safeStart,
          end_time: safeEnd,
          override_config: {
            min_score: safeMinScore,
            min_area: safeMinArea,
          },
        }),
      });
      if (
        !isMountedRef.current ||
        seq !== debugReplaySeqRef.current ||
        !shouldApplyEventMutationResult(
          selectedEventIdRef.current,
          targetId,
          true
        )
      ) {
        return;
      }
      if (res.ok) {
        const data = await res.json();
        if (
          isMountedRef.current &&
          seq === debugReplaySeqRef.current &&
          shouldApplyEventMutationResult(
            selectedEventIdRef.current,
            targetId,
            true
          )
        ) {
          setDebugTrace(data);
          setEventViewMode("clip");
        }
      } else {
        const err = await res.json().catch(() => null);
        if (isMountedRef.current) {
          notify(err?.message || "Retroactive detection debug replay failed");
        }
      }
    } catch (err) {
      if (
        isMountedRef.current &&
        (err as { name?: string })?.name !== "AbortError"
      ) {
        notify("Network error during retroactive detection debug replay");
      }
    } finally {
      if (isMountedRef.current && seq === debugReplaySeqRef.current) {
        setDebugBusy(false);
      }
    }
  };
  const [excludeLabel, setExcludeLabel] = React.useState("none");
  const [zoneFilter, setZoneFilter] = React.useState("all");
  const [minScoreFilter, setMinScoreFilter] = React.useState(0);
  const [stationaryFilter, setStationaryFilter] = React.useState<"all" | "moving" | "stationary">(
    "all"
  );
  const [presetName, setPresetName] = React.useState("");
  const [savedPresets, setSavedPresets] = React.useState<
    SavedExploreFilterPreset[]
  >(() => {
    try {
      const raw = window.localStorage?.getItem("ninjanvr_explore_saved_filters");
      return parseSavedExplorePresets(raw);
    } catch {
      return [];
    }
  });

  const availableZones = React.useMemo(() => {
    const set = new Set<string>();
    for (const ev of filteredEvents) {
      for (const z of ev.zones || ev.data?.provenance?.zones || []) {
        if (z) set.add(z);
      }
    }
    return Array.from(set).sort();
  }, [filteredEvents]);

  const visibleEvents = React.useMemo(() => {
    return filteredEvents.filter((ev) => {
      if (excludeLabel !== "none" && ev.label.toLowerCase() === excludeLabel) {
        return false;
      }
      if (zoneFilter !== "all") {
        const zList = ev.zones || ev.data?.provenance?.zones || [];
        if (!zList.includes(zoneFilter)) return false;
      }
      const sc = resolveFiniteEventScore(ev, 0.85);
      if (minScoreFilter > 0 && sc * 100 < minScoreFilter) {
        return false;
      }
      const isStat = Boolean(ev.stationary ?? ev.data?.provenance?.stationary);
      if (stationaryFilter === "moving" && isStat) return false;
      if (stationaryFilter === "stationary" && !isStat) return false;
      if (!matchesLicensePlateFilter(ev, plateFilter)) return false;
      return true;
    });
  }, [
    filteredEvents,
    excludeLabel,
    zoneFilter,
    minScoreFilter,
    stationaryFilter,
    plateFilter,
  ]);

  const handleSaveExplorePreset = () => {
    const name =
      presetName.trim() ||
      `${exploreCamera === "all" ? "ALL" : exploreCamera.toUpperCase()} · ${exploreLabel.toUpperCase()}`;
    const next = [
      ...savedPresets.filter((p) => p.name !== name),
      {
        name,
        camera: exploreCamera,
        label: exploreLabel,
        excludeLabel,
        zone: zoneFilter,
        minScore: minScoreFilter,
        stationary: stationaryFilter,
        retainedOnly: exploreRetainedOnly,
      },
    ];
    setSavedPresets(next);
    window.localStorage?.setItem("ninjanvr_explore_saved_filters", JSON.stringify(next));
    setPresetName("");
  };

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <input
            className="nvr-input"
            style={{ width: 240 }}
            aria-label="Filter tracked objects"
            placeholder={t.explore.searchPlaceholder}
            value={exploreQuery}
            onChange={(e) => setExploreQuery(e.target.value)}
          />
          <input
            className="nvr-input mono"
            style={{ width: 135, fontFamily: "var(--font-mono)" }}
            aria-label="Filter events by license plate"
            placeholder="Plate (ABC1D23)..."
            value={plateFilter}
            onChange={(e) => setPlateFilter(e.target.value.toUpperCase())}
          />
          <select
            className="nvr-select"
            aria-label="Filter objects by camera"
            value={exploreCamera}
            onChange={(e) => setExploreCamera(e.target.value)}
          >
            <option value="all">{t.live.allCameras}</option>
            {cameraNames.map((c) => (
              <option key={c} value={c}>
                {c.toUpperCase()}
              </option>
            ))}
          </select>
          <select
            className="nvr-select"
            aria-label="Filter objects by label"
            value={exploreLabel}
            onChange={(e) => setExploreLabel(e.target.value)}
          >
            <option value="all">{t.explore.allLabels}</option>
            <option value="person">PERSON</option>
            <option value="cat">CAT</option>
            <option value="dog">DOG</option>
            <option value="car">CAR</option>
            <option value="audio">AUDIO</option>
          </select>
          <select
            className="nvr-select mono"
            aria-label="Exclude objects by label (NOT)"
            value={excludeLabel}
            onChange={(e) => setExcludeLabel(e.target.value)}
          >
            <option value="none">NOT: NONE</option>
            <option value="cat">NOT: CAT</option>
            <option value="dog">NOT: DOG</option>
            <option value="car">NOT: CAR</option>
            <option value="person">NOT: PERSON</option>
          </select>
          <select
            className="nvr-select mono"
            aria-label="Filter objects by zone"
            value={zoneFilter}
            onChange={(e) => setZoneFilter(e.target.value)}
          >
            <option value="all">ALL ZONES</option>
            {availableZones.map((z) => (
              <option key={z} value={z}>
                ZONE: {z.toUpperCase()}
              </option>
            ))}
          </select>
          <select
            className="nvr-select mono"
            aria-label="Filter by minimum confidence score"
            value={minScoreFilter}
            onChange={(e) => setMinScoreFilter(Number(e.target.value))}
          >
            <option value={0}>MIN SCORE: ANY</option>
            <option value={70}>&ge; 70%</option>
            <option value={80}>&ge; 80%</option>
            <option value={90}>&ge; 90%</option>
          </select>
          <select
            className="nvr-select mono"
            aria-label="Filter by stationary state"
            value={stationaryFilter}
            onChange={(e) =>
              setStationaryFilter(e.target.value as "all" | "moving" | "stationary")
            }
          >
            <option value="all">ALL MOTION</option>
            <option value="moving">MOVING ONLY</option>
            <option value="stationary">STATIONARY ONLY</option>
          </select>
          <button
            className={`nvr-btn ${exploreRetainedOnly ? "active" : ""}`}
            onClick={() => setExploreRetainedOnly((v) => !v)}
          >
            {t.explore.favoritesOnly}
          </button>
          <RecordingCalendarPopover
            daysSummary={globalDaysSummary}
            selectedDate={reviewDateFilter}
            showTimePicker={false}
            allowClear={true}
            openUpward={false}
            buttonLabelPrefix="Date"
            onSelectDate={(dateStr) => setReviewDateFilter(dateStr)}
            onClear={() => setReviewDateFilter("")}
          />
          <input
            className="nvr-input mono"
            style={{ width: 110, height: 26, fontSize: "var(--text-xs)" }}
            placeholder="Preset name..."
            aria-label="Explore filter preset name"
            value={presetName}
            onChange={(e) => setPresetName(e.target.value)}
          />
          <button
            type="button"
            className="nvr-btn mono"
            style={{ height: 26, padding: "0 7px", fontSize: "var(--text-xs)" }}
            onClick={handleSaveExplorePreset}
          >
            + Save Preset
          </button>
          {savedPresets.map((p) => (
            <span key={p.name} style={{ display: "inline-flex", alignItems: "center", gap: 2 }}>
              <button
                type="button"
                className="nvr-btn mono"
                style={{ height: 24, padding: "0 6px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  setExploreCamera(p.camera);
                  setExploreLabel(p.label);
                  setExcludeLabel(p.excludeLabel || "none");
                  setZoneFilter(p.zone || "all");
                  setMinScoreFilter(p.minScore || 0);
                  setStationaryFilter(p.stationary || "all");
                  setExploreRetainedOnly(Boolean(p.retainedOnly));
                }}
              >
                {p.name}
              </button>
              <button
                type="button"
                className="nvr-btn danger mono"
                style={{ height: 24, padding: "0 4px", fontSize: "var(--text-xs)" }}
                title={`Delete preset ${p.name}`}
                onClick={() => {
                  const next = savedPresets.filter((x) => x.name !== p.name);
                  setSavedPresets(next);
                  window.localStorage?.setItem(
                    "ninjanvr_explore_saved_filters",
                    JSON.stringify(next)
                  );
                }}
              >
                ×
              </button>
            </span>
          ))}
        </div>
        <span
          className="mono"
          style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
        >
          {visibleEvents.length} tracked object events
        </span>
      </div>

      <div className="nvr-workbench">
        <div className="nvr-panel">
          <div className="nvr-panel-header" style={{ gap: 6, flexWrap: "wrap" }}>
            <span>TRACKED OBJECTS</span>
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              <button
                type="button"
                className="nvr-btn mono sm"
                style={{ height: 20, padding: "0 6px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  const pageIds = visibleEvents.slice(0, 50).map((e) => e.id);
                  setSelectedBatchEventIds((prev) =>
                    prev.length === pageIds.length ? [] : pageIds
                  );
                }}
              >
                {selectedBatchEventIds.length > 0
                  ? `Clear (${selectedBatchEventIds.length})`
                  : "Select Page"}
              </button>
              <span className="mono">TOP 50</span>
            </div>
          </div>
          {selectedBatchEventIds.length > 0 && (
            <div
              style={{
                padding: "6px 8px",
                background: "var(--bg-card-header)",
                borderBottom: "1px solid var(--border-hairline)",
                display: "flex",
                flexDirection: "column",
                gap: 6,
              }}
            >
              <div
                className="mono"
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  alignItems: "center",
                  fontSize: "var(--text-xs)",
                  color: "var(--signal-accent-text)",
                  fontWeight: 700,
                }}
              >
                <span>BATCH FACE ID ({selectedBatchEventIds.length} SELECTED)</span>
                {batchFaceProgress?.active && (
                  <span>
                    {batchFaceProgress.completed}/{batchFaceProgress.total}
                  </span>
                )}
              </div>
              <div style={{ display: "flex", gap: 4, flexWrap: "wrap", alignItems: "center" }}>
                <input
                  className="nvr-input mono"
                  style={{ flex: 1, minWidth: 95, height: 24, fontSize: "var(--text-xs)" }}
                  placeholder="Name (e.g. Erik)..."
                  value={faceNameDraft}
                  onChange={(e) => setFaceNameDraft(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn primary mono sm"
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  disabled={Boolean(batchFaceProgress?.active)}
                  onClick={() => void handleBatchAssignFace()}
                >
                  {batchFaceProgress?.active
                    ? `${batchFaceProgress.completed}/${batchFaceProgress.total}...`
                    : `Assign & Enroll (${selectedBatchEventIds.length})`}
                </button>
              </div>
              {enrolledFaces.length > 0 && (
                <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
                  {enrolledFaces.map((p) => (
                    <button
                      key={`batch-chip-${p.name}`}
                      type="button"
                      className="nvr-btn mono sm"
                      style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                      disabled={Boolean(batchFaceProgress?.active)}
                      onClick={() => void handleBatchAssignFace(p.name)}
                    >
                      + {p.name} ({p.sample_count})
                    </button>
                  ))}
                </div>
              )}
            </div>
          )}
          <div className="nvr-scroll-y">
            {visibleEvents.slice(0, 50).map((ev) => {
              const topScore = Math.round(resolveFiniteEventScore(ev, 0.85) * 100);
              const isSelected = selectedEvent?.id === ev.id;
              const isBatchChecked = selectedBatchEventIds.includes(ev.id);
              const matchedExport = findMatchingExport(
                exportsList,
                ev.camera,
                ev.start_time,
                ev.end_time
              );
              return (
                <div
                  key={ev.id}
                  role="button"
                  tabIndex={0}
                  className={`nvr-list-row ${isSelected ? "is-selected" : ""}`}
                  onClick={() => setSelectedEvent(ev)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      setSelectedEvent(ev);
                    }
                  }}
                >
                  <input
                    type="checkbox"
                    aria-label={`Select event ${ev.id} for batch face assignment`}
                    checked={isBatchChecked}
                    onClick={(e) => e.stopPropagation()}
                    onChange={(e) => {
                      const checked = e.target.checked;
                      setSelectedBatchEventIds((prev) =>
                        checked
                          ? prev.includes(ev.id)
                            ? prev
                            : [...prev, ev.id]
                          : prev.filter((id) => id !== ev.id)
                      );
                    }}
                    style={{ cursor: "pointer", flexShrink: 0 }}
                  />
                  <img
                    src={`/api/events/${encodeURIComponent(ev.id)}/thumbnail.webp`}
                    alt={ev.label}
                    width={80}
                    height={46}
                    loading="lazy"
                    style={{
                      width: 80,
                      height: 46,
                      objectFit: "contain",
                      background: "var(--video-stage-bg)",
                      borderRadius: 3,
                      border: "1px solid var(--border-hairline)",
                    }}
                  />
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: 6,
                        whiteSpace: "nowrap",
                      }}
                    >
                      <span
                        className="mono"
                        style={{ fontWeight: 700, textTransform: "uppercase" }}
                      >
                        {formatEventLabelWithSubLabel(
                          ev.label,
                          ev.sub_label,
                          ev.data?.detected_attributes
                        )}{" "}
                        · {topScore}%
                      </span>
                      {(() => {
                        const subBadge = formatSubLabelBadge(
                          ev.sub_label,
                          ev.sub_label_score ?? ev.data?.sub_label_score
                        );
                        if (!subBadge) return null;
                        return (
                          <span
                            className={`nvr-badge mono ${
                              subBadge.isUnknown ? "alert" : "live"
                            }`}
                            style={{
                              height: 18,
                              padding: "0 5px",
                              fontSize: "var(--text-xs)",
                            }}
                          >
                            {subBadge.text}
                          </span>
                        );
                      })()}
                      <span
                        className="mono"
                        style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                      >
                        {ev.camera.toUpperCase()}
                      </span>
                    </div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-muted)",
                        marginTop: 3,
                        whiteSpace: "nowrap",
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: 6,
                      }}
                    >
                      <span>
                        {formatDateTime(ev.start_time)} ·{" "}
                        {typeof ev.end_time === "number" &&
                        Number.isFinite(ev.end_time) &&
                        ev.end_time > 0 &&
                        typeof ev.start_time === "number" &&
                        Number.isFinite(ev.start_time)
                          ? `${Math.max(1, Math.round(ev.end_time - ev.start_time))}s`
                          : t.explore.inProgress}
                      </span>
                      <span style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                        {ev.data?.recognized_license_plate && (
                          <span
                            className="nvr-badge mono"
                            style={{
                              height: 19,
                              padding: "0 5px",
                              fontFamily: "var(--font-mono)",
                              fontSize: "var(--text-xs)",
                              color: "var(--signal-accent-text)",
                            }}
                          >
                            {ev.data.recognized_license_plate}
                          </span>
                        )}
                      {matchedExport && (
                        <button
                          className="nvr-badge live mono"
                          style={{
                            height: 19,
                            padding: "0 5px",
                            fontSize: "var(--text-xs)",
                            cursor: "pointer",
                          }}
                          title={`Open exported clip ${matchedExport.name}`}
                          onClick={(e) => {
                            e.stopPropagation();
                            if (onOpenExport) {
                              onOpenExport(matchedExport);
                            } else {
                              switchTab("exports");
                            }
                          }}
                        >
                          EXPORTED
                        </button>
                      )}
                      </span>
                    </div>
                  </div>
                </div>
              );
            })}
            {visibleEvents.length === 0 && (
              <div
                style={{
                  padding: 24,
                  textAlign: "center",
                  color: "var(--text-muted)",
                }}
              >
                No tracked object events match the active filter criteria.
              </div>
            )}
          </div>
        </div>

        <div className="nvr-focus-stage">
          {selectedEvent ? (
            <>
              <div className="nvr-subbar">
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    minWidth: 0,
                    whiteSpace: "nowrap",
                  }}
                >
                  <span className="nvr-badge alert">
                    {formatEventLabelWithSubLabel(
                      selectedEvent.label,
                      selectedEvent.sub_label,
                      selectedEvent.data?.detected_attributes
                    )}{" "}
                    {Math.round(resolveFiniteEventScore(selectedEvent, 0.88) * 100)}
                    %
                  </span>
                  {(() => {
                    const subBadge = formatSubLabelBadge(
                      selectedEvent.sub_label,
                      selectedEvent.sub_label_score ??
                        selectedEvent.data?.sub_label_score
                    );
                    if (!subBadge) return null;
                    return (
                      <span
                        className={`nvr-badge mono ${
                          subBadge.isUnknown ? "alert" : "live"
                        }`}
                      >
                        {subBadge.text}
                      </span>
                    );
                  })()}
                  {(selectedEvent.data?.detected_attributes || []).map((attr, idx) => (
                    <span
                      key={`${attr.label}-${idx}`}
                      className="nvr-badge mono"
                      style={{
                        fontFamily: "var(--font-mono)",
                        fontSize: "var(--text-xs)",
                        color: "var(--signal-accent-text)",
                      }}
                    >
                      {formatDetectedAttributePill(attr)}
                    </span>
                  ))}
                  {selectedEvent.data?.recognized_license_plate && (
                    <span
                      className="nvr-badge mono"
                      style={{
                        fontFamily: "var(--font-mono)",
                        fontSize: "var(--text-xs)",
                        color: "var(--signal-accent-text)",
                        fontWeight: 700,
                      }}
                    >
                      PLATE:{" "}
                      {formatLicensePlateBadge(
                        selectedEvent.data.recognized_license_plate,
                        null,
                        selectedEvent.sub_label
                      )}
                    </span>
                  )}
                  <span
                    className="mono"
                    title={selectedEvent.id}
                    style={{
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      color: "var(--text-secondary)",
                    }}
                  >
                    {selectedEvent.camera.toUpperCase()} · #{selectedEvent.id.slice(-8)}
                  </span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexShrink: 0 }}>
                  <button
                    className={`nvr-btn ${eventViewMode === "snapshot" ? "active" : ""}`}
                    onClick={() => setEventViewMode("snapshot")}
                  >
                    <Camera size={12} />
                    {t.explore.viewSnapshot}
                  </button>
                  <button
                    className={`nvr-btn ${eventViewMode === "clip" ? "active" : ""}`}
                    onClick={() => setEventViewMode("clip")}
                  >
                    <Play size={12} />
                    {t.explore.playClip}
                  </button>
                  <button
                    className={`nvr-btn ${selectedEvent.retain_indefinitely ? "active" : ""}`}
                    onClick={() => handleEventRetain(selectedEvent)}
                  >
                    {t.explore.retainToggle}
                  </button>
                  <button
                    className={`nvr-btn ${selectedEvent.false_positive ? "active danger" : ""}`}
                    onClick={handleToggleFalsePositive}
                    title="Toggle false-positive memory suppression signature (spatial IoU + dhash64)"
                  >
                    {selectedEvent.false_positive
                      ? "False Positive ✓"
                      : "Mark False Positive"}
                  </button>
                  <button
                    className="nvr-btn mono"
                    disabled={aiVerifyBusy}
                    onClick={handleVerifyWithAi}
                    title="Verify this detection snapshot using the configured Vision AI Judge (Gemini / OpenAI / Ollama)"
                  >
                    <Sparkles size={12} />
                    {aiVerifyBusy ? "Verifying..." : "Verify with AI"}
                  </button>
                  <button
                    className={`nvr-btn mono ${addedDatasetIds[selectedEvent.id] ? "active" : ""}`}
                    disabled={datasetAddBusy}
                    onClick={handleAddToDataset}
                    title="Add clean event frame and normalized YOLO bounding box to Active Learning Dataset Studio"
                  >
                    {datasetAddBusy
                      ? "Adding..."
                      : addedDatasetIds[selectedEvent.id]
                      ? "Added to Dataset ✓"
                      : "Add to Training Dataset"}
                  </button>
                  <button
                    className="nvr-btn"
                    onClick={() => {
                      const safeTs =
                        typeof selectedEvent.start_time === "number" &&
                        Number.isFinite(selectedEvent.start_time) &&
                        selectedEvent.start_time > 0
                          ? Math.floor(selectedEvent.start_time)
                          : Math.floor(Date.now() / 1000) - 20;
                      setSyncMasterTs(safeTs);
                      switchTab("sync");
                    }}
                  >
                    4-Cam Sync
                  </button>
                  <button
                    className={`nvr-btn mono ${showForensic ? "active" : ""}`}
                    onClick={() => setShowForensic((v) => !v)}
                    title="Inspect exact score, threshold, pixel area, aspect ratio, and zone provenance"
                  >
                    Explain Alert
                  </button>
                  <button
                    className={`nvr-btn mono ${showDebugReplay ? "active" : ""}`}
                    onClick={() => {
                      const next = !showDebugReplay;
                      setShowDebugReplay(next);
                      if (next && !debugTrace) {
                        void handleRunDebugReplay();
                      }
                    }}
                    title="Re-run recorded clip through motion + AI detector pipeline with custom thresholds"
                  >
                    Debug Replay
                  </button>
                  <button
                    className="nvr-btn danger"
                    aria-label="Delete tracked object event"
                    title="Delete tracked object event"
                    onClick={() => handleEventDelete(selectedEvent.id)}
                  >
                    <Trash2 size={12} />
                  </button>
                </div>
              </div>

              {(selectedEvent.data?.ai_verification || selectedEvent.false_positive) && (
                <div
                  className="mono"
                  style={{
                    background: "var(--bg-subcard)",
                    borderBottom: "1px solid var(--border-strong)",
                    padding: "7px 12px",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    flexWrap: "wrap",
                    gap: 10,
                    fontSize: "var(--text-xs)",
                    fontVariantNumeric: "tabular-nums",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
                    {selectedEvent.data?.ai_verification && (
                      <>
                        <span
                          className={`nvr-badge mono ${
                            selectedEvent.data.ai_verification.is_true_positive
                              ? "live"
                              : "alert"
                          }`}
                        >
                          {formatAiVerificationBadge(selectedEvent.data.ai_verification)}
                        </span>
                        <span style={{ color: "var(--text-primary)" }}>
                          Reason:{" "}
                          <strong>{selectedEvent.data.ai_verification.reason}</strong>
                        </span>
                        {selectedEvent.data.ai_verification.model && (
                          <span style={{ color: "var(--text-secondary)" }}>
                            Model:{" "}
                            <strong>
                              {(
                                selectedEvent.data.ai_verification.provider || "genai"
                              ).toUpperCase()}{" "}
                              / {selectedEvent.data.ai_verification.model}
                            </strong>
                          </span>
                        )}
                      </>
                    )}
                    {selectedEvent.false_positive && (
                      <span className="nvr-badge alert mono">
                        SUPPRESSED IN FALSE-POSITIVE MEMORY
                      </span>
                    )}
                  </div>
                </div>
              )}

              {showForensic && (
                <div
                  className="mono"
                  style={{
                    background: "var(--bg-subcard)",
                    borderBottom: "1px solid var(--border-strong)",
                    padding: "8px 12px",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    flexWrap: "wrap",
                    gap: 12,
                    fontSize: "var(--text-xs)",
                    fontVariantNumeric: "tabular-nums",
                  }}
                >
                  {(() => {
                    const p = selectedEvent.data?.provenance;
                    const topScore =
                      typeof p?.top_score === "number" && Number.isFinite(p.top_score)
                        ? p.top_score
                        : resolveFiniteEventScore(selectedEvent, 0.85);
                    const thresh =
                      typeof p?.threshold_cfg === "number" &&
                      Number.isFinite(p.threshold_cfg)
                        ? p.threshold_cfg
                        : 0.7;
                    const rawAreaPx =
                      typeof p?.area_px === "number" && Number.isFinite(p.area_px)
                        ? p.area_px
                        : typeof selectedEvent.area === "number" &&
                          Number.isFinite(selectedEvent.area)
                        ? selectedEvent.area
                        : 0;
                    const areaPx = Math.max(0, Math.round(rawAreaPx));
                    const areaPct =
                      typeof p?.area_pct === "number" && Number.isFinite(p.area_pct)
                        ? Math.max(0, p.area_pct)
                        : 0;
                    const ratio =
                      typeof p?.aspect_ratio === "number" &&
                      Number.isFinite(p.aspect_ratio)
                        ? p.aspect_ratio
                        : typeof selectedEvent.ratio === "number" &&
                          Number.isFinite(selectedEvent.ratio) &&
                          selectedEvent.ratio > 0
                        ? Math.round(selectedEvent.ratio * 100) / 100
                        : 0.5;
                    const bbox =
                      Array.isArray(p?.bbox) && p.bbox.length >= 4
                        ? p.bbox.map((v) =>
                            typeof v === "number" && Number.isFinite(v) ? v : 0
                          )
                        : [0, 0, 0, 0];
                    const anchorX = Math.round((bbox[0] + bbox[2]) / 2);
                    const anchorY = Math.round(bbox[3]);
                    const zones =
                      (p?.zones && p.zones.length > 0 ? p.zones : selectedEvent.zones) || [];
                    const fallbackStart =
                      typeof selectedEvent.start_time === "number" &&
                      Number.isFinite(selectedEvent.start_time) &&
                      selectedEvent.start_time > 0
                        ? selectedEvent.start_time
                        : Math.floor(Date.now() / 1000) - 20;
                    const triggerTs =
                      typeof p?.trigger_ts === "number" &&
                      Number.isFinite(p.trigger_ts) &&
                      p.trigger_ts > 0
                        ? p.trigger_ts
                        : fallbackStart;
                    return (
                      <>
                        <div style={{ display: "flex", alignItems: "center", gap: 14, flexWrap: "wrap" }}>
                          <span style={{ color: "var(--signal-accent-text)", fontWeight: 700 }}>
                            FORENSIC TRIGGER BREAKDOWN:
                          </span>
                          <span>
                            Score vs Threshold:{" "}
                            <strong>
                              {(topScore * 100).toFixed(1)}% &ge; {(thresh * 100).toFixed(0)}%
                            </strong>
                          </span>
                          <span>
                            Box Area:{" "}
                            <strong>
                              {areaPx} px² ({areaPct.toFixed(2)}%)
                            </strong>
                          </span>
                          <span>
                            Aspect Ratio (W/H): <strong>{ratio.toFixed(2)}</strong>
                          </span>
                          <span>
                            Bottom-Center Anchor:{" "}
                            <strong>
                              ({anchorX}, {anchorY})
                            </strong>
                          </span>
                          <span>
                            Matched Zones:{" "}
                            <strong>{zones.length > 0 ? zones.join(", ") : "NONE"}</strong>
                          </span>
                          {selectedEvent.sub_label && (
                            <span>
                              Sub-Label:{" "}
                              <strong>{selectedEvent.sub_label.toUpperCase()}</strong>
                            </span>
                          )}
                          {selectedEvent.data?.detected_attributes &&
                            selectedEvent.data.detected_attributes.length > 0 && (
                              <span
                                style={{
                                  display: "inline-flex",
                                  alignItems: "center",
                                  gap: 6,
                                  flexWrap: "wrap",
                                  fontFamily: "var(--font-mono)",
                                }}
                              >
                                Child Attributes:{" "}
                                {selectedEvent.data.detected_attributes.map((attr, idx) => (
                                  <span
                                    key={`${attr.label}-${idx}`}
                                    className="nvr-badge mono"
                                    style={{
                                      fontFamily: "var(--font-mono)",
                                      fontSize: "var(--text-xs)",
                                    }}
                                  >
                                    {formatDetectedAttributePill(attr)} [
                                    {(attr.norm_bbox || [0, 0, 0, 0])
                                      .map((v) => {
                                        const n = Number(v);
                                        return (Number.isFinite(n) ? n : 0).toFixed(2);
                                      })
                                      .join(", ")}
                                    ]
                                  </span>
                                ))}
                              </span>
                            )}
                        </div>
                        <button
                          type="button"
                          className="nvr-btn mono"
                          style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                          onClick={() => {
                            setEventViewMode("clip");
                            setSyncMasterTs(Math.floor(triggerTs));
                          }}
                        >
                          Jump to Trigger Frame
                        </button>
                      </>
                    );
                  })()}
                </div>
              )}

              {showDebugReplay && (
                <div
                  className="mono"
                  style={{
                    background: "var(--bg-subcard)",
                    borderBottom: "1px solid var(--border-strong)",
                    padding: "8px 12px",
                    display: "flex",
                    flexDirection: "column",
                    gap: 8,
                    fontSize: "var(--text-xs)",
                    fontVariantNumeric: "tabular-nums",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      flexWrap: "wrap",
                      gap: 10,
                    }}
                  >
                    <strong style={{ color: "var(--signal-accent-text)" }}>
                      RETROACTIVE DETECTION DEBUGGER // RE-RUN PIPELINE TRACE
                    </strong>
                    <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                      <label style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                        Override Min Score:
                        <input
                          type="number"
                          step="0.05"
                          min="0.1"
                          max="0.99"
                          className="nvr-input mono"
                          style={{ width: 64, height: 22 }}
                          value={debugMinScore}
                          onChange={(e) => setDebugMinScore(Number(e.target.value))}
                        />
                      </label>
                      <label style={{ display: "inline-flex", alignItems: "center", gap: 4 }}>
                        Override Min Area:
                        <input
                          type="number"
                          step="100"
                          min="0"
                          className="nvr-input mono"
                          style={{ width: 76, height: 22 }}
                          value={debugMinArea}
                          onChange={(e) => setDebugMinArea(Number(e.target.value))}
                        />
                      </label>
                      <button
                        type="button"
                        className="nvr-btn primary mono"
                        style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                        disabled={debugBusy}
                        onClick={handleRunDebugReplay}
                      >
                        {debugBusy ? "Reprocessing..." : "Re-run Detection"}
                      </button>
                    </div>
                  </div>
                  {debugTrace && (
                    <div
                      style={{
                        display: "flex",
                        flexDirection: "column",
                        gap: 4,
                        maxHeight: 110,
                        overflowY: "auto",
                      }}
                    >
                      {(Array.isArray(debugTrace.frames) ? debugTrace.frames : []).map((f, idx) => (
                        <div
                          key={`${f.ts}-${idx}`}
                          style={{
                            display: "flex",
                            alignItems: "center",
                            gap: 12,
                            flexWrap: "wrap",
                            padding: "2px 0",
                            borderTop: "1px solid var(--border-hairline)",
                          }}
                        >
                          <span>
                            TS: {(typeof f.ts === "number" && Number.isFinite(f.ts) ? f.ts : 0).toFixed(1)}s
                          </span>
                          <span>Motion Boxes: {(f.motion_boxes || []).length}</span>
                          <span style={{ color: "var(--signal-live-text)" }}>
                            Confirmed Tracks:{" "}
                            {(f.active_tracks || [])
                              .map(
                                (t) =>
                                  `${t.label} (${Math.round(
                                    (typeof t.score === "number" && Number.isFinite(t.score)
                                      ? t.score
                                      : 0) * 100
                                  )}%, ${
                                    typeof t.area_px === "number" && Number.isFinite(t.area_px)
                                      ? Math.round(t.area_px)
                                      : 0
                                  }px²)`
                              )
                              .join(", ") || "NONE"}
                          </span>
                          <span style={{ color: "var(--signal-alert-text)" }}>
                            Filtered/Rejected:{" "}
                            {(f.filtered_reasons || [])
                              .map(
                                (r) =>
                                  `${r.label} (${Math.round(
                                    (typeof r.score === "number" && Number.isFinite(r.score)
                                      ? r.score
                                      : 0) * 100
                                  )}% → ${r.reason})`
                              )
                              .join(", ") || "NONE"}
                          </span>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              )}

              <div className="nvr-cam-tile" style={{ flex: 1 }}>
                {eventViewMode === "snapshot" ? (
                  <img
                    src={`/api/events/${encodeURIComponent(selectedEvent.id)}/snapshot.jpg?bbox=1`}
                    alt={selectedEvent.id}
                    style={{
                      maxWidth: "100%",
                      maxHeight: "100%",
                      objectFit: "contain",
                    }}
                  />
                ) : (
                  (() => {
                    const rawMock = String(
                      (selectedEvent.data as Record<string, unknown> | undefined)
                        ?.mock_video || ""
                    ).trim();
                    const safeMock =
                      rawMock &&
                      rawMock !== "." &&
                      !rawMock.includes("..") &&
                      !/[/\\%\u0000-\u001f\u007f]/.test(rawMock)
                        ? rawMock
                        : "";
                    return (
                      <video
                        key={selectedEvent.id}
                        src={
                          safeMock
                            ? `/clips/mocks/${encodeURIComponent(safeMock)}`
                            : `/api/events/${encodeURIComponent(selectedEvent.id)}/clip.mp4`
                        }
                        controls
                        autoPlay
                        muted
                        playsInline
                        style={{ width: "100%", height: "100%", objectFit: "contain" }}
                      />
                    );
                  })()
                )}
              </div>

              <div
                style={{
                  padding: "6px 10px",
                  background: "var(--bg-panel)",
                  borderTop: "1px solid var(--border-hairline)",
                  display: "flex",
                  alignItems: "center",
                  flexWrap: "wrap",
                  gap: 8,
                }}
              >
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  FACE ID / SUB-LABEL:
                </span>
                <input
                  className="nvr-input mono"
                  style={{ width: 150, height: 26, fontSize: "var(--text-xs)" }}
                  aria-label="Assign Face Name"
                  placeholder="Person name (e.g. Bruno)..."
                  value={faceNameDraft}
                  onChange={(e) => setFaceNameDraft(e.target.value)}
                />
                {enrolledFaces.length > 0 && (
                  <select
                    className="nvr-select mono"
                    style={{ height: 26, fontSize: "var(--text-xs)" }}
                    aria-label="Pick enrolled face profile"
                    value=""
                    onChange={(e) => {
                      const val = e.target.value;
                      if (val) {
                        setFaceNameDraft(val);
                        void handleAssignFaceSubLabel(val);
                      }
                    }}
                  >
                    <option value="">Quick Assign Profile...</option>
                    {enrolledFaces.map((p) => (
                      <option key={p.name} value={p.name}>
                        {p.name} ({p.sample_count})
                      </option>
                    ))}
                  </select>
                )}
                <button
                  type="button"
                  className="nvr-btn primary mono"
                  style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                  onClick={() => void handleAssignFaceSubLabel()}
                  title="Assigns person name to event and automatically enrolls 112x112 face crop into Face Gallery"
                >
                  Assign &amp; Enroll Face
                </button>
                {enrolledFaces.map((p) => (
                  <button
                    key={`quick-face-${p.name}`}
                    type="button"
                    className={`nvr-btn mono ${
                      (selectedEvent.sub_label || "").trim().toLowerCase() ===
                      p.name.trim().toLowerCase()
                        ? "active"
                        : ""
                    }`}
                    style={{ height: 26, padding: "0 8px", fontSize: "var(--text-xs)" }}
                    onClick={() => {
                      setFaceNameDraft(p.name);
                      void handleAssignFaceSubLabel(p.name);
                    }}
                    title={`1-click assign & enroll face sample into '${p.name}' gallery (${p.sample_count} samples)`}
                  >
                    + {p.name} ({p.sample_count})
                  </button>
                ))}
                {(selectedEvent.sub_label || selectedEvent.data?.sub_label) && (
                  <button
                    type="button"
                    className="nvr-btn danger mono"
                    style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                    aria-label="Clear or unassign face sub-label"
                    onClick={() => {
                      setFaceNameDraft("");
                      void handleAssignFaceSubLabel(null);
                    }}
                  >
                    Clear / Unassign Face
                  </button>
                )}
                <span className="nvr-divider" aria-hidden="true" />
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  LICENSE PLATE:
                </span>
                <input
                  className="nvr-input mono"
                  aria-label="Manual license plate override"
                  style={{
                    width: 120,
                    height: 26,
                    fontSize: "var(--text-xs)",
                    fontFamily: "var(--font-mono)",
                  }}
                  placeholder="ABC1D23"
                  value={eventPlateDraft}
                  onChange={(e) => setEventPlateDraft(e.target.value.toUpperCase())}
                />
                <button
                  type="button"
                  className="nvr-btn mono"
                  style={{ height: 26, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={handleSaveEventLicensePlate}
                >
                  Save Plate
                </button>
                <span className="nvr-divider" aria-hidden="true" />
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  FORENSIC NOTE:
                </span>
                <input
                  className="nvr-input mono"
                  style={{ flex: 1, minWidth: 160, height: 26, fontSize: "var(--text-xs)" }}
                  placeholder="Attach operator annotation (searchable in Explore)..."
                  value={eventNoteDraft}
                  onChange={(e) => setEventNoteDraft(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn primary mono"
                  style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                  onClick={handleSaveEventNote}
                >
                  Save Note
                </button>
              </div>
            </>
          ) : (
            <div
              style={{
                flex: 1,
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                color: "var(--text-muted)",
              }}
            >
              Select a tracked object event on the left
            </div>
          )}
        </div>
      </div>
    </>
  );
};
