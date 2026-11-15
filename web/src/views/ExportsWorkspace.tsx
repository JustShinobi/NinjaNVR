import React from "react";
import {
  AlertTriangle,
  Copy,
  Download,
  Link2,
  Loader2,
  Scissors,
  ShieldOff,
  Trash2,
  X,
} from "lucide-react";
import { useI18n } from "../i18n";
import {
  ExportItem,
  ExportJobItem,
  NinjaConfigResponse,
  RecordingSegment,
  ReviewItem,
  ShareLinkItem,
} from "../types/nvr";
import { formatDateTime, formatDuration } from "../utils/format";
import { resolveRealCameraSource } from "../store/useNvrStore";
import {
  LoadedVodWindow,
  MAX_ARCHIVE_EXPORT_SPAN_SEC,
  createArchiveMetadataCache,
  resolveVodSeekAction,
  useArchiveMetadataCache,
  validateArchiveExportRange,
} from "../utils/archivePlayback";
import { resolveSafeMediaUrl } from "../utils/mediaUrlPolicy";
import {
  ShareLinkStatus,
  computeShareLinkStatus,
  createShareLinkSessionController,
  filterActiveAndRecentShareLinks,
  formatShareLinkRemainingTtl,
  formatShareLinkViews,
} from "../utils/shareLinks";
import {
  LiveMsePlayer,
  StageTrimOverlay,
  VodHlsPlayer,
} from "../components/VideoPlayers";
import {
  TimelineClipRequest,
  TimelineScrubber,
  TimelineTrimFeedbackState,
  sanitizeTimelineThumbUrl,
} from "../components/TimelineScrubber";
import { createTrimScrubChannel } from "../utils/exportClipFlow";

export const MAX_EXPORT_WINDOW_SEC = MAX_ARCHIVE_EXPORT_SPAN_SEC;

export interface ExportTimeWindowValidation {
  valid: boolean;
  startSec: number;
  endSec: number;
  durationSec: number;
  error?: string;
}

export function validateExportTimeWindow(
  startSec: number,
  endSec: number,
  maxWindowSec: number = MAX_EXPORT_WINDOW_SEC
): ExportTimeWindowValidation {
  return validateArchiveExportRange(startSec, endSec, maxWindowSec);
}

export function resolveSafeExportVideoUrl(
  videoPath: string | null | undefined,
  exportId?: string
): string {
  return resolveSafeMediaUrl(videoPath, {
    kind: "export_video",
    exportId: exportId ?? "",
    allowedPrefixes: ["/exports/", "/clips/"],
  });
}

interface ExportsWorkspaceProps {
  cameraNames: string[];
  config: NinjaConfigResponse | null;
  exportsList: ExportItem[];
  setExportsList: React.Dispatch<React.SetStateAction<ExportItem[]>>;
  exportJobs?: ExportJobItem[];
  onCancelExportJob?: (jobId: string) => Promise<void> | void;
  exportCam: string;
  setExportCam: (cam: string) => void;
  exportPresetMin?: number;
  setExportPresetMin?: (min: number) => void;
  exportLabel?: string;
  setExportLabel?: (label: string) => void;
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
  ) => Promise<{ exportId?: string; videoUrl?: string; queued?: boolean } | void>;
  notify: (msg: string) => void;
}

export interface CopyExistingShareLinkDeps {
  creatingShareId?: string | null;
  revokingShareId?: string | null;
  setCreatingShareId?: (id: string | null) => void;
  fetchShareLinks: () => Promise<ShareLinkItem[] | null>;
  shareUrlMap: Record<string, string>;
  setShareUrlMap: React.Dispatch<React.SetStateAction<Record<string, string>>>;
  setGeneratedShareUrl: React.Dispatch<React.SetStateAction<string>>;
  copyUrlToClipboard: (fullUrl: string, ttlHoursLabel?: string) => Promise<void>;
  notify: (msg: string) => void;
  origin?: string;
  fetchImpl?: typeof fetch;
}

