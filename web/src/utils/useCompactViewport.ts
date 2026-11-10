import { useEffect, useState } from "react";
import { COMPACT_MEDIA_QUERY, isCompactViewport } from "./liveTileHud";

/** Tracks whether the console runs on a phone: < 768px wide, or a touch screen <= 500px tall (landscape). */
export function useCompactViewport(): boolean {
  const read = () => {
    if (typeof window === "undefined") return false;
    const coarse =
      typeof window.matchMedia === "function" && window.matchMedia("(pointer: coarse)").matches;
    return isCompactViewport(window.innerWidth, window.innerHeight, coarse);
  };
  const [compact, setCompact] = useState<boolean>(read);
  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    const mql = window.matchMedia(COMPACT_MEDIA_QUERY);
    const sync = () => setCompact(mql.matches);
    sync();
    mql.addEventListener?.("change", sync);
    return () => mql.removeEventListener?.("change", sync);
  }, []);
  return compact;
}
