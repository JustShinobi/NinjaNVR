import React, { useEffect, useRef, useState } from "react";
import { Plus, Trash2 } from "lucide-react";
import { CardCollapseToggle } from "../../../components/CardCollapseToggle";
import { useNvrStore } from "../../../store/useNvrStore";
import {
  EnrolledFaceProfileItem,
  FaceRecognitionConfig,
  NinjaConfigResponse,
} from "../../../types/nvr";
import {
  createForensicTriageController,
  normalizeFaceProfileName,
} from "../../../utils/forensicTriage";
import type { ScopedConfigLens } from "../../../utils/scopedConfigDraft";
import { ToggleSwitch } from "../ConsoleAndStreamsPanels";
import { useTwoStepConfirm } from "./useTwoStepConfirm";

export interface FaceRecognitionSectionProps {
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  configLens: ScopedConfigLens;
  isPt: boolean;
  notify: (msg: string) => void;
  initialDataCardsOpen: boolean;
  scopeDivider: (group: "global" | "camera") => React.ReactNode;
}

export const FaceRecognitionSection: React.FC<FaceRecognitionSectionProps> = ({
  draftConfig,
  updateDraft,
  configLens,
  isPt,
  notify,
  initialDataCardsOpen,
  scopeDivider,
}) => {
  const { isArmed, confirmOrArm } = useTwoStepConfirm(3000);
  const [showFaceGallery, setShowFaceGallery] = useState(initialDataCardsOpen);
  const [enrolledProfiles, setEnrolledProfiles] = useState<EnrolledFaceProfileItem[]>([]);
  const [enrollNameInput, setEnrollNameInput] = useState("");
  const [enrollImageB64, setEnrollImageB64] = useState<string>("");
  const [enrollFileName, setEnrollFileName] = useState<string>("");
  const [enrollBusy, setEnrollBusy] = useState(false);
  const [excludeSubLabelInput, setExcludeSubLabelInput] = useState("");
  const isMountedRef = useRef(true);
  const enrollFileReaderRef = useRef<FileReader | null>(null);
  const triageControllerRef = useRef(createForensicTriageController());

  const faceCfg: FaceRecognitionConfig = draftConfig.face_recognition || {
    enabled: false,
    model_path: "/config/models/arcface_r50.onnx",
    similarity_threshold: 0.68,
    min_face_area: 750,
    unknown_alert: false,
  };
  const activeExcludeSubLabels: string[] = configLens.readExcludedSubLabels();

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
      if (enrollFileReaderRef.current) {
        try {
          enrollFileReaderRef.current.abort();
        } catch {
          // ignore
        }
        enrollFileReaderRef.current = null;
      }
    };
  }, []);

  const refreshEnrolledFaces = async (excludeDeletedProfile?: string) => {
    const profiles = await triageControllerRef.current.fetchEnrolledFaces(
      excludeDeletedProfile ? { excludeDeletedProfile } : undefined
    );
    if (isMountedRef.current) {
      setEnrolledProfiles(profiles);
    }
  };

  useEffect(() => {
    void refreshEnrolledFaces();
  }, []);

  const updateFaceRecognition = (patch: Partial<FaceRecognitionConfig>) => {
    updateDraft((next) => {
      next.face_recognition = {
        enabled: faceCfg.enabled ?? false,
        model_path: faceCfg.model_path || "/config/models/arcface_r50.onnx",
        similarity_threshold: faceCfg.similarity_threshold ?? 0.68,
        min_face_area: faceCfg.min_face_area ?? 750,
        unknown_alert: faceCfg.unknown_alert ?? false,
        ...patch,
      };
    });
  };

  const handleToggleExcludeSubLabel = (name: string) => {
    updateDraft((next) => {
      configLens.toggleExcludedSubLabel(next, name);
    });
  };

  const handleEnrollProfile = async () => {
    const cleanName = normalizeFaceProfileName(enrollNameInput);
    if (!cleanName) {
      notify("Enter a valid profile name (e.g. Bruno) before enrolling");
      return;
    }
    setEnrollBusy(true);
    try {
      const res = await fetch(`/api/faces/${encodeURIComponent(cleanName)}/enroll`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(
          enrollImageB64 ? { image_base64: enrollImageB64 } : {}
        ),
      });
      if (!isMountedRef.current) return;
      if (res.ok) {
        setEnrollNameInput("");
        setEnrollImageB64("");
        setEnrollFileName("");
        await refreshEnrolledFaces();
        if (isMountedRef.current) {
          notify(`Enrolled face profile '${cleanName}' into local gallery`);
        }
      } else {
        const data = await res.json().catch(() => null);
        if (isMountedRef.current) {
          notify(data?.message || `Failed to enroll face profile '${cleanName}'`);
        }
      }
    } catch {
      if (isMountedRef.current) {
        notify(`Network error while enrolling face profile '${cleanName}'`);
      }
    } finally {
      if (isMountedRef.current) {
        setEnrollBusy(false);
      }
    }
  };

  const handleDeleteProfile = async (name: string) => {
    try {
      const res = await fetch(`/api/faces/${encodeURIComponent(name)}`, {
        method: "DELETE",
        credentials: "include",
      });
      if (!isMountedRef.current) return;
      if (res.ok) {
        const data = await res.json().catch(() => ({}));
        if (!isMountedRef.current) return;
        setEnrolledProfiles((prev) =>
          prev.filter((p) => p.name.trim().toLowerCase() !== name.trim().toLowerCase())
        );
        useNvrStore.getState().recordDeletedFaceProfile(name);
        await refreshEnrolledFaces(name);
        if (!isMountedRef.current) return;
        const clearedCount = Number(data?.cleared_events || 0);
        notify(
          clearedCount > 0
            ? `Removed face profile '${name}' from gallery and cleared ${clearedCount} event(s)`
            : `Removed face profile '${name}' from gallery`
        );
      } else {
        notify(`Failed to delete face profile '${name}'`);
      }
    } catch {
      if (isMountedRef.current) {
        notify(`Network error while deleting face profile '${name}'`);
      }
    }
  };

  return (
    <>
      {scopeDivider("global")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "RECONHECIMENTO FACIAL" : "FACE RECOGNITION"}</span>
            <span className="nvr-yaml-key-tag">face_recognition</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span className={`nvr-badge ${faceCfg.enabled ? "live" : ""}`}>
              {faceCfg.enabled
                ? isPt
                  ? "ATIVO"
                  : "ACTIVE"
                : isPt
                ? "EM ESPERA (OFF)"
                : "STANDBY (OFF)"}{" "}
              · {enrolledProfiles.length}{" "}
              {isPt ? "PERFIS CADASTRADOS" : "ENROLLED PROFILES"}
            </span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
          <div style={{ display: "flex", flexDirection: "column" }}>
            <div className="nvr-field-row" style={{ padding: "4px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? "Ativar Reconhecimento Facial Local"
                    : "Enable Local Face Recognition"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Extrai vetor 512-d L2-normalizado da face e atribui sub_label (ex: Bruno)."
                    : "Extracts 512-d L2-normalized embedding from face crops and assigns sub_label."}
                </div>
              </div>
              <ToggleSwitch
                checked={faceCfg.enabled ?? false}
                label="Enable Local Face Recognition"
                onChange={(v) => updateFaceRecognition({ enabled: v })}
              />
            </div>

            <div
              className={`nvr-field-row ${
                !faceCfg.enabled ? "nvr-cfg-disabled-zone" : ""
              }`}
              style={{ padding: "4px 0" }}
            >
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? "Alerta para Pessoas Desconhecidas (unknown_alert)"
                    : "Flag Unrecognized Faces as 'unknown' Alert"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Define sub_label='unknown' abaixo do limiar de similaridade para acionar alerta."
                    : "Marks faces below similarity threshold as 'unknown' to trigger priority alerts."}
                </div>
              </div>
              <ToggleSwitch
                checked={faceCfg.unknown_alert ?? false}
                label="Flag Unrecognized Faces as unknown"
                onChange={(v) => updateFaceRecognition({ unknown_alert: v })}
              />
            </div>
          </div>

          <div
            className={!faceCfg.enabled ? "nvr-cfg-disabled-zone" : ""}
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
              gap: 10,
              alignItems: "end",
            }}
          >
            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Limiar de Similaridade Cosseno:" : "Cosine Similarity Threshold:"}{" "}
                <strong style={{ color: "var(--text-primary)" }}>
                  {(faceCfg.similarity_threshold ?? 0.68).toFixed(2)} (
                  {Math.round((faceCfg.similarity_threshold ?? 0.68) * 100)}%)
                </strong>
              </div>
              <input
                type="range"
                min={40}
                max={95}
                disabled={!faceCfg.enabled}
                style={{ width: "100%", marginTop: 4 }}
                aria-label="Face Recognition Similarity Threshold"
                value={Math.round((faceCfg.similarity_threshold ?? 0.68) * 100)}
                onChange={(e) =>
                  updateFaceRecognition({
                    similarity_threshold: Number(
                      (Number(e.target.value) / 100).toFixed(2)
                    ),
                  })
                }
              />
            </div>

            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt ? "Área Mínima da Face (px²)" : "Min Face Area (px²)"}
              </div>
              <input
                type="number"
                min={100}
                max={50000}
                step={50}
                disabled={!faceCfg.enabled}
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                aria-label="Minimum Face Crop Area"
                value={faceCfg.min_face_area ?? 750}
                onChange={(e) =>
                  updateFaceRecognition({ min_face_area: Number(e.target.value) })
                }
              />
            </div>

            <div>
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                {isPt
                  ? "Modelo de Embedding ONNX / OpenVINO"
                  : "Embedding Model Path (.onnx / .xml)"}
              </div>
              <input
                className="nvr-input mono"
                disabled={!faceCfg.enabled}
                style={{ width: "100%", marginTop: 2 }}
                aria-label="Face Recognition Model Path"
                value={faceCfg.model_path || "/config/models/arcface_r50.onnx"}
                onChange={(e) =>
                  updateFaceRecognition({ model_path: e.target.value })
                }
              />
            </div>
          </div>
        </div>
      </div>

      <div className={`nvr-cfg-card ${showFaceGallery ? "" : "is-collapsed"}`}>
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <CardCollapseToggle
              open={showFaceGallery}
              onToggle={() => setShowFaceGallery((v) => !v)}
              title={isPt ? "PERFIS CADASTRADOS" : "ENROLLED FACES"}
              count={enrolledProfiles.length}
              controlsId="nvr-face-gallery-body"
            />
          </div>
        </div>
        {showFaceGallery && (
          <div id="nvr-face-gallery-body" className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
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
                  <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700 }}>
                    {isPt ? "CADASTRAR NOVO PERFIL FACIAL:" : "ENROLL NEW FACE PROFILE:"}
                  </span>
                  <span className="nvr-yaml-key-tag">/media/ninjanvr/faces (Live Gallery)</span>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                  <input
                    className="nvr-input mono sm"
                    style={{ width: 220 }}
                    placeholder={
                      isPt ? "Nome do perfil (ex: Bruno)" : "Profile name (e.g. Bruno)"
                    }
                    aria-label="Face profile name to enroll"
                    value={enrollNameInput}
                    onChange={(e) => setEnrollNameInput(e.target.value)}
                  />
                  <label className="nvr-file-picker">
                    <span className="nvr-btn mono sm">
                      {isPt ? "Escolher Foto..." : "Choose Photo..."}
                    </span>
                    <span
                      className="mono"
                      style={{
                        maxWidth: 150,
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                        paddingRight: 4,
                        color: enrollFileName
                          ? "var(--text-primary)"
                          : "var(--text-muted)",
                      }}
                    >
                      {enrollFileName ||
                        (isPt ? "Nenhum arquivo" : "No file selected")}
                    </span>
                    <input
                      type="file"
                      accept="image/*"
                      aria-label="Upload face crop image"
                      style={{ display: "none" }}
                      onChange={(e) => {
                        const file = e.target.files?.[0];
                        if (enrollFileReaderRef.current) {
                          try {
                            enrollFileReaderRef.current.abort();
                          } catch {
                            // ignore
                          }
                          enrollFileReaderRef.current = null;
                        }
                        if (!file) {
                          setEnrollImageB64("");
                          setEnrollFileName("");
                          return;
                        }
                        setEnrollFileName(file.name);
                        const reader = new FileReader();
                        enrollFileReaderRef.current = reader;
                        reader.onload = () => {
                          enrollFileReaderRef.current = null;
                          if (isMountedRef.current && typeof reader.result === "string") {
                            setEnrollImageB64(reader.result);
                          }
                        };
                        reader.readAsDataURL(file);
                      }}
                    />
                  </label>
                  <button
                    type="button"
                    className="nvr-btn primary mono sm"
                    disabled={enrollBusy}
                    onClick={handleEnrollProfile}
                  >
                    <Plus size={11} />
                    {enrollBusy
                      ? isPt
                        ? "Cadastrando..."
                        : "Enrolling..."
                      : isPt
                      ? "Cadastrar Face"
                      : "Enroll Face"}
                  </button>
                </div>
              </div>

              <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
                {enrolledProfiles.map((prof) => {
                  const delKey = `delete-face:${prof.name}`;
                  const delArmed = isArmed(delKey);
                  return (
                    <div
                      key={prof.name}
                      className="mono"
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 8,
                        padding: "4px 8px",
                        background: "var(--bg-input)",
                        border: "1px solid var(--border-hairline)",
                        borderRadius: 3,
                        fontSize: "var(--text-xs)",
                      }}
                    >
                      <strong>{prof.name}</strong>
                      <span style={{ color: "var(--text-muted)" }}>
                        ({prof.sample_count} {prof.sample_count === 1 ? "sample" : "samples"})
                      </span>
                      <button
                        type="button"
                        className="nvr-btn danger mono sm"
                        aria-label={`Delete face profile ${prof.name}`}
                        onClick={() => {
                          confirmOrArm(delKey, () => {
                            void handleDeleteProfile(prof.name);
                          });
                        }}
                      >
                        <Trash2 size={11} />
                        {delArmed ? (isPt ? "Confirmar?" : "Confirm?") : null}
                      </button>
                    </div>
                  );
                })}
                {enrolledProfiles.length === 0 && (
                  <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                    {isPt
                      ? "Nenhuma face cadastrada em /media/ninjanvr/faces ainda."
                      : "No faces enrolled in /media/ninjanvr/faces yet."}
                  </span>
                )}
              </div>
            </div>
          </div>
        )}
      </div>

      {scopeDivider("camera")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "SUPRESSÃO DE ALERTA PARA MORADORES" : "RESIDENT ALERT SUPPRESSION"}</span>
            <span className="nvr-yaml-key-tag">review.alerts.exclude_sub_labels</span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
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
                <span className="nvr-field-desc">
                  {isPt
                    ? "Moradores listados aqui viram detecção em vez de alerta."
                    : "Residents listed here are downgraded from alert to detection."}
                </span>
              </div>
              <div style={{ display: "flex", gap: 6, alignItems: "center", flexWrap: "wrap" }}>
                <input
                  className="nvr-input mono sm"
                  style={{ width: 220 }}
                  placeholder={
                    isPt ? "Nome do morador (ex: Bruno)" : "Resident name (e.g. Bruno)"
                  }
                  aria-label="Exclude sub-label resident name"
                  value={excludeSubLabelInput}
                  onChange={(e) => setExcludeSubLabelInput(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn mono sm"
                  onClick={() => {
                    const clean = normalizeFaceProfileName(excludeSubLabelInput);
                    if (!clean) return;
                    handleToggleExcludeSubLabel(clean);
                    setExcludeSubLabelInput("");
                  }}
                >
                  {isPt ? "+ Suprimir Alerta" : "+ Suppress Alert"}
                </button>
              </div>
            </div>
            <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
              {Array.from(
                new Set([
                  ...enrolledProfiles.map((p) => p.name),
                  ...activeExcludeSubLabels,
                ])
              ).map((personName) => {
                const isExcluded = activeExcludeSubLabels.some(
                  (x) => x.toLowerCase() === personName.toLowerCase()
                );
                return (
                  <button
                    key={personName}
                    type="button"
                    className={`nvr-btn mono ${isExcluded ? "active" : ""} sm`}
                    onClick={() => handleToggleExcludeSubLabel(personName)}
                    title={
                      isPt
                        ? "Alternar rebaixamento de alerta (alerta -> detecção) quando este morador for reconhecido"
                        : "Toggle alert downgrade (alert -> detection) when this resident is recognized"
                    }
                  >
                    {isExcluded
                      ? isPt
                        ? `✓ ${personName} (Rebaixado p/ Detecção)`
                        : `✓ ${personName} (Downgraded to Detection)`
                      : isPt
                      ? `${personName} (Gera Alerta)`
                      : `${personName} (Triggers Alert)`}
                  </button>
                );
              })}
              {enrolledProfiles.length === 0 && activeExcludeSubLabels.length === 0 && (
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                  {isPt
                    ? "Nenhum morador configurado para supressão de alerta. Cadastre um rosto em Perfis Cadastrados ou adicione o nome acima."
                    : "No resident sub_labels excluded yet. Enroll a face under Enrolled Faces or add a name above."}
                </span>
              )}
            </div>
          </div>
        </div>
      </div>
    </>
  );
};
