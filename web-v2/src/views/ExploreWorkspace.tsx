import React from "react";
import { Camera, Play, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import {
  DetectedAttribute,
  EventItem,
  ExportItem,
  RecordingDaySummary,
} from "../types/nvr";
import { WorkspaceTab, findMatchingExport, formatDateTime } from "../utils/format";
import { RecordingCalendarPopover } from "../components/RecordingCalendarPopover";

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
  const pct = Math.round(attr.score * 100);
  return `${attr.label.toUpperCase()} ${pct}% · ${attr.area_px}px²`;
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

  React.useEffect(() => {
    setEventNoteDraft(selectedEvent?.data?.notes || "");
  }, [selectedEvent?.id, selectedEvent?.data?.notes]);

  const handleSaveEventNote = async () => {
    if (!selectedEvent) return;
    const res = await fetch(`/api/events/${encodeURIComponent(selectedEvent.id)}/notes`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ notes: eventNoteDraft }),
    });
    if (res.ok) {
      setSelectedEvent({
        ...selectedEvent,
        data: { ...(selectedEvent.data || {}), notes: eventNoteDraft },
      });
      notify("Saved forensic note on event");
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
    setDebugBusy(true);
    try {
      const res = await fetch("/api/debug/reprocess", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          camera: selectedEvent.camera,
          start_time: selectedEvent.start_time - 2,
          end_time: (selectedEvent.end_time || selectedEvent.start_time + 15) + 2,
          override_config: {
            min_score: debugMinScore,
            min_area: debugMinArea,
          },
        }),
      });
      if (res.ok) {
        const data = await res.json();
        setDebugTrace(data);
        setEventViewMode("clip");
      }
    } catch {
      // ignore
    } finally {
      setDebugBusy(false);
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
    Array<{
      name: string;
      camera: string;
      label: string;
      excludeLabel: string;
      zone: string;
      minScore: number;
      stationary: "all" | "moving" | "stationary";
      retainedOnly: boolean;
    }>
  >(() => {
    try {
      const raw = window.localStorage?.getItem("ninjanvr_explore_saved_filters");
      return raw ? JSON.parse(raw) : [];
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
      const sc = (ev.top_score ?? ev.data?.top_score ?? ev.score ?? 0.85) as number;
      if (minScoreFilter > 0 && sc * 100 < minScoreFilter) {
        return false;
      }
      const isStat = Boolean(ev.stationary ?? ev.data?.provenance?.stationary);
      if (stationaryFilter === "moving" && isStat) return false;
      if (stationaryFilter === "stationary" && !isStat) return false;
      return true;
    });
  }, [filteredEvents, excludeLabel, zoneFilter, minScoreFilter, stationaryFilter]);

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
          <div className="nvr-panel-header">
            <span>TRACKED OBJECTS</span>
            <span className="mono">TOP 50</span>
          </div>
          <div className="nvr-scroll-y">
            {visibleEvents.slice(0, 50).map((ev) => {
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
                        {formatEventLabelWithSubLabel(
                          ev.label,
                          ev.sub_label,
                          ev.data?.detected_attributes
                        )}{" "}
                        · {topScore}%
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
                    {formatEventLabelWithSubLabel(
                      selectedEvent.label,
                      selectedEvent.sub_label,
                      selectedEvent.data?.detected_attributes
                    )}{" "}
                    {Math.round(
                      ((selectedEvent.top_score ??
                        selectedEvent.data?.top_score ??
                        0.88) as number) * 100
                    )}
                    %
                  </span>
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
                                      .map((v) => Number(v).toFixed(2))
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
                    background: "var(--bg-canvas)",
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
                      {debugTrace.frames.map((f, idx) => (
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
                          <span>TS: {f.ts.toFixed(1)}s</span>
                          <span>Motion Boxes: {f.motion_boxes.length}</span>
                          <span style={{ color: "#34d399" }}>
                            Confirmed Tracks:{" "}
                            {f.active_tracks
                              .map((t) => `${t.label} (${Math.round(t.score * 100)}%, ${t.area_px}px²)`)
                              .join(", ") || "NONE"}
                          </span>
                          <span style={{ color: "#fca5a5" }}>
                            Filtered/Rejected:{" "}
                            {f.filtered_reasons
                              .map(
                                (r) =>
                                  `${r.label} (${Math.round(r.score * 100)}% → ${r.reason})`
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

              <div
                style={{
                  padding: "6px 10px",
                  background: "var(--bg-panel)",
                  borderTop: "1px solid var(--border-hairline)",
                  display: "flex",
                  alignItems: "center",
                  gap: 8,
                }}
              >
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  FORENSIC NOTE:
                </span>
                <input
                  className="nvr-input mono"
                  style={{ flex: 1, height: 26, fontSize: "var(--text-xs)" }}
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
