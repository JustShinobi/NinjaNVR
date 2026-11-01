import React, { useCallback, useEffect, useMemo, useState } from "react";
import {
  Activity,
  AlertTriangle,
  Camera,
  Check,
  CheckCircle2,
  Clock,
  Copy,
  Cpu,
  Database,
  Download,
  Eye,
  Film,
  Filter,
  HardDrive,
  Layers,
  LogOut,
  Pause,
  Play,
  RefreshCw,
  RotateCcw,
  Save,
  Search,
  Server,
  ShieldAlert,
  Sliders,
  Sparkles,
  Star,
  Terminal,
  Trash2,
  Video,
} from "lucide-react";
import { useI18n } from "./i18n";
import {
  CameraLiveActivity,
  EventItem,
  ExportItem,
  NinjaConfigResponse,
  NinjaStatsResponse,
  RecordingSegment,
  ReviewItem,
} from "./types/nvr";
import {
  CanvasSparkline,
  LiveMsePlayer,
  VodHlsPlayer,
  extractMaskPolygons,
} from "./components/VideoPlayers";
import { TimelineScrubber } from "./components/TimelineScrubber";

type WorkspaceTab =
  | "live"
  | "sync"
  | "review"
  | "explore"
  | "exports"
  | "zones"
  | "system";

type LiveLayoutMode = "2x2" | "focus" | "single";

function resolveInitialTab(): WorkspaceTab {
  const path = window.location.pathname.toLowerCase();
  if (path.startsWith("/sync")) return "sync";
  if (path.startsWith("/review")) return "review";
  if (path.startsWith("/explore")) return "explore";
  if (path.startsWith("/export")) return "exports";
  if (path.startsWith("/config") || path.startsWith("/settings")) return "zones";
  if (path.startsWith("/system") || path.startsWith("/logs")) return "system";
  return "live";
}

