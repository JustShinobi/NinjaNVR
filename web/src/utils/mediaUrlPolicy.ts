export type MediaUrlKind = "export_video" | "thumbnail" | "event_clip";

export interface ResolveSafeMediaUrlOptions {
  kind: MediaUrlKind;
  /** Explicit fallback URL returned when `rawPath` is empty or rejected. */
  fallbackUrl?: string;
  /** Camera name used to construct a safe `/api/:cam/latest.webp` or range export fallback. */
  fallbackCamera?: string;
  /** Export identifier used to construct a safe `/exports/:id.mp4` fallback. */
  exportId?: string;
  /** Start timestamp (seconds) for range-based export fallback construction. */
  startSec?: number;
  /** End timestamp (seconds) for range-based export fallback construction. */
  endSec?: number;
  /** Event identifier used to construct `/api/events/:id/clip.mp4` fallback. */
  eventId?: string;
  /** Optional explicit allowlist of canonical root prefixes (must start and end with `/`). */
  allowedPrefixes?: ReadonlyArray<string>;
  /** When true, rejects raw whitespace characters (e.g. unencoded 0x20) in `rawPath`. */
  rejectSpaces?: boolean;
}

const SENSITIVE_API_ROUTE_RE =
  /^\/api\/(?:(?:auth|logout|users|tokens|config|restart|camera-proxy|federation|logs|share|metrics|profile|ffprobe|streams|detectors|debug|mcp|ai|go2rtc)\b|dataset\/export\.zip\b)/i;

const EXTERNAL_SCHEME_RE = /^[a-zA-Z][a-zA-Z0-9+.-]*:/;

const DEFAULT_PREFIXES_BY_KIND: Record<MediaUrlKind, ReadonlyArray<string>> = {
  export_video: ["/exports/", "/clips/"],
  thumbnail: ["/clips/", "/api/", "/thumbs/", "/previews/", "/exports/"],
  event_clip: ["/clips/", "/api/events/", "/exports/", "/vod/"],
};

export function sanitizeMediaCameraSlug(
  camera: string | null | undefined,
  defaultCamera: string = "camera"
): string {
  const rawCam = (camera || "").trim();
  const cleanCam =
    !rawCam || rawCam === "." || rawCam === ".." || rawCam.includes("..")
      ? defaultCamera
      : rawCam;
  return encodeURIComponent(cleanCam);
}

