import React from "react";
import { Check, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { useNvrStore } from "../store/useNvrStore";
import { RecordingDaySummary, RecordingSegment, ReviewItem } from "../types/nvr";
import { WorkspaceTab, formatDateTime, formatRelTime } from "../utils/format";
import { VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";
import { RecordingCalendarPopover } from "../components/RecordingCalendarPopover";

interface ReviewWorkspaceProps {
  cameraNames: string[];
  reviews: ReviewItem[];
  filteredReviews: ReviewItem[];
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
          <span
            className="mono"
            style={{
              color: "var(--text-muted)",
              fontSize: "var(--text-xs)",
              whiteSpace: "nowrap",
            }}
          >
            {filteredReviews.length} items
          </span>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 6, flexShrink: 0 }}>
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
            {filteredReviews.slice(0, 40).map((item) => {
              const isSelected = activeReview?.id === item.id;
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
                    className="nvr-btn"
                    onClick={() => {
                      setSyncMasterTs(Math.floor(activeReview.start_time));
                      switchTab("sync");
                    }}
                  >
                    Open in 4-Cam Sync
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

              <div className="nvr-cam-tile" style={{ flex: 1 }}>
                <VodHlsPlayer
                  camera={activeReview.camera}
                  startTime={activeReview.start_time - 4}
                  endTime={(activeReview.end_time || activeReview.start_time + 20) + 6}
                  fallbackPreviewId={activeReview.id}
                  playbackRate={playbackSpeed}
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