export async function executeCopyExistingShareLink(
  link: ShareLinkItem,
  deps: CopyExistingShareLinkDeps
): Promise<void> {
  const {
    creatingShareId,
    revokingShareId,
    setCreatingShareId,
    fetchShareLinks,
    shareUrlMap,
    setShareUrlMap,
    setGeneratedShareUrl,
    copyUrlToClipboard,
    notify,
    origin = typeof window !== "undefined" && window.location
      ? window.location.origin
      : "",
    fetchImpl = fetch,
  } = deps;
  if (
    (creatingShareId !== null && creatingShareId !== undefined) ||
    (revokingShareId !== null && revokingShareId !== undefined)
  ) {
    return;
  }
  const controller = createShareLinkSessionController({
    fetchImpl,
    listShareLinks: fetchShareLinks,
    origin,
    copyUrlToClipboard,
    notify,
    onCreatingShareIdChange: (id) => setCreatingShareId?.(id),
    onShareUrlMapChange: (updater) => setShareUrlMap(updater),
    onGeneratedShareUrlChange: (updater) => setGeneratedShareUrl(updater),
  });
  controller.seedState({
    shareUrlMap,
    creatingShareId: creatingShareId ?? null,
    revokingShareId: revokingShareId ?? null,
  });
  await controller.copyOrRotate(link);
}

