import React, { useEffect, useState } from "react";
import {
  Box,
  Car,
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  Copy,
  Cpu,
  DoorOpen,
  Edit3,
  Package,
  Plus,
  Sparkles,
  Trash2,
} from "lucide-react";
import {
  coordStringToPoints,
  extractMaskPolygons,
  LiveMsePlayer,
} from "../../components/VideoPlayers";
import { useI18n } from "../../i18n";
import { useNvrStore } from "../../store/useNvrStore";
import {
  CameraDetectConfig,
  CameraMotionConfig,
  DatasetAnnotationBox,
  DatasetSampleItem,
  DatasetSummaryResponse,
  EnrolledFaceProfileItem,
  FaceRecognitionConfig,
  FalsePositiveSignatureItem,
  GenAiConfig,
  LprConfig,
  ModelConfig,
  ModelUploadPayload,
  NinjaConfigResponse,
  ObjectFilterConfig,
  StateRoiConfig,
} from "../../types/nvr";
import {
  buildGenAiProviderChangePatch,
  DEFAULT_LPR_FORMAT_REGEX,
  normalizeDiscoveredGenAiModels,
  normalizeLicensePlate,
  resolveKnownPlateLabel,
  validateLicensePlateFormat,
} from "../../utils/configYaml";
import { formatDateTime } from "../../utils/format";
import { ToggleSwitch } from "./ConsoleAndStreamsPanels";

export const NINJANVR_SECURITY_CLASSES = [
  "person",
  "face",
  "car",
  "motorcycle",
  "bicycle",
  "truck",
  "bus",
  "license_plate",
  "dog",
  "cat",
  "bird",
  "package",
  "waste_bin",
  "backpack",
  "umbrella",
] as const;

function round6(v: number): number {
  const clamped = Math.max(0, Math.min(1, Number.isFinite(v) ? v : 0));
  return Math.round(clamped * 1_000_000) / 1_000_000;
}

function toSafeCount(val: unknown): number {
  const n = Number(val ?? 0);
  if (!Number.isFinite(n) || n <= 0) return 0;
  return Math.floor(n);
}

export function xyxyToYoloNormalized(
  box: [number, number, number, number]
): { cx: number; cy: number; w: number; h: number } {
  const b0 = Number.isFinite(box?.[0]) ? box[0] : 0;
  const b1 = Number.isFinite(box?.[1]) ? box[1] : 0;
  const b2 = Number.isFinite(box?.[2]) ? box[2] : 0;
  const b3 = Number.isFinite(box?.[3]) ? box[3] : 0;
  const xmin = Math.max(0, Math.min(1, Math.min(b0, b2)));
  const xmax = Math.max(0, Math.min(1, Math.max(b0, b2)));
  const ymin = Math.max(0, Math.min(1, Math.min(b1, b3)));
  const ymax = Math.max(0, Math.min(1, Math.max(b1, b3)));
  const w = Math.max(0.001, Math.min(1, xmax - xmin));
  const h = Math.max(0.001, Math.min(1, ymax - ymin));
  const cx = (xmin + xmax) / 2;
  const cy = (ymin + ymax) / 2;
  return {
    cx: round6(cx),
    cy: round6(cy),
    w: round6(w),
    h: round6(h),
  };
}

export function yoloNormalizedToXyxy(box: {
  cx: number;
  cy: number;
  w: number;
  h: number;
}): [number, number, number, number] {
  const cx = Number.isFinite(box?.cx) ? box.cx : 0.5;
  const cy = Number.isFinite(box?.cy) ? box.cy : 0.5;
  const w = Number.isFinite(box?.w) ? Math.max(0.001, box.w) : 0.001;
  const h = Number.isFinite(box?.h) ? Math.max(0.001, box.h) : 0.001;
  const x1 = round6(cx - w / 2);
  const y1 = round6(cy - h / 2);
  const x2 = round6(cx + w / 2);
  const y2 = round6(cy + h / 2);
  return [x1, y1, x2, y2];
}

export function formatYoloAnnotationLine(ann: DatasetAnnotationBox): string {
  const rawCid = Number(ann?.class_id);
  const cid = Math.max(
    0,
    Math.min(
      NINJANVR_SECURITY_CLASSES.length - 1,
      Number.isFinite(rawCid) ? Math.trunc(rawCid) : 0
    )
  );
  const cx = round6(Number(ann?.cx));
  const cy = round6(Number(ann?.cy));
  const w = Math.max(0.001, round6(Number(ann?.w)));
  const h = Math.max(0.001, round6(Number(ann?.h)));
  return `${cid} ${cx.toFixed(6)} ${cy.toFixed(6)} ${w.toFixed(6)} ${h.toFixed(6)}`;
}

export interface DatasetReadinessMetrics {
  totalSamples: number;
  positiveCount: number;
  hardNegativeCount: number;
  positivePct: number;
  hardNegativePct: number;
  trainCount: number;
  valCount: number;
  trainPct: number;
  valPct: number;
  verifiedCount: number;
  verifiedPct: number;
  balanceScorePct: number;
  isBalanced: boolean;
}

export function computeDatasetReadinessMetrics(
  summary?: Partial<DatasetSummaryResponse> | null
): DatasetReadinessMetrics {
  const tp = toSafeCount(summary?.true_positive_count);
  const ai = toSafeCount(summary?.ai_annotated_count);
  const hn = toSafeCount(summary?.hard_negative_count);
  const positiveCount = tp + ai;
  const trainCount = toSafeCount(summary?.train_count);
  const valCount = toSafeCount(summary?.val_count);
  const verifiedCount = toSafeCount(summary?.verified_count);
  const rawTotal =
    summary?.total_samples !== undefined && summary?.total_samples !== null
      ? toSafeCount(summary.total_samples)
      : positiveCount + hn;
  const totalSamples = Math.max(
    rawTotal,
    positiveCount + hn,
    trainCount + valCount,
    verifiedCount
  );
  const positivePct =
    totalSamples > 0
      ? Math.max(0, Math.min(100, Math.round((positiveCount / totalSamples) * 100)))
      : 0;
  const hardNegativePct =
    totalSamples > 0 ? Math.max(0, 100 - positivePct) : 0;
  const trainPct =
    totalSamples > 0
      ? Math.max(0, Math.min(100, Math.round((trainCount / totalSamples) * 100)))
      : 0;
  const valPct = totalSamples > 0 ? Math.max(0, 100 - trainPct) : 0;
  const verifiedPct =
    totalSamples > 0
      ? Math.max(0, Math.min(100, Math.round((verifiedCount / totalSamples) * 100)))
      : 0;
  const deviationFrom80 = Math.abs(positivePct - 80);
  const balanceScorePct =
    totalSamples > 0
      ? Math.max(0, Math.min(100, Math.round(100 - deviationFrom80 * 1.25)))
      : 0;
  const isBalanced =
    totalSamples > 0 &&
    positivePct >= 70 &&
    positivePct <= 90 &&
    hardNegativePct >= 10 &&
    hardNegativePct <= 30;

  return {
    totalSamples,
    positiveCount,
    hardNegativeCount: hn,
    positivePct,
    hardNegativePct,
    trainCount,
    valCount,
    trainPct,
    valPct,
    verifiedCount,
    verifiedPct,
    balanceScorePct,
    isBalanced,
  };
}

export function buildModelUploadPayload(params: {
  filename: string;
  modelBase64: string;
  labelmap?: string;
  modelType?: string;
  width?: number;
  height?: number;
  inputTensor?: string;
  inputPixelFormat?: string;
}): ModelUploadPayload {
  const rawFile = (params.filename || "").trim();
  const baseSegment =
    rawFile
      .split(/[/\\]/)
      .pop()
      ?.replace(/\.\.+/g, "")
      .replace(/[^a-zA-Z0-9._-]/g, "_")
      .replace(/^[._-]+|[._-]+$/g, "") || "";
  const hasValidExt = /\.(?:onnx|xml)$/i.test(baseSegment);
  const cleanName =
    baseSegment && hasValidExt
      ? baseSegment
      : baseSegment
      ? `${baseSegment}.onnx`
      : "custom_yolo11s.onnx";
  const rawB64 = (params.modelBase64 || "").trim();
  const strippedB64 = rawB64.replace(/^data:[^,]*;base64,/i, "").trim();
  const rawW = Number(params.width);
  const rawH = Number(params.height);
  const safeW =
    Number.isFinite(rawW) && rawW >= 64 && rawW <= 4096
      ? Math.round(rawW)
      : 640;
  const safeH =
    Number.isFinite(rawH) && rawH >= 64 && rawH <= 4096
      ? Math.round(rawH)
      : 640;
  return {
    filename: cleanName,
    model_base64: strippedB64,
    labelmap:
      params.labelmap?.trim() || NINJANVR_SECURITY_CLASSES.join("\n"),
    model_type: params.modelType || "yolo-generic",
    width: safeW,
    height: safeH,
    input_tensor: params.inputTensor || "nchw",
    input_pixel_format: params.inputPixelFormat || "rgb",
    activate: true,
  };
}

export const NINJANVR_SECURITY_15CL_LABELMAP_PATH =
  "/openvino-model/ninjanvr_security_15cl.txt";

export const COLAB_MODEL_FACTORY_URL =
  "https://colab.research.google.com/github/JustShinobi/NinjaNVR/blob/dev/notebooks/NinjaNVR_Model_Factory.ipynb";

export function applySecurity15ClassPreset(
  currentModel?: Partial<ModelConfig> | null
): ModelConfig {
  const prevWidth = Number(currentModel?.width || 640);
  const prevHeight = Number(currentModel?.height || 640);
  const resolvedDim = prevWidth === 320 || prevWidth === 640 ? prevWidth : 640;
  const resolvedH = prevHeight === 320 || prevHeight === 640 ? prevHeight : resolvedDim;
  const prevPath = (currentModel?.path || "").trim();
  const nextPath =
    !prevPath || prevPath.includes("ssdlite_mobilenet_v2")
      ? "/config/models/ninjanvr_security_15cl.onnx"
      : prevPath;
  return {
    path: nextPath,
    labelmap_path: NINJANVR_SECURITY_15CL_LABELMAP_PATH,
    width: resolvedDim,
    height: resolvedH,
    input_tensor: "nchw",
    input_pixel_format: "rgb",
    model_type: "yolo-generic",
  };
}