export function buildSafeExportIdFallbackUrl(
  exportId?: string | null
): string {
  const cleanId = (exportId || "").trim();
  if (!cleanId) return "";
  let decodedId = cleanId;
  try {
    decodedId = decodeURIComponent(cleanId);
  } catch {
    return "";
  }
  if (
    cleanId === "." ||
    decodedId === "." ||
    cleanId.includes("..") ||
    decodedId.includes("..") ||
    decodedId.includes("%") ||
    /[/\\?#\u0000-\u001f\u007f]/.test(cleanId) ||
    /[/\\?#\u0000-\u001f\u007f]/.test(decodedId)
  ) {
    return "";
  }
  return `/exports/${encodeURIComponent(decodedId)}.mp4`;
}

export function buildSafeCameraRangeExportFallbackUrl(
  fallbackCamera: string | null | undefined,
  startSec: number,
  endSec: number
): string {
  const safeCam = sanitizeMediaCameraSlug(fallbackCamera, "camera");
  const s = Number.isFinite(startSec) ? Math.floor(startSec) : 0;
  const e = Number.isFinite(endSec)
    ? Math.ceil(Math.max(endSec, s + 5))
    : s + 5;
  return `/exports/${safeCam}_${s}_${e}.mp4`;
}

export function buildSafeCameraLatestThumbFallbackUrl(
  fallbackCamera: string | null | undefined,
  defaultCamera: string = "camera"
): string {
  const safeCam = sanitizeMediaCameraSlug(fallbackCamera, defaultCamera);
  return `/api/${safeCam}/latest.webp`;
}

export function buildSafeEventClipFallbackUrl(
  eventId?: string | null
): string {
  const cleanId = (eventId || "").trim();
  if (!cleanId) return "";
  let decodedId = cleanId;
  try {
    decodedId = decodeURIComponent(cleanId);
  } catch {
    return "";
  }
  if (
    cleanId === "." ||
    decodedId === "." ||
    cleanId.includes("..") ||
    decodedId.includes("..") ||
    decodedId.includes("%") ||
    /[/\\?#\u0000-\u001f\u007f]/.test(cleanId) ||
    /[/\\?#\u0000-\u001f\u007f]/.test(decodedId)
  ) {
    return "";
  }
  return `/api/events/${encodeURIComponent(decodedId)}/clip.mp4`;
}

function resolveDefaultFallbackUrl(options: ResolveSafeMediaUrlOptions): string {
  if (options.fallbackUrl !== undefined) {
    return options.fallbackUrl;
  }
  if (options.kind === "export_video") {
    if (options.exportId !== undefined) {
      return buildSafeExportIdFallbackUrl(options.exportId);
    }
    if (
      options.fallbackCamera !== undefined ||
      options.startSec !== undefined ||
      options.endSec !== undefined
    ) {
      return buildSafeCameraRangeExportFallbackUrl(
        options.fallbackCamera,
        options.startSec ?? 0,
        options.endSec ?? 0
      );
    }
    return "";
  }
  if (options.kind === "thumbnail") {
    if (options.fallbackCamera !== undefined) {
      return buildSafeCameraLatestThumbFallbackUrl(options.fallbackCamera);
    }
    return "";
  }
  if (options.kind === "event_clip") {
    if (options.eventId !== undefined) {
      return buildSafeEventClipFallbackUrl(options.eventId);
    }
    return "";
  }
  return "";
}

/**
 * Canonicalizes and validates relative NVR media paths (`export_video`,
 * `thumbnail`, `event_clip`), stripping `/media/<mount>/` prefixes, decoding
 * and canonicalizing percent-encoding, and rejecting traversal (`..`),
 * encoded separators (`%2f`, `%5c`), double-encoding (`%25..`), control chars,
 * query/hash suffixes, external origins/schemes, and sensitive `/api/*` routes.
 */
export function resolveSafeMediaUrl(
  rawPath: string | null | undefined,
  options: ResolveSafeMediaUrlOptions
): string {
  const fallback = resolveDefaultFallbackUrl(options);
  if (typeof rawPath !== "string") {
    return fallback;
  }
  const raw = rawPath.trim();
  if (!raw) {
    return fallback;
  }

  if (
    /[\u0000-\u001f\u007f\\?#]/.test(raw) ||
    (options.rejectSpaces && /\s/.test(raw))
  ) {
    return fallback;
  }

  if (raw.startsWith("//") || EXTERNAL_SCHEME_RE.test(raw)) {
    return fallback;
  }

  const stripped = raw.replace(/^\/media\/[^/]+(?=\/|$)/, "").trim();
  if (
    !stripped ||
    !stripped.startsWith("/") ||
    stripped.startsWith("//") ||
    stripped.includes("..") ||
    /%2e/i.test(stripped) ||
    /%2f/i.test(stripped) ||
    /%5c/i.test(stripped) ||
    /%00/i.test(stripped)
  ) {
    return fallback;
  }

  let decoded: string;
  try {
    decoded = decodeURIComponent(stripped);
  } catch {
    return fallback;
  }

  if (
    !decoded.startsWith("/") ||
    decoded.startsWith("//") ||
    decoded.includes("..") ||
    decoded.includes("%") ||
    /[\u0000-\u001f\u007f\\?#]/.test(decoded) ||
    SENSITIVE_API_ROUTE_RE.test(decoded)
  ) {
    return fallback;
  }

  const segments = decoded.split("/");
  if (
    segments.length < 2 ||
    segments.some((seg, idx) => (idx > 0 && seg.length === 0) || seg === "." || seg === "..")
  ) {
    return fallback;
  }

  const canonical = segments
    .map((seg, idx) => (idx === 0 ? "" : encodeURIComponent(seg)))
    .join("/");

  const allowedPrefixes =
    options.allowedPrefixes ?? DEFAULT_PREFIXES_BY_KIND[options.kind];
  if (
    allowedPrefixes.length > 0 &&
    !allowedPrefixes.some(
      (prefix) => decoded.startsWith(prefix) || canonical.startsWith(prefix)
    )
  ) {
    return fallback;
  }

  return canonical;
}
