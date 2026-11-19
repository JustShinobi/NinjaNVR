import React, { Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Clock,
  Download,
  LayoutGrid,
  ListChecks,
  LogOut,
  Monitor,
  Moon,
  MoreHorizontal,
  RefreshCw,
  Sun,
  X,
} from "lucide-react";
import { formatBadgeCount, resolveMobileNavTabs } from "./utils/liveTileHud";
import { useCompactViewport } from "./utils/useCompactViewport";
import { useSheetDismiss } from "./utils/useSheetDismiss";
import { useI18n } from "./i18n";
import {
  EventItem,
  ExportItem,
  ExportJobItem,
  FfprobeItem,
  NinjaConfigResponse,
  NinjaStatsResponse,
  RecordingDaySummary,
  RecordingSegment,
  ReviewItem,
} from "./types/nvr";
import {
  parseExportJobPayload,
  reconcileExportJobList,
} from "./utils/exportClipFlow";
import {
  WorkspaceTab,
  applyThemeToDocument,
  cycleThemeMode,
  formatDisplayVersion,
  formatTs,
  resolveHeaderTelemetryMeta,
  resolveInitialTab,
} from "./utils/format";
import {
  expandSimulatedFleet,
  resolveRealCameraSource,
  resolveValidCameraSelection,
  useNvrStore,
} from "./store/useNvrStore";
import { formatMeasured, parseMeasured, resolvePrimaryInferenceMs } from "./utils/detectorTelemetry";
import { LiveWorkspace } from "./views/LiveWorkspace";
import {
  type ConfigWorkbenchSession,
  ConfigWorkbenchProvider,
  createConfigWorkbenchSession,
  createFetchConfigTransport,
  useConfigWorkbenchSnapshot,
} from "./utils/configWorkbench";
import {
  type ArchiveMetadataCache,
  ArchiveMetadataCacheProvider,
  createArchiveMetadataCache,
} from "./utils/archivePlayback";
import { invalidateCameraHardwareCache } from "./utils/cameraCapabilities";
import { resolveSafeMediaUrl } from "./utils/mediaUrlPolicy";
import {
  type EventMutationPatch,
  type ReviewMutationPatch,
  buildEventsFeedUrl,
  buildReviewFeedUrl,
  matchesOperationalDateBounds,
  parseOperationalDateBoundsSec,
  reconcileDeletedFaceProfile,
  reconcileEventMutation,
  reconcileReviewMutation,
} from "./utils/operationalFeed";

const SyncWorkspace = React.lazy(() =>
  import("./views/SyncWorkspace").then((m) => ({ default: m.SyncWorkspace }))
);
const ReviewWorkspace = React.lazy(() =>
  import("./views/ReviewWorkspace").then((m) => ({ default: m.ReviewWorkspace }))
);
const ExploreWorkspace = React.lazy(() =>
  import("./views/ExploreWorkspace").then((m) => ({ default: m.ExploreWorkspace }))
);
const ExportsWorkspace = React.lazy(() =>
  import("./views/ExportsWorkspace").then((m) => ({ default: m.ExportsWorkspace }))
);
const ZonesWorkspace = React.lazy(() =>
  import("./views/ZonesWorkspace").then((m) => ({ default: m.ZonesWorkspace }))
);
const SystemWorkspace = React.lazy(() =>
  import("./views/SystemWorkspace").then((m) => ({ default: m.SystemWorkspace }))
);

export const MAX_LIVE_EVENTS = 250;
export const MAX_LIVE_REVIEWS = 300;
export const WS_HEALTHY_HEARTBEAT_MS = 60_000;
export const WS_FALLBACK_POLL_MS = 10_000;

export const WORKSPACE_TAB_URL_MAP: Record<WorkspaceTab, string> = {
  live: "/",
  sync: "/sync",
  review: "/review",
  explore: "/explore",
  exports: "/export",
  zones: "/config",
  system: "/system",
};

export function isWorkspaceTab(val: unknown): val is WorkspaceTab {
  return (
    val === "live" ||
    val === "sync" ||
    val === "review" ||
    val === "explore" ||
    val === "exports" ||
    val === "zones" ||
    val === "system"
  );
}

export function normalizeWsTopic(rawTopic: unknown): string {
  if (typeof rawTopic !== "string") return "";
  const clean = rawTopic.trim();
  return clean.startsWith("ninjanvr/") ? clean.slice("ninjanvr/".length) : clean;
}

export function resolveCoreDataPollIntervalMs(
  wsOnline: boolean,
  hasActiveExportJobs: boolean
): number {
  if (!wsOnline || hasActiveExportJobs) {
    return WS_FALLBACK_POLL_MS;
  }
  return WS_HEALTHY_HEARTBEAT_MS;
}

export function upsertBoundedEventList(
  prev: EventItem[],
  incoming: EventItem,
  maxItems = MAX_LIVE_EVENTS
): EventItem[] {
  const cap = Math.max(1, Math.floor(maxItems));
  const idx = prev.findIndex((x) => x.id === incoming.id);
  if (idx >= 0) {
    const copy = [...prev];
    copy[idx] = { ...prev[idx], ...incoming };
    return copy.slice(0, cap);
  }
  return [incoming, ...prev].slice(0, cap);
}

export function prependBoundedReviewAlert(
  prev: ReviewItem[],
  incoming: ReviewItem,
  maxItems = MAX_LIVE_REVIEWS
): ReviewItem[] {
  const cap = Math.max(1, Math.floor(maxItems));
  if (prev.some((r) => r.id === incoming.id)) {
    return prev.slice(0, cap);
  }
  return [incoming, ...prev].slice(0, cap);
}

export function upsertBoundedReviewList(
  prev: ReviewItem[],
  incoming: ReviewItem,
  maxItems = MAX_LIVE_REVIEWS
): ReviewItem[] {
  const cap = Math.max(1, Math.floor(maxItems));
  const idx = prev.findIndex((r) => r.id === incoming.id);
  if (idx >= 0) {
    const copy = [...prev];
    copy[idx] = {
      ...prev[idx],
      ...incoming,
      data: incoming.data
        ? { ...(prev[idx].data || {}), ...incoming.data }
        : prev[idx].data,
    };
    return copy.slice(0, cap);
  }
  return [incoming, ...prev].slice(0, cap);
}

export function extractOidcCallbackError(
  search: string,
  pathname = ""
): string | null {
  try {
    const rawSearch = (search || "").trim();
    if (rawSearch) {
      const params = new URLSearchParams(
        rawSearch.startsWith("?") ? rawSearch.slice(1) : rawSearch
      );
      const errCode =
        params.get("oidc_error") ||
        params.get("auth_error") ||
        params.get("error");
      const errDesc = params.get("error_description");
      if (errCode || errDesc) {
        const cleanDesc = (errDesc || errCode || "SSO authentication failed")
          .replace(/[\u0000-\u001f\u007f]/g, " ")
          .trim()
          .slice(0, 200);
        return cleanDesc || "SSO authentication failed";
      }
    }
    if (pathname.startsWith("/api/auth/oidc/callback")) {
      return "SSO callback failed";
    }
  } catch {
    // ignore malformed URL query
  }
  return null;
}

export function sanitizeExportVideoUrl(
  rawUrl: string | undefined | null,
  fallbackCamera: string,
  startSec: number,
  endSec: number
): string {
  return resolveSafeMediaUrl(rawUrl, {
    kind: "export_video",
    fallbackCamera,
    startSec,
    endSec,
    allowedPrefixes: ["/exports/", "/api/", "/vod/", "/clips/"],
    rejectSpaces: true,
  });
}

