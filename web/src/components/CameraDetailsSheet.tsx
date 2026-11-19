import React, { useEffect, useRef, useState } from "react";
import { useSheetDismiss } from "../utils/useSheetDismiss";
import { X } from "lucide-react";
import type { LiveStreamQualityMode } from "../utils/format";
import type { CameraRuntimeFlag, CameraTelemetryRow } from "../utils/liveTileHud";

/**
 * Camera Details sheet — the "on demand" HUD layer.
 *
 * Holds everything that used to crowd the live tile: stream telemetry and the
 * enabled / detect / record / alerts runtime switches, plus the global display
 * preferences (stream quality, detection overlay) that the mobile toolbar hides.
 * Renders as a bottom sheet on phones and a right-hand drawer on desktop (CSS).
 */
export interface CameraDetailsSheetProps {
  camera: string;
  telemetry: CameraTelemetryRow[];
  flags: Record<CameraRuntimeFlag, boolean>;
  onToggleFlag: (flag: CameraRuntimeFlag, next: boolean) => void;
  useMainStream: boolean;
  onSetMainStream: (next: boolean) => void;
  streamQualityMode?: LiveStreamQualityMode;
  onSetStreamQualityMode?: (next: LiveStreamQualityMode) => void;
  hasIndependentSubstream?: boolean;
  showVectors: boolean;
  onSetShowVectors: (next: boolean) => void;
  onClose: () => void;
  isPt?: boolean;
  canEdit?: boolean;
  canReboot?: boolean;
  onRebootCamera?: () => void;
}

const FLAG_ORDER: CameraRuntimeFlag[] = ["enabled", "detect", "record", "alerts"];

function flagLabel(flag: CameraRuntimeFlag, isPt: boolean): string {
  switch (flag) {
    case "enabled":
      return isPt ? "Câmera ativa" : "Camera enabled";
    case "detect":
      return isPt ? "Detecção de objetos" : "Object detection";
    case "record":
      return isPt ? "Gravação" : "Recording";
    case "alerts":
      return isPt ? "Alertas" : "Alerts";
  }
}

export const SheetSwitch: React.FC<{
  checked: boolean;
  label: string;
  disabled?: boolean;
  onChange: (next: boolean) => void;
  testId?: string;
}> = ({ checked, label, disabled, onChange, testId }) => (
  <button
    type="button"
    role="switch"
    aria-checked={checked}
    aria-label={label}
    data-testid={testId}
    disabled={disabled}
    className={`nvr-sheet-switch ${checked ? "on" : ""}`}
    onClick={() => onChange(!checked)}
  >
    <span className="nvr-sheet-switch-knob" aria-hidden="true" />
  </button>
);

