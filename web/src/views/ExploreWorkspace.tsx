import React from "react";
import { Camera, Play, Sparkles, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import {
  AiVerificationResult,
  EnrolledFaceProfileItem,
  EventItem,
  ExportItem,
  RecordingDaySummary,
} from "../types/nvr";
import { WorkspaceTab, findMatchingExport, formatDateTime } from "../utils/format";
import { RecordingCalendarPopover } from "../components/RecordingCalendarPopover";
import { useCompactViewport } from "../utils/useCompactViewport";
import { countActiveExploreFilters } from "../utils/liveTileHud";
import { resolveSafeMediaUrl } from "../utils/mediaUrlPolicy";
import {
  EventMutationPatch,
  buildEventsFeedUrl,
  matchesOperationalDateBounds,
  parseOperationalDateBoundsSec,
  reconcileDeletedFaceProfile,
  reconcileEventMutation,
} from "../utils/operationalFeed";
import { useTwoStepConfirm } from "./settings/vision/useTwoStepConfirm";
import {
  BatchFaceProgress,
  DEFAULT_LPR_FORMAT_REGEX,
  SavedExploreFilterPreset,
  applyEventSubLabelUpdate,
  applyReviewSubLabelUpdate,
  clearFaceProfileFromEvents,
  clearFaceProfileFromReviews,
  createForensicTriageController,
  formatAiVerificationBadge,
  formatDetectedAttributePill,
  formatEventLabelWithSubLabel,
  formatLicensePlateBadge,
  formatSubLabelBadge,
  matchesLicensePlateFilter,
  normalizeFaceProfileName,
  normalizeForensicLicensePlate,
  normalizeLicensePlate,
  parseFacesApiResponse,
  parseSavedExplorePresets,
  resolveFiniteEventScore,
  resolveKnownPlateLabel,
  shouldApplyEventMutationResult,
  validateLicensePlateFormat,
} from "../utils/forensicTriage";

export type {
  BatchFaceProgress,
  SavedExploreFilterPreset,
} from "../utils/forensicTriage";
export {
  DEFAULT_LPR_FORMAT_REGEX,
  applyEventSubLabelUpdate,
  applyReviewSubLabelUpdate,
  clearFaceProfileFromEvents,
  clearFaceProfileFromReviews,
  createForensicTriageController,
  formatAiVerificationBadge,
  formatDetectedAttributePill,
  formatEventLabelWithSubLabel,
  formatLicensePlateBadge,
  formatSubLabelBadge,
  matchesLicensePlateFilter,
  normalizeFaceProfileName,
  normalizeForensicLicensePlate,
  normalizeLicensePlate,
  parseFacesApiResponse,
  parseSavedExplorePresets,
  resolveFiniteEventScore,
  resolveKnownPlateLabel,
  shouldApplyEventMutationResult,
  validateLicensePlateFormat,
};

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
  onReconcileEvents?: (
    eventIds: string | ReadonlyArray<string>,
    patch: EventMutationPatch
  ) => void;
  userRole?: string;
  onLoadOlderEvents?: (
    beforeTs: number,
    filters: {
      excludeLabel?: string;
      zone?: string;
      minScore?: number;
      stationary?: "all" | "moving" | "stationary";
      plate?: string;
    }
  ) => Promise<number>;
}