export function buildLocalFinetuneCliCommand(params?: {
  nvrUrl?: string;
  datasetPath?: string;
  modelArch?: string;
  imgsz?: number;
  epochs?: number;
  freeze?: number;
  nvrToken?: string;
}): string {
  const rawDataset = (params?.datasetPath || "ninjanvr-dataset.zip")
    .replace(/[;&|`$><\r\n\0'"\\\s]/g, "")
    .trim();
  const dataset = rawDataset || "ninjanvr-dataset.zip";
  const rawArch = (params?.modelArch || "yolo11s.pt")
    .replace(/[;&|`$><\r\n\0'"\\\s]/g, "")
    .trim();
  const modelArch = rawArch || "yolo11s.pt";
  const imgsz = params?.imgsz === 320 ? 320 : 640;
  const rawEpochs = Number(params?.epochs ?? 30);
  const epochs =
    Number.isFinite(rawEpochs) && rawEpochs >= 1
      ? Math.min(1000, Math.trunc(rawEpochs))
      : 30;
  const rawFreeze = Number(params?.freeze ?? 10);
  const freeze =
    Number.isFinite(rawFreeze) && rawFreeze >= 0
      ? Math.min(100, Math.trunc(rawFreeze))
      : 10;
  const cleanedUrl = (params?.nvrUrl || "http://localhost:5000")
    .replace(/[;&|`$><\r\n\0'"\\\s]/g, "")
    .trim()
    .replace(/\/+$/, "");
  const rawUrl = /^https?:\/\/[a-zA-Z0-9._:-]+$/i.test(cleanedUrl)
    ? cleanedUrl
    : "http://localhost:5000";
  const candidateToken = (params?.nvrToken || "$NINJANVR_API_TOKEN").trim();
  const token = /^(?:\$[A-Za-z_][A-Za-z0-9_]*|[a-zA-Z0-9._-]+)$/.test(
    candidateToken
  )
    ? candidateToken
    : "$NINJANVR_API_TOKEN";
  return `python3 scripts/ai/train_ninjanvr_model.py --mode finetune --dataset ${dataset} --model ${modelArch} --imgsz ${imgsz} --epochs ${epochs} --freeze ${freeze} --nvr-url ${rawUrl} --nvr-token ${token}`;
}

export function formatDhash64Hex(dhash?: number | null): string {
  if (dhash === null || dhash === undefined || !Number.isFinite(Number(dhash))) {
    return "—";
  }
  const big = BigInt.asUintN(64, BigInt(Math.trunc(Number(dhash))));
  return `0x${big.toString(16).padStart(16, "0")}`;
}

export function formatSignatureBbox(
  normBbox?: [number, number, number, number] | null
): string {
  if (!normBbox || normBbox.length < 4) return "[0.00, 0.00, 0.00, 0.00]";
  return `[${normBbox
    .slice(0, 4)
    .map((v) => {
      const n = Number(v);
      return (Number.isFinite(n) ? n : 0).toFixed(2);
    })
    .join(", ")}]`;
}

export function isSafeGenAiCustomBaseUrl(rawUrl: string): boolean {
  const trimmed = (rawUrl || "").trim();
  if (
    !trimmed ||
    /[\u0000-\u001f\u007f\\]/.test(trimmed) ||
    trimmed.includes("..")
  ) {
    return false;
  }
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return false;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return false;
  }
  if (parsed.username || parsed.password || trimmed.includes("@")) {
    return false;
  }
  if (parsed.search || parsed.hash) {
    return false;
  }
  const portNum = parsed.port ? Number(parsed.port) : null;
  if (
    portNum !== null &&
    (portNum === 0 ||
      portNum === 5000 ||
      portNum === 8971 ||
      portNum === 1984 ||
      portNum === 8554)
  ) {
    return false;
  }
  const hostClean = parsed.hostname
    .replace(/^\[|\]$/g, "")
    .replace(/\.+$/, "")
    .toLowerCase();
  if (
    !hostClean ||
    hostClean === "0.0.0.0" ||
    hostClean === "::" ||
    hostClean === "0:0:0:0:0:0:0:0" ||
    hostClean === "metadata.google.internal" ||
    /^169\.254(?:\.\d{1,3}){2}$/.test(hostClean) ||
    /^fe[89ab][0-9a-f]:/i.test(hostClean) ||
    /^(?:22[4-9]|23\d)(?:\.\d{1,3}){3}$/.test(hostClean) ||
    /^ff[0-9a-f]{2}:/i.test(hostClean)
  ) {
    return false;
  }
  return true;
}

export function parseRoiCropInput(
  rawCrop: string,
  draftPoints?: Array<[number, number]>
): [number, number, number, number] {
  if (draftPoints && draftPoints.length >= 2) {
    const xs = draftPoints.map((p) => p[0]);
    const ys = draftPoints.map((p) => p[1]);
    const x1 = Number(Math.max(0, Math.min(1, Math.min(...xs))).toFixed(4));
    const y1 = Number(Math.max(0, Math.min(1, Math.min(...ys))).toFixed(4));
    const x2 = Number(
      Math.max(Math.min(0.99, x1 + 0.01), Math.min(1, Math.max(...xs))).toFixed(4)
    );
    const y2 = Number(
      Math.max(Math.min(0.99, y1 + 0.01), Math.min(1, Math.max(...ys))).toFixed(4)
    );
    return [x1, y1, x2, y2];
  }
  const nums = rawCrop
    .split(",")
    .map((s) => Number(s.trim()))
    .filter((n) => Number.isFinite(n));
  if (nums.length >= 4) {
    const x1 = Number(Math.max(0, Math.min(1, Math.min(nums[0], nums[2]))).toFixed(4));
    const y1 = Number(Math.max(0, Math.min(1, Math.min(nums[1], nums[3]))).toFixed(4));
    const x2 = Number(
      Math.max(Math.min(0.99, x1 + 0.01), Math.min(1, Math.max(nums[0], nums[2]))).toFixed(4)
    );
    const y2 = Number(
      Math.max(Math.min(0.99, y1 + 0.01), Math.min(1, Math.max(nums[1], nums[3]))).toFixed(4)
    );
    return [x1, y1, x2, y2];
  }
  return [0.25, 0.25, 0.75, 0.75];
}

export function parseRoiClassesInput(rawClasses: string): string[] {
  const parsed = rawClasses
    .split(",")
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  return parsed.length >= 2 ? Array.from(new Set(parsed)) : ["closed", "open"];
}

export function slugifyRoiLocationName(raw: string): string {
  return (raw || "")
    .normalize("NFD")
    .replace(/[\u0300-\u036f]/g, "")
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_]+/g, "_")
    .replace(/^_+|_+$/g, "");
}

export type StateRoiPresetId =
  | "door_gate"
  | "parking_spot"
  | "package_area"
  | "fixed_object"
  | "custom";

export interface StateRoiScenarioPreset {
  id: StateRoiPresetId;
  defaultName: string;
  classes: string;
  anomalyState: string;
  anomalyTimeoutSec: number;
  intervalSec: number;
  hysteresis: number;
  titleEn: string;
  titlePt: string;
  descEn: string;
  descPt: string;
}

export const STATE_ROI_SCENARIO_PRESETS: StateRoiScenarioPreset[] = [
  {
    id: "door_gate",
    defaultName: "gate",
    classes: "closed, open",
    anomalyState: "open",
    anomalyTimeoutSec: 300,
    intervalSec: 5,
    hysteresis: 3,
    titleEn: "Door / Gate",
    titlePt: "Porta / Portão",
    descEn: "closed ↔ open · Alert open 5m",
    descPt: "closed ↔ open · Alerta aberto 5m",
  },
  {
    id: "parking_spot",
    defaultName: "parking_spot",
    classes: "empty, occupied",
    anomalyState: "",
    anomalyTimeoutSec: 300,
    intervalSec: 5,
    hysteresis: 3,
    titleEn: "Parking / Garage Spot",
    titlePt: "Vaga / Garagem",
    descEn: "empty ↔ occupied · Status badge",
    descPt: "empty ↔ occupied · Crachá no vídeo",
  },
  {
    id: "package_area",
    defaultName: "delivery_area",
    classes: "empty, package_present",
    anomalyState: "package_present",
    anomalyTimeoutSec: 60,
    intervalSec: 5,
    hysteresis: 3,
    titleEn: "Package / Delivery Area",
    titlePt: "Área de Entregas / Pacotes",
    descEn: "empty ↔ package_present · Alert",
    descPt: "empty ↔ package_present · Alerta",
  },
  {
    id: "fixed_object",
    defaultName: "equipment",
    classes: "present, missing",
    anomalyState: "missing",
    anomalyTimeoutSec: 60,
    intervalSec: 5,
    hysteresis: 3,
    titleEn: "Fixed Object / Equipment",
    titlePt: "Objeto Fixo / Equipamento",
    descEn: "present ↔ missing · Alert 1m",
    descPt: "present ↔ missing · Alerta 1m",
  },
  {
    id: "custom",
    defaultName: "custom_roi",
    classes: "closed, open",
    anomalyState: "open",
    anomalyTimeoutSec: 300,
    intervalSec: 5,
    hysteresis: 3,
    titleEn: "Custom States",
    titlePt: "Estados Customizados",
    descEn: "Define your own state classes",
    descPt: "Defina seus próprios estados",
  },
];

export const ROI_ALERT_DURATION_PILLS: Array<{ sec: number; label: string }> = [
  { sec: 30, label: "30s" },
  { sec: 60, label: "1m" },
  { sec: 300, label: "5m" },
  { sec: 900, label: "15m" },
  { sec: 1800, label: "30m" },
];

export function buildStateRoiConfig(params: {
  cropInput: string;
  classesInput: string;
  intervalSec: number;
  hysteresis: number;
  anomalyState?: string | null;
  anomalyTimeoutSec: number;
  draftPoints?: Array<[number, number]>;
}): StateRoiConfig {
  const crop = parseRoiCropInput(params.cropInput, params.draftPoints);
  const classes = parseRoiClassesInput(params.classesInput);
  const cleanAnomaly = params.anomalyState?.trim().toLowerCase() || null;
  return {
    crop,
    classes,
    interval_sec: Math.max(1, Math.round(params.intervalSec || 5)),
    hysteresis: Math.max(1, Math.round(params.hysteresis || 3)),
    ...(cleanAnomaly ? { anomaly_state: cleanAnomaly } : {}),
    anomaly_timeout_sec: Math.max(1, Math.round(params.anomalyTimeoutSec || 300)),
  };
}

export const STANDARD_LABELS = [
  "person",
  "car",
  "truck",
  "bicycle",
  "motorcycle",
  "bus",
  "dog",
  "cat",
  "bird",
  "package",
  "waste_bin",
  "license_plate",
];

export const DEFAULT_ATTRIBUTE_LABELS = [
  "face",
  "license_plate",
  "correios",
  "mercado_livre",
  "amazon",
  "fedex",
  "dhl",
  "ups",
  "usps",
];

export function toggleAttributeLabelList(
  current: string[] | undefined,
  label: string
): string[] {
  const base = current ?? DEFAULT_ATTRIBUTE_LABELS;
  const clean = label.trim().toLowerCase();
  if (!clean) return [...base];
  return base.includes(clean)
    ? base.filter((x) => x !== clean)
    : [...base, clean];
}

export function upsertKnownPlateEntry(
  current: Record<string, string> | undefined,
  plate: string,
  label: string
): Record<string, string> {
  const norm = normalizeLicensePlate(plate);
  const cleanLabel = label.trim();
  const next = { ...(current || {}) };
  if (!norm || !cleanLabel) return next;
  next[norm] = cleanLabel;
  return next;
}

export function removeKnownPlateEntry(
  current: Record<string, string> | undefined,
  plate: string
): Record<string, string> {
  const norm = normalizeLicensePlate(plate);
  const next: Record<string, string> = {};
  for (const [k, v] of Object.entries(current || {})) {
    if (normalizeLicensePlate(k) !== norm) {
      next[k] = v;
    }
  }
  return next;
}

export function normalizeFaceProfileName(raw: string): string {
  const trimmed = raw.trim();
  if (!trimmed || trimmed.includes("..") || /[\\/]/.test(trimmed)) {
    return "";
  }
  const cleaned = trimmed.replace(/[^a-zA-Z0-9_\- ]/g, "").trim();
  if (!cleaned || cleaned.toLowerCase() === "unknown") {
    return "";
  }
  return cleaned;
}

export function toggleSubLabelRuleList(
  current: string[] | undefined,
  subLabel: string
): string[] {
  const base = current ? [...current] : [];
  const clean = subLabel.trim();
  if (!clean) return base;
  const existingIdx = base.findIndex(
    (item) => item.toLowerCase() === clean.toLowerCase()
  );
  if (existingIdx >= 0) {
    return base.filter((_, idx) => idx !== existingIdx);
  }
  return [...base, clean];
}

export function parseFacesApiResponse(payload: unknown): EnrolledFaceProfileItem[] {
  if (!payload || typeof payload !== "object") return [];
  const obj = payload as Record<string, unknown>;
  if (Array.isArray(obj.profiles)) {
    return obj.profiles
      .filter(
        (item): item is Record<string, unknown> =>
          item !== null && typeof item === "object" && typeof (item as Record<string, unknown>).name === "string"
      )
      .map((item) => ({
        name: String(item.name),
        sample_count:
          typeof item.sample_count === "number"
            ? item.sample_count
            : Array.isArray(item.sample_files)
            ? item.sample_files.length
            : 1,
        sample_files: Array.isArray(item.sample_files)
          ? item.sample_files.map(String)
          : [],
        updated_at:
          typeof item.updated_at === "number" ? item.updated_at : undefined,
      }));
  }
  const facesMap =
    obj.faces && typeof obj.faces === "object" && !Array.isArray(obj.faces)
      ? (obj.faces as Record<string, unknown>)
      : obj;
  const out: EnrolledFaceProfileItem[] = [];
  for (const [key, val] of Object.entries(facesMap)) {
    if (key === "profiles" || key === "faces") continue;
    if (Array.isArray(val)) {
      out.push({
        name: key,
        sample_count: val.length,
        sample_files: val.map(String),
      });
    }
  }
  return out;
}

export const VisionZonesMotionSection: React.FC<{
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  scope: "global" | string;
  inspectorCam: string;
  draftPoints: Array<[number, number]>;
  setDraftPoints: React.Dispatch<React.SetStateAction<Array<[number, number]>>>;
  notify: (msg: string) => void;
}> = ({
  draftConfig,
  updateDraft,
  inspectorCam,
  draftPoints,
  setDraftPoints,
  notify,
}) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const [polyType, setPolyType] = useState<
    | "zone"
    | "tripwire"
    | "state_roi"
    | "motion_mask"
    | "object_mask"
    | "privacy_mask"
    | "measure_box"
  >("zone");
  const [zoneNameInput, setZoneNameInput] = useState("entrada");
  const [zoneInertia, setZoneInertia] = useState(3);
  const [zoneLoitering, setZoneLoitering] = useState(0);
  const [zonePresetInput, setZonePresetInput] = useState("");
  const [zoneObjects, setZoneObjects] = useState<string[]>(["person", "car"]);
  const [lineDirection, setLineDirection] = useState<"any" | "a_to_b" | "b_to_a">("any");
  const [roiCropInput, setRoiCropInput] = useState("0.250,0.250,0.750,0.750");
  const [roiClassesInput, setRoiClassesInput] = useState("closed, open");
  const [roiIntervalSec, setRoiIntervalSec] = useState(5);
  const [roiHysteresis, setRoiHysteresis] = useState(3);
  const [roiAnomalyState, setRoiAnomalyState] = useState("open");
  const [roiAnomalyTimeoutSec, setRoiAnomalyTimeoutSec] = useState(300);
  const [roiScenario, setRoiScenario] = useState<StateRoiPresetId>("door_gate");

  const applyZoneRoiPreset = (preset: StateRoiScenarioPreset) => {
    setRoiScenario(preset.id);
    if (preset.id !== "custom") {
      setZoneNameInput(preset.defaultName);
      setRoiClassesInput(preset.classes);
      setRoiAnomalyState(preset.anomalyState);
      setRoiAnomalyTimeoutSec(preset.anomalyTimeoutSec);
      setRoiIntervalSec(preset.intervalSec);
      setRoiHysteresis(preset.hysteresis);
    }
  };

  const camCfg = draftConfig.cameras?.[inspectorCam];
  const detW = camCfg?.detect?.width || 1280;
  const detH = camCfg?.detect?.height || 720;

  const motionCfg: CameraMotionConfig = camCfg?.motion || {
    enabled: true,
    threshold: 30,
    lightning_threshold: 0.8,
    improve_contrast: true,
    contour_area: 10,
    delta_alpha: 0.2,
    frame_alpha: 0.01,
    frame_height: 100,
    mask: {},
  };

  const updateMotionField = <K extends keyof CameraMotionConfig>(
    key: K,
    val: CameraMotionConfig[K]
  ) => {
    updateDraft((next) => {
      if (next.cameras[inspectorCam]) {
        next.cameras[inspectorCam].motion[key] = val;
      }
    });
  };

  const applyMotionPreset = (
    threshold: number,
    contourArea: number,
    lightning: number,
    label: string
  ) => {
    updateDraft((next) => {
      const target = next.cameras[inspectorCam]?.motion;
      if (!target) return;
      target.threshold = threshold;
      target.contour_area = contourArea;
      target.lightning_threshold = lightning;
      target.improve_contrast = true;
    });
    notify(`Applied '${label}' motion sensitivity preset`);
  };

  const currentCoordStr =
    draftPoints.length >= 3
      ? draftPoints.map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`).join(",")
      : "";

  const handleCommitPolygon = () => {
    if (polyType === "state_roi") {
      const rSlug = slugifyRoiLocationName(zoneNameInput);
      if (!rSlug || rSlug.length < 2) {
        notify("Enter an ROI ID with at least 2 characters");
        return;
      }
      const roiCfg = buildStateRoiConfig({
        cropInput: roiCropInput,
        classesInput: roiClassesInput,
        intervalSec: roiIntervalSec,
        hysteresis: roiHysteresis,
        anomalyState: roiAnomalyState,
        anomalyTimeoutSec: roiAnomalyTimeoutSec,
        draftPoints: draftPoints.length >= 2 ? draftPoints : undefined,
      });
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        if (!cam.state_rois) cam.state_rois = {};
        cam.state_rois[rSlug] = roiCfg;
      });
      setRoiCropInput(roiCfg.crop.map((v) => v.toFixed(3)).join(","));
      setDraftPoints([]);
      notify(
        `Staged state ROI '${rSlug}' (${roiCfg.classes.join("/")}) on ${inspectorCam}`
      );
      return;
    }

    if (polyType === "tripwire") {
      if (draftPoints.length < 2) {
        notify("Plot at least 2 points on the camera preview for a directional tripwire");
        return;
      }
      const lSlug = zoneNameInput
        .trim()
        .toLowerCase()
        .replace(/[^a-z0-9_]/g, "_");
      if (!lSlug || lSlug.length < 2) {
        notify("Enter a tripwire ID with at least 2 characters");
        return;
      }
      const [p1, p2] = draftPoints;
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        if (!cam.lines) cam.lines = {};
        cam.lines[lSlug] = {
          points: [
            Number(p1[0].toFixed(4)),
            Number(p1[1].toFixed(4)),
            Number(p2[0].toFixed(4)),
            Number(p2[1].toFixed(4)),
          ],
          direction: lineDirection,
          objects: zoneObjects,
        };
      });
      setDraftPoints([]);
      notify(`Staged tripwire line '${lSlug}' (${lineDirection}) on ${inspectorCam}`);
      return;
    }

    if (draftPoints.length < 3) {
      notify(
        isPt
          ? "Marque pelo menos 3 pontos no vídeo antes de adicionar o polígono"
          : "Plot at least 3 vertices on the camera preview before adding a polygon"
      );
      return;
    }
    const coordStr = draftPoints
      .map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`)
      .join(",");

    if (polyType === "zone") {
      const zSlug = zoneNameInput
        .trim()
        .toLowerCase()
        .replace(/[^a-z0-9_]/g, "_");
      if (!zSlug || zSlug.length < 2) {
        notify("Enter a zone name with at least 2 characters");
        return;
      }
      const cleanPreset = zonePresetInput.trim();
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        if (!cam.zones) cam.zones = {};
        cam.zones[zSlug] = {
          coordinates: coordStr,
          objects: zoneObjects,
          inertia: zoneInertia,
          loitering_time: zoneLoitering,
          ...(cleanPreset ? { preset: cleanPreset } : {}),
        };
      });
      setDraftPoints([]);
      notify(
        cleanPreset
          ? `Staged zone '${zSlug}' bound to PTZ preset '${cleanPreset}' on ${inspectorCam}`
          : `Staged zone '${zSlug}' on ${inspectorCam}`
      );
    } else if (polyType === "motion_mask") {
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        const existing = extractMaskPolygons(cam.motion.mask);
        cam.motion.mask = [...existing, coordStr];
      });
      setDraftPoints([]);
      notify(`Staged motion mask polygon on ${inspectorCam}`);
    } else if (polyType === "privacy_mask") {
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        const existing = extractMaskPolygons(cam.privacy_mask);
        cam.privacy_mask = [...existing, coordStr];
      });
      setDraftPoints([]);
      notify(`Staged permanent privacy mask polygon on ${inspectorCam}`);
    } else {
      updateDraft((next) => {
        const cam = next.cameras[inspectorCam];
        if (!cam) return;
        if (!cam.objects) cam.objects = { track: ["person"], filters: {}, mask: [] };
        const existing = extractMaskPolygons(cam.objects.mask);
        cam.objects.mask = [...existing, coordStr];
      });
      setDraftPoints([]);
      notify(`Staged object mask polygon on ${inspectorCam}`);
    }
  };

  const motionMasks = extractMaskPolygons(camCfg?.motion?.mask);
  const objectMasks = extractMaskPolygons(camCfg?.objects?.mask);
  const privacyMasks = extractMaskPolygons(camCfg?.privacy_mask);
  const zonesMap = camCfg?.zones || {};

  const isIndoorMotion =
    (motionCfg.threshold ?? 30) === 22 && (motionCfg.contour_area ?? 10) === 8;
  const isBalancedMotion =
    (motionCfg.threshold ?? 30) === 30 && (motionCfg.contour_area ?? 10) === 10;
  const isOutdoorMotion =
    (motionCfg.threshold ?? 30) === 45 && (motionCfg.contour_area ?? 10) === 25;

  return (
    <div
      className="nvr-workbench"
      style={{
        flex: 1,
        minHeight: 0,
        gridTemplateColumns: "390px minmax(0, 1fr)",
      }}
    >
      {/* Left Interactive Polygon Builder & Vector Inventory */}
      <div className="nvr-panel">
        <div className="nvr-panel-header">
          <span>
            {inspectorCam.toUpperCase()} //{" "}
            {isPt ? "CONSTRUTOR DE ZONAS & MÁSCARAS" : "POLYGON VECTOR STUDIO"}
          </span>
          <span className="mono">{draftPoints.length} PTS</span>
        </div>
        <div
          className="nvr-scroll-y"
          style={{ padding: 10, display: "flex", flexDirection: "column", gap: 10 }}
        >
          {/* Polygon Plotter Controls */}
          <div
            className="nvr-subcard"
            style={{
              borderColor: "rgba(245, 158, 11, 0.45)",
            }}
          >
            <div
              className="nvr-subcard-header mono"
              style={{
                color: "var(--signal-motion-text)",
              }}
            >
              <span>
                {isPt ? "1. ESCOLHA O TIPO DE POLÍGONO" : "1. SELECT POLYGON TYPE"}
              </span>
              <span>
                {isPt ? "CLIQUE NO VÍDEO" : "CLICK VIDEO TO PLOT"}
              </span>
            </div>

            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fill, minmax(105px, 1fr))",
                gap: 4,
              }}
            >
              {(
                [
                  ["zone", isPt ? "Zona (Ciano)" : "Zone (Cyan)"],
                  ["tripwire", isPt ? "Tripwire (Verde)" : "Tripwire (Green)"],
                  ["state_roi", isPt ? "ROI Estado (Âmbar)" : "State ROI (Amber)"],
                  ["motion_mask", isPt ? "Másc. Mov (Verm)" : "Motion Mask (Red)"],
                  ["object_mask", isPt ? "Másc. Obj (Viol)" : "Object Mask (Violet)"],
                  ["privacy_mask", isPt ? "Másc. Privacidade" : "Privacy Mask"],
                  ["measure_box", isPt ? "Medir Caixa" : "Measure Box"],
                ] as const
              ).map(([id, label]) => (
                <button
                  key={id}
                  type="button"
                  className={`nvr-btn ${polyType === id ? "active" : ""}`}
                  style={{
                    height: 24,
                    padding: "0 6px",
                    fontSize: "var(--text-xs)",
                    minWidth: 0,
                  }}
                  onClick={() => setPolyType(id)}
                >
                  {label}
                </button>
              ))}
            </div>

            {polyType === "state_roi" && (
              <div
                className="nvr-subcard"
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: 8,
                  padding: "8px 10px",
                  overflow: "hidden",
                }}
              >
                <div
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    fontWeight: 700,
                    color: "var(--signal-motion-text)",
                  }}
                >
                  {isPt
                    ? "1. O QUE MONITORAR NESTA ÁREA?"
                    : "1. WHAT TO MONITOR IN THIS AREA?"}
                </div>
                <div
                  style={{
                    display: "grid",
                    gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
                    gap: 4,
                  }}
                >
                  {STATE_ROI_SCENARIO_PRESETS.map((preset) => {
                    const active = roiScenario === preset.id;
                    const IconComp =
                      preset.id === "door_gate"
                        ? DoorOpen
                        : preset.id === "parking_spot"
                        ? Car
                        : preset.id === "package_area"
                        ? Package
                        : preset.id === "fixed_object"
                        ? Box
                        : Edit3;
                    return (
                      <button
                        key={preset.id}
                        type="button"
                        className={`nvr-btn ${active ? "active" : ""}`}
                        title={isPt ? preset.descPt : preset.descEn}
                        style={{
                          height: 26,
                          padding: "0 8px",
                          justifyContent: "flex-start",
                          gap: 5,
                          minWidth: 0,
                          width: "100%",
                          overflow: "hidden",
                        }}
                        onClick={() => applyZoneRoiPreset(preset)}
                      >
                        <IconComp size={12} style={{ flexShrink: 0 }} />
                        <span
                          style={{
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                            fontSize: "var(--text-xs)",
                          }}
                        >
                          {isPt ? preset.titlePt : preset.titleEn}
                        </span>
                      </button>
                    );
                  })}
                </div>
                <div
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    color: "var(--text-secondary)",
                    background: "var(--bg-input)",
                    border: "1px solid var(--border-divider)",
                    borderRadius: 2,
                    padding: "4px 8px",
                    whiteSpace: "normal",
                    wordBreak: "break-word",
                  }}
                >
                  {isPt ? "Estados monitorados: " : "Monitored states: "}
                  <strong style={{ color: "var(--text-primary)" }}>
                    {parseRoiClassesInput(roiClassesInput).join(" ↔ ")}
                  </strong>
                </div>

                <div>
                  <div
                    className="mono"
                    style={{
                      display: "flex",
                      justifyContent: "space-between",
                      alignItems: "center",
                      gap: 6,
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                      marginBottom: 3,
                    }}
                  >
                    <span>
                      {isPt ? "2. NOME DO LOCAL" : "2. LOCATION NAME"}
                    </span>
                    <span style={{ color: "var(--text-muted)" }}>
                      ID: <strong>{slugifyRoiLocationName(zoneNameInput) || "gate"}</strong>
                    </span>
                  </div>
                  <input
                    className="nvr-input mono"
                    style={{ width: "100%", height: 26, fontSize: "var(--text-xs)" }}
                    placeholder={
                      isPt
                        ? "Ex: Porta do Escritório, Portão da Garagem"
                        : "e.g. Office Door, Garage Gate"
                    }
                    aria-label="State ROI Name"
                    value={zoneNameInput}
                    onChange={(e) => setZoneNameInput(e.target.value)}
                  />
                </div>

                {roiScenario === "custom" && (
                  <div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        marginBottom: 2,
                      }}
                    >
                      {isPt ? "Estados Customizados (separados por vírgula)" : "Custom States (comma-separated)"}
                    </div>
                    <input
                      className="nvr-input mono"
                      style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                      placeholder="closed, open"
                      value={roiClassesInput}
                      onChange={(e) => setRoiClassesInput(e.target.value)}
                    />
                  </div>
                )}

                <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                    }}
                  >
                    {isPt ? "3. QUANDO DISPARAR ALERTA?" : "3. WHEN TO TRIGGER ALERT?"}
                  </div>
                  <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
                    <button
                      type="button"
                      className={`nvr-btn mono ${!roiAnomalyState.trim() ? "active" : ""}`}
                      style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
                      onClick={() => setRoiAnomalyState("")}
                    >
                      {isPt
                        ? "Apenas Status (Sem Alerta)"
                        : "Status Only (No Alert)"}
                    </button>
                    {parseRoiClassesInput(roiClassesInput).map((st) => {
                      const isSel =
                        roiAnomalyState.trim().toLowerCase() === st.toLowerCase();
                      return (
                        <button
                          key={st}
                          type="button"
                          className={`nvr-btn mono ${isSel ? "active" : ""}`}
                          style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
                          onClick={() => setRoiAnomalyState(st)}
                        >
                          {isPt ? `Se ficar "${st}"` : `When "${st}"`}
                        </button>
                      );
                    })}
                  </div>
                  {roiAnomalyState.trim() && (
                    <div
                      style={{
                        display: "flex",
                        alignItems: "center",
                        gap: 4,
                        flexWrap: "wrap",
                        marginTop: 2,
                      }}
                    >
                      <span
                        className="mono"
                        style={{
                          fontSize: "var(--text-xs)",
                          color: "var(--text-muted)",
                        }}
                      >
                        {isPt ? "Tempo:" : "Duration:"}
                      </span>
                      {ROI_ALERT_DURATION_PILLS.map((pill) => (
                        <button
                          key={pill.sec}
                          type="button"
                          className={`nvr-btn mono ${
                            roiAnomalyTimeoutSec === pill.sec ? "active" : ""
                          }`}
                          style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                          onClick={() => setRoiAnomalyTimeoutSec(pill.sec)}
                        >
                          {pill.label}
                        </button>
                      ))}
                    </div>
                  )}
                </div>

                <div
                  className="mono"
                  style={{
                    padding: "5px 8px",
                    borderRadius: 2,
                    fontSize: "var(--text-xs)",
                    whiteSpace: "normal",
                    wordBreak: "break-word",
                    background:
                      draftPoints.length >= 2
                        ? "rgba(16, 185, 129, 0.14)"
                        : "var(--bg-input)",
                    border:
                      draftPoints.length >= 2
                        ? "1px solid rgba(16, 185, 129, 0.45)"
                        : "1px solid var(--border-divider)",
                    color:
                      draftPoints.length >= 2
                        ? "var(--signal-live-text)"
                        : "var(--signal-motion-text)",
                  }}
                >
                  {draftPoints.length >= 2
                    ? isPt
                      ? "✓ Área demarcada no vídeo (Pronto para salvar)"
                      : "✓ Area marked on video (Ready to save)"
                    : isPt
                    ? "Clique 2 ou 4 cantos no vídeo ao lado para demarcar a área"
                    : "Click 2 or 4 corners on the video to mark the area"}
                </div>

                <details
                  style={{
                    background: "var(--bg-input)",
                    border: "1px solid var(--border-divider)",
                    borderRadius: 2,
                    padding: "4px 6px",
                  }}
                >
                  <summary
                    className="mono"
                    style={{
                      cursor: "pointer",
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                    }}
                  >
                    {isPt
                      ? "Ajustes Avançados (Coordenadas & Histerese)"
                      : "Advanced Tuning (Coordinates & Hysteresis)"}
                  </summary>
                  <div
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      gap: 6,
                      marginTop: 6,
                    }}
                  >
                    <div>
                      <div
                        className="mono"
                        style={{
                          fontSize: "var(--text-xs)",
                          color: "var(--text-secondary)",
                          marginBottom: 2,
                        }}
                      >
                        {isPt ? "Crop Normalizado [x1,y1,x2,y2]" : "Normalized Crop [x1,y1,x2,y2]"}
                      </div>
                      <input
                        className="nvr-input mono"
                        style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                        placeholder="0.250,0.250,0.750,0.750"
                        aria-label="State ROI Crop Coordinates"
                        value={
                          draftPoints.length >= 2
                            ? parseRoiCropInput(roiCropInput, draftPoints)
                                .map((v) => v.toFixed(3))
                                .join(",")
                            : roiCropInput
                        }
                        onChange={(e) => setRoiCropInput(e.target.value)}
                      />
                    </div>
                    <div
                      style={{
                        display: "grid",
                        gridTemplateColumns: "minmax(0, 1.2fr) minmax(0, 1fr)",
                        gap: 6,
                      }}
                    >
                      <div>
                        <div
                          className="mono"
                          style={{
                            fontSize: "var(--text-xs)",
                            color: "var(--text-secondary)",
                            marginBottom: 2,
                          }}
                        >
                          {isPt ? "Estados (Classes)" : "States (Classes)"}
                        </div>
                        <input
                          className="nvr-input mono"
                          style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                          placeholder="closed, open"
                          aria-label="State ROI Classes"
                          value={roiClassesInput}
                          onChange={(e) => setRoiClassesInput(e.target.value)}
                        />
                      </div>
                      <div>
                        <div
                          className="mono"
                          style={{
                            fontSize: "var(--text-xs)",
                            color: "var(--text-secondary)",
                            marginBottom: 2,
                          }}
                        >
                          {isPt ? "Estado Anômalo" : "Anomaly State"}
                        </div>
                        <input
                          className="nvr-input mono"
                          style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                          placeholder="open (optional)"
                          aria-label="State ROI Anomaly State"
                          value={roiAnomalyState}
                          onChange={(e) => setRoiAnomalyState(e.target.value)}
                        />
                      </div>
                    </div>
                    <div
                      style={{
                        display: "grid",
                        gridTemplateColumns: "repeat(3, minmax(0, 1fr))",
                        gap: 6,
                      }}
                    >
                      <div>
                        <div
                          className="mono"
                          style={{
                            fontSize: "var(--text-xs)",
                            color: "var(--text-secondary)",
                            marginBottom: 2,
                          }}
                        >
                          {isPt ? "Intervalo (s)" : "Interval (s)"}
                        </div>
                        <input
                          type="number"
                          min={1}
                          max={300}
                          className="nvr-input mono"
                          style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                          aria-label="State ROI Interval Seconds"
                          value={roiIntervalSec}
                          onChange={(e) =>
                            setRoiIntervalSec(Math.max(1, Number(e.target.value)))
                          }
                        />
                      </div>
                      <div>
                        <div
                          className="mono"
                          style={{
                            fontSize: "var(--text-xs)",
                            color: "var(--text-secondary)",
                            marginBottom: 2,
                          }}
                        >
                          {isPt ? "Histerese" : "Hysteresis"}
                        </div>
                        <input
                          type="number"
                          min={1}
                          max={30}
                          className="nvr-input mono"
                          style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                          aria-label="State ROI Hysteresis Frames"
                          value={roiHysteresis}
                          onChange={(e) =>
                            setRoiHysteresis(Math.max(1, Number(e.target.value)))
                          }
                        />
                      </div>
                      <div>
                        <div
                          className="mono"
                          style={{
                            fontSize: "var(--text-xs)",
                            color: "var(--text-secondary)",
                            marginBottom: 2,
                          }}
                        >
                          {isPt ? "Timeout (s)" : "Timeout (s)"}
                        </div>
                        <input
                          type="number"
                          min={5}
                          max={86400}
                          className="nvr-input mono"
                          style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                          aria-label="State ROI Anomaly Timeout Seconds"
                          value={roiAnomalyTimeoutSec}
                          onChange={(e) =>
                            setRoiAnomalyTimeoutSec(Math.max(1, Number(e.target.value)))
                          }
                        />
                      </div>
                    </div>
                  </div>
                </details>
              </div>
            )}

            {polyType === "measure_box" && (
              <div
                className="mono"
                style={{
                  padding: "6px 8px",
                  background: "var(--bg-input)",
                  border: "1px solid var(--signal-accent)",
                  borderRadius: 2,
                  fontSize: "var(--text-xs)",
                  fontVariantNumeric: "tabular-nums",
                  color: "var(--signal-accent-text)",
                }}
              >
                {draftPoints.length >= 2 ? (
                  (() => {
                    const [a, b] = draftPoints.slice(-2);
                    const wPx = Math.max(1, Math.round(Math.abs(b[0] - a[0]) * detW));
                    const hPx = Math.max(1, Math.round(Math.abs(b[1] - a[1]) * detH));
                    const areaPx = wPx * hPx;
                    const areaPct = ((areaPx / Math.max(1, detW * detH)) * 100).toFixed(2);
                    const ratio = (wPx / hPx).toFixed(2);
                    return `Area: ${areaPx} px² (${areaPct}%) | Ratio: ${ratio} (${wPx}×${hPx})`;
                  })()
                ) : (
                  <span>
                    {isPt
                      ? "Clique 2 cantos opostos no vídeo para medir Área (px & %) e Proporção..."
                      : "Click 2 opposite corners on the video frame to measure Area (px & %) and Ratio..."}
                  </span>
                )}
              </div>
            )}

            {polyType === "tripwire" && (
              <div style={{ display: "flex", gap: 6, alignItems: "center" }}>
                <input
                  className="nvr-input mono"
                  style={{ flex: 1, height: 24, fontSize: "var(--text-xs)" }}
                  placeholder="Tripwire ID (e.g. driveway_line)"
                  aria-label="Tripwire Name"
                  value={zoneNameInput}
                  onChange={(e) => setZoneNameInput(e.target.value)}
                />
                <select
                  className="nvr-select mono"
                  style={{ height: 24, fontSize: "var(--text-xs)" }}
                  aria-label="Tripwire Crossing Direction"
                  value={lineDirection}
                  onChange={(e) =>
                    setLineDirection(e.target.value as "any" | "a_to_b" | "b_to_a")
                  }
                >
                  <option value="any">Direction: ANY</option>
                  <option value="a_to_b">Direction: A &rarr; B</option>
                  <option value="b_to_a">Direction: B &rarr; A</option>
                </select>
              </div>
            )}

            {polyType === "zone" && (
              <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <div style={{ display: "grid", gridTemplateColumns: "1.4fr 1fr 1fr", gap: 6 }}>
                  <div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        marginBottom: 2,
                        whiteSpace: "nowrap",
                      }}
                    >
                      {isPt ? "ID da Zona" : "Zone ID"}
                    </div>
                    <input
                      className="nvr-input mono"
                      style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                      placeholder="e.g. portao"
                      aria-label="Zone Name"
                      value={zoneNameInput}
                      onChange={(e) => setZoneNameInput(e.target.value)}
                    />
                  </div>
                  <div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        marginBottom: 2,
                        whiteSpace: "nowrap",
                      }}
                    >
                      {isPt ? "Inércia (fr)" : "Inertia (fr)"}
                    </div>
                    <input
                      type="number"
                      min={1}
                      max={60}
                      className="nvr-input mono"
                      style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                      aria-label="Zone Inertia Frames"
                      value={zoneInertia}
                      onChange={(e) => setZoneInertia(Math.max(1, Number(e.target.value)))}
                    />
                  </div>
                  <div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        marginBottom: 2,
                        whiteSpace: "nowrap",
                      }}
                    >
                      {isPt ? "Loiter (seg)" : "Loiter (sec)"}
                    </div>
                    <input
                      type="number"
                      min={0}
                      max={300}
                      className="nvr-input mono"
                      style={{ width: "100%", height: 24, fontSize: "var(--text-xs)" }}
                      aria-label="Zone Loitering Seconds"
                      value={zoneLoitering}
                      onChange={(e) => setZoneLoitering(Math.max(0, Number(e.target.value)))}
                    />
                  </div>
                </div>
                <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                  <span
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                      whiteSpace: "nowrap",
                    }}
                  >
                    {isPt ? "PTZ Preset:" : "PTZ Preset:"}
                  </span>
                  <input
                    className="nvr-input mono"
                    style={{ flex: 1, height: 24, fontSize: "var(--text-xs)" }}
                    placeholder={
                      isPt
                        ? "Preset PTZ opcional (ex: entry_gate)"
                        : "Optional PTZ Preset binding (e.g. entry_gate)"
                    }
                    aria-label="Zone PTZ Preset"
                    value={zonePresetInput}
                    onChange={(e) => setZonePresetInput(e.target.value)}
                  />
                </div>
                <div className="nvr-chip-group">
                  {STANDARD_LABELS.slice(0, 6).map((lbl) => {
                    const active = zoneObjects.includes(lbl);
                    return (
                      <button
                        key={lbl}
                        type="button"
                        className={`nvr-btn mono ${active ? "active" : ""}`}
                        style={{ height: 20, padding: "0 6px", fontSize: "var(--text-xs)" }}
                        onClick={() =>
                          setZoneObjects((prev) =>
                            prev.includes(lbl)
                              ? prev.filter((x) => x !== lbl)
                              : [...prev, lbl]
                          )
                        }
                      >
                        {active ? `✓ ${lbl}` : lbl}
                      </button>
                    );
                  })}
                </div>
              </div>
            )}

            {polyType !== "state_roi" && (
              <div
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--signal-motion-text)",
                  wordBreak: currentCoordStr ? "break-all" : "normal",
                  padding: "4px 6px",
                  background: "var(--bg-input)",
                  border: "1px solid var(--border-divider)",
                  borderRadius: 2,
                }}
              >
                {currentCoordStr ||
                  (isPt
                    ? "Clique 3+ pontos no vídeo ao lado para desenhar o polígono..."
                    : "Click 3+ points on the live preview to plot polygon...")}
              </div>
            )}

            <div style={{ display: "flex", gap: 5, flexWrap: "wrap" }}>
              <button
                type="button"
                className="nvr-btn success"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                onClick={handleCommitPolygon}
              >
                {polyType === "state_roi"
                  ? isPt
                    ? "+ Salvar Monitoramento de Área"
                    : "+ Save State Monitor"
                  : polyType === "zone"
                  ? isPt
                    ? "+ Adicionar Zona"
                    : "+ Stage Zone"
                  : polyType === "tripwire"
                  ? isPt
                    ? "+ Adicionar Tripwire"
                    : "+ Stage Tripwire"
                  : isPt
                  ? "+ Adicionar Máscara"
                  : "+ Stage Mask"}
              </button>
              {polyType === "motion_mask" && (
                <button
                  type="button"
                  className="nvr-btn"
                  style={{ height: 24, fontSize: "var(--text-xs)" }}
                  onClick={() =>
                    setDraftPoints([
                      [0.01, 0.01],
                      [0.34, 0.01],
                      [0.34, 0.08],
                      [0.01, 0.08],
                    ])
                  }
                >
                  {isPt ? "Preset Relógio OSD" : "OSD Clock Preset"}
                </button>
              )}
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                onClick={() => {
                  const text =
                    currentCoordStr || "0.010,0.010,0.340,0.010,0.340,0.080,0.010,0.080";
                  navigator.clipboard?.writeText(text);
                  notify("Copied polygon coordinates to clipboard");
                }}
              >
                <Copy size={10} />
                Copy
              </button>
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                disabled={draftPoints.length === 0}
                onClick={() => setDraftPoints((p) => p.slice(0, -1))}
              >
                Undo
              </button>
              <button
                type="button"
                className="nvr-btn danger"
                style={{ height: 24, fontSize: "var(--text-xs)" }}
                disabled={draftPoints.length === 0}
                onClick={() => setDraftPoints([])}
              >
                Clear
              </button>
            </div>
          </div>

          {/* Unified Active Zones & Masks List */}
          <div className="nvr-cfg-card">
            <div className="nvr-cfg-card-header">
              <span>
                {isPt ? "ZONAS" : "ZONES"} ({Object.keys(zonesMap).length}) ·{" "}
                {isPt ? "TRIPWIRES" : "TRIPWIRES"} (
                {Object.keys(camCfg?.lines || {}).length}) ·{" "}
                {isPt ? "ROIS" : "ROIS"} (
                {Object.keys(camCfg?.state_rois || {}).length}) ·{" "}
                {isPt ? "MÁSCARAS" : "MASKS"} ({motionMasks.length + objectMasks.length})
              </span>
              <span style={{ color: "var(--signal-accent-text)" }}>
                CYAN / GREEN / AMBER / RED / VIOLET
              </span>
            </div>
            <div className="nvr-cfg-card-body" style={{ padding: 8, gap: 6 }}>
              {Object.keys(zonesMap).length === 0 &&
                Object.keys(camCfg?.lines || {}).length === 0 &&
                Object.keys(camCfg?.state_rois || {}).length === 0 &&
                motionMasks.length === 0 &&
                objectMasks.length === 0 && (
                  <div style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                    {isPt
                      ? `Nenhuma zona ou máscara configurada em ${inspectorCam}. Clique no vídeo ao lado para desenhar.`
                      : `No zones or masks configured on ${inspectorCam}. Click on the live video to draw one.`}
                  </div>
                )}

              {Object.entries(camCfg?.state_rois || {}).map(([rName, rCfg]) => (
                <div
                  key={`roi-${rName}`}
                  className="nvr-subcard"
                  style={{
                    padding: 6,
                    borderColor: "rgba(245, 158, 11, 0.45)",
                    borderRadius: 2,
                    flexDirection: "row",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div style={{ minWidth: 0, flex: 1 }}>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        color: "var(--signal-motion-text)",
                      }}
                    >
                      STATE ROI: {rName.toUpperCase()} · [
                      {(rCfg.classes || ["closed", "open"]).join(" / ").toUpperCase()}] · hys=
                      {rCfg.hysteresis ?? 3}
                      {rCfg.anomaly_state
                        ? ` · alert=${rCfg.anomaly_state.toUpperCase()}@${
                            rCfg.anomaly_timeout_sec ?? 300
                          }s`
                        : ""}
                    </div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      crop=[{(rCfg.crop || [0.25, 0.25, 0.75, 0.75]).join(", ")}] · interval=
                      {rCfg.interval_sec ?? 5}s
                    </div>
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn"
                      style={{ height: 22, padding: "0 6px" }}
                      title="Load ROI crop and parameters into editor"
                      onClick={() => {
                        setPolyType("state_roi");
                        setZoneNameInput(rName);
                        const c = rCfg.crop || [0.25, 0.25, 0.75, 0.75];
                        setRoiCropInput(c.map((v) => Number(v).toFixed(3)).join(","));
                        setRoiClassesInput((rCfg.classes || ["closed", "open"]).join(", "));
                        setRoiIntervalSec(rCfg.interval_sec ?? 5);
                        setRoiHysteresis(rCfg.hysteresis ?? 3);
                        setRoiAnomalyState(rCfg.anomaly_state || "");
                        setRoiAnomalyTimeoutSec(rCfg.anomaly_timeout_sec ?? 300);
                        setDraftPoints([
                          [c[0], c[1]],
                          [c[2], c[1]],
                          [c[2], c[3]],
                          [c[0], c[3]],
                        ]);
                      }}
                    >
                      <Edit3 size={11} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 22, padding: "0 6px" }}
                      title="Delete State ROI"
                      onClick={() =>
                        updateDraft((next) => {
                          if (next.cameras[inspectorCam]?.state_rois) {
                            delete next.cameras[inspectorCam].state_rois![rName];
                          }
                        })
                      }
                    >
                      <Trash2 size={11} />
                    </button>
                  </div>
                </div>
              ))}

              {Object.entries(camCfg?.lines || {}).map(([lName, lCfg]) => (
                <div
                  key={`line-${lName}`}
                  className="nvr-subcard"
                  style={{
                    padding: 6,
                    borderColor: "rgba(16, 185, 129, 0.45)",
                    borderRadius: 2,
                    flexDirection: "row",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div style={{ minWidth: 0, flex: 1 }}>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        color: "var(--signal-live-text)",
                      }}
                    >
                      TRIPWIRE: {lName.toUpperCase()} · dir=
                      {(lCfg.direction || "any").toUpperCase()}
                    </div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {(lCfg.objects || ["all"]).join(", ")} · [
                      {(lCfg.points || [0, 0, 0, 0]).join(", ")}]
                    </div>
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn"
                      style={{ height: 22, padding: "0 6px" }}
                      title="Load tripwire vertices into canvas"
                      onClick={() => {
                        setPolyType("tripwire");
                        setZoneNameInput(lName);
                        setLineDirection(lCfg.direction || "any");
                        setZoneObjects(lCfg.objects || ["person"]);
                        const pts = lCfg.points || [0.2, 0.5, 0.8, 0.5];
                        setDraftPoints([
                          [pts[0], pts[1]],
                          [pts[2], pts[3]],
                        ]);
                      }}
                    >
                      <Edit3 size={11} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 22, padding: "0 6px" }}
                      title="Delete tripwire"
                      onClick={() =>
                        updateDraft((next) => {
                          if (next.cameras[inspectorCam]?.lines) {
                            delete next.cameras[inspectorCam].lines![lName];
                          }
                        })
                      }
                    >
                      <Trash2 size={11} />
                    </button>
                  </div>
                </div>
              ))}

              {Object.entries(zonesMap).map(([zName, zCfg]) => (
                <div
                  key={zName}
                  className="nvr-subcard"
                  style={{
                    padding: 6,
                    borderColor: "rgba(14, 165, 233, 0.35)",
                    borderRadius: 2,
                    flexDirection: "row",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div style={{ minWidth: 0, flex: 1 }}>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        fontWeight: 700,
                        color: "var(--signal-accent-text)",
                      }}
                    >
                      ZONE: {zName.toUpperCase()} · inertia={zCfg.inertia ?? 3} · loiter=
                      {zCfg.loitering_time ?? 0}s
                      {zCfg.preset ? ` · preset=${zCfg.preset}` : ""}
                    </div>
                    <div
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-secondary)",
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {(zCfg.objects || ["all"]).join(", ")} · {String(zCfg.coordinates)}
                    </div>
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn"
                      style={{ height: 22, padding: "0 6px" }}
                      title="Load zone vertices into canvas"
                      onClick={() => {
                        setPolyType("zone");
                        setZoneNameInput(zName);
                        setZoneInertia(zCfg.inertia ?? 3);
                        setZoneLoitering(zCfg.loitering_time ?? 0);
                        setZonePresetInput(zCfg.preset || "");
                        setZoneObjects(zCfg.objects || ["person"]);
                        setDraftPoints(
                          coordStringToPoints(String(zCfg.coordinates), detW, detH)
                        );
                      }}
                    >
                      <Edit3 size={11} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 22, padding: "0 6px" }}
                      title="Delete zone"
                      onClick={() =>
                        updateDraft((next) => {
                          delete next.cameras[inspectorCam].zones[zName];
                        })
                      }
                    >
                      <Trash2 size={11} />
                    </button>
                  </div>
                </div>
              ))}

              {privacyMasks.map((m, idx) => (
                <div
                  key={`pm-${idx}`}
                  className="nvr-subcard"
                  style={{
                    padding: 6,
                    borderColor: "var(--border-strong)",
                    borderRadius: 2,
                    flexDirection: "row",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    <strong style={{ color: "var(--text-primary)" }}>
                      PRIVACY REDACT #{idx + 1}:
                    </strong>{" "}
                    {m}
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() =>
                        updateDraft((next) => {
                          const list = extractMaskPolygons(
                            next.cameras[inspectorCam].privacy_mask
                          );
                          list.splice(idx, 1);
                          next.cameras[inspectorCam].privacy_mask = list;
                        })
                      }
                    >
                      <Trash2 size={10} />
                    </button>
                  </div>
                </div>
              ))}

              {motionMasks.map((m, idx) => (
                <div
                  key={`mm-${idx}`}
                  className="nvr-subcard"
                  style={{
                    padding: 6,
                    borderColor: "rgba(239, 68, 68, 0.35)",
                    borderRadius: 2,
                    flexDirection: "row",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    <strong style={{ color: "var(--signal-alert-text)" }}>
                      MOTION MASK #{idx + 1}:
                    </strong>{" "}
                    {m}
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() => {
                        setPolyType("motion_mask");
                        setDraftPoints(coordStringToPoints(m, detW, detH));
                      }}
                    >
                      <Edit3 size={10} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() =>
                        updateDraft((next) => {
                          const list = extractMaskPolygons(
                            next.cameras[inspectorCam].motion.mask
                          );
                          list.splice(idx, 1);
                          next.cameras[inspectorCam].motion.mask = list;
                        })
                      }
                    >
                      <Trash2 size={10} />
                    </button>
                  </div>
                </div>
              ))}

              {objectMasks.map((m, idx) => (
                <div
                  key={`om-${idx}`}
                  className="nvr-subcard"
                  style={{
                    padding: 6,
                    borderColor: "rgba(168, 85, 247, 0.4)",
                    borderRadius: 2,
                    flexDirection: "row",
                    alignItems: "center",
                    justifyContent: "space-between",
                    gap: 6,
                  }}
                >
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    <strong style={{ color: "var(--signal-accent-text)" }}>
                      OBJECT MASK #{idx + 1}:
                    </strong>{" "}
                    {m}
                  </div>
                  <div style={{ display: "flex", gap: 4 }}>
                    <button
                      type="button"
                      className="nvr-btn"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() => {
                        setPolyType("object_mask");
                        setDraftPoints(coordStringToPoints(m, detW, detH));
                      }}
                    >
                      <Edit3 size={10} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 20, padding: "0 5px" }}
                      onClick={() =>
                        updateDraft((next) => {
                          const list = extractMaskPolygons(
                            next.cameras[inspectorCam].objects?.mask
                          );
                          list.splice(idx, 1);
                          if (next.cameras[inspectorCam].objects) {
                            next.cameras[inspectorCam].objects!.mask = list;
                          }
                        })
                      }
                    >
                      <Trash2 size={10} />
                    </button>
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>
      </div>

      {/* Right Interactive Camera Video Stage + Docked SIMD Motion Tuner Bar */}
      <div className="nvr-focus-stage">
        <div className="nvr-cam-tile" style={{ flex: 1 }}>
          <LiveMsePlayer
            camera={inspectorCam}
            useMainStream={false}
            cameraConfig={camCfg}
            showVectors={true}
            draftPoints={draftPoints}
            onPlotPoint={(nx, ny) => setDraftPoints((prev) => [...prev, [nx, ny]])}
          />
          <div className="nvr-cam-hud pos-top-right" style={{ pointerEvents: "none" }}>
            <span className="nvr-badge live">
              {inspectorCam.toUpperCase()} // VECTORS ACTIVE
            </span>
            <span className="nvr-badge">
              RED MOTION · VIOLET OBJECT · CYAN ZONE · AMBER DRAFT
            </span>
          </div>
        </div>

        {/* Docked SIMD Motion Tuner Deck Below Live Preview */}
        <div
          style={{
            background: "var(--bg-panel)",
            borderTop: "1px solid var(--border-hairline)",
            padding: "8px 12px",
            display: "flex",
            flexDirection: "column",
            gap: 8,
          }}
        >
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: 8,
              flexWrap: "wrap",
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--text-secondary)",
                }}
              >
                SIMD MOTION TUNER ({inspectorCam.toUpperCase()})
              </span>
              <ToggleSwitch
                checked={motionCfg.enabled ?? true}
                label="Enable Motion Detector"
                onChange={(v) => updateMotionField("enabled", v)}
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 5 }}>
              <button
                type="button"
                className={`nvr-btn ${isIndoorMotion ? "active" : ""}`}
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => applyMotionPreset(22, 8, 0.85, "Indoor / High Sensitivity")}
              >
                {isPt ? "Interno (Sensível)" : "Indoor (Sensitive)"}
              </button>
              <button
                type="button"
                className={`nvr-btn ${isBalancedMotion ? "active" : ""}`}
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => applyMotionPreset(30, 10, 0.8, "Balanced Default")}
              >
                {isPt ? "Equilibrado (Padrão)" : "Balanced (Default)"}
              </button>
              <button
                type="button"
                className={`nvr-btn ${isOutdoorMotion ? "active" : ""}`}
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => applyMotionPreset(45, 25, 0.65, "Outdoor / Trees & Wind")}
              >
                {isPt ? "Externo (Vento/Sol)" : "Outdoor (Trees/Wind)"}
              </button>
              <span className="nvr-divider" aria-hidden="true" />
              <span
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                Contrast:
              </span>
              <ToggleSwitch
                checked={motionCfg.improve_contrast ?? true}
                label="Improve Contrast"
                onChange={(v) => updateMotionField("improve_contrast", v)}
              />
              <button
                type="button"
                className="nvr-btn"
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  updateDraft((next) => {
                    Object.values(next.cameras).forEach((cam) => {
                      cam.motion = {
                        ...cam.motion,
                        enabled: motionCfg.enabled ?? true,
                        threshold: motionCfg.threshold ?? 30,
                        contour_area: motionCfg.contour_area ?? 10,
                        lightning_threshold: motionCfg.lightning_threshold ?? 0.8,
                        improve_contrast: motionCfg.improve_contrast ?? true,
                        frame_height: motionCfg.frame_height ?? 100,
                      };
                    });
                  });
                  notify("Copied motion sensitivity parameters to all cameras");
                }}
              >
                {isPt ? "Aplicar a Todas" : "Apply to All"}
              </button>
            </div>
          </div>

          <div
            style={{
              display: "grid",
              gridTemplateColumns: "minmax(0, 1fr) minmax(0, 1fr) auto auto",
              gap: 10,
              alignItems: "center",
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 6, minWidth: 0 }}>
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                  whiteSpace: "nowrap",
                }}
              >
                {isPt ? "Luminância:" : "Threshold:"}{" "}
                <strong style={{ color: "var(--text-primary)" }}>
                  {motionCfg.threshold ?? 30}
                </strong>
              </span>
              <input
                type="range"
                min={5}
                max={120}
                style={{ flex: 1, minWidth: 0, width: "100%" }}
                aria-label="Motion Threshold"
                value={motionCfg.threshold ?? 30}
                onChange={(e) => updateMotionField("threshold", Number(e.target.value))}
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 6, minWidth: 0 }}>
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                  whiteSpace: "nowrap",
                }}
              >
                {isPt ? "Contorno:" : "Contour:"}{" "}
                <strong style={{ color: "var(--text-primary)" }}>
                  {motionCfg.contour_area ?? 10}px
                </strong>
              </span>
              <input
                type="range"
                min={3}
                max={150}
                style={{ flex: 1, minWidth: 0, width: "100%" }}
                aria-label="Contour Area"
                value={motionCfg.contour_area ?? 10}
                onChange={(e) =>
                  updateMotionField("contour_area", Number(e.target.value))
                }
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 5, whiteSpace: "nowrap" }}>
              <span
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                Lightning:
              </span>
              <input
                type="number"
                step={0.05}
                min={0.2}
                max={1.0}
                className="nvr-input mono"
                style={{ width: 54, height: 24 }}
                value={Number((motionCfg.lightning_threshold ?? 0.8).toFixed(2))}
                onChange={(e) =>
                  updateMotionField("lightning_threshold", Number(e.target.value))
                }
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 5, whiteSpace: "nowrap" }}>
              <span
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                Scale H:
              </span>
              <input
                type="number"
                min={32}
                max={360}
                className="nvr-input mono"
                style={{ width: 54, height: 24 }}
                value={motionCfg.frame_height ?? 100}
                onChange={(e) =>
                  updateMotionField("frame_height", Number(e.target.value))
                }
              />
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

export const AiDetectionObjectsSection: React.FC<{
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  scope: "global" | string;
  inspectorCam: string;
  notify: (msg: string) => void;
}> = ({ draftConfig, updateDraft, scope, inspectorCam, notify }) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const stats = useNvrStore((s) => s.stats);
  const [customLabel, setCustomLabel] = useState("");
  const [selectedFilterLabel, setSelectedFilterLabel] = useState("person");
  const [showAdvancedModel, setShowAdvancedModel] = useState(false);
  const [showAdvancedByteTrack, setShowAdvancedByteTrack] = useState(false);
  const [showAdvancedFilterBox, setShowAdvancedFilterBox] = useState(false);
  const [isCustomResolution, setIsCustomResolution] = useState(false);
  const [fpSignatures, setFpSignatures] = useState<FalsePositiveSignatureItem[]>([]);
  const [fpLoading, setFpLoading] = useState(false);

  const loadFpSignatures = async () => {
    setFpLoading(true);
    try {
      const res = await fetch("/api/false_positives/signatures", {
        credentials: "include",
      });
      if (res.ok) {
        const rows = await res.json();
        if (Array.isArray(rows)) {
          setFpSignatures(rows);
        }
      }
    } catch {
      // ignore
    } finally {
      setFpLoading(false);
    }
  };

  useEffect(() => {
    void loadFpSignatures();
  }, []);

  const handleDeleteFpSignature = async (id: number) => {
    try {
      const res = await fetch(
        `/api/false_positives/signatures/${encodeURIComponent(String(id))}`,
        {
          method: "DELETE",
          credentials: "include",
        }
      );
      if (res.ok) {
        setFpSignatures((prev) => prev.filter((item) => item.id !== id));
        notify(`Deleted false-positive memory signature #${id}`);
      } else {
        notify(`Failed to delete false-positive signature #${id}`);
      }
    } catch {
      notify(`Network error while deleting false-positive signature #${id}`);
    }
  };

  const genaiCfg: GenAiConfig = draftConfig.genai || {
    enabled: false,
    provider: "gemini",
    base_url: null,
    api_key: null,
    has_api_key: false,
    api_key_env: "GEMINI_API_KEY",
    model: "",
    verify_ambiguous_alerts: false,
    ambiguous_score_min: 0.45,
    ambiguous_score_max: 0.72,
    auto_suppress_false_positives: true,
    timeout_ms: 30000,
  };

  const [genaiModels, setGenaiModels] = useState<string[]>([]);
  const [genaiValidating, setGenaiValidating] = useState(false);
  const [genaiValidationState, setGenaiValidationState] = useState<{
    status: "idle" | "valid" | "invalid";
    message?: string;
  }>({ status: "idle" });

  const updateGenAi = (patch: Partial<GenAiConfig>) => {
    updateDraft((next) => {
      next.genai = {
        ...genaiCfg,
        ...(next.genai || {}),
        ...patch,
      };
    });
  };

  const handleValidateGenAiApi = async () => {
    setGenaiValidating(true);
    setGenaiValidationState({ status: "idle" });
    const prov = (genaiCfg.provider || "gemini").trim().toLowerCase();
    const directKey =
      genaiCfg.api_key && genaiCfg.api_key !== "***" && genaiCfg.api_key !== "*"
        ? genaiCfg.api_key.trim()
        : "";
    const customBase = (genaiCfg.base_url || "").trim().replace(/\/+$/, "");

    const applyValidModels = (models: string[], customMsg?: string) => {
      setGenaiModels(models);
      const msg =
        customMsg ||
        (isPt
          ? `API validada (${models.length} modelos disponíveis)`
          : `API validated (${models.length} models available)`);
      setGenaiValidationState({ status: "valid", message: msg });
      if (genaiCfg.model && !models.includes(genaiCfg.model)) {
        updateGenAi({ model: "" });
      }
      notify(msg);
    };

    try {
      const res = await fetch("/api/genai/models", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          provider: prov,
          api_key: directKey || undefined,
          api_key_env: genaiCfg.api_key_env || "GEMINI_API_KEY",
          base_url: customBase || null,
          clear_api_key: genaiCfg.clear_api_key || false,
        }),
      });
      const data = await res.json().catch(() => ({}));
      const loadedModels = normalizeDiscoveredGenAiModels(data, prov);
      if (res.ok && loadedModels.length > 0) {
        applyValidModels(loadedModels, data?.message);
        return;
      }

      // Direct browser fallback when user provided an API key or a safe custom endpoint
      const canUseDirectFallback =
        res.status !== 400 &&
        res.status !== 403 &&
        (!customBase || isSafeGenAiCustomBaseUrl(customBase));
      if (canUseDirectFallback && (directKey || prov === "ollama" || customBase)) {
        let directUrl = "";
        const headers: Record<string, string> = {};
        if (prov === "openai") {
          const base = customBase || "https://api.openai.com/v1";
          directUrl = base.endsWith("/models")
            ? base
            : base.endsWith("/v1")
            ? `${base}/models`
            : `${base}/v1/models`;
          if (directKey) headers.Authorization = `Bearer ${directKey}`;
        } else if (prov === "ollama") {
          const base = customBase || "http://127.0.0.1:11434";
          directUrl =
            base.endsWith("/api/tags") || base.endsWith("/models")
              ? base
              : `${base}/api/tags`;
          if (directKey) headers.Authorization = `Bearer ${directKey}`;
        } else {
          const base = customBase || "https://generativelanguage.googleapis.com";
          if (base.endsWith("/v1") || base.endsWith("/v1/models")) {
            directUrl = base.endsWith("/models") ? base : `${base}/models`;
            if (directKey) headers.Authorization = `Bearer ${directKey}`;
          } else {
            const endpoint = base.endsWith("/models")
              ? base
              : `${base}/v1beta/models`;
            directUrl = `${endpoint}?pageSize=100`;
            if (directKey) headers["x-goog-api-key"] = directKey;
          }
        }

        const directRes = await fetch(directUrl, { method: "GET", headers });
        const directJson = await directRes.json().catch(() => ({}));
        const directModels = normalizeDiscoveredGenAiModels(directJson, prov);
        if (directRes.ok && directModels.length > 0) {
          applyValidModels(directModels);
          return;
        }
        const apiErr =
          directJson?.error?.message ||
          data?.message ||
          (isPt
            ? `Falha ao validar API (HTTP ${directRes.status})`
            : `API validation failed (HTTP ${directRes.status})`);
        setGenaiModels([]);
        setGenaiValidationState({ status: "invalid", message: apiErr });
        notify(apiErr);
        return;
      }

      setGenaiModels([]);
      const errMsg =
        data?.message ||
        (isPt
          ? "Informe uma chave de API válida para carregar os modelos"
          : "Provide a valid API key to validate and load available models");
      setGenaiValidationState({ status: "invalid", message: errMsg });
      notify(errMsg);
    } catch {
      setGenaiModels([]);
      const errMsg = isPt
        ? "Erro de conexão ao validar API do provedor"
        : "Network error while validating provider API";
      setGenaiValidationState({ status: "invalid", message: errMsg });
      notify(errMsg);
    } finally {
      setGenaiValidating(false);
    }
  };

  const genaiModelOptions =
    genaiValidationState.status === "valid"
      ? genaiModels
      : genaiCfg.enabled && genaiCfg.model
      ? [genaiCfg.model]
      : [];

  const [enrolledProfiles, setEnrolledProfiles] = useState<EnrolledFaceProfileItem[]>([]);
  const [enrollNameInput, setEnrollNameInput] = useState("");
  const [enrollImageB64, setEnrollImageB64] = useState<string>("");
  const [enrollBusy, setEnrollBusy] = useState(false);
  const [excludeSubLabelInput, setExcludeSubLabelInput] = useState("");
  const [newKnownPlate, setNewKnownPlate] = useState("");
  const [newKnownPlateLabel, setNewKnownPlateLabel] = useState("");
  const [detectedPlates, setDetectedPlates] = useState<string[]>([]);

  const fetchDetectedPlates = async () => {
    try {
      const r = await fetch("/api/recognized_license_plates", {
        credentials: "include",
      });
      if (r.ok) {
        const list = await r.json();
        if (Array.isArray(list)) {
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

  const lprPersistSeqRef = React.useRef(0);
  const lprCfg: LprConfig = draftConfig.lpr || {
    enabled: false,
    min_confidence: 0.75,
    format_regex: DEFAULT_LPR_FORMAT_REGEX,
    known_plates: {},
  };
  const updateLpr = (patch: Partial<LprConfig>, persistImmediately = true) => {
    const mergedLpr: LprConfig = {
      enabled: draftConfig.lpr?.enabled ?? false,
      min_confidence: draftConfig.lpr?.min_confidence ?? 0.75,
      format_regex: draftConfig.lpr?.format_regex ?? DEFAULT_LPR_FORMAT_REGEX,
      detector_model_path: draftConfig.lpr?.detector_model_path,
      ocr_model_path: draftConfig.lpr?.ocr_model_path,
      known_plates: { ...(draftConfig.lpr?.known_plates || {}) },
      ...patch,
    };
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

  const faceCfg: FaceRecognitionConfig = draftConfig.face_recognition || {
    enabled: false,
    model_path: "/config/models/arcface_r50.onnx",
    similarity_threshold: 0.68,
    min_face_area: 750,
    unknown_alert: false,
  };

  const activeExcludeSubLabels: string[] =
    (scope === "global"
      ? draftConfig.review?.alerts?.exclude_sub_labels
      : draftConfig.cameras?.[inspectorCam]?.review?.alerts?.exclude_sub_labels ??
        draftConfig.review?.alerts?.exclude_sub_labels) || [];

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
    const nextList = toggleSubLabelRuleList(activeExcludeSubLabels, name);
    updateDraft((next) => {
      if (scope === "global") {
        if (!next.review) {
          next.review = {
            alerts: {
              enabled: true,
              labels: ["person", "car"],
              required_zones: [],
              cutoff_time: 40,
            },
            detections: {
              enabled: true,
              required_zones: [],
              cutoff_time: 30,
            },
          };
        }
        next.review.alerts.exclude_sub_labels = nextList;
      } else if (next.cameras[inspectorCam]) {
        if (!next.cameras[inspectorCam].review) {
          next.cameras[inspectorCam].review = {
            alerts: {
              enabled: true,
              labels: ["person", "car"],
              required_zones: [],
              cutoff_time: 40,
            },
            detections: {
              enabled: true,
              required_zones: [],
              cutoff_time: 30,
            },
          };
        }
        next.cameras[inspectorCam].review!.alerts.exclude_sub_labels = nextList;
      }
    });
  };

  const refreshEnrolledFaces = async () => {
    try {
      const res = await fetch("/api/faces", { credentials: "include" });
      if (res.ok) {
        const data = await res.json();
        setEnrolledProfiles(parseFacesApiResponse(data));
      }
    } catch {
      // ignore offline/unit environment
    }
  };

  useEffect(() => {
    void refreshEnrolledFaces();
  }, []);

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
      if (res.ok) {
        setEnrollNameInput("");
        setEnrollImageB64("");
        await refreshEnrolledFaces();
        notify(`Enrolled face profile '${cleanName}' into local gallery`);
      } else {
        const data = await res.json().catch(() => null);
        notify(data?.message || `Failed to enroll face profile '${cleanName}'`);
      }
    } catch {
      notify(`Network error while enrolling face profile '${cleanName}'`);
    } finally {
      setEnrollBusy(false);
    }
  };

  const handleDeleteProfile = async (name: string) => {
    try {
      const res = await fetch(`/api/faces/${encodeURIComponent(name)}`, {
        method: "DELETE",
        credentials: "include",
      });
      if (res.ok) {
        const data = await res.json().catch(() => ({}));
        setEnrolledProfiles((prev) =>
          prev.filter((p) => p.name.trim().toLowerCase() !== name.trim().toLowerCase())
        );
        useNvrStore.getState().recordDeletedFaceProfile(name);
        await refreshEnrolledFaces();
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
      notify(`Network error while deleting face profile '${name}'`);
    }
  };

  const [newRoiName, setNewRoiName] = useState("gate");
  const [newRoiCrop, setNewRoiCrop] = useState("0.200,0.300,0.650,0.850");
  const [newRoiClasses, setNewRoiClasses] = useState("closed, open");
  const [newRoiIntervalSec, setNewRoiIntervalSec] = useState(5);
  const [newRoiHysteresis, setNewRoiHysteresis] = useState(3);
  const [newRoiAnomalyState, setNewRoiAnomalyState] = useState("open");
  const [newRoiAnomalyTimeoutSec, setNewRoiAnomalyTimeoutSec] = useState(300);
  const [newRoiScenario, setNewRoiScenario] = useState<StateRoiPresetId>("door_gate");

  const [simLprCamera, setSimLprCamera] = useState<string>(inspectorCam || "escritorio");
  const [simLprPlate, setSimLprPlate] = useState<string>("BRA2E19");
  const [simLprBusy, setSimLprBusy] = useState<boolean>(false);
  const [simLprLastResult, setSimLprLastResult] = useState<{
    eventId: string;
    plate: string;
    camera: string;
    subLabel?: string | null;
  } | null>(null);

  useEffect(() => {
    if (inspectorCam) {
      setSimLprCamera(inspectorCam);
    }
  }, [inspectorCam]);

  const handleSimulateLprPass = async () => {
    if (simLprBusy) return;
    setSimLprBusy(true);
    try {
      const targetCam =
        simLprCamera ||
        inspectorCam ||
        Object.keys(draftConfig.cameras || {})[0] ||
        "escritorio";
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
      notify("Failed to simulate LPR vehicle pass");
    } finally {
      setSimLprBusy(false);
    }
  };

  const detKeys = Object.keys(draftConfig.detectors || { detector01: {} });
  const primaryDetKey = detKeys[0] || "detector01";
  const primaryDet = draftConfig.detectors?.[primaryDetKey] || {
    type: "openvino",
    device: "CPU",
  };
  const liveInferMs =
    stats?.detectors?.[primaryDetKey]?.inference_speed?.toFixed(1) || "7.0";

  const modelCfg = draftConfig.model || {
    path: "/openvino-model/ssdlite_mobilenet_v2.xml",
    labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
    width: 300,
    height: 300,
    input_tensor: "nhwc",
    input_pixel_format: "bgr",
    model_type: "ssd",
  };

  const activeDetect: CameraDetectConfig =
    scope === "global"
      ? draftConfig.detect || {
          enabled: true,
          width: 1280,
          height: 720,
          fps: 5,
          min_initialized: 2,
          max_disappeared: 25,
          stationary: { interval: 50, threshold: 50, classifier: true },
        }
      : draftConfig.cameras?.[inspectorCam]?.detect || {
          enabled: true,
          width: 1280,
          height: 720,
          fps: 5,
          min_initialized: 2,
          max_disappeared: 25,
          stationary: { interval: 50, threshold: 50, classifier: true },
        };

  const trackedLabels: string[] =
    scope === "global"
      ? draftConfig.objects?.track || ["person"]
      : draftConfig.cameras?.[inspectorCam]?.objects?.track ||
        draftConfig.objects?.track || ["person"];

  const attributeLabels: string[] =
    scope === "global"
      ? draftConfig.objects?.attribute_labels ?? DEFAULT_ATTRIBUTE_LABELS
      : draftConfig.cameras?.[inspectorCam]?.objects?.attribute_labels ??
        draftConfig.objects?.attribute_labels ??
        DEFAULT_ATTRIBUTE_LABELS;

  const activeSnapshotSelection =
    (scope === "global"
      ? draftConfig.snapshots?.selection
      : draftConfig.cameras?.[inspectorCam]?.snapshots?.selection ??
        draftConfig.snapshots?.selection) || "best_in_zone";

  const toggleAttributeLabel = (lbl: string) => {
    updateDraft((next) => {
      if (scope === "global") {
        if (!next.objects) next.objects = { track: ["person"], filters: {} };
        next.objects.attribute_labels = toggleAttributeLabelList(
          next.objects.attribute_labels ?? attributeLabels,
          lbl
        );
      } else if (next.cameras[inspectorCam]) {
        if (!next.cameras[inspectorCam].objects) {
          next.cameras[inspectorCam].objects = {
            track: [...trackedLabels],
            filters: {},
          };
        }
        next.cameras[inspectorCam].objects!.attribute_labels =
          toggleAttributeLabelList(
            next.cameras[inspectorCam].objects!.attribute_labels ??
              attributeLabels,
            lbl
          );
      }
    });
  };

  const updateSnapshotSelection = (
    selection: "best_overall" | "best_in_zone" | "first_trigger" | "best_attribute"
  ) => {
    updateDraft((next) => {
      if (scope === "global") {
        if (!next.snapshots) next.snapshots = { enabled: true };
        next.snapshots.selection = selection;
      } else if (next.cameras[inspectorCam]) {
        if (!next.cameras[inspectorCam].snapshots) {
          next.cameras[inspectorCam].snapshots = { enabled: true };
        }
        next.cameras[inspectorCam].snapshots.selection = selection;
      }
    });
  };

  // Keep selectedFilterLabel pointed at an active tracked class whenever possible
  useEffect(() => {
    if (trackedLabels.length > 0 && !trackedLabels.includes(selectedFilterLabel)) {
      setSelectedFilterLabel(trackedLabels[0]);
    }
  }, [trackedLabels, selectedFilterLabel]);

  const activeFilter: ObjectFilterConfig =
    (scope === "global"
      ? draftConfig.objects?.filters?.[selectedFilterLabel]
      : draftConfig.cameras?.[inspectorCam]?.objects?.filters?.[selectedFilterLabel]) || {
      min_area: 0,
      max_area: 24000000,
      min_ratio: 0,
      max_ratio: 24000000,
      threshold: 0.7,
      min_score: 0.5,
    };

  const updateDetect = (patch: Partial<CameraDetectConfig>) => {
    updateDraft((next) => {
      if (scope === "global") {
        next.detect = { ...activeDetect, ...patch };
      } else if (next.cameras[inspectorCam]) {
        next.cameras[inspectorCam].detect = {
          ...next.cameras[inspectorCam].detect,
          ...patch,
        };
      }
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

  const toggleTrackLabel = (lbl: string) => {
    const isCurrentlyTracked = trackedLabels.includes(lbl);
    if (isCurrentlyTracked && trackedLabels.length <= 1) {
      setSelectedFilterLabel(lbl);
      notify(
        isPt
          ? "Pelo menos 1 classe de objeto deve permanecer ativa"
          : "At least 1 object class must remain tracked"
      );
      return;
    }
    if (!isCurrentlyTracked) {
      setSelectedFilterLabel(lbl);
    }
    updateDraft((next) => {
      if (scope === "global") {
        if (!next.objects) next.objects = { track: ["person"], filters: {} };
        const curr = next.objects.track || [];
        next.objects.track = curr.includes(lbl)
          ? curr.filter((x) => x !== lbl)
          : [...curr, lbl];
      } else if (next.cameras[inspectorCam]) {
        if (!next.cameras[inspectorCam].objects) {
          next.cameras[inspectorCam].objects = {
            track: [...trackedLabels],
            filters: {},
          };
        }
        const curr = next.cameras[inspectorCam].objects!.track || [];
        next.cameras[inspectorCam].objects!.track = curr.includes(lbl)
          ? curr.filter((x) => x !== lbl)
          : [...curr, lbl];
      }
    });
  };

  const updateObjectFilter = (patch: Partial<ObjectFilterConfig>) => {
    updateDraft((next) => {
      const merged: ObjectFilterConfig = { ...activeFilter, ...patch };
      if (patch.min_score !== undefined && merged.min_score > merged.threshold) {
        merged.threshold = merged.min_score;
      }
      if (patch.threshold !== undefined && merged.threshold < merged.min_score) {
        merged.min_score = merged.threshold;
      }
      if (scope === "global") {
        if (!next.objects) next.objects = { track: ["person"], filters: {} };
        if (!next.objects.filters) next.objects.filters = {};
        next.objects.filters[selectedFilterLabel] = merged;
      } else if (next.cameras[inspectorCam]) {
        if (!next.cameras[inspectorCam].objects) {
          next.cameras[inspectorCam].objects = {
            track: [...trackedLabels],
            filters: {},
          };
        }
        if (!next.cameras[inspectorCam].objects!.filters) {
          next.cameras[inspectorCam].objects!.filters = {};
        }
        next.cameras[inspectorCam].objects!.filters![selectedFilterLabel] = merged;
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

  const confThresholdPct = Math.round((activeFilter.threshold ?? 0.7) * 100);
  const confMinScorePct = Math.round((activeFilter.min_score ?? 0.5) * 100);
  const activeConfPreset: "permissive" | "balanced" | "strict" | "custom" =
    confThresholdPct === 58 && confMinScorePct === 40
      ? "permissive"
      : confThresholdPct === 70 && confMinScorePct === 50
      ? "balanced"
      : confThresholdPct === 82 && confMinScorePct === 65
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
    <div className="nvr-card-grid" style={{ gap: 10 }}>
      {/* Guided OpenVINO AI Detector & Model Card */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "MOTOR DE IA & PERFIL DO MODELO (GLOBAL)"
              : "AI DETECTION ENGINE & MODEL PROFILE (GLOBAL)"}
          </span>
          <span className="nvr-badge live">
            <CheckCircle2 size={11} />
            {(primaryDet.type || "openvino").toUpperCase()} ·{" "}
            {primaryDet.device || "CPU"} ACTIVE · {liveInferMs}ms
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 7 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            {modelProfiles.map((prof) => {
              const isSelected = (modelCfg.model_type || "ssd") === prof.id;
              return (
                <div
                  key={prof.id}
                  role="button"
                  tabIndex={0}
                  onClick={() => selectModelProfile(prof.id)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      selectModelProfile(prof.id);
                    }
                  }}
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
                </div>
              );
            })}
          </div>

          {/* Step 2: Inference Runtime Backend & Hardware Accelerator */}
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
              <option value="openvino">
                {isPt
                  ? "OpenVINO — CPU / Intel iGPU / NPU"
                  : "OpenVINO — CPU / Intel iGPU / NPU"}
              </option>
              <option value="onnxruntime">
                {isPt
                  ? "ONNX Runtime — CUDA / TensorRT / ROCm / ARM64"
                  : "ONNX Runtime — CUDA / TensorRT / ROCm / ARM64"}
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

          {/* Collapsible Advanced Tensor & Path Overrides */}
          <button
            type="button"
            className="nvr-btn mono"
            style={{
              width: "100%",
              justifyContent: "space-between",
              height: 24,
              fontSize: "var(--text-xs)",
              marginTop: "auto",
            }}
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
              <div style={{ display: "grid", gridTemplateColumns: "repeat(4, minmax(0, 1fr))", gap: 6 }}>
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

      {/* Guided Detect & ByteTrack Behavior Card */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt ? "DETECÇÃO & RASTREAMENTO" : "CAMERA DETECTION & TRACKING"} (
            {scope === "global"
              ? isPt
                ? "PADRÃO GLOBAL"
                : "GLOBAL"
              : inspectorCam.toUpperCase()}
            )
          </span>
          <button
            type="button"
            className="nvr-btn"
            style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
            onClick={() => {
              updateDraft((next) => {
                Object.values(next.cameras).forEach((cam) => {
                  cam.detect = { ...activeDetect };
                });
              });
              notify("Applied detection & tracking settings to all cameras");
            }}
          >
            {isPt ? "Aplicar a Todas" : "Apply to All Cameras"}
          </button>
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

          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Roteamento de Detector IA"
                  : "Assigned AI Detector Instance"}
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
                style={{ height: 28, padding: "0 8px", fontSize: "var(--text-xs)" }}
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
                  });
                  updateDetect({ detector: newKey });
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
            <div style={{ display: "grid", gridTemplateColumns: "repeat(2, minmax(0, 1fr))", gap: 8 }}>
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

          {/* 1-Click Tracking Sensitivity Presets */}
          <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", fontWeight: 700 }}>
            {isPt ? "PERFIL DE RESPOSTA DO RASTREADOR" : "TRACKING RESPONSIVENESS PRESET"}
          </div>
          <div style={{ display: "grid", gridTemplateColumns: "repeat(3, minmax(0, 1fr))", gap: 6 }}>
            <button
              type="button"
              className={`nvr-btn ${activeTrackPreset === "balanced" ? "active" : ""}`}
              style={{
                height: "auto",
                minWidth: 0,
                padding: "6px 8px",
                flexDirection: "column",
                alignItems: "flex-start",
                textAlign: "left",
                whiteSpace: "normal",
                gap: 2,
              }}
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
              className={`nvr-btn ${activeTrackPreset === "fast" ? "active" : ""}`}
              style={{
                height: "auto",
                minWidth: 0,
                padding: "6px 8px",
                flexDirection: "column",
                alignItems: "flex-start",
                textAlign: "left",
                whiteSpace: "normal",
                gap: 2,
              }}
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
              className={`nvr-btn ${activeTrackPreset === "strict" ? "active" : ""}`}
              style={{
                height: "auto",
                minWidth: 0,
                padding: "6px 8px",
                flexDirection: "column",
                alignItems: "flex-start",
                textAlign: "left",
                whiteSpace: "normal",
                gap: 2,
              }}
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

          {/* Collapsible Advanced ByteTrack Frame Parameters */}
          <button
            type="button"
            className="nvr-btn mono"
            style={{
              width: "100%",
              justifyContent: "space-between",
              height: 24,
              fontSize: "var(--text-xs)",
              marginTop: "auto",
            }}
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
                gridTemplateColumns: "repeat(5, minmax(0, 1fr))",
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

      {/* Tracked Objects & Intuitive Confidence Sliders */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "CLASSES DE OBJETOS MONITORADAS & SENSIBILIDADE DE CONFIANÇA"
              : "OBJECTS TO DETECT & CONFIDENCE SENSITIVITY"}{" "}
            ({scope === "global" ? "GLOBAL" : inspectorCam.toUpperCase()})
          </span>
          <span className="mono">
            {trackedLabels.length} {isPt ? "CLASSES ATIVAS" : "ACTIVE CLASSES"}
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
            <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", marginRight: 4 }}>
              {isPt ? "1. ATIVAR/DESATIVAR CLASSES:" : "1. ENABLE/DISABLE CLASSES:"}
            </span>
            {Array.from(new Set([...STANDARD_LABELS, ...trackedLabels])).map((lbl) => {
              const isTracked = trackedLabels.includes(lbl);
              return (
                <button
                  key={lbl}
                  type="button"
                  className={`nvr-btn mono ${isTracked ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() => toggleTrackLabel(lbl)}
                >
                  {isTracked ? `✓ ${lbl}` : `+ ${lbl}`}
                </button>
              );
            })}
            <input
              className="nvr-input mono"
              style={{ width: 140, height: 24, fontSize: "var(--text-xs)" }}
              placeholder={isPt ? "Outra classe COCO..." : "Custom COCO label..."}
              value={customLabel}
              onChange={(e) => setCustomLabel(e.target.value)}
            />
            <button
              type="button"
              className="nvr-btn"
              style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
              onClick={() => {
                const clean = customLabel.trim().toLowerCase();
                if (!clean) return;
                if (!trackedLabels.includes(clean)) toggleTrackLabel(clean);
                setSelectedFilterLabel(clean);
                setCustomLabel("");
              }}
            >
              <Plus size={11} />
              {isPt ? "Adicionar" : "Add"}
            </button>
          </div>

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
              <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
                <span
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    fontWeight: 700,
                    color: "var(--signal-accent-text)",
                  }}
                >
                  {isPt
                    ? "ATRIBUTOS FILHOS EM PASSE ÚNICO (FACE, PLACA & LOGOS DE ENTREGA)"
                    : "ATTRIBUTE LABELS (SINGLE-PASS CHILD BOXES)"}
                </span>
                <span style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt
                    ? "Vincula face → person e license_plate/logos → veículos sem criar trilhas avulsas."
                    : "Binds face → person and license_plate/delivery logos → vehicles without spawning standalone tracks."}
                </span>
              </div>

              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <span
                  className="mono"
                  style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", whiteSpace: "nowrap" }}
                >
                  {isPt ? "Política de Snapshot:" : "Snapshot Selection Policy:"}
                </span>
                <select
                  className="nvr-select mono"
                  aria-label="Snapshot Selection Policy"
                  style={{ height: 24, width: 248, maxWidth: "100%", fontSize: "var(--text-xs)" }}
                  value={activeSnapshotSelection}
                  onChange={(e) =>
                    updateSnapshotSelection(
                      e.target.value as
                        | "best_overall"
                        | "best_in_zone"
                        | "first_trigger"
                        | "best_attribute"
                    )
                  }
                >
                  <option value="best_attribute">
                    {isPt
                      ? "Melhor Atributo (Face / Placa) (best_attribute)"
                      : "Best Attribute (Face / Plate) (best_attribute)"}
                  </option>
                  <option value="best_in_zone">
                    {isPt ? "Melhor na Zona (best_in_zone)" : "Best in Zone (best_in_zone)"}
                  </option>
                  <option value="best_overall">
                    {isPt ? "Melhor Geral (best_overall)" : "Best Overall (best_overall)"}
                  </option>
                  <option value="first_trigger">
                    {isPt ? "Primeiro Gatilho (first_trigger)" : "First Trigger (first_trigger)"}
                  </option>
                </select>
              </div>
            </div>

            <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
              {Array.from(
                new Set([...DEFAULT_ATTRIBUTE_LABELS, ...attributeLabels])
              ).map((attrLbl) => {
                const isActive = attributeLabels.includes(attrLbl);
                return (
                  <button
                    key={attrLbl}
                    type="button"
                    className={`nvr-btn mono ${isActive ? "active" : ""}`}
                    style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                    onClick={() => toggleAttributeLabel(attrLbl)}
                  >
                    {isActive ? `✓ ${attrLbl}` : `+ ${attrLbl}`}
                  </button>
                );
              })}
            </div>
          </div>

          <div className="nvr-subcard" style={{ padding: "8px 10px" }}>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: 8 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-accent-text)", display: "flex", alignItems: "center", gap: 5 }}>
                  <Sparkles size={12} />
                  {isPt ? "2. CALIBRAR CLASSE ATIVA:" : "2. TUNE ACTIVE CLASS:"}
                </span>
                {trackedLabels.map((lbl) => (
                  <button
                    key={lbl}
                    type="button"
                    className={`nvr-btn mono ${selectedFilterLabel === lbl ? "active" : ""}`}
                    style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                    onClick={() => setSelectedFilterLabel(lbl)}
                  >
                    {lbl}
                  </button>
                ))}
              </div>

              <div style={{ display: "flex", gap: 5, alignItems: "center", flexWrap: "wrap" }}>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "permissive" ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.4, threshold: 0.58 })
                  }
                >
                  {isPt ? "Permissivo (58%)" : "Permissive (58%)"}
                </button>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "balanced" ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.5, threshold: 0.7 })
                  }
                >
                  {isPt ? "Equilibrado (70%)" : "Balanced (70%)"}
                </button>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "strict" ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.65, threshold: 0.82 })
                  }
                >
                  {isPt ? "Rigoroso (82%)" : "Strict (82%)"}
                </button>
              </div>
            </div>

            <div style={{ display: "grid", gridTemplateColumns: "repeat(2, minmax(0, 1fr))", gap: 12 }}>
              <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
                <div className="nvr-field-meta">
                  <div className="nvr-field-label">
                    {isPt
                      ? `Confiança p/ Confirmar '${selectedFilterLabel}' (${confThresholdPct}%)`
                      : `Confirmed '${selectedFilterLabel}' Confidence (${confThresholdPct}%)`}
                  </div>
                  <div className="nvr-field-desc">
                    {isPt
                      ? "Pontuação necessária para confirmar evento real e alerta."
                      : "Score required to confirm object as a real event and alert."}
                  </div>
                </div>
                <input
                  type="range"
                  min={35}
                  max={95}
                  aria-label="Confirmed Object Confidence"
                  value={confThresholdPct}
                  onChange={(e) =>
                    updateObjectFilter({
                      threshold: Number((Number(e.target.value) / 100).toFixed(2)),
                    })
                  }
                />
              </div>

              <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
                <div className="nvr-field-meta">
                  <div className="nvr-field-label">
                    {isPt
                      ? `Pontuação Mínima por Frame (${confMinScorePct}%)`
                      : `Single-Frame Minimum Score (${confMinScorePct}%)`}
                  </div>
                  <div className="nvr-field-desc">
                    {isPt
                      ? "Detecções abaixo deste valor em um frame isolado são ignoradas."
                      : "Detections below this score on a single frame are ignored."}
                  </div>
                </div>
                <input
                  type="range"
                  min={20}
                  max={90}
                  aria-label="Single-Frame Minimum Score"
                  value={confMinScorePct}
                  onChange={(e) =>
                    updateObjectFilter({
                      min_score: Number((Number(e.target.value) / 100).toFixed(2)),
                    })
                  }
                />
              </div>
            </div>

            <button
              type="button"
              className="nvr-btn mono"
              style={{
                width: "100%",
                justifyContent: "space-between",
                height: 22,
                fontSize: "var(--text-xs)",
              }}
              onClick={() => setShowAdvancedFilterBox((v) => !v)}
            >
              <span>
                {isPt
                  ? `AVANÇADO: LIMITES DE ÁREA & PROPORÇÃO (ÁREA MÍN: ${activeFilter.min_area}px)`
                  : `ADVANCED BOUNDING-BOX SIZE & ASPECT RATIO LIMITS (MIN AREA: ${activeFilter.min_area}px)`}
              </span>
              {showAdvancedFilterBox ? <ChevronUp size={13} /> : <ChevronDown size={13} />}
            </button>

            {showAdvancedFilterBox && (
              <div
                style={{
                  display: "grid",
                  gridTemplateColumns: "repeat(4, minmax(0, 1fr))",
                  gap: 8,
                  paddingTop: 2,
                }}
              >
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Min Area (px)
                  </div>
                  <input
                    type="number"
                    min={0}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeFilter.min_area}
                    onChange={(e) =>
                      updateObjectFilter({ min_area: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Max Area (px)
                  </div>
                  <input
                    type="number"
                    min={100}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeFilter.max_area}
                    onChange={(e) =>
                      updateObjectFilter({ max_area: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Min Ratio (W/H)
                  </div>
                  <input
                    type="number"
                    step={0.1}
                    min={0}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeFilter.min_ratio}
                    onChange={(e) =>
                      updateObjectFilter({ min_ratio: Number(e.target.value) })
                    }
                  />
                </div>
                <div>
                  <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                    Max Ratio (W/H)
                  </div>
                  <input
                    type="number"
                    step={0.1}
                    min={0.1}
                    className="nvr-input mono"
                    style={{ width: "100%", marginTop: 2 }}
                    value={activeFilter.max_ratio}
                    onChange={(e) =>
                      updateObjectFilter({ max_ratio: Number(e.target.value) })
                    }
                  />
                </div>
              </div>
            )}
          </div>
        </div>
      </div>

      {/* Vision AI Judge (genai) Configuration Card */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "JUIZ DE IA VISUAL (GENAI) — AUDITORIA DE ALERTAS AMBÍGUOS & FALSOS POSITIVOS"
              : "VISION AI JUDGE (GENAI) — AMBIGUOUS ALERT VERIFICATION & AUTO-SUPPRESSION"}
          </span>
          <span
            className={`nvr-badge ${
              genaiCfg.enabled && Boolean(genaiCfg.model) ? "live" : ""
            }`}
          >
            <Sparkles size={11} />
            {(genaiCfg.provider || "gemini").toUpperCase()} ·{" "}
            {genaiCfg.model
              ? genaiCfg.model
              : isPt
              ? "MODELO NÃO SELECIONADO"
              : "NO MODEL SELECTED"}{" "}
            · {genaiCfg.enabled ? "ACTIVE" : "STANDBY"}
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Ativar Juiz de IA Visual (Gemini / OpenAI / Ollama Local)"
                  : "Enable Vision AI Judge (Gemini / OpenAI / Local Ollama)"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Fluxo guiado: 1. Escolha o provedor → 2. Configure a API → 3. Valide a API → 4. Selecione o Vision Model carregado."
                  : "Guided flow: 1. Choose provider → 2. Configure API → 3. Validate API → 4. Select available Vision Model from dropdown."}
              </div>
            </div>
            <ToggleSwitch
              checked={genaiCfg.enabled}
              label="Enable Vision AI Judge"
              onChange={(v) => updateGenAi({ enabled: v })}
            />
          </div>

          <div
            style={{
              display: "grid",
              gridTemplateColumns:
                "minmax(0, 1fr) minmax(0, 1.15fr) minmax(0, 0.9fr) minmax(0, 1.35fr)",
              gap: 8,
              alignItems: "end",
            }}
          >
            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt ? "1. Provedor (provider)" : "1. Provider"}
              </div>
              <select
                className="nvr-select mono"
                aria-label="Vision AI Judge Provider"
                style={{ width: "100%", marginTop: 2 }}
                value={genaiCfg.provider || "gemini"}
                onChange={(e) => {
                  updateGenAi(
                    buildGenAiProviderChangePatch(e.target.value, genaiCfg.base_url)
                  );
                  setGenaiModels([]);
                  setGenaiValidationState({ status: "idle" });
                }}
              >
                <option value="gemini">Google Gemini (gemini)</option>
                <option value="openai">OpenAI / Compatible (openai)</option>
                <option value="ollama">Local Ollama (ollama)</option>
              </select>
            </div>

            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt ? "2. Chave da API (api_key)" : "2. API Key"}
                {genaiCfg.has_api_key ? " (Saved)" : ""}
              </div>
              <div style={{ display: "flex", gap: 4, marginTop: 2 }}>
                <input
                  type="password"
                  className="nvr-input mono"
                  aria-label="Vision AI Judge API Key"
                  style={{ flex: 1, minWidth: 0, width: "100%" }}
                  placeholder={
                    genaiCfg.has_api_key
                      ? "•••••••• (saved key)"
                      : genaiCfg.provider === "ollama"
                      ? isPt
                        ? "Opcional (Ollama local)"
                        : "Optional (Local Ollama)"
                      : isPt
                      ? `Cole a chave ou use ${genaiCfg.api_key_env || "GEMINI_API_KEY"}`
                      : `Paste key or use ${genaiCfg.api_key_env || "GEMINI_API_KEY"}`
                  }
                  value={
                    genaiCfg.api_key === "***" ? "" : genaiCfg.api_key || ""
                  }
                  onChange={(e) => {
                    const val = e.target.value;
                    updateGenAi({
                      api_key: val || (genaiCfg.has_api_key ? "***" : null),
                      clear_api_key: false,
                      model: "",
                    });
                    setGenaiModels([]);
                    setGenaiValidationState({ status: "idle" });
                  }}
                />
                {(genaiCfg.has_api_key ||
                  (genaiCfg.api_key && genaiCfg.api_key !== "")) && (
                  <button
                    type="button"
                    className="nvr-btn danger mono"
                    style={{ padding: "0 7px", fontSize: "var(--text-xs)" }}
                    title="Clear saved Vision AI Judge API key"
                    aria-label="Clear Vision AI Judge API Key"
                    onClick={() => {
                      updateGenAi({
                        api_key: null,
                        has_api_key: false,
                        clear_api_key: true,
                        model: "",
                      });
                      setGenaiModels([]);
                      setGenaiValidationState({ status: "idle" });
                    }}
                  >
                    ×
                  </button>
                )}
              </div>
            </div>

            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt ? "Variável Env (api_key_env)" : "API Key Env Var"}
              </div>
              <input
                className="nvr-input mono"
                aria-label="Vision AI Judge API Key Env"
                style={{ width: "100%", marginTop: 2 }}
                placeholder={
                  genaiCfg.provider === "openai"
                    ? "OPENAI_API_KEY"
                    : genaiCfg.provider === "ollama"
                    ? "OLLAMA_API_KEY"
                    : "GEMINI_API_KEY"
                }
                value={genaiCfg.api_key_env}
                onChange={(e) => {
                  updateGenAi({ api_key_env: e.target.value });
                  setGenaiValidationState({ status: "idle" });
                }}
              />
            </div>

            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt
                  ? "Custom Endpoint Base URL (LLM Proxy)"
                  : "Custom Endpoint Base URL"}
              </div>
              <input
                className="nvr-input mono"
                aria-label="Vision AI Judge Base URL"
                style={{ width: "100%", marginTop: 2 }}
                placeholder={
                  genaiCfg.provider === "ollama"
                    ? "http://127.0.0.1:11434"
                    : isPt
                    ? "Opcional (Proxy LLM / OpenRouter / LiteLLM)..."
                    : "Optional endpoint override (LLM proxy)..."
                }
                value={genaiCfg.base_url || ""}
                onChange={(e) => {
                  updateGenAi({
                    base_url: e.target.value.trim() ? e.target.value.trim() : null,
                  });
                  setGenaiValidationState({ status: "idle" });
                }}
              />
            </div>
          </div>

          <div
            className="nvr-subcard"
            style={{
              padding: "8px 10px",
              display: "grid",
              gridTemplateColumns: "auto minmax(0, 1fr)",
              gap: 10,
              alignItems: "end",
            }}
          >
            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt ? "3. Validar API & Buscar Modelos" : "3. Validate API & Fetch Models"}
              </div>
              <button
                type="button"
                className={`nvr-btn mono ${
                  genaiValidationState.status === "valid" ? "success" : "primary"
                }`}
                aria-label="Validate Vision AI Judge API"
                style={{
                  marginTop: 2,
                  height: 28,
                  padding: "0 12px",
                  whiteSpace: "nowrap",
                  fontSize: "var(--text-xs)",
                }}
                disabled={genaiValidating}
                onClick={() => void handleValidateGenAiApi()}
              >
                <CheckCircle2 size={12} />
                {genaiValidating
                  ? isPt
                    ? "Validando API..."
                    : "Validating API..."
                  : genaiValidationState.status === "valid"
                  ? isPt
                    ? `API Validada (${genaiModels.length} modelos)`
                    : `API Validated (${genaiModels.length} models)`
                  : isPt
                  ? "Validar API & Carregar Modelos"
                  : "Validate API & Load Models"}
              </button>
            </div>

            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt
                  ? "4. Modelo Visual (selecione após validar)"
                  : "4. Vision Model (select after API validation)"}
              </div>
              <select
                className="nvr-select mono"
                aria-label="Vision AI Judge Model"
                style={{ width: "100%", marginTop: 2 }}
                disabled={genaiModelOptions.length === 0}
                value={
                  genaiModelOptions.includes(genaiCfg.model)
                    ? genaiCfg.model
                    : ""
                }
                onChange={(e) => updateGenAi({ model: e.target.value })}
              >
                <option value="">
                  {genaiModelOptions.length === 0
                    ? isPt
                      ? "— Valide a API para carregar os modelos disponíveis —"
                      : "— Validate API to load available models —"
                    : isPt
                    ? `— Escolha o Vision Model na lista (${genaiModelOptions.length} disponíveis) —`
                    : `— Select Vision Model from list (${genaiModelOptions.length} available) —`}
                </option>
                {genaiModelOptions.map((m) => (
                  <option key={m} value={m}>
                    {m}
                  </option>
                ))}
              </select>
            </div>
          </div>

          {genaiValidationState.status !== "idle" &&
            genaiValidationState.message && (
              <div
                className="mono"
                style={{
                  padding: "5px 8px",
                  borderRadius: 3,
                  fontSize: "var(--text-xs)",
                  background:
                    genaiValidationState.status === "valid"
                      ? "rgba(16, 185, 129, 0.12)"
                      : "rgba(239, 68, 68, 0.12)",
                  border:
                    genaiValidationState.status === "valid"
                      ? "1px solid rgba(16, 185, 129, 0.4)"
                      : "1px solid rgba(239, 68, 68, 0.4)",
                  color:
                    genaiValidationState.status === "valid"
                      ? "var(--signal-live-text)"
                      : "var(--signal-alert-text)",
                }}
              >
                {genaiValidationState.message}
              </div>
            )}

          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
              gap: 12,
              paddingTop: 4,
            }}
          >
            <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? "Auditar Alertas Ambíguos Automaticamente"
                    : "Auto-Verify Ambiguous Alerts"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? `Aciona o Juiz de IA quando a pontuação cair na faixa [${Math.round(
                        (genaiCfg.ambiguous_score_min ?? 0.45) * 100
                      )}%, ${Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}%].`
                    : `Triggers Vision AI Judge when score falls in [${Math.round(
                        (genaiCfg.ambiguous_score_min ?? 0.45) * 100
                      )}%, ${Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}%].`}
                </div>
              </div>
              <ToggleSwitch
                checked={genaiCfg.verify_ambiguous_alerts}
                label="Verify Ambiguous Alerts"
                onChange={(v) => updateGenAi({ verify_ambiguous_alerts: v })}
              />
            </div>

            <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? "Auto-Suprimir Falsos Positivos Rejeitados"
                    : "Auto-Suppress Rejected False Positives"}
                </div>
                <div className="nvr-field-desc">
                  {isPt
                    ? "Grava assinatura espacial + dhash64 no SQLite quando a IA rejeita o alerta."
                    : "Persists spatial IoU + dhash64 signature in SQLite when AI rejects a detection."}
                </div>
              </div>
              <ToggleSwitch
                checked={genaiCfg.auto_suppress_false_positives}
                label="Auto-Suppress False Positives"
                onChange={(v) => updateGenAi({ auto_suppress_false_positives: v })}
              />
            </div>
          </div>

          <div style={{ display: "grid", gridTemplateColumns: "repeat(2, minmax(0, 1fr))", gap: 12 }}>
            <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label mono">
                  Ambiguous Score Min ({Math.round((genaiCfg.ambiguous_score_min ?? 0.45) * 100)}%)
                </div>
              </div>
              <input
                type="range"
                min={25}
                max={80}
                aria-label="Ambiguous Score Minimum"
                value={Math.round((genaiCfg.ambiguous_score_min ?? 0.45) * 100)}
                onChange={(e) =>
                  updateGenAi({
                    ambiguous_score_min: Number(
                      (Number(e.target.value) / 100).toFixed(2)
                    ),
                  })
                }
              />
            </div>

            <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label mono">
                  Ambiguous Score Max ({Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}%)
                </div>
              </div>
              <input
                type="range"
                min={40}
                max={95}
                aria-label="Ambiguous Score Maximum"
                value={Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}
                onChange={(e) =>
                  updateGenAi({
                    ambiguous_score_max: Number(
                      (Number(e.target.value) / 100).toFixed(2)
                    ),
                  })
                }
              />
            </div>
          </div>
        </div>
      </div>

      {/* Active False-Positive Memory Signatures Table */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "MEMÓRIA DE FALSOS POSITIVOS ATIVA (ASSINATURAS ESPACIAIS + DHASH64)"
              : "ACTIVE FALSE-POSITIVE MEMORY SIGNATURES (SPATIAL IOU >= 0.72 + DHASH64 <= 12)"}
          </span>
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            <span className="mono">{fpSignatures.length} SIGNATURES</span>
            <button
              type="button"
              className="nvr-btn mono"
              style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
              onClick={() => void loadFpSignatures()}
            >
              {fpLoading ? "Refreshing..." : "Refresh"}
            </button>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 6 }}>
          {fpSignatures.length === 0 ? (
            <div
              className="mono"
              style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}
            >
              {isPt
                ? "Nenhuma assinatura de falso positivo aprendida ainda. Marque um evento como Falso Positivo na aba Explorar para suprimir recorrências instantaneamente."
                : "No learned false-positive signatures yet. Mark an event as False Positive in Explore or via the Vision AI Judge to suppress recurring static triggers at 0ms."}
            </div>
          ) : (
            <div style={{ overflowX: "auto" }}>
              <table
                className="mono"
                style={{
                  width: "100%",
                  borderCollapse: "collapse",
                  fontSize: "var(--text-xs)",
                  fontVariantNumeric: "tabular-nums",
                }}
              >
                <thead>
                  <tr
                    style={{
                      textAlign: "left",
                      color: "var(--text-secondary)",
                      borderBottom: "1px solid var(--border-hairline)",
                    }}
                  >
                    <th style={{ padding: "4px 6px" }}>CAMERA</th>
                    <th style={{ padding: "4px 6px" }}>LABEL</th>
                    <th style={{ padding: "4px 6px" }}>NORM BBOX</th>
                    <th style={{ padding: "4px 6px" }}>DHASH64</th>
                    <th style={{ padding: "4px 6px" }}>HITS</th>
                    <th style={{ padding: "4px 6px" }}>LAST SEEN</th>
                    <th style={{ padding: "4px 6px" }}>REASON</th>
                    <th style={{ padding: "4px 6px", textAlign: "right" }}>ACTION</th>
                  </tr>
                </thead>
                <tbody>
                  {fpSignatures.map((sig) => (
                    <tr
                      key={sig.id}
                      style={{ borderBottom: "1px solid var(--border-hairline)" }}
                    >
                      <td style={{ padding: "5px 6px", fontWeight: 700 }}>
                        {sig.camera.toUpperCase()}
                      </td>
                      <td style={{ padding: "5px 6px" }}>{sig.label.toUpperCase()}</td>
                      <td style={{ padding: "5px 6px" }}>
                        {formatSignatureBbox(sig.norm_bbox)}
                      </td>
                      <td style={{ padding: "5px 6px" }}>
                        {formatDhash64Hex(sig.dhash64)}
                      </td>
                      <td style={{ padding: "5px 6px" }}>{sig.hit_count}</td>
                      <td style={{ padding: "5px 6px" }}>
                        {formatDateTime(sig.last_seen_at)}
                      </td>
                      <td
                        style={{
                          padding: "5px 6px",
                          color: "var(--text-secondary)",
                          maxWidth: 260,
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                          whiteSpace: "nowrap",
                        }}
                      >
                        {sig.reason || sig.dominant_color || "operator_marked"}
                      </td>
                      <td style={{ padding: "5px 6px", textAlign: "right" }}>
                        <button
                          type="button"
                          className="nvr-btn danger mono"
                          style={{ height: 22, padding: "0 6px" }}
                          aria-label={`Delete false-positive signature ${sig.id}`}
                          onClick={() => void handleDeleteFpSignature(sig.id)}
                        >
                          <Trash2 size={11} />
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>

      {/* Face Recognition & Enrolled Gallery Card */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "RECONHECIMENTO FACIAL LOCAL & GALERIA CADASTRADA (ARCFACE / ADAFACE 512-D)"
              : "FACE RECOGNITION & ENROLLED GALLERY (ARCFACE / ADAFACE 512-D)"}
          </span>
          <span className="mono">
            {enrolledProfiles.length} {isPt ? "PERFIS CADASTRADOS" : "ENROLLED PROFILES"}
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(240px, 1fr))",
              gap: 10,
            }}
          >
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

            <div className="nvr-field-row" style={{ padding: "4px 0" }}>
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
            style={{
              display: "grid",
              gridTemplateColumns: "minmax(0, 1.2fr) minmax(0, 0.8fr) minmax(0, 1.4fr)",
              gap: 10,
              alignItems: "end",
            }}
          >
            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
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
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt ? "Área Mínima da Face (px²)" : "Min Face Area (px²)"}
              </div>
              <input
                type="number"
                min={100}
                max={50000}
                step={50}
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
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt
                  ? "Modelo de Embedding ONNX / OpenVINO"
                  : "Embedding Model Path (.onnx / .xml)"}
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                aria-label="Face Recognition Model Path"
                value={faceCfg.model_path || "/config/models/arcface_r50.onnx"}
                onChange={(e) =>
                  updateFaceRecognition({ model_path: e.target.value })
                }
              />
            </div>
          </div>

          {/* Alert Suppression for Known Residents (exclude_sub_labels) */}
          <div className="nvr-subcard" style={{ padding: "8px 10px", gap: 6 }}>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                flexWrap: "wrap",
                gap: 6,
              }}
            >
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-accent-text)",
                }}
              >
                {isPt
                  ? "REGRA DE ALERTA: MORADORES CONHECIDOS (EXCLUDE_SUB_LABELS → DOWNGRADE P/ DETECÇÃO)"
                  : "RESIDENT ALERT SUPPRESSION (EXCLUDE_SUB_LABELS → DOWNGRADE ALERT TO DETECTION)"}
              </span>
              <div style={{ display: "flex", gap: 4, alignItems: "center" }}>
                <input
                  className="nvr-input mono"
                  style={{ width: 150, height: 24, fontSize: "var(--text-xs)" }}
                  placeholder="Resident name (e.g. Bruno)..."
                  aria-label="Exclude sub-label resident name"
                  value={excludeSubLabelInput}
                  onChange={(e) => setExcludeSubLabelInput(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn mono"
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() => {
                    const clean = normalizeFaceProfileName(excludeSubLabelInput);
                    if (!clean) return;
                    handleToggleExcludeSubLabel(clean);
                    setExcludeSubLabelInput("");
                  }}
                >
                  + Suppress Alert
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
                    className={`nvr-btn mono ${isExcluded ? "active" : ""}`}
                    style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                    onClick={() => handleToggleExcludeSubLabel(personName)}
                    title="Toggle alert downgrade (alert -> detection) when this resident is recognized"
                  >
                    {isExcluded
                      ? `✓ ${personName} (Downgraded to Detection)`
                      : `${personName} (Triggers Alert)`}
                  </button>
                );
              })}
              {enrolledProfiles.length === 0 && activeExcludeSubLabels.length === 0 && (
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                  No resident sub_labels excluded yet. Enroll a profile below or add a name above.
                </span>
              )}
            </div>
          </div>

          {/* Enrolled Profiles Gallery & Upload */}
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
              <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700 }}>
                {isPt ? "CADASTRAR NOVO PERFIL FACIAL:" : "ENROLL NEW FACE PROFILE:"}
              </span>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <input
                  className="nvr-input mono"
                  style={{ width: 160, height: 26, fontSize: "var(--text-xs)" }}
                  placeholder="Profile name (e.g. Bruno)"
                  aria-label="Face profile name to enroll"
                  value={enrollNameInput}
                  onChange={(e) => setEnrollNameInput(e.target.value)}
                />
                <input
                  type="file"
                  accept="image/*"
                  aria-label="Upload face crop image"
                  style={{ fontSize: "var(--text-xs)", maxWidth: 210 }}
                  onChange={(e) => {
                    const file = e.target.files?.[0];
                    if (!file) {
                      setEnrollImageB64("");
                      return;
                    }
                    const reader = new FileReader();
                    reader.onload = () => {
                      if (typeof reader.result === "string") {
                        setEnrollImageB64(reader.result);
                      }
                    };
                    reader.readAsDataURL(file);
                  }}
                />
                <button
                  type="button"
                  className="nvr-btn primary mono"
                  style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                  disabled={enrollBusy}
                  onClick={handleEnrollProfile}
                >
                  <Plus size={11} />
                  {enrollBusy ? "Enrolling..." : "Enroll Face"}
                </button>
              </div>
            </div>

            <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
              {enrolledProfiles.map((prof) => (
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
                    className="nvr-btn danger mono"
                    style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
                    aria-label={`Delete face profile ${prof.name}`}
                    onClick={() => handleDeleteProfile(prof.name)}
                  >
                    <Trash2 size={11} />
                  </button>
                </div>
              ))}
              {enrolledProfiles.length === 0 && (
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                  No faces enrolled in /media/ninjanvr/faces yet.
                </span>
              )}
            </div>
          </div>
        </div>
      </div>

      {/* License Plate Recognition (LPR) & Known Plates Card */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "RECONHECIMENTO DE PLACAS (LPR / ANPR) & PLACAS CONHECIDAS"
              : "LICENSE PLATE RECOGNITION (LPR) & KNOWN PLATES"}
          </span>
          <span className="mono">
            {Object.keys(lprCfg.known_plates || {}).length}{" "}
            {isPt ? "PLACAS REGISTRADAS" : "KNOWN PLATES"}
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
              gap: 12,
              alignItems: "center",
            }}
          >
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
                onChange={(val) => updateLpr({ enabled: val })}
              />
            </div>

            <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
              <div className="nvr-field-meta">
                <div className="nvr-field-label">
                  {isPt
                    ? `Confiança Mínima de Consenso OCR (${Math.round((lprCfg.min_confidence ?? 0.75) * 100)}%)`
                    : `Minimum OCR Consensus Confidence (${Math.round((lprCfg.min_confidence ?? 0.75) * 100)}%)`}
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
            style={{
              display: "grid",
              gridTemplateColumns: "minmax(0, 2fr) minmax(0, 1fr) minmax(0, 1fr)",
              gap: 10,
              alignItems: "end",
            }}
          >
            <div>
              <div
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                  display: "flex",
                  justifyContent: "space-between",
                  gap: 6,
                }}
              >
                <span>
                  {isPt
                    ? "Expressão Regular de Validação (format_regex)"
                    : "Plate Format Validation Regex (format_regex)"}
                </span>
                <button
                  type="button"
                  className="nvr-btn mono"
                  style={{ height: 18, padding: "0 6px", fontSize: "var(--text-xs)" }}
                  onClick={() =>
                    updateLpr({ format_regex: "^[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$" })
                  }
                >
                  Mercosul + Legacy
                </button>
              </div>
              <input
                className="nvr-input mono"
                aria-label="LPR Format Regex"
                style={{ width: "100%", marginTop: 3, fontFamily: "var(--font-mono)" }}
                placeholder={DEFAULT_LPR_FORMAT_REGEX}
                value={lprCfg.format_regex ?? ""}
                onChange={(e) => updateLpr({ format_regex: e.target.value }, false)}
              />
            </div>

            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt ? "Modelo Detector de Placa (Opcional)" : "Plate Detector Model (Optional)"}
              </div>
              <input
                className="nvr-input mono"
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
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt ? "Modelo OCR CTC (Opcional)" : "Plate OCR CTC Model (Optional)"}
              </div>
              <input
                className="nvr-input mono"
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
              <span
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  fontWeight: 700,
                  color: "var(--signal-accent-text)",
                }}
              >
                {isPt
                  ? "DIRETÓRIO DE PLACAS CONHECIDAS (KNOWN_PLATES → SUB_LABEL)"
                  : "KNOWN PLATES REGISTRY (KNOWN_PLATES → SUB_LABEL MAPPING)"}
              </span>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <input
                  className="nvr-input mono"
                  aria-label="Known plate number"
                  style={{
                    width: 120,
                    height: 24,
                    fontSize: "var(--text-xs)",
                    fontFamily: "var(--font-mono)",
                  }}
                  placeholder="ABC1D23"
                  value={newKnownPlate}
                  onChange={(e) => setNewKnownPlate(e.target.value.toUpperCase())}
                />
                <input
                  className="nvr-input"
                  aria-label="Known plate vehicle label"
                  style={{ width: 170, height: 24, fontSize: "var(--text-xs)" }}
                  placeholder={isPt ? "Ex: SUV da Família" : "e.g. Family SUV"}
                  value={newKnownPlateLabel}
                  onChange={(e) => setNewKnownPlateLabel(e.target.value)}
                />
                <button
                  type="button"
                  className="nvr-btn primary mono"
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
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
                        ? `Placa conhecida registrada e salva: ${norm} → ${vehicleLabel}`
                        : `Saved known plate: ${norm} → ${vehicleLabel}`
                    );
                  }}
                >
                  <Plus size={11} />
                  {isPt ? "Registrar Placa" : "Add Known Plate"}
                </button>
              </div>
            </div>

            <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
              {Object.entries(lprCfg.known_plates || {}).map(([plateKey, vehicleLabel]) => (
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
                    className="nvr-btn danger mono"
                    style={{ height: 18, padding: "0 4px", fontSize: "var(--text-xs)" }}
                    aria-label={`Remove known plate ${plateKey}`}
                    onClick={() =>
                      updateLpr({
                        known_plates: removeKnownPlateEntry(lprCfg.known_plates, plateKey),
                      })
                    }
                  >
                    ×
                  </button>
                </span>
              ))}
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
                <span
                  className="mono"
                  style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                >
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
                          className="nvr-btn primary mono"
                          style={{
                            height: 18,
                            padding: "0 6px",
                            fontSize: "var(--text-xs)",
                          }}
                          onClick={() => {
                            const label =
                              newKnownPlateLabel.trim() ||
                              (dp === "BRA2E19"
                                ? "SUV Prata (BRA2E19)"
                                : `Veículo ${dp}`);
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
                                ? `Placa ${dp} cadastrada como '${label}'`
                                : `Registered ${dp} as '${label}'`
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

          {/* Test LPR Scenario (Simulate Vehicle Pass) */}
          <div className="nvr-subcard" style={{ padding: "8px 10px", gap: 8 }}>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                flexWrap: "wrap",
                gap: 8,
              }}
            >
              <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
                <span
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    fontWeight: 700,
                    color: "var(--signal-live-text)",
                  }}
                >
                  {isPt
                    ? "TESTAR CENÁRIO LPR (SIMULAR PASSAGEM DE VEÍCULO)"
                    : "TEST LPR SCENARIO (SIMULATE VEHICLE PASS)"}
                </span>
                <span style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt
                    ? "Gera um frame sintético Mercosul, executa votação multi-frame LprTrackVoter + lookup known_plates e grava o evento no Explore/Review."
                    : "Generates a synthetic Mercosul plate frame, runs LprTrackVoter multi-frame consensus + known_plates lookup, and persists the event."}
                </span>
              </div>

              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <select
                  className="nvr-select mono"
                  aria-label="Simulate LPR Camera"
                  style={{ height: 26, fontSize: "var(--text-xs)" }}
                  value={simLprCamera}
                  onChange={(e) => setSimLprCamera(e.target.value)}
                >
                  {(Object.keys(draftConfig.cameras || {}).length > 0
                    ? Object.keys(draftConfig.cameras || {})
                    : [inspectorCam || "escritorio"]
                  ).map((c) => (
                    <option key={c} value={c}>
                      {c.toUpperCase()}
                    </option>
                  ))}
                </select>
                <input
                  className="nvr-input mono"
                  aria-label="Simulate LPR Plate"
                  style={{
                    width: 110,
                    height: 26,
                    fontSize: "var(--text-xs)",
                    fontFamily: "var(--font-mono)",
                  }}
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
                    }`}
                    style={{ height: 24, padding: "0 7px", fontSize: "var(--text-xs)" }}
                    onClick={() => setSimLprPlate(presetPlate)}
                  >
                    {presetPlate}
                  </button>
                ))}
                <button
                  type="button"
                  className="nvr-btn success mono"
                  style={{ height: 26, padding: "0 10px", fontSize: "var(--text-xs)" }}
                  disabled={simLprBusy}
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
        </div>
      </div>

      {/* ROI State Classifier Card (Gate Open/Closed, Parking Spot Occupied, Anomaly Mode) */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "CLASSIFICADOR DE ESTADO DE ROI (PORTÃO ABERTO/FECHADO, VAGA OCUPADA & ANOMALIA)"
              : "ROI STATE CLASSIFIER (STATE_ROIS — GATE OPEN/CLOSED, PARKING & ANOMALY MODE)"}{" "}
            ({inspectorCam.toUpperCase()})
          </span>
          <span className="mono">
            {Object.keys(draftConfig.cameras?.[inspectorCam]?.state_rois || {}).length}{" "}
            {isPt ? "ROIS CONFIGURADOS" : "CONFIGURED ROIS"}
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            <div
              className="mono"
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                color: "var(--signal-motion-text)",
              }}
            >
              {isPt
                ? "PASSO 1 — O QUE VOCÊ QUER MONITORAR?"
                : "STEP 1 — WHAT DO YOU WANT TO MONITOR?"}
            </div>
            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(175px, 1fr))",
                gap: 6,
              }}
            >
              {STATE_ROI_SCENARIO_PRESETS.map((preset) => {
                const active = newRoiScenario === preset.id;
                const IconComp =
                  preset.id === "door_gate"
                    ? DoorOpen
                    : preset.id === "parking_spot"
                    ? Car
                    : preset.id === "package_area"
                    ? Package
                    : preset.id === "fixed_object"
                    ? Box
                    : Edit3;
                return (
                  <button
                    key={preset.id}
                    type="button"
                    className={`nvr-btn ${active ? "active" : ""}`}
                    style={{
                      height: "auto",
                      padding: "6px 8px",
                      flexDirection: "column",
                      alignItems: "flex-start",
                      textAlign: "left",
                      gap: 2,
                      minWidth: 0,
                      overflow: "hidden",
                      whiteSpace: "normal",
                    }}
                    onClick={() => {
                      setNewRoiScenario(preset.id);
                      if (preset.id !== "custom") {
                        setNewRoiName(preset.defaultName);
                        setNewRoiClasses(preset.classes);
                        setNewRoiAnomalyState(preset.anomalyState);
                        setNewRoiAnomalyTimeoutSec(preset.anomalyTimeoutSec);
                        setNewRoiIntervalSec(preset.intervalSec);
                        setNewRoiHysteresis(preset.hysteresis);
                      }
                    }}
                  >
                    <span
                      style={{
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 5,
                        fontWeight: 700,
                        fontSize: "var(--text-xs)",
                        maxWidth: "100%",
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      <IconComp size={12} style={{ flexShrink: 0 }} />
                      {isPt ? preset.titlePt : preset.titleEn}
                    </span>
                    <span
                      className="mono"
                      style={{
                        fontSize: "var(--text-xs)",
                        color: "var(--text-muted)",
                        maxWidth: "100%",
                        overflow: "hidden",
                        textOverflow: "ellipsis",
                        whiteSpace: "nowrap",
                      }}
                    >
                      {isPt ? preset.descPt : preset.descEn}
                    </span>
                  </button>
                );
              })}
            </div>
          </div>

          <div
            style={{
              display: "grid",
              gridTemplateColumns: "minmax(0, 1.2fr) minmax(0, 1.8fr) auto",
              gap: 10,
              alignItems: "end",
            }}
          >
            <div>
              <div
                className="mono"
                style={{
                  display: "flex",
                  justifyContent: "space-between",
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                }}
              >
                <span>
                  {isPt ? "PASSO 2 — NOME DO LOCAL" : "STEP 2 — LOCATION NAME"}
                </span>
                <span>
                  ID: <strong>{slugifyRoiLocationName(newRoiName) || "gate"}</strong>
                </span>
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                aria-label="ROI Classifier Name"
                placeholder={
                  isPt
                    ? "Ex: Porta do Escritório, Portão da Garagem"
                    : "e.g. Porta do Escritório, Portão da Garagem"
                }
                value={newRoiName}
                onChange={(e) => setNewRoiName(e.target.value)}
              />
            </div>

            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              <div
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                }}
              >
                {isPt ? "REGRA DE ALERTA:" : "ALERT RULE:"}
              </div>
              <div style={{ display: "flex", gap: 4, flexWrap: "wrap", alignItems: "center" }}>
                <button
                  type="button"
                  className={`nvr-btn mono ${!newRoiAnomalyState.trim() ? "active" : ""}`}
                  style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                  onClick={() => setNewRoiAnomalyState("")}
                >
                  {isPt
                    ? "Apenas Crachá (Sem Alerta)"
                    : "Status Badge Only (No Alert)"}
                </button>
                {parseRoiClassesInput(newRoiClasses).map((st) => {
                  const isSel =
                    newRoiAnomalyState.trim().toLowerCase() === st.toLowerCase();
                  return (
                    <button
                      key={st}
                      type="button"
                      className={`nvr-btn mono ${isSel ? "active" : ""}`}
                      style={{ height: 24, padding: "0 8px", fontSize: "var(--text-xs)" }}
                      onClick={() => setNewRoiAnomalyState(st)}
                    >
                      {isPt ? `Alertar quando '${st}'` : `Alert when ${st}`}
                    </button>
                  );
                })}
                {newRoiAnomalyState.trim() &&
                  ROI_ALERT_DURATION_PILLS.map((pill) => (
                    <button
                      key={pill.sec}
                      type="button"
                      className={`nvr-btn mono ${
                        newRoiAnomalyTimeoutSec === pill.sec ? "active" : ""
                      }`}
                      style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                      onClick={() => setNewRoiAnomalyTimeoutSec(pill.sec)}
                    >
                      {pill.label}
                    </button>
                  ))}
              </div>
            </div>

            <button
              type="button"
              className="nvr-btn success mono"
              style={{ height: 28, padding: "0 12px", fontSize: "var(--text-xs)" }}
              onClick={() => {
                const slug = slugifyRoiLocationName(newRoiName);
                if (!slug || slug.length < 2) {
                  notify("Enter a valid ROI ID with at least 2 characters");
                  return;
                }
                const roiCfg = buildStateRoiConfig({
                  cropInput: newRoiCrop,
                  classesInput: newRoiClasses,
                  intervalSec: newRoiIntervalSec,
                  hysteresis: newRoiHysteresis,
                  anomalyState: newRoiAnomalyState,
                  anomalyTimeoutSec: newRoiAnomalyTimeoutSec,
                });
                updateDraft((next) => {
                  const cam = next.cameras[inspectorCam];
                  if (!cam) return;
                  if (!cam.state_rois) cam.state_rois = {};
                  cam.state_rois[slug] = roiCfg;
                });
                notify(`Added ROI state classifier '${slug}' on ${inspectorCam}`);
              }}
            >
              {isPt ? "+ Salvar Monitoramento de Área" : "+ Save State Monitor"}
            </button>
          </div>

          {newRoiScenario === "custom" && (
            <div>
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt
                  ? "Estados Customizados (separados por vírgula)"
                  : "Custom States (comma-separated)"}
              </div>
              <input
                className="nvr-input mono"
                style={{ width: "100%", marginTop: 2 }}
                placeholder="closed, open"
                value={newRoiClasses}
                onChange={(e) => setNewRoiClasses(e.target.value)}
              />
            </div>
          )}

          <details
            style={{
              background: "var(--bg-input)",
              border: "1px solid var(--border-divider)",
              borderRadius: 2,
              padding: "5px 8px",
            }}
          >
            <summary
              className="mono"
              style={{
                cursor: "pointer",
                fontSize: "var(--text-xs)",
                color: "var(--text-secondary)",
              }}
            >
              Advanced Technical Tuning / Coordenadas &amp; Histerese
            </summary>
            <div
              style={{
                display: "grid",
                gridTemplateColumns:
                  "minmax(0, 1.4fr) minmax(0, 1.2fr) minmax(0, 0.7fr) minmax(0, 0.7fr) minmax(0, 0.9fr) minmax(0, 0.9fr)",
                gap: 6,
                alignItems: "end",
                marginTop: 6,
              }}
            >
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", whiteSpace: "nowrap" }}>
                  Normalized Crop [x1,y1,x2,y2]
                </div>
                <input
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  aria-label="ROI Normalized Crop"
                  placeholder="0.200,0.300,0.650,0.850"
                  value={newRoiCrop}
                  onChange={(e) => setNewRoiCrop(e.target.value)}
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", whiteSpace: "nowrap" }}>
                  States (Classes)
                </div>
                <input
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  aria-label="ROI State Classes"
                  placeholder="closed, open"
                  value={newRoiClasses}
                  onChange={(e) => setNewRoiClasses(e.target.value)}
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", whiteSpace: "nowrap" }}>
                  Interval (s)
                </div>
                <input
                  type="number"
                  min={1}
                  max={300}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  aria-label="ROI Evaluation Interval"
                  value={newRoiIntervalSec}
                  onChange={(e) => setNewRoiIntervalSec(Math.max(1, Number(e.target.value)))}
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", whiteSpace: "nowrap" }}>
                  Hysteresis
                </div>
                <input
                  type="number"
                  min={1}
                  max={30}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  aria-label="ROI Hysteresis Frames"
                  value={newRoiHysteresis}
                  onChange={(e) => setNewRoiHysteresis(Math.max(1, Number(e.target.value)))}
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", whiteSpace: "nowrap" }}>
                  Anomaly State
                </div>
                <input
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  aria-label="ROI Anomaly State"
                  placeholder="open"
                  value={newRoiAnomalyState}
                  onChange={(e) => setNewRoiAnomalyState(e.target.value)}
                />
              </div>
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", whiteSpace: "nowrap" }}>
                  Timeout (s)
                </div>
                <input
                  type="number"
                  min={5}
                  max={86400}
                  className="nvr-input mono"
                  style={{ width: "100%", marginTop: 2 }}
                  aria-label="ROI Anomaly Timeout"
                  value={newRoiAnomalyTimeoutSec}
                  onChange={(e) =>
                    setNewRoiAnomalyTimeoutSec(Math.max(1, Number(e.target.value)))
                  }
                />
              </div>
            </div>
          </details>

          {Object.entries(draftConfig.cameras?.[inspectorCam]?.state_rois || {}).length > 0 && (
            <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
              {Object.entries(draftConfig.cameras?.[inspectorCam]?.state_rois || {}).map(
                ([rName, rCfg]) => (
                  <div
                    key={rName}
                    className="nvr-subcard"
                    style={{
                      padding: "6px 8px",
                      flexDirection: "row",
                      alignItems: "center",
                      justifyContent: "space-between",
                      gap: 8,
                    }}
                  >
                    <div className="mono" style={{ fontSize: "var(--text-xs)" }}>
                      <strong style={{ color: "var(--signal-motion-text)" }}>
                        {rName.toUpperCase()}
                      </strong>{" "}
                      · crop=[{(rCfg.crop || [0.25, 0.25, 0.75, 0.75]).join(", ")}] · states=[
                      {(rCfg.classes || ["closed", "open"]).join(", ")}] · interval=
                      {rCfg.interval_sec ?? 5}s · hysteresis={rCfg.hysteresis ?? 3}
                      {rCfg.anomaly_state
                        ? ` · anomaly=${rCfg.anomaly_state} (${
                            rCfg.anomaly_timeout_sec ?? 300
                          }s)`
                        : ""}
                    </div>
                    <button
                      type="button"
                      className="nvr-btn danger"
                      style={{ height: 22, padding: "0 6px" }}
                      title={`Remove ROI ${rName}`}
                      onClick={() =>
                        updateDraft((next) => {
                          if (next.cameras[inspectorCam]?.state_rois) {
                            delete next.cameras[inspectorCam].state_rois![rName];
                          }
                        })
                      }
                    >
                      <Trash2 size={11} />
                    </button>
                  </div>
                )
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};

export const DatasetStudioSection: React.FC<{
  draftConfig?: NinjaConfigResponse | null;
  updateDraft?: (updater: (next: NinjaConfigResponse) => void) => void;
  notify: (msg: string) => void;
}> = ({ draftConfig, updateDraft, notify }) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const [summary, setSummary] = useState<DatasetSummaryResponse | null>(null);
  const [samples, setSamples] = useState<DatasetSampleItem[]>([]);
  const [selectedSampleId, setSelectedSampleId] = useState<string | null>(null);
  const [filterKind, setFilterKind] = useState<string>("all");
  const [activeClassId, setActiveClassId] = useState<number>(0);
  const [drawStart, setDrawStart] = useState<[number, number] | null>(null);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [uploadFilename, setUploadFilename] = useState("ninjanvr_yolo11s.onnx");
  const [uploadBase64, setUploadBase64] = useState("");
  const [uploadWidth, setUploadWidth] = useState(640);
  const [uploadHeight, setUploadHeight] = useState(640);

  const modelCfg = draftConfig?.model || {
    path: "/openvino-model/ssdlite_mobilenet_v2.xml",
    labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
    width: 300,
    height: 300,
    input_tensor: "nhwc",
    input_pixel_format: "bgr",
    model_type: "ssd",
  };

  const datasetLoadSeqRef = React.useRef(0);
  const isMountedRef = React.useRef(true);

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    setDrawStart(null);
  }, [selectedSampleId, filterKind]);

  const loadDatasetState = async () => {
    const seq = ++datasetLoadSeqRef.current;
    try {
      const [sumRes, listRes] = await Promise.all([
        fetch("/api/dataset/summary", { credentials: "include" }),
        fetch(
          filterKind === "all"
            ? "/api/dataset/samples"
            : `/api/dataset/samples?sample_kind=${encodeURIComponent(filterKind)}`,
          { credentials: "include" }
        ),
      ]);
      if (!isMountedRef.current || seq !== datasetLoadSeqRef.current) return;
      if (sumRes.ok) {
        const sumData = await sumRes.json().catch(() => null);
        if (isMountedRef.current && seq === datasetLoadSeqRef.current && sumData) {
          setSummary(sumData);
        }
      }
      if (listRes.ok) {
        const listData = await listRes.json().catch(() => null);
        if (!isMountedRef.current || seq !== datasetLoadSeqRef.current) return;
        const rows: DatasetSampleItem[] = Array.isArray(listData)
          ? listData
          : Array.isArray(listData?.items)
          ? listData.items
          : [];
        setSamples(rows);
        if (rows.length > 0 && !rows.some((r) => r.id === selectedSampleId)) {
          setSelectedSampleId(rows[0].id);
        }
      }
    } catch {
      // ignore
    }
  };

  useEffect(() => {
    void loadDatasetState();
  }, [filterKind]);

  const selectedSample =
    samples.find((s) => s.id === selectedSampleId) || samples[0] || null;
  const readiness = computeDatasetReadinessMetrics(summary);

  const handleUpdateSample = async (
    sample: DatasetSampleItem,
    patch: {
      annotations?: DatasetAnnotationBox[];
      verified?: boolean;
      split?: string;
    }
  ) => {
    try {
      const res = await fetch(
        `/api/dataset/samples/${encodeURIComponent(sample.id)}`,
        {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify(patch),
        }
      );
      if (!isMountedRef.current) return;
      if (res.ok) {
        const updated = await res.json().catch(() => null);
        if (isMountedRef.current && updated) {
          const item: DatasetSampleItem = updated.sample || updated;
          setSamples((prev) => prev.map((s) => (s.id === item.id ? item : s)));
          void loadDatasetState();
        }
      } else {
        const err = await res.json().catch(() => null);
        notify(err?.message || "Failed to update dataset sample");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error while updating dataset sample");
      }
    }
  };

  const handleAutoAnnotateSample = async (sampleId: string) => {
    setBusyAction(`auto-${sampleId}`);
    try {
      const res = await fetch(
        `/api/dataset/samples/${encodeURIComponent(sampleId)}/auto-annotate`,
        {
          method: "POST",
          credentials: "include",
        }
      );
      const data = await res.json().catch(() => null);
      if (res.ok && data) {
        const item: DatasetSampleItem = data.sample || data;
        setSamples((prev) => prev.map((s) => (s.id === item.id ? item : s)));
        notify(
          `AI Teacher generated ${(item.annotations || []).length} YOLO boxes on ${sampleId}`
        );
        void loadDatasetState();
      } else {
        notify(data?.message || "Vision AI auto-annotation failed");
      }
    } catch {
      notify("Network error during Vision AI auto-annotation");
    } finally {
      setBusyAction(null);
    }
  };

  const handleBatchAutoAnnotate = async () => {
    setBusyAction("batch-auto");
    try {
      const res = await fetch("/api/dataset/auto-annotate", {
        method: "POST",
        credentials: "include",
      });
      const data = await res.json().catch(() => null);
      if (res.ok && data) {
        notify(
          `Batch AI Teacher auto-annotated ${data.annotated_count ?? 0} unverified samples`
        );
        void loadDatasetState();
      } else {
        notify(data?.message || "Batch AI auto-annotation failed");
      }
    } catch {
      notify("Network error during batch AI auto-annotation");
    } finally {
      setBusyAction(null);
    }
  };

  const handleDeleteSample = async (sampleId: string) => {
    try {
      const res = await fetch(
        `/api/dataset/samples/${encodeURIComponent(sampleId)}`,
        {
          method: "DELETE",
          credentials: "include",
        }
      );
      if (!isMountedRef.current) return;
      if (res.ok) {
        setSamples((prev) => prev.filter((s) => s.id !== sampleId));
        notify(`Deleted dataset sample ${sampleId}`);
        void loadDatasetState();
      } else {
        const err = await res.json().catch(() => null);
        notify(err?.message || `Failed to delete dataset sample ${sampleId}`);
      }
    } catch {
      if (isMountedRef.current) {
        notify(`Network error while deleting dataset sample ${sampleId}`);
      }
    }
  };

  const handleUploadModel = async () => {
    setBusyAction("upload-model");
    try {
      const payload = buildModelUploadPayload({
        filename: uploadFilename,
        modelBase64: uploadBase64 || btoa("ONNX_CUSTOM_MODEL"),
        width: uploadWidth,
        height: uploadHeight,
      });
      const res = await fetch("/api/detectors/models/upload", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(payload),
      });
      const data = await res.json().catch(() => null);
      if (res.ok && data?.success) {
        notify(
          `Uploaded '${data.filename}' (${data.width}×${data.height}) and hot-reloaded AiDetectorEngine`
        );
      } else {
        notify(data?.message || "Model upload failed");
      }
    } catch {
      notify("Network error uploading custom detector model");
    } finally {
      setBusyAction(null);
    }
  };

  const handleSvgClick = (e: React.MouseEvent<SVGSVGElement>) => {
    if (!selectedSample) return;
    const rect = e.currentTarget.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return;
    const nx = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
    const ny = Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height));
    if (!drawStart) {
      setDrawStart([nx, ny]);
      return;
    }
    const [x1, y1] = drawStart;
    setDrawStart(null);
    const yolo = xyxyToYoloNormalized([x1, y1, nx, ny]);
    const label = NINJANVR_SECURITY_CLASSES[activeClassId] || "person";
    const nextBox: DatasetAnnotationBox = {
      class_id: activeClassId,
      label,
      cx: yolo.cx,
      cy: yolo.cy,
      w: yolo.w,
      h: yolo.h,
      score: 1.0,
      source: "manual",
    };
    void handleUpdateSample(selectedSample, {
      annotations: [...(selectedSample.annotations || []), nextBox],
    });
  };

  const quickClassIndices = [0, 1, 2, 3, 7, 8, 11] as const;

  return (
    <div className="nvr-card-grid" style={{ gap: 12 }}>
      {/* Step 1: Dataset Curation, Readiness Gauges & Interactive YOLO Annotation Stage */}
      <div className="nvr-cfg-card" style={{ gridColumn: "1 / -1" }}>
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "ESTÚDIO DE DATASET & APRENDIZADO ATIVO (DESTILAÇÃO PROFESSOR-ALUNO YOLO)"
              : "ACTIVE LEARNING DATASET STUDIO & TEACHER-STUDENT YOLO DISTILLATION"}
          </span>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span className={`nvr-badge ${readiness.isBalanced ? "live" : ""}`}>
              {readiness.totalSamples} SAMPLES · {readiness.verifiedPct}% VERIFIED
            </span>
            <button
              type="button"
              className="nvr-btn mono"
              style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
              onClick={() => void loadDatasetState()}
            >
              {isPt ? "Atualizar" : "Refresh"}
            </button>
            <button
              type="button"
              className="nvr-btn mono"
              style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
              disabled={busyAction === "batch-auto"}
              onClick={() => void handleBatchAutoAnnotate()}
            >
              <Sparkles size={11} />
              {busyAction === "batch-auto"
                ? "Auto-Annotating..."
                : "Batch Auto-Annotate Unverified"}
            </button>
            <a
              href="/api/dataset/export.zip"
              download="ninjanvr-yolo-dataset.zip"
              className="nvr-btn primary mono"
              style={{
                height: 22,
                padding: "0 8px",
                fontSize: "var(--text-xs)",
              }}
            >
              Export YOLO Dataset (.zip)
            </a>
          </div>
        </div>

        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
          {/* 4 Readiness Gauges */}
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(4, minmax(0, 1fr))",
              gap: 8,
            }}
          >
            <div
              className="nvr-subcard mono"
              style={{
                padding: "8px 10px",
                gap: 4,
                fontSize: "var(--text-xs)",
              }}
            >
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: 6 }}>
                <span style={{ color: "var(--text-secondary)", fontWeight: 700 }}>TOTAL SAMPLES</span>
                <span className={`nvr-badge ${readiness.verifiedPct >= 80 ? "live" : ""}`}>
                  {readiness.verifiedPct}% VERIFIED
                </span>
              </div>
              <div style={{ fontSize: "var(--text-md)", fontWeight: 700 }}>
                {readiness.totalSamples} FRAMES · {summary?.total_annotations ?? 0} BOXES
              </div>
              <div className="nvr-util-bar" style={{ width: "100%" }}>
                <div
                  className="nvr-util-fill"
                  style={{
                    width: `${Math.min(100, readiness.verifiedPct)}%`,
                    background: "var(--signal-live)",
                  }}
                />
              </div>
              <div style={{ color: "var(--text-muted)" }}>
                Verified: {readiness.verifiedCount} ({readiness.verifiedPct}%)
              </div>
            </div>

            <div
              className="nvr-subcard mono"
              style={{
                padding: "8px 10px",
                gap: 4,
                fontSize: "var(--text-xs)",
              }}
            >
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: 6 }}>
                <span style={{ color: "var(--text-secondary)", fontWeight: 700 }}>
                  TP vs HARD NEGATIVES
                </span>
                <span className={`nvr-badge ${readiness.isBalanced ? "live" : "motion"}`}>
                  {readiness.isBalanced ? "OPTIMAL 80/20" : `${readiness.balanceScorePct}% SCORE`}
                </span>
              </div>
              <div style={{ fontSize: "var(--text-md)", fontWeight: 700 }}>
                {readiness.positivePct}% TP ({readiness.positiveCount}) /{" "}
                {readiness.hardNegativePct}% BG ({readiness.hardNegativeCount})
              </div>
              <div className="nvr-util-bar" style={{ width: "100%" }}>
                <div
                  className="nvr-util-fill"
                  style={{
                    width: `${Math.min(100, readiness.positivePct)}%`,
                    background: "var(--signal-accent)",
                  }}
                />
              </div>
              <div style={{ color: "var(--text-muted)" }}>
                Balance Score: {readiness.balanceScorePct}%{" "}
                {readiness.isBalanced ? "· OPTIMAL" : "· TARGET 80% / 20%"}
              </div>
            </div>

            <div
              className="nvr-subcard mono"
              style={{
                padding: "8px 10px",
                gap: 4,
                fontSize: "var(--text-xs)",
              }}
            >
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: 6 }}>
                <span style={{ color: "var(--text-secondary)", fontWeight: 700 }}>
                  STRATIFIED SPLIT (80/20)
                </span>
                <span className="nvr-badge">
                  NIGHT IR: {summary?.night_mode_count ?? 0}
                </span>
              </div>
              <div style={{ fontSize: "var(--text-md)", fontWeight: 700 }}>
                TRAIN: {readiness.trainCount} ({readiness.trainPct}%) · VAL:{" "}
                {readiness.valCount} ({readiness.valPct}%)
              </div>
              <div className="nvr-util-bar" style={{ width: "100%" }}>
                <div
                  className="nvr-util-fill"
                  style={{
                    width: `${Math.min(100, readiness.trainPct)}%`,
                    background: "var(--signal-live)",
                  }}
                />
              </div>
              <div style={{ color: "var(--text-muted)" }}>
                Night IR Frames: {summary?.night_mode_count ?? 0} · Deterministic hash
              </div>
            </div>

            <div
              className="nvr-subcard mono"
              style={{
                padding: "8px 10px",
                gap: 4,
                fontSize: "var(--text-xs)",
                justifyContent: "space-between",
              }}
            >
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: 6 }}>
                <span style={{ color: "var(--text-secondary)", fontWeight: 700 }}>
                  PER-CAMERA DISTRIBUTION
                </span>
                <span className="nvr-badge">
                  {Object.keys(summary?.by_camera || {}).length} CHANNELS
                </span>
              </div>
              <div
                style={{
                  display: "flex",
                  gap: 4,
                  flexWrap: "wrap",
                  alignItems: "center",
                }}
              >
                {Object.entries(summary?.by_camera || {}).map(([cam, cnt]) => (
                  <span key={cam} className="nvr-badge mono">
                    {cam.toUpperCase()}: {cnt}
                  </span>
                ))}
                {Object.keys(summary?.by_camera || {}).length === 0 && (
                  <span style={{ fontSize: "var(--text-md)", fontWeight: 700, color: "var(--text-secondary)" }}>
                    0 ACTIVE SAMPLES
                  </span>
                )}
              </div>
              <div style={{ color: "var(--text-muted)" }}>
                {Object.keys(summary?.by_camera || {}).length === 0
                  ? "No camera samples yet"
                  : "Balanced multi-camera dataset coverage"}
              </div>
            </div>
          </div>

          {/* Sample Queue + Interactive SVG Bounding-Box Overlay Editor */}
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "320px minmax(0, 1fr)",
              gap: 10,
              minHeight: 290,
            }}
          >
            <div
              className="nvr-subcard"
              style={{
                padding: "8px 10px",
                gap: 8,
              }}
            >
              <div className="nvr-subcard-header">
                <span>
                  {isPt ? "FILA DE AMOSTRAS" : "SAMPLE QUEUE"} ({samples.length})
                </span>
                <span style={{ color: "var(--signal-accent-text)" }}>
                  {readiness.verifiedCount} VERIFIED
                </span>
              </div>

              <select
                className="nvr-select mono"
                aria-label="Filter dataset samples by kind"
                style={{ width: "100%", height: 26, fontSize: "var(--text-xs)" }}
                value={filterKind}
                onChange={(e) => setFilterKind(e.target.value)}
              >
                <option value="all">ALL SAMPLES ({samples.length})</option>
                <option value="true_positive">TRUE POSITIVES</option>
                <option value="ai_annotated">AI ANNOTATED</option>
                <option value="hard_negative">HARD NEGATIVES (BG)</option>
              </select>

              <div
                className="nvr-scroll-y"
                style={{ display: "flex", flexDirection: "column", gap: 4, flex: 1, maxHeight: 280 }}
              >
                {samples.map((s) => {
                  const isSel = selectedSample?.id === s.id;
                  return (
                    <div
                      key={s.id}
                      role="button"
                      tabIndex={0}
                      onClick={() => setSelectedSampleId(s.id)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          setSelectedSampleId(s.id);
                        }
                      }}
                      className="mono"
                      style={{
                        padding: 6,
                        background: isSel
                          ? "rgba(14, 165, 233, 0.14)"
                          : "var(--bg-panel)",
                        border: `1px solid ${
                          isSel ? "var(--signal-accent)" : "var(--border-hairline)"
                        }`,
                        borderRadius: 3,
                        cursor: "pointer",
                        display: "flex",
                        alignItems: "center",
                        justifyContent: "space-between",
                        gap: 6,
                        fontSize: "var(--text-xs)",
                      }}
                    >
                      <div style={{ minWidth: 0 }}>
                        <div style={{ fontWeight: 700 }}>
                          {s.camera.toUpperCase()} · {s.split.toUpperCase()}
                        </div>
                        <div style={{ color: "var(--text-muted)" }}>
                          {s.sample_kind.toUpperCase()} · {(s.annotations || []).length} boxes
                        </div>
                      </div>
                      <span className={`nvr-badge ${s.verified ? "live" : ""}`}>
                        {s.verified ? "VERIFIED" : "DRAFT"}
                      </span>
                    </div>
                  );
                })}
                {samples.length === 0 && (
                  <div
                    className="mono"
                    style={{
                      flex: 1,
                      padding: 10,
                      background: "var(--bg-input)",
                      border: "1px solid var(--border-divider)",
                      borderRadius: 3,
                      fontSize: "var(--text-xs)",
                      color: "var(--text-secondary)",
                      display: "flex",
                      flexDirection: "column",
                      justifyContent: "center",
                      gap: 6,
                    }}
                  >
                    <div style={{ fontWeight: 700, color: "var(--signal-accent-text)" }}>
                      DATASET QUEUE EMPTY
                    </div>
                    <div style={{ color: "var(--text-muted)", lineHeight: 1.45 }}>
                      Mark events as False Positive or click &quot;Add to Training Dataset&quot; in
                      Explore to populate frames.
                    </div>
                  </div>
                )}
              </div>
            </div>

            {/* Interactive SVG Bounding-Box Stage */}
            <div
              className="nvr-subcard"
              style={{
                padding: "8px 10px",
                gap: 8,
              }}
            >
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: 8,
                  flexWrap: "wrap",
                }}
              >
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                  <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-accent-text)" }}>
                    CLASS (0..14):
                  </span>
                  <select
                    className="nvr-select mono"
                    aria-label="YOLO Security Class Selector"
                    style={{ height: 24, width: 165, fontSize: "var(--text-xs)" }}
                    value={activeClassId}
                    onChange={(e) => setActiveClassId(Number(e.target.value))}
                  >
                    {NINJANVR_SECURITY_CLASSES.map((cls, idx) => (
                      <option key={cls} value={idx}>
                        {idx}: {cls}
                      </option>
                    ))}
                  </select>
                  {quickClassIndices.map((idx) => {
                    const cls = NINJANVR_SECURITY_CLASSES[idx];
                    const isAct = activeClassId === idx;
                    return (
                      <button
                        key={cls}
                        type="button"
                        className={`nvr-btn mono ${isAct ? "active" : ""}`}
                        style={{ height: 22, padding: "0 7px", fontSize: "var(--text-xs)" }}
                        onClick={() => setActiveClassId(idx)}
                      >
                        {idx}:{cls}
                      </button>
                    );
                  })}
                </div>

                {selectedSample ? (
                  <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                    <button
                      type="button"
                      className={`nvr-btn mono ${selectedSample.verified ? "active" : ""}`}
                      style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                      onClick={() =>
                        void handleUpdateSample(selectedSample, {
                          verified: !selectedSample.verified,
                        })
                      }
                    >
                      {selectedSample.verified ? "Verified ✓" : "Verify"}
                    </button>
                    <button
                      type="button"
                      className="nvr-btn mono"
                      style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                      disabled={busyAction === `auto-${selectedSample.id}`}
                      onClick={() => void handleAutoAnnotateSample(selectedSample.id)}
                    >
                      <Sparkles size={11} />
                      Auto-Annotate with AI
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger mono"
                      style={{ height: 22, padding: "0 6px", fontSize: "var(--text-xs)" }}
                      aria-label="Delete dataset sample"
                      onClick={() => void handleDeleteSample(selectedSample.id)}
                    >
                      <Trash2 size={11} />
                    </button>
                  </div>
                ) : (
                  <span className="nvr-badge mono">
                    CLICK 2 CORNERS TO DRAW NORMALIZED YOLO BOX
                  </span>
                )}
              </div>

              <div
                style={{
                  position: "relative",
                  flex: 1,
                  minHeight: 240,
                  background: "var(--video-stage-bg)",
                  border: "1px solid var(--border-hairline)",
                  borderRadius: 3,
                  overflow: "hidden",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                }}
              >
                {selectedSample ? (
                  <>
                    <img
                      src={`/api/dataset/samples/${encodeURIComponent(
                        selectedSample.id
                      )}/image.webp`}
                      alt={selectedSample.id}
                      style={{
                        width: "100%",
                        height: "100%",
                        objectFit: "contain",
                        display: "block",
                      }}
                    />
                    <svg
                      viewBox="0 0 1000 1000"
                      preserveAspectRatio="none"
                      onClick={handleSvgClick}
                      style={{
                        position: "absolute",
                        inset: 0,
                        width: "100%",
                        height: "100%",
                        cursor: "crosshair",
                      }}
                    >
                      {(selectedSample.annotations || []).map((ann, idx) => {
                        const [x1, y1, x2, y2] = yoloNormalizedToXyxy(ann);
                        return (
                          <g key={`${ann.class_id}-${idx}`}>
                            <rect
                              x={x1 * 1000}
                              y={y1 * 1000}
                              width={(x2 - x1) * 1000}
                              height={(y2 - y1) * 1000}
                              fill="rgba(14, 165, 233, 0.16)"
                              stroke="var(--signal-accent-text)"
                              strokeWidth={4}
                            />
                            <text
                              x={x1 * 1000 + 8}
                              y={Math.max(24, y1 * 1000 + 22)}
                              fill="var(--text-primary)"
                              fontSize={22}
                              fontFamily="monospace"
                            >
                              {ann.class_id}:{ann.label} ({ann.cx.toFixed(2)},{ann.cy.toFixed(2)})
                            </text>
                          </g>
                        );
                      })}
                    </svg>
                  </>
                ) : (
                  <div
                    className="mono"
                    style={{
                      display: "flex",
                      flexDirection: "column",
                      alignItems: "center",
                      gap: 6,
                      color: "var(--text-muted)",
                      fontSize: "var(--text-xs)",
                      textAlign: "center",
                      padding: 16,
                    }}
                  >
                    <span className="nvr-badge">
                      YOLO ANNOTATION STAGE // 15-CLASS SECURITY TAXONOMY
                    </span>
                    <span>
                      Select a dataset sample to inspect or draw normalized YOLO bounding boxes.
                    </span>
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>
      </div>

      {/* Step 2: NinjaNVR Model Factory (Colab G4 / Local RTX Fine-Tuning) Card */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "FÁBRICA DE MODELOS NINJANVR (COLAB G4 / FINE-TUNING LOCAL RTX)"
              : "NinjaNVR Model Factory (Colab G4 / Local RTX Fine-Tuning)"}
          </span>
          <span className="nvr-badge live">
            15-CLASS · FREEZE=10 · FP16
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Preset de 15 Classes de Segurança (Labelmap & Tensor)"
                  : "15-Class Security Taxonomy Preset"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? `Labelmap ativo: ${modelCfg.labelmap_path} (${modelCfg.model_type})`
                  : `Active labelmap: ${modelCfg.labelmap_path} (${modelCfg.model_type})`}
              </div>
            </div>
            <button
              type="button"
              className={`nvr-btn mono ${
                modelCfg.labelmap_path === NINJANVR_SECURITY_15CL_LABELMAP_PATH &&
                modelCfg.model_type === "yolo-generic"
                  ? "active"
                  : "primary"
              }`}
              style={{ height: 28, padding: "0 10px", fontSize: "var(--text-xs)" }}
              onClick={() => {
                updateDraft?.((next) => {
                  next.model = applySecurity15ClassPreset(next.model);
                });
                notify(
                  "Applied 15-Class Security Labelmap Preset (/openvino-model/ninjanvr_security_15cl.txt · yolo-generic)"
                );
              }}
            >
              <CheckCircle2 size={11} />
              Apply 15-Class Security Labelmap Preset
            </button>
          </div>

          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Treinamento em Nuvem Google Colab (G4 / A100 / T4)"
                  : "Cloud GPU Training Factory (Google Colab)"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Treine um modelo YOLOv11 customizado com backbone congelado (freeze=10) e exporte para .onnx / OpenVINO."
                  : "Fine-tune a frozen-backbone YOLOv11 detector from your exported dataset (.zip) and export to .onnx / FP16."}
              </div>
            </div>
            <a
              href={COLAB_MODEL_FACTORY_URL}
              target="_blank"
              rel="noopener noreferrer"
              className="nvr-btn success mono"
              style={{
                height: 28,
                padding: "0 10px",
                fontSize: "var(--text-xs)",
              }}
            >
              <Sparkles size={11} />
              Launch Google Colab Notebook (G4 / A100 / T4)
            </a>
          </div>

          <div className="nvr-subcard" style={{ padding: "8px 10px", marginTop: "auto" }}>
            <div
              style={{
                display: "flex",
                alignItems: "center",
                justifyContent: "space-between",
                gap: 8,
                flexWrap: "wrap",
              }}
            >
              <span
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", fontWeight: 700 }}
              >
                LOCAL GPU (RTX 4070 Ti Super 16GB · ~60s) / CPU FROZEN-BACKBONE (freeze=10) CLI:
              </span>
              <button
                type="button"
                className="nvr-btn mono"
                style={{ height: 22, padding: "0 8px", fontSize: "var(--text-xs)" }}
                onClick={() => {
                  const origin =
                    typeof window !== "undefined" && window.location?.origin
                      ? window.location.origin
                      : "http://localhost:5000";
                  const cmd = buildLocalFinetuneCliCommand({
                    nvrUrl: origin,
                    imgsz: modelCfg.width === 320 ? 320 : 640,
                  });
                  navigator.clipboard?.writeText(cmd);
                  notify("Copied local RTX / CPU fine-tuning CLI command to clipboard");
                }}
              >
                <Copy size={11} />
                Copy CLI Command
              </button>
            </div>
            <code
              className="mono"
              style={{
                display: "block",
                padding: "6px 8px",
                background: "var(--bg-input)",
                border: "1px solid var(--border-input)",
                borderRadius: 2,
                fontSize: "var(--text-xs)",
                color: "var(--signal-live-text)",
                wordBreak: "break-all",
              }}
            >
              {buildLocalFinetuneCliCommand({
                nvrUrl:
                  typeof window !== "undefined" && window.location?.origin
                    ? window.location.origin
                    : "http://localhost:5000",
                imgsz: modelCfg.width === 320 ? 320 : 640,
              })}
            </code>
          </div>
        </div>
      </div>

      {/* Step 3: Zero-Downtime Custom Model Upload & Engine Hot-Reload Card */}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <span>
            {isPt
              ? "HOT-RELOAD DE MODELO TREINADO (.ONNX / .XML)"
              : "HOT-RELOAD FINE-TUNED MODEL (.ONNX / .XML)"}
          </span>
          <span className="nvr-badge live">
            {uploadBase64 ? "WEIGHTS STAGED" : "ZERO-DOWNTIME HOT-RELOAD"}
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Arquivo de Pesos do Modelo (.onnx / .xml)"
                  : "Select Custom Model Weights (.onnx / .xml)"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Selecione o arquivo .onnx ou .xml exportado pelo Colab ou script local."
                  : "Choose a trained .onnx or OpenVINO .xml file from disk to stage for upload."}
              </div>
            </div>
            <label
              className="nvr-btn mono"
              style={{ height: 28, padding: "0 10px", fontSize: "var(--text-xs)", cursor: "pointer" }}
            >
              {uploadBase64 ? `✓ ${uploadFilename}` : isPt ? "Escolher Arquivo .onnx..." : "Choose .onnx / .xml File..."}
              <input
                type="file"
                accept=".onnx,.xml,.bin"
                aria-label="Select custom .onnx or .xml model file"
                style={{ display: "none" }}
                onChange={(e) => {
                  const file = e.target.files?.[0];
                  if (!file) return;
                  setUploadFilename(file.name);
                  const reader = new FileReader();
                  reader.onload = () => {
                    if (typeof reader.result === "string") {
                      setUploadBase64(reader.result);
                    }
                  };
                  reader.readAsDataURL(file);
                }}
              />
            </label>
          </div>

          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Nome do Arquivo de Destino" : "Target Model Filename"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Gravado em /config/models/ e ativado automaticamente no detector."
                  : "Persisted under /config/models/ and bound to the active detector pool."}
              </div>
            </div>
            <input
              type="text"
              className="nvr-input mono"
              aria-label="Custom model filename"
              style={{ width: 280, maxWidth: "100%" }}
              value={uploadFilename}
              onChange={(e) => setUploadFilename(e.target.value)}
            />
          </div>

          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt ? "Dimensões do Tensor de Entrada" : "Model Input Dimensions"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Resolução quadrada (NCHW · RGB) usada na exportação do modelo."
                  : "Input tensor resolution matching the exported YOLO / SSD network."}
              </div>
            </div>
            <select
              className="nvr-select mono"
              aria-label="Model input dimensions"
              style={{ width: 280, maxWidth: "100%" }}
              value={`${uploadWidth}x${uploadHeight}`}
              onChange={(e) => {
                const [w, h] = e.target.value.split("x").map(Number);
                setUploadWidth(w || 640);
                setUploadHeight(h || 640);
              }}
            >
              <option value="640x640">640×640 (YOLOv8 / YOLOv11 · NCHW)</option>
              <option value="320x320">320×320 (YOLOX Edge · NCHW)</option>
              <option value="300x300">300×300 (MobileNetV2 SSD · NHWC)</option>
            </select>
          </div>

          <button
            type="button"
            className="nvr-btn success mono"
            style={{
              width: "100%",
              height: 28,
              padding: "0 12px",
              fontSize: "var(--text-xs)",
              marginTop: "auto",
            }}
            disabled={busyAction === "upload-model"}
            onClick={() => void handleUploadModel()}
          >
            {busyAction === "upload-model"
              ? "Uploading & Reloading..."
              : "Upload Trained .onnx Model & Hot-Reload"}
          </button>
        </div>
      </div>
    </div>
  );
};
