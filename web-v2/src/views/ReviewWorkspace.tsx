import React from "react";
import { Check, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import { ExportItem, RecordingDaySummary, RecordingSegment, ReviewItem } from "../types/nvr";
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
import { normalizeFaceProfileName } from "./settings/VisionAndAiPanels";

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
  const [reviewPlateDraft, setReviewPlateDraft] = React.useState("");
  const lastDeletedFaceProfile = useNvrStore((s) => s.lastDeletedFaceProfile);
  const bumpSubLabelsRefresh = useNvrStore((s) => s.bumpSubLabelsRefresh);
  const [shareTtlSec, setShareTtlSec] = React.useState<number>(86400);
  const [shareMaxViews, setShareMaxViews] = React.useState<number>(0);
  const [shareCopied, setShareCopied] = React.useState(false);
  const [shareError, setShareError] = React.useState<string | null>(null);
  const [shareFallbackUrl, setShareFallbackUrl] = React.useState<string | null>(null);
  const [shareCreating, setShareCreating] = React.useState(false);
  const shareCopiedTimerRef = React.useRef<number | null>(null);
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
      if (shareCopiedTimerRef.current !== null) {
        window.clearTimeout(shareCopiedTimerRef.current);
        shareCopiedTimerRef.current = null;
      }
      setShareError(null);
      setShareFallbackUrl(null);
      setShareCopied(false);
      setShareCreating(false);
    }
  }, [reviews, activeReview]);

  React.useEffect(() => {
    return () => {
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

  const handleAssignReviewFace = async (overrideName?: string | null) => {
    if (!activeReview) return;
    const detId = activeReview.data?.detections?.[0] || activeReview.id;
    const raw = overrideName !== undefined ? overrideName : reviewFaceDraft;
    const clean = raw && raw.trim().length > 0 ? raw.trim() : null;
    const res = await fetch(`/api/events/${encodeURIComponent(detId)}/sub_label`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ sub_label: clean }),
    });
    if (res.ok) {
      const [updated] = applyReviewSubLabelUpdate(
        [activeReview],
        activeReview.id,
        clean
      );
      setActiveReview(updated);
      bumpSubLabelsRefresh();
    }
  };

  const handleEnrollReviewFace = async () => {
    if (!activeReview) return;
    const cleanName = normalizeFaceProfileName(reviewFaceDraft);
    if (!cleanName) return;
    const detId = activeReview.data?.detections?.[0] || activeReview.id;
    const res = await fetch(`/api/faces/${encodeURIComponent(cleanName)}/enroll`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ event_id: detId, assign_sub_label: true }),
    });
    if (res.ok) {
      const [updated] = applyReviewSubLabelUpdate(
        [activeReview],
        activeReview.id,
        cleanName
      );
      setActiveReview(updated);
    }
  };

  const handleSaveReviewLicensePlate = async () => {
    if (!activeReview) return;
    const norm = normalizeLicensePlate(reviewPlateDraft);
    const primaryEventId = activeReview.data?.detections?.[0] || activeReview.id;
    await fetch(`/api/events/${encodeURIComponent(primaryEventId)}/license_plate`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ recognized_license_plate: norm || null }),
    }).catch(() => {});
    setActiveReview({
      ...activeReview,
      data: {
        ...activeReview.data,
        recognized_license_plates: norm ? [norm] : [],
      },
    });
    setReviewPlateDraft(norm);
  };

  const handleSaveReviewNote = async () => {
    if (!activeReview) return;
    const res = await fetch(`/api/review/${encodeURIComponent(activeReview.id)}/notes`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ notes: reviewNoteDraft }),
    });
    if (res.ok) {
      setActiveReview({
        ...activeReview,
        data: { ...activeReview.data, notes: reviewNoteDraft },
      });
    }
  };
  const [briefingActive, setBriefingActive] = React.useState(false);
  const [briefingSpeed, setBriefingSpeed] = React.useState<1 | 2 | 4>(2);
  const [briefingAutoMark, setBriefingAutoMark] = React.useState(true);
  const [briefingQueueIds, setBriefingQueueIds] = React.useState<string[]>([]);
  const [briefingIdx, setBriefingIdx] = React.useState(0);
  const [savedPresets, setSavedPresets] = React.useState<
    Array<{
      name: string;
      camera: string;
      severity: string;
      object: string;
      excludeObject: string;
      zone: string;
      status: "unreviewed" | "reviewed" | "all";
    }>
  >(() => {
    try {
      const raw = window.localStorage?.getItem("ninjanvr_saved_filters");
      return raw ? JSON.parse(raw) : [];
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
    const currentId = briefingQueueIds[briefingIdx] || activeReview?.id;
    if (briefingAutoMark && currentId) {
      void handleMarkReviews([currentId], true);
    }
    const nextIdx = briefingIdx + 1;
    if (nextIdx >= briefingQueueIds.length) {
      setBriefingActive(false);
      return;
    }
    const nextId = briefingQueueIds[nextIdx];
    const nextItem = reviews.find((r) => r.id === nextId);
    setBriefingIdx(nextIdx);
    if (nextItem) {
      setActiveReview(nextItem);
    } else {
      setBriefingActive(false);
    }
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
          <div className="nvr-panel-header">
            <span>REVIEW QUEUE</span>
            {filteredReviews.length > 0 && (
              <button
                className="nvr-btn success"
                style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                onClick={() =>
                  handleMarkReviews(
                    filteredReviews.slice(0, 40).map((r) => r.id),
                    true
                  )
                }
              >
                <Check size={11} />
                Mark Page Viewed
              </button>
            )}
          </div>
          <div className="nvr-scroll-y">
            {visibleFilteredReviews.slice(0, 40).map((item) => {
              const isSelected = activeReview?.id === item.id;
              const matchedExport = findMatchingExport(
                exportsList,
                item.camera,
                item.start_time,
                item.end_time
              );
              const thumbUrl = item.thumb_path
                ? item.thumb_path.replace(/^\/media\/[^/]+/, "")
                : `/api/${item.camera}/latest.webp`;
              return (
                <div
                  key={item.id}
                  role="button"
                  tabIndex={0}
                  className={`nvr-list-row ${isSelected ? "is-selected" : ""}`}
                  onClick={() => setActiveReview(item)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
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
                          item.data?.sub_labels?.[0] ?? null,
                          item.data?.detected_attributes
                        )}
                      </span>
                      <div className="nvr-row-actions">
                        {(() => {
                          const subBadge = formatSubLabelBadge(
                            item.data?.sub_labels?.[0] ?? null,
                            item.data?.sub_label_score
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
            {filteredReviews.length === 0 && (
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
                      setSyncMasterTs(Math.floor(activeReview.start_time));
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
                        if (activeReviewIdRef.current !== requestReviewId) {
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
                        if (activeReviewIdRef.current !== requestReviewId) {
                          return;
                        }
                        if (copiedOk) {
                          setShareCopied(true);
                          shareCopiedTimerRef.current = window.setTimeout(() => {
                            if (activeReviewIdRef.current === requestReviewId) {
                              setShareCopied(false);
                            }
                            shareCopiedTimerRef.current = null;
                          }, 2000);
                        } else {
                          setShareCopied(false);
                        }
                      } catch {
                        if (activeReviewIdRef.current !== requestReviewId) {
                          return;
                        }
                        setShareError(t.review.shareError);
                        setShareCopied(false);
                      } finally {
                        if (activeReviewIdRef.current === requestReviewId) {
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
                    onClick={() =>
                      handleQuickExport(
                        activeReview.camera,
                        activeReview.start_time - 5,
                        (activeReview.end_time || activeReview.start_time + 20) + 5,
                        `${activeReview.camera}_${activeReview.id}`
                      )
                    }
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
                    const topScore = p?.top_score ?? 0.88;
                    const thresh = p?.threshold_cfg ?? 0.7;
                    const areaPx = p?.area_px ?? 0;
                    const areaPct = p?.area_pct ?? 0;
                    const ratio = p?.aspect_ratio ?? 0.5;
                    const bbox = p?.bbox ?? [0, 0, 0, 0];
                    const anchorX = Math.round((bbox[0] + bbox[2]) / 2);
                    const anchorY = bbox[3];
                    const zones =
                      (p?.zones && p.zones.length > 0 ? p.zones : activeReview.data?.zones) || [];
                    const triggerTs = p?.trigger_ts ?? activeReview.start_time;
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
                {(activeReview.data as Record<string, unknown> | undefined)
                  ?.mock_video ? (
                  <video
                    src={`/clips/mocks/${encodeURIComponent(
                      String(
                        (activeReview.data as Record<string, unknown>).mock_video
                      )
                    )}`}
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
                ) : (
                  <VodHlsPlayer
                    camera={activeReview.camera}
                    startTime={activeReview.start_time - 4}
                    endTime={(activeReview.end_time || activeReview.start_time + 20) + 6}
                    fallbackPreviewId={activeReview.id}
                    playbackRate={briefingActive ? briefingSpeed : playbackSpeed}
                    onEnded={briefingActive ? handleAdvanceBriefing : undefined}
                  />
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
                  style={{ width: 145, height: 26, fontSize: "var(--text-xs)" }}
                  aria-label="Assign Face Name"
                  placeholder="Person name (e.g. Bruno)..."
                  value={reviewFaceDraft}
                  onChange={(e) => setReviewFaceDraft(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn mono"
                  style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                  onClick={() => void handleAssignReviewFace()}
                >
                  Assign Face Name
                </button>
                <button
                  type="button"
                  className="nvr-btn primary mono"
                  style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                  onClick={() => void handleEnrollReviewFace()}
                >
                  Enroll Face
                </button>
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
                currentTs={Math.floor(activeReview.start_time)}
                onSeek={(ts) => {
                  if (ts !== null) {
                    setActiveReview({
                      ...activeReview,
                      start_time: ts,
                      end_time: ts + 30,
                    });
                  }
                }}
                onExportRange={handleQuickExport}
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