function formatTs(unixSec: number): string {
  const d = new Date(unixSec * 1000);
  return d.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

function formatDateTime(unixSec: number): string {
  const d = new Date(unixSec * 1000);
  return d.toLocaleString([], {
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

function formatRelTime(unixSec: number): string {
  const diff = Math.max(0, Math.round(Date.now() / 1000 - unixSec));
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return `${Math.floor(diff / 86400)}d ago`;
}

function formatUptime(sec: number): string {
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = sec % 60;
  return `${h}h ${m}m ${s}s`;
}

export const App: React.FC = () => {
  const { t } = useI18n();
  const [activeTab, setActiveTab] = useState<WorkspaceTab>(resolveInitialTab);
  const [authChecked, setAuthChecked] = useState(false);
  const [authNeeded, setAuthNeeded] = useState(false);
  const [username, setUsername] = useState("admin");
  const [loginUser, setLoginUser] = useState("admin");
  const [loginPass, setLoginPass] = useState("");
  const [loginError, setLoginError] = useState<string | null>(null);
  const [loginBusy, setLoginBusy] = useState(false);

  // Core NVR state
  const [config, setConfig] = useState<NinjaConfigResponse | null>(null);
  const [stats, setStats] = useState<NinjaStatsResponse | null>(null);
  const [statsHistory, setStatsHistory] = useState<NinjaStatsResponse[]>([]);
  const [wsOnline, setWsOnline] = useState(false);
  const [cameraActivity, setCameraActivity] = useState<Record<string, CameraLiveActivity>>({});
  const [toastMsg, setToastMsg] = useState<string | null>(null);

  // Live Matrix & Time-Shift state
  const [liveLayout, setLiveLayout] = useState<LiveLayoutMode>("2x2");
  const [selectedCamera, setSelectedCamera] = useState<string>("escritorio");
  const [useMainStream, setUseMainStream] = useState(false);
  const [showVectors, setShowVectors] = useState(false);
  const [timeShiftTs, setTimeShiftTs] = useState<number | null>(null);
  const [focusSegments, setFocusSegments] = useState<RecordingSegment[]>([]);
  const [hoveredAlertId, setHoveredAlertId] = useState<string | null>(null);

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
  const [playbackSpeed, setPlaybackSpeed] = useState<number>(1);

  // Sync Playback state
  const [syncMasterTs, setSyncMasterTs] = useState<number>(() => Math.floor(Date.now() / 1000) - 120);
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
  const [ffprobeInfo, setFfprobeInfo] = useState<unknown[] | null>(null);

  // System & Logs state
  const [logService, setLogService] = useState<"frigate" | "go2rtc" | "nginx">("frigate");
  const [logLines, setLogLines] = useState<string[]>([]);
  const [logSearch, setLogSearch] = useState("");
  const [logPaused, setLogPaused] = useState(false);

  const notify = useCallback((msg: string) => {
    setToastMsg(msg);
    window.setTimeout(() => {
      setToastMsg((curr) => (curr === msg ? null : curr));
    }, 3200);
  }, []);

  const cameraNames = useMemo(() => {
    if (!config?.cameras) return ["escritorio", "sala", "quarto", "cozinha"];
    return Object.keys(config.cameras).filter((k) => config.cameras[k].enabled);
  }, [config]);

  const unreviewedAlerts = useMemo(
    () => reviews.filter((r) => !r.has_been_reviewed),
    [reviews]
  );

  // Navigate tab + update browser URL cleanly
  const switchTab = useCallback((tab: WorkspaceTab) => {
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
  }, []);

  // Fetch core bootstrap data
  const fetchCoreData = useCallback(async () => {
    try {
      let revUrl = "/api/review?limit=200";
      if (reviewDateFilter) {
        const parts = reviewDateFilter.split("-").map(Number);
        if (parts.length === 3) {
          const dayStart = Math.floor(
            new Date(parts[0], parts[1] - 1, parts[2], 0, 0, 0).getTime() / 1000
          );
          const dayEnd = dayStart + 86400;
          revUrl = `/api/review?after=${dayStart}&before=${dayEnd}&limit=300`;
        }
      }
      const [cfgRes, statsRes, histRes, revRes] = await Promise.all([
        fetch("/api/config", { credentials: "include" }),
        fetch("/api/stats", { credentials: "include" }),
        fetch("/api/stats/history", { credentials: "include" }),
        fetch(revUrl, { credentials: "include" }),
      ]);

      if (cfgRes.status === 401 || statsRes.status === 401) {
        setAuthNeeded(true);
        setAuthChecked(true);
        return;
      }

      setAuthNeeded(false);
      setAuthChecked(true);
      if (cfgRes.ok) {
        const cfgJson: NinjaConfigResponse = await cfgRes.json();
        setConfig(cfgJson);
        const cams = Object.keys(cfgJson.cameras || {});
        if (cams.length > 0 && !cams.includes(selectedCamera)) {
          setSelectedCamera(cams[0]);
        }
      }
      if (statsRes.ok) {
        setStats(await statsRes.json());
      }
      if (histRes.ok) {
        const h = await histRes.json();
        if (Array.isArray(h)) setStatsHistory(h);
      }
      if (revRes.ok) {
        const revJson: ReviewItem[] = await revRes.json();
        setReviews(revJson);
        if (!activeReview && revJson.length > 0) {
          setActiveReview(revJson[0]);
          setSyncMasterTs(Math.floor(revJson[0].start_time));
        }
      }
    } catch {
      // network retry on next tick
    }
  }, [selectedCamera, activeReview, reviewDateFilter]);

  useEffect(() => {
    fetchCoreData();
    if (authNeeded) return;
    const timer = window.setInterval(fetchCoreData, 5000);
    return () => window.clearInterval(timer);
  }, [fetchCoreData, authNeeded]);

  // Connect to /ws for real-time camera_activity and stats
  useEffect(() => {
    if (!authChecked || authNeeded) return;
    let closed = false;
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const ws = new WebSocket(`${proto}//${window.location.host}/ws`);

    ws.onopen = () => {
      if (!closed) setWsOnline(true);
    };
    ws.onclose = () => {
      if (!closed) setWsOnline(false);
    };
    ws.onmessage = (ev) => {
      if (closed || typeof ev.data !== "string") return;
      try {
        const msg = JSON.parse(ev.data);
        if (msg.topic === "camera_activity" && msg.payload) {
          const parsed =
            typeof msg.payload === "string" ? JSON.parse(msg.payload) : msg.payload;
          setCameraActivity(parsed);
        }
      } catch {
        // ignore
      }
    };

    return () => {
      closed = true;
      ws.close();
    };
  }, [authNeeded]);

  // Fetch 24h recordings for the active timeline camera
  const timelineCam =
    activeTab === "review" && activeReview
      ? activeReview.camera
      : activeTab === "exports"
      ? exportCam
      : selectedCamera;

  useEffect(() => {
    if (!authChecked || authNeeded || !timelineCam) return;
    const now = Math.floor(Date.now() / 1000);
    fetch(
      `/api/${encodeURIComponent(timelineCam)}/recordings?after=${now - 86400}&before=${now + 10}`,
      { credentials: "include" }
    )
      .then((r) => (r.ok ? r.json() : []))
      .then((segs) => {
        if (Array.isArray(segs)) setFocusSegments(segs);
      })
      .catch(() => {});
  }, [authNeeded, timelineCam, stats?.service?.last_updated]);

  // Tab-specific data loaders
  useEffect(() => {
    if (authNeeded) return;
    if (activeTab === "explore") {
      let evUrl = "/api/events?limit=120";
      if (reviewDateFilter) {
        const parts = reviewDateFilter.split("-").map(Number);
        if (parts.length === 3) {
          const dayStart = Math.floor(
            new Date(parts[0], parts[1] - 1, parts[2], 0, 0, 0).getTime() / 1000
          );
          const dayEnd = dayStart + 86400;
          evUrl = `/api/events?after=${dayStart}&before=${dayEnd}&limit=250`;
        }
      }
      fetch(evUrl, { credentials: "include" })
        .then((r) => (r.ok ? r.json() : []))
        .then((arr: EventItem[]) => {
          if (Array.isArray(arr)) {
            setEvents(arr);
            if (!selectedEvent && arr.length > 0) setSelectedEvent(arr[0]);
          }
        })
        .catch(() => {});
    } else if (activeTab === "exports") {
      fetch("/api/exports", { credentials: "include" })
        .then((r) => (r.ok ? r.json() : []))
        .then((arr: ExportItem[]) => {
          if (Array.isArray(arr)) {
            setExportsList(arr);
            if (!activeExportPreview && arr.length > 0) setActiveExportPreview(arr[0]);
          }
        })
        .catch(() => {});
    } else if (activeTab === "zones") {
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
          if (txt && !rawYaml) setRawYaml(txt);
        })
        .catch(() => {});
      fetch(`/api/ffprobe?paths=camera:${encodeURIComponent(inspectorCam)}`, {
        credentials: "include",
      })
        .then((r) => (r.ok ? r.json() : null))
        .then((info) => {
          if (Array.isArray(info)) setFfprobeInfo(info);
        })
        .catch(() => {});
    } else if (activeTab === "system" && !logPaused) {
      fetch(`/api/logs/${logService}?start=-120`, { credentials: "include" })
        .then((r) => (r.ok ? r.json() : { lines: [] }))
        .then((d) => {
          if (Array.isArray(d.lines)) setLogLines(d.lines);
        })
        .catch(() => {});
    }
  }, [
    authNeeded,
    activeTab,
    inspectorCam,
    logService,
    logPaused,
    rawYaml,
    selectedEvent,
    activeExportPreview,
    reviewDateFilter,
    stats?.service?.last_updated,
  ]);

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
        setUsername(loginUser);
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

  const handleMarkReviews = async (ids: string[], reviewed = true) => {
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
  };

  const handleDeleteReviews = async (ids: string[]) => {
    if (ids.length === 0) return;
    await fetch("/api/reviews/delete", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ ids }),
    });
    setReviews((prev) => prev.filter((r) => !ids.includes(r.id)));
    notify(`Deleted ${ids.length} review segment(s)`);
  };

  const handleQuickExport = async (
    camera: string,
    startSec: number,
    endSec: number,
    label?: string
  ): Promise<{ exportId?: string; videoUrl?: string } | void> => {
    const s = Math.floor(startSec);
    const e = Math.ceil(Math.max(endSec, startSec + 5));
    const res = await fetch(`/api/export/${encodeURIComponent(camera)}/start/${s}/end/${e}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ name: label || `${camera} ${formatTs(s)}-${formatTs(e)}` }),
    });
    if (res.ok) {
      const data = await res.json().catch(() => ({}));
      notify(`MP4 clip ready for ${camera} (${formatTs(s)} - ${formatTs(e)})`);
      const listRes = await fetch("/api/exports", { credentials: "include" });
      if (listRes.ok) {
        const arr: ExportItem[] = await listRes.json();
        setExportsList(arr);
        if (arr.length > 0) setActiveExportPreview(arr[0]);
      }
      return {
        exportId: data.export_id,
        videoUrl: data.video_url || `/exports/${camera}_${s}_${e}.mp4`,
      };
    }
  };

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
    const res = await fetch(`/api/config/save?save_option=${saveOption}`, {
      method: "POST",
      headers: { "Content-Type": "text/plain" },
      credentials: "include",
      body: rawYaml,
    });
    if (res.ok) {
      notify(
        saveOption === "restart"
          ? "Configuration saved and NinjaNVR restarted"
          : "Configuration saved to disk"
      );
      fetchCoreData();
    } else {
      notify("Failed to validate/save YAML configuration");
    }
  };

  // Filtered Review items (virtualized window of 40 items)
  const filteredReviews = useMemo(() => {
    return reviews.filter((r) => {
      if (reviewCameraFilter !== "all" && r.camera !== reviewCameraFilter) return false;
      if (reviewSeverityFilter !== "all" && r.severity !== reviewSeverityFilter) return false;
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
        const hay = `${ev.id} ${ev.camera} ${ev.label} ${(ev.zones || []).join(" ")}`.toLowerCase();
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
      else if (e.key === "6") switchTab("zones");
      else if (e.key === "7") switchTab("system");
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [activeTab, activeReview, filteredReviews, switchTab]);

  // Telemetry summary values
  const sysCpu = stats?.cpu_usages?.["frigate.full_system"]?.cpu || "6.5";
  const sysMem = stats?.cpu_usages?.["frigate.full_system"]?.mem || "26.0";
  const inferMs = stats?.detectors?.detector01?.inference_speed?.toFixed(1) || "7.0";
  const totalFps = stats?.camera_fps?.toFixed(1) || "20.0";

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
            width: 380,
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
            <div className="nvr-brand-mark" />
            <div>
              <div className="nvr-brand-title">{t.brand.name} // {t.brand.release}</div>
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
                color: "#fca5a5",
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
          <button type="submit" className="nvr-btn primary" style={{ height: 34 }} disabled={loginBusy}>
            {loginBusy ? t.auth.signingIn : t.auth.signIn}
          </button>
          <div style={{ display: "flex", justifyContent: "space-between", paddingTop: 6 }}>
            <a
              href="/api/ui/switch?target=v1"
              className="mono"
              style={{ color: "var(--text-muted)", fontSize: 11, textDecoration: "none" }}
            >
              ← {t.brand.classicSwitch}
            </a>
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
      {/* Top Tactical Header */}
      <header className="nvr-header">
        <div className="nvr-brand">
          <div className="nvr-brand-mark" />
          <div>
            <div style={{ display: "flex", alignItems: "center", gap: 7 }}>
              <span className="nvr-brand-title">{t.brand.name}</span>
              <span
                className="mono"
                style={{
                  fontSize: 10,
                  padding: "1px 5px",
                  background: "rgba(14, 165, 233, 0.16)",
                  border: "1px solid rgba(14, 165, 233, 0.45)",
                  color: "#38bdf8",
                  borderRadius: 2,
                }}
              >
                v2.0
              </span>
            </div>
            <div className="nvr-brand-sub">
              {wsOnline ? "WS NOMINAL" : "POLLING"} · {cameraNames.length} CH
            </div>
          </div>
        </div>

        <nav className="nvr-nav" aria-label="Primary Workspace Navigation">
          <button
            className={`nvr-nav-btn ${activeTab === "live" ? "active" : ""}`}
            onClick={() => switchTab("live")}
          >
            <Video size={14} />
            {t.nav.live}
            <span className="nvr-kbd">1</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "sync" ? "active" : ""}`}
            onClick={() => switchTab("sync")}
          >
            <Layers size={14} />
            {t.nav.sync}
            <span className="nvr-kbd">2</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "review" ? "active" : ""}`}
            onClick={() => switchTab("review")}
          >
            <ShieldAlert size={14} />
            {t.nav.review}
            {unreviewedAlerts.length > 0 && (
              <span
                className="mono"
                style={{
                  fontSize: 10,
                  padding: "0 5px",
                  background: "rgba(239, 68, 68, 0.22)",
                  border: "1px solid rgba(239, 68, 68, 0.55)",
                  color: "#fca5a5",
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
            <Search size={14} />
            {t.nav.explore}
            <span className="nvr-kbd">4</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "exports" ? "active" : ""}`}
            onClick={() => switchTab("exports")}
          >
            <Film size={14} />
            {t.nav.exports}
            <span className="nvr-kbd">5</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "zones" ? "active" : ""}`}
            onClick={() => switchTab("zones")}
          >
            <Sliders size={14} />
            {t.nav.zones}
            <span className="nvr-kbd">6</span>
          </button>
          <button
            className={`nvr-nav-btn ${activeTab === "system" ? "active" : ""}`}
            onClick={() => switchTab("system")}
          >
            <Activity size={14} />
            {t.nav.system}
            <span className="nvr-kbd">7</span>
          </button>
        </nav>

        <div className="nvr-header-right">
          <button
            className="nvr-telemetry-pill"
            onClick={() => switchTab("system")}
            title="Inspect System Telemetry & OpenVINO Detector"
          >
            <span className="nvr-dot live" />
            <span>CPU {sysCpu}%</span>
            <span style={{ color: "var(--border-strong)" }}>|</span>
            <span>MEM {sysMem}%</span>
            <span style={{ color: "var(--border-strong)" }}>|</span>
            <span>OV {inferMs}ms</span>
            <span style={{ color: "var(--border-strong)" }}>|</span>
            <span>{totalFps} FPS</span>
          </button>

          <a
            href="/api/ui/switch?target=v1"
            className="nvr-btn"
            title={t.brand.classicSwitchHint}
            style={{ textDecoration: "none" }}
          >
            <RotateCcw size={13} />
            {t.brand.classicSwitch}
          </a>
        </div>
      </header>

      {/* Toast Notification Banner */}
      {toastMsg && (
        <div
          className="mono"
          style={{
            position: "fixed",
            bottom: 14,
            right: 16,
            zIndex: 9999,
            background: "#10151f",
            border: "1px solid var(--signal-accent)",
            color: "var(--text-primary)",
            padding: "8px 14px",
            borderRadius: 3,
            fontSize: 11.5,
            boxShadow: "0 8px 24px rgba(0,0,0,0.65)",
          }}
        >
          {toastMsg}
        </div>
      )}

      {/* Main Content Area */}
      <main className="nvr-main">
        {/* ===================================================================
            WORKSPACE 1: LIVE MATRIX & FOCUS STAGE WITH TIME-SHIFT
           =================================================================== */}
        {activeTab === "live" && (
          <>
            {/* Sub-bar controls */}
            <div className="nvr-subbar">
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <button
                  className={`nvr-btn ${liveLayout === "2x2" ? "active" : ""}`}
                  onClick={() => {
                    setLiveLayout("2x2");
                    setTimeShiftTs(null);
                  }}
                >
                  {t.live.grid2x2}
                </button>
                <button
                  className={`nvr-btn ${liveLayout === "focus" ? "active" : ""}`}
                  onClick={() => setLiveLayout("focus")}
                >
                  {t.live.focusStage}
                </button>
                <button
                  className={`nvr-btn ${liveLayout === "single" ? "active" : ""}`}
                  onClick={() => setLiveLayout("single")}
                >
                  {t.live.singleFull}
                </button>
                <span style={{ color: "var(--border-strong)", margin: "0 4px" }}>|</span>
                {cameraNames.map((cam) => (
                  <button
                    key={cam}
                    className={`nvr-btn mono ${selectedCamera === cam && liveLayout !== "2x2" ? "active" : ""}`}
                    onClick={() => {
                      setSelectedCamera(cam);
                      if (liveLayout === "2x2") setLiveLayout("focus");
                    }}
                    style={{ textTransform: "uppercase" }}
                  >
                    {cam}
                  </button>
                ))}
              </div>

              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <button
                  className={`nvr-btn ${useMainStream ? "active" : ""}`}
                  onClick={() => setUseMainStream((v) => !v)}
                  title="Switch between 1080p Main Stream and 720p Substream"
                >
                  {useMainStream ? t.live.hdStream : t.live.subStream}
                </button>
                <button
                  className={`nvr-btn ${showVectors ? "active" : ""}`}
                  onClick={() => setShowVectors((v) => !v)}
                  title="Overlay configured Motion Masks and Detection Zones"
                >
                  <Eye size={13} />
                  {t.live.zonesOverlay}
                </button>
                {unreviewedAlerts.length > 0 && (
                  <button
                    className="nvr-btn success"
                    onClick={() =>
                      handleMarkReviews(
                        unreviewedAlerts.slice(0, 25).map((r) => r.id),
                        true
                      )
                    }
                  >
                    <Check size={13} />
                    {t.live.markAllReviewed} ({unreviewedAlerts.length})
                  </button>
                )}
              </div>
            </div>

            {/* Zero-Decoder-Bloat Unreviewed Alert Ribbon (Video mounts ONLY on pointer hover!) */}
            {unreviewedAlerts.length > 0 && (
              <div className="nvr-alert-ribbon" aria-label={t.live.unreviewedAlerts}>
                <div
                  style={{
                    minWidth: 118,
                    display: "flex",
                    flexDirection: "column",
                    justifyContent: "center",
                    gap: 3,
                    paddingRight: 6,
                    borderRight: "1px solid var(--border-hairline)",
                  }}
                >
                  <div
                    className="mono"
                    style={{ fontSize: 10.5, fontWeight: 700, color: "#fca5a5" }}
                  >
                    {unreviewedAlerts.length} UNREVIEWED
                  </div>
                  <div style={{ fontSize: 10.5, color: "var(--text-muted)", lineHeight: 1.25 }}>
                    Hover for MP4 · Click for Time-Shift
                  </div>
                </div>

                {unreviewedAlerts.slice(0, 14).map((item) => {
                  const isHovered = hoveredAlertId === item.id;
                  const thumbSrc = item.thumb_path
                    ? item.thumb_path.replace("/media/frigate", "")
                    : `/api/${item.camera}/latest.webp`;
                  const objs = item.data?.objects || ["person"];

                  return (
                    <div
                      key={item.id}
                      className={`nvr-alert-card severity-${item.severity}`}
                      onPointerEnter={() => setHoveredAlertId(item.id)}
                      onPointerLeave={() =>
                        setHoveredAlertId((curr) => (curr === item.id ? null : curr))
                      }
                      onClick={() => {
                        setSelectedCamera(item.camera);
                        setLiveLayout("focus");
                        setTimeShiftTs(Math.floor(item.start_time));
                      }}
                    >
                      {isHovered ? (
                        <video
                          src={`/api/review/${encodeURIComponent(item.id)}/preview?format=mp4`}
                          autoPlay
                          muted
                          loop
                          playsInline
                        />
                      ) : (
                        <img src={thumbSrc} alt={`${item.camera} alert`} loading="lazy" />
                      )}
                      <div className="nvr-alert-card-hud">
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                          }}
                        >
                          <span
                            className="mono"
                            style={{
                              fontSize: 9.5,
                              fontWeight: 700,
                              textTransform: "uppercase",
                              color: "#fff",
                            }}
                          >
                            {item.camera}
                          </span>
                          <button
                            className="nvr-btn"
                            style={{
                              height: 18,
                              padding: "0 4px",
                              fontSize: 9,
                              background: "rgba(16,185,129,0.25)",
                              borderColor: "rgba(16,185,129,0.6)",
                            }}
                            title={t.live.markReviewed}
                            onClick={(e) => {
                              e.stopPropagation();
                              handleMarkReviews([item.id], true);
                            }}
                          >
                            <Check size={10} />
                          </button>
                        </div>
                        <div
                          style={{
                            display: "flex",
                            alignItems: "center",
                            justifyContent: "space-between",
                            fontSize: 9.5,
                          }}
                        >
                          <span
                            className="mono"
                            style={{ color: "#fcd34d", textTransform: "uppercase" }}
                          >
                            {objs[0]}
                          </span>
                          <span className="mono" style={{ color: "#cbd5e1" }}>
                            {formatTs(item.start_time)}
                          </span>
                        </div>
                      </div>
                    </div>
                  );
                })}
              </div>
            )}

            {/* 2x2 Matrix View */}
            {liveLayout === "2x2" && (
              <div className="nvr-matrix-2x2">
                {cameraNames.map((cam) => {
                  const camCfg = config?.cameras?.[cam];
                  const camStat = stats?.cameras?.[cam];
                  const act = cameraActivity[cam];
                  const hasPerson =
                    (act?.objects || []).length > 0 || (camStat?.detection_fps || 0) > 1.5;
                  const hasMotion = act?.motion || (camStat?.detection_fps || 0) > 0.2;
                  const isTimelineCam = selectedCamera === cam;
                  const isTimeShifted = isTimelineCam && timeShiftTs !== null;

                  return (
                    <div
                      key={cam}
                      className="nvr-cam-tile"
                      style={{
                        outline: isTimelineCam ? "1px solid var(--signal-accent)" : "none",
                        outlineOffset: -1,
                      }}
                    >
                      {isTimeShifted && timeShiftTs !== null ? (
                        <VodHlsPlayer
                          camera={cam}
                          startTime={timeShiftTs}
                          endTime={timeShiftTs + 180}
                          playbackRate={playbackSpeed}
                        />
                      ) : (
                        <LiveMsePlayer
                          camera={cam}
                          useMainStream={useMainStream}
                          cameraConfig={camCfg}
                          showVectors={showVectors}
                          onClick={() => {
                            if (selectedCamera === cam) {
                              setLiveLayout("focus");
                            } else {
                              setSelectedCamera(cam);
                            }
                          }}
                        />
                      )}
                      <div className="nvr-cam-hud-top">
                        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                          <span
                            className={`nvr-badge ${isTimeShifted ? "rewind" : "live"}`}
                          >
                            <span
                              className={`nvr-dot ${isTimeShifted ? "motion" : "live"}`}
                            />
                            {cam.toUpperCase()}
                            {isTimeShifted && timeShiftTs !== null
                              ? ` @ ${formatTs(timeShiftTs)}`
                              : ""}
                          </span>
                          <span className="nvr-badge">
                            {camCfg?.detect?.width || 1280}×{camCfg?.detect?.height || 720} ·{" "}
                            {camStat?.camera_fps?.toFixed(1) || "5.0"} FPS
                          </span>
                        </div>
                        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                          {hasPerson && <span className="nvr-badge alert">PERSON TRACK</span>}
                          {hasMotion && !hasPerson && (
                            <span className="nvr-badge motion">{t.live.motionActive}</span>
                          )}
                          <span className="nvr-badge">
                            AI {camStat?.detection_fps?.toFixed(1) || "0.0"} FPS
                          </span>
                        </div>
                      </div>
                      <div className="nvr-cam-hud-bottom">
                        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                          <button
                            className="nvr-btn"
                            style={{ height: 22, padding: "0 7px", fontSize: 10.5 }}
                            onClick={(e) => {
                              e.stopPropagation();
                              setSelectedCamera(cam);
                              setTimeShiftTs(Math.floor(Date.now() / 1000) - 30);
                            }}
                          >
                            <Clock size={11} />
                            {t.live.rewind30s}
                          </button>
                          {isTimeShifted && (
                            <button
                              className="nvr-btn success"
                              style={{ height: 22, padding: "0 7px", fontSize: 10.5 }}
                              onClick={(e) => {
                                e.stopPropagation();
                                setTimeShiftTs(null);
                              }}
                            >
                              LIVE
                            </button>
                          )}
                          <button
                            className="nvr-btn"
                            style={{ height: 22, padding: "0 7px", fontSize: 10.5 }}
                            onClick={(e) => {
                              e.stopPropagation();
                              setSelectedCamera(cam);
                              setLiveLayout("focus");
                            }}
                          >
                            Expand Focus
                          </button>
                        </div>
                        <a
                          href={`/api/${cam}/latest.jpg`}
                          download={`${cam}-snapshot.jpg`}
                          className="nvr-btn"
                          style={{
                            height: 22,
                            padding: "0 7px",
                            fontSize: 10.5,
                            textDecoration: "none",
                          }}
                          onClick={(e) => e.stopPropagation()}
                        >
                          <Camera size={11} />
                          {t.live.snapshot}
                        </a>
                      </div>
                    </div>
                  );
                })}
              </div>
            )}

            {/* Focus Stage + Side Dock */}
            {(liveLayout === "focus" || liveLayout === "single") && (
              <div
                className={
                  liveLayout === "focus" ? "nvr-focus-layout" : "nvr-matrix-single"
                }
              >
                <div className="nvr-focus-stage">
                  <div className="nvr-cam-tile" style={{ flex: 1 }}>
                    {timeShiftTs === null ? (
                      <LiveMsePlayer
                        camera={selectedCamera}
                        useMainStream={true}
                        cameraConfig={config?.cameras?.[selectedCamera]}
                        showVectors={showVectors}
                      />
                    ) : (
                      <VodHlsPlayer
                        camera={selectedCamera}
                        startTime={timeShiftTs}
                        endTime={timeShiftTs + 180}
                        playbackRate={playbackSpeed}
                      />
                    )}
                    <div className="nvr-cam-hud-top">
                      <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                        <span
                          className={`nvr-badge ${timeShiftTs === null ? "live" : "rewind"}`}
                        >
                          <span
                            className={`nvr-dot ${timeShiftTs === null ? "live" : "motion"}`}
                          />
                          {selectedCamera.toUpperCase()} //{" "}
                          {timeShiftTs === null
                            ? t.live.liveBadge
                            : `${t.live.timeShiftBadge} @ ${formatTs(timeShiftTs)}`}
                        </span>
                        <span className="nvr-badge">
                          {stats?.cameras?.[selectedCamera]?.camera_fps?.toFixed(1) || "5.0"} FPS ·
                          AI {stats?.cameras?.[selectedCamera]?.detection_fps?.toFixed(1) || "0.0"}{" "}
                          FPS
                        </span>
                      </div>
                      <div style={{ display: "flex", alignItems: "center", gap: 6, pointerEvents: "auto" }}>
                        {timeShiftTs !== null && (
                          <button
                            className="nvr-btn success"
                            onClick={() => setTimeShiftTs(null)}
                          >
                            <Play size={12} />
                            {t.live.returnToLive}
                          </button>
                        )}
                      </div>
                    </div>
                  </div>
                </div>

                {/* Right-hand camera dock in Focus mode */}
                {liveLayout === "focus" && (
                  <div className="nvr-focus-dock">
                    {cameraNames
                      .filter((c) => c !== selectedCamera)
                      .map((cam) => (
                        <div
                          key={cam}
                          className="nvr-cam-tile"
                          style={{ height: "33.33%", minHeight: 160 }}
                        >
                          <LiveMsePlayer
                            camera={cam}
                            useMainStream={false}
                            cameraConfig={config?.cameras?.[cam]}
                            showVectors={showVectors}
                            onClick={() => setSelectedCamera(cam)}
                          />
                          <div className="nvr-cam-hud-top">
                            <span className="nvr-badge live">{cam.toUpperCase()}</span>
                            <span className="nvr-badge">
                              {stats?.cameras?.[cam]?.camera_fps?.toFixed(1) || "5.0"} FPS
                            </span>
                          </div>
                        </div>
                      ))}
                  </div>
                )}
              </div>
            )}

            {/* Always-Docked Interactive Timeline & Segment Export Bar */}
            <TimelineScrubber
              camera={selectedCamera}
              cameras={cameraNames}
              onSelectCamera={(cam) => setSelectedCamera(cam)}
              segments={focusSegments}
              reviews={reviews}
              currentTs={timeShiftTs}
              onSeek={(ts) => setTimeShiftTs(ts)}
              onExportRange={handleQuickExport}
              titlePrefix="TIMELINE"
            />
          </>
        )}

        {/* ===================================================================
            WORKSPACE 2: MULTI-CAMERA SYNCHRONIZED TIME MACHINE (SYNC PLAYBACK)
           =================================================================== */}
        {activeTab === "sync" && (
          <>
            <div className="nvr-subbar">
              <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <span className="nvr-badge rewind">{t.sync.syncLocked}</span>
                <span className="mono" style={{ fontWeight: 700, color: "var(--text-primary)" }}>
                  {formatDateTime(syncMasterTs)}
                </span>
                <button
                  className="nvr-btn"
                  onClick={() => setSyncMasterTs((ts) => ts - 30)}
                >
                  {t.sync.jumpBack30}
                </button>
                <button
                  className="nvr-btn"
                  onClick={() => setSyncMasterTs((ts) => ts - 10)}
                >
                  {t.sync.jumpBack10}
                </button>
                <button
                  className="nvr-btn"
                  onClick={() => setSyncMasterTs((ts) => ts + 10)}
                >
                  {t.sync.jumpFwd10}
                </button>
                <button
                  className="nvr-btn"
                  onClick={() => setSyncMasterTs((ts) => ts + 30)}
                >
                  {t.sync.jumpFwd30}
                </button>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                {[1, 2, 4].map((spd) => (
                  <button
                    key={spd}
                    className={`nvr-btn mono ${syncSpeed === spd ? "active" : ""}`}
                    onClick={() => setSyncSpeed(spd)}
                  >
                    {spd}x
                  </button>
                ))}
              </div>
            </div>

            <div className="nvr-workbench">
              {/* Left Incident Jump List */}
              <div className="nvr-panel">
                <div className="nvr-panel-header">
                  <span>{t.sync.recentIncidents}</span>
                  <span>{reviews.length}</span>
                </div>
                <div className="nvr-scroll-y">
                  {reviews.slice(0, 35).map((rev) => {
                    const isSelected = Math.abs(rev.start_time - syncMasterTs) < 5;
                    return (
                      <div
                        key={rev.id}
                        onClick={() => setSyncMasterTs(Math.floor(rev.start_time))}
                        style={{
                          padding: "8px 10px",
                          borderBottom: "1px solid var(--border-hairline)",
                          background: isSelected ? "var(--bg-active)" : "transparent",
                          cursor: "pointer",
                          display: "flex",
                          alignItems: "center",
                          gap: 10,
                        }}
                      >
                        <img
                          src={rev.thumb_path.replace("/media/frigate", "")}
                          alt={rev.camera}
                          loading="lazy"
                          style={{
                            width: 68,
                            height: 40,
                            objectFit: "cover",
                            borderRadius: 2,
                            border: "1px solid var(--border-hairline)",
                          }}
                        />
                        <div style={{ flex: 1 }}>
                          <div
                            style={{
                              display: "flex",
                              justifyContent: "space-between",
                              fontWeight: 600,
                            }}
                          >
                            <span style={{ textTransform: "uppercase" }}>{rev.camera}</span>
                            <span className="mono" style={{ fontSize: 11, color: "#38bdf8" }}>
                              {formatTs(rev.start_time)}
                            </span>
                          </div>
                          <div
                            className="mono"
                            style={{ fontSize: 10.5, color: "var(--text-secondary)" }}
                          >
                            {(rev.data?.objects || ["motion"]).join(", ")} · {rev.severity}
                          </div>
                        </div>
                      </div>
                    );
                  })}
                </div>
              </div>

              {/* Right 2x2 Synchronized VOD Grid */}
              <div className="nvr-matrix-2x2">
                {cameraNames.map((cam) => (
                  <div key={cam} className="nvr-cam-tile">
                    <VodHlsPlayer
                      camera={cam}
                      startTime={syncMasterTs}
                      endTime={syncMasterTs + 120}
                      playbackRate={syncSpeed}
                      controls={false}
                    />
                    <div className="nvr-cam-hud-top">
                      <span className="nvr-badge rewind">
                        {cam.toUpperCase()} // SYNC @ {formatTs(syncMasterTs)}
                      </span>
                      <button
                        className="nvr-btn"
                        style={{ height: 22, padding: "0 7px", fontSize: 10, pointerEvents: "auto" }}
                        onClick={() => handleQuickExport(cam, syncMasterTs, syncMasterTs + 60)}
                      >
                        <Film size={11} />
                        {t.live.export60s}
                      </button>
                    </div>
                  </div>
                ))}
              </div>
            </div>

            <TimelineScrubber
              camera={selectedCamera}
              cameras={cameraNames}
              onSelectCamera={(cam) => setSelectedCamera(cam)}
              segments={focusSegments}
              reviews={reviews}
              currentTs={syncMasterTs}
              onSeek={(ts) =>
                setSyncMasterTs(ts ?? Math.floor(Date.now() / 1000) - 30)
              }
              onExportRange={handleQuickExport}
              titlePrefix="4-CH SYNC TIMELINE"
            />
          </>
        )}

        {/* ===================================================================
            WORKSPACE 3: REVIEW & TRIAGE WORKBENCH (VIRTUALIZED / WINDOWED)
           =================================================================== */}
        {activeTab === "review" && (
          <>
            <div className="nvr-subbar">
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <Filter size={13} color="var(--text-muted)" />
                <select
                  className="nvr-select mono"
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
                  className="nvr-select mono"
                  value={reviewSeverityFilter}
                  onChange={(e) => setReviewSeverityFilter(e.target.value)}
                >
                  <option value="all">{t.review.allSeverities}</option>
                  <option value="alert">{t.review.alerts}</option>
                  <option value="detection">{t.review.detections}</option>
                </select>
                <select
                  className="nvr-select mono"
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
                  className="nvr-select mono"
                  value={reviewStatusFilter}
                  onChange={(e) =>
                    setReviewStatusFilter(e.target.value as "unreviewed" | "reviewed" | "all")
                  }
                >
                  <option value="unreviewed">{t.review.unreviewedOnly}</option>
                  <option value="reviewed">{t.review.reviewedOnly}</option>
                  <option value="all">{t.review.allStatuses}</option>
                </select>
                <input
                  type="date"
                  className="nvr-input mono"
                  style={{ height: 28, fontSize: 11.5 }}
                  value={reviewDateFilter}
                  onChange={(e) => {
                    setReviewDateFilter(e.target.value);
                    if (e.target.value) setReviewStatusFilter("all");
                  }}
                  title="Filter reviews by specific historical date"
                />
                {reviewDateFilter && (
                  <button
                    className="nvr-btn mono"
                    style={{ height: 28, fontSize: 10.5 }}
                    onClick={() => setReviewDateFilter("")}
                  >
                    All Dates
                  </button>
                )}
                <span className="mono" style={{ color: "var(--text-muted)", fontSize: 11 }}>
                  {filteredReviews.length} matching (showing top 40)
                </span>
              </div>

              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <span className="mono" style={{ fontSize: 11, color: "var(--text-muted)" }}>
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
              {/* Left Windowed Review Feed (capped at 40 DOM cards for <300 total DOM nodes) */}
              <div className="nvr-panel">
                <div className="nvr-panel-header">
                  <span>REVIEW QUEUE</span>
                  {filteredReviews.length > 0 && (
                    <button
                      className="nvr-btn success"
                      style={{ height: 22, padding: "0 7px", fontSize: 10 }}
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
                        onClick={() => setActiveReview(item)}
                        style={{
                          padding: "8px 10px",
                          borderBottom: "1px solid var(--border-hairline)",
                          borderLeft:
                            item.severity === "alert"
                              ? "3px solid var(--signal-alert)"
                              : "3px solid var(--signal-motion)",
                          background: isSelected ? "var(--bg-active)" : "transparent",
                          cursor: "pointer",
                          display: "flex",
                          alignItems: "center",
                          gap: 10,
                        }}
                      >
                        <img
                          src={thumbUrl}
                          alt={item.camera}
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
                            <span
                              className="mono"
                              style={{ fontWeight: 700, textTransform: "uppercase" }}
                            >
                              {item.camera}
                            </span>
                            <span className="mono" style={{ fontSize: 11, color: "var(--text-secondary)" }}>
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
                                fontSize: 10.5,
                                color: item.severity === "alert" ? "#fca5a5" : "#fcd34d",
                                textTransform: "uppercase",
                              }}
                            >
                              {(item.data?.objects || ["motion"]).join(", ")}
                            </span>
                            <div style={{ display: "flex", gap: 4 }}>
                              {!item.has_been_reviewed && (
                                <button
                                  className="nvr-btn success"
                                  style={{ height: 20, padding: "0 6px", fontSize: 10 }}
                                  onClick={(e) => {
                                    e.stopPropagation();
                                    handleMarkReviews([item.id], true);
                                  }}
                                >
                                  <Check size={10} />
                                </button>
                              )}
                              <button
                                className="nvr-btn danger"
                                style={{ height: 20, padding: "0 6px", fontSize: 10 }}
                                onClick={(e) => {
                                  e.stopPropagation();
                                  handleDeleteReviews([item.id]);
                                }}
                              >
                                <Trash2 size={10} />
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

              {/* Right Review Theater Player */}
              <div className="nvr-focus-stage">
                {activeReview ? (
                  <>
                    <div className="nvr-cam-tile" style={{ flex: 1 }}>
                      <VodHlsPlayer
                        camera={activeReview.camera}
                        startTime={activeReview.start_time - 4}
                        endTime={
                          (activeReview.end_time || activeReview.start_time + 20) + 6
                        }
                        fallbackPreviewId={activeReview.id}
                        playbackRate={playbackSpeed}
                      />
                      <div className="nvr-cam-hud-top">
                        <span className="nvr-badge alert">
                          {activeReview.camera.toUpperCase()} //{" "}
                          {formatDateTime(activeReview.start_time)}
                        </span>
                        <span className="nvr-badge">
                          {(activeReview.data?.objects || ["person"]).join(", ").toUpperCase()}
                        </span>
                      </div>
                    </div>
                    <div className="nvr-subbar" style={{ borderTop: "1px solid var(--border-hairline)" }}>
                      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                        <span className="mono" style={{ color: "var(--text-secondary)" }}>
                          ID: {activeReview.id}
                        </span>
                        <span className="mono" style={{ color: "var(--text-muted)" }}>
                          ({formatRelTime(activeReview.start_time)})
                        </span>
                      </div>
                      <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                        <button
                          className="nvr-btn"
                          onClick={() => {
                            setSyncMasterTs(Math.floor(activeReview.start_time));
                            switchTab("sync");
                          }}
                        >
                          <Layers size={12} />
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
                          <Film size={12} />
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
        )}

        {/* ===================================================================
            WORKSPACE 4: OBJECT EXPLORE
           =================================================================== */}
        {activeTab === "explore" && (
          <>
            <div className="nvr-subbar">
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <input
                  className="nvr-input"
                  style={{ width: 240 }}
                  placeholder={t.explore.searchPlaceholder}
                  value={exploreQuery}
                  onChange={(e) => setExploreQuery(e.target.value)}
                />
                <select
                  className="nvr-select mono"
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
                  className="nvr-select mono"
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
                  <Star size={12} />
                  {t.explore.favoritesOnly}
                </button>
                <input
                  type="date"
                  className="nvr-input mono"
                  style={{ height: 28, fontSize: 11.5 }}
                  value={reviewDateFilter}
                  onChange={(e) => setReviewDateFilter(e.target.value)}
                  title="Filter tracked objects by specific historical date"
                />
                {reviewDateFilter && (
                  <button
                    className="nvr-btn mono"
                    style={{ height: 28, fontSize: 10.5 }}
                    onClick={() => setReviewDateFilter("")}
                  >
                    All Dates
                  </button>
                )}
              </div>
              <span className="mono" style={{ fontSize: 11, color: "var(--text-secondary)" }}>
                {filteredEvents.length} tracked object events
              </span>
            </div>

            <div className="nvr-workbench">
              {/* Left Object Grid */}
              <div className="nvr-panel">
                <div className="nvr-panel-header">
                  <span>TRACKED OBJECTS</span>
                  <span>TOP 50</span>
                </div>
                <div className="nvr-scroll-y">
                  {filteredEvents.slice(0, 50).map((ev) => {
                    const topScore = Math.round(
                      ((ev.top_score ?? ev.data?.top_score ?? ev.score ?? 0.85) as number) * 100
                    );
                    const isSelected = selectedEvent?.id === ev.id;
                    return (
                      <div
                        key={ev.id}
                        onClick={() => setSelectedEvent(ev)}
                        style={{
                          padding: "8px 10px",
                          borderBottom: "1px solid var(--border-hairline)",
                          background: isSelected ? "var(--bg-active)" : "transparent",
                          cursor: "pointer",
                          display: "flex",
                          alignItems: "center",
                          gap: 10,
                        }}
                      >
                        <img
                          src={`/api/events/${encodeURIComponent(ev.id)}/thumbnail.webp`}
                          alt={ev.label}
                          loading="lazy"
                          style={{
                            width: 80,
                            height: 46,
                            objectFit: "contain",
                            background: "#050608",
                            borderRadius: 3,
                            border: "1px solid var(--border-hairline)",
                          }}
                        />
                        <div style={{ flex: 1 }}>
                          <div style={{ display: "flex", justifyContent: "space-between" }}>
                            <span
                              className="mono"
                              style={{ fontWeight: 700, textTransform: "uppercase" }}
                            >
                              {ev.label} · {topScore}%
                            </span>
                            <span
                              className="mono"
                              style={{ fontSize: 11, color: "var(--text-secondary)" }}
                            >
                              {ev.camera.toUpperCase()}
                            </span>
                          </div>
                          <div
                            className="mono"
                            style={{ fontSize: 10.5, color: "var(--text-muted)", marginTop: 3 }}
                          >
                            {formatDateTime(ev.start_time)} ·{" "}
                            {ev.end_time
                              ? `${Math.max(1, Math.round(ev.end_time - ev.start_time))}s`
                              : t.explore.inProgress}
                          </div>
                        </div>
                      </div>
                    );
                  })}
                </div>
              </div>

              {/* Right Object Detail Inspector */}
              <div className="nvr-focus-stage">
                {selectedEvent ? (
                  <>
                    <div className="nvr-subbar">
                      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                        <span className="nvr-badge alert">
                          {selectedEvent.label.toUpperCase()}{" "}
                          {Math.round(
                            ((selectedEvent.top_score ??
                              selectedEvent.data?.top_score ??
                              0.88) as number) * 100
                          )}
                          %
                        </span>
                        <span className="mono">
                          {selectedEvent.camera.toUpperCase()} · ID {selectedEvent.id}
                        </span>
                      </div>
                      <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
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
                          <Star size={12} />
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
                          <AlertTriangle size={12} />
                          {t.explore.falsePositive}
                        </button>
                        <button
                          className="nvr-btn"
                          onClick={() => {
                            setSyncMasterTs(Math.floor(selectedEvent.start_time));
                            switchTab("sync");
                          }}
                        >
                          <Layers size={12} />
                          4-Cam Sync
                        </button>
                        <button
                          className="nvr-btn danger"
                          onClick={() => handleEventDelete(selectedEvent.id)}
                        >
                          <Trash2 size={12} />
                        </button>
                      </div>
                    </div>

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
        )}

        {/* ===================================================================
            WORKSPACE 5: EXPORTS & CASES
           =================================================================== */}
        {activeTab === "exports" && (
          <>
            <div className="nvr-subbar">
              <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <span className="mono" style={{ fontWeight: 700 }}>
                  {t.exports.createTitle}:
                </span>
                <select
                  className="nvr-select mono"
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
                  className="nvr-select mono"
                  value={exportPresetMin}
                  onChange={(e) => setExportPresetMin(Number(e.target.value))}
                >
                  <option value={1}>{t.exports.last1m}</option>
                  <option value={5}>{t.exports.last5m}</option>
                  <option value={15}>{t.exports.last15m}</option>
                </select>
                <input
                  className="nvr-input"
                  placeholder={t.exports.customName}
                  value={exportLabel}
                  onChange={(e) => setExportLabel(e.target.value)}
                />
                <button
                  className="nvr-btn primary"
                  onClick={async () => {
                    const now = Math.floor(Date.now() / 1000);
                    await handleQuickExport(
                      exportCam,
                      now - exportPresetMin * 60,
                      now,
                      exportLabel || `${exportCam}_${exportPresetMin}m`
                    );
                    const r = await fetch("/api/exports", { credentials: "include" });
                    if (r.ok) setExportsList(await r.json());
                  }}
                >
                  <Film size={12} />
                  {t.exports.generateBtn}
                </button>
              </div>
            </div>

            <div className="nvr-workbench">
              <div className="nvr-panel">
                <div className="nvr-panel-header">
                  <span>{t.exports.archiveTitle}</span>
                  <span>{exportsList.length}</span>
                </div>
                <div className="nvr-scroll-y">
                  {exportsList.map((ex) => (
                    <div
                      key={ex.id}
                      onClick={() => setActiveExportPreview(ex)}
                      style={{
                        padding: "10px 12px",
                        borderBottom: "1px solid var(--border-hairline)",
                        background:
                          activeExportPreview?.id === ex.id ? "var(--bg-active)" : "transparent",
                        cursor: "pointer",
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                      }}
                    >
                      <div>
                        <div className="mono" style={{ fontWeight: 700 }}>
                          {ex.name}
                        </div>
                        <div
                          className="mono"
                          style={{ fontSize: 11, color: "var(--text-secondary)" }}
                        >
                          {ex.camera.toUpperCase()} · {formatDateTime(ex.date)}
                        </div>
                      </div>
                      <div style={{ display: "flex", gap: 6 }}>
                        <a
                          href={ex.video_path.replace("/media/frigate", "")}
                          download={`${ex.id}.mp4`}
                          className="nvr-btn"
                          style={{ textDecoration: "none" }}
                          onClick={(e) => e.stopPropagation()}
                        >
                          <Download size={12} />
                        </a>
                        <button
                          className="nvr-btn danger"
                          onClick={async (e) => {
                            e.stopPropagation();
                            await fetch(`/api/export/${encodeURIComponent(ex.id)}`, {
                              method: "DELETE",
                              credentials: "include",
                            });
                            setExportsList((prev) => prev.filter((i) => i.id !== ex.id));
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
                {activeExportPreview ? (
                  <div className="nvr-cam-tile" style={{ flex: 1 }}>
                    <video
                      src={activeExportPreview.video_path.replace("/media/frigate", "")}
                      controls
                      autoPlay
                      muted
                      playsInline
                      style={{ width: "100%", height: "100%", objectFit: "contain" }}
                    />
                    <div className="nvr-cam-hud-top">
                      <span className="nvr-badge live">
                        ARCHIVE PREVIEW // {activeExportPreview.name}
                      </span>
                      <button
                        className="nvr-btn"
                        style={{ height: 22, padding: "0 8px", fontSize: 10, pointerEvents: "auto" }}
                        onClick={() => setActiveExportPreview(null)}
                      >
                        Close Preview & Use Timeline Clipper
                      </button>
                    </div>
                  </div>
                ) : (
                  <div className="nvr-cam-tile" style={{ flex: 1 }}>
                    {timeShiftTs === null ? (
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
                    <div className="nvr-cam-hud-top">
                      <span className="nvr-badge rewind">
                        CLIPPER STAGE // {exportCam.toUpperCase()}
                      </span>
                      <span className="nvr-badge">
                        USE TIMELINE BELOW TO SCRUB OR SELECT [ IN ... OUT ] RANGE TO EXPORT
                      </span>
                    </div>
                  </div>
                )}

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
                  onExportRange={handleQuickExport}
                  titlePrefix="EXPORT TIMELINE CLIPPER"
                />
              </div>
            </div>
          </>
        )}

        {/* ===================================================================
            WORKSPACE 6: VECTOR ZONE / MASK INSPECTOR & YAML CONFIG STUDIO
           =================================================================== */}
        {activeTab === "zones" && (
          <>
            <div className="nvr-subbar">
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <button
                  className={`nvr-btn ${zonesSubTab === "visual" ? "active" : ""}`}
                  onClick={() => setZonesSubTab("visual")}
                >
                  <Eye size={13} />
                  {t.zones.visualTab}
                </button>
                <button
                  className={`nvr-btn ${zonesSubTab === "yaml" ? "active" : ""}`}
                  onClick={() => setZonesSubTab("yaml")}
                >
                  <Terminal size={13} />
                  {t.zones.yamlTab}
                </button>

                {zonesSubTab === "visual" && (
                  <>
                    <span style={{ color: "var(--border-strong)", margin: "0 4px" }}>|</span>
                    {cameraNames.map((cam) => (
                      <button
                        key={cam}
                        className={`nvr-btn mono ${inspectorCam === cam ? "active" : ""}`}
                        onClick={() => setInspectorCam(cam)}
                        style={{ textTransform: "uppercase" }}
                      >
                        {cam}
                      </button>
                    ))}
                  </>
                )}
              </div>

              {zonesSubTab === "yaml" && (
                <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                  <button
                    className="nvr-btn"
                    onClick={() => {
                      navigator.clipboard?.writeText(rawYaml);
                      notify("Copied config.yml to clipboard");
                    }}
                  >
                    <Copy size={12} />
                    {t.zones.copyYaml}
                  </button>
                  <button
                    className="nvr-btn primary"
                    onClick={() => handleSaveConfig("saveonly")}
                  >
                    <Save size={12} />
                    {t.zones.saveOnly}
                  </button>
                  <button
                    className="nvr-btn success"
                    onClick={() => handleSaveConfig("restart")}
                  >
                    <RefreshCw size={12} />
                    {t.zones.saveAndRestart}
                  </button>
                </div>
              )}
            </div>

            {zonesSubTab === "visual" ? (
              <div className="nvr-workbench">
                <div className="nvr-panel">
                  <div className="nvr-panel-header">
                    <span>{inspectorCam.toUpperCase()} // POLYGON VECTORS</span>
                  </div>
                  <div className="nvr-scroll-y" style={{ padding: 12 }}>
                    <div
                      className="mono"
                      style={{
                        fontSize: 11,
                        fontWeight: 700,
                        color: "#fbbf24",
                        marginBottom: 6,
                      }}
                    >
                      INTERACTIVE POLYGON BUILDER ({draftPoints.length} vertices)
                    </div>
                    <div
                      style={{
                        padding: 8,
                        background: "var(--bg-canvas)",
                        border: "1px solid rgba(245, 158, 11, 0.4)",
                        borderRadius: 3,
                        marginBottom: 12,
                        display: "flex",
                        flexDirection: "column",
                        gap: 6,
                      }}
                    >
                      <div style={{ fontSize: 11, color: "var(--text-secondary)" }}>
                        Click anywhere on the camera preview to plot normalized polygon vertices.
                      </div>
                      <div
                        className="mono"
                        style={{
                          fontSize: 10.5,
                          color: "#fcd34d",
                          wordBreak: "break-all",
                          padding: "4px 6px",
                          background: "#050608",
                          borderRadius: 2,
                        }}
                      >
                        {draftPoints.length > 0
                          ? draftPoints
                              .map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`)
                              .join(",")
                          : "0.010,0.010,0.340,0.010,0.340,0.080,0.010,0.080"}
                      </div>
                      <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
                        <button
                          className="nvr-btn"
                          style={{ height: 22, fontSize: 10 }}
                          onClick={() =>
                            setDraftPoints([
                              [0.01, 0.01],
                              [0.34, 0.01],
                              [0.34, 0.08],
                              [0.01, 0.08],
                            ])
                          }
                        >
                          Preset: OSD Clock Mask
                        </button>
                        <button
                          className="nvr-btn primary"
                          style={{ height: 22, fontSize: 10 }}
                          onClick={() => {
                            const coords =
                              draftPoints.length > 0
                                ? draftPoints
                                    .map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`)
                                    .join(",")
                                : "0.010,0.010,0.340,0.010,0.340,0.080,0.010,0.080";
                            navigator.clipboard?.writeText(coords);
                            notify("Copied normalized polygon coordinates to clipboard");
                          }}
                        >
                          <Copy size={10} />
                          Copy Coords
                        </button>
                        <button
                          className="nvr-btn danger"
                          style={{ height: 22, fontSize: 10 }}
                          onClick={() => setDraftPoints([])}
                        >
                          Clear
                        </button>
                      </div>
                    </div>

                    <div
                      className="mono"
                      style={{
                        fontSize: 11,
                        fontWeight: 700,
                        color: "#fca5a5",
                        marginBottom: 6,
                      }}
                    >
                      {t.zones.motionMasks} (
                      {extractMaskPolygons(config?.cameras?.[inspectorCam]?.motion?.mask).length})
                    </div>
                    {extractMaskPolygons(config?.cameras?.[inspectorCam]?.motion?.mask).map(
                      (m, idx) => (
                      <div
                        key={idx}
                        className="mono"
                        style={{
                          padding: 8,
                          background: "var(--bg-canvas)",
                          border: "1px solid rgba(239, 68, 68, 0.35)",
                          borderRadius: 3,
                          fontSize: 10.5,
                          marginBottom: 8,
                          wordBreak: "break-all",
                        }}
                      >
                        MASK #{idx + 1}: {m}
                      </div>
                    ))}

                    <div
                      className="mono"
                      style={{
                        fontSize: 11,
                        fontWeight: 700,
                        color: "#38bdf8",
                        marginTop: 14,
                        marginBottom: 6,
                      }}
                    >
                      {t.zones.detectionZones} (
                      {Object.keys(config?.cameras?.[inspectorCam]?.zones || {}).length})
                    </div>
                    {Object.entries(config?.cameras?.[inspectorCam]?.zones || {}).map(
                      ([zName, zCfg]) => (
                        <div
                          key={zName}
                          className="mono"
                          style={{
                            padding: 8,
                            background: "var(--bg-canvas)",
                            border: "1px solid rgba(14, 165, 233, 0.35)",
                            borderRadius: 3,
                            fontSize: 10.5,
                            marginBottom: 8,
                            wordBreak: "break-all",
                          }}
                        >
                          <strong>{zName.toUpperCase()}</strong>: {zCfg.coordinates}
                        </div>
                      )
                    )}

                    <div
                      className="mono"
                      style={{
                        fontSize: 11,
                        fontWeight: 700,
                        color: "#34d399",
                        marginTop: 14,
                        marginBottom: 6,
                      }}
                    >
                      {t.zones.streamProbe}
                    </div>
                    <pre
                      className="mono"
                      style={{
                        padding: 8,
                        background: "var(--bg-canvas)",
                        border: "1px solid var(--border-hairline)",
                        borderRadius: 3,
                        fontSize: 10.5,
                        overflowX: "auto",
                      }}
                    >
                      {JSON.stringify(ffprobeInfo, null, 2)}
                    </pre>
                  </div>
                </div>

                <div className="nvr-focus-stage">
                  <div className="nvr-cam-tile" style={{ flex: 1 }}>
                    <LiveMsePlayer
                      camera={inspectorCam}
                      useMainStream={false}
                      cameraConfig={config?.cameras?.[inspectorCam]}
                      showVectors={true}
                      draftPoints={draftPoints}
                      onPlotPoint={(nx, ny) => setDraftPoints((prev) => [...prev, [nx, ny]])}
                    />
                    <div className="nvr-cam-hud-top">
                      <span className="nvr-badge live">
                        {inspectorCam.toUpperCase()} // VECTOR OVERLAY & DRAWER ACTIVE
                      </span>
                      <span className="nvr-badge">
                        CLICK VIDEO TO PLOT VERTICES · RED = MASK · CYAN = ZONE · AMBER = DRAFT
                      </span>
                    </div>
                  </div>
                </div>
              </div>
            ) : (
              <div style={{ flex: 1, display: "flex", flexDirection: "column" }}>
                <textarea
                  className="nvr-code-editor"
                  value={rawYaml}
                  onChange={(e) => setRawYaml(e.target.value)}
                  spellCheck={false}
                  aria-label="config.yml editor"
                />
              </div>
            )}
          </>
        )}

        {/* ===================================================================
            WORKSPACE 7: SYSTEM TELEMETRY (ZERO-REFLOW CANVAS) & LIVE LOGS
           =================================================================== */}
        {activeTab === "system" && (
          <div className="nvr-workbench" style={{ gridTemplateColumns: "1.1fr 1fr" }}>
            {/* Left Telemetry Column */}
            <div className="nvr-panel">
              <div className="nvr-panel-header">
                <span>
                  HARDWARE & PIPELINE TELEMETRY (UPTIME{" "}
                  {formatUptime(stats?.service?.uptime || 0)})
                </span>
                <span>{stats?.service?.version || "0.18.2"}</span>
              </div>
              <div className="nvr-scroll-y" style={{ padding: 12, display: "flex", flexDirection: "column", gap: 12 }}>
                {/* 3 Canvas Sparklines */}
                <div
                  style={{
                    display: "grid",
                    gridTemplateColumns: "repeat(3, 1fr)",
                    gap: 10,
                  }}
                >
                  <div
                    style={{
                      background: "var(--bg-canvas)",
                      border: "1px solid var(--border-hairline)",
                      borderRadius: 3,
                      padding: 10,
                    }}
                  >
                    <div
                      className="mono"
                      style={{
                        display: "flex",
                        justifyContent: "space-between",
                        fontSize: 11,
                        marginBottom: 6,
                      }}
                    >
                      <span style={{ color: "var(--text-secondary)" }}>OPENVINO LATENCY</span>
                      <span style={{ color: "#38bdf8", fontWeight: 700 }}>{inferMs} ms</span>
                    </div>
                    <CanvasSparkline
                      values={statsHistory.map(
                        (s) => s.detectors?.detector01?.inference_speed || 7.0
                      )}
                      color="#0ea5e9"
                      maxVal={20}
                    />
                  </div>

                  <div
                    style={{
                      background: "var(--bg-canvas)",
                      border: "1px solid var(--border-hairline)",
                      borderRadius: 3,
                      padding: 10,
                    }}
                  >
                    <div
                      className="mono"
                      style={{
                        display: "flex",
                        justifyContent: "space-between",
                        fontSize: 11,
                        marginBottom: 6,
                      }}
                    >
                      <span style={{ color: "var(--text-secondary)" }}>SYSTEM CPU</span>
                      <span style={{ color: "#10b981", fontWeight: 700 }}>{sysCpu}%</span>
                    </div>
                    <CanvasSparkline
                      values={statsHistory.map((s) =>
                        parseFloat(s.cpu_usages?.["frigate.full_system"]?.cpu || "6.5")
                      )}
                      color="#10b981"
                      maxVal={100}
                    />
                  </div>

                  <div
                    style={{
                      background: "var(--bg-canvas)",
                      border: "1px solid var(--border-hairline)",
                      borderRadius: 3,
                      padding: 10,
                    }}
                  >
                    <div
                      className="mono"
                      style={{
                        display: "flex",
                        justifyContent: "space-between",
                        fontSize: 11,
                        marginBottom: 6,
                      }}
                    >
                      <span style={{ color: "var(--text-secondary)" }}>MEMORY RSS</span>
                      <span style={{ color: "#f59e0b", fontWeight: 700 }}>{sysMem}%</span>
                    </div>
                    <CanvasSparkline
                      values={statsHistory.map((s) =>
                        parseFloat(s.cpu_usages?.["frigate.full_system"]?.mem || "26.0")
                      )}
                      color="#f59e0b"
                      maxVal={100}
                    />
                  </div>
                </div>

                {/* Per-Camera Pipeline Table */}
                <div
                  style={{
                    background: "var(--bg-canvas)",
                    border: "1px solid var(--border-hairline)",
                    borderRadius: 3,
                  }}
                >
                  <div className="nvr-panel-header">
                    <span>{t.system.cameraHealth}</span>
                  </div>
                  <table className="nvr-table mono">
                    <thead>
                      <tr>
                        <th>Channel</th>
                        <th>Cam FPS</th>
                        <th>Proc FPS</th>
                        <th>AI Detect FPS</th>
                        <th>FFmpeg CPU</th>
                        <th>Capture CPU</th>
                      </tr>
                    </thead>
                    <tbody>
                      {cameraNames.map((cam) => {
                        const cs = stats?.cameras?.[cam];
                        return (
                          <tr key={cam}>
                            <td style={{ fontWeight: 700, textTransform: "uppercase" }}>{cam}</td>
                            <td>{cs?.camera_fps?.toFixed(1) || "5.0"}</td>
                            <td>{cs?.process_fps?.toFixed(1) || "5.0"}</td>
                            <td style={{ color: "#38bdf8" }}>
                              {cs?.detection_fps?.toFixed(1) || "0.0"}
                            </td>
                            <td>{cs?.ffmpeg_cpu || "1.2"}%</td>
                            <td>{cs?.capture_cpu || "1.0"}%</td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>

                {/* Storage Volumes */}
                <div
                  style={{
                    background: "var(--bg-canvas)",
                    border: "1px solid var(--border-hairline)",
                    borderRadius: 3,
                  }}
                >
                  <div className="nvr-panel-header">
                    <span>{t.system.storageVolumes}</span>
                  </div>
                  <table className="nvr-table mono">
                    <thead>
                      <tr>
                        <th>Mount Path</th>
                        <th>FS</th>
                        <th>Used</th>
                        <th>Total</th>
                        <th>Utilization</th>
                      </tr>
                    </thead>
                    <tbody>
                      {Object.entries(stats?.service?.storage || {}).map(([mPath, st]) => {
                        const pct = Math.min(
                          100,
                          Math.round((st.used / Math.max(1, st.total)) * 100)
                        );
                        return (
                          <tr key={mPath}>
                            <td>{mPath}</td>
                            <td>{st.mount_type}</td>
                            <td>{(st.used / 1024).toFixed(1)} GB</td>
                            <td>{(st.total / 1024).toFixed(1)} GB</td>
                            <td>{pct}%</td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
              </div>
            </div>

            {/* Right Real-Time Log Stream Column */}
            <div className="nvr-panel">
              <div className="nvr-panel-header">
                <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                  <span>{t.system.liveLogs}</span>
                  {(["frigate", "go2rtc", "nginx"] as const).map((svc) => (
                    <button
                      key={svc}
                      className={`nvr-btn mono ${logService === svc ? "active" : ""}`}
                      style={{ height: 22, padding: "0 7px", fontSize: 10 }}
                      onClick={() => setLogService(svc)}
                    >
                      {svc.toUpperCase()}
                    </button>
                  ))}
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                  <input
                    className="nvr-input"
                    style={{ height: 22, width: 130, fontSize: 11 }}
                    placeholder="Filter logs..."
                    value={logSearch}
                    onChange={(e) => setLogSearch(e.target.value)}
                  />
                  <button
                    className={`nvr-btn ${logPaused ? "active" : ""}`}
                    style={{ height: 22, padding: "0 7px", fontSize: 10 }}
                    onClick={() => setLogPaused((p) => !p)}
                  >
                    {logPaused ? <Play size={11} /> : <Pause size={11} />}
                    {logPaused ? t.system.resumeStream : t.system.pauseStream}
                  </button>
                  <button
                    className="nvr-btn"
                    style={{ height: 22, padding: "0 7px", fontSize: 10 }}
                    onClick={() => {
                      navigator.clipboard?.writeText(logLines.join("\n"));
                      notify("Copied service logs to clipboard");
                    }}
                  >
                    <Copy size={11} />
                  </button>
                </div>
              </div>
              <div
                className="nvr-scroll-y mono"
                style={{
                  background: "#05070a",
                  padding: "8px 10px",
                  fontSize: 11,
                  lineHeight: 1.5,
                  userSelect: "text",
                }}
              >
                {logLines
                  .filter((l) =>
                    logSearch ? l.toLowerCase().includes(logSearch.toLowerCase()) : true
                  )
                  .map((line, idx) => (
                    <div
                      key={idx}
                      style={{
                        padding: "2px 0",
                        borderBottom: "1px solid rgba(255,255,255,0.03)",
                        color: line.includes("ERR") || line.includes("WARN")
                          ? "#fcd34d"
                          : "#cbd5e1",
                        wordBreak: "break-all",
                      }}
                    >
                      {line}
                    </div>
                  ))}
              </div>
            </div>
          </div>
        )}
      </main>
    </div>
  );
};