const EXPLORE_PAGE_STEP = 50;

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
  selectedEvent,
  setSelectedEvent,
  eventViewMode,
  setEventViewMode,
  handleEventRetain,
  handleEventDelete,
  setSyncMasterTs,
  switchTab,
  notify,
  onReconcileEvents,
  userRole = "admin",
  onLoadOlderEvents,
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const isViewer = userRole === "viewer" || userRole === "read_only";
  const isAdmin = userRole === "admin";
  const [loadingOlder, setLoadingOlder] = React.useState(false);
  const [olderExhausted, setOlderExhausted] = React.useState(false);
  const { isArmed, confirmOrArm, clearArmed } = useTwoStepConfirm(3000);
  const [exploreDateFilter, setExploreDateFilter] = React.useState("");
  const [dateScopedEvents, setDateScopedEvents] = React.useState<EventItem[] | null>(
    null
  );
  const dateScopedEventsRef = React.useRef<EventItem[] | null>(dateScopedEvents);
  dateScopedEventsRef.current = dateScopedEvents;
  const [visibleLimit, setVisibleLimit] = React.useState<number>(EXPLORE_PAGE_STEP);
  const [showForensic, setShowForensic] = React.useState(false);
  const [eventNoteDraft, setEventNoteDraft] = React.useState("");
  const [aiVerifyBusy, setAiVerifyBusy] = React.useState(false);
  const [datasetAddBusy, setDatasetAddBusy] = React.useState(false);
  const [addedDatasetIds, setAddedDatasetIds] = React.useState<Record<string, boolean>>({});
  const isMountedRef = React.useRef(true);
  const eventClipVideoRef = React.useRef<HTMLVideoElement | null>(null);
  const pendingClipSeekSecRef = React.useRef<number | null>(null);
  const selectedEventIdRef = React.useRef<string | null>(selectedEvent?.id ?? null);
  selectedEventIdRef.current = selectedEvent?.id ?? null;
  const debugReplayAbortRef = React.useRef<AbortController | null>(null);
  const debugReplaySeqRef = React.useRef(0);
  const triageController = React.useMemo(() => {
    const ctrl = createForensicTriageController();
    ctrl.setActiveTarget("event", selectedEvent?.id ?? null);
    return ctrl;
  }, []);

  const handleAddToDataset = async () => {
    if (!selectedEvent || datasetAddBusy) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    triageController.setActiveTarget("event", targetId);
    setDatasetAddBusy(true);
    let isStale = false;
    try {
      const result = await triageController.addEventToDataset({
        eventId: targetId,
        camera: targetEvent.camera,
        label: targetEvent.label,
        timestamp: targetEvent.start_time,
        autoAnnotate: true,
        getActiveTargetId: () => selectedEventIdRef.current,
        isCancelled: () => !isMountedRef.current,
      });
      isStale = result.stale;
      if (!isMountedRef.current || result.stale) return;
      if (result.ok) {
        const split = result.split || "train";
        setAddedDatasetIds((prev) => ({ ...prev, [targetId]: true }));
        notify(
          result.autoAnnotated
            ? `Added event #${targetId.slice(-8)} & AI-annotated ${result.annotationCount} YOLO boxes (split: ${String(
                split
              ).toUpperCase()})`
            : `Added event #${targetId.slice(-8)} clean frame to Active Learning Dataset (split: ${String(
                split
              ).toUpperCase()})`
        );
      } else {
        notify(result.error || "Failed to add event snapshot to training dataset");
      }
    } catch {
      if (isMountedRef.current && selectedEventIdRef.current === targetId) {
        notify("Network error while adding event snapshot to training dataset");
      }
    } finally {
      if (
        isMountedRef.current &&
        !isStale &&
        selectedEventIdRef.current === targetId
      ) {
        setDatasetAddBusy(false);
      }
    }
  };

  const handleToggleFalsePositive = async () => {
    if (!selectedEvent) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    triageController.setActiveTarget("event", targetId);
    try {
      const result = await triageController.toggleFalsePositive(
        targetId,
        Boolean(targetEvent.false_positive),
        {
          getActiveTargetId: () => selectedEventIdRef.current,
          isCancelled: () => !isMountedRef.current,
        }
      );
      if (!isMountedRef.current || result.stale) return;
      if (result.ok) {
        onReconcileEvents?.(targetId, { falsePositive: result.nextFp });
        if (result.shouldApply && !onReconcileEvents) {
          const reconciled = reconcileEventMutation(
            { events: [], selectedEvent: targetEvent },
            targetId,
            { falsePositive: result.nextFp }
          );
          setSelectedEvent(reconciled.selectedEvent);
        }
        notify(
          result.nextFp
            ? "Marked as False Positive — spatial & dhash64 suppression signature saved"
            : "Cleared False Positive flag & removed suppression signature"
        );
      } else {
        notify(result.message || "Failed to update False Positive status");
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
    triageController.setActiveTarget("event", targetId);
    setAiVerifyBusy(true);
    let isStale = false;
    try {
      const result = await triageController.verifyWithAi(targetId, {
        getActiveTargetId: () => selectedEventIdRef.current,
        isCancelled: () => !isMountedRef.current,
      });
      isStale = result.stale;
      if (!isMountedRef.current || result.stale) return;
      if (result.ok && result.aiVerification) {
        const aiVerdict: AiVerificationResult = result.aiVerification;
        const patch: EventMutationPatch = {
          aiVerification: aiVerdict,
          ...(result.falsePositive !== undefined
            ? { falsePositive: result.falsePositive }
            : {}),
        };
        onReconcileEvents?.(targetId, patch);
        if (result.shouldApply && !onReconcileEvents) {
          const reconciled = reconcileEventMutation(
            { events: [], selectedEvent: targetEvent },
            targetId,
            patch
          );
          setSelectedEvent(reconciled.selectedEvent);
        }
        notify(
          `${formatAiVerificationBadge(aiVerdict)} — ${aiVerdict.reason}`
        );
      } else {
        notify(result.error || "Vision AI Judge verification failed");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Vision AI Judge request failed");
      }
    } finally {
      if (
        isMountedRef.current &&
        !isStale &&
        selectedEventIdRef.current === targetId
      ) {
        setAiVerifyBusy(false);
      }
    }
  };
  const [faceNameDraft, setFaceNameDraft] = React.useState("");
  const [enrolledFaces, setEnrolledFaces] = React.useState<EnrolledFaceProfileItem[]>([]);
  const [selectedBatchEventIds, setSelectedBatchEventIds] = React.useState<string[]>([]);
  const [localEventSubLabels, setLocalEventSubLabels] = React.useState<
    Record<string, string | null>
  >({});
  const [batchFaceProgress, setBatchFaceProgress] =
    React.useState<BatchFaceProgress | null>(null);
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
    pendingClipSeekSecRef.current = null;
    triageController.setActiveTarget("event", selectedEvent?.id ?? null);
    setAiVerifyBusy(false);
    setDatasetAddBusy(false);
    if (debugReplayAbortRef.current) {
      debugReplayAbortRef.current.abort();
      debugReplayAbortRef.current = null;
    }
    debugReplaySeqRef.current += 1;
    setDebugBusy(false);
    setDebugTrace(null);
    clearArmed();
  }, [selectedEvent?.id, triageController, clearArmed]);

  React.useEffect(() => {
    if (
      eventViewMode === "clip" &&
      pendingClipSeekSecRef.current !== null &&
      eventClipVideoRef.current &&
      eventClipVideoRef.current.readyState >= 1
    ) {
      try {
        eventClipVideoRef.current.currentTime = pendingClipSeekSecRef.current;
        pendingClipSeekSecRef.current = null;
      } catch {
        // Applied on loadedmetadata
      }
    }
  }, [eventViewMode, selectedEvent?.id]);

  React.useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
      triageController.invalidatePendingMutations();
      debugReplaySeqRef.current += 1;
      if (debugReplayAbortRef.current) {
        debugReplayAbortRef.current.abort();
        debugReplayAbortRef.current = null;
      }
    };
  }, [triageController]);

  React.useEffect(() => {
    let cancelled = false;
    const controller = new AbortController();
    triageController
      .fetchEnrolledFaces({
        excludeDeletedProfile: lastDeletedFaceProfile,
        signal: controller.signal,
      })
      .then((faces) => {
        if (!cancelled) {
          setEnrolledFaces(faces);
        }
      });
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [triageController, subLabelsRefreshTick, lastDeletedFaceProfile]);

  React.useEffect(() => {
    if (!lastDeletedFaceProfile?.name) return;
    setEnrolledFaces((prev) =>
      reconcileDeletedFaceProfile(
        { enrolledFaces: prev },
        lastDeletedFaceProfile.name
      ).enrolledFaces
    );
    setLocalEventSubLabels((prev) =>
      reconcileDeletedFaceProfile(
        { localSubLabels: prev },
        lastDeletedFaceProfile.name
      ).localSubLabels
    );
    if (selectedEvent) {
      const scrubbed = reconcileDeletedFaceProfile(
        { selectedEvent },
        lastDeletedFaceProfile.name
      );
      if (scrubbed.clearedSelectedEvent) {
        setSelectedEvent(scrubbed.selectedEvent);
        setFaceNameDraft("");
      }
    }
  }, [lastDeletedFaceProfile, selectedEvent, setSelectedEvent]);

  const handleAssignFaceSubLabel = async (overrideName?: string | null) => {
    const raw = overrideName !== undefined ? overrideName : faceNameDraft;
    const clean =
      raw && raw.trim().length > 0
        ? normalizeFaceProfileName(raw) || null
        : null;
    if (raw && raw.trim().length > 0 && !clean) {
      notify("Enter a valid face name (letters, numbers, spaces, _ or -)");
      return;
    }
    if (selectedBatchEventIds.length > 0 && clean) {
      await handleBatchAssignFace(clean);
      return;
    }
    if (selectedBatchEventIds.length > 0 && clean === null) {
      const targetIds = [...selectedBatchEventIds];
      await triageController.batchAssignFaceSubLabel({
        items: targetIds,
        rawName: null,
        isCancelled: () => !isMountedRef.current,
        onItemSuccess: (eid) => {
          setLocalEventSubLabels((prev) => ({ ...prev, [eid]: null }));
          onReconcileEvents?.(eid, { subLabel: null, subLabelScore: null });
        },
      });
      if (!isMountedRef.current) return;
      if (!onReconcileEvents && selectedEvent && targetIds.includes(selectedEvent.id)) {
        const reconciled = reconcileEventMutation(
          { events: [], selectedEvent },
          selectedEvent.id,
          { subLabel: null, subLabelScore: null }
        );
        setSelectedEvent(reconciled.selectedEvent);
      }
      setSelectedBatchEventIds([]);
      bumpSubLabelsRefresh();
      notify(`Cleared face sub-label on ${targetIds.length} selected events`);
      return;
    }
    if (!selectedEvent) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    triageController.setActiveTarget("event", targetId);
    try {
      const result = await triageController.assignFaceSubLabel({
        primaryEventId: targetId,
        rawName: clean,
        targetKind: "event",
        targetId,
        getActiveTargetId: () => selectedEventIdRef.current,
        isCancelled: () => !isMountedRef.current,
      });
      if (!isMountedRef.current) return;
      if (result.ok) {
        setLocalEventSubLabels((prev) => ({
          ...prev,
          [targetId]: result.cleanName,
        }));
        const patch: EventMutationPatch = {
          subLabel: result.cleanName,
          subLabelScore: result.cleanName ? 1.0 : null,
        };
        onReconcileEvents?.(targetId, patch);
        if (result.shouldApply && !onReconcileEvents) {
          const reconciled = reconcileEventMutation(
            { events: [], selectedEvent: targetEvent },
            targetId,
            patch
          );
          setSelectedEvent(reconciled.selectedEvent);
        }
        bumpSubLabelsRefresh();
        notify(
          result.cleanName
            ? `Assigned & enrolled '${result.cleanName}' into Face Gallery${
                result.sampleCount > 0 ? ` (${result.sampleCount} samples)` : ""
              }${
                result.backfilledEvents > 0
                  ? ` · +${result.backfilledEvents} similar events matched`
                  : ""
              }`
            : "Cleared event sub-label"
        );
      } else {
        notify(result.message || "Failed to update event face sub-label");
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
    try {
      const batchResult = await triageController.batchAssignFaceSubLabel({
        items: targetIds,
        rawName: cleanName,
        isCancelled: () => !isMountedRef.current,
        onProgress: (progress) => setBatchFaceProgress(progress),
        onItemSuccess: (eid, assignedName) => {
          setLocalEventSubLabels((prev) => ({ ...prev, [eid]: assignedName }));
          const patch: EventMutationPatch = {
            subLabel: assignedName,
            subLabelScore: assignedName ? 1.0 : null,
          };
          if (onReconcileEvents) {
            onReconcileEvents(eid, patch);
          } else if (selectedEvent && selectedEvent.id === eid) {
            const reconciled = reconcileEventMutation(
              { events: [], selectedEvent },
              eid,
              patch
            );
            setSelectedEvent(reconciled.selectedEvent);
          }
        },
      });
      if (isMountedRef.current) {
        bumpSubLabelsRefresh();
        setSelectedBatchEventIds([]);
        notify(
          `Batch assigned & enrolled '${cleanName}' on ${batchResult.succeededIds.length}/${targetIds.length} events${
            batchResult.totalBackfilled > 0
              ? ` (+${batchResult.totalBackfilled} auto-matched)`
              : ""
          }`
        );
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error during batch face assignment");
      }
    }
  };

  const handleSaveEventLicensePlate = async () => {
    if (!selectedEvent) return;
    const targetEvent = selectedEvent;
    const targetId = targetEvent.id;
    triageController.setActiveTarget("event", targetId);
    try {
      const result = await triageController.saveLicensePlate({
        eventId: targetId,
        rawPlate: eventPlateDraft,
        targetKind: "event",
        targetId,
        getActiveTargetId: () => selectedEventIdRef.current,
        isCancelled: () => !isMountedRef.current,
      });
      if (!isMountedRef.current || result.stale) return;
      if (result.ok) {
        const nextPlate = result.normalizedPlate;
        const nextSubLabel =
          result.subLabel !== undefined
            ? result.subLabel
            : targetEvent.sub_label;
        const patch: EventMutationPatch = {
          recognizedLicensePlate: nextPlate,
          ...(result.subLabel !== undefined ? { subLabel: nextSubLabel } : {}),
        };
        onReconcileEvents?.(targetId, patch);
        if (result.shouldApply) {
          if (!onReconcileEvents) {
            const reconciled = reconcileEventMutation(
              { events: [], selectedEvent: targetEvent },
              targetId,
              patch
            );
            setSelectedEvent(reconciled.selectedEvent);
          }
          setEventPlateDraft(nextPlate || "");
        }
        notify(
          nextPlate
            ? `License plate set to ${nextPlate}`
            : "Cleared recognized license plate"
        );
      } else {
        notify(result.message || "Failed to save recognized license plate");
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
    triageController.setActiveTarget("event", targetId);
    try {
      const result = await triageController.saveForensicNote({
        targetKind: "event",
        targetId,
        notes: noteToSave,
        getActiveTargetId: () => selectedEventIdRef.current,
        isCancelled: () => !isMountedRef.current,
      });
      if (!isMountedRef.current || result.stale) return;
      if (result.ok) {
        onReconcileEvents?.(targetId, { notes: result.notes });
        if (result.shouldApply && !onReconcileEvents) {
          const reconciled = reconcileEventMutation(
            { events: [], selectedEvent: targetEvent },
            targetId,
            { notes: result.notes }
          );
          setSelectedEvent(reconciled.selectedEvent);
        }
        notify("Saved forensic note on event");
      } else {
        notify(result.message || "Failed to save forensic note");
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
  const isCompact = useCompactViewport();
  const [filtersOpen, setFiltersOpen] = React.useState(false);
  const detailStageRef = React.useRef<HTMLDivElement | null>(null);
  const selectedEventIdForScroll = selectedEvent?.id ?? null;
  const lastScrolledEventIdRef = React.useRef<string | null | undefined>(undefined);
  // Phones: list-first master/detail — an operator's selection brings the detail into view.
  React.useEffect(() => {
    const prev = lastScrolledEventIdRef.current;
    lastScrolledEventIdRef.current = selectedEventIdForScroll;
    if (prev === undefined) return;
    if (!isCompact || !selectedEventIdForScroll || prev === selectedEventIdForScroll) return;
    detailStageRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
  }, [isCompact, selectedEventIdForScroll]);
  const [zoneFilter, setZoneFilter] = React.useState("all");
  const [minScoreFilter, setMinScoreFilter] = React.useState(0);
  const [stationaryFilter, setStationaryFilter] = React.useState<"all" | "moving" | "stationary">(
    "all"
  );
  const [presetName, setPresetName] = React.useState("");
  const [savedPresets, setSavedPresets] = React.useState<
    SavedExploreFilterPreset[]
  >(() => triageController.loadFilterPresets("explore"));

  React.useEffect(() => {
    const cleanDate = exploreDateFilter.trim();
    if (!cleanDate) {
      setDateScopedEvents(null);
      return;
    }
    if (typeof fetch !== "function") return;
    let cancelled = false;
    const controller = new AbortController();
    fetch(buildEventsFeedUrl(cleanDate), {
      credentials: "include",
      signal: controller.signal,
    })
      .then((res) => (res.ok ? res.json() : null))
      .then((arr: unknown) => {
        if (!cancelled && Array.isArray(arr)) {
          setDateScopedEvents(arr as EventItem[]);
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      controller.abort();
    };
  }, [exploreDateFilter]);

  const baseExploreEvents = React.useMemo(() => {
    if (!exploreDateFilter) {
      return filteredEvents;
    }
    const source = dateScopedEvents ?? filteredEvents;
    const bounds = parseOperationalDateBoundsSec(exploreDateFilter);
    const q = exploreQuery.trim().toLowerCase();
    return source.filter((ev) => {
      if (!matchesOperationalDateBounds(ev, bounds)) return false;
      if (exploreCamera !== "all" && ev.camera !== exploreCamera) return false;
      if (exploreLabel !== "all" && ev.label !== exploreLabel) return false;
      if (exploreRetainedOnly && !ev.retain_indefinitely) return false;
      if (q) {
        const attrs = Array.isArray(ev.data?.attributes)
          ? ev.data.attributes.join(" ")
          : "";
        const notes = ev.data?.notes || "";
        const hay = `${ev.id} ${ev.camera} ${ev.label} ${ev.sub_label || ""} ${(
          ev.zones || []
        ).join(" ")} ${attrs} ${notes}`.toLowerCase();
        if (!hay.includes(q)) return false;
      }
      return true;
    });
  }, [
    filteredEvents,
    dateScopedEvents,
    exploreDateFilter,
    exploreCamera,
    exploreLabel,
    exploreRetainedOnly,
    exploreQuery,
  ]);

  const availableZones = React.useMemo(() => {
    const set = new Set<string>();
    for (const ev of baseExploreEvents) {
      for (const z of ev.zones || ev.data?.provenance?.zones || []) {
        if (z) set.add(z);
      }
    }
    return Array.from(set).sort();
  }, [baseExploreEvents]);

  const visibleEvents = React.useMemo(() => {
    return baseExploreEvents.filter((ev) => {
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
    baseExploreEvents,
    excludeLabel,
    zoneFilter,
    minScoreFilter,
    stationaryFilter,
    plateFilter,
  ]);

  React.useEffect(() => {
    setVisibleLimit(EXPLORE_PAGE_STEP);
    setOlderExhausted(false);
  }, [
    exploreQuery,
    exploreCamera,
    exploreLabel,
    excludeLabel,
    zoneFilter,
    minScoreFilter,
    stationaryFilter,
    exploreRetainedOnly,
    plateFilter,
    exploreDateFilter,
  ]);

  const pagedEvents = React.useMemo(
    () => visibleEvents.slice(0, visibleLimit),
    [visibleEvents, visibleLimit]
  );
  const remainingEventsCount = Math.max(
    0,
    visibleEvents.length - pagedEvents.length
  );

  const handleClearAllExploreFilters = React.useCallback(() => {
    setExploreQuery("");
    setExploreCamera("all");
    setExploreLabel("all");
    setExcludeLabel("none");
    setZoneFilter("all");
    setMinScoreFilter(0);
    setStationaryFilter("all");
    setExploreRetainedOnly(false);
    setPlateFilter("");
    setExploreDateFilter("");
  }, [
    setExploreQuery,
    setExploreCamera,
    setExploreLabel,
    setExploreRetainedOnly,
  ]);

  const handleSaveExplorePreset = () => {
    const next = triageController.saveFilterPreset(
      "explore",
      savedPresets,
      presetName,
      {
        camera: exploreCamera,
        label: exploreLabel,
        excludeLabel,
        zone: zoneFilter,
        minScore: minScoreFilter,
        stationary: stationaryFilter,
        retainedOnly: exploreRetainedOnly,
      }
    );
    setSavedPresets(next);
    setPresetName("");
  };

  const activeExploreFilterCount =
    countActiveExploreFilters({
      camera: exploreCamera,
      label: exploreLabel,
      excludeLabel,
      zone: zoneFilter,
      minScore: minScoreFilter,
      stationary: stationaryFilter,
      retainedOnly: exploreRetainedOnly,
      plate: plateFilter,
      date: exploreDateFilter,
    }) + (exploreQuery.trim() ? 1 : 0);

  return (
    <>
      {isCompact && (
        <div className="nvr-filter-toggle-row">
          <input
            className="nvr-input nvr-filter-search"
            type="search"
            aria-label="Filter tracked objects"
            placeholder={t.explore.searchPlaceholder}
            value={exploreQuery}
            onChange={(e) => setExploreQuery(e.target.value)}
          />
          <button
            type="button"
            className={`nvr-btn ${filtersOpen ? "active" : ""}`}
            aria-expanded={filtersOpen}
            aria-controls="explore-filters"
            data-testid="explore-filters-toggle"
            onClick={() => setFiltersOpen((v) => !v)}
          >
            {t.review.filtersShort}
            {activeExploreFilterCount > 0 && (
              <span className="nvr-filter-count mono">{activeExploreFilterCount}</span>
            )}
          </button>
        </div>
      )}
      <div
        id="explore-filters"
        className={`nvr-subbar nvr-review-filters ${filtersOpen ? "is-open" : ""}`}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <input
            className="nvr-input nvr-desktop-search"
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
            selectedDate={exploreDateFilter}
            showTimePicker={false}
            allowClear={true}
            openUpward={false}
            buttonLabelPrefix="Date"
            onSelectDate={(dateStr) => setExploreDateFilter(dateStr)}
            onClear={() => setExploreDateFilter("")}
          />
          {activeExploreFilterCount > 0 && (
            <button
              type="button"
              className="nvr-btn mono sm"
              data-testid="explore-clear-filters-bar"
              onClick={handleClearAllExploreFilters}
            >
              {isPt ? "Limpar Filtros" : "Clear All Filters"}
            </button>
          )}
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
                  setSavedPresets(
                    triageController.deleteFilterPreset(
                      "explore",
                      savedPresets,
                      p.name
                    )
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

      <div className="nvr-workbench is-list-first">
        <div className="nvr-panel">
          <div className="nvr-panel-header" style={{ gap: 6, flexWrap: "wrap" }}>
            <span>TRACKED OBJECTS</span>
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              {isAdmin && (
                <button
                  type="button"
                  className="nvr-btn mono sm"
                  style={{ height: 20, padding: "0 6px", fontSize: "var(--text-xs)" }}
                  onClick={() => {
                    const pageIds = pagedEvents.map((e) => e.id);
                    setSelectedBatchEventIds((prev) =>
                      prev.length === pageIds.length ? [] : pageIds
                    );
                  }}
                >
                  {selectedBatchEventIds.length > 0
                    ? `Clear (${selectedBatchEventIds.length})`
                    : "Select Page"}
                </button>
              )}
              <span className="mono">
                {Math.min(visibleLimit, visibleEvents.length)}/{visibleEvents.length}
              </span>
            </div>
          </div>
          {isAdmin && selectedBatchEventIds.length > 0 && (
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
            {pagedEvents.map((ev) => {
              const topScore = Math.round(resolveFiniteEventScore(ev, 0.85) * 100);
              const isSelected = selectedEvent?.id === ev.id;
              const isBatchChecked = selectedBatchEventIds.includes(ev.id);
              const rowDeleteKey = `delete-event-row:${ev.id}`;
              const rowDeleteArmed = isArmed(rowDeleteKey);
              const matchedExport = findMatchingExport(
                exportsList,
                ev.camera,
                ev.start_time,
                ev.end_time
              );
              const effSub =
                ev.id in localEventSubLabels
                  ? localEventSubLabels[ev.id]
                  : ev.sub_label;
              const subBadge = formatSubLabelBadge(
                effSub,
                ev.sub_label_score ?? ev.data?.sub_label_score ?? (effSub ? 1.0 : null)
              );
              return (
                <div
                  key={ev.id}
                  className={`nvr-list-row ${isSelected ? "is-selected" : ""}`}
                  style={{ cursor: "default" }}
                >
                  {isAdmin && (
                    <input
                      type="checkbox"
                      aria-label={`Select event ${ev.id} for batch face assignment`}
                      checked={isBatchChecked}
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
                  )}
                  <button
                    type="button"
                    aria-pressed={isSelected}
                    onClick={() => setSelectedEvent(ev)}
                    style={{
                      flex: 1,
                      minWidth: 0,
                      overflow: "hidden",
                      display: "flex",
                      alignItems: "center",
                      gap: 10,
                      background: "transparent",
                      border: "none",
                      padding: 0,
                      color: "inherit",
                      font: "inherit",
                      textAlign: "left",
                      cursor: "pointer",
                    }}
                  >
                    <img
                      className="nvr-queue-thumb"
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
                        flexShrink: 0,
                      }}
                    />
                    <span style={{ flex: 1, minWidth: 0, overflow: "hidden", display: "block" }}>
                      <span
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          flexWrap: "wrap",
                          gap: 6,
                          rowGap: 3,
                        }}
                      >
                        <span
                          className="mono"
                          style={{ fontWeight: 700, textTransform: "uppercase" }}
                        >
                          {formatEventLabelWithSubLabel(
                            ev.label,
                            effSub,
                            ev.data?.detected_attributes
                          )}{" "}
                          · {topScore}%
                        </span>
                        {subBadge && (
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
                        )}
                        <span
                          className="mono"
                          style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                        >
                          {ev.camera.toUpperCase()}
                        </span>
                      </span>
                      <span
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
                      </span>
                    </span>
                  </button>
                  <div className="nvr-row-actions">
                    {matchedExport && (
                      <button
                        type="button"
                        className="nvr-badge live mono"
                        style={{
                          height: 19,
                          padding: "0 5px",
                          fontSize: "var(--text-xs)",
                          cursor: "pointer",
                        }}
                        title={`Open exported clip ${matchedExport.name}`}
                        onClick={() => {
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
                    {isAdmin && (
                      <button
                        type="button"
                        className="nvr-btn danger mono"
                        style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                        aria-label={
                          rowDeleteArmed
                            ? isPt
                              ? `Confirmar exclusão do evento ${ev.id}`
                              : `Click again to confirm deleting event ${ev.id}`
                            : `Delete event ${ev.id}`
                        }
                        title={
                          rowDeleteArmed
                            ? isPt
                              ? "Clique novamente para confirmar"
                              : "Click again to confirm"
                            : "Delete tracked object event"
                        }
                        onClick={() => {
                          confirmOrArm(rowDeleteKey, () => {
                            void handleEventDelete(ev.id);
                          });
                        }}
                      >
                        {rowDeleteArmed ? (
                          isPt ? "Confirmar?" : "Confirm?"
                        ) : (
                          <Trash2 size={11} />
                        )}
                      </button>
                    )}
                  </div>
                </div>
              );
            })}
            {remainingEventsCount > 0 && (
              <div style={{ padding: "10px 12px", textAlign: "center" }}>
                <button
                  type="button"
                  className="nvr-btn mono"
                  data-testid="explore-load-more"
                  onClick={() => setVisibleLimit((prev) => prev + EXPLORE_PAGE_STEP)}
                >
                  {isPt
                    ? `Mostrar mais 50 (${remainingEventsCount} restantes)`
                    : `Show 50 more (${remainingEventsCount} remaining)`}
                </button>
              </div>
            )}
            {remainingEventsCount === 0 &&
              visibleEvents.length > 0 &&
              onLoadOlderEvents &&
              !olderExhausted && (
                <div style={{ padding: "10px 12px", textAlign: "center" }}>
                  <button
                    type="button"
                    className="nvr-btn mono sm"
                    data-testid="explore-load-older-server"
                    disabled={loadingOlder}
                    onClick={async () => {
                      const oldest = visibleEvents[visibleEvents.length - 1];
                      if (!oldest || loadingOlder) return;
                      setLoadingOlder(true);
                      try {
                        let added = 0;
                        if (exploreDateFilter.trim()) {
                          const url = buildEventsFeedUrl(exploreDateFilter, 150, {
                            before: oldest.start_time,
                            camera: exploreCamera,
                            label: exploreLabel,
                            excludeLabel,
                            zone: zoneFilter,
                            minScore: minScoreFilter > 0 ? minScoreFilter / 100 : undefined,
                            stationary: stationaryFilter,
                            retainedOnly: exploreRetainedOnly,
                            plate: plateFilter,
                          });
                          const res = await fetch(url, { credentials: "include" });
                          if (res.ok) {
                            const older: EventItem[] = await res.json();
                            if (Array.isArray(older) && older.length > 0) {
                              const prevList = dateScopedEventsRef.current ?? [];
                              const existing = new Set(prevList.map((ev) => ev.id));
                              const nextItems = older.filter((ev) => !existing.has(ev.id));
                              added = nextItems.length;
                              if (added > 0) {
                                setDateScopedEvents((prev) => {
                                  const cur = prev ?? [];
                                  const curIds = new Set(cur.map((ev) => ev.id));
                                  return [...cur, ...older.filter((ev) => !curIds.has(ev.id))];
                                });
                              }
                            }
                          }
                        } else if (onLoadOlderEvents) {
                          added = await onLoadOlderEvents(oldest.start_time, {
                            excludeLabel,
                            zone: zoneFilter,
                            minScore: minScoreFilter > 0 ? minScoreFilter / 100 : undefined,
                            stationary: stationaryFilter,
                            plate: plateFilter,
                          });
                        }
                        if (added === 0) setOlderExhausted(true);
                        else setVisibleLimit((prev) => prev + added);
                      } finally {
                        setLoadingOlder(false);
                      }
                    }}
                  >
                    {loadingOlder
                      ? isPt
                        ? "Carregando histórico..."
                        : "Loading older..."
                      : isPt
                        ? "Carregar mais antigos do servidor"
                        : "Load older from server"}
                  </button>
                </div>
              )}
            {visibleEvents.length === 0 && (
              <div
                style={{
                  padding: 24,
                  textAlign: "center",
                  color: "var(--text-muted)",
                  display: "flex",
                  flexDirection: "column",
                  alignItems: "center",
                  gap: 10,
                }}
              >
                <span>
                  {isPt
                    ? "Nenhum evento de objeto rastreado corresponde aos filtros ativos."
                    : "No tracked object events match the active filter criteria."}
                </span>
                {(activeExploreFilterCount > 0 || filteredEvents.length > 0) && (
                  <button
                    type="button"
                    className="nvr-btn mono sm"
                    data-testid="explore-clear-filters-empty"
                    onClick={handleClearAllExploreFilters}
                  >
                    {isPt ? "Limpar Filtros" : "Clear All Filters"}
                  </button>
                )}
              </div>
            )}
          </div>
        </div>

        <div className="nvr-focus-stage" ref={detailStageRef}>
          {selectedEvent ? (
            <>
              <div className="nvr-subbar">
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    minWidth: 0,
                    overflow: "hidden",
                    flex: "1 1 200px",
                    flexWrap: "wrap",
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
                    {selectedEvent.camera.toUpperCase()} ·{" "}
                    {selectedEvent.id.length > 16
                      ? `#${selectedEvent.id.slice(-8)}`
                      : selectedEvent.id}
                  </span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap", minWidth: 0 }}>
                  <button
                    type="button"
                    className={`nvr-btn ${eventViewMode === "snapshot" ? "active" : ""}`}
                    onClick={() => setEventViewMode("snapshot")}
                  >
                    <Camera size={12} />
                    {t.explore.viewSnapshot}
                  </button>
                  <button
                    type="button"
                    className={`nvr-btn ${eventViewMode === "clip" ? "active" : ""}`}
                    onClick={() => setEventViewMode("clip")}
                  >
                    <Play size={12} />
                    {t.explore.playClip}
                  </button>
                  {!isViewer && (
                    <>
                      <button
                        type="button"
                        className={`nvr-btn ${selectedEvent.retain_indefinitely ? "active" : ""}`}
                        onClick={() => handleEventRetain(selectedEvent)}
                      >
                        {t.explore.retainToggle}
                      </button>
                      <button
                        type="button"
                        className={`nvr-btn ${selectedEvent.false_positive ? "active danger" : ""}`}
                        onClick={handleToggleFalsePositive}
                        title={
                          isPt
                            ? "Alternar assinatura de supressão de falso positivo (IoU espacial + dhash64)"
                            : "Toggle false-positive memory suppression signature (spatial IoU + dhash64)"
                        }
                      >
                        {selectedEvent.false_positive
                          ? isPt
                            ? "Falso Positivo ✓"
                            : "False Positive ✓"
                          : isPt
                            ? "Marcar Falso Positivo"
                            : "Mark False Positive"}
                      </button>
                      <button
                        type="button"
                        className="nvr-btn mono"
                        disabled={aiVerifyBusy}
                        onClick={handleVerifyWithAi}
                        title="Verify this detection snapshot using the configured Vision AI Judge (Gemini / OpenAI / Ollama)"
                      >
                        <Sparkles size={12} />
                        {aiVerifyBusy
                          ? isPt
                            ? "Verificando..."
                            : "Verifying..."
                          : isPt
                            ? "Verificar com IA"
                            : "Verify with AI"}
                      </button>
                      {isAdmin && (
                        <button
                          type="button"
                          className={`nvr-btn mono ${addedDatasetIds[selectedEvent.id] ? "active" : ""}`}
                          disabled={datasetAddBusy}
                          onClick={handleAddToDataset}
                          title="Add clean event frame and normalized YOLO bounding box to Active Learning Dataset Studio"
                        >
                          {datasetAddBusy
                            ? isPt
                              ? "Adicionando..."
                              : "Adding..."
                            : addedDatasetIds[selectedEvent.id]
                            ? isPt
                              ? "No Dataset ✓"
                              : "Added to Dataset ✓"
                            : isPt
                              ? "Adicionar ao Dataset"
                              : "Add to Training Dataset"}
                        </button>
                      )}
                    </>
                  )}
                  <button
                    type="button"
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
                    {isPt ? "Sync 4-Câm" : "4-Cam Sync"}
                  </button>
                  <button
                    type="button"
                    className={`nvr-btn mono ${showForensic ? "active" : ""}`}
                    onClick={() => setShowForensic((v) => !v)}
                    title="Inspect exact score, threshold, pixel area, aspect ratio, and zone provenance"
                  >
                    {isPt ? "Explicar Alerta" : "Explain Alert"}
                  </button>
                  {isAdmin && (
                    <>
                      <button
                        type="button"
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
                        {isPt ? "Replay Debug" : "Debug Replay"}
                      </button>
                      <button
                        type="button"
                        className="nvr-btn danger mono"
                        aria-label={
                          isArmed(`delete-event:${selectedEvent.id}`)
                            ? isPt
                              ? "Confirmar exclusão do evento"
                              : "Click again to confirm deleting event"
                            : "Delete tracked object event"
                        }
                        title={
                          isArmed(`delete-event:${selectedEvent.id}`)
                            ? isPt
                              ? "Clique novamente para confirmar"
                              : "Click again to confirm"
                            : "Delete tracked object event"
                        }
                        onClick={() => {
                          confirmOrArm(`delete-event:${selectedEvent.id}`, () => {
                            void handleEventDelete(selectedEvent.id);
                          });
                        }}
                      >
                        <Trash2 size={12} />
                        {isArmed(`delete-event:${selectedEvent.id}`)
                          ? isPt
                            ? "Confirmar?"
                            : "Click again to confirm"
                          : null}
                      </button>
                    </>
                  )}
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
                            const safeEventStart =
                              typeof selectedEvent.start_time === "number" &&
                              Number.isFinite(selectedEvent.start_time)
                                ? selectedEvent.start_time
                                : triggerTs;
                            const targetOffset = Math.max(
                              0,
                              triggerTs - safeEventStart
                            );
                            pendingClipSeekSecRef.current = targetOffset;
                            setEventViewMode("clip");
                            if (eventClipVideoRef.current) {
                              try {
                                eventClipVideoRef.current.currentTime = targetOffset;
                                pendingClipSeekSecRef.current = null;
                              } catch {
                                // Applied on loadedmetadata once media element is ready
                              }
                            }
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

              <div
                className="nvr-cam-tile"
                style={{
                  flex: 1,
                  ...(isCompact ? { order: -1, minHeight: 205 } : {}),
                }}
              >
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
                    const clipSrc = resolveSafeMediaUrl(
                      safeMock
                        ? `/clips/mocks/${encodeURIComponent(safeMock)}`
                        : null,
                      {
                        kind: "event_clip",
                        eventId: selectedEvent.id,
                      }
                    );
                    return (
                      <video
                        key={selectedEvent.id}
                        ref={eventClipVideoRef}
                        src={clipSrc}
                        onLoadedMetadata={(e) => {
                          if (pendingClipSeekSecRef.current !== null) {
                            try {
                              e.currentTarget.currentTime =
                                pendingClipSeekSecRef.current;
                              pendingClipSeekSecRef.current = null;
                            } catch {
                              // ignore seek error on empty media
                            }
                          }
                        }}
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

              {!isViewer && (
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
                  {isAdmin && (
                    <>
                  <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {isPt ? "ROSTO / SUB-RÓTULO:" : "FACE ID / SUB-LABEL:"}
                  </span>
                  <input
                    className="nvr-input mono"
                    style={{ width: 190, height: 26, fontSize: "var(--text-xs)" }}
                    aria-label={isPt ? "Atribuir nome da pessoa" : "Assign Face Name"}
                    placeholder={isPt ? "Nome (ex.: Bruno)..." : "Person name (e.g. Bruno)..."}
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
                      <option value="">
                        {isPt ? "Perfil Rápido..." : "Quick Assign Profile..."}
                      </option>
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
                    disabled={Boolean(batchFaceProgress?.active)}
                    onClick={() => void handleAssignFaceSubLabel()}
                    title="Assigns person name to selected event(s) and automatically enrolls 112x112 face crop into Face Gallery"
                  >
                    {batchFaceProgress?.active
                      ? `Enrolling ${batchFaceProgress.completed}/${batchFaceProgress.total}...`
                      : selectedBatchEventIds.length > 0
                      ? isPt
                        ? `Atribuir (${selectedBatchEventIds.length} sel.)`
                        : `Assign & Enroll (${selectedBatchEventIds.length} Selected)`
                      : isPt
                        ? "Atribuir & Cadastrar Rosto"
                        : "Assign & Enroll Face"}
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
                      {isPt ? "Limpar Rosto" : "Clear / Unassign Face"}
                    </button>
                  )}
                  <span className="nvr-divider" aria-hidden="true" />
                    </>
                  )}
                  <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {isPt ? "PLACA:" : "LICENSE PLATE:"}
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
                    {isPt ? "Salvar Placa" : "Save Plate"}
                  </button>
                  <span className="nvr-divider" aria-hidden="true" />
                  <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {isPt ? "NOTA FORENSE:" : "FORENSIC NOTE:"}
                  </span>
                  <input
                    className="nvr-input mono"
                    style={{ flex: 1, minWidth: 160, height: 26, fontSize: "var(--text-xs)" }}
                    placeholder={
                      isPt
                        ? "Anexar observação forense (pesquisável no Explorar)..."
                        : "Attach operator annotation (searchable in Explore)..."
                    }
                    value={eventNoteDraft}
                    onChange={(e) => setEventNoteDraft(e.target.value)}
                  />
                  <button
                    type="button"
                    className="nvr-btn primary mono"
                    style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                    onClick={handleSaveEventNote}
                  >
                    {isPt ? "Salvar Nota" : "Save Note"}
                  </button>
                </div>
              )}
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
              {isPt
                ? "Selecione um evento de objeto rastreado à esquerda"
                : "Select a tracked object event on the left"}
            </div>
          )}
        </div>
      </div>
    </>
  );
};
