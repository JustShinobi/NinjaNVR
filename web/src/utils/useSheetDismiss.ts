import { useEffect, useRef } from "react";

export const SHEET_FOCUSABLE_SELECTOR =
  'button:not([disabled]), a[href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"]):not([disabled])';

/**
 * Returns ordered, non-hidden focusable descendants within `container`.
 */
export function getFocusableSheetElements(
  container: ParentNode | null | undefined
): HTMLElement[] {
  if (!container || typeof container.querySelectorAll !== "function") {
    return [];
  }
  const nodes = Array.from(
    container.querySelectorAll<HTMLElement>(SHEET_FOCUSABLE_SELECTOR)
  );
  return nodes.filter((el) => {
    if (el.hasAttribute?.("hidden")) return false;
    if (el.getAttribute?.("aria-hidden") === "true") return false;
    if (typeof el.closest === "function" && el.closest("[hidden], [aria-hidden='true']")) {
      return false;
    }
    return true;
  });
}

/**
 * Traps `Tab` and `Shift+Tab` within `container`, cycling between the first and last
 * focusable descendants. Returns `true` when focus was redirected.
 */
export function trapSheetTabNavigation(
  event: Pick<KeyboardEvent, "key" | "shiftKey" | "preventDefault">,
  container: HTMLElement | null | undefined,
  activeElement: Element | null | undefined
): boolean {
  if (event.key !== "Tab" || !container) {
    return false;
  }
  const focusables = getFocusableSheetElements(container);
  if (focusables.length === 0) {
    event.preventDefault();
    container.focus?.();
    return true;
  }
  const first = focusables[0];
  const last = focusables[focusables.length - 1];
  const inside =
    activeElement !== null &&
    activeElement !== undefined &&
    (typeof container.contains === "function"
      ? container.contains(activeElement as Node)
      : focusables.includes(activeElement as HTMLElement));

  if (event.shiftKey) {
    if (!inside || activeElement === first || activeElement === container) {
      event.preventDefault();
      last.focus?.();
      return true;
    }
    return false;
  }

  if (!inside || activeElement === last) {
    event.preventDefault();
    first.focus?.();
    return true;
  }
  return false;
}

/**
 * Modal sheet behaviour shared by every bottom sheet / drawer:
 * - Escape closes it even when focus never entered the sheet;
 * - Tab / Shift+Tab cycles strictly inside focusable descendants of the sheet;
 * - focus moves into the sheet on open and returns to the invoker button on close.
 */
export function useSheetDismiss<T extends HTMLElement>(
  open: boolean,
  onClose: () => void
) {
  const sheetRef = useRef<T | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    if (!open || typeof document === "undefined") return;
    const opener = document.activeElement as HTMLElement | null;
    const container = sheetRef.current;
    const focusables = getFocusableSheetElements(container);
    if (focusables.length > 0) {
      focusables[0].focus?.();
    } else if (container) {
      if (!container.hasAttribute?.("tabindex")) {
        container.setAttribute?.("tabindex", "-1");
      }
      container.focus?.();
    }

    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onCloseRef.current();
        return;
      }
      if (e.key === "Tab") {
        trapSheetTabNavigation(e, sheetRef.current, document.activeElement);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      if (opener && opener.isConnected !== false) {
        opener.focus?.();
      }
    };
  }, [open]);

  return sheetRef;
}
