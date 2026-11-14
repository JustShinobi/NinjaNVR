import type { ShareLinkItem } from "../types/nvr";

export type ShareLinkStatus = "active" | "revoked" | "expired" | "exhausted";

function resolveNowSec(nowSec?: number): number {
  if (typeof nowSec === "number" && Number.isFinite(nowSec)) {
    return nowSec;
  }
  return Math.floor(Date.now() / 1000);
}

export interface ShareLinkRotationPlan {
  expiresInSec: number;
  remainingMaxViews: number | null;
  revokeOldId: string;
}

export function computeShareLinkStatus(
  link: ShareLinkItem,
  nowSec?: number
): ShareLinkStatus {
  if (link.revoked) {
    return "revoked";
  }
  const now = resolveNowSec(nowSec);
  if (!Number.isFinite(link.expires_at) || link.expires_at <= now) {
    return "expired";
  }
  const safeAccessCount =
    typeof link.access_count === "number" && Number.isFinite(link.access_count)
      ? Math.max(0, Math.floor(link.access_count))
      : 0;
  if (
    typeof link.max_views === "number" &&
    Number.isFinite(link.max_views) &&
    link.max_views > 0 &&
    safeAccessCount >= link.max_views
  ) {
    return "exhausted";
  }
  return "active";
}

export function computeShareLinkRotationParams(
  link: ShareLinkItem,
  nowSec?: number
): ShareLinkRotationPlan | null {
  if (computeShareLinkStatus(link, nowSec) !== "active") {
    return null;
  }
  const now = resolveNowSec(nowSec);
  const expiresInSec = Math.floor(link.expires_at - now);
  if (expiresInSec <= 0) {
    return null;
  }
  let remainingMaxViews: number | null = null;
  if (
    typeof link.max_views === "number" &&
    Number.isFinite(link.max_views) &&
    link.max_views > 0
  ) {
    const rawUsed =
      typeof link.access_count === "number" && Number.isFinite(link.access_count)
        ? link.access_count
        : 0;
    const used = Math.max(0, Math.floor(rawUsed));
    const rem = Math.max(1, Math.floor(link.max_views) - used);
    remainingMaxViews = rem;
  }
  return {
    expiresInSec,
    remainingMaxViews,
    revokeOldId: link.id,
  };
}

export function formatShareLinkRemainingTtl(
  expiresAtSec: number,
  nowSec?: number
): string {
  const now = resolveNowSec(nowSec);
  if (!Number.isFinite(expiresAtSec) || expiresAtSec <= now) {
    return "Expired";
  }
  const diff = Math.floor(expiresAtSec - now);
  if (diff <= 0) {
    return "Expired";
  }
  if (diff >= 86400) {
    const days = Math.floor(diff / 86400);
    const hours = Math.floor((diff % 86400) / 3600);
    return hours > 0 ? `${days}d ${hours}h` : `${days}d`;
  }
  if (diff >= 3600) {
    const hours = Math.floor(diff / 3600);
    const minutes = Math.floor((diff % 3600) / 60);
    return minutes > 0 ? `${hours}h ${minutes}m` : `${hours}h`;
  }
  if (diff >= 60) {
    const minutes = Math.floor(diff / 60);
    const seconds = diff % 60;
    return seconds > 0 ? `${minutes}m ${seconds}s` : `${minutes}m`;
  }
  return `${diff}s`;
}

export function filterActiveAndRecentShareLinks(
  links: ShareLinkItem[],
  includeInactive: boolean,
  nowSec?: number
): ShareLinkItem[] {
  if (!Array.isArray(links) || links.length === 0) {
    return [];
  }
  const now = resolveNowSec(nowSec);
  const filtered = includeInactive
    ? [...links]
    : links.filter((link) => computeShareLinkStatus(link, now) === "active");

  return filtered.sort((a, b) => {
    const aCreated = Number.isFinite(a.created_at) ? a.created_at : 0;
    const bCreated = Number.isFinite(b.created_at) ? b.created_at : 0;
    if (bCreated !== aCreated) {
      return bCreated - aCreated;
    }
    return b.id.localeCompare(a.id);
  });
}

