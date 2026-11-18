import { useCallback, useEffect, useRef, useState } from "react";

export interface UseTwoStepConfirmResult {
  armedKey: string | null;
  isArmed: (key: string) => boolean;
  confirmOrArm: (key: string, onConfirmed: () => void) => boolean;
  clearArmed: () => void;
}

/**
 * Provides a 3-second inline 2-click confirmation state for destructive actions
 * without blocking the operator console with `window.confirm`.
 */
export function useTwoStepConfirm(
  timeoutMs: number = 3000
): UseTwoStepConfirmResult {
  const [armedKey, setArmedKey] = useState<string | null>(null);
  const armedKeyRef = useRef<string | null>(null);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearTimer = useCallback(() => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  }, []);

  const clearArmed = useCallback(() => {
    clearTimer();
    armedKeyRef.current = null;
    setArmedKey(null);
  }, [clearTimer]);

  useEffect(() => {
    return () => {
      clearTimer();
    };
  }, [clearTimer]);

  const isArmed = useCallback(
    (key: string) => Boolean(key && armedKey === key),
    [armedKey]
  );

  const confirmOrArm = useCallback(
    (key: string, onConfirmed: () => void): boolean => {
      if (!key) return false;
      if (armedKeyRef.current === key) {
        clearTimer();
        armedKeyRef.current = null;
        setArmedKey(null);
        onConfirmed();
        return true;
      }
      clearTimer();
      armedKeyRef.current = key;
      setArmedKey(key);
      timerRef.current = setTimeout(() => {
        if (armedKeyRef.current === key) {
          armedKeyRef.current = null;
          setArmedKey(null);
        }
        timerRef.current = null;
      }, timeoutMs);
      return false;
    },
    [clearTimer, timeoutMs]
  );

  return { armedKey, isArmed, confirmOrArm, clearArmed };
}
