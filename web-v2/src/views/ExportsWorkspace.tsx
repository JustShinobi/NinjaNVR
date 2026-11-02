import React from "react";
import { Download, Trash2 } from "lucide-react";
import { useI18n } from "../i18n";
import {
  ExportItem,
  NinjaConfigResponse,
  RecordingSegment,
  ReviewItem,
} from "../types/nvr";
import { formatDateTime } from "../utils/format";
import { LiveMsePlayer, VodHlsPlayer } from "../components/VideoPlayers";
import { TimelineScrubber } from "../components/TimelineScrubber";

interface ExportsWorkspaceProps {
  cameraNames: string[];
  config: NinjaConfigResponse | null;
  exportsList: ExportItem[];
  setExportsList: React.Dispatch<React.SetStateAction<ExportItem[]>>;
  exportCam: string;
  setExportCam: (cam: string) => void;
  exportPresetMin: number;
  setExportPresetMin: (min: number) => void;
  exportLabel: string;
  setExportLabel: (label: string) => void;
  activeExportPreview: ExportItem | null;
  setActiveExportPreview: (item: ExportItem | null) => void;
  timeShiftTs: number | null;
  setTimeShiftTs: (ts: number | null) => void;
  focusSegments: RecordingSegment[];
  reviews: ReviewItem[];
  handleQuickExport: (
    camera: string,
    startSec: number,
    endSec: number,
    label?: string,
    includeAudio?: boolean
  ) => Promise<{ exportId?: string; videoUrl?: string } | void>;
  notify: (msg: string) => void;
}