export function isValidShareTokenSegment(seg: string): boolean {
  return (
    seg.length > 0 &&
    seg.length <= 512 &&
    seg !== "." &&
    !seg.includes("..") &&
    /^[A-Za-z0-9._-]+$/.test(seg)
  );
}

export function buildSharePublicUrl(
  origin: string,
  tokenOrUrl: string
): string {
  const rawTarget = (tokenOrUrl || "").trim();
  if (!rawTarget || /[\u0000-\u0020\u007f\\]/.test(rawTarget)) {
    return "";
  }
  if (
    rawTarget.startsWith("//") ||
    rawTarget.includes("..") ||
    /%2e/i.test(rawTarget) ||
    /%2f/i.test(rawTarget) ||
    /%5c/i.test(rawTarget) ||
    /%00/i.test(rawTarget)
  ) {
    return "";
  }

  const cleanOrigin = (origin || "").trim().replace(/\/+$/, "");
  let normalizedOrigin = "";
  if (cleanOrigin) {
    if (/[\u0000-\u0020\u007f\\]/.test(cleanOrigin)) {
      return "";
    }
    try {
      const parsedOrigin = new URL(cleanOrigin);
      if (
        (parsedOrigin.protocol !== "http:" &&
          parsedOrigin.protocol !== "https:") ||
        parsedOrigin.username !== "" ||
        parsedOrigin.password !== "" ||
        parsedOrigin.pathname !== "/" ||
        parsedOrigin.search !== "" ||
        parsedOrigin.hash !== ""
      ) {
        return "";
      }
      normalizedOrigin = parsedOrigin.origin;
    } catch {
      return "";
    }
  }

  if (/^https?:\/\//i.test(rawTarget)) {
    try {
      const parsed = new URL(rawTarget);
      if (
        (parsed.protocol !== "http:" && parsed.protocol !== "https:") ||
        parsed.username !== "" ||
        parsed.password !== "" ||
        parsed.search !== "" ||
        parsed.hash !== "" ||
        !parsed.pathname.startsWith("/share/")
      ) {
        return "";
      }
      const tokenSeg = parsed.pathname.slice("/share/".length).trim();
      if (!isValidShareTokenSegment(tokenSeg)) {
        return "";
      }
      if (
        normalizedOrigin &&
        parsed.origin.toLowerCase() !== normalizedOrigin.toLowerCase()
      ) {
        return "";
      }
      return `${parsed.origin}/share/${tokenSeg}`;
    } catch {
      return "";
    }
  }

  let path: string;
  if (rawTarget.startsWith("/share/")) {
    const tokenSeg = rawTarget.slice("/share/".length).trim();
    if (!isValidShareTokenSegment(tokenSeg)) {
      return "";
    }
    path = `/share/${tokenSeg}`;
  } else if (rawTarget.startsWith("share/")) {
    const tokenSeg = rawTarget.slice("share/".length).trim();
    if (!isValidShareTokenSegment(tokenSeg)) {
      return "";
    }
    path = `/share/${tokenSeg}`;
  } else if (rawTarget.startsWith("/")) {
    return "";
  } else {
    if (!isValidShareTokenSegment(rawTarget)) {
      return "";
    }
    path = `/share/${rawTarget}`;
  }

  return normalizedOrigin ? `${normalizedOrigin}${path}` : path;
}

export function formatShareLinkViews(
  link: Pick<ShareLinkItem, "access_count" | "max_views">
): string {
  const rawCount =
    typeof link.access_count === "number" && Number.isFinite(link.access_count)
      ? link.access_count
      : 0;
  const count = Math.max(0, Math.floor(rawCount));
  if (
    typeof link.max_views === "number" &&
    Number.isFinite(link.max_views) &&
    link.max_views > 0
  ) {
    return `${count}/${Math.floor(link.max_views)}`;
  }
  return `${count}`;
}

export interface ShareLinkSessionState {
  links: ShareLinkItem[];
  shareUrlMap: Record<string, string>;
  generatedShareUrl: string;
  creatingShareId: string | null;
  revokingShareId: string | null;
}

