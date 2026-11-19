import React from "react";
import {
  AlertTriangle,
  Check,
  Copy,
  Download,
  FolderKanban,
  Link2,
  Loader2,
  Pencil,
  Scissors,
  ShieldOff,
  Trash2,
  Volume2,
  VolumeX,
  X,
} from "lucide-react";
import { useI18n } from "../i18n";
import {
  ExportCaseItem,
  ExportItem,
  ExportJobItem,
  NinjaConfigResponse,
  RecordingSegment,
  ReviewItem,
  ShareLinkItem,
} from "../types/nvr";
import { formatDateTime, formatDuration } from "../utils/format";
import { resolveRealCameraSource, useNvrStore } from "../store/useNvrStore";
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
import { useTwoStepConfirm } from "./settings/vision/useTwoStepConfirm";

function sanitizeExportDownloadName(rawName: string, fallbackId: string): string {
  const cleaned = (rawName || "")
    .replace(/[\u0000-\u001f\u007f\u0080-\u009f\u202a-\u202e\u2066-\u2069<>:"/\\|?*]+/g, "_")
    .replace(/\s+/g, "_")
    .replace(/^[._]+|[._]+$/g, "")
    .slice(0, 96);
  return cleaned || fallbackId || "clip";
}

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
  userRole?: string;
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
  userRole = "admin",
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const isViewer = userRole === "viewer" || userRole === "read_only";
  const isAdmin = userRole === "admin";
  const { isArmed, confirmOrArm } = useTwoStepConfirm(3000);
  const [renamingExportId, setRenamingExportId] = React.useState<string | null>(null);
  const [renameDraft, setRenameDraft] = React.useState("");
  const [renameBusy, setRenameBusy] = React.useState(false);
  const [lowerPanelTab, setLowerPanelTab] = React.useState<"links" | "cases">("links");
  const [casesList, setCasesList] = React.useState<ExportCaseItem[]>([]);
  const [newCaseName, setNewCaseName] = React.useState("");
  const [caseBusy, setCaseBusy] = React.useState(false);
  const unmutedLiveCamera = useNvrStore((s) => s.unmutedLiveCamera);
  const toggleLiveCameraAudio = useNvrStore((s) => s.toggleLiveCameraAudio);
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
    fetch("/api/cases", { credentials: "include" })
      .then((r) => (r.ok ? r.json() : null))
      .then((arr: ExportCaseItem[] | null) => {
        if (isMountedRef.current && Array.isArray(arr)) {
          setCasesList(arr);
        }
      })
      .catch(() => {});
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

  const inProgressDbExports = React.useMemo(
    () =>
      exportsList.filter(
        (ex) => ex.in_progress && !visibleJobs.some((j) => j.id === ex.id)
      ),
    [exportsList, visibleJobs]
  );

  const handleRenameExport = async (id: string, nextNameRaw: string) => {
    const clean = nextNameRaw.trim();
    if (!clean || renameBusy) return;
    setRenameBusy(true);
    try {
      const res = await fetch(`/api/export/${encodeURIComponent(id)}/rename`, {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ name: clean }),
      });
      const data = await res.json().catch(() => ({}));
      if (!isMountedRef.current) return;
      if (res.ok) {
        setExportsList((prev) =>
          prev.map((item) => (item.id === id ? { ...item, name: clean } : item))
        );
        if (activeExportPreview?.id === id) {
          setActiveExportPreview({ ...activeExportPreview, name: clean });
        }
        setRenamingExportId(null);
        notify(isPt ? "Exportação renomeada" : "Export renamed");
      } else {
        notify(
          (typeof data?.message === "string" && data.message) ||
            (isPt ? "Falha ao renomear exportação" : "Failed to rename export")
        );
      }
    } catch {
      if (isMountedRef.current) {
        notify(isPt ? "Erro de rede ao renomear exportação" : "Network error renaming export");
      }
    } finally {
      if (isMountedRef.current) {
        setRenameBusy(false);
      }
    }
  };

  const handleCreateCase = async () => {
    const cleanName = newCaseName.trim();
    if (!cleanName || caseBusy) return;
    setCaseBusy(true);
    try {
      const attachedExports = activeExportPreview ? [activeExportPreview.id] : [];
      const res = await fetch("/api/cases", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          name: cleanName,
          export_ids: attachedExports,
          event_ids: [],
        }),
      });
      const data = await res.json().catch(() => null);
      if (!isMountedRef.current) return;
      const unwrapped = (
        data && typeof data === "object" && data.case && typeof data.case === "object"
          ? data.case
          : data
      ) as Partial<ExportCaseItem> | null;
      if (
        res.ok &&
        unwrapped &&
        typeof unwrapped.id === "string" &&
        typeof unwrapped.name === "string"
      ) {
        const createdItem: ExportCaseItem = {
          id: unwrapped.id,
          name: unwrapped.name,
          description: unwrapped.description ?? null,
          created_at: unwrapped.created_at ?? Math.floor(Date.now() / 1000),
          updated_at: unwrapped.updated_at ?? Math.floor(Date.now() / 1000),
          event_ids: Array.isArray(unwrapped.event_ids) ? unwrapped.event_ids : [],
          export_ids: Array.isArray(unwrapped.export_ids) ? unwrapped.export_ids : attachedExports,
        };
        setCasesList((prev) => [
          createdItem,
          ...prev.filter((c) => c.id !== createdItem.id),
        ]);
        setNewCaseName("");
        notify(isPt ? "Caso forense criado" : "Investigation case created");
      } else {
        notify(
          (typeof data?.message === "string" && data.message) ||
            (isPt ? "Falha ao criar caso" : "Failed to create case")
        );
      }
    } catch {
      if (isMountedRef.current) {
        notify(isPt ? "Erro de rede ao criar caso" : "Network error creating case");
      }
    } finally {
      if (isMountedRef.current) {
        setCaseBusy(false);
      }
    }
  };

  const handleDeleteCase = async (caseId: string) => {
    try {
      const res = await fetch(`/api/cases/${encodeURIComponent(caseId)}`, {
        method: "DELETE",
        credentials: "include",
      });
      if (!isMountedRef.current) return;
      if (res.ok) {
        setCasesList((prev) => prev.filter((c) => c.id !== caseId));
        notify(isPt ? "Caso removido" : "Investigation case deleted");
      } else {
        notify(isPt ? "Falha ao remover caso" : "Failed to delete case");
      }
    } catch {
      if (isMountedRef.current) {
        notify(isPt ? "Erro de rede ao remover caso" : "Network error deleting case");
      }
    }
  };

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

            {/* Persisted In-Progress Exports (not already tracked in active job queue) */}
            {inProgressDbExports.map((ex) => (
              <div
                key={`inprog-${ex.id}`}
                className="nvr-list-row"
                style={{
                  padding: "8px 12px",
                  flexDirection: "column",
                  alignItems: "stretch",
                  gap: 6,
                  cursor: "default",
                }}
              >
                <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 8 }}>
                  <span className="nvr-badge motion mono" style={{ display: "inline-flex", alignItems: "center", gap: 5 }}>
                    <Loader2 size={11} className="spin" />
                    {isPt ? "EM PROCESSAMENTO" : "TRANSCODING"}
                  </span>
                  <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {ex.camera.toUpperCase()}
                  </span>
                </div>
                <div className="mono" style={{ fontWeight: 700, wordBreak: "break-word" }}>
                  {ex.name}
                </div>
              </div>
            ))}

            {/* Saved Completed Exports */}
            {completedExports.map((ex) => {
              const safeThumb = ex.thumb_path
                ? sanitizeTimelineThumbUrl(ex.thumb_path, ex.camera)
                : "";
              const isPreviewSelected = activeExportPreview?.id === ex.id;
              const isRenaming = renamingExportId === ex.id;
              const delKey = `delete-export:${ex.id}`;
              const delArmed = isArmed(delKey);
              return (
                <div
                  key={ex.id}
                  className={`nvr-list-row ${
                    isPreviewSelected ? "is-selected" : ""
                  }`}
                  style={{
                    padding: "8px 12px",
                    flexDirection: "column",
                    alignItems: "stretch",
                    gap: 8,
                    cursor: "default",
                  }}
                >
                  <button
                    type="button"
                    className="nvr-row-primary-btn"
                    aria-pressed={isPreviewSelected}
                    onClick={() => setActiveExportPreview(ex)}
                    style={{ width: "100%", alignItems: "flex-start" }}
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
                    <div style={{ minWidth: 0, flex: 1, textAlign: "left" }}>
                      <div
                        className="mono"
                        title={ex.name}
                        style={{
                          fontWeight: 700,
                          lineHeight: 1.3,
                          display: "-webkit-box",
                          WebkitLineClamp: 2,
                          WebkitBoxOrient: "vertical",
                          overflow: "hidden",
                          wordBreak: "break-word",
                        }}
                      >
                        {ex.name}
                      </div>
                      <div
                        className="mono"
                        style={{
                          fontSize: "var(--text-xs)",
                          color: "var(--text-secondary)",
                          marginTop: 3,
                        }}
                      >
                        {ex.camera.toUpperCase()} · {formatDateTime(ex.date, { locale })}
                      </div>
                    </div>
                  </button>
                  {isRenaming && !isViewer && (
                    <form
                      onSubmit={(ev) => {
                        ev.preventDefault();
                        void handleRenameExport(ex.id, renameDraft);
                      }}
                      style={{ display: "flex", alignItems: "center", gap: 6 }}
                    >
                      <input
                        className="nvr-input sm mono"
                        style={{ flex: 1, minWidth: 0 }}
                        aria-label={isPt ? "Novo nome do clipe" : "New export name"}
                        value={renameDraft}
                        onChange={(ev) => setRenameDraft(ev.target.value)}
                        autoFocus
                      />
                      <button
                        type="submit"
                        className="nvr-btn sm primary mono"
                        disabled={renameBusy || !renameDraft.trim()}
                      >
                        <Check size={12} />
                      </button>
                      <button
                        type="button"
                        className="nvr-btn sm mono"
                        onClick={() => setRenamingExportId(null)}
                      >
                        <X size={12} />
                      </button>
                    </form>
                  )}
                  <div style={{ display: "flex", gap: 6, flexWrap: "wrap", justifyContent: "flex-end" }}>
                    {!isViewer && (
                      <button
                        type="button"
                        className="nvr-btn sm mono"
                        aria-label={`${t.exports.renameBtn}: ${ex.name}`}
                        title={t.exports.renameBtn}
                        onClick={() => {
                          setRenamingExportId(ex.id);
                          setRenameDraft(ex.name);
                        }}
                      >
                        <Pencil size={11} />
                        {t.exports.renameBtn}
                      </button>
                    )}
                    {!isViewer && (
                      <button
                        type="button"
                        className="nvr-btn sm mono"
                        title="Copy expiring HMAC-signed shareable link"
                        disabled={
                          creatingShareId !== null || revokingShareId !== null
                        }
                        onClick={() => {
                          void handleCopyShareLink(ex.id);
                        }}
                      >
                        {t.review.shareLinkBtn}
                      </button>
                    )}
                    <a
                      href={resolveSafeExportVideoUrl(ex.video_path, ex.id)}
                      download={`${sanitizeExportDownloadName(ex.name, ex.id)}.mp4`}
                      className="nvr-btn sm"
                      style={{ textDecoration: "none" }}
                      aria-label={`Download ${ex.name} MP4`}
                      title={t.exports.downloadMp4}
                    >
                      <Download size={12} />
                    </a>
                    {isAdmin && (
                      <button
                        type="button"
                        className="nvr-btn sm danger mono"
                        aria-label={
                          delArmed
                            ? isPt
                              ? `Confirmar exclusão de ${ex.name}`
                              : `Click again to confirm deleting ${ex.name}`
                            : `Delete export ${ex.name}`
                        }
                        title={
                          delArmed
                            ? isPt
                              ? "Clique novamente para confirmar exclusão permanente"
                              : "Click again to confirm permanent deletion"
                            : t.exports.deleteBtn
                        }
                        disabled={deletingExportId === ex.id}
                        onClick={() => {
                          if (deletingExportId !== null) return;
                          confirmOrArm(delKey, async () => {
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
                                notify(isPt ? "Exportação excluída" : "Export deleted");
                              } else {
                                const data = await res.json().catch(() => ({}));
                                if (isMountedRef.current) {
                                  notify(
                                    (typeof data?.message === "string" &&
                                      data.message) ||
                                      (isPt ? "Falha ao excluir exportação" : "Failed to delete export")
                                  );
                                }
                              }
                            } catch {
                              if (isMountedRef.current) {
                                notify(isPt ? "Falha ao excluir exportação" : "Failed to delete export");
                              }
                            } finally {
                              if (isMountedRef.current) {
                                setDeletingExportId(null);
                              }
                            }
                          });
                        }}
                      >
                        {delArmed ? (isPt ? "Confirmar?" : "Confirm?") : <Trash2 size={12} />}
                      </button>
                    )}
                  </div>
                </div>
              );
            })}
            {completedExports.length === 0 &&
              inProgressDbExports.length === 0 &&
              visibleJobs.length === 0 && (
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
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <button
                  type="button"
                  className={`nvr-btn sm mono ${lowerPanelTab === "links" ? "active" : ""}`}
                  onClick={() => setLowerPanelTab("links")}
                >
                  <Link2 size={11} />
                  {t.exports.activeShareLinks} ({visibleShareLinks.length})
                </button>
                <button
                  type="button"
                  className={`nvr-btn sm mono ${lowerPanelTab === "cases" ? "active" : ""}`}
                  data-testid="exports-cases-tab"
                  onClick={() => setLowerPanelTab("cases")}
                >
                  <FolderKanban size={11} />
                  {isPt ? "CASOS" : "CASES"} ({casesList.length})
                </button>
              </div>
              {lowerPanelTab === "links" && (
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
              )}
            </div>
            {lowerPanelTab === "cases" ? (
              <div className="nvr-scroll-y" style={{ padding: "8px 12px", display: "flex", flexDirection: "column", gap: 8 }}>
                {!isViewer && (
                  <form
                    onSubmit={(ev) => {
                      ev.preventDefault();
                      void handleCreateCase();
                    }}
                    style={{ display: "flex", gap: 6 }}
                  >
                    <input
                      className="nvr-input sm mono"
                      style={{ flex: 1, minWidth: 0 }}
                      placeholder={
                        isPt
                          ? "Novo caso (ex.: INC-2026-01)..."
                          : "New case title (e.g. INC-2026-01)..."
                      }
                      value={newCaseName}
                      onChange={(ev) => setNewCaseName(ev.target.value)}
                    />
                    <button
                      type="submit"
                      className="nvr-btn sm primary mono"
                      disabled={caseBusy || !newCaseName.trim()}
                    >
                      + {isPt ? "Criar" : "Create"}
                    </button>
                  </form>
                )}
                {casesList.map((cs) => {
                  const caseDelKey = `del-case:${cs.id}`;
                  const caseDelArmed = isArmed(caseDelKey);
                  return (
                    <div
                      key={cs.id}
                      className="nvr-list-row"
                      style={{
                        padding: "8px 10px",
                        flexDirection: "column",
                        alignItems: "stretch",
                        gap: 4,
                        cursor: "default",
                      }}
                    >
                      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 6 }}>
                        <span className="mono" style={{ fontWeight: 700, wordBreak: "break-word" }}>
                          {cs.name}
                        </span>
                        {isAdmin && (
                          <button
                            type="button"
                            className="nvr-btn sm danger mono"
                            onClick={() => {
                              confirmOrArm(caseDelKey, () => {
                                void handleDeleteCase(cs.id);
                              });
                            }}
                          >
                            {caseDelArmed ? (isPt ? "Confirmar?" : "Confirm?") : <Trash2 size={11} />}
                          </button>
                        )}
                      </div>
                      {cs.description && (
                        <div style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                          {cs.description}
                        </div>
                      )}
                      <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                        {(cs.export_ids?.length || 0)} {isPt ? "clipe(s)" : "clip(s)"} · {(cs.event_ids?.length || 0)} {isPt ? "evento(s)" : "event(s)"}
                      </div>
                    </div>
                  );
                })}
                {casesList.length === 0 && (
                  <div style={{ padding: 14, textAlign: "center", color: "var(--text-muted)", fontSize: "var(--text-xs)" }}>
                    {isPt
                      ? "Nenhum caso forense criado ainda."
                      : "No investigation cases created yet."}
                  </div>
                )}
              </div>
            ) : (
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
            )}
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
            ) : !exportCam ? (
              <div
                className="mono"
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  color: "var(--text-muted)",
                  padding: 24,
                }}
              >
                {t.live.noCamerasConfigured}
              </div>
            ) : effectiveVodPlayheadTs === null ||
              !Number.isFinite(effectiveVodPlayheadTs) ? (
              (() => {
                const realExportCam = resolveRealCameraSource(
                  exportCam,
                  config?.cameras
                );
                const isExportLiveUnmuted = unmutedLiveCamera === exportCam;
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
                      muted={!isExportLiveUnmuted}
                      cameraConfig={
                        config?.cameras?.[exportCam] ??
                        config?.cameras?.[realExportCam]
                      }
                    />
                    {!isDraggingTrim && (
                      <div className="nvr-cam-hud pos-bottom-right">
                        <button
                          type="button"
                          className={`nvr-tile-icon ${
                            isExportLiveUnmuted ? "active" : ""
                          }`}
                          aria-pressed={isExportLiveUnmuted}
                          aria-label={`${
                            isExportLiveUnmuted
                              ? t.live.muteAudio
                              : t.live.unmuteAudio
                          }: ${exportCam}`}
                          title={
                            isExportLiveUnmuted
                              ? t.live.muteAudio
                              : t.live.unmuteAudio
                          }
                          onClick={() => toggleLiveCameraAudio(exportCam)}
                        >
                          {isExportLiveUnmuted ? (
                            <Volume2 size={15} />
                          ) : (
                            <VolumeX size={15} />
                          )}
                        </button>
                      </div>
                    )}
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
