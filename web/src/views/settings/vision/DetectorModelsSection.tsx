import React, { useState } from "react";
import { CheckCircle2, ChevronDown, ChevronUp, Cpu } from "lucide-react";
import {
  CameraDetectConfig,
  ModelConfig,
  NinjaConfigResponse,
  NinjaStatsResponse,
} from "../../../types/nvr";
import { formatMeasured, resolvePrimaryInferenceMs } from "../../../utils/detectorTelemetry";
import type { ScopedConfigLens } from "../../../utils/scopedConfigDraft";
import { ToggleSwitch } from "../ConsoleAndStreamsPanels";

export interface DetectorModelsSectionProps {
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  configLens: ScopedConfigLens;
  stats: NinjaStatsResponse | null;
  isPt: boolean;
  notify: (msg: string) => void;
  scopeDivider: (group: "global" | "camera") => React.ReactNode;
}

export const DetectorModelsSection: React.FC<DetectorModelsSectionProps> = ({
  draftConfig,
  updateDraft,
  configLens,
  stats,
  isPt,
  notify,
  scopeDivider,
}) => {
  const [showAdvancedModel, setShowAdvancedModel] = useState(false);
  const [showAdvancedByteTrack, setShowAdvancedByteTrack] = useState(false);
  const [isCustomResolution, setIsCustomResolution] = useState(false);

  const detKeys = Object.keys(draftConfig.detectors || { detector01: {} });
  const primaryDetKey = detKeys[0] || "detector01";
  const primaryDet = draftConfig.detectors?.[primaryDetKey] || {
    type: "openvino",
    device: "CPU",
  };
  const liveInferMs = formatMeasured(resolvePrimaryInferenceMs(stats), 1, "ms");

  const modelCfg: ModelConfig = draftConfig.model || {
    path: "/openvino-model/ssdlite_mobilenet_v2.xml",
    labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
    width: 300,
    height: 300,
    input_tensor: "nhwc",
    input_pixel_format: "bgr",
    model_type: "ssd",
  };

  const activeDetect: CameraDetectConfig = configLens.readDetect();
  const updateDetect = (patch: Partial<CameraDetectConfig>) => {
    updateDraft((next) => {
      configLens.mutateDetect(next, patch);
    });
  };

  const selectModelProfile = (arch: "ssd" | "yolo-generic" | "yolox") => {
    if (arch !== "ssd") {
      setShowAdvancedModel(true);
    }
    updateDraft((next) => {
      if (arch === "ssd") {
        next.model = {
          ...modelCfg,
          model_type: "ssd",
          width: 300,
          height: 300,
          input_tensor: "nhwc",
          input_pixel_format: "bgr",
          path: "/openvino-model/ssdlite_mobilenet_v2.xml",
        };
      } else if (arch === "yolo-generic") {
        next.model = {
          ...modelCfg,
          model_type: "yolo-generic",
          width: 640,
          height: 640,
          input_tensor: "nchw",
          input_pixel_format: "rgb",
        };
      } else {
        next.model = {
          ...modelCfg,
          model_type: "yolox",
          width: 320,
          height: 320,
          input_tensor: "nchw",
          input_pixel_format: "bgr",
        };
      }
    });
  };

  const currentResPreset = `${activeDetect.width}x${activeDetect.height}`;
  const isStandardRes = ["1280x720", "640x480", "640x360", "1920x1080"].includes(
    currentResPreset
  );

  const detFps = activeDetect.fps ?? 5;
  const detHits = activeDetect.min_initialized ?? 2;
  const activeTrackPreset: "balanced" | "fast" | "strict" | "custom" =
    detFps === 5 && detHits === 2
      ? "balanced"
      : detFps === 8 && detHits === 1
      ? "fast"
      : detFps === 5 && detHits === 4
      ? "strict"
      : "custom";

  const modelProfiles: Array<{
    id: "ssd" | "yolo-generic" | "yolox";
    title: string;
    badge: string;
    badgeKind: "live" | "motion" | "";
    desc: string;
    specs: string;
  }> = [
    {
      id: "ssd",
      title: isPt
        ? "CPU Rápido Integrado — MobileNetV2 SSD"
        : "Built-In Fast CPU — MobileNetV2 SSD",
      badge: isPt ? "RECOMENDADO · INTEGRADO" : "RECOMMENDED · BUNDLED",
      badgeKind: "live",
      desc: isPt
        ? "Pré-instalado (~7 ms/frame). Ideal para servidores CPU com baixíssimo uso de recursos."
        : "Pre-installed (~7 ms/frame). Ideal for CPU-only servers with near-zero CPU overhead.",
      specs: "300×300 · NHWC · BGR · 91 COCO Classes",
    },
    {
      id: "yolo-generic",
      title: isPt
        ? "Alta Precisão Customizado — YOLOv8 / YOLOv11"
        : "High-Precision Custom — YOLOv8 / YOLOv11",
      badge: "CUSTOM .ONNX / .XML",
      badgeKind: "",
      desc: isPt
        ? "Maior alcance para objetos distantes ao montar um arquivo .onnx/.xml em /config/."
        : "Higher accuracy on distant objects when mounting a custom .onnx/.xml in /config/.",
      specs: "640×640 · NCHW · RGB · 80 COCO Classes",
    },
    {
      id: "yolox",
      title: isPt
        ? "Edge Equilibrado Customizado — YOLOX"
        : "Balanced Edge Custom — YOLOX",
      badge: "CUSTOM .ONNX / .XML",
      badgeKind: "",
      desc: isPt
        ? "Detector YOLO anchor-free em 320×320 para modelos customizados em /config/."
        : "Anchor-free YOLO detector at 320×320 for custom models in /config/.",
      specs: "320×320 · NCHW · BGR · 80 COCO Classes",
    },
  ];

  return (
    <>
      {scopeDivider("global")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "MODELO DE DETECÇÃO IA" : "AI DETECTION MODEL"}</span>
            <span className="nvr-yaml-key-tag">model</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span className="nvr-badge live">
              <CheckCircle2 size={11} />
              {(primaryDet.type || "openvino").toUpperCase()} ·{" "}
              {primaryDet.device || "CPU"} ACTIVE · {liveInferMs}
            </span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 7 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            {modelProfiles.map((prof) => {
              const isSelected = (modelCfg.model_type || "ssd") === prof.id;
              return (
                <button
                  key={prof.id}
                  type="button"
                  aria-pressed={isSelected}
                  onClick={() => selectModelProfile(prof.id)}
                  className="nvr-subcard"
                  style={{
                    padding: "7px 10px",
                    background: isSelected
                      ? "rgba(14, 165, 233, 0.12)"
                      : "var(--bg-subcard)",
                    borderColor: isSelected
                      ? "var(--signal-accent)"
                      : "var(--border-hairline)",
                    cursor: "pointer",
                    gap: 2,
                    textAlign: "left",
                    width: "100%",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: 8,
                    }}
                  >
                    <span
                      style={{
                        fontSize: "var(--text-sm)",
                        fontWeight: 700,
                        color: isSelected
                          ? "var(--signal-accent-text)"
                          : "var(--text-primary)",
                      }}
                    >
                      {prof.title}
                    </span>
                    <span className={`nvr-badge ${prof.badgeKind}`}>{prof.badge}</span>
                  </div>
                  <div
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                    }}
                  >
                    {prof.desc}{" "}
                    <span className="mono" style={{ color: "var(--text-muted)" }}>
                      ({prof.specs})
                    </span>
                  </div>
                </button>
              );
            })}
          </div>
        </div>
      </div>

      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "RUNTIME DE INFERÊNCIA" : "INFERENCE RUNTIME"}</span>
            <span className="nvr-yaml-key-tag">detectors</span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 7 }}>
          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                <Cpu size={13} />
                {isPt ? "Motor de Inferência (Runtime)" : "Inference Runtime Backend"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "OpenVINO para x86_64/Intel; ONNX Runtime para NVIDIA CUDA/TensorRT, AMD ROCm e ARM64."
                  : "OpenVINO for x86_64/Intel; ONNX Runtime for NVIDIA CUDA/TensorRT, AMD ROCm, and ARM64."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              style={{ width: 310, maxWidth: "100%" }}
              aria-label="Inference Runtime Backend"
              value={
                (primaryDet.type || "openvino").toLowerCase() === "onnxruntime" ||
                (primaryDet.type || "openvino").toLowerCase() === "ort"
                  ? "onnxruntime"
                  : "openvino"
              }
              onChange={(e) => {
                const nextType = e.target.value;
                updateDraft((next) => {
                  if (!next.detectors) next.detectors = {};
                  const currDev = primaryDet.device || "CPU";
                  const nextDev =
                    nextType === "openvino" &&
                    (currDev === "CUDA" || currDev === "TensorRT" || currDev === "ROCM")
                      ? "CPU"
                      : nextType === "onnxruntime" &&
                        (currDev === "GPU" || currDev === "NPU")
                      ? "CUDA"
                      : currDev;
                  next.detectors[primaryDetKey] = {
                    ...primaryDet,
                    type: nextType,
                    device: nextDev,
                  };
                });
              }}
            >
              <option value="openvino">OpenVINO — CPU / Intel iGPU / NPU</option>
              <option value="onnxruntime">
                ONNX Runtime — CUDA / TensorRT / ROCm / ARM64
              </option>
            </select>
          </div>

          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                <Cpu size={13} />
                {isPt ? "Dispositivo de Hardware" : "Hardware Execution Device"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Fallback automático para CPU se GPU/NPU não estiver montada."
                  : "Auto-fallbacks to CPU if GPU/NPU is not mounted in container."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              style={{ width: 310, maxWidth: "100%" }}
              aria-label="OpenVINO Execution Device"
              value={primaryDet.device || "CPU"}
              onChange={(e) => {
                const dev = e.target.value;
                updateDraft((next) => {
                  if (!next.detectors) next.detectors = {};
                  next.detectors[primaryDetKey] = {
                    ...primaryDet,
                    type: primaryDet.type || "openvino",
                    device: dev,
                  };
                });
              }}
            >
              <option value="CPU">
                {isPt ? "CPU — Pronto (Recomendado)" : "CPU — Ready (Recommended)"}
              </option>
              <option value="AUTO">
                {isPt ? "AUTO — Melhor Disponível" : "AUTO — Best Available"}
              </option>
              {(primaryDet.type || "openvino").toLowerCase() === "onnxruntime" ||
              (primaryDet.type || "openvino").toLowerCase() === "ort" ? (
                <>
                  <option value="CUDA">CUDA — NVIDIA GPU (CUDA)</option>
                  <option value="TensorRT">TensorRT — NVIDIA GPU (FP16/INT8)</option>
                  <option value="ROCM">ROCM — AMD GPU (ROCm / MIGraphX)</option>
                </>
              ) : (
                <>
                  <option value="GPU">GPU — Intel iGPU / Arc</option>
                  <option value="NPU">NPU — Intel AI Boost</option>
                </>
              )}
            </select>
          </div>

          {modelCfg.model_type !== "ssd" &&
            modelCfg.path.includes("ssdlite_mobilenet_v2") && (
              <div
                className="mono"
                style={{
                  padding: "6px 8px",
                  background: "rgba(245, 158, 11, 0.14)",
                  border: "1px solid rgba(245, 158, 11, 0.45)",
                  borderRadius: 3,
                  fontSize: "var(--text-xs)",
                  color: "var(--signal-motion-text)",
                }}
              >
                {isPt
                  ? "AVISO: Caminho ainda aponta para ssdlite_mobilenet_v2.xml (SSD). Informe o caminho do arquivo .onnx/.xml abaixo."
                  : "NOTE: Path still points to bundled ssdlite_mobilenet_v2.xml (SSD). Provide a custom YOLO .onnx/.xml path below."}
              </div>
            )}

          <button
            type="button"
            className="nvr-btn mono sm"
            style={{ width: "100%", justifyContent: "space-between", marginTop: "auto" }}
            onClick={() => setShowAdvancedModel((v) => !v)}
          >
            <span>
              {isPt
                ? `AVANÇADO: TENSOR & CAMINHO (${modelCfg.width}×${modelCfg.height} · ${modelCfg.input_tensor.toUpperCase()} · ${modelCfg.input_pixel_format.toUpperCase()})`
                : `ADVANCED TENSOR & PATH OVERRIDES (${modelCfg.width}×${modelCfg.height} · ${modelCfg.input_tensor.toUpperCase()} · ${modelCfg.input_pixel_format.toUpperCase()})`}
            </span>
            {showAdvancedModel ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
          </button>

          {showAdvancedModel && (
            <div className="nvr-subcard">
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
                  gap: 6,
                }}
              >
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Width
                  </div>
                  <input
                    type="number"
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={modelCfg.width}
                    onChange={(e) =>
                      updateDraft((next) => {
                        next.model = { ...modelCfg, width: Number(e.target.value) };
                      })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Height
                  </div>
                  <input
                    type="number"
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={modelCfg.height}
                    onChange={(e) =>
                      updateDraft((next) => {
                        next.model = { ...modelCfg, height: Number(e.target.value) };
                      })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Tensor
                  </div>
                  <select
                    className="nvr-select mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={modelCfg.input_tensor}
                    onChange={(e) =>
                      updateDraft((next) => {
                        next.model = { ...modelCfg, input_tensor: e.target.value };
                      })
                    }
                  >
                    <option value="nhwc">NHWC</option>
                    <option value="nchw">NCHW</option>
                  </select>
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Color
                  </div>
                  <select
                    className="nvr-select mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={modelCfg.input_pixel_format}
                    onChange={(e) =>
                      updateDraft((next) => {
                        next.model = {
                          ...modelCfg,
                          input_pixel_format: e.target.value,
                        };
                      })
                    }
                  >
                    <option value="bgr">BGR</option>
                    <option value="rgb">RGB</option>
                  </select>
                </div>
              </div>

              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  Model XML / ONNX File Path
                </div>
                <input
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  value={modelCfg.path}
                  onChange={(e) =>
                    updateDraft((next) => {
                      next.model = { ...modelCfg, path: e.target.value };
                    })
                  }
                />
              </div>
            </div>
          )}
        </div>
      </div>

      {scopeDivider("camera")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "DETECÇÃO DE OBJETOS" : "OBJECT DETECTION"}</span>
            <span className="nvr-yaml-key-tag">detect</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span className={`nvr-badge ${activeDetect.enabled ? "live" : ""}`}>
              {activeDetect.enabled
                ? `${detFps} FPS · ACTIVE`
                : isPt
                ? "DESATIVADO"
                : "STANDBY (OFF)"}
            </span>
            <button
              type="button"
              className="nvr-btn sm"
              onClick={() => {
                updateDraft((next) => {
                  configLens.applySectionToAllCameras(next, "detect");
                });
                notify("Applied detection & tracking settings to all cameras");
              }}
            >
              {isPt ? "Aplicar a Todas" : "Apply to All Cameras"}
            </button>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Ativar Detecção IA de Objetos" : "Enable AI Object Detection"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Analisa regiões com movimento no substream e rastreia objetos."
                  : "Analyze substream motion regions and track moving objects."}
              </div>
            </div>
            <ToggleSwitch
              checked={activeDetect.enabled}
              label="Enable AI Object Detection"
              onChange={(v) => updateDetect({ enabled: v })}
            />
          </div>

          <div
            className={!activeDetect.enabled ? "nvr-cfg-disabled-zone" : ""}
            style={{ display: "flex", flexDirection: "column", gap: 8, flex: 1 }}
          >
            <div className="nvr-field-row" style={{ padding: "4px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt ? "Roteamento de Detector IA" : "Assigned AI Detector Instance"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Atribui esta câmera a um detector dedicado ou ao pool padrão."
                    : "Routes this camera to a dedicated detector instance or the default pool."}
                </div>
              </div>
              <div style={{ display: "flex", gap: 6, alignItems: "center", flexShrink: 0, maxWidth: "100%" }}>
                <select
                  className="nvr-select mono"
                  style={{ width: 220, maxWidth: "100%" }}
                  aria-label="Assigned AI Detector Instance"
                  value={activeDetect.detector || ""}
                  onChange={(e) =>
                    updateDetect({ detector: e.target.value ? e.target.value : null })
                  }
                >
                  <option value="">
                    {isPt ? "Pool Padrão (Auto)" : "Default Pool (Auto)"}
                  </option>
                  {detKeys.map((dk) => (
                    <option key={dk} value={dk}>
                      {dk} (
                      {(draftConfig.detectors?.[dk]?.type || "openvino").toUpperCase()}{" "}
                      · {draftConfig.detectors?.[dk]?.device || "CPU"})
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  className="nvr-btn mono"
                  title="Add another detector worker instance to the pool"
                  onClick={() => {
                    const nextIdx = detKeys.length + 1;
                    const newKey = `detector0${nextIdx}`;
                    updateDraft((next) => {
                      if (!next.detectors) next.detectors = {};
                      next.detectors[newKey] = {
                        type: "openvino",
                        device: "CPU",
                      };
                      configLens.mutateDetect(next, { detector: newKey });
                    });
                    notify(`Added detector instance '${newKey}'`);
                  }}
                >
                  + Detector
                </button>
              </div>
            </div>

            <div className="nvr-field-row" style={{ padding: "4px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? "Resolução do Substream de Detecção"
                    : "Substream Detection Resolution"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Resolução de decodificação para movimento e detecção de objetos."
                    : "Decode resolution for motion and AI object detection pipeline."}
                </div>
              </div>
              <select
                className="nvr-select mono"
                style={{ width: 310, maxWidth: "100%" }}
                aria-label="Substream Detection Resolution"
                value={isCustomResolution || !isStandardRes ? "custom" : currentResPreset}
                onChange={(e) => {
                  const v = e.target.value;
                  if (v === "custom") {
                    setIsCustomResolution(true);
                  } else {
                    setIsCustomResolution(false);
                    if (v === "1280x720") updateDetect({ width: 1280, height: 720 });
                    else if (v === "640x480") updateDetect({ width: 640, height: 480 });
                    else if (v === "640x360") updateDetect({ width: 640, height: 360 });
                    else if (v === "1920x1080") updateDetect({ width: 1920, height: 1080 });
                  }
                }}
              >
                <option value="1280x720">
                  {isPt
                    ? "720p HD (1280×720 · 16:9 Recomendado)"
                    : "720p HD (1280×720 · 16:9 Recommended)"}
                </option>
                <option value="640x480">
                  {isPt ? "480p SD (640×480 · 4:3 Baixa CPU)" : "480p SD (640×480 · 4:3 Low CPU)"}
                </option>
                <option value="640x360">
                  {isPt
                    ? "360p Compacto (640×360 · 16:9 Ultra-Leve)"
                    : "360p Compact (640×360 · 16:9 Ultra-Low CPU)"}
                </option>
                <option value="1920x1080">
                  {isPt ? "1080p Full HD (1920×1080 · Alta CPU)" : "1080p Full HD (1920×1080 · High CPU)"}
                </option>
                <option value="custom">
                  {isPt
                    ? `Resolução Customizada (${activeDetect.width}×${activeDetect.height})...`
                    : `Custom Resolution (${activeDetect.width}×${activeDetect.height})...`}
                </option>
              </select>
            </div>

            {(isCustomResolution || !isStandardRes) && (
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
                  gap: 8,
                }}
              >
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {isPt ? "Largura (px)" : "Width (px)"}
                  </div>
                  <input
                    type="number"
                    min={160}
                    max={3840}
                    step={4}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeDetect.width}
                    onChange={(e) => updateDetect({ width: Number(e.target.value) })}
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    {isPt ? "Altura (px)" : "Height (px)"}
                  </div>
                  <input
                    type="number"
                    min={120}
                    max={2160}
                    step={4}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeDetect.height}
                    onChange={(e) => updateDetect({ height: Number(e.target.value) })}
                  />
                </div>
              </div>
            )}
          </div>
        </div>
      </div>

      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "RESPOSTA DO RASTREADOR" : "TRACKING RESPONSIVENESS"}</span>
            <span className="nvr-yaml-key-tag">detect.min_initialized · max_disappeared</span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div
            className={!activeDetect.enabled ? "nvr-cfg-disabled-zone" : ""}
            style={{ display: "flex", flexDirection: "column", gap: 8, flex: 1 }}
          >
            <div className="nvr-field-desc">
              {isPt
                ? "Quanto tempo um objeto precisa persistir antes de virar evento."
                : "How long an object must persist before it becomes an event."}
            </div>
            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
                gap: 6,
              }}
            >
              <button
                type="button"
                className={`nvr-btn tile ${activeTrackPreset === "balanced" ? "active" : ""}`}
                onClick={() =>
                  updateDetect({
                    fps: 5,
                    min_initialized: 2,
                    max_disappeared: 25,
                    stationary: { interval: 50, threshold: 50, classifier: true },
                  })
                }
              >
                <span style={{ fontWeight: 700, fontSize: "var(--text-xs)" }}>
                  {isPt ? "Equilibrado" : "Balanced (Default)"}
                </span>
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                  5 FPS · 0.4s confirm
                </span>
              </button>

              <button
                type="button"
                className={`nvr-btn tile ${activeTrackPreset === "fast" ? "active" : ""}`}
                onClick={() =>
                  updateDetect({
                    fps: 8,
                    min_initialized: 1,
                    max_disappeared: 40,
                    stationary: { interval: 50, threshold: 50, classifier: true },
                  })
                }
              >
                <span style={{ fontWeight: 700, fontSize: "var(--text-xs)" }}>
                  {isPt ? "Entrada Rápida" : "Fast Entryway"}
                </span>
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                  8 FPS · Instant hit
                </span>
              </button>

              <button
                type="button"
                className={`nvr-btn tile ${activeTrackPreset === "strict" ? "active" : ""}`}
                onClick={() =>
                  updateDetect({
                    fps: 5,
                    min_initialized: 4,
                    max_disappeared: 20,
                    stationary: { interval: 40, threshold: 40, classifier: true },
                  })
                }
              >
                <span style={{ fontWeight: 700, fontSize: "var(--text-xs)" }}>
                  {isPt ? "Externo Rigoroso" : "Strict Outdoor"}
                </span>
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                  5 FPS · 4-hit filter
                </span>
              </button>
            </div>

            <button
              type="button"
              className="nvr-btn mono sm"
              style={{ width: "100%", justifyContent: "space-between", marginTop: "auto" }}
              onClick={() => setShowAdvancedByteTrack((v) => !v)}
            >
              <span>
                {isPt
                  ? `AVANÇADO: BYTETRACK (${detFps} FPS · ${detHits} HITS)`
                  : `ADVANCED BYTETRACK PARAMETERS (${detFps} FPS · ${detHits} HITS)`}
              </span>
              {showAdvancedByteTrack ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
            </button>

            {showAdvancedByteTrack && (
              <div
                className="nvr-subcard"
                style={{
                  padding: 8,
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
                  gap: 6,
                }}
              >
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    FPS
                  </div>
                  <input
                    type="number"
                    min={1}
                    max={30}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeDetect.fps}
                    onChange={(e) => updateDetect({ fps: Number(e.target.value) })}
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Min Hits
                  </div>
                  <input
                    type="number"
                    min={1}
                    max={20}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeDetect.min_initialized ?? 2}
                    onChange={(e) =>
                      updateDetect({ min_initialized: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div
                    className="mono"
                    style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                    title="Minimum consecutive hits required when camera is in Night IR mode (Spider/Insect flash filter)"
                  >
                    Night IR Hits
                  </div>
                  <input
                    type="number"
                    min={1}
                    max={25}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={
                      activeDetect.min_initialized_night ??
                      (activeDetect.min_initialized ?? 2) + 2
                    }
                    onChange={(e) =>
                      updateDetect({ min_initialized_night: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Max Gone
                  </div>
                  <input
                    type="number"
                    min={5}
                    max={200}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeDetect.max_disappeared ?? 25}
                    onChange={(e) =>
                      updateDetect({ max_disappeared: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Stat. Frames
                  </div>
                  <input
                    type="number"
                    min={10}
                    max={500}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeDetect.stationary?.threshold ?? 50}
                    onChange={(e) =>
                      updateDetect({
                        stationary: {
                          interval: activeDetect.stationary?.interval ?? 50,
                          threshold: Number(e.target.value),
                          classifier: activeDetect.stationary?.classifier ?? true,
                        },
                      })
                    }
                  />
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </>
  );
};
