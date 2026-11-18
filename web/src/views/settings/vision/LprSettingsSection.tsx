import React, { useEffect, useRef, useState } from "react";
import { Plus } from "lucide-react";
import { CardCollapseToggle } from "../../../components/CardCollapseToggle";
import { useNvrStore } from "../../../store/useNvrStore";
import { LprConfig, NinjaConfigResponse } from "../../../types/nvr";
import {
  DEFAULT_LPR_FORMAT_REGEX,
  normalizeLicensePlate,
  resolveKnownPlateLabel,
  validateLicensePlateFormat,
} from "../../../utils/configYaml";
import type { ConfigWorkbenchSession } from "../../../utils/configWorkbench";
import { ToggleSwitch } from "../ConsoleAndStreamsPanels";
import { useTwoStepConfirm } from "./useTwoStepConfirm";

export interface LprSettingsSectionProps {
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  inspectorCam: string;
  isPt: boolean;
  notify: (msg: string) => void;
  initialDataCardsOpen: boolean;
  activeWorkbench: ConfigWorkbenchSession | null;
  scopeDivider: (group: "global" | "camera") => React.ReactNode;
  upsertKnownPlateEntry: (
    current: Record<string, string> | undefined,
    plate: string,
    label: string
  ) => Record<string, string>;
  removeKnownPlateEntry: (
    current: Record<string, string> | undefined,
    plate: string
  ) => Record<string, string>;
}