export const CameraDetailsSheet: React.FC<CameraDetailsSheetProps> = ({
  camera,
  telemetry,
  flags,
  onToggleFlag,
  useMainStream,
  onSetMainStream,
  streamQualityMode,
  onSetStreamQualityMode,
  hasIndependentSubstream = true,
  showVectors,
  onSetShowVectors,
  onClose,
  isPt = false,
  canEdit = true,
  canReboot = false,
  onRebootCamera,
}) => {
  const sheetRef = useSheetDismiss<HTMLElement>(true, onClose);
  const [confirmReboot, setConfirmReboot] = useState(false);
  const confirmTimerRef = useRef<number | null>(null);

  const effectiveMode: LiveStreamQualityMode =
    streamQualityMode ?? (useMainStream ? "main" : "sub");

  const selectQualityMode = (next: LiveStreamQualityMode) => {
    if (onSetStreamQualityMode) {
      onSetStreamQualityMode(next);
    } else {
      onSetMainStream(next === "main");
    }
  };

  useEffect(() => {
    return () => {
      if (confirmTimerRef.current !== null && typeof window !== "undefined") {
        window.clearTimeout(confirmTimerRef.current);
      }
    };
  }, []);

  const titleId = `nvr-details-title-${camera}`;
  return (
    <div className="nvr-sheet-backdrop" onClick={onClose} data-testid="camera-details-backdrop">
      <section
        className="nvr-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        data-testid={`camera-details-${camera}`}
        ref={sheetRef}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="nvr-sheet-handle" aria-hidden="true" />
        <header className="nvr-sheet-head">
          <div>
            <h2 id={titleId} className="mono nvr-sheet-title">
              {camera.toUpperCase()}
            </h2>
            <p className="nvr-sheet-sub">
              {isPt ? "Stream, detecção e exibição" : "Stream, detection and display"}
            </p>
          </div>
          <button
            type="button"
            className="nvr-icon-btn"
            aria-label={isPt ? "Fechar detalhes" : "Close details"}
            onClick={onClose}
          >
            <X size={20} />
          </button>
        </header>

        <div className="nvr-sheet-stats">
          {telemetry.map((row) => (
            <div key={row.key} className="nvr-sheet-stat" data-testid={`telemetry-${row.key}`}>
              <span className="nvr-sheet-k">{row.label}</span>
              <span className="nvr-sheet-v mono">{row.value}</span>
              {row.subValue && (
                <span className="nvr-sheet-subval mono" data-testid={`telemetry-${row.key}-sub`}>
                  {row.subValue}
                </span>
              )}
            </div>
          ))}
        </div>

        <div className="nvr-sheet-group">
          <span className="nvr-sheet-k">Pipeline</span>
          {FLAG_ORDER.map((flag) => (
            <div key={flag} className="nvr-sheet-row">
              <span>{flagLabel(flag, isPt)}</span>
              <SheetSwitch
                checked={flags[flag]}
                label={flagLabel(flag, isPt)}
                disabled={!canEdit}
                testId={`flag-${flag}`}
                onChange={(next) => onToggleFlag(flag, next)}
              />
            </div>
          ))}
        </div>

        <div className="nvr-sheet-group">
          <span className="nvr-sheet-k">{isPt ? "Qualidade do stream" : "Stream quality"}</span>
          <div
            className="nvr-segmented nvr-sheet-seg"
            role="group"
            aria-label={isPt ? "Qualidade do stream" : "Stream quality"}
          >
            <button
              type="button"
              data-testid="sheet-quality-auto"
              className={`nvr-btn ${effectiveMode === "auto" ? "active" : ""}`}
              aria-pressed={effectiveMode === "auto"}
              onClick={() => selectQualityMode("auto")}
            >
              Auto
            </button>
            <button
              type="button"
              data-testid="sheet-quality-sub"
              className={`nvr-btn ${effectiveMode === "sub" ? "active" : ""}`}
              aria-pressed={effectiveMode === "sub"}
              onClick={() => selectQualityMode("sub")}
            >
              {isPt ? "Sub (leve)" : "Sub (light)"}
            </button>
            <button
              type="button"
              data-testid="sheet-quality-main"
              className={`nvr-btn ${effectiveMode === "main" ? "active" : ""}`}
              aria-pressed={effectiveMode === "main"}
              onClick={() => selectQualityMode("main")}
            >
              {isPt ? "Principal" : "Main"}
            </button>
          </div>
          <p className="nvr-sheet-hint" data-testid="sheet-quality-hint">
            {!hasIndependentSubstream
              ? isPt
                ? "Esta câmera não possui substream separado — usa sempre o stream principal."
                : "This camera has no separate substream — it always uses the main stream."
              : effectiveMode === "auto"
              ? isPt
                ? "Auto usa Stream Principal na câmera em destaque e Substream na grade."
                : "Auto uses Main stream on hero views and Substream in grid thumbnails."
              : effectiveMode === "sub"
              ? isPt
                ? "Forçando Substream em todas as câmeras ao vivo."
                : "Forcing Substream across all live cameras."
              : isPt
              ? "Forçando Stream Principal (resolução máxima) em todas as câmeras ao vivo."
              : "Forcing Main stream (full native resolution) across all live cameras."}
          </p>
          <div className="nvr-sheet-row">
            <span>{isPt ? "Zonas e máscaras" : "Zones and masks overlay"}</span>
            <SheetSwitch
              checked={showVectors}
              label={isPt ? "Zonas e máscaras" : "Zones and masks overlay"}
              onChange={onSetShowVectors}
              testId="flag-vectors"
            />
          </div>
          {canReboot && onRebootCamera && (
            <div className="nvr-sheet-row">
              <span>{isPt ? "Hardware da câmera" : "Camera hardware"}</span>
              <button
                type="button"
                className="nvr-btn danger mono"
                data-testid={`sheet-reboot-${camera}`}
                data-confirming={confirmReboot ? "true" : "false"}
                onClick={() => {
                  if (confirmReboot) {
                    if (confirmTimerRef.current !== null && typeof window !== "undefined") {
                      window.clearTimeout(confirmTimerRef.current);
                      confirmTimerRef.current = null;
                    }
                    setConfirmReboot(false);
                    onRebootCamera();
                    return;
                  }
                  setConfirmReboot(true);
                  if (typeof window !== "undefined") {
                    confirmTimerRef.current = window.setTimeout(() => {
                      confirmTimerRef.current = null;
                      setConfirmReboot(false);
                    }, 3000);
                  }
                }}
              >
                {confirmReboot
                  ? isPt
                    ? "Clique novamente para confirmar"
                    : "Click again to confirm"
                  : isPt
                  ? "Reiniciar Câmera"
                  : "Reboot Camera"}
              </button>
            </div>
          )}
        </div>
      </section>
    </div>
  );
};
