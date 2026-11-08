import React from "react";
import { useSheetDismiss } from "../utils/useSheetDismiss";
import { X } from "lucide-react";
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
  showVectors: boolean;
  onSetShowVectors: (next: boolean) => void;
  onClose: () => void;
  isPt?: boolean;
  canEdit?: boolean;
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

const Switch: React.FC<{
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
    className={`nvr-switch ${checked ? "on" : ""}`}
    onClick={() => onChange(!checked)}
  >
    <span className="nvr-switch-knob" aria-hidden="true" />
  </button>
);

export const CameraDetailsSheet: React.FC<CameraDetailsSheetProps> = ({
  camera,
  telemetry,
  flags,
  onToggleFlag,
  useMainStream,
  onSetMainStream,
  showVectors,
  onSetShowVectors,
  onClose,
  isPt = false,
  canEdit = true,
}) => {
  const sheetRef = useSheetDismiss<HTMLElement>(true, onClose);

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
            </div>
          ))}
        </div>

        <div className="nvr-sheet-group">
          <span className="nvr-sheet-k">Pipeline</span>
          {FLAG_ORDER.map((flag) => (
            <div key={flag} className="nvr-sheet-row">
              <span>{flagLabel(flag, isPt)}</span>
              <Switch
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
          <div className="nvr-segmented nvr-sheet-seg" role="group" aria-label="Stream quality">
            <button
              type="button"
              className={`nvr-btn ${!useMainStream ? "active" : ""}`}
              aria-pressed={!useMainStream}
              onClick={() => onSetMainStream(false)}
            >
              {isPt ? "Sub (leve)" : "Sub (light)"}
            </button>
            <button
              type="button"
              className={`nvr-btn ${useMainStream ? "active" : ""}`}
              aria-pressed={useMainStream}
              onClick={() => onSetMainStream(true)}
            >
              {isPt ? "Principal (HD)" : "Main (HD)"}
            </button>
          </div>
          <div className="nvr-sheet-row">
            <span>{isPt ? "Zonas e máscaras" : "Zones and masks overlay"}</span>
            <Switch
              checked={showVectors}
              label={isPt ? "Zonas e máscaras" : "Zones and masks overlay"}
              onChange={onSetShowVectors}
              testId="flag-vectors"
            />
          </div>
        </div>
      </section>
    </div>
  );
};
