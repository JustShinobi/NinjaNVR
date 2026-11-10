import { useEffect, useRef } from "react";

/**
 * Modal sheet behaviour shared by every bottom sheet / drawer:
 * - Escape closes it even when focus never entered the sheet;
 * - focus moves into the sheet on open and returns to the opener on close.
 */
export function useSheetDismiss<T extends HTMLElement>(open: boolean, onClose: () => void) {
  const sheetRef = useRef<T | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;

  useEffect(() => {
    if (!open || typeof document === "undefined") return;
    const opener = document.activeElement as HTMLElement | null;
    const focusTarget = sheetRef.current?.querySelector<HTMLElement>(
      "button:not([disabled]), a[href], input, select, [tabindex]:not([tabindex='-1'])"
    );
    focusTarget?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onCloseRef.current();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      opener?.focus?.();
    };
  }, [open]);

  return sheetRef;
}
