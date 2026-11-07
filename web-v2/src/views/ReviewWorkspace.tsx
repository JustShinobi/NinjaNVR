import React from "react";
import { Check, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import {
  EnrolledFaceProfileItem,
  ExportItem,
  RecordingDaySummary,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import { WorkspaceTab, findMatchingExport, formatDateTime, formatRelTime } from "../utils/format";
import { buildSharePublicUrl } from "../utils/shareLinks";
import { VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";
import { RecordingCalendarPopover } from "../components/RecordingCalendarPopover";
import {
  applyReviewSubLabelUpdate,
  clearFaceProfileFromReviews,
  formatDetectedAttributePill,
  formatEventLabelWithSubLabel,
  formatLicensePlateBadge,
  formatSubLabelBadge,
  normalizeLicensePlate,
} from "./ExploreWorkspace";
import {
  MAX_EXPORT_WINDOW_SEC,
  validateExportTimeWindow,
} from "./ExportsWorkspace";
import {
  normalizeFaceProfileName,
  parseFacesApiResponse,
} from "./settings/VisionAndAiPanels";

export interface SavedReviewFilterPreset {
  name: string;
  camera: string;
  severity: string;
  object: string;
  excludeObject: string;
  zone: string;
  status: "unreviewed" | "reviewed" | "all";
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
  const limit = Number.isFinite(pageSize) && pageSize > 0 ? Math.floor(pageSize) : 40;
  return visibleReviews
    .slice(0, limit)
    .map((r) => r.id)
    .filter((id): id is string => typeof id === "string" && id.length > 0);
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
    startSec + MAX_EXPORT_WINDOW_SEC,
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
    label?: string
  ) => Promise<{ exportId?: string; videoUrl?: string } | void>;
  setSyncMasterTs: (ts: number) => void;
  switchTab: (tab: WorkspaceTab) => void;
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
}) => {
  const { t } = useI18n();
  const playbackSpeed = useNvrStore((s) => s.playbackSpeed);
  const setPlaybackSpeed = useNvrStore((s) => s.setPlaybackSpeed);
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
  const [batchReviewFaceProgress, setBatchReviewFaceProgress] = React.useState<{
    active: boolean;
    completed: number;
    total: number;
    name: string;
  } | null>(null);
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
  const reviewMutationSeqRef = React.useRef(0);
  const activeReviewIdRef = React.useRef<string | null>(activeReview?.id ?? null);
  activeReviewIdRef.current = activeReview?.id ?? null;

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
    activeReviewIdRef.current = activeReview?.id ?? null;
    reviewMutationSeqRef.current += 1;
    if (shareCopiedTimerRef.current !== null) {
      window.clearTimeout(shareCopiedTimerRef.current);
      shareCopiedTimerRef.current = null;
    }
    setShareError(null);
    setShareFallbackUrl(null);
    setShareCopied(false);
    setShareCreating(false);
  }, [activeReview?.id]);

  React.useEffect(() => {
    if (!activeReview || !reviews.some((r) => r.id === activeReview.id)) {
      activeReviewIdRef.current = null;
      reviewMutationSeqRef.current += 1;
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
  }, [reviews, activeReview]);

  React.useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
      if (shareCopiedTimerRef.current !== null) {
        window.clearTimeout(shareCopiedTimerRef.current);
        shareCopiedTimerRef.current = null;
      }
    };
  }, []);

  React.useEffect(() => {
    if (!lastDeletedFaceProfile?.name || !activeReview) return;
    const deletedLower = lastDeletedFaceProfile.name.trim().toLowerCase();
    if (
      (activeReview.data?.sub_labels || []).some(
        (s) => s.trim().toLowerCase() === deletedLower
      )
    ) {
      const [cleared] = clearFaceProfileFromReviews(
        [activeReview],
        lastDeletedFaceProfile.name
      );
      setActiveReview(cleared);
      setReviewFaceDraft("");
    }
  }, [lastDeletedFaceProfile, activeReview, setActiveReview]);

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
      for (const rid of targetRevIds) {
        const revObj = reviews.find((r) => r.id === rid);
        const allDets = (revObj?.data?.detections || [])
          .map((d) => d.trim())
          .filter((d) => d.length > 0);
        const detIds = allDets.length > 0 ? allDets : [rid];
        for (const did of detIds) {
          await fetch(`/api/events/${encodeURIComponent(did)}/sub_label`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({ sub_label: null }),
          }).catch(() => null);
        }
        setLocalReviewSubLabels((prev) => ({ ...prev, [rid]: null }));
      }
      if (activeReview && targetRevIds.includes(activeReview.id)) {
        const [updated] = applyReviewSubLabelUpdate(
          [activeReview],
          activeReview.id,
          null
        );
        setActiveReview(updated);
      }
      setSelectedBatchReviewIds([]);
      bumpSubLabelsRefresh();
      return;
    }
    if (!activeReview) return;
    const targetReview = activeReview;
    const targetId = targetReview.id;
    const detId = (targetReview.data?.detections?.[0] || targetId).trim();
    if (!detId) return;
    const seq = ++reviewMutationSeqRef.current;
    try {
      let anyOk = false;
      if (clean) {
        const res = await fetch(`/api/faces/${encodeURIComponent(clean)}/enroll`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({
            event_id: detId,
            assign_sub_label: true,
            backfill_events: true,
          }),
        });
        if (res.ok) {
          anyOk = true;
        } else {
          const fbRes = await fetch(`/api/events/${encodeURIComponent(detId)}/sub_label`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({
              sub_label: clean,
              sub_label_score: 1.0,
              enroll_face: true,
            }),
          });
          if (fbRes.ok) anyOk = true;
        }
      } else {
        const allDets = (targetReview.data?.detections || [])
          .map((d) => d.trim())
          .filter((d) => d.length > 0);
        const targetDetIds = allDets.length > 1 ? allDets : [detId];
        for (const did of targetDetIds) {
          const res = await fetch(`/api/events/${encodeURIComponent(did)}/sub_label`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({ sub_label: null }),
          });
          if (res.ok) anyOk = true;
        }
      }
      if (anyOk && isMountedRef.current) {
        setLocalReviewSubLabels((prev) => ({ ...prev, [targetId]: clean }));
      }
      if (
        isMountedRef.current &&
        seq === reviewMutationSeqRef.current &&
        shouldApplyReviewMutationResult(
          activeReviewIdRef.current,
          targetId,
          anyOk
        )
      ) {
        const [updated] = applyReviewSubLabelUpdate(
          [targetReview],
          targetId,
          clean
        );
        setActiveReview(updated);
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
    setBatchReviewFaceProgress({
      active: true,
      completed: 0,
      total: targetRevIds.length,
      name: cleanName,
    });
    try {
      for (let i = 0; i < targetRevIds.length; i += 1) {
        if (!isMountedRef.current) break;
        const rid = targetRevIds[i];
        const revObj = reviews.find((r) => r.id === rid);
        const detId = (revObj?.data?.detections?.[0] || rid).trim();
        const isLast = i === targetRevIds.length - 1;
        if (detId) {
          const res = await fetch(
            `/api/faces/${encodeURIComponent(cleanName)}/enroll`,
            {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              credentials: "include",
              body: JSON.stringify({
                event_id: detId,
                assign_sub_label: true,
                backfill_events: isLast,
              }),
            }
          );
          if (res.ok) {
            if (isMountedRef.current) {
              setLocalReviewSubLabels((prev) => ({ ...prev, [rid]: cleanName }));
            }
            if (activeReview && activeReview.id === rid) {
              const [updated] = applyReviewSubLabelUpdate(
                [activeReview],
                rid,
                cleanName
              );
              setActiveReview(updated);
            }
          }
        }
        if (isMountedRef.current) {
          setBatchReviewFaceProgress({
            active: true,
            completed: i + 1,
            total: targetRevIds.length,
            name: cleanName,
          });
        }
      }
      if (isMountedRef.current) {
        bumpSubLabelsRefresh();
        setSelectedBatchReviewIds([]);
      }
    } catch {
      // ignore transient error
    } finally {
      if (isMountedRef.current) {
        setBatchReviewFaceProgress(null);
      }
    }
  };

  const handleSaveReviewLicensePlate = async () => {
    if (!activeReview) return;
    const targetReview = activeReview;
    const targetId = targetReview.id;
    const norm = normalizeLicensePlate(reviewPlateDraft);
    const primaryEventId = (targetReview.data?.detections?.[0] || targetId).trim();
    if (!primaryEventId) return;
    const seq = ++reviewMutationSeqRef.current;
    try {
      const res = await fetch(
        `/api/events/${encodeURIComponent(primaryEventId)}/license_plate`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ recognized_license_plate: norm || null }),
        }
      );
      if (
        isMountedRef.current &&
        seq === reviewMutationSeqRef.current &&
        shouldApplyReviewMutationResult(
          activeReviewIdRef.current,
          targetId,
          res.ok
        )
      ) {
        setActiveReview({
          ...targetReview,
          data: {
            ...targetReview.data,
            recognized_license_plates: norm ? [norm] : [],
          },
        });
        setReviewPlateDraft(norm);
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
    const seq = ++reviewMutationSeqRef.current;
    try {
      const res = await fetch(`/api/review/${encodeURIComponent(targetId)}/notes`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ notes: noteToSave }),
      });
      if (
        isMountedRef.current &&
        seq === reviewMutationSeqRef.current &&
        shouldApplyReviewMutationResult(
          activeReviewIdRef.current,
          targetId,
          res.ok
        )
      ) {
        setActiveReview({
          ...targetReview,
          data: { ...targetReview.data, notes: noteToSave },
        });
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
  >(() => {
    try {
      const raw = window.localStorage?.getItem("ninjanvr_saved_filters");
      return parseSavedReviewPresets(raw);
    } catch {
      return [];
    }
  });

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
      const normPlateQuery = normalizeLicensePlate(plateFilter);
      if (normPlateQuery) {
        const plates = (r.data?.recognized_license_plates || []).map(normalizeLicensePlate);
        const subs = (r.data?.sub_labels || []).map(normalizeLicensePlate);
        const matched =
          plates.some((p) => p.includes(normPlateQuery)) ||
          subs.some((s) => s.includes(normPlateQuery));
        if (!matched) return false;
      }
      return true;
    });
  }, [filteredReviews, excludeObjectFilter, zoneFilter, plateFilter]);

  const handleSavePreset = () => {
    const label =
      presetName.trim() ||
      `${reviewCameraFilter === "all" ? "ALL" : reviewCameraFilter.toUpperCase()} · ${reviewSeverityFilter.toUpperCase()}`;
    const next = [
      ...savedPresets.filter((p) => p.name !== label),
      {
        name: label,
        camera: reviewCameraFilter,
        severity: reviewSeverityFilter,
        object: reviewObjectFilter,
        excludeObject: excludeObjectFilter,
        zone: zoneFilter,
        status: reviewStatusFilter,
      },
    ];
    setSavedPresets(next);
    window.localStorage?.setItem("ninjanvr_saved_filters", JSON.stringify(next));
    setPresetName("");
  };

  const handleStartBriefing = () => {
    const queue = visibleFilteredReviews.slice(0, 40);
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
      <div className="nvr-subbar">
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
                  const next = savedPresets.filter((x) => x.name !== p.name);
                  setSavedPresets(next);
                  window.localStorage?.setItem("ninjanvr_saved_filters", JSON.stringify(next));
                }}
              >
                ×
              </button>
            </span>
          ))}
          <span
            className="mono"
            style={{
              color: "var(--text-muted)",
              fontSize: "var(--text-xs)",
              whiteSpace: "nowrap",
            }}
          >
            {visibleFilteredReviews.length} items
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
                : `▶ Play Tactical Brief (${Math.min(40, visibleFilteredReviews.length)})`}
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
            {t.review.keyboardHint}
          </span>
          {[0.5, 1, 2, 4].map((spd) => (
            <button
              key={spd}
              className={`nvr-btn mono ${playbackSpeed === spd ? "active" : ""}`}
              onClick={() => setPlaybackSpeed(spd)}
            >
              {spd}x
            </button>
          ))}
        </div>
      </div>

      <div className="nvr-workbench">
        <div className="nvr-panel">
          <div className="nvr-panel-header" style={{ gap: 6, flexWrap: "wrap" }}>
            <span>REVIEW QUEUE</span>
            <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
              {visibleFilteredReviews.length > 0 && (
                <>
                  <button
                    type="button"
                    className="nvr-btn mono sm"
                    style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                    onClick={() => {
                      const pageIds = getVisibleReviewPageIds(visibleFilteredReviews, 40);
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
                    className="nvr-btn success"
                    style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                    onClick={() =>
                      handleMarkReviews(
                        selectedBatchReviewIds.length > 0
                          ? selectedBatchReviewIds
                          : getVisibleReviewPageIds(visibleFilteredReviews, 40),
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
            {visibleFilteredReviews.slice(0, 40).map((item) => {
              const isSelected = activeReview?.id === item.id;
              const isBatchChecked = selectedBatchReviewIds.includes(item.id);
              const matchedExport = findMatchingExport(
                exportsList,
                item.camera,
                item.start_time,
                item.end_time
              );
              const thumbUrl = resolveReviewThumbUrl(item);
              return (
                <div
                  key={item.id}
                  role="button"
                  tabIndex={0}
                  className={`nvr-list-row ${isSelected ? "is-selected" : ""}`}
                  onClick={() => {
                    if (briefingActive) {
                      const qIdx = briefingQueueIds.indexOf(item.id);
                      if (qIdx >= 0) setBriefingIdx(qIdx);
                    }
                    setActiveReview(item);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      if (briefingActive) {
                        const qIdx = briefingQueueIds.indexOf(item.id);
                        if (qIdx >= 0) setBriefingIdx(qIdx);
                      }
                      setActiveReview(item);
                    }
                  }}
                  style={{
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
                    onClick={(e) => e.stopPropagation()}
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
                    }}
                  />
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                      }}
                    >
                      <span style={{ fontWeight: 700, textTransform: "uppercase" }}>
                        {item.camera}
                      </span>
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
                    </div>
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
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
                          item.id in localReviewSubLabels
                            ? localReviewSubLabels[item.id]
                            : item.data?.sub_labels?.[0] ?? null,
                          item.data?.detected_attributes
                        )}
                      </span>
                      <div className="nvr-row-actions">
                        {(() => {
                          const effSub =
                            item.id in localReviewSubLabels
                              ? localReviewSubLabels[item.id]
                              : item.data?.sub_labels?.[0] ?? null;
                          const subBadge = formatSubLabelBadge(
                            effSub,
                            item.data?.sub_label_score ?? (effSub ? 1.0 : null)
                          );
                          if (!subBadge) return null;
                          return (
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
                          );
                        })()}
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
                        {matchedExport && (
                          <button
                            className="nvr-badge live mono"
                            style={{
                              height: 20,
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
                        {!item.has_been_reviewed && (
                          <button
                            className="nvr-btn success"
                            style={{ height: 22, padding: "0 6px" }}
                            aria-label={`Mark ${item.camera} review item as viewed`}
                            title={t.live.markReviewed}
                            onClick={(e) => {
                              e.stopPropagation();
                              handleMarkReviews([item.id], true);
                            }}
                          >
                            <Check size={11} />
                          </button>
                        )}
                        <button
                          className="nvr-btn danger"
                          style={{ height: 22, padding: "0 6px" }}
                          aria-label={`Delete ${item.camera} review item`}
                          title={t.review.deleteSelected}
                          onClick={(e) => {
                            e.stopPropagation();
                            handleDeleteReviews([item.id]);
                          }}
                        >
                          <Trash2 size={11} />
                        </button>
                      </div>
                    </div>
                  </div>
                </div>
              );
            })}
            {visibleFilteredReviews.length === 0 && (
              <div
                style={{
                  padding: 24,
                  textAlign: "center",
                  color: "var(--text-muted)",
                }}
              >
                {t.review.emptyState}
              </div>
            )}
          </div>
        </div>

        <div className="nvr-focus-stage">
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
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexShrink: 0 }}>
                  <button
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
                    className="nvr-btn primary"
                    onClick={() => {
                      const win = computeReviewPlaybackWindow(
                        activeReview.start_time,
                        activeReview.end_time,
                        5,
                        5,
                        20
                      );
                      const checked = validateExportTimeWindow(
                        win.startSec,
                        win.endSec
                      );
                      if (!checked.valid) return;
                      void handleQuickExport(
                        activeReview.camera,
                        checked.startSec,
                        checked.endSec,
                        `${activeReview.camera}_${activeReview.id}`
                      );
                    }}
                  >
                    {t.review.exportClip}
                  </button>
                  <button
                    className="nvr-btn success"
                    onClick={() => handleMarkReviews([activeReview.id], true)}
                  >
                    <Check size={12} />
                    {t.review.markSelectedReviewed}
                  </button>
                </div>
              </div>

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
                            setActiveReview({
                              ...activeReview,
                              start_time: triggerTs,
                              end_time: triggerTs + 25,
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
                  const vodWin = computeReviewPlaybackWindow(
                    activeReview.start_time,
                    activeReview.end_time,
                    4,
                    6,
                    20
                  );
                  return (
                    <VodHlsPlayer
                      key={activeReview.id}
                      camera={activeReview.camera}
                      startTime={vodWin.startSec}
                      endTime={vodWin.endSec}
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
                  typeof activeReview.start_time === "number" &&
                  Number.isFinite(activeReview.start_time) &&
                  activeReview.start_time > 0
                    ? Math.floor(activeReview.start_time)
                    : Math.floor(Date.now() / 1000) - 20
                }
                onSeek={(ts) => {
                  if (ts !== null && Number.isFinite(ts)) {
                    setActiveReview({
                      ...activeReview,
                      start_time: ts,
                      end_time: ts + 30,
                    });
                  }
                }}
                onOpenExports={() => useNvrStore.getState().setActiveTab("exports")}
                onExportRange={async (cam, start, end, label) => {
                  const cleanCam = (cam || "").trim();
                  if (!cleanCam) return;
                  const win = validateExportTimeWindow(start, end);
                  if (!win.valid) return;
                  return handleQuickExport(
                    cleanCam,
                    win.startSec,
                    win.endSec,
                    label
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
