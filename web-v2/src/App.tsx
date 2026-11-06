import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { LogOut, Monitor, Moon, Sun } from "lucide-react";
import { useI18n } from "./i18n";
import {
  EventItem,
  ExportItem,
  FfprobeItem,
  NinjaConfigResponse,
  RecordingDaySummary,
  RecordingSegment,
  ReviewItem,
} from "./types/nvr";
import {
  WorkspaceTab,
  applyThemeToDocument,
  cycleThemeMode,
  formatDisplayVersion,
  formatTs,
  resolveHeaderTelemetryMeta,
} from "./utils/format";
import {
  expandSimulatedFleet,
  resolveRealCameraSource,
  resolveValidCameraSelection,
  useNvrStore,
} from "./store/useNvrStore";
import { LiveWorkspace } from "./views/LiveWorkspace";
import { SyncWorkspace } from "./views/SyncWorkspace";
import { ReviewWorkspace } from "./views/ReviewWorkspace";
import {
  ExploreWorkspace,
  clearFaceProfileFromEvents,
  clearFaceProfileFromReviews,
} from "./views/ExploreWorkspace";
import { ExportsWorkspace } from "./views/ExportsWorkspace";
import { ZonesWorkspace } from "./views/ZonesWorkspace";
import { SystemWorkspace } from "./views/SystemWorkspace";

export const MAX_LIVE_EVENTS = 250;
export const MAX_LIVE_REVIEWS = 300;

