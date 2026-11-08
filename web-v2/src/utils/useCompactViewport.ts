import { useEffect, useState } from "react";
import { MOBILE_BREAKPOINT_PX, isCompactViewport } from "./liveTileHud";

/** Tracks whether the console is running on a phone-width viewport (< 768px). */
export function useCompactViewport(): boolean {
  const read = () =>
    typeof window !== "undefined" ? isCompactViewport(window.innerWidth) : false;
  const [compact, setCompact] = useState<boolean>(read);
  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") return;
    const mql = window.matchMedia(`(max-width: ${MOBILE_BREAKPOINT_PX - 1}px)`);
    const sync = () => setCompact(mql.matches);
    sync();
    mql.addEventListener?.("change", sync);
    return () => mql.removeEventListener?.("change", sync);
  }, []);
  return compact;
}
