import React from "react";
import { Check, Sparkles, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { resolveRealCameraSource, useNvrStore } from "../store/useNvrStore";
import { useCompactViewport } from "../utils/useCompactViewport";
import { countActiveReviewFilters } from "../utils/liveTileHud";
import {
  LoadedVodWindow,
  resolveReviewArchivePlayback,
  resolveVodSeekAction,
} from "../utils/archivePlayback";
import {
  ReviewMutationPatch,
  reconcileDeletedFaceProfile,
  reconcileReviewMutation,
} from "../utils/operationalFeed";
import {
  AiVerificationResult,
  EnrolledFaceProfileItem,
  ExportItem,
  RecordingDaySummary,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import { WorkspaceTab, findMatchingExport, formatDateTime, formatRelTime } from "../utils/format";
import { buildSharePublicUrl } from "../utils/shareLinks";
import { VodHlsPlayer } from "../components/VideoPlayers";
import {
  TimelineScrubber,
  TimelineTrimFeedbackState,
} from "../components/TimelineScrubber";
import { createTrimScrubChannel } from "../utils/exportClipFlow";
import { RecordingCalendarPopover } from "../components/RecordingCalendarPopover";
import { validateExportTimeWindow } from "./ExportsWorkspace";
import { useTwoStepConfirm } from "./settings/vision/useTwoStepConfirm";
import {
  BatchFaceProgress,
  SavedReviewFilterPreset,
  applyReviewSubLabelUpdate,
  clearFaceProfileFromReviews,
  computeReviewPlaybackWindow,
  createForensicTriageController,
  formatAiVerificationBadge,
  formatDetectedAttributePill,
  formatEventLabelWithSubLabel,
  formatLicensePlateBadge,
  formatSubLabelBadge,
  getVisibleReviewPageIds,
  isOngoingIncident,
  matchesLicensePlateFilter,
  normalizeFaceProfileName,
  normalizeForensicLicensePlate,
  normalizeLicensePlate,
  parseFacesApiResponse,
  parseSavedReviewPresets,
  resolveAdjacentReviewAfterAck,
  resolveNextBriefingStep,
  resolveReviewThumbUrl,
  shouldApplyReviewMutationResult,
} from "../utils/forensicTriage";

export type {
  BatchFaceProgress,
  SavedReviewFilterPreset,
} from "../utils/forensicTriage";
export {
  applyReviewSubLabelUpdate,
  clearFaceProfileFromReviews,
  computeReviewPlaybackWindow,
  createForensicTriageController,
  formatDetectedAttributePill,
  formatEventLabelWithSubLabel,
  formatLicensePlateBadge,
  formatSubLabelBadge,
  getVisibleReviewPageIds,
  isOngoingIncident,
  matchesLicensePlateFilter,
  normalizeFaceProfileName,
  normalizeForensicLicensePlate,
  normalizeLicensePlate,
  parseFacesApiResponse,
  parseSavedReviewPresets,
  resolveAdjacentReviewAfterAck,
  resolveNextBriefingStep,
  resolveReviewThumbUrl,
  shouldApplyReviewMutationResult,
};

const REVIEW_PAGE_STEP = 50;
const REVIEW_SPEED_STEPS = [0.5, 1, 2, 4] as const;

function isEditableKeyTarget(target: EventTarget | null): boolean {
  if (!target || typeof target !== "object") return false;
  const el = target as {
    tagName?: string;
    isContentEditable?: boolean;
    getAttribute?: (name: string) => string | null;
  };
  const tag = (el.tagName || "").toLowerCase();
  if (tag === "input" || tag === "textarea" || tag === "select") {
    return true;
  }
  if (el.isContentEditable) {
    return true;
  }
  const contentEditableAttr = el.getAttribute?.("contenteditable");
  return contentEditableAttr === "true" || contentEditableAttr === "";
}

interface ReviewWorkspaceProps {
  cameraNames: string[];
  reviews: ReviewItem[];
  filteredReviews: ReviewItem[];
  exportsList?: ExportItem[];
  onOpenExport?: (exp: ExportItem) => void;
  reviewCameraFilter: string;
  setReviewCameraFilter: (cam: string) => void;
  reviewSeverityFilter: string;
  setReviewSeverityFilter: (sev: string) => void;
  reviewObjectFilter: string;
  setReviewObjectFilter: (obj: string) => void;
  reviewStatusFilter: "unreviewed" | "reviewed" | "all";
  setReviewStatusFilter: (st: "unreviewed" | "reviewed" | "all") => void;
  reviewDateFilter: string;
  setReviewDateFilter: (date: string) => void;
  globalDaysSummary: RecordingDaySummary[];
  activeReview: ReviewItem | null;
  setActiveReview: (rev: ReviewItem | null) => void;
  focusSegments: RecordingSegment[];
  handleMarkReviews: (ids: string[], reviewed?: boolean) => Promise<void>;
  handleDeleteReviews: (ids: string[]) => Promise<void>;
  handleQuickExport: (
    camera: string,
    startSec: number,
    endSec: number,
    label?: string,
    includeAudio?: boolean
  ) => Promise<{ exportId?: string; videoUrl?: string; queued?: boolean } | void>;
  setSyncMasterTs: (ts: number) => void;
  switchTab: (tab: WorkspaceTab) => void;
  onReconcileReviews?: (
    reviewIds: string | ReadonlyArray<string>,
    patch: ReviewMutationPatch
  ) => void;
}

export const ReviewWorkspace: React.FC<ReviewWorkspaceProps> = ({
  cameraNames,
  reviews,
  filteredReviews,
  exportsList = [],
  onOpenExport,
  reviewCameraFilter,
  setReviewCameraFilter,
  reviewSeverityFilter,
  setReviewSeverityFilter,
  reviewObjectFilter,
  setReviewObjectFilter,
  reviewStatusFilter,
  setReviewStatusFilter,
  reviewDateFilter,
  setReviewDateFilter,
  globalDaysSummary,
  activeReview,
  setActiveReview,
  focusSegments,
  handleMarkReviews,
  handleDeleteReviews,
  handleQuickExport,
  setSyncMasterTs,
  switchTab,
  onReconcileReviews,
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const playbackSpeed = useNvrStore((s) => s.playbackSpeed);
  const setPlaybackSpeed = useNvrStore((s) => s.setPlaybackSpeed);
  const configuredCameras = useNvrStore((s) => s.config?.cameras);
  const { isArmed, confirmOrArm, clearArmed } = useTwoStepConfirm(3000);
  const [visibleLimit, setVisibleLimit] = React.useState<number>(REVIEW_PAGE_STEP);
  const [reviewEventTriageState, setReviewEventTriageState] = React.useState<
    Record<
      string,
      {
        falsePositive?: boolean;
        retained?: boolean;
        aiVerification?: AiVerificationResult;
        statusMessage?: string;
      }
    >
  >({});
  const [reviewAiVerifyBusy, setReviewAiVerifyBusy] = React.useState(false);
  const [reviewFpBusy, setReviewFpBusy] = React.useState(false);
  const [reviewRetainBusy, setReviewRetainBusy] = React.useState(false);
  const [reviewPlayhead, setReviewPlayhead] = React.useState<{
    reviewId: string;
    ts: number;
  } | null>(null);
  const [reviewTrimFeedback, setReviewTrimFeedback] = React.useState<{
    reviewId: string;
    state: TimelineTrimFeedbackState;
  } | null>(null);
  const reviewTrimScrubChannel = React.useMemo(
    () => createTrimScrubChannel(),
    []
  );
  const activeReviewTrim =
    activeReview && reviewTrimFeedback?.reviewId === activeReview.id
      ? reviewTrimFeedback.state
      : null;
  const loadedVodWindowRef = React.useRef<{
    reviewId: string;
    window: LoadedVodWindow;
  } | null>(null);
  const reviewPlayheadTs =
    activeReview && reviewPlayhead?.reviewId === activeReview.id
      ? reviewPlayhead.ts
      : null;
  const [showForensic, setShowForensic] = React.useState(false);
  const [excludeObjectFilter, setExcludeObjectFilter] = React.useState("none");
  const [zoneFilter, setZoneFilter] = React.useState("all");
  const [plateFilter, setPlateFilter] = React.useState("");
  const [presetName, setPresetName] = React.useState("");
  const [reviewNoteDraft, setReviewNoteDraft] = React.useState("");
  const [reviewFaceDraft, setReviewFaceDraft] = React.useState("");
  const [enrolledFaces, setEnrolledFaces] = React.useState<EnrolledFaceProfileItem[]>([]);
  const [selectedBatchReviewIds, setSelectedBatchReviewIds] = React.useState<string[]>([]);
  const [localReviewSubLabels, setLocalReviewSubLabels] = React.useState<
    Record<string, string | null>
  >({});
  const [batchReviewFaceProgress, setBatchReviewFaceProgress] =
    React.useState<BatchFaceProgress | null>(null);
  const [reviewPlateDraft, setReviewPlateDraft] = React.useState("");
  const lastDeletedFaceProfile = useNvrStore((s) => s.lastDeletedFaceProfile);
  const subLabelsRefreshTick = useNvrStore((s) => s.subLabelsRefreshTick);
  const bumpSubLabelsRefresh = useNvrStore((s) => s.bumpSubLabelsRefresh);
  const [shareTtlSec, setShareTtlSec] = React.useState<number>(86400);
  const [shareMaxViews, setShareMaxViews] = React.useState<number>(0);
  const [shareCopied, setShareCopied] = React.useState(false);
  const [shareError, setShareError] = React.useState<string | null>(null);
  const [shareFallbackUrl, setShareFallbackUrl] = React.useState<string | null>(null);
  const [shareCreating, setShareCreating] = React.useState(false);
  const shareCopiedTimerRef = React.useRef<number | null>(null);
  const isMountedRef = React.useRef(true);
  const activeReviewIdRef = React.useRef<string | null>(activeReview?.id ?? null);
  activeReviewIdRef.current = activeReview?.id ?? null;
  const wasInReviewsRef = React.useRef<boolean>(
    Boolean(
      activeReview &&
        (reviews.some((r) => r.id === activeReview.id) ||
          filteredReviews.some((r) => r.id === activeReview.id))
    )
  );
  const triageController = React.useMemo(() => {
    const ctrl = createForensicTriageController();
    ctrl.setActiveTarget("review", activeReview?.id ?? null);
    return ctrl;
  }, []);

  const isCompact = useCompactViewport();
  const [filtersOpen, setFiltersOpen] = React.useState(false);
  const detailStageRef = React.useRef<HTMLDivElement | null>(null);
  const activeReviewIdForScroll = activeReview?.id ?? null;
  // Phones: list-first master/detail — selecting an item brings its player into view.
  const lastScrolledReviewIdRef = React.useRef<string | null | undefined>(undefined);
  React.useEffect(() => {
    const prev = lastScrolledReviewIdRef.current;
    lastScrolledReviewIdRef.current = activeReviewIdForScroll;
    // Skip the auto-selected first item on load; only follow an operator's change.
    if (prev === undefined || prev === null) return;
    if (!isCompact || !activeReviewIdForScroll || prev === activeReviewIdForScroll) return;
    detailStageRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
  }, [isCompact, activeReviewIdForScroll]);

  React.useEffect(() => {
    setReviewNoteDraft(activeReview?.data?.notes || "");
    const firstSub = activeReview?.data?.sub_labels?.[0] || "";
    setReviewFaceDraft(firstSub && firstSub !== "unknown" ? firstSub : "");
    setReviewPlateDraft(activeReview?.data?.recognized_license_plates?.[0] || "");
  }, [
    activeReview?.id,
    activeReview?.data?.notes,
    activeReview?.data?.sub_labels,
    activeReview?.data?.recognized_license_plates,
  ]);

  React.useEffect(() => {
    const nextId = activeReview?.id ?? null;
    activeReviewIdRef.current = nextId;
    triageController.setActiveTarget("review", nextId);
    setReviewPlayhead((curr) =>
      curr && curr.reviewId === nextId ? curr : null
    );
    if (loadedVodWindowRef.current?.reviewId !== nextId) {
      loadedVodWindowRef.current = null;
    }
    wasInReviewsRef.current = Boolean(
      nextId &&
        (reviews.some((r) => r.id === nextId) ||
          filteredReviews.some((r) => r.id === nextId))
    );
    if (shareCopiedTimerRef.current !== null) {
      window.clearTimeout(shareCopiedTimerRef.current);
      shareCopiedTimerRef.current = null;
    }
    setShareError(null);
    setShareFallbackUrl(null);
    setShareCopied(false);
    setShareCreating(false);
    setReviewAiVerifyBusy(false);
    setReviewFpBusy(false);
    setReviewRetainBusy(false);
    clearArmed();
  }, [activeReview?.id, triageController, clearArmed]);

  React.useEffect(() => {
    if (!activeReview) {
      wasInReviewsRef.current = false;
      activeReviewIdRef.current = null;
      triageController.setActiveTarget("review", null);
      setBriefingActive(false);
      return;
    }
    const isPresent =
      reviews.some((r) => r.id === activeReview.id) ||
      filteredReviews.some((r) => r.id === activeReview.id);
    if (isPresent) {
      wasInReviewsRef.current = true;
      activeReviewIdRef.current = activeReview.id;
      triageController.setActiveTarget("review", activeReview.id);
      return;
    }
    if (wasInReviewsRef.current) {
      wasInReviewsRef.current = false;
      activeReviewIdRef.current = null;
      triageController.setActiveTarget("review", null);
      if (shareCopiedTimerRef.current !== null) {
        window.clearTimeout(shareCopiedTimerRef.current);
        shareCopiedTimerRef.current = null;
      }
      setShareError(null);
      setShareFallbackUrl(null);
      setShareCopied(false);
      setShareCreating(false);
      setBriefingActive(false);
    }
  }, [reviews, filteredReviews, activeReview, triageController]);

  React.useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
      triageController.invalidatePendingMutations();
      if (shareCopiedTimerRef.current !== null) {
        window.clearTimeout(shareCopiedTimerRef.current);
        shareCopiedTimerRef.current = null;
      }
    };
  }, [triageController]);

  React.useEffect(() => {
    if (!lastDeletedFaceProfile?.name) return;
    setLocalReviewSubLabels((prev) =>
      reconcileDeletedFaceProfile(
        { localSubLabels: prev },
        lastDeletedFaceProfile.name
      ).localSubLabels
    );
    if (!activeReview) return;
    const scrubbed = reconcileDeletedFaceProfile(
      { activeReview },
      lastDeletedFaceProfile.name
    );
    if (scrubbed.clearedActiveReview && scrubbed.activeReview) {
      setActiveReview(scrubbed.activeReview);
      setReviewFaceDraft("");
    }
  }, [lastDeletedFaceProfile, activeReview, setActiveReview]);

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

  const handleAssignReviewFace = async (overrideName?: string | null) => {
    const raw = overrideName !== undefined ? overrideName : reviewFaceDraft;
    const clean =
      raw && raw.trim().length > 0
        ? normalizeFaceProfileName(raw) || null
        : null;
    if (raw && raw.trim().length > 0 && !clean) return;
    if (selectedBatchReviewIds.length > 0 && clean) {
      await handleBatchAssignReviewFace(clean);
      return;
    }
    if (selectedBatchReviewIds.length > 0 && clean === null) {
      const targetRevIds = [...selectedBatchReviewIds];
      const batchItems = targetRevIds.map((rid) => {
        const revObj = reviews.find((r) => r.id === rid);
        const allDets = (revObj?.data?.detections || [])
          .map((d) => d.trim())
          .filter((d) => d.length > 0);
        return {
          id: rid,
          primaryEventId: allDets[0] || rid,
          detectionEventIds: allDets.length > 0 ? allDets : [rid],
        };
      });
      await triageController.batchAssignFaceSubLabel({
        items: batchItems,
        rawName: null,
        isCancelled: () => !isMountedRef.current,
        onItemSuccess: (rid) => {
          setLocalReviewSubLabels((prev) => ({ ...prev, [rid]: null }));
          onReconcileReviews?.(rid, { subLabel: null });
        },
      });
      if (!isMountedRef.current) return;
      if (!onReconcileReviews && activeReview && targetRevIds.includes(activeReview.id)) {
        const reconciled = reconcileReviewMutation(
          { reviews: [], activeReview },
          activeReview.id,
          { subLabel: null }
        );
        setActiveReview(reconciled.activeReview);
      }
      setSelectedBatchReviewIds([]);
      bumpSubLabelsRefresh();
      return;
    }
    if (!activeReview) return;
    const targetReview = activeReview;
    const targetId = targetReview.id;
    const allDets = (targetReview.data?.detections || [])
      .map((d) => d.trim())
      .filter((d) => d.length > 0);
    const detId = (allDets[0] || targetId).trim();
    if (!detId) return;
    triageController.setActiveTarget("review", targetId);
    try {
      const result = await triageController.assignFaceSubLabel({
        primaryEventId: detId,
        detectionEventIds: allDets.length > 1 ? allDets : [detId],
        rawName: clean,
        targetKind: "review",
        targetId,
        getActiveTargetId: () => activeReviewIdRef.current,
        isCancelled: () => !isMountedRef.current,
      });
      if (result.ok && isMountedRef.current) {
        setLocalReviewSubLabels((prev) => ({
          ...prev,
          [targetId]: result.cleanName,
        }));
        onReconcileReviews?.(targetId, { subLabel: result.cleanName });
      }
      if (result.shouldApply) {
        if (!onReconcileReviews) {
          const reconciled = reconcileReviewMutation(
            { reviews: [], activeReview: targetReview },
            targetId,
            { subLabel: result.cleanName }
          );
          setActiveReview(reconciled.activeReview);
        }
        bumpSubLabelsRefresh();
      }
    } catch {
      // Preserve current review state on transient network failure
    }
  };

  const handleEnrollReviewFace = async () => {
    await handleAssignReviewFace();
  };

  const handleBatchAssignReviewFace = async (overrideName?: string) => {
    const raw = overrideName !== undefined ? overrideName : reviewFaceDraft;
    const cleanName = normalizeFaceProfileName(raw || "");
    if (!cleanName) return;
    const targetRevIds =
      selectedBatchReviewIds.length > 0
        ? selectedBatchReviewIds
        : activeReview
        ? [activeReview.id]
        : [];
    if (targetRevIds.length === 0) return;
    setReviewFaceDraft(cleanName);
    try {
      const batchItems = targetRevIds.map((rid) => {
        const revObj = reviews.find((r) => r.id === rid);
        const detId = (revObj?.data?.detections?.[0] || rid).trim();
        return { id: rid, primaryEventId: detId };
      });
      await triageController.batchAssignFaceSubLabel({
        items: batchItems,
        rawName: cleanName,
        isCancelled: () => !isMountedRef.current,
        onProgress: (progress) => setBatchReviewFaceProgress(progress),
        onItemSuccess: (rid, assignedName) => {
          setLocalReviewSubLabels((prev) => ({
            ...prev,
            [rid]: assignedName,
          }));
          if (onReconcileReviews) {
            onReconcileReviews(rid, { subLabel: assignedName });
          } else if (activeReview && activeReview.id === rid) {
            const reconciled = reconcileReviewMutation(
              { reviews: [], activeReview },
              rid,
              { subLabel: assignedName }
            );
            setActiveReview(reconciled.activeReview);
          }
        },
      });
      if (isMountedRef.current) {
        bumpSubLabelsRefresh();
        setSelectedBatchReviewIds([]);
      }
    } catch {
      // ignore transient error
    }
  };

  const handleSaveReviewLicensePlate = async () => {
    if (!activeReview) return;
    const targetReview = activeReview;
    const targetId = targetReview.id;
    const primaryEventId = (targetReview.data?.detections?.[0] || targetId).trim();
    if (!primaryEventId) return;
    triageController.setActiveTarget("review", targetId);
    try {
      const result = await triageController.saveLicensePlate({
        eventId: primaryEventId,
        rawPlate: reviewPlateDraft,
        targetKind: "review",
        targetId,
        getActiveTargetId: () => activeReviewIdRef.current,
        isCancelled: () => !isMountedRef.current,
      });
      if (result.ok && isMountedRef.current) {
        const norm = result.normalizedPlate || "";
        onReconcileReviews?.(targetId, { recognizedLicensePlate: norm });
        if (result.shouldApply) {
          if (!onReconcileReviews) {
            const reconciled = reconcileReviewMutation(
              { reviews: [], activeReview: targetReview },
              targetId,
              { recognizedLicensePlate: norm }
            );
            setActiveReview(reconciled.activeReview);
          }
          setReviewPlateDraft(norm);
        }
      }
    } catch {
      // Preserve current review state on transient network failure
    }
  };

  const handleSaveReviewNote = async () => {
    if (!activeReview) return;
    const targetReview = activeReview;
    const targetId = targetReview.id;
    const noteToSave = reviewNoteDraft;
    triageController.setActiveTarget("review", targetId);
    try {
      const result = await triageController.saveForensicNote({
        targetKind: "review",
        targetId,
        notes: noteToSave,
        getActiveTargetId: () => activeReviewIdRef.current,
        isCancelled: () => !isMountedRef.current,
      });
      if (result.ok && isMountedRef.current) {
        onReconcileReviews?.(targetId, { notes: result.notes });
        if (result.shouldApply && !onReconcileReviews) {
          const reconciled = reconcileReviewMutation(
            { reviews: [], activeReview: targetReview },
            targetId,
            { notes: result.notes }
          );
          setActiveReview(reconciled.activeReview);
        }
      }
    } catch {
      // Preserve current review state on transient network failure
    }
  };
  const [briefingActive, setBriefingActive] = React.useState(false);
  const [briefingSpeed, setBriefingSpeed] = React.useState<1 | 2 | 4>(2);
  const [briefingAutoMark, setBriefingAutoMark] = React.useState(true);
  const [briefingQueueIds, setBriefingQueueIds] = React.useState<string[]>([]);
  const [briefingIdx, setBriefingIdx] = React.useState(0);
  const [savedPresets, setSavedPresets] = React.useState<
    SavedReviewFilterPreset[]
  >(() => triageController.loadFilterPresets("review"));

  const availableZones = React.useMemo(() => {
    const set = new Set<string>();
    for (const r of reviews) {
      for (const z of r.data?.zones || []) {
        if (z) set.add(z);
      }
    }
    return Array.from(set).sort();
  }, [reviews]);

  const visibleFilteredReviews = React.useMemo(() => {
    return filteredReviews.filter((r) => {
      if (
        excludeObjectFilter !== "none" &&
        (r.data?.objects || []).some((o) => o.toLowerCase() === excludeObjectFilter)
      ) {
        return false;
      }
      if (zoneFilter !== "all" && !(r.data?.zones || []).includes(zoneFilter)) {
        return false;
      }
      if (!matchesLicensePlateFilter(r, plateFilter)) {
        return false;
      }
      return true;
    });
  }, [filteredReviews, excludeObjectFilter, zoneFilter, plateFilter]);

  React.useEffect(() => {
    setVisibleLimit(REVIEW_PAGE_STEP);
  }, [
    reviewCameraFilter,
    reviewSeverityFilter,
    reviewObjectFilter,
    excludeObjectFilter,
    zoneFilter,
    reviewStatusFilter,
    reviewDateFilter,
    plateFilter,
  ]);

  const pagedReviews = React.useMemo(
    () => visibleFilteredReviews.slice(0, visibleLimit),
    [visibleFilteredReviews, visibleLimit]
  );
  const remainingReviewsCount = Math.max(
    0,
    visibleFilteredReviews.length - pagedReviews.length
  );

  const activeFilterCount = countActiveReviewFilters({
    camera: reviewCameraFilter,
    severity: reviewSeverityFilter,
    object: reviewObjectFilter,
    status: reviewStatusFilter,
    date: reviewDateFilter,
    excludeObject: excludeObjectFilter,
    zone: zoneFilter,
    plate: plateFilter,
  });

  const handleClearAllReviewFilters = React.useCallback(() => {
    setReviewCameraFilter("all");
    setReviewSeverityFilter("all");
    setReviewObjectFilter("all");
    setExcludeObjectFilter("none");
    setZoneFilter("all");
    setReviewDateFilter("");
    setPlateFilter("");
    const hasUnreviewed = reviews.some((r) => !r.has_been_reviewed);
    setReviewStatusFilter(hasUnreviewed ? "unreviewed" : "all");
  }, [
    reviews,
    setReviewCameraFilter,
    setReviewSeverityFilter,
    setReviewObjectFilter,
    setReviewDateFilter,
    setReviewStatusFilter,
  ]);

  const handleAcknowledgeReviews = React.useCallback(
    async (ids: string[], reviewed = true) => {
      if (ids.length === 0) return;
      const shouldAdvanceActive = Boolean(
        reviewed &&
          reviewStatusFilter === "unreviewed" &&
          activeReview &&
          ids.includes(activeReview.id)
      );
      const nextActive = shouldAdvanceActive
        ? resolveAdjacentReviewAfterAck(
            visibleFilteredReviews,
            ids,
            activeReview?.id,
            reviewStatusFilter
          )
        : null;
      if (shouldAdvanceActive) {
        if (nextActive) {
          const nextIdx = visibleFilteredReviews.findIndex(
            (r) => r.id === nextActive.id
          );
          if (nextIdx >= visibleLimit) {
            setVisibleLimit(nextIdx + REVIEW_PAGE_STEP);
          }
        }
        setActiveReview(nextActive);
      }
      await handleMarkReviews(ids, reviewed);
    },
    [
      reviewStatusFilter,
      activeReview,
      visibleFilteredReviews,
      visibleLimit,
      setActiveReview,
      handleMarkReviews,
    ]
  );

  const handleConfirmDeleteReviews = React.useCallback(
    async (ids: string[]) => {
      if (ids.length === 0) return;
      const deletingActive = Boolean(activeReview && ids.includes(activeReview.id));
      const nextActive = deletingActive
        ? resolveAdjacentReviewAfterAck(
            visibleFilteredReviews,
            ids,
            activeReview?.id,
            "unreviewed"
          )
        : null;
      await handleDeleteReviews(ids);
      if (deletingActive && isMountedRef.current) {
        setActiveReview(nextActive);
      }
    },
    [activeReview, visibleFilteredReviews, handleDeleteReviews, setActiveReview]
  );

  const triggerExportActiveReview = React.useCallback(() => {
    if (!activeReview) return;
    const nowSec = Math.floor(Date.now() / 1000);
    const win = computeReviewPlaybackWindow(
      activeReview.start_time,
      activeReview.end_time,
      5,
      5,
      20,
      nowSec
    );
    const checked = validateExportTimeWindow(win.startSec, win.endSec);
    if (!checked.valid) return;
    const realReviewCam = resolveRealCameraSource(
      activeReview.camera,
      configuredCameras
    );
    void handleQuickExport(
      realReviewCam,
      checked.startSec,
      checked.endSec,
      `${activeReview.camera}_${activeReview.id}`
    );
  }, [activeReview, configuredCameras, handleQuickExport]);

  const primaryDetectionEventId = React.useMemo(() => {
    const dets = (activeReview?.data?.detections || [])
      .map((d) => d.trim())
      .filter((d) => d.length > 0);
    return dets[0] || null;
  }, [activeReview?.id, activeReview?.data?.detections]);

  const handleToggleReviewFalsePositive = async () => {
    if (!activeReview || !primaryDetectionEventId || reviewFpBusy) return;
    const targetReviewId = activeReview.id;
    const currentFp = Boolean(
      reviewEventTriageState[targetReviewId]?.falsePositive
    );
    triageController.setActiveTarget("review", targetReviewId);
    setReviewFpBusy(true);
    try {
      const result = await triageController.toggleFalsePositive(
        primaryDetectionEventId,
        currentFp,
        {
          targetKind: "review",
          targetId: targetReviewId,
          getActiveTargetId: () => activeReviewIdRef.current,
          isCancelled: () => !isMountedRef.current,
        }
      );
      if (!isMountedRef.current || result.stale) return;
      if (result.ok) {
        setReviewEventTriageState((prev) => ({
          ...prev,
          [targetReviewId]: {
            ...(prev[targetReviewId] || {}),
            falsePositive: result.nextFp,
            statusMessage: result.nextFp
              ? isPt
                ? "Marcado como Falso Positivo — assinatura espacial salva"
                : "Marked as False Positive — spatial & dhash64 suppression saved"
              : isPt
              ? "Marcação de Falso Positivo removida"
              : "Cleared False Positive flag",
          },
        }));
      }
    } catch {
      // Preserve UI state on transient network failure
    } finally {
      if (isMountedRef.current && activeReviewIdRef.current === targetReviewId) {
        setReviewFpBusy(false);
      }
    }
  };

  const handleVerifyReviewWithAi = async () => {
    if (!activeReview || !primaryDetectionEventId || reviewAiVerifyBusy) return;
    const targetReviewId = activeReview.id;
    triageController.setActiveTarget("review", targetReviewId);
    setReviewAiVerifyBusy(true);
    try {
      const result = await triageController.verifyWithAi(
        primaryDetectionEventId,
        {
          targetKind: "review",
          targetId: targetReviewId,
          getActiveTargetId: () => activeReviewIdRef.current,
          isCancelled: () => !isMountedRef.current,
        }
      );
      if (!isMountedRef.current || result.stale) return;
      if (result.ok && result.aiVerification) {
        setReviewEventTriageState((prev) => ({
          ...prev,
          [targetReviewId]: {
            ...(prev[targetReviewId] || {}),
            aiVerification: result.aiVerification,
            ...(result.falsePositive !== undefined
              ? { falsePositive: result.falsePositive }
              : {}),
            statusMessage: undefined,
          },
        }));
      } else if (result.error) {
        setReviewEventTriageState((prev) => ({
          ...prev,
          [targetReviewId]: {
            ...(prev[targetReviewId] || {}),
            statusMessage: result.error,
          },
        }));
      }
    } catch {
      // Preserve UI state on transient network failure
    } finally {
      if (isMountedRef.current && activeReviewIdRef.current === targetReviewId) {
        setReviewAiVerifyBusy(false);
      }
    }
  };

  const handleToggleReviewEventRetain = async () => {
    if (!activeReview || !primaryDetectionEventId || reviewRetainBusy) return;
    const targetReviewId = activeReview.id;
    const currentRetained = Boolean(
      reviewEventTriageState[targetReviewId]?.retained
    );
    triageController.setActiveTarget("review", targetReviewId);
    setReviewRetainBusy(true);
    try {
      const result = await triageController.toggleEventRetain(
        primaryDetectionEventId,
        currentRetained,
        {
          targetKind: "review",
          targetId: targetReviewId,
          getActiveTargetId: () => activeReviewIdRef.current,
          isCancelled: () => !isMountedRef.current,
        }
      );
      if (!isMountedRef.current || result.stale) return;
      if (result.ok) {
        setReviewEventTriageState((prev) => ({
          ...prev,
          [targetReviewId]: {
            ...(prev[targetReviewId] || {}),
            retained: result.nextRetained,
            statusMessage: result.nextRetained
              ? isPt
                ? "Evento retido indefinidamente"
                : "Event retained indefinitely"
              : isPt
              ? "Trava de retenção removida"
              : "Event retention lock removed",
          },
        }));
      }
    } catch {
      // Preserve UI state on transient network failure
    } finally {
      if (isMountedRef.current && activeReviewIdRef.current === targetReviewId) {
        setReviewRetainBusy(false);
      }
    }
  };

  React.useEffect(() => {
    if (typeof window === "undefined") return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.metaKey || e.altKey) return;
      if (isEditableKeyTarget(e.target)) return;

      if (e.shiftKey) {
        if (e.code === "Digit1" || e.key === "!") {
          e.preventDefault();
          e.stopImmediatePropagation();
          setPlaybackSpeed(1);
          return;
        }
        if (e.code === "Digit2" || e.key === "@") {
          e.preventDefault();
          e.stopImmediatePropagation();
          setPlaybackSpeed(2);
          return;
        }
        if (e.code === "Digit4" || e.key === "$") {
          e.preventDefault();
          e.stopImmediatePropagation();
          setPlaybackSpeed(4);
          return;
        }
        return;
      }

      if (e.key === "[" || e.key === "]") {
        e.preventDefault();
        e.stopImmediatePropagation();
        const currIdx = REVIEW_SPEED_STEPS.findIndex(
          (s) => Math.abs(s - playbackSpeed) < 0.01
        );
        const baseIdx = currIdx >= 0 ? currIdx : 1;
        const nextIdx =
          e.key === "["
            ? Math.max(0, baseIdx - 1)
            : Math.min(REVIEW_SPEED_STEPS.length - 1, baseIdx + 1);
        setPlaybackSpeed(REVIEW_SPEED_STEPS[nextIdx]);
        return;
      }

      const key = e.key.toLowerCase();
      if (key === "j" || key === "k") {
        e.preventDefault();
        e.stopImmediatePropagation();
        if (visibleFilteredReviews.length === 0) return;
        const idx = visibleFilteredReviews.findIndex(
          (r) => r.id === activeReview?.id
        );
        const nextIdx =
          key === "j"
            ? idx < 0
              ? 0
              : Math.min(visibleFilteredReviews.length - 1, idx + 1)
            : idx <= 0
            ? 0
            : idx - 1;
        const nextItem = visibleFilteredReviews[nextIdx];
        if (nextItem) {
          if (nextIdx >= visibleLimit) {
            setVisibleLimit(nextIdx + REVIEW_PAGE_STEP);
          }
          setActiveReview(nextItem);
        }
        return;
      }

      if (key === "r" && activeReview) {
        e.preventDefault();
        e.stopImmediatePropagation();
        void handleAcknowledgeReviews([activeReview.id], true);
        return;
      }

      if (key === "e" && activeReview) {
        e.preventDefault();
        e.stopImmediatePropagation();
        triggerExportActiveReview();
      }
    };

    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [
    activeReview,
    visibleFilteredReviews,
    visibleLimit,
    playbackSpeed,
    setPlaybackSpeed,
    setActiveReview,
    handleAcknowledgeReviews,
    triggerExportActiveReview,
  ]);

  const handleSavePreset = () => {
    const next = triageController.saveFilterPreset(
      "review",
      savedPresets,
      presetName,
      {
        camera: reviewCameraFilter,
        severity: reviewSeverityFilter,
        object: reviewObjectFilter,
        excludeObject: excludeObjectFilter,
        zone: zoneFilter,
        status: reviewStatusFilter,
      }
    );
    setSavedPresets(next);
    setPresetName("");
  };

  const handleStartBriefing = () => {
    const queue = visibleFilteredReviews.slice(0, visibleLimit);
    if (queue.length === 0) return;
    setBriefingQueueIds(queue.map((r) => r.id));
    setBriefingIdx(0);
    setBriefingActive(true);
    setPlaybackSpeed(briefingSpeed);
    setActiveReview(queue[0]);
  };

  const handleAdvanceBriefing = React.useCallback(() => {
    if (!briefingActive) return;
    const currentId = activeReview?.id || briefingQueueIds[briefingIdx];
    if (briefingAutoMark && currentId && reviews.some((r) => r.id === currentId)) {
      void handleMarkReviews([currentId], true);
    }
    const nextStep = resolveNextBriefingStep(
      briefingQueueIds,
      briefingIdx,
      reviews,
      activeReview?.id
    );
    if (!nextStep) {
      setBriefingActive(false);
      return;
    }
    setBriefingIdx(nextStep.nextIdx);
    setActiveReview(nextStep.nextReview);
  }, [
    briefingActive,
    briefingQueueIds,
    briefingIdx,
    activeReview,
    briefingAutoMark,
    handleMarkReviews,
    reviews,
    setActiveReview,
  ]);

  return (
    <>
      {isCompact && (
        <div className="nvr-filter-toggle-row">
          <button
            type="button"
            className={`nvr-btn ${filtersOpen ? "active" : ""}`}
            aria-expanded={filtersOpen}
            aria-controls="review-filters"
            data-testid="review-filters-toggle"
            onClick={() => setFiltersOpen((v) => !v)}
          >
            {t.review.filtersShort}
            {(() => {
              const n = countActiveReviewFilters({
                camera: reviewCameraFilter,
                severity: reviewSeverityFilter,
                object: reviewObjectFilter,
                status: reviewStatusFilter,
                date: reviewDateFilter,
                excludeObject: excludeObjectFilter,
                zone: zoneFilter,
                plate: plateFilter,
              });
              return n > 0 ? <span className="nvr-filter-count mono">{n}</span> : null;
            })()}
          </button>
          <span className="mono nvr-filter-toggle-meta">
            {visibleFilteredReviews.length} items
          </span>
        </div>
      )}
      <div
        id="review-filters"
        className={`nvr-subbar nvr-review-filters ${filtersOpen ? "is-open" : ""}`}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <select
            className="nvr-select"
            aria-label="Filter review queue by camera"
            value={reviewCameraFilter}
            onChange={(e) => setReviewCameraFilter(e.target.value)}
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
            aria-label="Filter review queue by severity"
            value={reviewSeverityFilter}
            onChange={(e) => setReviewSeverityFilter(e.target.value)}
          >
            <option value="all">{t.review.allSeverities}</option>
            <option value="alert">{t.review.alerts}</option>
            <option value="detection">{t.review.detections}</option>
            <option value="audio">Audio Detections</option>
          </select>
          <select
            className="nvr-select"
            aria-label="Filter review queue by object class"
            value={reviewObjectFilter}
            onChange={(e) => setReviewObjectFilter(e.target.value)}
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
            aria-label="Exclude object class (NOT)"
            value={excludeObjectFilter}
            onChange={(e) => setExcludeObjectFilter(e.target.value)}
            title="Boolean NOT filter: exclude review items containing this class"
          >
            <option value="none">NOT: NONE</option>
            <option value="cat">NOT: CAT</option>
            <option value="dog">NOT: DOG</option>
            <option value="car">NOT: CAR</option>
            <option value="person">NOT: PERSON</option>
          </select>
          <select
            className="nvr-select mono"
            aria-label="Filter review queue by zone"
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
            className="nvr-select"
            aria-label="Filter review queue by status"
            value={reviewStatusFilter}
            onChange={(e) =>
              setReviewStatusFilter(e.target.value as "unreviewed" | "reviewed" | "all")
            }
          >
            <option value="unreviewed">{t.review.unreviewedOnly}</option>
            <option value="reviewed">{t.review.reviewedOnly}</option>
            <option value="all">{t.review.allStatuses}</option>
          </select>
          <RecordingCalendarPopover
            daysSummary={globalDaysSummary}
            selectedDate={reviewDateFilter}
            showTimePicker={false}
            allowClear={true}
            openUpward={false}
            buttonLabelPrefix="Date"
            onSelectDate={(dateStr) => {
              setReviewDateFilter(dateStr);
              if (dateStr) setReviewStatusFilter("all");
            }}
            onClear={() => setReviewDateFilter("")}
          />
          <input
            className="nvr-input mono"
            style={{ width: 125, height: 26, fontSize: "var(--text-xs)", fontFamily: "var(--font-mono)" }}
            placeholder="Plate (ABC1D23)..."
            aria-label="Filter review queue by license plate"
            value={plateFilter}
            onChange={(e) => setPlateFilter(e.target.value.toUpperCase())}
          />
          <input
            className="nvr-input mono"
            style={{ width: 110, height: 26, fontSize: "var(--text-xs)" }}
            placeholder="Preset name..."
            aria-label="Filter preset name"
            value={presetName}
            onChange={(e) => setPresetName(e.target.value)}
          />
          <button
            type="button"
            className="nvr-btn mono"
            style={{ height: 26, padding: "0 7px", fontSize: "var(--text-xs)" }}
            onClick={handleSavePreset}
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
                  setReviewCameraFilter(p.camera);
                  setReviewSeverityFilter(p.severity);
                  setReviewObjectFilter(p.object);
                  setExcludeObjectFilter(p.excludeObject || "none");
                  setZoneFilter(p.zone || "all");
                  setReviewStatusFilter(p.status);
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
                      "review",
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
          {activeFilterCount > 0 && (
            <button
              type="button"
              className="nvr-btn mono sm"
              data-testid="review-clear-filters-bar"
              onClick={handleClearAllReviewFilters}
            >
              {isPt ? "Limpar Filtros" : "Clear All Filters"}
            </button>
          )}
          <span
            className="mono"
            style={{
              color: "var(--text-muted)",
              fontSize: "var(--text-xs)",
              whiteSpace: "nowrap",
            }}
          >
            {visibleFilteredReviews.length} {isPt ? "itens" : "items"}
          </span>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 6, flexShrink: 0 }}>
          {visibleFilteredReviews.length > 0 && (
            <button
              type="button"
              className={`nvr-btn mono ${briefingActive ? "active" : "primary"}`}
              onClick={() => {
                if (briefingActive) {
                  setBriefingActive(false);
                } else {
                  handleStartBriefing();
                }
              }}
              title="Sequential accelerated auto-playback of queued review segments"
            >
              {briefingActive
                ? `■ Stop Brief (${briefingIdx + 1}/${briefingQueueIds.length})`
                : `▶ Play Tactical Brief (${Math.min(visibleLimit, visibleFilteredReviews.length)})`}
            </button>
          )}
          <span
            className="mono"
            style={{
              fontSize: "var(--text-xs)",
              color: "var(--text-muted)",
              whiteSpace: "nowrap",
            }}
          >
            {isPt
              ? "[J/K] Nav · [R] Visto · [E] Clipe · [[/]] Vel"
              : "[J/K] Nav · [R] Ack · [E] Clip · [[/]] Spd"}
          </span>
          {[0.5, 1, 2, 4].map((spd) => (
            <button
              key={spd}
              type="button"
              className={`nvr-btn mono ${playbackSpeed === spd ? "active" : ""}`}
              onClick={() => setPlaybackSpeed(spd)}
            >
              {spd}x
            </button>
          ))}
        </div>
      </div>

      <div className="nvr-workbench is-list-first">
        <div className="nvr-panel">
          <div className="nvr-panel-header" style={{ gap: 6, flexWrap: "wrap" }}>
            <span>
              {isPt ? "FILA DE REVISÃO" : "REVIEW QUEUE"} (
              {Math.min(visibleLimit, visibleFilteredReviews.length)}/
              {visibleFilteredReviews.length})
            </span>
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              {visibleFilteredReviews.length > 0 && (
                <>
                  <button
                    type="button"
                    className="nvr-btn mono sm"
                    style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                    onClick={() => {
                      const pageIds = getVisibleReviewPageIds(
                        visibleFilteredReviews,
                        visibleLimit
                      );
                      setSelectedBatchReviewIds((prev) =>
                        prev.length === pageIds.length ? [] : pageIds
                      );
                    }}
                  >
                    {selectedBatchReviewIds.length > 0
                      ? `Clear (${selectedBatchReviewIds.length})`
                      : "Select Page"}
                  </button>
                  <button
                    type="button"
                    className="nvr-btn success"
                    style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                    onClick={() =>
                      void handleAcknowledgeReviews(
                        selectedBatchReviewIds.length > 0
                          ? selectedBatchReviewIds
                          : getVisibleReviewPageIds(
                              visibleFilteredReviews,
                              visibleLimit
                            ),
                        true
                      )
                    }
                  >
                    <Check size={11} />
                    {selectedBatchReviewIds.length > 0
                      ? `Mark (${selectedBatchReviewIds.length}) Viewed`
                      : "Mark Page Viewed"}
                  </button>
                </>
              )}
            </div>
          </div>
          {selectedBatchReviewIds.length > 0 && (
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
                <span>BATCH FACE ID ({selectedBatchReviewIds.length} SELECTED)</span>
                {batchReviewFaceProgress?.active && (
                  <span>
                    {batchReviewFaceProgress.completed}/{batchReviewFaceProgress.total}
                  </span>
                )}
              </div>
              <div style={{ display: "flex", gap: 4, flexWrap: "wrap", alignItems: "center" }}>
                <input
                  className="nvr-input mono"
                  style={{ flex: 1, minWidth: 95, height: 24, fontSize: "var(--text-xs)" }}
                  placeholder="Name (e.g. Erik)..."
                  value={reviewFaceDraft}
                  onChange={(e) => setReviewFaceDraft(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn primary mono sm"
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  disabled={Boolean(batchReviewFaceProgress?.active)}
                  onClick={() => void handleBatchAssignReviewFace()}
                >
                  {batchReviewFaceProgress?.active
                    ? `${batchReviewFaceProgress.completed}/${batchReviewFaceProgress.total}...`
                    : `Assign & Enroll (${selectedBatchReviewIds.length})`}
                </button>
              </div>
              {enrolledFaces.length > 0 && (
                <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
                  {enrolledFaces.map((p) => (
                    <button
                      key={`rev-batch-chip-${p.name}`}
                      type="button"
                      className="nvr-btn mono sm"
                      style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                      disabled={Boolean(batchReviewFaceProgress?.active)}
                      onClick={() => void handleBatchAssignReviewFace(p.name)}
                    >
                      + {p.name} ({p.sample_count})
                    </button>
                  ))}
                </div>
              )}
            </div>
          )}
          <div className="nvr-scroll-y">
            {pagedReviews.map((item) => {
              const isSelected = activeReview?.id === item.id;
              const isBatchChecked = selectedBatchReviewIds.includes(item.id);
              const isOngoing = isOngoingIncident(item.end_time);
              const deleteKey = `delete-review:${item.id}`;
              const deleteArmed = isArmed(deleteKey);
              const matchedExport = findMatchingExport(
                exportsList,
                item.camera,
                item.start_time,
                item.end_time
              );
              const thumbUrl = resolveReviewThumbUrl(item);
              const effSub =
                item.id in localReviewSubLabels
                  ? localReviewSubLabels[item.id]
                  : item.data?.sub_labels?.[0] ?? null;
              const subBadge = formatSubLabelBadge(
                effSub,
                item.data?.sub_label_score ?? (effSub ? 1.0 : null)
              );
              return (
                <div
                  key={item.id}
                  className={`nvr-list-row ${isSelected ? "is-selected" : ""}`}
                  style={{
                    cursor: "default",
                    borderLeft:
                      item.severity === "alert"
                        ? "3px solid var(--signal-alert)" /* slop-lint-disable-line side-tab-border -- real alert severity indicator */
                        : "3px solid var(--signal-motion)" /* slop-lint-disable-line side-tab-border -- real detection severity indicator */,
                  }}
                >
                  <input
                    type="checkbox"
                    aria-label={`Select review ${item.id} for batch face assignment`}
                    checked={isBatchChecked}
                    onChange={(e) => {
                      const checked = e.target.checked;
                      setSelectedBatchReviewIds((prev) =>
                        checked
                          ? prev.includes(item.id)
                            ? prev
                            : [...prev, item.id]
                          : prev.filter((id) => id !== item.id)
                      );
                    }}
                    style={{ cursor: "pointer", flexShrink: 0 }}
                  />
                  <button
                    type="button"
                    aria-pressed={isSelected}
                    onClick={() => {
                      if (briefingActive) {
                        const qIdx = briefingQueueIds.indexOf(item.id);
                        if (qIdx >= 0) setBriefingIdx(qIdx);
                      }
                      setActiveReview(item);
                    }}
                    style={{
                      flex: 1,
                      minWidth: 0,
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
                      src={thumbUrl}
                      alt={item.camera}
                      width={84}
                      height={48}
                      loading="lazy"
                      style={{
                        width: 84,
                        height: 48,
                        objectFit: "cover",
                        borderRadius: 2,
                        border: "1px solid var(--border-hairline)",
                        flexShrink: 0,
                      }}
                    />
                    <span style={{ flex: 1, minWidth: 0, display: "block" }}>
                      <span
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: 6,
                        }}
                      >
                        <span style={{ fontWeight: 700, textTransform: "uppercase" }}>
                          {item.camera}
                        </span>
                        <span
                          style={{
                            display: "inline-flex",
                            alignItems: "center",
                            gap: 5,
                            flexShrink: 0,
                          }}
                        >
                          {isOngoing && (
                            <span
                              className="nvr-badge alert mono"
                              data-testid={`review-ongoing-badge-${item.id}`}
                              style={{
                                height: 18,
                                padding: "0 5px",
                                fontSize: "var(--text-xs)",
                                display: "inline-flex",
                                alignItems: "center",
                                gap: 4,
                              }}
                            >
                              <span className="nvr-dot alert" aria-hidden="true" />
                              {isPt ? "EM ANDAMENTO" : "ONGOING"}
                            </span>
                          )}
                          <span
                            className="mono"
                            style={{
                              fontSize: "var(--text-xs)",
                              color: "var(--text-secondary)",
                              whiteSpace: "nowrap",
                            }}
                          >
                            {formatDateTime(item.start_time)}
                          </span>
                        </span>
                      </span>
                      <span
                        style={{
                          display: "flex",
                          alignItems: "center",
                          justifyContent: "space-between",
                          gap: 6,
                          flexWrap: "wrap",
                          marginTop: 4,
                        }}
                      >
                        <span
                          className="mono"
                          style={{
                            fontSize: "var(--text-xs)",
                            color:
                              item.severity === "alert"
                                ? "var(--signal-alert-text)"
                                : "var(--signal-motion-text)",
                            textTransform: "uppercase",
                          }}
                        >
                          {formatEventLabelWithSubLabel(
                            (item.data?.objects || ["motion"]).join(", "),
                            effSub,
                            item.data?.detected_attributes
                          )}
                        </span>
                        <span style={{ display: "inline-flex", alignItems: "center", gap: 4, flexWrap: "wrap" }}>
                          {subBadge && (
                            <span
                              className={`nvr-badge mono ${
                                subBadge.isUnknown ? "alert" : "live"
                              }`}
                              style={{
                                height: 20,
                                padding: "0 5px",
                                fontSize: "var(--text-xs)",
                              }}
                            >
                              {subBadge.text}
                            </span>
                          )}
                          {(item.data?.recognized_license_plates || []).map((plate) => (
                            <span
                              key={plate}
                              className="nvr-badge mono"
                              style={{
                                height: 20,
                                padding: "0 5px",
                                fontFamily: "var(--font-mono)",
                                fontSize: "var(--text-xs)",
                                color: "var(--signal-accent-text)",
                              }}
                            >
                              {plate}
                            </span>
                          ))}
                          {((item.data?.audio && item.data.audio.length > 0) ||
                            (item.data?.objects || []).includes("audio")) && (
                            <span
                              className="nvr-badge mono"
                              style={{
                                height: 20,
                                padding: "0 5px",
                                fontSize: "var(--text-xs)",
                                color: "var(--signal-accent-text)",
                              }}
                            >
                              AUDIO
                            </span>
                          )}
                        </span>
                      </span>
                    </span>
                  </button>
                  <div className="nvr-row-actions">
                    {matchedExport && (
                      <button
                        type="button"
                        className="nvr-badge live mono"
                        style={{
                          height: 20,
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
                    {!item.has_been_reviewed && (
                      <button
                        type="button"
                        className="nvr-btn success"
                        style={{ height: 22, padding: "0 6px" }}
                        aria-label={`Mark ${item.camera} review item as viewed`}
                        title={t.live.markReviewed}
                        onClick={() => {
                          void handleAcknowledgeReviews([item.id], true);
                        }}
                      >
                        <Check size={11} />
                      </button>
                    )}
                    <button
                      type="button"
                      className="nvr-btn danger mono"
                      style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                      aria-label={
                        deleteArmed
                          ? isPt
                            ? `Confirmar exclusão de ${item.camera}`
                            : `Click again to confirm deleting ${item.camera}`
                          : `Delete ${item.camera} review item`
                      }
                      title={
                        deleteArmed
                          ? isPt
                            ? "Clique novamente para confirmar"
                            : "Click again to confirm"
                          : t.review.deleteSelected
                      }
                      onClick={() => {
                        confirmOrArm(deleteKey, () => {
                          void handleConfirmDeleteReviews([item.id]);
                        });
                      }}
                    >
                      {deleteArmed ? (
                        isPt ? "Confirmar?" : "Confirm?"
                      ) : (
                        <Trash2 size={11} />
                      )}
                    </button>
                  </div>
                </div>
              );
            })}
            {remainingReviewsCount > 0 && (
              <div style={{ padding: "10px 12px", textAlign: "center" }}>
                <button
                  type="button"
                  className="nvr-btn mono"
                  data-testid="review-load-more"
                  onClick={() => setVisibleLimit((prev) => prev + REVIEW_PAGE_STEP)}
                >
                  {isPt
                    ? `Mostrar mais 50 (${remainingReviewsCount} restantes)`
                    : `Show 50 more (${remainingReviewsCount} remaining)`}
                </button>
              </div>
            )}
            {visibleFilteredReviews.length === 0 && (
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
                <span>{t.review.emptyState}</span>
                {(activeFilterCount > 0 || reviews.length > 0) && (
                  <button
                    type="button"
                    className="nvr-btn mono sm"
                    data-testid="review-clear-filters-empty"
                    onClick={handleClearAllReviewFilters}
                  >
                    {isPt ? "Limpar Filtros" : "Clear All Filters"}
                  </button>
                )}
              </div>
            )}
          </div>
        </div>

        <div className="nvr-focus-stage" ref={detailStageRef}>
          {activeReview ? (
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
                    {activeReview.camera.toUpperCase()} //{" "}
                    {formatDateTime(activeReview.start_time)}
                  </span>
                  {isOngoingIncident(activeReview.end_time) && (
                    <span
                      className="nvr-badge alert mono"
                      data-testid="review-detail-ongoing-badge"
                      style={{ display: "inline-flex", alignItems: "center", gap: 5 }}
                    >
                      <span className="nvr-dot alert" aria-hidden="true" />
                      {isPt ? "EM ANDAMENTO" : "ONGOING"}
                    </span>
                  )}
                  <span className="nvr-badge">
                    {formatEventLabelWithSubLabel(
                      (activeReview.data?.objects || ["person"]).join(", "),
                      activeReview.data?.sub_labels?.[0] ?? null,
                      activeReview.data?.detected_attributes
                    )}
                  </span>
                  {(() => {
                    const subBadge = formatSubLabelBadge(
                      activeReview.data?.sub_labels?.[0] ?? null,
                      activeReview.data?.sub_label_score
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
                  {(activeReview.data?.detected_attributes || []).map((attr, idx) => (
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
                  {(activeReview.data?.recognized_license_plates || []).map((plate) => (
                    <span
                      key={plate}
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
                        plate,
                        null,
                        activeReview.data?.sub_labels?.[0] ?? null
                      )}
                    </span>
                  ))}
                  <span
                    className="mono"
                    style={{
                      color: "var(--text-secondary)",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                    }}
                  >
                    ID: {activeReview.id}
                  </span>
                  <span className="mono" style={{ color: "var(--text-muted)", flexShrink: 0 }}>
                    ({formatRelTime(activeReview.start_time)})
                  </span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexShrink: 0, flexWrap: "wrap" }}>
                  {primaryDetectionEventId && (
                    <>
                      <button
                        type="button"
                        className={`nvr-btn mono ${
                          reviewEventTriageState[activeReview.id]?.retained ? "active" : ""
                        }`}
                        disabled={reviewRetainBusy}
                        data-testid="review-retain-event"
                        title={
                          isPt
                            ? "Reter evento associado indefinidamente"
                            : "Retain associated detection event indefinitely"
                        }
                        onClick={() => void handleToggleReviewEventRetain()}
                      >
                        {reviewEventTriageState[activeReview.id]?.retained
                          ? isPt
                            ? "Retido ✓"
                            : "Retained ✓"
                          : isPt
                          ? "Reter Indefinidamente"
                          : "Retain Indefinitely"}
                      </button>
                      <button
                        type="button"
                        className={`nvr-btn mono ${
                          reviewEventTriageState[activeReview.id]?.falsePositive
                            ? "active danger"
                            : ""
                        }`}
                        disabled={reviewFpBusy}
                        data-testid="review-mark-false-positive"
                        title={
                          isPt
                            ? "Marcar evento associado como falso positivo"
                            : "Toggle false-positive memory suppression on associated detection"
                        }
                        onClick={() => void handleToggleReviewFalsePositive()}
                      >
                        {reviewEventTriageState[activeReview.id]?.falsePositive
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
                        disabled={reviewAiVerifyBusy}
                        data-testid="review-verify-ai"
                        title={
                          isPt
                            ? "Verificar detecção com o Juiz de IA Visual"
                            : "Verify detection snapshot with Vision AI Judge"
                        }
                        onClick={() => void handleVerifyReviewWithAi()}
                      >
                        <Sparkles size={12} />
                        {reviewAiVerifyBusy
                          ? isPt
                            ? "Verificando..."
                            : "Verifying..."
                          : isPt
                          ? "Verificar com IA"
                          : "Verify with AI"}
                      </button>
                    </>
                  )}
                  <button
                    type="button"
                    className={`nvr-btn mono ${showForensic ? "active" : ""}`}
                    onClick={() => setShowForensic((v) => !v)}
                    title="Inspect exact score, threshold, pixel area, aspect ratio, and zone provenance"
                  >
                    Explain Alert
                  </button>
                  <button
                    className="nvr-btn"
                    onClick={() => {
                      const safeTs =
                        typeof activeReview.start_time === "number" &&
                        Number.isFinite(activeReview.start_time) &&
                        activeReview.start_time > 0
                          ? Math.floor(activeReview.start_time)
                          : Math.floor(Date.now() / 1000) - 20;
                      setSyncMasterTs(safeTs);
                      switchTab("sync");
                    }}
                  >
                    Open in 4-Cam Sync
                  </button>
                  <select
                    className="nvr-select mono"
                    aria-label="Share link expiration"
                    style={{ height: 28, fontSize: "var(--text-xs)" }}
                    value={shareTtlSec}
                    onChange={(e) => setShareTtlSec(Number(e.target.value))}
                  >
                    <option value={3600}>TTL: 1h</option>
                    <option value={86400}>TTL: 24h</option>
                    <option value={604800}>TTL: 7d</option>
                  </select>
                  <select
                    className="nvr-select mono"
                    aria-label="Share link maximum views"
                    style={{ height: 28, fontSize: "var(--text-xs)" }}
                    value={shareMaxViews}
                    onChange={(e) => setShareMaxViews(Number(e.target.value))}
                  >
                    <option value={0}>Views: Unlimited</option>
                    <option value={1}>Views: 1</option>
                    <option value={5}>Views: 5</option>
                    <option value={25}>Views: 25</option>
                  </select>
                  <button
                    className={`nvr-btn mono ${shareError ? "danger" : shareCopied ? "success" : ""}`}
                    disabled={shareCreating}
                    onClick={async () => {
                      if (shareCreating) return;
                      const requestReviewId = activeReview.id;
                      activeReviewIdRef.current = requestReviewId;
                      if (shareCopiedTimerRef.current !== null) {
                        window.clearTimeout(shareCopiedTimerRef.current);
                        shareCopiedTimerRef.current = null;
                      }
                      setShareError(null);
                      setShareFallbackUrl(null);
                      setShareCopied(false);
                      setShareCreating(true);
                      try {
                        const res = await fetch("/api/share", {
                          method: "POST",
                          headers: { "Content-Type": "application/json" },
                          credentials: "include",
                          body: JSON.stringify({
                            kind: "review",
                            target_id: requestReviewId,
                            expires_in_sec: shareTtlSec,
                            ...(shareMaxViews > 0 ? { max_views: shareMaxViews } : {}),
                          }),
                        });
                        const data = await res.json().catch(() => ({}));
                        if (!isMountedRef.current || activeReviewIdRef.current !== requestReviewId) {
                          return;
                        }
                        if (!res.ok || !(data?.url || data?.token)) {
                          const errMsg =
                            (typeof data?.message === "string" && data.message) ||
                            (typeof data?.error === "string" && data.error) ||
                            `${t.review.shareError} (${res.status})`;
                          setShareError(errMsg);
                          setShareCopied(false);
                          return;
                        }
                        const fullUrl = buildSharePublicUrl(
                          window.location.origin,
                          data.url || data.token
                        );
                        if (!fullUrl) {
                          setShareError(t.review.shareError);
                          setShareCopied(false);
                          return;
                        }
                        setShareFallbackUrl(fullUrl);
                        let copiedOk = false;
                        if (
                          typeof navigator !== "undefined" &&
                          navigator.clipboard &&
                          typeof navigator.clipboard.writeText === "function"
                        ) {
                          try {
                            await navigator.clipboard.writeText(fullUrl);
                            copiedOk = true;
                          } catch {
                            copiedOk = false;
                          }
                        }
                        if (!isMountedRef.current || activeReviewIdRef.current !== requestReviewId) {
                          return;
                        }
                        if (copiedOk) {
                          setShareCopied(true);
                          shareCopiedTimerRef.current = window.setTimeout(() => {
                            if (isMountedRef.current && activeReviewIdRef.current === requestReviewId) {
                              setShareCopied(false);
                            }
                            shareCopiedTimerRef.current = null;
                          }, 2000);
                        } else {
                          setShareCopied(false);
                        }
                      } catch {
                        if (!isMountedRef.current || activeReviewIdRef.current !== requestReviewId) {
                          return;
                        }
                        setShareError(t.review.shareError);
                        setShareCopied(false);
                      } finally {
                        if (isMountedRef.current && activeReviewIdRef.current === requestReviewId) {
                          setShareCreating(false);
                        }
                      }
                    }}
                    title="Copy HMAC-signed shareable link for this review event"
                  >
                    {shareError
                      ? t.review.shareError
                      : shareCopied
                        ? t.review.shareCopiedBtn
                        : t.review.shareLinkBtn}
                  </button>
                  <button
                    type="button"
                    className="nvr-btn primary"
                    onClick={triggerExportActiveReview}
                  >
                    {t.review.exportClip}
                  </button>
                  <button
                    type="button"
                    className="nvr-btn success"
                    onClick={() => void handleAcknowledgeReviews([activeReview.id], true)}
                  >
                    <Check size={12} />
                    {t.review.markSelectedReviewed}
                  </button>
                  <button
                    type="button"
                    className="nvr-btn danger mono"
                    aria-label={
                      isArmed(`delete-active-review:${activeReview.id}`)
                        ? isPt
                          ? "Confirmar exclusão do segmento"
                          : "Click again to confirm deleting review"
                        : t.review.deleteSelected
                    }
                    title={
                      isArmed(`delete-active-review:${activeReview.id}`)
                        ? isPt
                          ? "Clique novamente para confirmar"
                          : "Click again to confirm"
                        : t.review.deleteSelected
                    }
                    onClick={() => {
                      confirmOrArm(`delete-active-review:${activeReview.id}`, () => {
                        void handleConfirmDeleteReviews([activeReview.id]);
                      });
                    }}
                  >
                    <Trash2 size={12} />
                    {isArmed(`delete-active-review:${activeReview.id}`)
                      ? isPt
                        ? "Confirmar?"
                        : "Click again to confirm"
                      : null}
                  </button>
                </div>
              </div>

              {(reviewEventTriageState[activeReview.id]?.aiVerification ||
                reviewEventTriageState[activeReview.id]?.falsePositive ||
                reviewEventTriageState[activeReview.id]?.statusMessage) && (
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
                    {reviewEventTriageState[activeReview.id]?.aiVerification && (
                      <>
                        <span
                          className={`nvr-badge mono ${
                            reviewEventTriageState[activeReview.id]!.aiVerification!
                              .is_true_positive
                              ? "live"
                              : "alert"
                          }`}
                        >
                          {formatAiVerificationBadge(
                            reviewEventTriageState[activeReview.id]!.aiVerification!
                          )}
                        </span>
                        <span style={{ color: "var(--text-primary)" }}>
                          Reason:{" "}
                          <strong>
                            {reviewEventTriageState[activeReview.id]!.aiVerification!.reason}
                          </strong>
                        </span>
                      </>
                    )}
                    {reviewEventTriageState[activeReview.id]?.falsePositive && (
                      <span className="nvr-badge alert mono">
                        {isPt
                          ? "SUPRIMIDO NA MEMÓRIA DE FALSOS POSITIVOS"
                          : "SUPPRESSED IN FALSE-POSITIVE MEMORY"}
                      </span>
                    )}
                    {reviewEventTriageState[activeReview.id]?.statusMessage && (
                      <span style={{ color: "var(--text-secondary)" }}>
                        {reviewEventTriageState[activeReview.id]?.statusMessage}
                      </span>
                    )}
                  </div>
                </div>
              )}

              {(shareError || shareFallbackUrl) && (
                <div
                  className="mono"
                  style={{
                    background: "var(--bg-subcard)",
                    borderBottom: "1px solid var(--border-hairline)",
                    padding: "6px 12px",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 8,
                    fontSize: "var(--text-xs)",
                  }}
                >
                  {shareError ? (
                    <span className="nvr-badge alert" role="alert">
                      {shareError}
                    </span>
                  ) : (
                    shareFallbackUrl && (
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: 8,
                          flex: 1,
                          minWidth: 0,
                        }}
                      >
                        <span style={{ color: "var(--text-secondary)", flexShrink: 0 }}>
                          {t.review.shareManualCopyHint}
                        </span>
                        <input
                          type="text"
                          readOnly
                          className="nvr-input mono"
                          aria-label="Generated review share link URL"
                          value={shareFallbackUrl}
                          onFocus={(e) => e.currentTarget.select()}
                          onClick={(e) => e.currentTarget.select()}
                          style={{
                            flex: 1,
                            minWidth: 0,
                            height: 24,
                            fontSize: "var(--text-xs)",
                          }}
                        />
                      </div>
                    )
                  )}
                </div>
              )}

              {showForensic && (
                <div
                  className="mono"
                  style={{
                    background: "var(--bg-canvas)",
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
                    const p = activeReview.data?.provenance;
                    const topScore =
                      typeof p?.top_score === "number" && Number.isFinite(p.top_score)
                        ? p.top_score
                        : 0.88;
                    const thresh =
                      typeof p?.threshold_cfg === "number" &&
                      Number.isFinite(p.threshold_cfg)
                        ? p.threshold_cfg
                        : 0.7;
                    const areaPx =
                      typeof p?.area_px === "number" && Number.isFinite(p.area_px)
                        ? Math.max(0, Math.round(p.area_px))
                        : 0;
                    const areaPct =
                      typeof p?.area_pct === "number" && Number.isFinite(p.area_pct)
                        ? Math.max(0, p.area_pct)
                        : 0;
                    const ratio =
                      typeof p?.aspect_ratio === "number" &&
                      Number.isFinite(p.aspect_ratio)
                        ? p.aspect_ratio
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
                      (p?.zones && p.zones.length > 0 ? p.zones : activeReview.data?.zones) || [];
                    const fallbackStart =
                      typeof activeReview.start_time === "number" &&
                      Number.isFinite(activeReview.start_time) &&
                      activeReview.start_time > 0
                        ? activeReview.start_time
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
                          {activeReview.data?.detected_attributes &&
                            activeReview.data.detected_attributes.length > 0 && (
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
                                {activeReview.data.detected_attributes.map((attr, idx) => (
                                  <span
                                    key={`${attr.label}-${idx}`}
                                    className="nvr-badge mono"
                                    style={{
                                      fontFamily: "var(--font-mono)",
                                      fontSize: "var(--text-xs)",
                                    }}
                                  >
                                    {formatDetectedAttributePill(attr)}
                                  </span>
                                ))}
                              </span>
                            )}
                        </div>
                        <button
                          type="button"
                          className="nvr-btn mono"
                          style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                          onClick={() =>
                            setReviewPlayhead({
                              reviewId: activeReview.id,
                              ts: triggerTs,
                            })
                          }
                        >
                          Jump to Trigger Frame
                        </button>
                      </>
                    );
                  })()}
                </div>
              )}

              {briefingActive && (
                <div
                  className="mono"
                  style={{
                    padding: "6px 12px",
                    background: "rgba(14, 165, 233, 0.14)",
                    borderBottom: "1px solid var(--signal-accent)",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 10,
                    fontSize: "var(--text-xs)",
                    fontVariantNumeric: "tabular-nums",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
                    <strong style={{ color: "var(--signal-accent-text)" }}>
                      BRIEFING: {briefingIdx + 1} / {briefingQueueIds.length} •{" "}
                      {briefingSpeed.toFixed(1)}x
                    </strong>
                    {([1, 2, 4] as const).map((spd) => (
                      <button
                        key={spd}
                        type="button"
                        className={`nvr-btn mono ${briefingSpeed === spd ? "active" : ""}`}
                        style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                        onClick={() => {
                          setBriefingSpeed(spd);
                          setPlaybackSpeed(spd);
                        }}
                      >
                        {spd}x
                      </button>
                    ))}
                    <label
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 5,
                        cursor: "pointer",
                      }}
                    >
                      <input
                        type="checkbox"
                        checked={briefingAutoMark}
                        onChange={(e) => setBriefingAutoMark(e.target.checked)}
                      />
                      Auto-mark reviewed on completion
                    </label>
                  </div>
                  <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                    <button
                      type="button"
                      className="nvr-btn mono"
                      style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                      onClick={handleAdvanceBriefing}
                    >
                      SKIP NEXT
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger mono"
                      style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                      onClick={() => setBriefingActive(false)}
                    >
                      STOP
                    </button>
                  </div>
                </div>
              )}

              <div className="nvr-cam-tile" style={{ flex: 1 }}>
                {(() => {
                  const rawMock = String(
                    (activeReview.data as Record<string, unknown> | undefined)
                      ?.mock_video || ""
                  ).trim();
                  const safeMock =
                    rawMock &&
                    rawMock !== "." &&
                    !rawMock.includes("..") &&
                    !/[/\\%\u0000-\u001f\u007f]/.test(rawMock)
                      ? rawMock
                      : "";
                  if (safeMock) {
                    return (
                      <video
                        key={activeReview.id}
                        src={`/clips/mocks/${encodeURIComponent(safeMock)}`}
                        controls
                        autoPlay
                        loop
                        muted
                        playsInline
                        style={{
                          width: "100%",
                          height: "100%",
                          objectFit: "contain",
                          background: "var(--video-stage-bg)",
                        }}
                      />
                    );
                  }
                  const currentVodWindow =
                    loadedVodWindowRef.current?.reviewId === activeReview.id
                      ? loadedVodWindowRef.current.window
                      : null;
                  const isDraggingReviewTrim =
                    activeReviewTrim?.mode === "dragging_in" ||
                    activeReviewTrim?.mode === "dragging_out";
                  const isSettledOrPreviewReviewTrim =
                    activeReviewTrim?.mode === "settled_in" ||
                    activeReviewTrim?.mode === "settled_out" ||
                    activeReviewTrim?.mode === "preview_clip";
                  const effectiveReviewPlayhead =
                    isSettledOrPreviewReviewTrim &&
                    activeReviewTrim &&
                    Number.isFinite(activeReviewTrim.targetTs)
                      ? activeReviewTrim.targetTs
                      : reviewPlayheadTs;
                  const basePlayback = resolveReviewArchivePlayback(
                    activeReview,
                    effectiveReviewPlayhead,
                    {
                      currentWindow: currentVodWindow,
                      configuredCameras,
                    }
                  );
                  const vodWin =
                    activeReviewTrim?.mode === "preview_clip"
                      ? resolveVodSeekAction(basePlayback.vodWindow, {
                          camera: basePlayback.realCamera,
                          targetTs: activeReviewTrim.rangeStart,
                          requiredEndTs: activeReviewTrim.rangeEnd,
                          windowSpanSec: Math.max(
                            60,
                            Math.min(
                              86400,
                              activeReviewTrim.rangeEnd -
                                activeReviewTrim.rangeStart +
                                12
                            )
                          ),
                          configuredCameras,
                        }).window
                      : basePlayback.vodWindow;
                  loadedVodWindowRef.current = {
                    reviewId: activeReview.id,
                    window: vodWin,
                  };
                  return (
                    <VodHlsPlayer
                      key={activeReview.id}
                      camera={basePlayback.realCamera}
                      startTime={vodWin.startSec}
                      endTime={vodWin.endSec}
                      playheadTs={effectiveReviewPlayhead}
                      scrubTargetTs={
                        isDraggingReviewTrim ? activeReviewTrim?.targetTs : null
                      }
                      trimScrubChannel={
                        isDraggingReviewTrim ? reviewTrimScrubChannel : null
                      }
                      pauseOnSeek={
                        isDraggingReviewTrim ||
                        activeReviewTrim?.mode === "settled_in" ||
                        activeReviewTrim?.mode === "settled_out"
                      }
                      clipBounds={
                        activeReviewTrim?.mode === "preview_clip"
                          ? {
                              startSec: activeReviewTrim.rangeStart,
                              endSec: activeReviewTrim.rangeEnd,
                              loop: true,
                            }
                          : null
                      }
                      scrubPreviewUrl={
                        isDraggingReviewTrim
                          ? activeReviewTrim?.previewFrameUrl
                          : null
                      }
                      trimBadgeLabel={activeReviewTrim?.badgeLabel ?? null}
                      fallbackPreviewId={activeReview.id}
                      playbackRate={briefingActive ? briefingSpeed : playbackSpeed}
                      onEnded={briefingActive ? handleAdvanceBriefing : undefined}
                    />
                  );
                })()}
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
                  style={{ width: 145, height: 26, fontSize: "var(--text-xs)" }}
                  aria-label="Assign Face Name"
                  placeholder="Person name (e.g. Bruno)..."
                  value={reviewFaceDraft}
                  onChange={(e) => setReviewFaceDraft(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn primary mono"
                  style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                  disabled={Boolean(batchReviewFaceProgress?.active)}
                  onClick={() => void handleAssignReviewFace()}
                  title="Assigns person name to selected review(s) and automatically enrolls 112x112 face crop into Face Gallery"
                >
                  {batchReviewFaceProgress?.active
                    ? `Enrolling ${batchReviewFaceProgress.completed}/${batchReviewFaceProgress.total}...`
                    : selectedBatchReviewIds.length > 0
                    ? `Assign & Enroll (${selectedBatchReviewIds.length} Selected)`
                    : "Assign & Enroll Face"}
                </button>
                {enrolledFaces.map((p) => (
                  <button
                    key={`rev-quick-face-${p.name}`}
                    type="button"
                    className={`nvr-btn mono ${
                      (activeReview.data?.sub_labels || []).some(
                        (s) => s.trim().toLowerCase() === p.name.trim().toLowerCase()
                      )
                        ? "active"
                        : ""
                    }`}
                    style={{ height: 26, padding: "0 8px", fontSize: "var(--text-xs)" }}
                    onClick={() => {
                      setReviewFaceDraft(p.name);
                      void handleAssignReviewFace(p.name);
                    }}
                    title={`1-click assign & enroll face sample into '${p.name}' gallery (${p.sample_count} samples)`}
                  >
                    + {p.name} ({p.sample_count})
                  </button>
                ))}
                {(activeReview.data?.sub_labels?.length ?? 0) > 0 && (
                  <button
                    type="button"
                    className="nvr-btn danger mono"
                    style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                    aria-label="Clear or unassign review face sub-label"
                    onClick={() => {
                      setReviewFaceDraft("");
                      void handleAssignReviewFace(null);
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
                  aria-label="Manual review license plate override"
                  style={{
                    width: 120,
                    height: 26,
                    fontSize: "var(--text-xs)",
                    fontFamily: "var(--font-mono)",
                  }}
                  placeholder="ABC1D23"
                  value={reviewPlateDraft}
                  onChange={(e) => setReviewPlateDraft(e.target.value.toUpperCase())}
                />
                <button
                  type="button"
                  className="nvr-btn mono"
                  style={{ height: 26, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={handleSaveReviewLicensePlate}
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
                  placeholder="Attach operator annotation to this review segment..."
                  value={reviewNoteDraft}
                  onChange={(e) => setReviewNoteDraft(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn primary mono"
                  style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                  onClick={handleSaveReviewNote}
                >
                  Save Note
                </button>
              </div>

              <TimelineScrubber
                camera={activeReview.camera}
                segments={focusSegments}
                reviews={reviews}
                currentTs={
                  reviewPlayheadTs !== null && Number.isFinite(reviewPlayheadTs)
                    ? Math.floor(reviewPlayheadTs)
                    : typeof activeReview.start_time === "number" &&
                      Number.isFinite(activeReview.start_time) &&
                      activeReview.start_time > 0
                    ? Math.floor(activeReview.start_time)
                    : Math.floor(Date.now() / 1000) - 20
                }
                onSeek={(ts) => {
                  if (ts !== null && Number.isFinite(ts)) {
                    setReviewPlayhead({ reviewId: activeReview.id, ts });
                  } else {
                    setReviewPlayhead(null);
                  }
                }}
                trimScrubChannel={reviewTrimScrubChannel}
                onTrimFeedback={(state) => {
                  if (state) {
                    setReviewTrimFeedback({
                      reviewId: activeReview.id,
                      state,
                    });
                  } else {
                    setReviewTrimFeedback(null);
                  }
                }}
                onOpenExports={() => useNvrStore.getState().setActiveTab("exports")}
                onExportRange={async (cam, start, end, label, includeAudio) => {
                  const cleanCam = (cam || "").trim();
                  if (!cleanCam) return;
                  const win = validateExportTimeWindow(start, end);
                  if (!win.valid) return;
                  const realRangeCam = resolveRealCameraSource(
                    cleanCam,
                    configuredCameras
                  );
                  return handleQuickExport(
                    realRangeCam,
                    win.startSec,
                    win.endSec,
                    label,
                    includeAudio
                  );
                }}
                titlePrefix={`REVIEW TIMELINE (${activeReview.camera.toUpperCase()})`}
              />
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
              Select a review segment on the left to inspect
            </div>
          )}
        </div>
      </div>
    </>
  );
};