export function upsertBoundedEventList(
  prev: EventItem[],
  incoming: EventItem,
  maxItems = MAX_LIVE_EVENTS
): EventItem[] {
  const cap = Math.max(1, Math.floor(maxItems));
  const idx = prev.findIndex((x) => x.id === incoming.id);
  if (idx >= 0) {
    const copy = [...prev];
    copy[idx] = incoming;
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
  const safeCam = encodeURIComponent((fallbackCamera || "camera").trim() || "camera");
  const s = Number.isFinite(startSec) ? Math.floor(startSec) : 0;
  const e = Number.isFinite(endSec) ? Math.ceil(Math.max(endSec, s + 5)) : s + 5;
  const fallbackPath = `/exports/${safeCam}_${s}_${e}.mp4`;

  const candidate = (rawUrl || "").trim();
  if (!candidate || /[\u0000-\u0020\u007f\\]/.test(candidate)) {
    return fallbackPath;
  }
  if (
    candidate.startsWith("//") ||
    candidate.includes("..") ||
    /%2e/i.test(candidate) ||
    /%2f/i.test(candidate) ||
    /%5c/i.test(candidate) ||
    /%00/i.test(candidate)
  ) {
    return fallbackPath;
  }
  if (
    candidate.startsWith("/exports/") ||
    candidate.startsWith("/api/") ||
    candidate.startsWith("/vod/") ||
    candidate.startsWith("/clips/")
  ) {
    return candidate;
  }
  return fallbackPath;
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
  const { t } = useI18n();
  const activeTab = useNvrStore((s) => s.activeTab);
  const setActiveTab = useNvrStore((s) => s.setActiveTab);
  const securityProfile = useNvrStore((s) => s.securityProfile);
  const setSecurityProfile = useNvrStore((s) => s.setSecurityProfile);
  const audibleAlertChime = useNvrStore((s) => s.audibleAlertChime);
  const browserNotifications = useNvrStore((s) => s.browserNotifications);
  const themeMode = useNvrStore((s) => s.themeMode);
  const setThemeMode = useNvrStore((s) => s.setThemeMode);
  const highContrast = useNvrStore((s) => s.highContrast);
  const selectedCamera = useNvrStore((s) => s.selectedCamera);
  const setSelectedCamera = useNvrStore((s) => s.setSelectedCamera);
  const timeShiftTs = useNvrStore((s) => s.timeShiftTs);
  const setTimeShiftTs = useNvrStore((s) => s.setTimeShiftTs);
  const setPlaybackSpeed = useNvrStore((s) => s.setPlaybackSpeed);
  const wsOnline = useNvrStore((s) => s.wsOnline);
  const setWsOnline = useNvrStore((s) => s.setWsOnline);
  const config = useNvrStore((s) => s.config);
  const setConfig = useNvrStore((s) => s.setConfig);
  const stats = useNvrStore((s) => s.stats);
  const setStats = useNvrStore((s) => s.setStats);
  const setStatsHistory = useNvrStore((s) => s.setStatsHistory);
  const setCameraActivity = useNvrStore((s) => s.setCameraActivity);
  const lastDeletedFaceProfile = useNvrStore((s) => s.lastDeletedFaceProfile);
  const subLabelsRefreshTick = useNvrStore((s) => s.subLabelsRefreshTick);

  const [authChecked, setAuthChecked] = useState(false);
  const [authNeeded, setAuthNeeded] = useState(false);
  const manualLogoutRef = useRef(false);
  const coreFetchSeqRef = useRef(0);
  const [username, setUsername] = useState("admin");
  const [loginUser, setLoginUser] = useState("admin");
  const [userRole, setUserRole] = useState<string>("admin");
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
  const [exportCam, setExportCam] = useState<string>("escritorio");
  const [exportPresetMin, setExportPresetMin] = useState<number>(1);
  const [exportLabel, setExportLabel] = useState<string>("");
  const [activeExportPreview, setActiveExportPreview] = useState<ExportItem | null>(null);

  // Zones & Config Studio state
  const [zonesSubTab, setZonesSubTab] = useState<"visual" | "yaml">("visual");
  const [inspectorCam, setInspectorCam] = useState<string>("escritorio");
  const [rawYaml, setRawYaml] = useState<string>("");
  const [draftPoints, setDraftPoints] = useState<Array<[number, number]>>([]);
  const [ffprobeInfo, setFfprobeInfo] = useState<FfprobeItem[] | null>(null);

  // System & Logs state
  const [logService, setLogService] = useState<"ninjanvr" | "go2rtc" | "nginx">("ninjanvr");
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

  const simulatedFleetCount = useNvrStore((s) => s.simulatedFleetCount);
  const cameraNames = useMemo(() => {
    const base = !config?.cameras
      ? ["escritorio", "sala", "quarto", "cozinha"]
      : Object.keys(config.cameras).filter((k) => config.cameras[k].enabled);
    if (simulatedFleetCount > 0) {
      return expandSimulatedFleet(
        base,
        config?.cameras,
        config?.camera_groups,
        simulatedFleetCount
      ).cameraNames;
    }
    return base;
  }, [config, simulatedFleetCount]);

  const unreviewedAlerts = useMemo(
    () => reviews.filter((r) => !r.has_been_reviewed),
    [reviews]
  );

  // Navigate tab + update browser URL cleanly
  const switchTab = useCallback(
    (tab: WorkspaceTab) => {
      setActiveTab(tab);
      const urlMap: Record<WorkspaceTab, string> = {
        live: "/",
        sync: "/sync",
        review: "/review",
        explore: "/explore",
        exports: "/export",
        zones: "/config",
        system: "/system",
      };
      window.history.replaceState(null, "", urlMap[tab]);
    },
    [setActiveTab]
  );

  // Fetch core bootstrap data
  const fetchCoreData = useCallback(async () => {
    const mySeq = ++coreFetchSeqRef.current;
    try {
      let revUrl = "/api/review?limit=200";
      if (reviewDateFilter) {
        const parts = reviewDateFilter.split("-").map(Number);
        if (parts.length === 3 && parts.every((n) => Number.isFinite(n))) {
          const dayStart = Math.floor(
            new Date(parts[0], parts[1] - 1, parts[2], 0, 0, 0).getTime() / 1000
          );
          if (Number.isFinite(dayStart)) {
            const dayEnd = dayStart + 86400;
            revUrl = `/api/review?after=${dayStart}&before=${dayEnd}&limit=300`;
          }
        }
      }
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
          validCams.length > 0 && !validCams.includes(curr) ? validCams[0] : curr
        );
        setExportCam((curr) =>
          validCams.length > 0 && !validCams.includes(curr) ? validCams[0] : curr
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
            switchTab("live");
          }
        }
        if (profJson && typeof profJson.active_profile === "string") {
          const p = profJson.active_profile.trim().toLowerCase();
          if (p === "armed" || p === "home" || p === "disarmed") {
            setSecurityProfile(p);
          }
        }
      }
    } catch {
      // network retry on next tick
    }
  }, [
    reviewDateFilter,
    setConfig,
    setSelectedCamera,
    setStats,
    setStatsHistory,
    setSecurityProfile,
    switchTab,
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

  useEffect(() => {
    if (authNeeded) return;
    void fetchCoreData();
    const timer = window.setInterval(fetchCoreData, 5000);
    return () => window.clearInterval(timer);
  }, [fetchCoreData, authNeeded]);

  // Connect to /ws for real-time camera_activity and stats (with auto-reconnect on disconnect)
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
          if (msg.topic === "camera_activity" && msg.payload) {
            const parsed =
              typeof msg.payload === "string" ? JSON.parse(msg.payload) : msg.payload;
            setCameraActivity(parsed);
          } else if (msg.topic === "profile/state" && typeof msg.payload === "string") {
            const p = msg.payload.trim().toLowerCase();
            if (p === "armed" || p === "home" || p === "disarmed") {
              setSecurityProfile(p);
            }
          } else if (msg.topic === "events" && msg.payload) {
            const parsed =
              typeof msg.payload === "string" ? JSON.parse(msg.payload) : msg.payload;
            if (
              (parsed?.type === "new" || parsed?.type === "update") &&
              typeof parsed?.after?.id === "string" &&
              typeof parsed?.after?.camera === "string"
            ) {
              const evItem: EventItem = parsed.after;
              setEvents((prev) => upsertBoundedEventList(prev, evItem));
            }
          } else if (msg.topic === "reviews" && msg.payload) {
            const parsed =
              typeof msg.payload === "string" ? JSON.parse(msg.payload) : msg.payload;
            if (
              parsed?.type === "new" &&
              parsed?.after?.severity === "alert" &&
              typeof parsed?.after?.id === "string" &&
              typeof parsed?.after?.camera === "string"
            ) {
              const alertItem: ReviewItem = parsed.after;
              setReviews((prev) => prependBoundedReviewAlert(prev, alertItem));
              if (useNvrStore.getState().audibleAlertChime) {
                playTacticalAlertChime();
              }
              if (
                useNvrStore.getState().browserNotifications &&
                "Notification" in window &&
                Notification.permission === "granted"
              ) {
                const rawObjs = alertItem.data?.objects;
                const objList =
                  Array.isArray(rawObjs) && rawObjs.length > 0 ? rawObjs : ["person"];
                const objs = objList.join(", ").toUpperCase();
                const n = new Notification(
                  `NinjaNVR Alert // ${alertItem.camera.toUpperCase()}`,
                  {
                    body: `Detected ${objs}`,
                    icon: `/api/${encodeURIComponent(alertItem.camera)}/latest.jpg`,
                  }
                );
                n.onclick = () => {
                  window.focus();
                  setActiveReview(alertItem);
                  switchTab("review");
                };
              }
            }
          }
        } catch {
          // ignore
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
    setWsOnline,
    setCameraActivity,
    setSecurityProfile,
    switchTab,
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
    fetch(
      `/api/${encodeURIComponent(timelineCam)}/recordings?after=${now - 86400}&before=${now + 10}`,
      { credentials: "include" }
    )
      .then((r) => (r.ok ? r.json() : []))
      .then((segs) => {
        if (!cancelled && Array.isArray(segs)) setFocusSegments(segs);
      })
      .catch(() => {});
    fetch(`/api/${encodeURIComponent(timelineCam)}/recordings/summary`, {
      credentials: "include",
    })
      .then((r) => (r.ok ? r.json() : []))
      .then((days) => {
        if (!cancelled && Array.isArray(days)) setGlobalDaysSummary(days);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [authChecked, authNeeded, timelineCam, stats?.service?.last_updated]);

  // Tab-specific data loaders
  useEffect(() => {
    if (!authChecked || authNeeded) return;
    let cancelled = false;
    if (activeTab === "explore") {
      let evUrl = "/api/events?limit=120";
      if (reviewDateFilter) {
        const parts = reviewDateFilter.split("-").map(Number);
        if (parts.length === 3 && parts.every((n) => Number.isFinite(n))) {
          const dayStart = Math.floor(
            new Date(parts[0], parts[1] - 1, parts[2], 0, 0, 0).getTime() / 1000
          );
          if (Number.isFinite(dayStart)) {
            const dayEnd = dayStart + 86400;
            evUrl = `/api/events?after=${dayStart}&before=${dayEnd}&limit=250`;
          }
        }
      }
      fetch(evUrl, { credentials: "include" })
        .then((r) => (r.ok ? r.json() : []))
        .then((arr: EventItem[]) => {
          if (!cancelled && Array.isArray(arr)) {
            setEvents(arr.slice(0, MAX_LIVE_EVENTS));
            if (arr.length > 0) {
              setSelectedEvent((curr) => curr ?? arr[0]);
            }
          }
        })
        .catch(() => {});
    } else if (activeTab === "exports") {
      fetch("/api/exports", { credentials: "include" })
        .then((r) => (r.ok ? r.json() : []))
        .then((arr: ExportItem[]) => {
          if (!cancelled && Array.isArray(arr)) {
            setExportsList(arr);
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
            setRawYaml((curr) => curr || txt);
          }
        })
        .catch(() => {});
      fetch(`/api/ffprobe?paths=camera:${encodeURIComponent(inspectorCam)}`, {
        credentials: "include",
      })
        .then((r) => (r.ok ? r.json() : null))
        .then((info) => {
          if (!cancelled && Array.isArray(info)) setFfprobeInfo(info);
        })
        .catch(() => {});
    } else if (activeTab === "system" && userRole === "admin" && !logPaused) {
      fetch(`/api/logs/${encodeURIComponent(logService)}?start=-120`, {
        credentials: "include",
      })
        .then((r) => (r.ok ? r.json() : { lines: [] }))
        .then((d) => {
          if (!cancelled && Array.isArray(d.lines)) setLogLines(d.lines);
        })
        .catch(() => {});
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
    logService,
    logPaused,
    reviewDateFilter,
    subLabelsRefreshTick,
    stats?.service?.last_updated,
  ]);

  useEffect(() => {
    if (!lastDeletedFaceProfile?.name) return;
    let cancelled = false;
    setEvents((prev) => clearFaceProfileFromEvents(prev, lastDeletedFaceProfile.name));
    setSelectedEvent((curr) =>
      curr ? clearFaceProfileFromEvents([curr], lastDeletedFaceProfile.name)[0] : null
    );
    setReviews((prev) => clearFaceProfileFromReviews(prev, lastDeletedFaceProfile.name));
    setActiveReview((curr) =>
      curr ? clearFaceProfileFromReviews([curr], lastDeletedFaceProfile.name)[0] : null
    );
    fetch("/api/events?limit=120", { credentials: "include" })
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
  }, [lastDeletedFaceProfile]);

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
        fetchCoreData();
      } else {
        setLoginError(t.auth.invalidCredentials);
      }
    } catch {
      setLoginError(t.auth.invalidCredentials);
    } finally {
      setLoginBusy(false);
    }
  };

  const handleMarkReviews = useCallback(
    async (ids: string[], reviewed = true) => {
      if (ids.length === 0) return;
      await fetch("/api/reviews/viewed", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ ids, reviewed }),
      });
      setReviews((prev) =>
        prev.map((r) => (ids.includes(r.id) ? { ...r, has_been_reviewed: reviewed } : r))
      );
      notify(`Marked ${ids.length} review item(s) as reviewed`);
    },
    [notify]
  );

  const handleDeleteReviews = useCallback(
    async (ids: string[]) => {
      if (ids.length === 0) return;
      await fetch("/api/reviews/delete", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ ids }),
      });
      setReviews((prev) => prev.filter((r) => !ids.includes(r.id)));
      setActiveReview((curr) => (curr && ids.includes(curr.id) ? null : curr));
      notify(`Deleted ${ids.length} review segment(s)`);
    },
    [notify]
  );

  const handleQuickExport = useCallback(
    async (
      camera: string,
      startSec: number,
      endSec: number,
      label?: string,
      includeAudio: boolean = true
    ): Promise<{ exportId?: string; videoUrl?: string } | void> => {
      const nowFallback = Math.floor(Date.now() / 1000);
      const s =
        typeof startSec === "number" && Number.isFinite(startSec) && startSec > 0
          ? Math.floor(startSec)
          : nowFallback - 60;
      const e =
        typeof endSec === "number" && Number.isFinite(endSec)
          ? Math.ceil(Math.max(endSec, s + 5))
          : s + 30;
      const res = await fetch(`/api/export/${encodeURIComponent(camera)}/start/${s}/end/${e}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          name: label || `${camera} ${formatTs(s)}-${formatTs(e)}`,
          include_audio: includeAudio,
        }),
      });
      const data = await res.json().catch(() => ({}));
      if (res.ok && data.success !== false) {
        notify(`MP4 clip ready for ${camera} (${formatTs(s)} - ${formatTs(e)})`);
        const listRes = await fetch("/api/exports", { credentials: "include" });
        if (listRes.ok) {
          const arr: ExportItem[] = await listRes.json();
          setExportsList(arr);
          const created = arr.find((x) => x.id === data.export_id);
          if (created) {
            setActiveExportPreview(created);
          }
        }
        return {
          exportId: data.export_id,
          videoUrl: sanitizeExportVideoUrl(data.video_url, camera, s, e),
        };
      } else {
        notify(data.message || `Export failed (${res.status})`);
      }
    },
    [notify]
  );

  const handleEventRetain = async (ev: EventItem) => {
    const next = !ev.retain_indefinitely;
    await fetch(`/api/events/${encodeURIComponent(ev.id)}/retain`, {
      method: next ? "POST" : "DELETE",
      credentials: "include",
    });
    setEvents((prev) =>
      prev.map((x) => (x.id === ev.id ? { ...x, retain_indefinitely: next } : x))
    );
    setSelectedEvent((curr) =>
      curr && curr.id === ev.id ? { ...curr, retain_indefinitely: next } : curr
    );
    notify(next ? "Event retained indefinitely" : "Event retention lock removed");
  };

  const handleEventDelete = async (id: string) => {
    await fetch(`/api/events/${encodeURIComponent(id)}`, {
      method: "DELETE",
      credentials: "include",
    });
    setEvents((prev) => prev.filter((x) => x.id !== id));
    if (selectedEvent?.id === id) setSelectedEvent(null);
    notify("Deleted event record");
  };

  const handleSaveConfig = async (saveOption: "saveonly" | "restart") => {
    const res = await fetch(
      `/api/config/save?save_option=${encodeURIComponent(saveOption)}`,
      {
        method: "POST",
        headers: { "Content-Type": "text/plain" },
        credentials: "include",
        body: rawYaml,
      }
    );
    const data = await res.json().catch(() => ({}));
    if (res.ok && data.success !== false) {
      if (saveOption === "restart") {
        await fetch("/api/restart", { method: "POST", credentials: "include" });
      }
      notify(
        saveOption === "restart"
          ? "Configuration saved and NinjaNVR restarted"
          : "Configuration saved to disk"
      );
      await fetchCoreData();
    } else {
      notify(data.message || "Failed to validate/save YAML configuration");
    }
  };

  // Filtered Review items (virtualized window of 40 items)
  const filteredReviews = useMemo(() => {
    return reviews.filter((r) => {
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
    reviewCameraFilter,
    reviewSeverityFilter,
    reviewObjectFilter,
    reviewStatusFilter,
  ]);

  // Filtered Explore items
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
  }, [events, exploreCamera, exploreLabel, exploreRetainedOnly, exploreQuery]);

  // Global keyboard shortcuts (1-7 tabs, J/K/R/E in Review)
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement | null)?.tagName?.toLowerCase();
      if (tag === "input" || tag === "textarea" || tag === "select") return;
      if (e.ctrlKey || e.metaKey || e.altKey) return;

      if (activeTab === "review") {
        const key = e.key.toLowerCase();
        if (key === "j" || key === "k") {
          const idx = filteredReviews.findIndex((r) => r.id === activeReview?.id);
          const nextIdx =
            key === "j"
              ? Math.min(filteredReviews.length - 1, Math.max(0, idx + 1))
              : Math.max(0, idx - 1);
          if (filteredReviews[nextIdx]) setActiveReview(filteredReviews[nextIdx]);
          return;
        }
        if (key === "r" && activeReview) {
          handleMarkReviews([activeReview.id], true);
          return;
        }
        if (key === "e" && activeReview) {
          handleQuickExport(
            activeReview.camera,
            activeReview.start_time - 5,
            (activeReview.end_time || activeReview.start_time + 20) + 5,
            `${activeReview.camera}_${activeReview.id}`
          );
          return;
        }
        if (key === "1") {
          setPlaybackSpeed(1);
          return;
        }
        if (key === "2") {
          setPlaybackSpeed(2);
          return;
        }
        if (key === "4") {
          setPlaybackSpeed(4);
          return;
        }
      }

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
  }, [
    activeTab,
    activeReview,
    filteredReviews,
    switchTab,
    handleMarkReviews,
    handleQuickExport,
    setPlaybackSpeed,
    userRole,
  ]);

  // Telemetry summary values
  const sysCpu = stats?.cpu_usages?.["ninjanvr.full_system"]?.cpu || "6.5";
  const sysMem = stats?.cpu_usages?.["ninjanvr.full_system"]?.mem || "26.0";
  const inferMs = stats?.detectors?.detector01?.inference_speed?.toFixed(1) || "7.0";
  const totalFps = stats?.camera_fps?.toFixed(1) || "20.0";
  const displayVersion = formatDisplayVersion(stats?.service?.version, t.brand.release);
  const headerTelemetry = resolveHeaderTelemetryMeta(wsOnline, cameraNames.length);

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
            style={{ height: 34 }}
            disabled={loginBusy || !authProviders.local_login_enabled}
          >
            {loginBusy ? t.auth.signingIn : t.auth.signIn}
          </button>
          {authProviders.oidc?.enabled && (
            <a
              href="/api/auth/oidc/login"
              className="nvr-btn mono"
              style={{
                height: 34,
                display: "flex",
                alignItems: "center",
                justifyContent: "center",
                textDecoration: "none",
                border: "1px solid var(--signal-accent)",
                color: "var(--signal-accent-text)",
              }}
            >
              Sign in with {authProviders.oidc.provider_name || "SSO"} (OIDC)
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

  return (
    <div className="nvr-shell">
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

        <nav className="nvr-nav" aria-label="Primary Workspace Navigation">
          <button
            className={`nvr-nav-btn ${activeTab === "live" ? "active" : ""}`}
            onClick={() => switchTab("live")}
          >
            {t.nav.live}
            <span className="nvr-kbd">1</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "sync" ? "active" : ""}`}
            onClick={() => switchTab("sync")}
          >
            {t.nav.sync}
            <span className="nvr-kbd">2</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "review" ? "active" : ""}`}
            onClick={() => switchTab("review")}
          >
            {t.nav.review}
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
            {t.nav.explore}
            <span className="nvr-kbd">4</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "exports" ? "active" : ""}`}
            onClick={() => switchTab("exports")}
          >
            {t.nav.exports}
            <span className="nvr-kbd">5</span>
          </button>
          {userRole === "admin" && (
            <>
              <button
                className={`nvr-nav-btn ${activeTab === "zones" ? "active" : ""}`}
                onClick={() => switchTab("zones")}
              >
                {t.nav.zones}
                <span className="nvr-kbd">6</span>
              </button>
              <button
                className={`nvr-nav-btn ${activeTab === "system" ? "active" : ""}`}
                onClick={() => switchTab("system")}
              >
                {t.nav.system}
                <span className="nvr-kbd">7</span>
              </button>
            </>
          )}
        </nav>

        <div className="nvr-header-right">
          {config?.peers && Object.keys(config.peers).length > 0 && (
            <select
              className="nvr-select mono"
              aria-label="Federated NVR Node Selector"
              style={{ height: 26, fontSize: "var(--text-xs)" }}
              value={selectedNode}
              onChange={(e) => setSelectedNode(e.target.value)}
              title="Switch between local NVR node and federated peer NVR nodes"
            >
              <option value="all">NODE: ALL FEDERATED</option>
              <option value="local">NODE: LOCAL (PRIMARY)</option>
              {Object.entries(config.peers)
                .filter(([, p]) => p.enabled)
                .map(([peerId]) => (
                  <option key={peerId} value={peerId}>
                    NODE: {peerId.toUpperCase()}
                  </option>
                ))}
            </select>
          )}
          <div
            className="mono"
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 2,
              background: "var(--bg-subcard)",
              border: "1px solid var(--border-strong)",
              borderRadius: 3,
              padding: 2,
            }}
            aria-label="Operational Security Profile"
          >
            {(["armed", "home", "disarmed"] as const).map((prof) => {
              const isAct = securityProfile === prof;
              return (
                <button
                  key={prof}
                  type="button"
                  disabled={userRole === "viewer"}
                  className={`nvr-btn mono ${isAct ? "active" : ""}`}
                  style={{
                    height: 22,
                    padding: "0 6px",
                    fontSize: "var(--text-xs)",
                    color:
                      isAct && prof === "armed"
                        ? "var(--signal-alert-text)"
                        : isAct && prof === "home"
                        ? "var(--signal-motion-text)"
                        : undefined,
                  }}
                  onClick={async () => {
                    setSecurityProfile(prof);
                    try {
                      await fetch("/api/profile", {
                        method: "POST",
                        headers: { "Content-Type": "application/json" },
                        credentials: "include",
                        body: JSON.stringify({ profile: prof }),
                      });
                      notify(`Operational security profile switched to ${prof.toUpperCase()}`);
                    } catch {
                      // ignore
                    }
                  }}
                >
                  {prof.toUpperCase()}
                </button>
              );
            })}
          </div>
          {stats?.service?.storage_degraded && (
            <button
              className="nvr-badge alert mono"
              onClick={() => switchTab("system")}
              title={
                stats.service.storage_error ||
                "Recordings storage mount is unavailable or read-only; Live Wall remains active"
              }
              style={{
                cursor: "pointer",
                padding: "3px 8px",
                fontSize: "var(--text-xs)",
                fontWeight: 700,
              }}
            >
              STORAGE DEGRADED
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
            <span>CPU {sysCpu}%</span>
            <span className="nvr-divider nvr-telemetry-ext" aria-hidden="true" />
            <span className="nvr-telemetry-ext">MEM {sysMem}%</span>
            <span className="nvr-divider" aria-hidden="true" />
            <span>OV {inferMs}ms</span>
            <span className="nvr-divider" aria-hidden="true" />
            <span>{totalFps} FPS</span>
          </button>
          <button
            type="button"
            data-testid="topbar-theme-toggle"
            className="nvr-btn mono"
            style={{
              height: 26,
              padding: "0 8px",
              fontSize: "var(--text-xs)",
              display: "inline-flex",
              alignItems: "center",
              gap: 4,
            }}
            title={`${t.theme.cycleTooltip} — Current: ${
              themeMode === "dark"
                ? t.theme.dark
                : themeMode === "light"
                ? t.theme.light
                : t.theme.system
            }${highContrast ? " (High-Contrast Boost)" : ""}`}
            aria-label="Cycle Console Theme (Dark / Light / System)"
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
            <span>
              {themeMode === "dark"
                ? "DARK"
                : themeMode === "light"
                ? "LIGHT"
                : "AUTO"}
            </span>
          </button>
          <button
            type="button"
            className="nvr-btn mono"
            style={{
              height: 26,
              padding: "0 8px",
              fontSize: "var(--text-xs)",
              display: "inline-flex",
              alignItems: "center",
              gap: 4,
            }}
            title={`Signed in as ${username} (${userRole}) — Click to log out`}
            aria-label="Log out of NinjaNVR Console"
            onClick={async () => {
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
              setFocusSegments([]);
              setGlobalDaysSummary([]);
              setFfprobeInfo(null);
              setDraftPoints([]);
              setRawYaml("");
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
                switchTab("live");
              }
              setAuthNeeded(true);
            }}
          >
            <LogOut size={12} />
            <span>Logout</span>
          </button>
        </div>
      </header>

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
          />
        )}

        {activeTab === "exports" && (
          <ExportsWorkspace
            cameraNames={cameraNames}
            config={config}
            exportsList={exportsList}
            setExportsList={setExportsList}
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
      </main>
    </div>
  );
};