export function playTacticalAlertChime(): void {
  try {
    const AudioCtx =
      window.AudioContext ||
      (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext;
    if (!AudioCtx) return;
    const ctx = new AudioCtx();
    const closeCtx = () => {
      try {
        if (ctx.state !== "closed") {
          void ctx.close().catch(() => {});
        }
      } catch {
        // ignore close error
      }
    };
    const now = ctx.currentTime;
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = "sine";
    osc.frequency.setValueAtTime(880, now);
    osc.frequency.setValueAtTime(1174.66, now + 0.11);
    gain.gain.setValueAtTime(0.08, now);
    gain.gain.exponentialRampToValueAtTime(0.001, now + 0.28);
    osc.connect(gain);
    gain.connect(ctx.destination);
    osc.onended = closeCtx;
    osc.start(now);
    osc.stop(now + 0.28);
    if (typeof window !== "undefined" && typeof window.setTimeout === "function") {
      window.setTimeout(closeCtx, 400);
    }
  } catch {
    // ignore autoplay restriction
  }
}

export const App: React.FC = () => {
  const { t, locale } = useI18n();
  const activeTab = useNvrStore((s) => s.activeTab);
  const isCompact = useCompactViewport();
  const [mobileMoreOpen, setMobileMoreOpen] = useState(false);
  const [profileSheetOpen, setProfileSheetOpen] = useState(false);
  const profileSheetRef = useSheetDismiss<HTMLElement>(isCompact && profileSheetOpen, () =>
    setProfileSheetOpen(false)
  );
  const moreSheetRef = useSheetDismiss<HTMLElement>(isCompact && mobileMoreOpen, () =>
    setMobileMoreOpen(false)
  );
  const setActiveTab = useNvrStore((s) => s.setActiveTab);
  const securityProfile = useNvrStore((s) => s.securityProfile);
  const setSecurityProfile = useNvrStore((s) => s.setSecurityProfile);
  const themeMode = useNvrStore((s) => s.themeMode);
  const setThemeMode = useNvrStore((s) => s.setThemeMode);
  const highContrast = useNvrStore((s) => s.highContrast);
  const selectedCamera = useNvrStore((s) => s.selectedCamera);
  const setSelectedCamera = useNvrStore((s) => s.setSelectedCamera);
  const timeShiftTs = useNvrStore((s) => s.timeShiftTs);
  const setTimeShiftTs = useNvrStore((s) => s.setTimeShiftTs);
  const wsOnline = useNvrStore((s) => s.wsOnline);
  const setWsOnline = useNvrStore((s) => s.setWsOnline);
  const coreDataError = useNvrStore((s) => s.coreDataError);
  const setCoreDataError = useNvrStore((s) => s.setCoreDataError);
  const config = useNvrStore((s) => s.config);
  const setConfig = useNvrStore((s) => s.setConfig);
  const stats = useNvrStore((s) => s.stats);
  const setStats = useNvrStore((s) => s.setStats);
  const appendStatsSample = useNvrStore((s) => s.appendStatsSample);
  const setStatsHistory = useNvrStore((s) => s.setStatsHistory);
  const setCameraActivity = useNvrStore((s) => s.setCameraActivity);
  const simulatedFleetCount = useNvrStore((s) => s.simulatedFleetCount);
  const setSimulatedFleetCount = useNvrStore((s) => s.setSimulatedFleetCount);
  const lastDeletedFaceProfile = useNvrStore((s) => s.lastDeletedFaceProfile);
  const subLabelsRefreshTick = useNvrStore((s) => s.subLabelsRefreshTick);

  const [authChecked, setAuthChecked] = useState(false);
  const [authNeeded, setAuthNeeded] = useState(false);
  const manualLogoutRef = useRef(false);
  const coreFetchSeqRef = useRef(0);
  const [username, setUsername] = useState("admin");
  const [loginUser, setLoginUser] = useState("admin");
  const [userRole, setUserRole] = useState<string>("admin");
  const userRoleRef = useRef<string>(userRole);
  userRoleRef.current = userRole;
  const [selectedNode, setSelectedNode] = useState<string>("local");
  const [authProviders, setAuthProviders] = useState<{
    local_login_enabled: boolean;
    oidc?: { enabled: boolean; provider_name: string; auto_redirect: boolean };
  }>({ local_login_enabled: true });
  const [loginPass, setLoginPass] = useState("");
  const [loginError, setLoginError] = useState<string | null>(null);
  const [loginBusy, setLoginBusy] = useState(false);
  const [toastMsg, setToastMsg] = useState<string | null>(null);
  const [focusSegments, setFocusSegments] = useState<RecordingSegment[]>([]);
  const [globalDaysSummary, setGlobalDaysSummary] = useState<RecordingDaySummary[]>([]);

  // Review & Triage state
  const [reviews, setReviews] = useState<ReviewItem[]>([]);
  const [reviewCameraFilter, setReviewCameraFilter] = useState<string>("all");
  const [reviewSeverityFilter, setReviewSeverityFilter] = useState<string>("all");
  const [reviewObjectFilter, setReviewObjectFilter] = useState<string>("all");
  const [reviewDateFilter, setReviewDateFilter] = useState<string>("");
  const [reviewStatusFilter, setReviewStatusFilter] = useState<"unreviewed" | "reviewed" | "all">(
    "unreviewed"
  );
  const [activeReview, setActiveReview] = useState<ReviewItem | null>(null);

  // Sync Playback state
  const [syncMasterTs, setSyncMasterTs] = useState<number>(
    () => Math.floor(Date.now() / 1000) - 120
  );
  const [syncSpeed, setSyncSpeed] = useState<number>(1);

  // Object Explore state
  const [events, setEvents] = useState<EventItem[]>([]);
  const [exploreQuery, setExploreQuery] = useState("");
  const [exploreCamera, setExploreCamera] = useState("all");
  const [exploreLabel, setExploreLabel] = useState("all");
  const [exploreRetainedOnly, setExploreRetainedOnly] = useState(false);
  const [selectedEvent, setSelectedEvent] = useState<EventItem | null>(null);
  const [eventViewMode, setEventViewMode] = useState<"snapshot" | "clip">("snapshot");

  // Exports state
  const [exportsList, setExportsList] = useState<ExportItem[]>([]);
  const [exportJobs, setExportJobs] = useState<ExportJobItem[]>([]);
  const notifiedJobTerminalStatusRef = useRef<Map<string, string>>(new Map());
  const [exportCam, setExportCam] = useState<string>("");
  const [exportPresetMin, setExportPresetMin] = useState<number>(1);
  const [exportLabel, setExportLabel] = useState<string>("");
  const [activeExportPreview, setActiveExportPreview] = useState<ExportItem | null>(null);

  // Zones & Config Studio state
  const [zonesSubTab, setZonesSubTab] = useState<"visual" | "yaml">("visual");
  const [inspectorCam, setInspectorCam] = useState<string>("");
  const workbenchSessionRef = useRef<ConfigWorkbenchSession | null>(null);
  if (!workbenchSessionRef.current) {
    workbenchSessionRef.current = createConfigWorkbenchSession(
      createFetchConfigTransport(),
      {
        initialConfig: config,
        onServerCommitted: (nextCfg) => {
          if (nextCfg) {
            setConfig(nextCfg);
          }
        },
      }
    );
  }
  const workbenchSession = workbenchSessionRef.current;
  const archiveMetadataCacheRef = useRef<ArchiveMetadataCache | null>(null);
  if (!archiveMetadataCacheRef.current) {
    archiveMetadataCacheRef.current = createArchiveMetadataCache({
      getConfiguredCameras: () => useNvrStore.getState().config?.cameras,
    });
  }
  const archiveMetadataCache = archiveMetadataCacheRef.current;
  const workbenchSnap = useConfigWorkbenchSnapshot(workbenchSession);
  const rawYaml = workbenchSnap.rawYaml;
  const setRawYaml = useCallback(
    (nextYaml: string) => {
      workbenchSession.setRawYamlDraft(nextYaml);
    },
    [workbenchSession]
  );
  const [draftPoints, setDraftPoints] = useState<Array<[number, number]>>([]);
  const [ffprobeInfo, setFfprobeInfo] = useState<FfprobeItem[] | null>(null);

  useEffect(() => {
    if (config) {
      workbenchSession.hydrateFromServer(config);
    }
  }, [config, workbenchSession]);

  // Reconcile exportCam & inspectorCam automatically whenever config.cameras loads or changes
  useEffect(() => {
    const cams = config?.cameras;
    if (!cams) return;
    const allCams = Object.keys(cams);
    if (allCams.length === 0) return;
    const enabledCams = allCams.filter((k) => cams[k]?.enabled !== false);
    const validCams = enabledCams.length > 0 ? enabledCams : allCams;
    setInspectorCam((curr) =>
      !curr || !allCams.includes(curr) ? validCams[0] : curr
    );
    setExportCam((curr) =>
      !curr || !validCams.includes(curr) ? validCams[0] : curr
    );
  }, [config?.cameras]);

  // System & Logs state
  const [logService, setLogService] = useState<"ninjanvr" | "go2rtc" | "nginx" | "cameras">("ninjanvr");
  const [logLines, setLogLines] = useState<string[]>([]);
  const [logSearch, setLogSearch] = useState("");
  const [logPaused, setLogPaused] = useState(false);

  const toastTimerRef = useRef<number | null>(null);

  const notify = useCallback((msg: string) => {
    setToastMsg(msg);
    if (toastTimerRef.current !== null && typeof window !== "undefined") {
      window.clearTimeout(toastTimerRef.current);
    }
    if (typeof window !== "undefined") {
      toastTimerRef.current = window.setTimeout(() => {
        toastTimerRef.current = null;
        setToastMsg((curr) => (curr === msg ? null : curr));
      }, 3200);
    }
  }, []);

  useEffect(() => {
    return () => {
      if (toastTimerRef.current !== null && typeof window !== "undefined") {
        window.clearTimeout(toastTimerRef.current);
        toastTimerRef.current = null;
      }
    };
  }, []);

  // Synchronize documentElement data-theme and data-high-contrast attributes + OS prefers-color-scheme listener
  useEffect(() => {
    applyThemeToDocument(themeMode, highContrast);
    if (
      themeMode !== "system" ||
      typeof window === "undefined" ||
      typeof window.matchMedia !== "function"
    ) {
      return;
    }
    const mql = window.matchMedia("(prefers-color-scheme: light)");
    const onChange = (ev: MediaQueryListEvent) => {
      applyThemeToDocument("system", highContrast, ev.matches);
    };
    if (typeof mql.addEventListener === "function") {
      mql.addEventListener("change", onChange);
      return () => mql.removeEventListener("change", onChange);
    }
  }, [themeMode, highContrast]);

  // Update document.title dynamically on workspace transitions
  useEffect(() => {
    if (typeof document !== "undefined") {
      document.title = `${t.nav[activeTab]} | ${t.brand.name}`;
    }
  }, [activeTab, t]);

  const cameraNames = useMemo(() => {
    if (!config?.cameras) {
      return simulatedFleetCount > 0
        ? expandSimulatedFleet([], null, null, simulatedFleetCount, locale).cameraNames
        : [];
    }
    const base = Object.keys(config.cameras).filter((k) => config.cameras[k]?.enabled !== false);
    if (simulatedFleetCount > 0) {
      return expandSimulatedFleet(
        base,
        config.cameras,
        config.camera_groups,
        simulatedFleetCount,
        locale
      ).cameraNames;
    }
    return base;
  }, [config, simulatedFleetCount, locale]);

  const enabledPeers = useMemo(
    () =>
      Object.entries(config?.peers || {}).filter(([, p]) =>
        Boolean(p && p.enabled && typeof p.url === "string" && p.url.trim())
      ),
    [config?.peers]
  );

  const unreviewedAlerts = useMemo(
    () => reviews.filter((r) => !r.has_been_reviewed),
    [reviews]
  );

  // Navigate tab + push browser history entry so Browser/Mobile Back returns to previous workspace
  const switchTab = useCallback(
    (tab: WorkspaceTab, options?: { replace?: boolean }) => {
      setActiveTab(tab);
      if (typeof window === "undefined" || !window.history) return;
      const targetUrl = WORKSPACE_TAB_URL_MAP[tab];
      try {
        if (options?.replace) {
          window.history.replaceState?.({ tab }, "", targetUrl);
        } else if (window.location?.pathname !== targetUrl) {
          window.history.pushState?.({ tab }, "", targetUrl);
        }
      } catch {
        // ignore synthetic history errors
      }
    },
    [setActiveTab]
  );

  // Browser Back / Forward (`popstate`) synchronization
  useEffect(() => {
    if (typeof window === "undefined") return;
    try {
      const initialTab = useNvrStore.getState().activeTab;
      window.history?.replaceState?.(
        { tab: initialTab },
        "",
        WORKSPACE_TAB_URL_MAP[initialTab]
      );
    } catch {
      // ignore
    }
    const onPopState = (ev: PopStateEvent) => {
      const stateTab =
        ev.state && typeof ev.state === "object" ? (ev.state as { tab?: unknown }).tab : null;
      let nextTab: WorkspaceTab = isWorkspaceTab(stateTab)
        ? stateTab
        : resolveInitialTab();
      if (
        userRoleRef.current !== "admin" &&
        (nextTab === "zones" || nextTab === "system")
      ) {
        nextTab = "live";
      }
      setActiveTab(nextTab);
    };
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, [setActiveTab]);

  // Fetch core bootstrap & reconciliation data
  const fetchCoreData = useCallback(async () => {
    const mySeq = ++coreFetchSeqRef.current;
    try {
      const revUrl = buildReviewFeedUrl(reviewDateFilter);
      const [cfgRes, statsRes, histRes, revRes, expRes, profRes] = await Promise.all([
        fetch("/api/config", { credentials: "include" }),
        fetch("/api/stats", { credentials: "include" }),
        fetch("/api/stats/history", { credentials: "include" }),
        fetch(revUrl, { credentials: "include" }),
        fetch("/api/exports", { credentials: "include" }),
        fetch("/api/profile", { credentials: "include" }),
      ]);

      if (mySeq !== coreFetchSeqRef.current || manualLogoutRef.current) {
        return;
      }

      if (
        [cfgRes, statsRes, histRes, revRes, expRes, profRes].some(
          (r) => r.status === 401
        )
      ) {
        archiveMetadataCache.clear();
        invalidateCameraHardwareCache();
        setCameraActivity({});
        setAuthNeeded(true);
        setAuthChecked(true);
        return;
      }

      setAuthNeeded(false);
      setAuthChecked(true);
      if (cfgRes.ok) {
        const cfgJson: NinjaConfigResponse = await cfgRes.json();
        if (mySeq !== coreFetchSeqRef.current || manualLogoutRef.current) {
          return;
        }
        setConfig(cfgJson);
        workbenchSession.hydrateFromServer(cfgJson);
        const allCams = Object.keys(cfgJson.cameras || {});
        const enabledCams = allCams.filter(
          (k) => cfgJson.cameras?.[k]?.enabled !== false
        );
        const validCams = enabledCams.length > 0 ? enabledCams : allCams;
        const curState = useNvrStore.getState();
        const nextSelected = resolveValidCameraSelection(
          curState.selectedCamera,
          cfgJson.cameras,
          curState.simulatedFleetCount
        );
        if (nextSelected !== curState.selectedCamera) {
          setSelectedCamera(nextSelected);
        }
        setInspectorCam((curr) =>
          validCams.length > 0 && (!curr || !allCams.includes(curr))
            ? validCams[0]
            : curr
        );
        setExportCam((curr) =>
          validCams.length > 0 && (!curr || !validCams.includes(curr))
            ? validCams[0]
            : curr
        );
      }
      if (statsRes.ok) {
        const nextStats = await statsRes.json();
        if (mySeq === coreFetchSeqRef.current && !manualLogoutRef.current) {
          setStats(nextStats);
        }
      }
      if (histRes.ok) {
        const h = await histRes.json();
        if (
          mySeq === coreFetchSeqRef.current &&
          !manualLogoutRef.current &&
          Array.isArray(h)
        ) {
          setStatsHistory(h);
        }
      }
      if (revRes.ok) {
        const revJson: ReviewItem[] = await revRes.json();
        if (
          mySeq === coreFetchSeqRef.current &&
          !manualLogoutRef.current &&
          Array.isArray(revJson)
        ) {
          setCoreDataError(null);
          setReviews(revJson.slice(0, MAX_LIVE_REVIEWS));
          if (revJson.length > 0) {
            setActiveReview((curr) => {
              if (!curr) {
                setSyncMasterTs(Math.floor(revJson[0].start_time));
                return revJson[0];
              }
              return curr;
            });
          }
        }
      } else {
        setCoreDataError(`HTTP ${revRes.status} (/api/review)`);
      }
      if (expRes.ok) {
        const expJson: ExportItem[] = await expRes.json();
        if (
          mySeq === coreFetchSeqRef.current &&
          !manualLogoutRef.current &&
          Array.isArray(expJson)
        ) {
          setExportsList(expJson);
        }
      }
      fetch("/api/jobs/export", { credentials: "include" })
        .then((r) => (r.ok ? r.json() : []))
        .then((jobsArr: unknown) => {
          if (
            mySeq === coreFetchSeqRef.current &&
            !manualLogoutRef.current &&
            Array.isArray(jobsArr)
          ) {
            const parsed = jobsArr
              .map(parseExportJobPayload)
              .filter((j): j is ExportJobItem => j !== null);
            setExportJobs(parsed);
          }
        })
        .catch(() => {});
      if (profRes.ok) {
        const profJson = await profRes.json();
        if (mySeq !== coreFetchSeqRef.current || manualLogoutRef.current) {
          return;
        }
        if (profJson && typeof profJson.username === "string" && profJson.username.trim()) {
          setUsername(profJson.username.trim());
        }
        if (profJson && typeof profJson.role === "string") {
          const nextRole = profJson.role.trim().toLowerCase();
          setUserRole(nextRole);
          const curTab = useNvrStore.getState().activeTab;
          if (nextRole !== "admin" && (curTab === "zones" || curTab === "system")) {
            switchTab("live", { replace: true });
          }
        }
        if (profJson && typeof profJson.active_profile === "string") {
          const p = profJson.active_profile.trim().toLowerCase();
          if (p === "armed" || p === "home" || p === "disarmed") {
            setSecurityProfile(p);
          }
        }
      }
    } catch (err) {
      if (mySeq === coreFetchSeqRef.current && !manualLogoutRef.current) {
        setAuthChecked(true);
        const detail = err instanceof Error ? err.message : "Network error";
        setCoreDataError(detail);
      }
    }
  }, [
    reviewDateFilter,
    setConfig,
    setCoreDataError,
    setSelectedCamera,
    setStats,
    setStatsHistory,
    setSecurityProfile,
    switchTab,
    workbenchSession,
  ]);

  const fetchCoreDataRef = useRef(fetchCoreData);
  fetchCoreDataRef.current = fetchCoreData;

  useEffect(() => {
    if (!authNeeded) return;
    let cancelled = false;
    const oidcErr =
      typeof window !== "undefined"
        ? extractOidcCallbackError(window.location.search, window.location.pathname)
        : null;
    if (oidcErr) {
      setLoginError(oidcErr);
    }
    fetch("/api/auth/providers", { credentials: "include" })
      .then((r) => (r.ok ? r.json() : null))
      .then((data) => {
        if (cancelled) return;
        if (data && typeof data === "object") {
          setAuthProviders({
            local_login_enabled: data.local_login_enabled !== false,
            oidc: data.oidc,
          });
          if (
            data.oidc?.enabled &&
            data.oidc?.auto_redirect &&
            !manualLogoutRef.current &&
            !oidcErr
          ) {
            window.location.href = "/api/auth/oidc/login";
          }
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [authNeeded]);

  // Initial bootstrap + filter reconciliation
  useEffect(() => {
    if (authNeeded) return;
    void fetchCoreData();
  }, [fetchCoreData, authNeeded]);

  // Event-driven `/ws` first: 60s safety heartbeat when `/ws` is healthy,
  // 10s fallback polling ONLY when `/ws` is disconnected or export jobs are running.
  const hasActiveExportJobs = useMemo(
    () =>
      exportJobs.some((j) => j.status === "queued" || j.status === "running"),
    [exportJobs]
  );
  const pollIntervalMs = resolveCoreDataPollIntervalMs(
    wsOnline,
    hasActiveExportJobs
  );

  useEffect(() => {
    if (authNeeded) return;
    const timer = window.setInterval(() => {
      if (typeof document !== "undefined" && document.visibilityState === "hidden") {
        return;
      }
      void fetchCoreDataRef.current();
    }, pollIntervalMs);
    return () => window.clearInterval(timer);
  }, [authNeeded, pollIntervalMs]);

  // Reconcile immediately when operator returns to a backgrounded tab
  useEffect(() => {
    if (authNeeded || typeof document === "undefined") return;
    const onVisibilityChange = () => {
      if (document.visibilityState === "visible") {
        void fetchCoreDataRef.current();
      }
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => document.removeEventListener("visibilitychange", onVisibilityChange);
  }, [authNeeded]);

  // Connect to /ws for real-time camera_activity, stats, reviews, events, config, and exports
  useEffect(() => {
    if (!authChecked || authNeeded) return;
    let closed = false;
    let ws: WebSocket | null = null;
    let reconnectTimer: number | null = null;
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";

    const disposeSocket = (target: WebSocket | null) => {
      if (!target) return;
      target.onopen = null;
      target.onclose = null;
      target.onerror = null;
      target.onmessage = null;
      try {
        target.close();
      } catch {
        // ignore close errors
      }
    };

    const connect = () => {
      if (closed) return;
      if (reconnectTimer !== null) {
        window.clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      if (ws) {
        disposeSocket(ws);
        ws = null;
      }
      const currentWs = new WebSocket(`${proto}//${window.location.host}/ws`);
      ws = currentWs;

      currentWs.onopen = () => {
        if (!closed && ws === currentWs) {
          setWsOnline(true);
          void fetchCoreDataRef.current();
        }
      };
      currentWs.onclose = () => {
        if (!closed && ws === currentWs) {
          setWsOnline(false);
          if (reconnectTimer === null) {
            reconnectTimer = window.setTimeout(() => {
              reconnectTimer = null;
              connect();
            }, 2000);
          }
        }
      };
      currentWs.onmessage = (ev) => {
        if (closed || ws !== currentWs || typeof ev.data !== "string") return;
        try {
          const msg = JSON.parse(ev.data);
          const topic = normalizeWsTopic(msg?.topic);
          if (topic === "camera_activity" && msg.payload) {
            const parsed =
              typeof msg.payload === "string" ? JSON.parse(msg.payload) : msg.payload;
            setCameraActivity(parsed);
          } else if (topic === "stats" && msg.payload) {
            const parsedStats: NinjaStatsResponse =
              typeof msg.payload === "string" ? JSON.parse(msg.payload) : msg.payload;
            if (
              parsedStats &&
              typeof parsedStats === "object" &&
              typeof parsedStats.cameras === "object"
            ) {
              appendStatsSample(parsedStats);
            }
          } else if (
            (topic === "profile/state" || topic === "profile") &&
            typeof msg.payload === "string"
          ) {
            const p = msg.payload.trim().toLowerCase();
            if (p === "armed" || p === "home" || p === "disarmed") {
              setSecurityProfile(p);
            }
          } else if (topic === "config") {
            fetch("/api/config", { credentials: "include" })
              .then((r) => (r.ok ? r.json() : null))
              .then((cfgJson: NinjaConfigResponse | null) => {
                if (!closed && cfgJson && typeof cfgJson === "object") {
                  setConfig(cfgJson);
                  workbenchSession.hydrateFromServer(cfgJson);
                }
              })
              .catch(() => {});
          } else if (topic === "events" && msg.payload) {
            const parsed =
              typeof msg.payload === "string" ? JSON.parse(msg.payload) : msg.payload;
            if (
              (parsed?.type === "new" ||
                parsed?.type === "update" ||
                parsed?.type === "end") &&
              typeof parsed?.after?.id === "string" &&
              typeof parsed?.after?.camera === "string"
            ) {
              const evItem: EventItem = parsed.after;
              setEvents((prev) => upsertBoundedEventList(prev, evItem));
              setSelectedEvent((curr) =>
                curr && curr.id === evItem.id ? { ...curr, ...evItem } : curr
              );
            }
          } else if (topic === "reviews" && msg.payload) {
            const parsed =
              typeof msg.payload === "string" ? JSON.parse(msg.payload) : msg.payload;
            if (
              (parsed?.type === "new" ||
                parsed?.type === "update" ||
                parsed?.type === "end") &&
              typeof parsed?.after?.id === "string" &&
              typeof parsed?.after?.camera === "string"
            ) {
              const revItem: ReviewItem = parsed.after;
              setReviews((prev) => upsertBoundedReviewList(prev, revItem));
              setActiveReview((curr) =>
                curr && curr.id === revItem.id
                  ? {
                      ...curr,
                      ...revItem,
                      data: revItem.data
                        ? { ...(curr.data || {}), ...revItem.data }
                        : curr.data,
                    }
                  : curr
              );

              if (parsed.type === "new" && revItem.severity === "alert") {
                if (useNvrStore.getState().audibleAlertChime) {
                  playTacticalAlertChime();
                }
                if (
                  useNvrStore.getState().browserNotifications &&
                  "Notification" in window &&
                  Notification.permission === "granted"
                ) {
                  const rawObjs = revItem.data?.objects;
                  const objList =
                    Array.isArray(rawObjs) && rawObjs.length > 0 ? rawObjs : ["person"];
                  const objs = objList.join(", ").toUpperCase();
                  const rawCam = (revItem.camera || "camera").trim();
                  const safeCam =
                    !rawCam || rawCam === "." || rawCam === ".." || rawCam.includes("..")
                      ? "camera"
                      : rawCam;
                  const n = new Notification(
                    `NinjaNVR Alert // ${safeCam.toUpperCase()}`,
                    {
                      body: `Detected ${objs}`,
                      icon: `/api/${encodeURIComponent(safeCam)}/latest.jpg`,
                    }
                  );
                  n.onclick = () => {
                    window.focus();
                    setActiveReview(revItem);
                    switchTab("review");
                  };
                }
              }
            }
          } else if (topic === "exports" && msg.payload) {
            const parsed =
              typeof msg.payload === "string"
                ? JSON.parse(msg.payload)
                : msg.payload;
            const job = parseExportJobPayload(parsed);
            if (job) {
              setExportJobs((prev) => reconcileExportJobList(prev, job));
              const prevSeen = notifiedJobTerminalStatusRef.current.get(job.id);
              if (job.status === "completed" && prevSeen !== "completed") {
                notifiedJobTerminalStatusRef.current.set(job.id, "completed");
                if (notifiedJobTerminalStatusRef.current.size > 128) {
                  const firstKey = notifiedJobTerminalStatusRef.current
                    .keys()
                    .next().value;
                  if (firstKey) {
                    notifiedJobTerminalStatusRef.current.delete(firstKey);
                  }
                }
                notify(t.common.exportReadyNamedToast.replace("{name}", job.name));
                fetch("/api/exports", { credentials: "include" })
                  .then((r) => (r.ok ? r.json() : null))
                  .then((arr: ExportItem[] | null) => {
                    if (Array.isArray(arr)) {
                      setExportsList(arr);
                    }
                  })
                  .catch(() => {});
              } else if (job.status === "failed" && prevSeen !== "failed") {
                notifiedJobTerminalStatusRef.current.set(job.id, "failed");
                notify(
                  job.error ||
                    t.common.exportFailedNamedToast.replace("{name}", job.name)
                );
              }
            }
          }
        } catch {
          // ignore malformed ws frame
        }
      };
    };

    connect();

    const onBrowserOnline = () => {
      if (closed) return;
      void fetchCoreDataRef.current();
      if (!ws || ws.readyState === WebSocket.CLOSED || ws.readyState === WebSocket.CLOSING) {
        connect();
      }
    };
    const onBrowserOffline = () => {
      if (!closed) {
        setWsOnline(false);
      }
    };
    window.addEventListener("online", onBrowserOnline);
    window.addEventListener("offline", onBrowserOffline);

    return () => {
      closed = true;
      window.removeEventListener("online", onBrowserOnline);
      window.removeEventListener("offline", onBrowserOffline);
      if (reconnectTimer !== null) {
        window.clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      disposeSocket(ws);
      ws = null;
    };
  }, [
    authChecked,
    authNeeded,
    appendStatsSample,
    notify,
    setCameraActivity,
    setConfig,
    setSecurityProfile,
    setWsOnline,
    switchTab,
    t.common.exportFailedNamedToast,
    t.common.exportReadyNamedToast,
    workbenchSession,
  ]);

  // Fetch 24h recordings for the active timeline camera
  const timelineCam =
    activeTab === "review" && activeReview
      ? activeReview.camera
      : activeTab === "exports"
      ? exportCam
      : selectedCamera;

  useEffect(() => {
    if (!authChecked || authNeeded || !timelineCam) return;
    let cancelled = false;
    const now = Math.floor(Date.now() / 1000);
    const afterBucket = Math.floor((now - 86400) / 15) * 15;
    const beforeBucket = Math.ceil((now + 75) / 15) * 15;
    archiveMetadataCache
      .getRecordings(
        timelineCam,
        { after: afterBucket, before: beforeBucket },
        { configuredCameras: config?.cameras }
      )
      .then((segs) => {
        if (!cancelled && Array.isArray(segs)) setFocusSegments(segs);
      })
      .catch(() => {});
    archiveMetadataCache
      .getSummary(timelineCam, { configuredCameras: config?.cameras })
      .then((days) => {
        if (!cancelled && Array.isArray(days)) setGlobalDaysSummary(days);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [
    authChecked,
    authNeeded,
    timelineCam,
    config?.cameras,
    archiveMetadataCache,
  ]);

  // Fetch review feed on tab / date filter / auth / sub-label transitions
  useEffect(() => {
    if (!authChecked || authNeeded || activeTab !== "review") return;
    let cancelled = false;
    fetch(buildReviewFeedUrl(reviewDateFilter), { credentials: "include" })
      .then(async (r) => {
        if (!r.ok) {
          if (r.status !== 401 && !cancelled) {
            setCoreDataError(`HTTP ${r.status} (/api/review)`);
          }
          return null;
        }
        return r.json();
      })
      .then((revJson: ReviewItem[] | null) => {
        if (!cancelled && Array.isArray(revJson)) {
          setCoreDataError(null);
          setReviews(revJson.slice(0, MAX_LIVE_REVIEWS));
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setCoreDataError(err instanceof Error ? err.message : "Network error (/api/review)");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [
    authChecked,
    authNeeded,
    activeTab,
    reviewDateFilter,
    subLabelsRefreshTick,
    setCoreDataError,
  ]);

  // Tab-specific data loaders (decoupled from stats.service.last_updated so editing Settings never re-fetches /api/config/raw & /api/ffprobe!)
  const [exploreRetryTick, setExploreRetryTick] = useState(0);
  useEffect(() => {
    if (!authChecked || authNeeded) return;
    let cancelled = false;
    if (activeTab === "explore") {
      const evUrl = buildEventsFeedUrl("");
      fetch(evUrl, { credentials: "include" })
        .then(async (r) => {
          if (!r.ok) {
            if (r.status !== 401 && !cancelled) {
              setCoreDataError(`HTTP ${r.status} (/api/events)`);
            }
            return null;
          }
          return r.json();
        })
        .then((arr: EventItem[] | null) => {
          if (!cancelled && Array.isArray(arr)) {
            setCoreDataError(null);
            setEvents(arr.slice(0, MAX_LIVE_EVENTS));
            if (arr.length > 0) {
              setSelectedEvent((curr) => curr ?? arr[0]);
            }
          }
        })
        .catch((err) => {
          if (!cancelled) {
            setCoreDataError(
              err instanceof Error ? err.message : "Network error (/api/events)"
            );
          }
        });
    } else if (activeTab === "exports") {
      Promise.all([
        fetch("/api/exports", { credentials: "include" }).then((r) =>
          r.ok ? r.json() : []
        ),
        fetch("/api/jobs/export", { credentials: "include" }).then((r) =>
          r.ok ? r.json() : []
        ),
      ])
        .then(([arr, jobsArr]) => {
          if (cancelled) return;
          if (Array.isArray(arr)) {
            setExportsList(arr);
          }
          if (Array.isArray(jobsArr)) {
            setExportJobs(
              jobsArr
                .map(parseExportJobPayload)
                .filter((j): j is ExportJobItem => j !== null)
            );
          }
        })
        .catch(() => {});
    } else if (activeTab === "zones" && userRole === "admin") {
      fetch("/api/config/raw", { credentials: "include" })
        .then(async (r) => {
          if (!r.ok) return "";
          const text = await r.text();
          try {
            const parsed = JSON.parse(text);
            return typeof parsed === "string" ? parsed : text;
          } catch {
            return text;
          }
        })
        .then((txt) => {
          if (!cancelled && txt) {
            workbenchSession.hydrateRawYamlFromServer(txt);
          }
        })
        .catch(() => {});
      if (inspectorCam) {
        fetch(`/api/ffprobe?paths=camera:${encodeURIComponent(inspectorCam)}`, {
          credentials: "include",
        })
          .then((r) => (r.ok ? r.json() : null))
          .then((info) => {
            if (!cancelled && Array.isArray(info)) setFfprobeInfo(info);
          })
          .catch(() => {});
      }
    }
    return () => {
      cancelled = true;
    };
  }, [
    authChecked,
    authNeeded,
    activeTab,
    userRole,
    inspectorCam,
    subLabelsRefreshTick,
    exploreRetryTick,
    setCoreDataError,
    workbenchSession,
  ]);

  // System logs refresh only while viewing System tab
  useEffect(() => {
    if (!authChecked || authNeeded || activeTab !== "system" || userRole !== "admin" || logPaused) {
      return;
    }
    let cancelled = false;
    fetch(`/api/logs/${encodeURIComponent(logService)}?start=-120`, {
      credentials: "include",
    })
      .then((r) => (r.ok ? r.json() : { lines: [] }))
      .then((d) => {
        if (!cancelled && Array.isArray(d.lines)) setLogLines(d.lines);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [
    authChecked,
    authNeeded,
    activeTab,
    userRole,
    logService,
    logPaused,
    stats?.service?.last_updated,
  ]);

  useEffect(() => {
    if (!lastDeletedFaceProfile?.name) return;
    let cancelled = false;
    setEvents(
      (prev) =>
        reconcileDeletedFaceProfile({ events: prev }, lastDeletedFaceProfile.name)
          .events
    );
    setSelectedEvent(
      (curr) =>
        reconcileDeletedFaceProfile(
          { selectedEvent: curr },
          lastDeletedFaceProfile.name
        ).selectedEvent
    );
    setReviews(
      (prev) =>
        reconcileDeletedFaceProfile({ reviews: prev }, lastDeletedFaceProfile.name)
          .reviews
    );
    setActiveReview(
      (curr) =>
        reconcileDeletedFaceProfile(
          { activeReview: curr },
          lastDeletedFaceProfile.name
        ).activeReview
    );
    fetch(buildEventsFeedUrl(reviewDateFilter), { credentials: "include" })
      .then((r) => (r.ok ? r.json() : null))
      .then((arr: EventItem[] | null) => {
        if (!cancelled && Array.isArray(arr)) {
          setEvents(arr.slice(0, MAX_LIVE_EVENTS));
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [lastDeletedFaceProfile, reviewDateFilter]);

  // Actions
  const handleLogin = async (e: React.FormEvent) => {
    e.preventDefault();
    setLoginBusy(true);
    setLoginError(null);
    try {
      const res = await fetch("/api/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ user: loginUser, password: loginPass }),
      });
      if (res.ok) {
        manualLogoutRef.current = false;
        setUsername(loginUser.trim() || "admin");
        setAuthNeeded(false);
        setLoginPass("");
        void fetchCoreData();
      } else {
        setLoginError(t.auth.invalidCredentials);
      }
    } catch {
      setLoginError(t.auth.invalidCredentials);
    } finally {
      setLoginBusy(false);
    }
  };

  const handleReconcileReviews = useCallback(
    (reviewIds: string | ReadonlyArray<string>, patch: ReviewMutationPatch) => {
      setReviews(
        (prev) =>
          reconcileReviewMutation({ reviews: prev, activeReview: null }, reviewIds, patch)
            .reviews
      );
      setActiveReview(
        (curr) =>
          reconcileReviewMutation({ reviews: [], activeReview: curr }, reviewIds, patch)
            .activeReview
      );
    },
    []
  );

  const handleReconcileEvents = useCallback(
    (eventIds: string | ReadonlyArray<string>, patch: EventMutationPatch) => {
      setEvents(
        (prev) =>
          reconcileEventMutation({ events: prev, selectedEvent: null }, eventIds, patch)
            .events
      );
      setSelectedEvent(
        (curr) =>
          reconcileEventMutation({ events: [], selectedEvent: curr }, eventIds, patch)
            .selectedEvent
      );
    },
    []
  );

  const handleMarkReviews = useCallback(
    async (ids: string[], reviewed = true) => {
      if (ids.length === 0) return;
      try {
        await fetch("/api/reviews/viewed", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ ids, reviewed }),
        });
        handleReconcileReviews(ids, { hasBeenReviewed: reviewed });
        notify(t.common.markedReviewedToast.replace("{count}", String(ids.length)));
      } catch {
        notify(t.common.markReviewedErrorToast);
      }
    },
    [notify, handleReconcileReviews, t.common.markedReviewedToast, t.common.markReviewedErrorToast]
  );

  const handleDeleteReviews = useCallback(
    async (ids: string[]) => {
      if (ids.length === 0) return;
      try {
        await fetch("/api/reviews/delete", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ ids }),
        });
        setReviews((prev) => prev.filter((r) => !ids.includes(r.id)));
        setActiveReview((curr) => (curr && ids.includes(curr.id) ? null : curr));
        notify(t.common.deletedReviewsToast.replace("{count}", String(ids.length)));
      } catch {
        notify(t.common.deleteReviewsErrorToast);
      }
    },
    [notify, t.common.deletedReviewsToast, t.common.deleteReviewsErrorToast]
  );

  const handleQuickExport = useCallback(
    async (
      camera: string,
      startSec: number,
      endSec: number,
      label?: string,
      includeAudio: boolean = true
    ): Promise<{ exportId?: string; videoUrl?: string; queued?: boolean } | void> => {
      const realCam = resolveRealCameraSource(
        camera,
        useNvrStore.getState().config?.cameras
      );
      if (!realCam) return;
      const nowFallback = Math.floor(Date.now() / 1000);
      const s =
        typeof startSec === "number" && Number.isFinite(startSec) && startSec > 0
          ? Math.floor(startSec)
          : nowFallback - 60;
      const rawEnd =
        typeof endSec === "number" && Number.isFinite(endSec)
          ? Math.ceil(Math.max(endSec, s + 1))
          : s + 30;
      const e = s < nowFallback ? Math.max(s + 1, Math.min(nowFallback, rawEnd)) : rawEnd;
      const rangeStr = `${formatTs(s)} - ${formatTs(e)}`;
      try {
        const res = await fetch(`/api/export/${encodeURIComponent(realCam)}/start/${s}/end/${e}`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({
            name: label || `${camera} ${formatTs(s)}-${formatTs(e)}`,
            include_audio: includeAudio,
            async: true,
          }),
        });
        const data = await res.json().catch(() => ({}));
        if (res.ok && data.success !== false) {
          if (data.queued) {
            const job = parseExportJobPayload(data.job);
            if (job) {
              setExportJobs((prev) => reconcileExportJobList(prev, job));
            }
            notify(
              t.common.exportBackgroundToast
                .replace("{camera}", camera)
                .replace("{range}", rangeStr)
            );
            return {
              exportId: data.export_id,
              videoUrl: sanitizeExportVideoUrl(data.video_url, realCam, s, e),
              queued: true,
            };
          }
          notify(
            t.common.exportReadyToast
              .replace("{camera}", camera)
              .replace("{range}", rangeStr)
          );
          const listRes = await fetch("/api/exports", { credentials: "include" }).catch(
            () => null
          );
          if (listRes && listRes.ok) {
            const arr: ExportItem[] = await listRes.json().catch(() => []);
            if (Array.isArray(arr)) {
              setExportsList(arr);
              const created = arr.find((x) => x.id === data.export_id);
              if (created) {
                setActiveExportPreview(created);
              }
            }
          }
          return {
            exportId: data.export_id,
            videoUrl: sanitizeExportVideoUrl(data.video_url, realCam, s, e),
            queued: false,
          };
        } else {
          notify(data.message || `Export failed (${res.status})`);
        }
      } catch {
        notify(t.common.exportNetworkErrorToast);
      }
    },
    [
      notify,
      t.common.exportBackgroundToast,
      t.common.exportReadyToast,
      t.common.exportNetworkErrorToast,
    ]
  );

  const handleCancelExportJob = useCallback(
    async (jobId: string) => {
      try {
        const res = await fetch(`/api/jobs/export/${encodeURIComponent(jobId)}/cancel`, {
          method: "POST",
          credentials: "include",
        });
        if (res.ok) {
          const data = await res.json().catch(() => ({}));
          const job = parseExportJobPayload(data?.job);
          if (job) {
            setExportJobs((prev) => reconcileExportJobList(prev, job));
          } else {
            setExportJobs((prev) =>
              prev.map((j) =>
                j.id === jobId ? { ...j, status: "cancelled" } : j
              )
            );
          }
          notify(t.common.exportCancelledToast);
        } else {
          notify(t.common.exportCancelFailedToast);
        }
      } catch {
        notify(t.common.exportCancelNetworkErrorToast);
      }
    },
    [
      notify,
      t.common.exportCancelledToast,
      t.common.exportCancelFailedToast,
      t.common.exportCancelNetworkErrorToast,
    ]
  );

  const handleEventRetain = async (ev: EventItem) => {
    const next = !ev.retain_indefinitely;
    try {
      await fetch(`/api/events/${encodeURIComponent(ev.id)}/retain`, {
        method: next ? "POST" : "DELETE",
        credentials: "include",
      });
      handleReconcileEvents(ev.id, { retainIndefinitely: next });
      notify(next ? t.common.eventRetainedToast : t.common.eventUnretainedToast);
    } catch {
      notify(t.common.eventRetainErrorToast);
    }
  };

  const handleEventDelete = async (id: string) => {
    try {
      await fetch(`/api/events/${encodeURIComponent(id)}`, {
        method: "DELETE",
        credentials: "include",
      });
      setEvents((prev) => prev.filter((x) => x.id !== id));
      if (selectedEvent?.id === id) setSelectedEvent(null);
      notify(t.common.eventDeletedToast);
    } catch {
      notify(t.common.eventDeleteErrorToast);
    }
  };

  const handleSaveConfig = async (saveOption: "saveonly" | "restart") => {
    const res = await workbenchSession.commitRaw({
      restart: saveOption === "restart",
    });
    notify(res.message);
    if (res.ok) {
      await fetchCoreData();
    }
  };

  // Filtered Review items
  const filteredReviews = useMemo(() => {
    const dateBounds = reviewDateFilter
      ? parseOperationalDateBoundsSec(reviewDateFilter)
      : null;
    return reviews.filter((r) => {
      if (!matchesOperationalDateBounds(r, dateBounds)) return false;
      if (reviewCameraFilter !== "all" && r.camera !== reviewCameraFilter) return false;
      if (reviewSeverityFilter !== "all") {
        if (reviewSeverityFilter === "audio") {
          const hasAudio =
            (r.data?.audio && r.data.audio.length > 0) ||
            (r.data?.objects || []).some((o) => o.toLowerCase() === "audio");
          if (!hasAudio) return false;
        } else if (r.severity !== reviewSeverityFilter) {
          return false;
        }
      }
      if (
        reviewObjectFilter !== "all" &&
        !(r.data?.objects || []).some((o) => o.toLowerCase() === reviewObjectFilter)
      ) {
        return false;
      }
      if (reviewStatusFilter === "unreviewed" && r.has_been_reviewed) return false;
      if (reviewStatusFilter === "reviewed" && !r.has_been_reviewed) return false;
      return true;
    });
  }, [
    reviews,
    reviewDateFilter,
    reviewCameraFilter,
    reviewSeverityFilter,
    reviewObjectFilter,
    reviewStatusFilter,
  ]);

  // Filtered Explore items (ExploreWorkspace manages its own independent exploreDateFilter)
  const filteredEvents = useMemo(() => {
    const q = exploreQuery.trim().toLowerCase();
    return events.filter((ev) => {
      if (exploreCamera !== "all" && ev.camera !== exploreCamera) return false;
      if (exploreLabel !== "all" && ev.label !== exploreLabel) return false;
      if (exploreRetainedOnly && !ev.retain_indefinitely) return false;
      if (q) {
        const attrs = Array.isArray(ev.data?.attributes) ? ev.data.attributes.join(" ") : "";
        const notes = ev.data?.notes || "";
        const hay = `${ev.id} ${ev.camera} ${ev.label} ${ev.sub_label || ""} ${(
          ev.zones || []
        ).join(" ")} ${attrs} ${notes}`.toLowerCase();
        if (!hay.includes(q)) return false;
      }
      return true;
    });
  }, [
    events,
    exploreCamera,
    exploreLabel,
    exploreRetainedOnly,
    exploreQuery,
  ]);

  // Global keyboard shortcuts (1-7 tabs; Review [J/K/R/E] is owned directly by ReviewWorkspace)
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const tag = target?.tagName?.toLowerCase();
      if (
        tag === "input" ||
        tag === "textarea" ||
        tag === "select" ||
        target?.isContentEditable
      ) {
        return;
      }
      if (e.ctrlKey || e.metaKey || e.altKey) return;

      if (e.key === "1") switchTab("live");
      else if (e.key === "2") switchTab("sync");
      else if (e.key === "3") switchTab("review");
      else if (e.key === "4") switchTab("explore");
      else if (e.key === "5") switchTab("exports");
      else if (e.key === "6" && userRole === "admin") switchTab("zones");
      else if (e.key === "7" && userRole === "admin") switchTab("system");
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [switchTab, userRole]);

  // Telemetry summary values
  const sysCpu = formatMeasured(parseMeasured(stats?.cpu_usages?.["ninjanvr.full_system"]?.cpu), 1, "%");
  const sysMem = formatMeasured(parseMeasured(stats?.cpu_usages?.["ninjanvr.full_system"]?.mem), 1, "%");
  const inferMs = formatMeasured(resolvePrimaryInferenceMs(stats), 1, "ms");
  const totalFps = formatMeasured(parseMeasured(stats?.camera_fps), 1, " FPS");
  const displayVersion = formatDisplayVersion(stats?.service?.version, t.brand.release);
  const headerTelemetry = resolveHeaderTelemetryMeta(wsOnline, cameraNames.length);
  const securityProfileLabel = (prof: "armed" | "home" | "disarmed"): string =>
    prof === "armed"
      ? t.security.armed
      : prof === "home"
      ? t.security.home
      : t.security.disarmed;

  if (!authChecked) {
    return <div className="nvr-shell" />;
  }

  if (authNeeded) {
    return (
      <div
        className="nvr-shell"
        style={{ alignItems: "center", justifyContent: "center", padding: 16 }}
      >
        <form
          onSubmit={handleLogin}
          style={{
            width: "100%",
            maxWidth: 380,
            background: "var(--bg-panel)",
            border: "1px solid var(--border-strong)",
            borderRadius: 4,
            padding: 24,
            display: "flex",
            flexDirection: "column",
            gap: 14,
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
            <img
              className="nvr-brand-logo"
              src="/favicon.svg?v=ninjanvr"
              alt=""
              width={28}
              height={28}
              style={{ width: 28, height: 28 }}
            />
            <div style={{ display: "flex", flexDirection: "column", gap: 3 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 7 }}>
                <span className="nvr-brand-title">{t.brand.name}</span>
                <span className="nvr-brand-version mono">{displayVersion}</span>
              </div>
              <div className="nvr-brand-sub">{t.auth.title}</div>
            </div>
          </div>
          <div style={{ color: "var(--text-secondary)", fontSize: 12 }}>{t.auth.subtitle}</div>
          {loginError && (
            <div
              role="alert"
              style={{
                padding: "8px 10px",
                background: "rgba(239, 68, 68, 0.14)",
                border: "1px solid rgba(239, 68, 68, 0.5)",
                color: "var(--signal-alert-text)",
                fontSize: 12,
                borderRadius: 3,
              }}
            >
              {loginError}
            </div>
          )}
          <div style={{ display: "flex", flexDirection: "column", gap: 5 }}>
            <label
              htmlFor="nvr-username"
              className="mono"
              style={{ fontSize: 11, color: "var(--text-secondary)" }}
            >
              {t.auth.username}
            </label>
            <input
              id="nvr-username"
              name="username"
              className="nvr-input"
              value={loginUser}
              onChange={(e) => setLoginUser(e.target.value)}
              autoComplete="username"
              required
            />
          </div>
          <div style={{ display: "flex", flexDirection: "column", gap: 5 }}>
            <label
              htmlFor="nvr-password"
              className="mono"
              style={{ fontSize: 11, color: "var(--text-secondary)" }}
            >
              {t.auth.password}
            </label>
            <input
              id="nvr-password"
              name="password"
              type="password"
              className="nvr-input"
              value={loginPass}
              onChange={(e) => setLoginPass(e.target.value)}
              autoComplete="current-password"
              required
            />
          </div>
          <button
            type="submit"
            className="nvr-btn primary"
            style={{ minHeight: 34 }}
            disabled={loginBusy || !authProviders.local_login_enabled}
          >
            {loginBusy ? t.auth.signingIn : t.auth.signIn}
          </button>
          {authProviders.oidc?.enabled && (
            <a
              href="/api/auth/oidc/login"
              className="nvr-btn mono"
              style={{
                minHeight: 34,
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                textDecoration: "none",
                border: "1px solid var(--signal-accent)",
                color: "var(--signal-accent-text)",
              }}
            >
              {t.auth.ssoSignIn.replace(
                "{provider}",
                authProviders.oidc.provider_name || "SSO"
              )}
            </a>
          )}
          <div style={{ display: "flex", justifyContent: "flex-end", paddingTop: 6 }}>
            <span className="mono" style={{ color: "var(--text-muted)", fontSize: 11 }}>
              {t.brand.engine}
            </span>
          </div>
        </form>
      </div>
    );
  }

  const applySecurityProfile = async (prof: "armed" | "home" | "disarmed") => {
    setSecurityProfile(prof);
    try {
      await fetch("/api/profile", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ profile: prof }),
      });
      notify(
        t.security.switchedToast.replace("{profile}", securityProfileLabel(prof))
      );
    } catch {
      // ignore
    }
  };

  const handleLogout = async () => {
    manualLogoutRef.current = true;
    coreFetchSeqRef.current += 1;
    try {
      await fetch("/api/logout", {
        method: "POST",
        credentials: "include",
      });
    } catch {
      // ignore
    }
    coreFetchSeqRef.current += 1;
    setConfig(null);
    setStats(null);
    setStatsHistory([]);
    setCameraActivity({});
    setCoreDataError(null);
    setFocusSegments([]);
    setGlobalDaysSummary([]);
    archiveMetadataCache.clear();
    invalidateCameraHardwareCache();
    setFfprobeInfo(null);
    setDraftPoints([]);
    workbenchSession.hydrateFromServer(null, "");
    setEvents([]);
    setReviews([]);
    setExportsList([]);
    setLogLines([]);
    setActiveReview(null);
    setSelectedEvent(null);
    setActiveExportPreview(null);
    setUserRole("viewer");
    setWsOnline(false);
    if (activeTab === "zones" || activeTab === "system") {
      switchTab("live", { replace: true });
    }
    setAuthNeeded(true);
  };

  const mobileNav = resolveMobileNavTabs(userRole);
  const navLabel = (tab: WorkspaceTab): string => t.nav[tab];
  const mobileTabIcon = (tab: WorkspaceTab) =>
    tab === "live" ? (
      <LayoutGrid size={22} aria-hidden="true" />
    ) : tab === "sync" ? (
      <Clock size={22} aria-hidden="true" />
    ) : tab === "review" ? (
      <ListChecks size={22} aria-hidden="true" />
    ) : (
      <Download size={22} aria-hidden="true" />
    );

  return (
    <div className={`nvr-shell ${isCompact ? "is-compact" : ""}`}>
      <header className="nvr-header">
        <div className="nvr-brand">
          <img
            className="nvr-brand-logo"
            src="/favicon.svg?v=ninjanvr"
            alt=""
            width={24}
            height={24}
          />
          <span className="nvr-brand-title">{t.brand.name}</span>
          <span className="nvr-brand-version mono">{displayVersion}</span>
        </div>

        <nav className="nvr-nav" aria-label={t.header.primaryNavAria}>
          <button
            className={`nvr-nav-btn ${activeTab === "live" ? "active" : ""}`}
            onClick={() => switchTab("live")}
          >
            <span className="nvr-nav-label">{t.nav.live}</span>
            <span className="nvr-nav-short" aria-hidden="true">
              {t.navShort.live}
            </span>
            <span className="nvr-kbd">1</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "sync" ? "active" : ""}`}
            onClick={() => switchTab("sync")}
          >
            <span className="nvr-nav-label">{t.nav.sync}</span>
            <span className="nvr-nav-short" aria-hidden="true">
              {t.navShort.sync}
            </span>
            <span className="nvr-kbd">2</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "review" ? "active" : ""}`}
            onClick={() => switchTab("review")}
          >
            <span className="nvr-nav-label">{t.nav.review}</span>
            <span className="nvr-nav-short" aria-hidden="true">
              {t.navShort.review}
            </span>
            {unreviewedAlerts.length > 0 && (
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  padding: "0 5px",
                  background: "rgba(239, 68, 68, 0.22)",
                  border: "1px solid rgba(239, 68, 68, 0.55)",
                  color: "var(--signal-alert-text)",
                  borderRadius: 2,
                }}
              >
                {unreviewedAlerts.length}
              </span>
            )}
            <span className="nvr-kbd">3</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "explore" ? "active" : ""}`}
            onClick={() => switchTab("explore")}
          >
            <span className="nvr-nav-label">{t.nav.explore}</span>
            <span className="nvr-nav-short" aria-hidden="true">
              {t.navShort.explore}
            </span>
            <span className="nvr-kbd">4</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "exports" ? "active" : ""}`}
            onClick={() => switchTab("exports")}
          >
            <span className="nvr-nav-label">{t.nav.exports}</span>
            <span className="nvr-nav-short" aria-hidden="true">
              {t.navShort.exports}
            </span>
            <span className="nvr-kbd">5</span>
          </button>
          {userRole === "admin" && (
            <>
              <button
                className={`nvr-nav-btn ${activeTab === "zones" ? "active" : ""}`}
                onClick={() => switchTab("zones")}
              >
                <span className="nvr-nav-label">{t.nav.zones}</span>
                <span className="nvr-nav-short" aria-hidden="true">
                  {t.navShort.zones}
                </span>
                <span className="nvr-kbd">6</span>
              </button>
              <button
                className={`nvr-nav-btn ${activeTab === "system" ? "active" : ""}`}
                onClick={() => switchTab("system")}
              >
                <span className="nvr-nav-label">{t.nav.system}</span>
                <span className="nvr-nav-short" aria-hidden="true">
                  {t.navShort.system}
                </span>
                <span className="nvr-kbd">7</span>
              </button>
            </>
          )}
        </nav>

        <div className="nvr-header-right">
          {enabledPeers.length > 0 && (
            <select
              className="nvr-select sm mono"
              aria-label={t.header.nodeSelectorAria}
              value={selectedNode}
              onChange={(e) => {
                const nextNode = e.target.value;
                setSelectedNode(nextNode);
                notify(
                  t.header.nodeSwitchedToast.replace(
                    "{node}",
                    nextNode.toUpperCase()
                  )
                );
              }}
              title={t.header.nodeSelectorHint}
            >
              <option value="all">{t.header.nodeAll}</option>
              <option value="local">{t.header.nodeLocal}</option>
              {enabledPeers.map(([peerId]) => (
                <option key={peerId} value={peerId}>
                  {t.header.nodePeerPrefix}: {peerId.toUpperCase()}
                </option>
              ))}
            </select>
          )}
          <button
            type="button"
            data-testid="profile-pill"
            className={`nvr-profile-pill mono is-${securityProfile}`}
            aria-haspopup="dialog"
            aria-label={t.security.profilePillAria.replace(
              "{profile}",
              securityProfileLabel(securityProfile)
            )}
            disabled={userRole === "viewer"}
            onClick={() => setProfileSheetOpen(true)}
          >
            <span className="nvr-profile-pill-dot" aria-hidden="true" />
            {securityProfileLabel(securityProfile)}
          </button>
          <div
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 2,
              background: "var(--bg-subcard)",
              border: "1px solid var(--border-strong)",
              borderRadius: 3,
              padding: 2,
            }}
            aria-label={t.security.profileLabel}
            className="mono nvr-profile-seg"
          >
            {(["armed", "home", "disarmed"] as const).map((prof) => {
              const isAct = securityProfile === prof;
              return (
                <button
                  key={prof}
                  type="button"
                  disabled={userRole === "viewer"}
                  className={`nvr-btn sm mono ${isAct ? "active" : ""}`}
                  style={{
                    padding: "0 6px",
                    color:
                      isAct && prof === "armed"
                        ? "var(--signal-alert-text)"
                        : isAct && prof === "home"
                        ? "var(--signal-motion-text)"
                        : undefined,
                  }}
                  onClick={() => void applySecurityProfile(prof)}
                >
                  {securityProfileLabel(prof)}
                </button>
              );
            })}
          </div>
          {stats?.service?.storage_degraded && (
            <button
              className="nvr-badge alert mono"
              onClick={() => switchTab("system")}
              title={
                stats.service.storage_error || t.header.storageDegradedHint
              }
              style={{
                cursor: "pointer",
                padding: "3px 8px",
                fontSize: "var(--text-xs)",
                fontWeight: 700,
              }}
            >
              {t.header.storageDegraded}
            </button>
          )}
          <button
            className="nvr-telemetry-pill"
            onClick={() => switchTab("system")}
            title={headerTelemetry.tooltip}
          >
            <span className={`nvr-dot ${headerTelemetry.wsDotClass}`} />
            <span>{headerTelemetry.channelBadge}</span>
            <span className="nvr-divider" aria-hidden="true" />
            <span>CPU {sysCpu}</span>
            <span className="nvr-divider nvr-telemetry-ext" aria-hidden="true" />
            <span className="nvr-telemetry-ext">MEM {sysMem}</span>
            <span className="nvr-divider nvr-telemetry-perf" aria-hidden="true" />
            <span className="nvr-telemetry-perf">OV {inferMs}</span>
            <span className="nvr-divider nvr-telemetry-perf" aria-hidden="true" />
            <span className="nvr-telemetry-perf">{totalFps}</span>
          </button>
          <button
            type="button"
            data-testid="topbar-theme-toggle"
            className="nvr-btn sm mono nvr-header-aux nvr-header-btn"
            title={`${t.theme.cycleTooltip} — ${
              themeMode === "dark"
                ? t.theme.dark
                : themeMode === "light"
                ? t.theme.light
                : t.theme.system
            }${highContrast ? ` (${t.theme.highContrast})` : ""}`}
            aria-label={t.theme.cycleTooltip}
            onClick={() => {
              const nextMode = cycleThemeMode(themeMode);
              setThemeMode(nextMode);
              const nextLabel =
                nextMode === "dark"
                  ? t.theme.dark
                  : nextMode === "light"
                  ? t.theme.light
                  : t.theme.system;
              notify(`${t.theme.label}: ${nextLabel}`);
            }}
          >
            {themeMode === "dark" ? (
              <Moon size={12} />
            ) : themeMode === "light" ? (
              <Sun size={12} />
            ) : (
              <Monitor size={12} />
            )}
            <span className="nvr-header-label">
              {themeMode === "dark"
                ? t.theme.shortDark
                : themeMode === "light"
                ? t.theme.shortLight
                : t.theme.shortAuto}
            </span>
          </button>
          <button
            type="button"
            className="nvr-btn sm mono nvr-header-btn"
            title={t.auth.signedInTooltip
              .replace("{user}", username)
              .replace("{role}", userRole)}
            data-testid="topbar-logout"
            aria-label={t.auth.signOutAria}
            onClick={handleLogout}
          >
            <LogOut size={12} />
            <span className="nvr-header-label">{t.auth.signOut}</span>
          </button>
        </div>
      </header>

      {/* Global Simulated Fleet Banner (visible on both Desktop and Mobile across all workspaces) */}
      {simulatedFleetCount > 0 && (
        <div
          className="nvr-sim-banner mono"
          role="status"
          data-testid="global-sim-banner"
        >
          <span>
            {t.live.simulationBanner.replace(
              "{sim}",
              String(simulatedFleetCount)
            )}
          </span>
          <button
            type="button"
            data-testid="exit-simulation-btn"
            className="nvr-btn danger sm mono"
            onClick={() => setSimulatedFleetCount(0)}
          >
            {t.live.exitSimulation}
          </button>
        </div>
      )}

      {/* Non-intrusive retryable warning banner when /api/review or /api/events fails */}
      {coreDataError && (
        <div
          className="nvr-error-banner mono"
          role="alert"
          data-testid="core-data-error-banner"
        >
          <span className="nvr-error-banner-msg">
            <AlertTriangle size={13} aria-hidden="true" />
            <span>
              {t.common.telemetryError} ({coreDataError})
            </span>
          </span>
          <div style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
            <button
              type="button"
              className="nvr-btn sm mono"
              onClick={() => {
                setCoreDataError(null);
                void fetchCoreData();
                if (activeTab === "explore") {
                  setExploreRetryTick((n) => n + 1);
                }
              }}
            >
              <RefreshCw size={11} />
              {t.common.retry}
            </button>
            <button
              type="button"
              className="nvr-btn sm"
              aria-label={t.common.dismiss}
              title={t.common.dismiss}
              onClick={() => setCoreDataError(null)}
            >
              <X size={12} />
            </button>
          </div>
        </div>
      )}

      {toastMsg && (
        <div
          className="mono"
          role="status"
          aria-live="polite"
          style={{
            position: "fixed",
            bottom: 14,
            right: 16,
            zIndex: 9999,
            background: "var(--bg-elevated)",
            border: "1px solid var(--signal-accent)",
            color: "var(--text-primary)",
            padding: "8px 14px",
            borderRadius: 3,
            fontSize: "var(--text-sm)",
            boxShadow: "0 8px 24px rgba(0,0,0,0.65)",
          }}
        >
          {toastMsg}
        </div>
      )}

      <ConfigWorkbenchProvider session={workbenchSession}>
        <ArchiveMetadataCacheProvider cache={archiveMetadataCache}>
          <main className="nvr-main">
            {activeTab === "live" && (
              <LiveWorkspace
                cameraNames={cameraNames}
                reviews={reviews}
                unreviewedAlerts={unreviewedAlerts}
                focusSegments={focusSegments}
                handleMarkReviews={handleMarkReviews}
                handleQuickExport={handleQuickExport}
              />
            )}

            {activeTab !== "live" && (
              <Suspense
                fallback={
                  <div className="nvr-workspace-loading mono" role="status">
                    {t.common.loadingWorkspace}
                  </div>
                }
              >
                {activeTab === "sync" && (
                  <SyncWorkspace
                    cameraNames={cameraNames}
                    syncMasterTs={syncMasterTs}
                    setSyncMasterTs={setSyncMasterTs}
                    syncSpeed={syncSpeed}
                    setSyncSpeed={setSyncSpeed}
                    reviews={reviews}
                    focusSegments={focusSegments}
                    handleQuickExport={handleQuickExport}
                  />
                )}

                {activeTab === "review" && (
                  <ReviewWorkspace
                    cameraNames={cameraNames}
                    reviews={reviews}
                    filteredReviews={filteredReviews}
                    exportsList={exportsList}
                    onOpenExport={(ex) => {
                      setActiveExportPreview(ex);
                      switchTab("exports");
                    }}
                    reviewCameraFilter={reviewCameraFilter}
                    setReviewCameraFilter={setReviewCameraFilter}
                    reviewSeverityFilter={reviewSeverityFilter}
                    setReviewSeverityFilter={setReviewSeverityFilter}
                    reviewObjectFilter={reviewObjectFilter}
                    setReviewObjectFilter={setReviewObjectFilter}
                    reviewStatusFilter={reviewStatusFilter}
                    setReviewStatusFilter={setReviewStatusFilter}
                    reviewDateFilter={reviewDateFilter}
                    setReviewDateFilter={setReviewDateFilter}
                    globalDaysSummary={globalDaysSummary}
                    activeReview={activeReview}
                    setActiveReview={setActiveReview}
                    focusSegments={focusSegments}
                    handleMarkReviews={handleMarkReviews}
                    handleDeleteReviews={handleDeleteReviews}
                    handleQuickExport={handleQuickExport}
                    setSyncMasterTs={setSyncMasterTs}
                    switchTab={switchTab}
                    onReconcileReviews={handleReconcileReviews}
                  />
                )}

                {activeTab === "explore" && (
                  <ExploreWorkspace
                    cameraNames={cameraNames}
                    filteredEvents={filteredEvents}
                    exportsList={exportsList}
                    onOpenExport={(ex) => {
                      setActiveExportPreview(ex);
                      switchTab("exports");
                    }}
                    exploreQuery={exploreQuery}
                    setExploreQuery={setExploreQuery}
                    exploreCamera={exploreCamera}
                    setExploreCamera={setExploreCamera}
                    exploreLabel={exploreLabel}
                    setExploreLabel={setExploreLabel}
                    exploreRetainedOnly={exploreRetainedOnly}
                    setExploreRetainedOnly={setExploreRetainedOnly}
                    globalDaysSummary={globalDaysSummary}
                    reviewDateFilter={reviewDateFilter}
                    setReviewDateFilter={setReviewDateFilter}
                    selectedEvent={selectedEvent}
                    setSelectedEvent={setSelectedEvent}
                    eventViewMode={eventViewMode}
                    setEventViewMode={setEventViewMode}
                    handleEventRetain={handleEventRetain}
                    handleEventDelete={handleEventDelete}
                    setSyncMasterTs={setSyncMasterTs}
                    switchTab={switchTab}
                    notify={notify}
                    onReconcileEvents={handleReconcileEvents}
                  />
                )}

                {activeTab === "exports" && (
                  <ExportsWorkspace
                    cameraNames={cameraNames}
                    config={config}
                    exportsList={exportsList}
                    setExportsList={setExportsList}
                    exportJobs={exportJobs}
                    onCancelExportJob={handleCancelExportJob}
                    exportCam={exportCam}
                    setExportCam={setExportCam}
                    exportPresetMin={exportPresetMin}
                    setExportPresetMin={setExportPresetMin}
                    exportLabel={exportLabel}
                    setExportLabel={setExportLabel}
                    activeExportPreview={activeExportPreview}
                    setActiveExportPreview={setActiveExportPreview}
                    timeShiftTs={timeShiftTs}
                    setTimeShiftTs={setTimeShiftTs}
                    focusSegments={focusSegments}
                    reviews={reviews}
                    handleQuickExport={handleQuickExport}
                    notify={notify}
                  />
                )}

                {activeTab === "zones" && userRole === "admin" && (
                  <ZonesWorkspace
                    cameraNames={cameraNames}
                    config={config}
                    setConfig={setConfig}
                    zonesSubTab={zonesSubTab}
                    setZonesSubTab={setZonesSubTab}
                    inspectorCam={inspectorCam}
                    setInspectorCam={setInspectorCam}
                    rawYaml={rawYaml}
                    setRawYaml={setRawYaml}
                    draftPoints={draftPoints}
                    setDraftPoints={setDraftPoints}
                    ffprobeInfo={ffprobeInfo}
                    handleSaveConfig={handleSaveConfig}
                    notify={notify}
                    workbenchSession={workbenchSession}
                  />
                )}

                {activeTab === "system" && userRole === "admin" && (
                  <SystemWorkspace
                    cameraNames={cameraNames}
                    logService={logService}
                    setLogService={setLogService}
                    logLines={logLines}
                    logSearch={logSearch}
                    setLogSearch={setLogSearch}
                    logPaused={logPaused}
                    setLogPaused={setLogPaused}
                    notify={notify}
                  />
                )}
              </Suspense>
            )}
          </main>
        </ArchiveMetadataCacheProvider>
      </ConfigWorkbenchProvider>

      {isCompact && (
        <nav className="nvr-tabbar" aria-label={t.header.mobileNavAria}>
          {mobileNav.primary.map((tab) => {
            const badge = tab === "review" ? formatBadgeCount(unreviewedAlerts.length) : "";
            return (
              <button
                key={tab}
                type="button"
                data-testid={`tabbar-${tab}`}
                aria-label={
                  badge
                    ? `${t.navMobile[tab as keyof typeof t.navMobile]} (${badge})`
                    : t.navMobile[tab as keyof typeof t.navMobile]
                }
                className={`nvr-tabbar-btn ${activeTab === tab ? "active" : ""}`}
                aria-current={activeTab === tab ? "page" : undefined}
                onClick={() => {
                  setMobileMoreOpen(false);
                  switchTab(tab);
                }}
              >
                {mobileTabIcon(tab)}
                <span className="nvr-tabbar-label">{t.navMobile[tab as keyof typeof t.navMobile]}</span>
                {badge && <span className="nvr-tabbar-badge mono">{badge}</span>}
              </button>
            );
          })}
          <button
            type="button"
            data-testid="tabbar-more"
            aria-label={t.navMobile.more}
            className={`nvr-tabbar-btn ${
              mobileMoreOpen || mobileNav.overflow.includes(activeTab as WorkspaceTab) ? "active" : ""
            }`}
            aria-expanded={mobileMoreOpen}
            onClick={() => setMobileMoreOpen((v) => !v)}
          >
            <MoreHorizontal size={22} aria-hidden="true" />
            <span className="nvr-tabbar-label">{t.navMobile.more}</span>
          </button>
        </nav>
      )}

      {isCompact && profileSheetOpen && (
        <div className="nvr-sheet-backdrop" onClick={() => setProfileSheetOpen(false)}>
          <section
            className="nvr-sheet nvr-more-sheet"
            role="dialog"
            aria-modal="true"
            aria-label={t.security.profileLabel}
            ref={profileSheetRef}
            onClick={(e) => e.stopPropagation()}
          >
            <div className="nvr-sheet-handle" aria-hidden="true" />
            {(["armed", "home", "disarmed"] as const).map((prof) => (
              <button
                key={prof}
                type="button"
                data-testid={`profile-option-${prof}`}
                className={`nvr-sheet-link nvr-profile-option is-${prof} ${
                  securityProfile === prof ? "active" : ""
                }`}
                aria-pressed={securityProfile === prof}
                onClick={() => {
                  setProfileSheetOpen(false);
                  void applySecurityProfile(prof);
                }}
              >
                <span className="nvr-profile-pill-dot" aria-hidden="true" />
                <span className="mono">{securityProfileLabel(prof)}</span>
              </button>
            ))}
          </section>
        </div>
      )}

      {isCompact && mobileMoreOpen && (
        <div className="nvr-sheet-backdrop" onClick={() => setMobileMoreOpen(false)}>
          <section
            className="nvr-sheet nvr-more-sheet"
            role="dialog"
            aria-modal="true"
            aria-label={t.navMobile.more}
            ref={moreSheetRef}
            onClick={(e) => e.stopPropagation()}
          >
            <div className="nvr-sheet-handle" aria-hidden="true" />
            {mobileNav.overflow.map((tab) => (
              <button
                key={tab}
                type="button"
                data-testid={`more-${tab}`}
                className={`nvr-sheet-link ${activeTab === tab ? "active" : ""}`}
                onClick={() => {
                  setMobileMoreOpen(false);
                  switchTab(tab);
                }}
              >
                {navLabel(tab)}
              </button>
            ))}
            <div className="nvr-sheet-row">
              <span className="mono">{headerTelemetry.channelBadge} · CPU {sysCpu} · {totalFps}</span>
            </div>
            <button
              type="button"
              className="nvr-sheet-link"
              onClick={() => setThemeMode(cycleThemeMode(themeMode))}
            >
              {t.theme.label}:{" "}
              {themeMode === "dark"
                ? t.theme.dark
                : themeMode === "light"
                ? t.theme.light
                : t.theme.system}
            </button>
            <button
              type="button"
              className="nvr-sheet-link danger"
              onClick={() => {
                setMobileMoreOpen(false);
                void handleLogout();
              }}
            >
              <LogOut size={16} aria-hidden="true" /> {t.auth.signOut} ({username})
            </button>
          </section>
        </div>
      )}
    </div>
  );
};