export const ExportsWorkspace: React.FC<ExportsWorkspaceProps> = ({
  cameraNames,
  config,
  exportsList,
  setExportsList,
  exportJobs = [],
  onCancelExportJob,
  exportCam,
  setExportCam,
  activeExportPreview,
  setActiveExportPreview,
  timeShiftTs,
  setTimeShiftTs,
  focusSegments,
  reviews,
  handleQuickExport,
  notify,
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const [includeAudio, setIncludeAudio] = React.useState(true);
  const [shareTtlSec, setShareTtlSec] = React.useState(86400);
  const [shareMaxViews, setShareMaxViews] = React.useState<number>(0);
  const [showInactiveLinks, setShowInactiveLinks] = React.useState(false);
  const [deletingExportId, setDeletingExportId] = React.useState<string | null>(
    null
  );
  const [cancellingJobId, setCancellingJobId] = React.useState<string | null>(
    null
  );
  const [trimFeedback, setTrimFeedback] =
    React.useState<TimelineTrimFeedbackState | null>(null);
  const trimScrubChannel = React.useMemo(() => createTrimScrubChannel(), []);
  const [autoSeedClipRequest, setAutoSeedClipRequest] =
    React.useState<TimelineClipRequest | null>(null);
  const [dismissedJobIds, setDismissedJobIds] = React.useState<
    Record<string, true>
  >({});

  const contextMetadataCache = useArchiveMetadataCache();
  const fallbackMetadataCache = React.useMemo(
    () => createArchiveMetadataCache(),
    []
  );
  const archiveMetadataCache = contextMetadataCache ?? fallbackMetadataCache;

  const isMountedRef = React.useRef(true);
  const exportVodWindowRef = React.useRef<LoadedVodWindow | null>(null);
  const autoSeededCamRef = React.useRef<string | null>(null);
  const notifyRef = React.useRef(notify);
  notifyRef.current = notify;

  const shareControllerRef = React.useRef<ReturnType<
    typeof createShareLinkSessionController
  > | null>(null);
  if (!shareControllerRef.current) {
    shareControllerRef.current = createShareLinkSessionController({
      notify: (msg) => notifyRef.current(msg),
    });
  }
  const shareController = shareControllerRef.current;

  const [shareSessionState, setShareSessionState] = React.useState(() =>
    shareController.getState()
  );
  const shareLinks = shareSessionState.links;
  const generatedShareUrl = shareSessionState.generatedShareUrl;
  const creatingShareId = shareSessionState.creatingShareId;
  const revokingShareId = shareSessionState.revokingShareId;

  React.useEffect(() => {
    isMountedRef.current = true;
    const unsub = shareController.subscribe((snap) => {
      if (isMountedRef.current) {
        setShareSessionState(snap);
      }
    });
    void shareController.refresh();
    return () => {
      isMountedRef.current = false;
      unsub();
      shareController.dispose();
    };
  }, [shareController]);

  // Enter archive VOD mode automatically for `exportCam` on initial open when recordings exist,
  // verified directly against `realExportCam` (never using a stale previous camera's `focusSegments`)
  // and aligning `[rangeStart, rangeEnd]` with the seeded archive timestamp.
  React.useEffect(() => {
    const cleanCam = (exportCam || "").trim();
    if (!cleanCam || autoSeededCamRef.current === cleanCam) return;
    if (activeExportPreview || timeShiftTs !== null) {
      autoSeededCamRef.current = cleanCam;
      return;
    }
    let cancelled = false;
    const realCam = resolveRealCameraSource(cleanCam, config?.cameras);
    const now = Math.floor(Date.now() / 1000);
    const afterBucket = Math.floor((now - 86400) / 15) * 15;
    const beforeBucket = Math.ceil((now + 75) / 15) * 15;
    archiveMetadataCache
      .getRecordings(
        realCam,
        { after: afterBucket, before: beforeBucket },
        { configuredCameras: config?.cameras }
      )
      .then((segs) => {
        if (cancelled || !isMountedRef.current) return;
        autoSeededCamRef.current = cleanCam;
        if (Array.isArray(segs) && segs.length > 0) {
          const latestSeg = segs[segs.length - 1];
          if (latestSeg && Number.isFinite(latestSeg.end_time)) {
            const clipEnd = Math.min(now, Math.ceil(latestSeg.end_time));
            const clipStart = Math.max(
              Math.floor(latestSeg.start_time),
              clipEnd - 45
            );
            if (clipEnd > clipStart) {
              setTimeShiftTs(clipStart);
              setAutoSeedClipRequest({
                camera: cleanCam,
                start: clipStart,
                end: clipEnd,
                nonce: Date.now(),
              });
            }
          }
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [
    exportCam,
    config?.cameras,
    archiveMetadataCache,
    activeExportPreview,
    timeShiftTs,
    setTimeShiftTs,
  ]);

  React.useEffect(() => {
    if (timeShiftTs === null || !Number.isFinite(timeShiftTs)) {
      exportVodWindowRef.current = null;
    }
  }, [timeShiftTs]);

  React.useEffect(() => {
    if (
      activeExportPreview &&
      !exportsList.some((ex) => ex.id === activeExportPreview.id)
    ) {
      setActiveExportPreview(null);
    }
  }, [exportsList, activeExportPreview, setActiveExportPreview]);

  const copyUrlToClipboard = async (
    fullUrl: string,
    ttlHoursLabel?: string
  ) => {
    shareController.seedState({ generatedShareUrl: fullUrl });
    let copied = false;
    if (
      typeof navigator !== "undefined" &&
      navigator.clipboard &&
      typeof navigator.clipboard.writeText === "function"
    ) {
      try {
        await navigator.clipboard.writeText(fullUrl);
        copied = true;
      } catch {
        copied = false;
      }
    }
    if (copied) {
      notify(
        ttlHoursLabel
          ? `Copied expiring share link (${ttlHoursLabel}) to clipboard`
          : "Copied share link to clipboard"
      );
    } else {
      notify("Share link ready — copy URL from the field below");
    }
  };

  const handleCopyShareLink = async (
    targetId: string,
    kind: string = "export",
    overrideTtlSec?: number,
    overrideMaxViews?: number | null
  ) => {
    const ttlToUse = overrideTtlSec ?? shareTtlSec;
    const viewsToUse =
      overrideMaxViews !== undefined ? overrideMaxViews : shareMaxViews;
    await shareController.createAndCopy({
      targetId,
      kind,
      expiresInSec: ttlToUse,
      maxViews: viewsToUse,
    });
  };

  const handleCopyExistingShareLink = async (link: ShareLinkItem) => {
    await shareController.copyOrRotate(link);
  };

  const handleRevokeShareLink = async (id: string) => {
    await shareController.revoke(id);
  };

  const visibleShareLinks = React.useMemo(
    () => filterActiveAndRecentShareLinks(shareLinks, showInactiveLinks),
    [shareLinks, showInactiveLinks]
  );

  const visibleJobs = React.useMemo(
    () =>
      exportJobs.filter(
        (j) =>
          !dismissedJobIds[j.id] &&
          (j.status === "queued" ||
            j.status === "running" ||
            j.status === "failed")
      ),
    [exportJobs, dismissedJobIds]
  );

  const runningJobsCount = React.useMemo(
    () =>
      exportJobs.filter(
        (j) => j.status === "queued" || j.status === "running"
      ).length,
    [exportJobs]
  );

  const completedExports = React.useMemo(
    () => exportsList.filter((ex) => !ex.in_progress),
    [exportsList]
  );

  const handleSelectExportCamera = React.useCallback(
    (nextCam: string) => {
      setTrimFeedback(null);
      setActiveExportPreview(null);
      setTimeShiftTs(null);
      setExportCam(nextCam);
    },
    [setActiveExportPreview, setTimeShiftTs, setExportCam]
  );

  const renderStatusBadge = (status: ShareLinkStatus) => {
    if (status === "active") {
      return <span className="nvr-badge live">{t.exports.statusActive}</span>;
    }
    if (status === "revoked") {
      return <span className="nvr-badge alert">{t.exports.statusRevoked}</span>;
    }
    if (status === "exhausted") {
      return (
        <span className="nvr-badge motion">{t.exports.statusExhausted}</span>
      );
    }
    return <span className="nvr-badge">{t.exports.statusExpired}</span>;
  };

  // During active handle drag (`dragging_in` / `dragging_out`), keep `VodHlsPlayer`'s `playheadTs`
  // parked at `timeShiftTs` so we never thrash HLS manifests at 60Hz while scrubbing!
  const isSettledOrPreviewTrim =
    trimFeedback !== null &&
    (trimFeedback.mode === "settled_in" ||
      trimFeedback.mode === "settled_out" ||
      trimFeedback.mode === "preview_clip");
  const isDraggingTrim =
    trimFeedback !== null &&
    (trimFeedback.mode === "dragging_in" ||
      trimFeedback.mode === "dragging_out");
  const effectiveVodPlayheadTs =
    isSettledOrPreviewTrim && Number.isFinite(trimFeedback.targetTs)
      ? trimFeedback.targetTs
      : timeShiftTs;

  return (
    <>
      <div className="nvr-subbar">
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            flexWrap: "wrap",
          }}
        >
          <span
            className="mono"
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 6,
              fontWeight: 700,
              color: "var(--signal-accent-text)",
            }}
          >
            <Scissors size={13} />
            {t.exports.title}
          </span>
          <select
            className="nvr-select mono"
            aria-label="Select camera channel to export"
            value={exportCam}
            onChange={(e) => handleSelectExportCamera(e.target.value)}
          >
            {cameraNames.map((c) => (
              <option key={c} value={c}>
                {c.toUpperCase()}
              </option>
            ))}
          </select>
          <span
            className="mono nvr-tl-desktop"
            style={{
              fontSize: "var(--text-xs)",
              color: "var(--text-muted)",
            }}
          >
            {isPt
              ? "[I / O] Marcar IN/OUT · [ [ / ] ] Ver corte · [Shift+Arrastar] Precisão · [Scroll] Navegar sem mover marcador"
              : "[I / O] Mark IN/OUT · [ [ / ] ] Inspect cut · [Shift+Drag] Fine trim · [Wheel] Pan without moving marker"}
          </span>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          {runningJobsCount > 0 && (
            <span className="nvr-badge motion mono">
              <Loader2 size={11} />
              {isPt
                ? `${runningJobsCount} EXPORTAÇÃO EM SEGUNDO PLANO`
                : `${runningJobsCount} BACKGROUND EXPORT(S)`}
            </span>
          )}
          {activeExportPreview && (
            <>
              <span className="nvr-badge live">
                PREVIEWING // {activeExportPreview.name}
              </span>
              <button
                className="nvr-btn"
                style={{
                  height: 24,
                  padding: "0 10px",
                  fontSize: "var(--text-xs)",
                }}
                onClick={() => setActiveExportPreview(null)}
              >
                {isPt
                  ? "Fechar Preview e Voltar à Mesa de Corte"
                  : "Close Preview & Return to Clipper"}
              </button>
            </>
          )}
        </div>
      </div>

      <div className="nvr-workbench">
        <div className="nvr-panel">
          <div
            className="nvr-panel-header"
            style={{
              height: "auto",
              minHeight: 36,
              padding: "6px 12px",
              flexWrap: "wrap",
              gap: 6,
            }}
          >
            <span>{t.exports.archiveTitle}</span>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                gap: 6,
                flexWrap: "wrap",
              }}
            >
              <select
                className="nvr-select mono"
                style={{
                  height: 22,
                  fontSize: "var(--text-xs)",
                  padding: "0 6px",
                }}
                aria-label="Share link expiration"
                value={shareTtlSec}
                onChange={(e) => setShareTtlSec(Number(e.target.value))}
              >
                <option value={3600}>Share TTL: 1h</option>
                <option value={86400}>Share TTL: 24h</option>
                <option value={604800}>Share TTL: 7d</option>
              </select>
              <select
                className="nvr-select mono"
                style={{
                  height: 22,
                  fontSize: "var(--text-xs)",
                  padding: "0 6px",
                }}
                aria-label="Share link maximum views"
                value={shareMaxViews}
                onChange={(e) => setShareMaxViews(Number(e.target.value))}
              >
                <option value={0}>{t.exports.maxViewsUnlimited}</option>
                <option value={1}>{t.exports.maxViews1}</option>
                <option value={5}>{t.exports.maxViews5}</option>
                <option value={25}>{t.exports.maxViews25}</option>
              </select>
              <span className="mono">{completedExports.length}</span>
            </div>
          </div>
          {generatedShareUrl && (
            <div
              style={{
                padding: "6px 12px",
                background: "var(--bg-subcard)",
                borderBottom: "1px solid var(--border-hairline)",
                display: "flex",
                alignItems: "center",
                gap: 6,
              }}
            >
              <Link2
                size={12}
                style={{ color: "var(--signal-accent-text)", flexShrink: 0 }}
              />
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                  flexShrink: 0,
                }}
              >
                {t.exports.generatedUrlLabel}:
              </span>
              <input
                type="text"
                readOnly
                className="nvr-input mono"
                aria-label="Generated share link URL"
                value={generatedShareUrl}
                onFocus={(e) => e.currentTarget.select()}
                onClick={(e) => e.currentTarget.select()}
                style={{
                  flex: 1,
                  minWidth: 0,
                  height: 24,
                  fontSize: "var(--text-xs)",
                }}
              />
              <button
                type="button"
                className="nvr-btn mono"
                style={{
                  height: 24,
                  padding: "0 8px",
                  fontSize: "var(--text-xs)",
                }}
                onClick={() => void copyUrlToClipboard(generatedShareUrl)}
                title={t.exports.copyLinkBtn}
              >
                <Copy size={11} />
                {t.exports.copyLinkBtn}
              </button>
            </div>
          )}

          <div className="nvr-scroll-y" style={{ flex: 1, minHeight: 140 }}>
            {/* Active & Failed Background Export Jobs Queue */}
            {visibleJobs.map((job) => {
              const isFailed = job.status === "failed";
              const pct = Math.round(
                Math.max(0, Math.min(100, job.progress_pct))
              );
              const dur = Math.max(1, Math.round(job.end_time - job.start_time));
              return (
                <div
                  key={`job-${job.id}`}
                  className="nvr-list-row"
                  style={{
                    padding: "10px 12px",
                    flexDirection: "column",
                    alignItems: "stretch",
                    gap: 6,
                    background: "var(--bg-subcard)",
                    borderBottom: "1px solid var(--border-strong)",
                    cursor: "default",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: 8,
                    }}
                  >
                    <div style={{ minWidth: 0, flex: 1 }}>
                      <div
                        className="mono"
                        title={job.name}
                        style={{
                          fontWeight: 700,
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                          whiteSpace: "nowrap",
                          display: "flex",
                          alignItems: "center",
                          gap: 6,
                        }}
                      >
                        {isFailed ? (
                          <AlertTriangle
                            size={12}
                            style={{
                              color: "var(--signal-alert-text)",
                              flexShrink: 0,
                            }}
                          />
                        ) : (
                          <Loader2
                            size={12}
                            style={{
                              color: "var(--signal-accent-text)",
                              flexShrink: 0,
                            }}
                          />
                        )}
                        <span>{job.name}</span>
                      </div>
                      <div
                        className="mono"
                        style={{
                          fontSize: "var(--text-xs)",
                          color: "var(--text-secondary)",
                          marginTop: 2,
                        }}
                      >
                        {job.camera.toUpperCase()} · {formatDuration(dur)} ·{" "}
                        {job.status === "queued"
                          ? isPt
                            ? "NA FILA"
                            : "QUEUED"
                          : isFailed
                          ? isPt
                            ? "FALHOU"
                            : "FAILED"
                          : `${pct}%`}
                      </div>
                    </div>

                    {isFailed ? (
                      <button
                        type="button"
                        className="nvr-btn sm"
                        title={isPt ? "Dispensar aviso" : "Dismiss"}
                        onClick={() =>
                          setDismissedJobIds((prev) => ({
                            ...prev,
                            [job.id]: true,
                          }))
                        }
                      >
                        <X size={12} />
                      </button>
                    ) : (
                      onCancelExportJob && (
                        <button
                          type="button"
                          className="nvr-btn danger sm mono"
                          disabled={cancellingJobId === job.id}
                          title={
                            isPt
                              ? "Cancelar exportação em andamento"
                              : "Cancel running export job"
                          }
                          onClick={async () => {
                            setCancellingJobId(job.id);
                            try {
                              await onCancelExportJob(job.id);
                            } finally {
                              if (isMountedRef.current) {
                                setCancellingJobId(null);
                              }
                            }
                          }}
                        >
                          <X size={11} />
                          {isPt ? "Cancelar" : "Cancel"}
                        </button>
                      )
                    )}
                  </div>

                  {isFailed ? (
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--signal-alert-text)",
                        wordBreak: "break-word",
                      }}
                    >
                      {job.error ||
                        (isPt
                          ? "Falha ao exportar trecho MP4"
                          : "Failed to export MP4 clip")}
                    </div>
                  ) : (
                    <div
                      role="progressbar"
                      aria-valuenow={pct}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      style={{
                        height: 5,
                        width: "100%",
                        background: "var(--bg-deep)",
                        borderRadius: 2,
                        overflow: "hidden",
                        border: "1px solid var(--border-hairline)",
                      }}
                    >
                      <div
                        style={{
                          height: "100%",
                          width: `${Math.max(4, pct)}%`,
                          background: "var(--signal-accent)",
                          transition: "width 0.25s ease",
                        }}
                      />
                    </div>
                  )}
                </div>
              );
            })}

            {/* Saved Completed Exports */}
            {completedExports.map((ex) => {
              const safeThumb = ex.thumb_path
                ? sanitizeTimelineThumbUrl(ex.thumb_path, ex.camera)
                : "";
              return (
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
                    padding: "8px 12px",
                    justifyContent: "space-between",
                    gap: 10,
                  }}
                >
                  {safeThumb && (
                    <img
                      src={safeThumb}
                      alt=""
                      width={64}
                      height={36}
                      loading="lazy"
                      style={{
                        width: 64,
                        height: 36,
                        objectFit: "cover",
                        borderRadius: 2,
                        background: "var(--bg-deep)",
                        border: "1px solid var(--border-hairline)",
                        flexShrink: 0,
                      }}
                    />
                  )}
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
                    <button
                      className="nvr-btn mono"
                      style={{
                        height: 26,
                        padding: "0 7px",
                        fontSize: "var(--text-xs)",
                      }}
                      title="Copy expiring HMAC-signed shareable link"
                      disabled={
                        creatingShareId !== null || revokingShareId !== null
                      }
                      onClick={(e) => {
                        e.stopPropagation();
                        void handleCopyShareLink(ex.id);
                      }}
                    >
                      Share Link
                    </button>
                    <a
                      href={resolveSafeExportVideoUrl(ex.video_path, ex.id)}
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
                      disabled={deletingExportId === ex.id}
                      onClick={async (e) => {
                        e.stopPropagation();
                        if (deletingExportId !== null) return;
                        setDeletingExportId(ex.id);
                        try {
                          const res = await fetch(
                            `/api/export/${encodeURIComponent(ex.id)}`,
                            {
                              method: "DELETE",
                              credentials: "include",
                            }
                          );
                          if (!isMountedRef.current) return;
                          if (res.ok) {
                            setExportsList((prev) =>
                              prev.filter((i) => i.id !== ex.id)
                            );
                            if (activeExportPreview?.id === ex.id) {
                              setActiveExportPreview(null);
                            }
                            shareController.evictTarget(ex.id, shareLinks);
                            void shareController.refresh();
                            notify("Export deleted");
                          } else {
                            const data = await res.json().catch(() => ({}));
                            if (isMountedRef.current) {
                              notify(
                                (typeof data?.message === "string" &&
                                  data.message) ||
                                  "Failed to delete export"
                              );
                            }
                          }
                        } catch {
                          if (isMountedRef.current) {
                            notify("Failed to delete export");
                          }
                        } finally {
                          if (isMountedRef.current) {
                            setDeletingExportId(null);
                          }
                        }
                      }}
                    >
                      <Trash2 size={12} />
                    </button>
                  </div>
                </div>
              );
            })}
            {completedExports.length === 0 && visibleJobs.length === 0 && (
              <div
                style={{
                  padding: 24,
                  textAlign: "center",
                  color: "var(--text-muted)",
                }}
              >
                {t.exports.noExports}
              </div>
            )}
          </div>

          <div
            style={{
              borderTop: "1px solid var(--border-strong)",
              display: "flex",
              flexDirection: "column",
              flex: 1,
              minHeight: 160,
              maxHeight: "48%",
              background: "var(--bg-panel)",
            }}
          >
            <div
              className="nvr-panel-header"
              style={{
                height: "auto",
                minHeight: 36,
                padding: "6px 12px",
                flexWrap: "wrap",
                gap: 6,
              }}
            >
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <span>{t.exports.activeShareLinks}</span>
                <span className="mono">{visibleShareLinks.length}</span>
              </div>
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
                  textTransform: "none",
                }}
              >
                <input
                  type="checkbox"
                  checked={showInactiveLinks}
                  onChange={(e) => setShowInactiveLinks(e.target.checked)}
                  aria-label={t.exports.showInactiveLinks}
                />
                {t.exports.showInactiveLinks}
              </label>
            </div>
            <div className="nvr-scroll-y">
              {visibleShareLinks.map((link) => {
                const status = computeShareLinkStatus(link);
                const remainingTtl = formatShareLinkRemainingTtl(
                  link.expires_at
                );
                const viewsStr = formatShareLinkViews(link);
                return (
                  <div
                    key={link.id}
                    className="nvr-list-row"
                    style={{
                      padding: "8px 12px",
                      flexDirection: "column",
                      alignItems: "stretch",
                      gap: 6,
                      cursor: "default",
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: 8,
                      }}
                    >
                      <div
                        style={{
                          display: "flex",
                          alignItems: "center",
                          gap: 6,
                          minWidth: 0,
                          flex: 1,
                        }}
                      >
                        {renderStatusBadge(status)}
                        <span
                          className="mono"
                          style={{
                            fontWeight: 700,
                            fontSize: "var(--text-xs)",
                            color: "var(--text-primary)",
                            flexShrink: 0,
                          }}
                        >
                          {(link.camera || "GLOBAL").toUpperCase()}
                        </span>
                        <span
                          className="mono"
                          title={`${link.kind}: ${link.target_id}`}
                          style={{
                            fontSize: "var(--text-xs)",
                            color: "var(--text-secondary)",
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          [{link.kind.toUpperCase()}] {link.target_id}
                        </span>
                      </div>
                      <div style={{ display: "flex", gap: 5, flexShrink: 0 }}>
                        {status === "active" && (
                          <>
                            <button
                              type="button"
                              className="nvr-btn mono"
                              style={{
                                height: 24,
                                padding: "0 7px",
                                fontSize: "var(--text-xs)",
                              }}
                              disabled={
                                creatingShareId !== null ||
                                revokingShareId !== null
                              }
                              onClick={() =>
                                void handleCopyExistingShareLink(link)
                              }
                              title={t.exports.copyLinkBtn}
                            >
                              <Copy size={11} />
                              {t.exports.copyLinkBtn}
                            </button>
                            <button
                              type="button"
                              className="nvr-btn danger mono"
                              style={{
                                height: 24,
                                padding: "0 7px",
                                fontSize: "var(--text-xs)",
                              }}
                              disabled={
                                creatingShareId !== null ||
                                revokingShareId !== null
                              }
                              onClick={() =>
                                void handleRevokeShareLink(link.id)
                              }
                              title={t.exports.revokeBtn}
                            >
                              <ShieldOff size={11} />
                              {t.exports.revokeBtn}
                            </button>
                          </>
                        )}
                      </div>
                    </div>
                    <div
                      className="mono"
                      style={{
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        flexWrap: "wrap",
                        gap: 8,
                        fontSize: "var(--text-xs)",
                        color: "var(--text-muted)",
                      }}
                    >
                      <span>
                        by {link.created_by} · TTL: {remainingTtl} (
                        {formatDateTime(link.expires_at)})
                      </span>
                      <span>Views: {viewsStr}</span>
                    </div>
                  </div>
                );
              })}
              {visibleShareLinks.length === 0 && (
                <div
                  style={{
                    padding: 18,
                    textAlign: "center",
                    color: "var(--text-muted)",
                    fontSize: "var(--text-xs)",
                  }}
                >
                  {t.exports.noShareLinks}
                </div>
              )}
            </div>
          </div>
        </div>

        <div className="nvr-focus-stage">
          <div className="nvr-cam-tile" style={{ flex: 1 }}>
            {activeExportPreview ? (
              <video
                key={activeExportPreview.id}
                src={resolveSafeExportVideoUrl(
                  activeExportPreview.video_path,
                  activeExportPreview.id
                )}
                controls
                autoPlay
                muted
                playsInline
                style={{ width: "100%", height: "100%", objectFit: "contain" }}
              />
            ) : effectiveVodPlayheadTs === null ||
              !Number.isFinite(effectiveVodPlayheadTs) ? (
              (() => {
                const realExportCam = resolveRealCameraSource(
                  exportCam,
                  config?.cameras
                );
                return (
                  <div
                    style={{
                      position: "relative",
                      width: "100%",
                      height: "100%",
                    }}
                  >
                    <LiveMsePlayer
                      camera={exportCam}
                      useMainStream={true}
                      cameraConfig={
                        config?.cameras?.[exportCam] ??
                        config?.cameras?.[realExportCam]
                      }
                    />
                    {isDraggingTrim && (
                      <StageTrimOverlay
                        scrubPreviewUrl={trimFeedback?.previewFrameUrl}
                        trimBadgeLabel={trimFeedback?.badgeLabel}
                        trimScrubChannel={trimScrubChannel}
                      />
                    )}
                  </div>
                );
              })()
            ) : (
              (() => {
                const realExportCam = resolveRealCameraSource(
                  exportCam,
                  config?.cameras
                );
                const desiredSpanSec =
                  trimFeedback?.mode === "preview_clip"
                    ? Math.max(
                        180,
                        Math.min(
                          MAX_EXPORT_WINDOW_SEC,
                          trimFeedback.rangeEnd - trimFeedback.rangeStart + 12
                        )
                      )
                    : 180;
                const seekAction = resolveVodSeekAction(
                  exportVodWindowRef.current,
                  {
                    camera: realExportCam,
                    targetTs: effectiveVodPlayheadTs,
                    requiredEndTs:
                      trimFeedback?.mode === "preview_clip"
                        ? trimFeedback.rangeEnd
                        : undefined,
                    windowSpanSec: desiredSpanSec,
                    configuredCameras: config?.cameras,
                  }
                );
                exportVodWindowRef.current = seekAction.window;
                const pauseOnSeek =
                  isDraggingTrim ||
                  trimFeedback?.mode === "settled_in" ||
                  trimFeedback?.mode === "settled_out";
                const scrubPreviewUrl = isDraggingTrim
                  ? trimFeedback.previewFrameUrl
                  : null;
                return (
                  <VodHlsPlayer
                    camera={realExportCam}
                    startTime={seekAction.window.startSec}
                    endTime={seekAction.window.endSec}
                    playheadTs={effectiveVodPlayheadTs}
                    scrubTargetTs={
                      isDraggingTrim ? trimFeedback?.targetTs : null
                    }
                    trimScrubChannel={isDraggingTrim ? trimScrubChannel : null}
                    pauseOnSeek={pauseOnSeek}
                    clipBounds={
                      trimFeedback?.mode === "preview_clip"
                        ? {
                            startSec: trimFeedback.rangeStart,
                            endSec: trimFeedback.rangeEnd,
                            loop: true,
                          }
                        : null
                    }
                    scrubPreviewUrl={scrubPreviewUrl}
                    trimBadgeLabel={trimFeedback?.badgeLabel ?? null}
                  />
                );
              })()
            )}
          </div>

          <TimelineScrubber
            camera={exportCam}
            cameras={cameraNames}
            onSelectCamera={handleSelectExportCamera}
            segments={focusSegments}
            reviews={reviews}
            currentTs={timeShiftTs}
            clipRequest={autoSeedClipRequest}
            onSeek={(ts) => {
              setActiveExportPreview(null);
              setTimeShiftTs(ts);
            }}
            includeAudio={includeAudio}
            onIncludeAudioChange={setIncludeAudio}
            trimScrubChannel={trimScrubChannel}
            onTrimFeedback={(state) => {
              if (state) setActiveExportPreview(null);
              setTrimFeedback(state);
            }}
            onExportRange={async (cam, start, end, label, audioOpt) => {
              const cleanCam = (cam || "").trim();
              if (!cleanCam) {
                notify("Select a valid camera channel before exporting");
                return;
              }
              const win = validateExportTimeWindow(start, end);
              if (!win.valid) {
                notify(win.error || "Invalid export time window");
                return;
              }
              const realTargetCam = resolveRealCameraSource(
                cleanCam,
                config?.cameras
              );
              return handleQuickExport(
                realTargetCam,
                win.startSec,
                win.endSec,
                label,
                audioOpt ?? includeAudio
              );
            }}
            titlePrefix="EXPORT CLIPPER"
            defaultRangeMode={true}
          />
        </div>
      </div>
    </>
  );
};
