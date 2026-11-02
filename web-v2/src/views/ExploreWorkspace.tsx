import React from "react";
import { Camera, Play, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import { EventItem, ExportItem, RecordingDaySummary } from "../types/nvr";
import { WorkspaceTab, findMatchingExport, formatDateTime } from "../utils/format";
import { RecordingCalendarPopover } from "../components/RecordingCalendarPopover";

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
        </div>
        <span
          className="mono"
          style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
        >
          {filteredEvents.length} tracked object events
        </span>
      </div>

      <div className="nvr-workbench">
        <div className="nvr-panel">
          <div className="nvr-panel-header">
            <span>TRACKED OBJECTS</span>
            <span className="mono">TOP 50</span>
          </div>
          <div className="nvr-scroll-y">
            {filteredEvents.slice(0, 50).map((ev) => {
              const topScore = Math.round(
                ((ev.top_score ?? ev.data?.top_score ?? ev.score ?? 0.85) as number) * 100
              );
              const isSelected = selectedEvent?.id === ev.id;
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
                      background: "var(--bg-deep)",
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
                        {ev.label} · {topScore}%
                      </span>
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
                        {ev.end_time
                          ? `${Math.max(1, Math.round(ev.end_time - ev.start_time))}s`
                          : t.explore.inProgress}
                      </span>
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
                    </div>
                  </div>
                </div>
              );
            })}
            {filteredEvents.length === 0 && (
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
                    {selectedEvent.label.toUpperCase()}{" "}
                    {Math.round(
                      ((selectedEvent.top_score ??
                        selectedEvent.data?.top_score ??
                        0.88) as number) * 100
                    )}
                    %
                  </span>
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
                    className="nvr-btn"
                    onClick={async () => {
                      await fetch(
                        `/api/events/${encodeURIComponent(selectedEvent.id)}/false_positive`,
                        {
                          method: "PUT",
                          headers: { "Content-Type": "application/json" },
                          credentials: "include",
                          body: JSON.stringify({ false_positive: true }),
                        }
                      );
                      notify("Event flagged as false positive");
                    }}
                  >
                    {t.explore.falsePositive}
                  </button>
                  <button
                    className="nvr-btn"
                    onClick={() => {
                      setSyncMasterTs(Math.floor(selectedEvent.start_time));
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
                    className="nvr-btn danger"
                    aria-label="Delete tracked object event"
                    title="Delete tracked object event"
                    onClick={() => handleEventDelete(selectedEvent.id)}
                  >
                    <Trash2 size={12} />
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
                    const p = selectedEvent.data?.provenance;
                    const topScore =
                      p?.top_score ??
                      selectedEvent.top_score ??
                      selectedEvent.data?.top_score ??
                      0.85;
                    const thresh = p?.threshold_cfg ?? 0.7;
                    const areaPx = p?.area_px ?? selectedEvent.area ?? 0;
                    const areaPct = p?.area_pct ?? 0;
                    const ratio =
                      p?.aspect_ratio ??
                      (selectedEvent.ratio ? Math.round(selectedEvent.ratio * 100) / 100 : 0.5);
                    const bbox = p?.bbox ?? [0, 0, 0, 0];
                    const anchorX = Math.round((bbox[0] + bbox[2]) / 2);
                    const anchorY = bbox[3];
                    const zones =
                      (p?.zones && p.zones.length > 0 ? p.zones : selectedEvent.zones) || [];
                    const triggerTs = p?.trigger_ts ?? selectedEvent.start_time;
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
                  <video
                    src={`/api/events/${encodeURIComponent(selectedEvent.id)}/clip.mp4`}
                    controls
                    autoPlay
                    muted
                    playsInline
                    style={{ width: "100%", height: "100%", objectFit: "contain" }}
                  />
                )}
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
