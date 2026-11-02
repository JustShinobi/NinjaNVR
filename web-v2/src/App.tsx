import React, { useCallback, useEffect, useMemo, useState } from "react";
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
import { WorkspaceTab, formatTs } from "./utils/format";
import { useNvrStore } from "./store/useNvrStore";
import { LiveWorkspace } from "./views/LiveWorkspace";
import { SyncWorkspace } from "./views/SyncWorkspace";
import { ReviewWorkspace } from "./views/ReviewWorkspace";
import { ExploreWorkspace } from "./views/ExploreWorkspace";
import { ExportsWorkspace } from "./views/ExportsWorkspace";
import { ZonesWorkspace } from "./views/ZonesWorkspace";
import { SystemWorkspace } from "./views/SystemWorkspace";

export const App: React.FC = () => {
  const { t } = useI18n();
  const activeTab = useNvrStore((s) => s.activeTab);
  const setActiveTab = useNvrStore((s) => s.setActiveTab);
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

  const [authChecked, setAuthChecked] = useState(false);
  const [authNeeded, setAuthNeeded] = useState(false);
  const [, setUsername] = useState("admin");
  const [loginUser, setLoginUser] = useState("admin");
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
      const [cfgRes, statsRes, histRes, revRes, expRes] = await Promise.all([
        fetch("/api/config", { credentials: "include" }),
        fetch("/api/stats", { credentials: "include" }),
        fetch("/api/stats/history", { credentials: "include" }),
        fetch(revUrl, { credentials: "include" }),
        fetch("/api/exports", { credentials: "include" }),
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
        if (cams.length > 0 && !cams.includes(inspectorCam)) {
          setInspectorCam(cams[0]);
        }
        if (cams.length > 0 && !cams.includes(exportCam)) {
          setExportCam(cams[0]);
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
      if (expRes.ok) {
        const expJson: ExportItem[] = await expRes.json();
        if (Array.isArray(expJson)) {
          setExportsList(expJson);
        }
      }
    } catch {
      // network retry on next tick
    }
  }, [
    selectedCamera,
    inspectorCam,
    exportCam,
    activeReview,
    reviewDateFilter,
    setConfig,
    setSelectedCamera,
    setStats,
    setStatsHistory,
  ]);

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
  }, [authChecked, authNeeded, setWsOnline, setCameraActivity]);

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
    fetch(`/api/${encodeURIComponent(timelineCam)}/recordings/summary`, {
      credentials: "include",
    })
      .then((r) => (r.ok ? r.json() : []))
      .then((days) => {
        if (Array.isArray(days)) setGlobalDaysSummary(days);
      })
      .catch(() => {});
  }, [authChecked, authNeeded, timelineCam, stats?.service?.last_updated]);

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
      const s = Math.floor(startSec);
      const e = Math.ceil(Math.max(endSec, startSec + 5));
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
          videoUrl: data.video_url || `/exports/${camera}_${s}_${e}.mp4`,
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
    const res = await fetch(`/api/config/save?save_option=${saveOption}`, {
      method: "POST",
      headers: { "Content-Type": "text/plain" },
      credentials: "include",
      body: rawYaml,
    });
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
  }, [
    activeTab,
    activeReview,
    filteredReviews,
    switchTab,
    handleMarkReviews,
    handleQuickExport,
    setPlaybackSpeed,
  ]);

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
            <div className="nvr-brand-mark" />
            <div>
              <div className="nvr-brand-title">
                {t.brand.name} // {t.brand.release}
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
          <button
            type="submit"
            className="nvr-btn primary"
            style={{ height: 34 }}
            disabled={loginBusy}
          >
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
      <header className="nvr-header">
        <div className="nvr-brand">
          <div className="nvr-brand-mark" />
          <div>
            <div style={{ display: "flex", alignItems: "center", gap: 7 }}>
              <span className="nvr-brand-title">{t.brand.name}</span>
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
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
        </nav>

        <div className="nvr-header-right">
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
            title="Inspect System Telemetry & OpenVINO Detector"
          >
            <span className="nvr-dot live" />
            <span>CPU {sysCpu}%</span>
            <span className="nvr-divider nvr-telemetry-ext" aria-hidden="true" />
            <span className="nvr-telemetry-ext">MEM {sysMem}%</span>
            <span className="nvr-divider" aria-hidden="true" />
            <span>OV {inferMs}ms</span>
            <span className="nvr-divider" aria-hidden="true" />
            <span>{totalFps} FPS</span>
          </button>

          <a
            href="/api/ui/switch?target=v1"
            className="nvr-btn"
            title={t.brand.classicSwitchHint}
            style={{ textDecoration: "none" }}
          >
            {t.brand.classicSwitch}
          </a>
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

        {activeTab === "zones" && (
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

        {activeTab === "system" && (
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
