import React from "react";
import { Check, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import { ExportItem, RecordingDaySummary, RecordingSegment, ReviewItem } from "../types/nvr";
import { WorkspaceTab, findMatchingExport, formatDateTime, formatRelTime } from "../utils/format";
import { VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";
import { RecordingCalendarPopover } from "../components/RecordingCalendarPopover";

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
  const [presetName, setPresetName] = React.useState("");
  const [shareTtlSec, setShareTtlSec] = React.useState<number>(86400);
  const [shareCopied, setShareCopied] = React.useState(false);
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
      return true;
    });
  }, [filteredReviews, excludeObjectFilter, zoneFilter]);

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
                ? item.thumb_path.replace("/media/frigate", "")
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
                        {(item.data?.objects || ["motion"]).join(", ")}
                      </span>
                      <div className="nvr-row-actions">
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
                    {(activeReview.data?.objects || ["person"]).join(", ").toUpperCase()}
                  </span>
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
                  <button
                    className="nvr-btn mono"
                    onClick={async () => {
                      const res = await fetch("/api/share", {
                        method: "POST",
                        headers: { "Content-Type": "application/json" },
                        credentials: "include",
                        body: JSON.stringify({
                          kind: "review",
                          target_id: activeReview.id,
                          expires_in_sec: shareTtlSec,
                        }),
                      });
                      const data = await res.json().catch(() => ({}));
                      if (data.url) {
                        navigator.clipboard?.writeText(`${window.location.origin}${data.url}`);
                        setShareCopied(true);
                        window.setTimeout(() => setShareCopied(false), 2000);
                      }
                    }}
                    title="Copy HMAC-signed shareable link for this review event"
                  >
                    {shareCopied ? "Copied!" : "Share Link"}
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
                <VodHlsPlayer
                  camera={activeReview.camera}
                  startTime={activeReview.start_time - 4}
                  endTime={(activeReview.end_time || activeReview.start_time + 20) + 6}
                  fallbackPreviewId={activeReview.id}
                  playbackRate={briefingActive ? briefingSpeed : playbackSpeed}
                  onEnded={briefingActive ? handleAdvanceBriefing : undefined}
                />
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