export interface ShareLinkSessionTransport {
  fetchImpl?: typeof fetch;
  listShareLinks?: () => Promise<ShareLinkItem[] | null>;
  origin?: string | (() => string);
  nowSec?: () => number;
  copyUrlToClipboard?: (fullUrl: string, ttlHoursLabel?: string) => Promise<void>;
  notify?: (msg: string) => void;
  onShareUrlMapChange?: (
    updater: (prev: Record<string, string>) => Record<string, string>
  ) => void;
  onGeneratedShareUrlChange?: (updater: (prev: string) => string) => void;
  onCreatingShareIdChange?: (id: string | null) => void;
  onRevokingShareIdChange?: (id: string | null) => void;
}

export interface CreateAndCopyShareLinkParams {
  targetId: string;
  kind?: string;
  expiresInSec: number;
  maxViews?: number | null;
}

export interface ShareLinkSessionController {
  getState(): ShareLinkSessionState;
  subscribe(listener: (state: ShareLinkSessionState) => void): () => void;
  seedState(partial: Partial<ShareLinkSessionState>): void;
  refresh(): Promise<ShareLinkItem[] | null>;
  createAndCopy(
    params: CreateAndCopyShareLinkParams
  ): Promise<{ ok: boolean; url: string }>;
  copyOrRotate(
    link: ShareLinkItem
  ): Promise<{ ok: boolean; url: string; rotated: boolean }>;
  revoke(id: string): Promise<boolean>;
  evictTarget(
    targetId: string,
    knownLinks?: ReadonlyArray<ShareLinkItem>
  ): { evictedIds: string[]; clearedGeneratedUrl: boolean };
  dispose(): void;
}

function resolveTransportOrigin(origin?: string | (() => string)): string {
  if (typeof origin === "function") {
    return origin();
  }
  if (typeof origin === "string") {
    return origin;
  }
  if (typeof window !== "undefined" && window.location) {
    return window.location.origin;
  }
  return "";
}

/**
 * Stateful session controller encapsulating one-time HMAC share token URL caching
 * (`shareUrlMap`), create-and-copy, server-revalidated copy-or-rotate, revocation,
 * and target-deletion eviction.
 */
