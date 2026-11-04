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
  if (
    typeof link.max_views === "number" &&
    Number.isFinite(link.max_views) &&
    link.max_views > 0 &&
    (link.access_count ?? 0) >= link.max_views
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
    const used = Math.max(0, Math.floor(link.access_count ?? 0));
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
  return seg.length > 0 && !seg.includes("..") && !/[:/?#\\]/.test(seg);
}

export function buildSharePublicUrl(
  origin: string,
  tokenOrUrl: string
): string {
  const rawTarget = (tokenOrUrl || "").trim();
  if (!rawTarget || /[\u0000-\u0020\u007f]/.test(rawTarget)) {
    return "";
  }
  if (rawTarget.startsWith("//") || rawTarget.includes("..")) {
    return "";
  }

  const cleanOrigin = (origin || "").trim().replace(/\/+$/, "");
  let normalizedOrigin = "";
  if (cleanOrigin) {
    if (/[\u0000-\u0020\u007f]/.test(cleanOrigin)) {
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
  const count = Math.max(0, Math.floor(link.access_count ?? 0));
  if (
    typeof link.max_views === "number" &&
    Number.isFinite(link.max_views) &&
    link.max_views > 0
  ) {
    return `${count}/${Math.floor(link.max_views)}`;
  }
  return `${count}`;
}
