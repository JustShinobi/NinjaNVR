import React, { useEffect, useState } from "react";

/** Milliseconds each flipbook frame stays on screen. */
export const FLIPBOOK_FRAME_MS = 450;

interface ReviewFramesResponse {
  frames?: Array<{ url?: unknown }>;
}

/**
 * Keeps only same-origin review frame URLs for the given review id, so a
 * tampered API response can never point an <img> at another origin or path.
 */
export function sanitizeReviewFrameUrls(reviewId: string, payload: unknown): string[] {
  if (!reviewId || !/^[A-Za-z0-9_.-]+$/.test(reviewId) || reviewId.includes("..")) return [];
  const frames = (payload as ReviewFramesResponse | null)?.frames;
  if (!Array.isArray(frames)) return [];
  const prefix = `/api/review/${reviewId}/frames/`;
  const out: string[] = [];
  for (const frame of frames) {
    const url = typeof frame?.url === "string" ? frame.url : "";
    if (url.startsWith(prefix) && /^\d{1,16}\.jpg$/.test(url.slice(prefix.length))) {
      out.push(url);
    }
    if (out.length >= 16) break;
  }
  return out;
}

/** Next frame index for a looping flipbook; stays on 0 for 0–1 frames. */
export function nextFlipbookIndex(current: number, total: number): number {
  if (!Number.isFinite(total) || total <= 1) return 0;
  const cur = Number.isFinite(current) && current >= 0 ? Math.floor(current) : 0;
  return (cur + 1) % Math.floor(total);
}

function prefersReducedMotion(): boolean {
  try {
    return typeof window !== "undefined" &&
      typeof window.matchMedia === "function" &&
      window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  } catch {
    return false;
  }
}

interface ReviewFlipbookProps {
  reviewId: string;
  fallbackSrc: string;
  alt: string;
}

/**
 * Loops a review item's still frames as a flipbook using plain <img> swaps:
 * no video decoder is opened (Zero-Decoder-Bloat). Falls back to the static
 * thumbnail when frames are unavailable, and holds still for reduced motion
 * or while the page is hidden.
 */
export const ReviewFlipbook: React.FC<ReviewFlipbookProps> = ({ reviewId, fallbackSrc, alt }) => {
  // Frames that finished loading, in order. Frames are preloaded one at a time per
  // card so a page of cards never floods the server's bounded extraction queue.
  const [ready, setReady] = useState<string[]>([]);
  const [index, setIndex] = useState(0);

  useEffect(() => {
    let cancelled = false;
    let pending: HTMLImageElement | null = null;
    setReady([]);
    setIndex(0);

    const preloadSequentially = (urls: string[], at: number) => {
      if (cancelled || at >= urls.length || typeof Image === "undefined") return;
      const img = new Image();
      pending = img;
      img.onload = () => {
        if (cancelled) return;
        setReady((prev) => [...prev, urls[at]]);
        preloadSequentially(urls, at + 1);
      };
      // A missing or throttled frame is skipped; the loop keeps the ones that loaded.
      img.onerror = () => preloadSequentially(urls, at + 1);
      img.src = urls[at];
    };

    fetch(`/api/review/${encodeURIComponent(reviewId)}/frames`, { credentials: "include" })
      .then((res) => (res.ok ? res.json() : null))
      .then((payload) => {
        if (!cancelled) preloadSequentially(sanitizeReviewFrameUrls(reviewId, payload), 0);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      if (pending) {
        pending.onload = null;
        pending.onerror = null;
        pending.src = "";
      }
    };
  }, [reviewId]);

  const usable = ready;

  useEffect(() => {
    if (usable.length <= 1 || prefersReducedMotion()) return;
    const timer = window.setInterval(() => {
      if (typeof document !== "undefined" && document.visibilityState === "hidden") return;
      setIndex((i) => nextFlipbookIndex(i, usable.length));
    }, FLIPBOOK_FRAME_MS);
    return () => window.clearInterval(timer);
  }, [usable.length]);

  const current = usable.length > 0 ? usable[index % usable.length] : null;
  const progress = usable.length > 1 ? ((index % usable.length) + 1) / usable.length : 0;

  return (
    <>
      <img src={current ?? fallbackSrc} alt={alt} width={166} height={94} loading="lazy" />
      {progress > 0 && (
        <span className="nvr-event-progress" aria-hidden="true">
          <span style={{ width: `${Math.round(progress * 100)}%` }} />
        </span>
      )}
    </>
  );
};