export const LprSettingsSection: React.FC<LprSettingsSectionProps> = ({
  draftConfig,
  updateDraft,
  inspectorCam,
  isPt,
  notify,
  initialDataCardsOpen,
  activeWorkbench,
  scopeDivider,
  upsertKnownPlateEntry,
  removeKnownPlateEntry,
}) => {
  const { isArmed, confirmOrArm } = useTwoStepConfirm(3000);
  const cameraKeys = Object.keys(draftConfig.cameras || {});
  const [showKnownPlates, setShowKnownPlates] = useState(initialDataCardsOpen);
  const [newKnownPlate, setNewKnownPlate] = useState("");
  const [newKnownPlateLabel, setNewKnownPlateLabel] = useState("");
  const [detectedPlates, setDetectedPlates] = useState<string[]>([]);
  const [simLprCamera, setSimLprCamera] = useState<string>(
    () => inspectorCam || cameraKeys[0] || ""
  );
  const [simLprPlate, setSimLprPlate] = useState<string>("BRA2E19");
  const [simLprBusy, setSimLprBusy] = useState<boolean>(false);
  const [simLprLastResult, setSimLprLastResult] = useState<{
    eventId: string;
    plate: string;
    camera: string;
    subLabel?: string | null;
  } | null>(null);
  const isMountedRef = useRef(true);
  const lprPersistSeqRef = useRef(0);

  const lprCfg: LprConfig = draftConfig.lpr || {
    enabled: false,
    min_confidence: 0.75,
    format_regex: DEFAULT_LPR_FORMAT_REGEX,
    known_plates: {},
  };

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    const nextDefault = inspectorCam || cameraKeys[0] || "";
    setSimLprCamera((prev) =>
      inspectorCam
        ? inspectorCam
        : prev && cameraKeys.includes(prev)
        ? prev
        : nextDefault
    );
  }, [inspectorCam, cameraKeys.join("\0")]);

  const fetchDetectedPlates = async () => {
    try {
      const r = await fetch("/api/recognized_license_plates", {
        credentials: "include",
      });
      if (!isMountedRef.current) return;
      if (r.ok) {
        const list = await r.json();
        if (isMountedRef.current && Array.isArray(list)) {
          setDetectedPlates(
            list.map((p) => normalizeLicensePlate(String(p))).filter(Boolean)
          );
        }
      }
    } catch {
      // ignore
    }
  };

  useEffect(() => {
    void fetchDetectedPlates();
  }, []);

  const updateLpr = (patch: Partial<LprConfig>, persistImmediately = false) => {
    const mergedLpr: LprConfig = {
      enabled: draftConfig.lpr?.enabled ?? false,
      min_confidence: draftConfig.lpr?.min_confidence ?? 0.75,
      format_regex: draftConfig.lpr?.format_regex ?? DEFAULT_LPR_FORMAT_REGEX,
      detector_model_path: draftConfig.lpr?.detector_model_path,
      ocr_model_path: draftConfig.lpr?.ocr_model_path,
      known_plates: { ...(draftConfig.lpr?.known_plates || {}) },
      ...patch,
    };
    if (persistImmediately && activeWorkbench) {
      const seq = ++lprPersistSeqRef.current;
      void activeWorkbench
        .applyOutOfBandPatch({ lpr: mergedLpr })
        .then((res) => {
          if (seq === lprPersistSeqRef.current && res.ok && res.config) {
            useNvrStore.getState().setConfig(res.config);
          }
        })
        .catch(() => {});
      return;
    }
    updateDraft((next) => {
      next.lpr = mergedLpr;
    });
    if (persistImmediately && typeof window !== "undefined") {
      const seq = ++lprPersistSeqRef.current;
      void fetch("/api/config/structured", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ lpr: mergedLpr }),
      })
        .then((r) => r.json().catch(() => null))
        .then((res) => {
          if (seq === lprPersistSeqRef.current && res?.config) {
            useNvrStore.getState().setConfig(res.config);
          }
        })
        .catch(() => {});
    }
  };

  const handleSimulateLprPass = async () => {
    if (simLprBusy) return;
    const targetCam = simLprCamera || inspectorCam || cameraKeys[0] || "";
    if (!targetCam) {
      notify(
        isPt
          ? "Configure uma câmera antes de simular detecção LPR"
          : "Configure a camera before simulating LPR detection"
      );
      return;
    }
    setSimLprBusy(true);
    try {
      const plateVal = normalizeLicensePlate(simLprPlate) || "BRA2E19";
      const res = await fetch("/api/debug/lpr-simulate", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          camera: targetCam,
          plate: plateVal,
          label: "car",
          score: 0.94,
        }),
      });
      const data = await res.json().catch(() => null);
      if (!isMountedRef.current) return;
      if (res.ok && data?.success) {
        const subLabel =
          data.sub_label ||
          lprCfg.known_plates?.[plateVal] ||
          null;
        setSimLprLastResult({
          eventId: String(data.event_id || ""),
          plate: String(data.plate || plateVal),
          camera: String(data.camera || targetCam),
          subLabel,
        });
        void fetchDetectedPlates();
        useNvrStore.getState().bumpSubLabelsRefresh();
        notify(
          subLabel
            ? `Simulated LPR detection: ${data.plate} → ${subLabel} on ${targetCam} (Event #${String(
                data.event_id || ""
              ).slice(-8)})`
            : `Simulated LPR detection: ${data.plate} on ${targetCam} (Event #${String(
                data.event_id || ""
              ).slice(-8)})`
        );
      } else {
        notify(data?.message || "Failed to simulate LPR vehicle pass");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Failed to simulate LPR vehicle pass");
      }
    } finally {
      if (isMountedRef.current) {
        setSimLprBusy(false);
      }
    }
  };

  const availableSimCameras =
    cameraKeys.length > 0 ? cameraKeys : inspectorCam ? [inspectorCam] : [];

  return (
    <>
      {scopeDivider("global")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>
              {isPt ? "RECONHECIMENTO DE PLACAS (LPR)" : "LICENSE PLATE RECOGNITION (LPR)"}
            </span>
            <span className="nvr-yaml-key-tag">lpr</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span className={`nvr-badge ${lprCfg.enabled ? "live" : ""}`}>
              {lprCfg.enabled
                ? isPt
                  ? "ATIVO"
                  : "ACTIVE"
                : isPt
                ? "EM ESPERA (OFF)"
                : "STANDBY (OFF)"}{" "}
              · {Object.keys(lprCfg.known_plates || {}).length}{" "}
              {isPt ? "PLACAS REGISTRADAS" : "KNOWN PLATES"}
            </span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
            <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? "Ativar OCR de Placas Multi-Frame (LPR / ANPR)"
                    : "Enable Multi-Frame License Plate Recognition (LPR)"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Consolida leituras OCR por votação majoritária ponderada entre frames consecutivos (Mercosul ABC1D23 e legado ABC1234)."
                    : "Aggregates CTC/OCR character reads via confidence-weighted majority voting across track frames (Mercosul ABC1D23 & standard)."}
                </div>
              </div>
              <ToggleSwitch
                checked={Boolean(lprCfg.enabled)}
                label="Enable Multi-Frame License Plate Recognition"
                onChange={(val) => updateLpr({ enabled: val })}
              />
            </div>

            <div
              className={`nvr-field-row ${
                !lprCfg.enabled ? "nvr-cfg-disabled-zone" : ""
              }`}
              style={{ borderBottom: "none", padding: "2px 0" }}
            >
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? `Confiança Mínima de Consenso OCR (${Math.round(
                        (lprCfg.min_confidence ?? 0.75) * 100
                      )}%)`
                    : `Minimum OCR Consensus Confidence (${Math.round(
                        (lprCfg.min_confidence ?? 0.75) * 100
                      )}%)`}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Pontuação mínima após votação multi-frame para gravar recognized_license_plate."
                    : "Minimum consolidated character score required to record recognized_license_plate."}
                </div>
              </div>
              <input
                type="range"
                min={50}
                max={98}
                disabled={!lprCfg.enabled}
                aria-label="Minimum LPR Confidence"
                value={Math.round((lprCfg.min_confidence ?? 0.75) * 100)}
                onChange={(e) =>
                  updateLpr({
                    min_confidence: Number((Number(e.target.value) / 100).toFixed(2)),
                  })
                }
              />
            </div>
          </div>

          <div
            className={!lprCfg.enabled ? "nvr-cfg-disabled-zone" : ""}
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
              gap: 10,
              alignItems: "end",
            }}
          >
            <div style={{ gridColumn: "1 / -1" }}>
              <div
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                  display: "flex",
                  justifyContent: "space-between",
                  gap: 6,
                  flexWrap: "wrap",
                }}
              >
                <span>
                  {isPt
                    ? "Expressão Regular de Validação (format_regex)"
                    : "Plate Format Validation Regex (format_regex)"}
                </span>
                <button
                  type="button"
                  className="nvr-btn mono sm"
                  onClick={() => updateLpr({ format_regex: DEFAULT_LPR_FORMAT_REGEX })}
                >
                  {isPt
                    ? "Restaurar Padrão (Mercosul + Legado)"
                    : "Reset Default (Mercosul + Legacy)"}
                </button>
              </div>
              <input
                className="nvr-input mono"
                aria-label="LPR Format Regex"
                disabled={!lprCfg.enabled}
                style={{ width: "100%", marginTop: 3, fontFamily: "var(--font-mono)" }}
                placeholder={DEFAULT_LPR_FORMAT_REGEX}
                value={lprCfg.format_regex ?? ""}
                onChange={(e) => updateLpr({ format_regex: e.target.value }, false)}
              />
            </div>

            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt
                  ? "Modelo Detector de Placa (Opcional)"
                  : "Plate Detector Model (Optional)"}
              </div>
              <input
                className="nvr-input mono"
                disabled={!lprCfg.enabled}
                style={{ width: "100%", marginTop: 3, fontFamily: "var(--font-mono)" }}
                placeholder="/config/models/lpr_det.onnx"
                value={lprCfg.detector_model_path ?? ""}
                onChange={(e) =>
                  updateLpr(
                    {
                      detector_model_path: e.target.value.trim() || undefined,
                    },
                    false
                  )
                }
              />
            </div>

            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Modelo OCR CTC (Opcional)" : "Plate OCR CTC Model (Optional)"}
              </div>
              <input
                className="nvr-input mono"
                disabled={!lprCfg.enabled}
                style={{ width: "100%", marginTop: 3, fontFamily: "var(--font-mono)" }}
                placeholder="/config/models/lpr_ocr.onnx"
                value={lprCfg.ocr_model_path ?? ""}
                onChange={(e) =>
                  updateLpr(
                    {
                      ocr_model_path: e.target.value.trim() || undefined,
                    },
                    false
                  )
                }
              />
            </div>
          </div>

          <details className="nvr-diag-drawer">
            <summary>
              <span>
                {isPt
                  ? "▸ FERRAMENTA DE DIAGNÓSTICO: SIMULAR PASSAGEM DE VEÍCULO (TESTE DE PIPELINE LPR)"
                  : "▸ DIAGNOSTIC TOOL: SIMULATE VEHICLE PASS (TEST LPR PIPELINE)"}
              </span>
              <span className="nvr-yaml-key-tag">/api/debug/lpr-simulate</span>
            </summary>
            <div
              style={{
                display: "flex",
                flexDirection: "column",
                gap: 8,
                marginTop: 8,
                paddingTop: 8,
                borderTop: "1px solid var(--border-divider)",
              }}
            >
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  flexWrap: "wrap",
                  gap: 8,
                }}
              >
                <span style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt
                    ? "Gera um frame sintético Mercosul, executa votação multi-frame LprTrackVoter + lookup known_plates e grava um evento de teste."
                    : "Generates a synthetic Mercosul plate frame, runs LprTrackVoter multi-frame consensus + known_plates lookup, and persists a test event."}
                </span>

                <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                  <select
                    className="nvr-select mono sm"
                    aria-label="Simulate LPR Camera"
                    value={simLprCamera}
                    onChange={(e) => setSimLprCamera(e.target.value)}
                  >
                    {availableSimCameras.map((c) => (
                      <option key={c} value={c}>
                        {c.toUpperCase()}
                      </option>
                    ))}
                  </select>
                  <input
                    className="nvr-input mono sm"
                    aria-label="Simulate LPR Plate"
                    style={{ width: 110, fontFamily: "var(--font-mono)" }}
                    placeholder="BRA2E19"
                    value={simLprPlate}
                    onChange={(e) => setSimLprPlate(e.target.value.toUpperCase())}
                  />
                  {(["BRA2E19", "ABC1D23", "XYZ9876"] as const).map((presetPlate) => (
                    <button
                      key={presetPlate}
                      type="button"
                      className={`nvr-btn mono ${
                        normalizeLicensePlate(simLprPlate) === presetPlate ? "active" : ""
                      } sm`}
                      onClick={() => setSimLprPlate(presetPlate)}
                    >
                      {presetPlate}
                    </button>
                  ))}
                  <button
                    type="button"
                    className="nvr-btn success mono sm"
                    disabled={simLprBusy || availableSimCameras.length === 0}
                    onClick={() => void handleSimulateLprPass()}
                  >
                    {simLprBusy
                      ? isPt
                        ? "Simulando..."
                        : "Simulating..."
                      : isPt
                      ? "Simular Detecção de Placa Agora"
                      : "Simulate Plate Detection Now"}
                  </button>
                </div>
              </div>
              {simLprLastResult && (
                <div
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    color: "var(--signal-live-text)",
                  }}
                >
                  ✓ Simulated Event #{simLprLastResult.eventId.slice(-8)} on{" "}
                  {simLprLastResult.camera.toUpperCase()} · Plate:{" "}
                  <strong>{simLprLastResult.plate}</strong>
                  {simLprLastResult.subLabel
                    ? ` · Matched Known Vehicle: ${simLprLastResult.subLabel}`
                    : ""}
                </div>
              )}
            </div>
          </details>
        </div>
      </div>

      <div className={`nvr-cfg-card ${showKnownPlates ? "" : "is-collapsed"}`}>
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <CardCollapseToggle
              open={showKnownPlates}
              onToggle={() => setShowKnownPlates((v) => !v)}
              title={isPt ? "PLACAS CONHECIDAS" : "KNOWN PLATES"}
              count={Object.keys(lprCfg.known_plates || {}).length}
              controlsId="nvr-known-plates-body"
            />
            <span className="nvr-yaml-key-tag">lpr.known_plates</span>
          </div>
        </div>
        {showKnownPlates && (
          <div id="nvr-known-plates-body" className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
            <div className="nvr-subcard" style={{ padding: "8px 10px" }}>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  flexWrap: "wrap",
                  gap: 8,
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                  <span
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      fontWeight: 700,
                      color: "var(--signal-accent-text)",
                    }}
                  >
                    {isPt ? "DIRETÓRIO DE PLACAS CONHECIDAS" : "KNOWN PLATES REGISTRY"}
                  </span>
                  <span className="nvr-yaml-key-tag">lpr.known_plates</span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                  <input
                    className="nvr-input mono sm"
                    aria-label="Known plate number"
                    style={{ width: 120, fontFamily: "var(--font-mono)" }}
                    placeholder="ABC1D23"
                    value={newKnownPlate}
                    onChange={(e) => setNewKnownPlate(e.target.value.toUpperCase())}
                  />
                  <input
                    className="nvr-input sm"
                    aria-label="Known plate vehicle label"
                    style={{ width: 190 }}
                    placeholder={isPt ? "Ex: SUV Prata" : "e.g. Silver SUV"}
                    value={newKnownPlateLabel}
                    onChange={(e) => setNewKnownPlateLabel(e.target.value)}
                  />
                  <button
                    type="button"
                    className="nvr-btn primary mono sm"
                    onClick={() => {
                      const norm = normalizeLicensePlate(newKnownPlate || simLprPlate);
                      if (!norm) {
                        notify(
                          isPt
                            ? "Digite uma placa válida (ex: BRA2E19)"
                            : "Enter a valid license plate (e.g. BRA2E19)"
                        );
                        return;
                      }
                      const vehicleLabel =
                        newKnownPlateLabel.trim() ||
                        (isPt ? `Veículo ${norm}` : `Vehicle ${norm}`);
                      if (!validateLicensePlateFormat(norm, lprCfg.format_regex)) {
                        notify(`Plate ${norm} does not match configured format_regex`);
                      }
                      updateLpr({
                        enabled: true,
                        known_plates: upsertKnownPlateEntry(
                          lprCfg.known_plates,
                          norm,
                          vehicleLabel
                        ),
                      });
                      setNewKnownPlate("");
                      setNewKnownPlateLabel("");
                      notify(
                        isPt
                          ? `Placa conhecida adicionada ao rascunho: ${norm} → ${vehicleLabel}`
                          : `Staged known plate: ${norm} → ${vehicleLabel}`
                      );
                    }}
                  >
                    <Plus size={11} />
                    {isPt ? "Registrar Placa" : "Add Known Plate"}
                  </button>
                </div>
              </div>

              <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
                {Object.entries(lprCfg.known_plates || {}).map(([plateKey, vehicleLabel]) => {
                  const rmKey = `rm-plate:${plateKey}`;
                  const rmArmed = isArmed(rmKey);
                  return (
                    <span
                      key={plateKey}
                      className="nvr-badge mono"
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 6,
                        fontFamily: "var(--font-mono)",
                        fontSize: "var(--text-xs)",
                        padding: "2px 7px",
                        borderColor: "var(--signal-live)",
                      }}
                    >
                      <strong style={{ color: "var(--signal-live-text)" }}>{plateKey}</strong>
                      <span style={{ color: "var(--text-primary)" }}>→ {vehicleLabel}</span>
                      <button
                        type="button"
                        className="nvr-btn danger mono sm"
                        aria-label={`Remove known plate ${plateKey}`}
                        onClick={() =>
                          confirmOrArm(rmKey, () =>
                            updateLpr({
                              known_plates: removeKnownPlateEntry(
                                lprCfg.known_plates,
                                plateKey
                              ),
                            })
                          )
                        }
                      >
                        {rmArmed ? (isPt ? "Confirmar?" : "Confirm?") : "×"}
                      </button>
                    </span>
                  );
                })}
                {Object.keys(lprCfg.known_plates || {}).length === 0 && (
                  <span style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                    {isPt
                      ? "Nenhuma placa conhecida cadastrada. Adicione placas acima ou clique em uma placa detectada abaixo para cadastrá-la."
                      : "No known plates registered yet. Add plates above or click a detected plate below to register it."}
                  </span>
                )}
              </div>

              {detectedPlates.length > 0 && (
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 6,
                    flexWrap: "wrap",
                    paddingTop: 6,
                    borderTop: "1px solid var(--border-divider)",
                  }}
                >
                  <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {isPt
                      ? "PLACAS DETECTADAS EM EVENTOS:"
                      : "DETECTED PLATES IN EVENTS:"}
                  </span>
                  {detectedPlates.map((dp) => {
                    const matchedName = resolveKnownPlateLabel(dp, lprCfg.known_plates);
                    return (
                      <span
                        key={dp}
                        className="nvr-badge mono"
                        style={{
                          display: "inline-flex",
                          alignItems: "center",
                          gap: 6,
                          padding: "2px 7px",
                          borderColor: matchedName
                            ? "var(--signal-live)"
                            : "var(--signal-motion)",
                        }}
                      >
                        <strong
                          style={{
                            color: matchedName
                              ? "var(--signal-live-text)"
                              : "var(--signal-motion-text)",
                          }}
                        >
                          {dp}
                        </strong>
                        {matchedName ? (
                          <span style={{ color: "var(--text-secondary)" }}>
                            ✓ {matchedName}
                          </span>
                        ) : (
                          <button
                            type="button"
                            className="nvr-btn primary mono sm"
                            onClick={() => {
                              const label =
                                newKnownPlateLabel.trim() ||
                                (dp === "BRA2E19"
                                  ? isPt
                                    ? "SUV Prata (BRA2E19)"
                                    : "Silver SUV (BRA2E19)"
                                  : isPt
                                  ? `Veículo ${dp}`
                                  : `Vehicle ${dp}`);
                              updateLpr({
                                enabled: true,
                                known_plates: upsertKnownPlateEntry(
                                  lprCfg.known_plates,
                                  dp,
                                  label
                                ),
                              });
                              notify(
                                isPt
                                  ? `Placa ${dp} adicionada ao rascunho como '${label}'`
                                  : `Staged ${dp} as '${label}'`
                              );
                            }}
                          >
                            + {isPt ? "Cadastrar" : "Register"}
                          </button>
                        )}
                      </span>
                    );
                  })}
                </div>
              )}
            </div>
          </div>
        )}
      </div>
    </>
  );
};