export const ExportsWorkspace: React.FC<ExportsWorkspaceProps> = ({
  cameraNames,
  config,
  exportsList,
  setExportsList,
  exportCam,
  setExportCam,
  exportPresetMin,
  setExportPresetMin,
  exportLabel,
  setExportLabel,
  activeExportPreview,
  setActiveExportPreview,
  timeShiftTs,
  setTimeShiftTs,
  focusSegments,
  reviews,
  handleQuickExport,
  notify,
}) => {
  const { t } = useI18n();
  const [includeAudio, setIncludeAudio] = React.useState(true);

  return (
    <>
      <div className="nvr-subbar">
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ fontWeight: 700 }}>{t.exports.createTitle}:</span>
          <select
            className="nvr-select"
            aria-label="Select camera channel to export"
            value={exportCam}
            onChange={(e) => setExportCam(e.target.value)}
          >
            {cameraNames.map((c) => (
              <option key={c} value={c}>
                {c.toUpperCase()}
              </option>
            ))}
          </select>
          <select
            className="nvr-select"
            aria-label="Select export duration preset"
            value={exportPresetMin}
            onChange={(e) => setExportPresetMin(Number(e.target.value))}
          >
            <option value={1}>{t.exports.last1m}</option>
            <option value={5}>{t.exports.last5m}</option>
            <option value={15}>{t.exports.last15m}</option>
          </select>
          <input
            className="nvr-input"
            aria-label="Custom export label"
            placeholder={t.exports.customName}
            value={exportLabel}
            onChange={(e) => setExportLabel(e.target.value)}
          />
          <label
            className="mono"
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 5,
              fontSize: "var(--text-xs)",
              color: "var(--text-secondary)",
              cursor: "pointer",
              userSelect: "none",
            }}
          >
            <input
              type="checkbox"
              checked={includeAudio}
              onChange={(e) => setIncludeAudio(e.target.checked)}
              aria-label="Include Audio"
            />
            Include Audio
          </label>
          <button
            className="nvr-btn primary"
            onClick={async () => {
              const now = Math.floor(Date.now() / 1000);
              await handleQuickExport(
                exportCam,
                now - exportPresetMin * 60,
                now,
                exportLabel || `${exportCam}_${exportPresetMin}m`,
                includeAudio
              );
            }}
          >
            {t.exports.generateBtn}
          </button>
        </div>

        {activeExportPreview && (
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <span className="nvr-badge live">
              PREVIEWING // {activeExportPreview.name}
            </span>
            <button
              className="nvr-btn"
              style={{ height: 24, padding: "0 10px", fontSize: "var(--text-xs)" }}
              onClick={() => setActiveExportPreview(null)}
            >
              Close Preview & Return to Live Clipper
            </button>
          </div>
        )}
      </div>

      <div className="nvr-workbench">
        <div className="nvr-panel">
          <div className="nvr-panel-header">
            <span>{t.exports.archiveTitle}</span>
            <span className="mono">{exportsList.length}</span>
          </div>
          <div className="nvr-scroll-y">
            {exportsList.map((ex) => (
              <div
                key={ex.id}
                role="button"
                tabIndex={0}
                className={`nvr-list-row ${
                  activeExportPreview?.id === ex.id ? "is-selected" : ""
                }`}
                onClick={() => setActiveExportPreview(ex)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    setActiveExportPreview(ex);
                  }
                }}
                style={{
                  padding: "10px 12px",
                  justifyContent: "space-between",
                }}
              >
                <div style={{ minWidth: 0, flex: 1 }}>
                  <div
                    className="mono"
                    title={ex.name}
                    style={{
                      fontWeight: 700,
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    {ex.name}
                  </div>
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                      whiteSpace: "nowrap",
                      marginTop: 2,
                    }}
                  >
                    {ex.camera.toUpperCase()} · {formatDateTime(ex.date)}
                  </div>
                </div>
                <div style={{ display: "flex", gap: 6, flexShrink: 0 }}>
                  <a
                    href={ex.video_path.replace("/media/frigate", "")}
                    download={`${ex.id}.mp4`}
                    className="nvr-btn"
                    style={{ textDecoration: "none" }}
                    aria-label={`Download ${ex.name} MP4`}
                    title={t.exports.downloadMp4}
                    onClick={(e) => e.stopPropagation()}
                  >
                    <Download size={12} />
                  </a>
                  <button
                    className="nvr-btn danger"
                    aria-label={`Delete export ${ex.name}`}
                    title={t.exports.deleteBtn}
                    onClick={async (e) => {
                      e.stopPropagation();
                      await fetch(`/api/export/${encodeURIComponent(ex.id)}`, {
                        method: "DELETE",
                        credentials: "include",
                      });
                      setExportsList((prev) => prev.filter((i) => i.id !== ex.id));
                      if (activeExportPreview?.id === ex.id) {
                        setActiveExportPreview(null);
                      }
                      notify("Export deleted");
                    }}
                  >
                    <Trash2 size={12} />
                  </button>
                </div>
              </div>
            ))}
            {exportsList.length === 0 && (
              <div
                style={{ padding: 24, textAlign: "center", color: "var(--text-muted)" }}
              >
                {t.exports.noExports}
              </div>
            )}
          </div>
        </div>

        <div className="nvr-focus-stage">
          <div className="nvr-cam-tile" style={{ flex: 1 }}>
            {activeExportPreview ? (
              <video
                src={activeExportPreview.video_path.replace("/media/frigate", "")}
                controls
                autoPlay
                muted
                playsInline
                style={{ width: "100%", height: "100%", objectFit: "contain" }}
              />
            ) : timeShiftTs === null ? (
              <LiveMsePlayer
                camera={exportCam}
                useMainStream={true}
                cameraConfig={config?.cameras?.[exportCam]}
              />
            ) : (
              <VodHlsPlayer
                camera={exportCam}
                startTime={timeShiftTs}
                endTime={timeShiftTs + 180}
              />
            )}
          </div>

          <TimelineScrubber
            camera={exportCam}
            cameras={cameraNames}
            onSelectCamera={(cam) => setExportCam(cam)}
            segments={focusSegments}
            reviews={reviews}
            currentTs={timeShiftTs}
            onSeek={(ts) => {
              setActiveExportPreview(null);
              setTimeShiftTs(ts);
            }}
            onExportRange={(cam, start, end, label) =>
              handleQuickExport(cam, start, end, label, includeAudio)
            }
            titlePrefix="EXPORT CLIPPER"
            defaultRangeMode={true}
          />
        </div>
      </div>
    </>
  );
};