export function createShareLinkSessionController(
  transport: ShareLinkSessionTransport = {}
): ShareLinkSessionController {
  let state: ShareLinkSessionState = {
    links: [],
    shareUrlMap: {},
    generatedShareUrl: "",
    creatingShareId: null,
    revokingShareId: null,
  };
  let disposed = false;
  let refreshSeq = 0;
  let refreshAbort: AbortController | null = null;
  const listeners = new Set<(next: ShareLinkSessionState) => void>();

  const getFetch = (): typeof fetch => transport.fetchImpl ?? fetch;
  const getNowSec = (): number =>
    transport.nowSec ? transport.nowSec() : resolveNowSec();
  const notify = (msg: string) => {
    if (!disposed) {
      transport.notify?.(msg);
    }
  };

  const emit = () => {
    if (disposed) return;
    const snap: ShareLinkSessionState = {
      ...state,
      links: [...state.links],
      shareUrlMap: { ...state.shareUrlMap },
    };
    for (const listener of listeners) {
      listener(snap);
    }
  };

  const updateShareUrlMap = (
    updater: (prev: Record<string, string>) => Record<string, string>
  ) => {
    const nextMap = updater(state.shareUrlMap);
    state = { ...state, shareUrlMap: nextMap };
    if (!disposed) {
      transport.onShareUrlMapChange?.(updater);
    }
    emit();
  };

  const updateGeneratedShareUrl = (updater: (prev: string) => string) => {
    const nextUrl = updater(state.generatedShareUrl);
    state = { ...state, generatedShareUrl: nextUrl };
    if (!disposed) {
      transport.onGeneratedShareUrlChange?.(updater);
    }
    emit();
  };

  const setCreatingId = (id: string | null) => {
    state = { ...state, creatingShareId: id };
    if (!disposed) {
      transport.onCreatingShareIdChange?.(id);
    }
    emit();
  };

  const setRevokingId = (id: string | null) => {
    state = { ...state, revokingShareId: id };
    if (!disposed) {
      transport.onRevokingShareIdChange?.(id);
    }
    emit();
  };

  const defaultCopyToClipboard = async (
    fullUrl: string,
    ttlHoursLabel?: string
  ) => {
    updateGeneratedShareUrl(() => fullUrl);
    if (transport.copyUrlToClipboard) {
      await transport.copyUrlToClipboard(fullUrl, ttlHoursLabel);
      return;
    }
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

  const refresh = async (): Promise<ShareLinkItem[] | null> => {
    if (transport.listShareLinks) {
      const rows = await transport.listShareLinks();
      if (rows !== null && !disposed) {
        state = { ...state, links: [...rows] };
        emit();
      }
      return rows;
    }
    const seq = ++refreshSeq;
    const controller =
      typeof AbortController !== "undefined" ? new AbortController() : null;
    refreshAbort = controller;
    try {
      const res = await getFetch()("/api/share", {
        credentials: "include",
        ...(controller ? { signal: controller.signal } : {}),
      });
      if (!res.ok) return null;
      const data = await res.json().catch(() => null);
      if (Array.isArray(data)) {
        const parsed = data as ShareLinkItem[];
        if (!disposed && seq === refreshSeq) {
          state = { ...state, links: parsed };
          emit();
        }
        return parsed;
      }
      return null;
    } catch {
      return null;
    }
  };

  const evictLinkUrl = (id: string, fallbackOldUrl: string) => {
    let evictedUrl = fallbackOldUrl;
    updateShareUrlMap((prev) => {
      if (prev[id]) {
        evictedUrl = prev[id];
      }
      if (!(id in prev)) return prev;
      const next = { ...prev };
      delete next[id];
      return next;
    });
    if (evictedUrl) {
      updateGeneratedShareUrl((prev) => (prev === evictedUrl ? "" : prev));
    }
  };

  return {
    getState() {
      return {
        ...state,
        links: [...state.links],
        shareUrlMap: { ...state.shareUrlMap },
      };
    },

    subscribe(listener) {
      disposed = false;
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },

    seedState(partial) {
      state = {
        ...state,
        ...(partial.links !== undefined ? { links: [...partial.links] } : {}),
        ...(partial.shareUrlMap !== undefined
          ? { shareUrlMap: { ...partial.shareUrlMap } }
          : {}),
        ...(partial.generatedShareUrl !== undefined
          ? { generatedShareUrl: partial.generatedShareUrl }
          : {}),
        ...(partial.creatingShareId !== undefined
          ? { creatingShareId: partial.creatingShareId }
          : {}),
        ...(partial.revokingShareId !== undefined
          ? { revokingShareId: partial.revokingShareId }
          : {}),
      };
      emit();
    },

    refresh,

    async createAndCopy(params) {
      if (state.creatingShareId !== null || state.revokingShareId !== null) {
        return { ok: false, url: "" };
      }
      setCreatingId(params.targetId);
      try {
        const payload: Record<string, unknown> = {
          kind: params.kind || "export",
          target_id: params.targetId,
          expires_in_sec: params.expiresInSec,
        };
        if (
          typeof params.maxViews === "number" &&
          Number.isFinite(params.maxViews) &&
          params.maxViews > 0
        ) {
          payload.max_views = params.maxViews;
        }
        const res = await getFetch()("/api/share", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify(payload),
        });
        const data = await res.json().catch(() => ({}));
        if (disposed) return { ok: false, url: "" };
        const origin = resolveTransportOrigin(transport.origin);
        if (res.ok && (data.url || data.token)) {
          const fullUrl = buildSharePublicUrl(origin, data.url || data.token);
          if (!fullUrl) {
            await refresh();
            notify("Failed to generate share link");
            return { ok: false, url: "" };
          }
          if (data.id && !disposed) {
            updateShareUrlMap((prev) => ({
              ...prev,
              [String(data.id)]: fullUrl,
            }));
          }
          await refresh();
          if (!disposed) {
            const now = getNowSec();
            await defaultCopyToClipboard(
              fullUrl,
              formatShareLinkRemainingTtl(now + params.expiresInSec, now)
            );
          }
          return { ok: true, url: fullUrl };
        }
        notify(
          (typeof data?.message === "string" && data.message) ||
            "Failed to generate share link"
        );
        return { ok: false, url: "" };
      } catch {
        notify("Failed to generate share link");
        return { ok: false, url: "" };
      } finally {
        setCreatingId(null);
      }
    },

    async copyOrRotate(link) {
      if (state.creatingShareId !== null || state.revokingShareId !== null) {
        return { ok: false, url: "", rotated: false };
      }
      setCreatingId(link.target_id);
      try {
        const origin = resolveTransportOrigin(transport.origin);
        const latestLinks = await refresh();
        if (disposed) return { ok: false, url: "", rotated: false };
        const currentLink =
          latestLinks !== null
            ? latestLinks.find((l) => l.id === link.id)
            : link;
        const oldUrl =
          state.shareUrlMap[link.id] ||
          (currentLink?.url
            ? buildSharePublicUrl(origin, currentLink.url)
            : currentLink?.token
              ? buildSharePublicUrl(origin, currentLink.token)
              : link.url
                ? buildSharePublicUrl(origin, link.url)
                : link.token
                  ? buildSharePublicUrl(origin, link.token)
                  : "");

        const now = getNowSec();
        if (!currentLink || computeShareLinkStatus(currentLink, now) !== "active") {
          evictLinkUrl(link.id, oldUrl);
          notify("Share link is no longer active");
          return { ok: false, url: "", rotated: false };
        }

        const knownUrl =
          (state.shareUrlMap[currentLink.id]
            ? buildSharePublicUrl(origin, state.shareUrlMap[currentLink.id])
            : "") ||
          (currentLink.url
            ? buildSharePublicUrl(origin, currentLink.url)
            : currentLink.token
              ? buildSharePublicUrl(origin, currentLink.token)
              : link.url
                ? buildSharePublicUrl(origin, link.url)
                : link.token
                  ? buildSharePublicUrl(origin, link.token)
                  : "");

        if (knownUrl) {
          if (!disposed) {
            if (transport.copyUrlToClipboard) {
              await transport.copyUrlToClipboard(knownUrl);
            } else {
              await defaultCopyToClipboard(knownUrl);
            }
          }
          return { ok: !disposed, url: disposed ? "" : knownUrl, rotated: false };
        }

        const rotation = computeShareLinkRotationParams(currentLink, now);
        if (!rotation) {
          evictLinkUrl(link.id, oldUrl);
          notify("Share link is no longer active");
          return { ok: false, url: "", rotated: false };
        }

        let delRes: Response;
        try {
          delRes = await getFetch()(
            `/api/share/${encodeURIComponent(rotation.revokeOldId)}`,
            {
              method: "DELETE",
              credentials: "include",
            }
          );
        } catch {
          notify("Failed to revoke previous share link");
          return { ok: false, url: "", rotated: false };
        }

        if (!delRes.ok && delRes.status !== 404) {
          notify("Failed to revoke previous share link");
          return { ok: false, url: "", rotated: false };
        }

        const revokedOldUrl = state.shareUrlMap[rotation.revokeOldId] || oldUrl;
        evictLinkUrl(rotation.revokeOldId, revokedOldUrl);
        if (disposed) return { ok: false, url: "", rotated: false };

        const payload: Record<string, unknown> = {
          kind: currentLink.kind || "export",
          target_id: currentLink.target_id,
          expires_in_sec: rotation.expiresInSec,
        };
        if (
          typeof rotation.remainingMaxViews === "number" &&
          Number.isFinite(rotation.remainingMaxViews) &&
          rotation.remainingMaxViews > 0
        ) {
          payload.max_views = rotation.remainingMaxViews;
        }

        const res = await getFetch()("/api/share", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify(payload),
        });
        const data = await res.json().catch(() => ({}));
        if (disposed) return { ok: false, url: "", rotated: false };
        if (res.ok && (data.url || data.token)) {
          const fullUrl = buildSharePublicUrl(origin, data.url || data.token);
          if (!fullUrl) {
            await refresh();
            notify("Failed to generate share link");
            return { ok: false, url: "", rotated: false };
          }
          updateShareUrlMap((prev) => {
            const next = { ...prev };
            delete next[rotation.revokeOldId];
            if (data.id) {
              next[String(data.id)] = fullUrl;
            }
            return next;
          });
          await refresh();
          if (!disposed) {
            const postNow = getNowSec();
            const ttlLabel = formatShareLinkRemainingTtl(
              postNow + rotation.expiresInSec,
              postNow
            );
            if (transport.copyUrlToClipboard) {
              await transport.copyUrlToClipboard(fullUrl, ttlLabel);
            } else {
              await defaultCopyToClipboard(fullUrl, ttlLabel);
            }
          }
          return { ok: true, url: fullUrl, rotated: true };
        }

        await refresh();
        notify(
          (typeof data?.message === "string" && data.message) ||
            "Failed to generate share link"
        );
        return { ok: false, url: "", rotated: false };
      } catch {
        await refresh();
        notify("Failed to generate share link");
        return { ok: false, url: "", rotated: false };
      } finally {
        setCreatingId(null);
      }
    },

    async revoke(id) {
      if (state.creatingShareId !== null || state.revokingShareId !== null) {
        return false;
      }
      setRevokingId(id);
      try {
        const res = await getFetch()(`/api/share/${encodeURIComponent(id)}`, {
          method: "DELETE",
          credentials: "include",
        });
        if (disposed) return false;
        if (res.ok) {
          const origin = resolveTransportOrigin(transport.origin);
          const revokedLink = state.links.find((l) => l.id === id);
          const revokedUrl =
            state.shareUrlMap[id] ||
            (revokedLink?.url
              ? buildSharePublicUrl(origin, revokedLink.url)
              : revokedLink?.token
                ? buildSharePublicUrl(origin, revokedLink.token)
                : "");
          evictLinkUrl(id, revokedUrl);
          await refresh();
          notify(`Revoked share link ${id}`);
          return true;
        }
        const data = await res.json().catch(() => ({}));
        notify(
          (typeof data?.message === "string" && data.message) ||
            "Failed to revoke share link"
        );
        return false;
      } catch {
        notify("Failed to revoke share link");
        return false;
      } finally {
        setRevokingId(null);
      }
    },

    evictTarget(targetId, knownLinks) {
      const origin = resolveTransportOrigin(transport.origin);
      const sourceLinks = knownLinks ?? state.links;
      const matchingLinks = sourceLinks.filter((l) => l.target_id === targetId);
      const matchingIds = new Set(matchingLinks.map((l) => l.id));
      const matchingUrls = new Set<string>();

      for (const l of matchingLinks) {
        if (state.shareUrlMap[l.id]) {
          matchingUrls.add(state.shareUrlMap[l.id]);
        }
        if (l.url) {
          const built = buildSharePublicUrl(origin, l.url);
          if (built) matchingUrls.add(built);
        }
        if (l.token) {
          const built = buildSharePublicUrl(origin, l.token);
          if (built) matchingUrls.add(built);
        }
      }
      if (state.shareUrlMap[targetId]) {
        matchingUrls.add(state.shareUrlMap[targetId]);
      }

      const evictedIds: string[] = [];
      updateShareUrlMap((prev) => {
        let changed = false;
        const next = { ...prev };
        for (const id of matchingIds) {
          if (id in next) {
            delete next[id];
            evictedIds.push(id);
            changed = true;
          }
        }
        if (targetId in next) {
          delete next[targetId];
          evictedIds.push(targetId);
          changed = true;
        }
        return changed ? next : prev;
      });

      let clearedGeneratedUrl = false;
      if (matchingUrls.size > 0) {
        updateGeneratedShareUrl((prev) => {
          if (matchingUrls.has(prev)) {
            clearedGeneratedUrl = true;
            return "";
          }
          return prev;
        });
      }

      state = {
        ...state,
        links: state.links.filter((l) => l.target_id !== targetId),
      };
      emit();

      return {
        evictedIds,
        clearedGeneratedUrl,
      };
    },

    dispose() {
      disposed = true;
      refreshSeq += 1;
      if (refreshAbort) {
        try {
          refreshAbort.abort();
        } catch {
          // ignore
        }
        refreshAbort = null;
      }
      listeners.clear();
    },
  };
}
