import {
  DatasetAnnotationBox,
  DatasetSampleItem,
  DatasetSummaryResponse,
  FalsePositiveSignatureItem,
  ModelConfig,
  ModelUploadPayload,
  StateRoiConfig,
} from "../../../types/nvr";
import { normalizeLicensePlate } from "../../../utils/configYaml";
import { checkBundledCocoSsdModel } from "./ObjectConfidenceSection";

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

export interface BatchAutoAnnotateProgress {
  active: boolean;
  mode: "pending" | "all_positives";
  total: number;
  completed: number;
  succeeded: number;
  failed: number;
  boxesGenerated: number;
  currentSampleId: string | null;
  currentCamera: string | null;
  startedAtMs: number;
  finishedAtMs: number | null;
  cancelled: boolean;
}

export function isSafeDatasetSampleId(id: unknown): id is string {
  return (
    typeof id === "string" &&
    id.length > 0 &&
    id.length <= 128 &&
    id !== "." &&
    !id.includes("..") &&
    /^[A-Za-z0-9_.-]+$/.test(id)
  );
}

export function selectBatchAutoAnnotateCandidates(
  samples: DatasetSampleItem[],
  mode: "pending" | "all_positives" = "pending"
): DatasetSampleItem[] {
  if (!Array.isArray(samples)) return [];
  const positives = samples.filter(
    (s) => s && isSafeDatasetSampleId(s.id) && s.sample_kind !== "hard_negative"
  );
  if (mode === "all_positives") {
    return positives.filter((s) => !(s.verified && s.verified_by === "manual"));
  }
  return positives.filter((s) => !s.verified);
}

export function computeBatchProgressMetrics(
  progress: BatchAutoAnnotateProgress | null | undefined,
  nowMs: number = Date.now()
): {
  pct: number;
  remaining: number;
  elapsedSec: number;
  etaSec: number | null;
} {
  if (
    !progress ||
    typeof progress.total !== "number" ||
    !Number.isFinite(progress.total) ||
    progress.total <= 0
  ) {
    return { pct: 0, remaining: 0, elapsedSec: 0, etaSec: null };
  }
  const total = Math.max(1, toSafeCount(progress.total));
  const completed = Math.min(total, toSafeCount(progress.completed));
  const pct = Math.max(0, Math.min(100, Math.round((completed / total) * 100)));
  const remaining = Math.max(0, total - completed);
  const safeNowMs =
    typeof nowMs === "number" && Number.isFinite(nowMs) ? nowMs : Date.now();
  const endRef =
    typeof progress.finishedAtMs === "number" && Number.isFinite(progress.finishedAtMs)
      ? progress.finishedAtMs
      : safeNowMs;
  const startRef =
    typeof progress.startedAtMs === "number" &&
    Number.isFinite(progress.startedAtMs) &&
    progress.startedAtMs > 0
      ? progress.startedAtMs
      : endRef;
  const rawElapsedMs = endRef - startRef;
  const elapsedMs =
    Number.isFinite(rawElapsedMs) && rawElapsedMs > 0 ? rawElapsedMs : 0;
  const elapsedSec = Math.round(elapsedMs / 1000);
  const etaSec =
    progress.active && completed > 0 && remaining > 0
      ? Math.max(1, Math.round(((elapsedMs / completed) * remaining) / 1000))
      : null;
  return { pct, remaining, elapsedSec, etaSec };
}

const VISION_RESERVED_KEYS = new Set(["__proto__", "constructor", "prototype"]);

function extractEmbeddedIpv4Vision(hostClean: string): string | null {
  const hexMatch =
    /^(?:::ffff:|0:0:0:0:0:ffff:|64:ff9b::|::)([0-9a-f]{1,4}):([0-9a-f]{1,4})$/i.exec(
      hostClean
    );
  if (hexMatch) {
    const hi = parseInt(hexMatch[1], 16);
    const lo = parseInt(hexMatch[2], 16);
    return `${(hi >> 8) & 0xff}.${hi & 0xff}.${(lo >> 8) & 0xff}.${lo & 0xff}`;
  }
  const dottedMatch =
    /^(?:::ffff:|0:0:0:0:0:ffff:|64:ff9b::|::)(\d{1,3}(?:\.\d{1,3}){3})$/i.exec(
      hostClean
    );
  if (dottedMatch) {
    return dottedMatch[1];
  }
  return null;
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
  let decodedFile = rawFile;
  try {
    decodedFile = decodeURIComponent(rawFile);
  } catch {
    decodedFile = rawFile;
  }
  const baseSegment =
    decodedFile
      .split(/[/\\]/)
      .pop()
      ?.replace(/\.\.+/g, "")
      .replace(/[^a-zA-Z0-9._-]/g, "_")
      .replace(/^[._-]+|[._-]+$/g, "") || "";
  const stemPart = baseSegment.replace(/\.(?:onnx|xml)$/i, "");
  const hasValidStem = Boolean(stemPart && /[a-zA-Z0-9]/.test(stemPart));
  const hasValidExt = /\.(?:onnx|xml)$/i.test(baseSegment);
  const cleanName =
    hasValidStem && hasValidExt
      ? baseSegment
      : hasValidStem
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
  const safeModelType =
    params.modelType === "yolo-generic" ||
    params.modelType === "yolox" ||
    params.modelType === "ssd"
      ? params.modelType
      : "yolo-generic";
  const safeTensor =
    params.inputTensor === "nchw" || params.inputTensor === "nhwc"
      ? params.inputTensor
      : "nchw";
  const safePixelFormat =
    params.inputPixelFormat === "rgb" || params.inputPixelFormat === "bgr"
      ? params.inputPixelFormat
      : "rgb";
  return {
    filename: cleanName,
    model_base64: strippedB64,
    labelmap:
      params.labelmap?.trim() || NINJANVR_SECURITY_CLASSES.join("\n"),
    model_type: safeModelType,
    width: safeW,
    height: safeH,
    input_tensor: safeTensor,
    input_pixel_format: safePixelFormat,
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
    .replace(/[^a-zA-Z0-9._/-]/g, "")
    .replace(/\.\.+/g, "")
    .replace(/^[-./]+/, "")
    .trim();
  const dataset =
    rawDataset && /[a-zA-Z0-9]/.test(rawDataset)
      ? rawDataset
      : "ninjanvr-dataset.zip";
  const rawArch = (params?.modelArch || "yolo11s.pt")
    .replace(/[^a-zA-Z0-9._/-]/g, "")
    .replace(/\.\.+/g, "")
    .replace(/^[-./]+/, "")
    .trim();
  const modelArch =
    rawArch && /[a-zA-Z0-9]/.test(rawArch) ? rawArch : "yolo11s.pt";
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
    .replace(/[;&|`$><\r\n\0'"\\\s()#*?[\]{}~!]/g, "")
    .trim()
    .replace(/\/+$/, "");
  const rawUrl = /^https?:\/\/[a-zA-Z0-9._:-]+$/i.test(cleanedUrl)
    ? cleanedUrl
    : "http://localhost:5000";
  const candidateToken = (params?.nvrToken || "$NINJANVR_API_TOKEN").trim();
  const token =
    /^(?:\$[A-Za-z_][A-Za-z0-9_]*|[a-zA-Z0-9_][a-zA-Z0-9._-]*)$/.test(
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
  const effectiveIpv4 = extractEmbeddedIpv4Vision(hostClean) || hostClean;
  if (
    !hostClean ||
    hostClean === "0.0.0.0" ||
    hostClean === "::" ||
    hostClean === "0:0:0:0:0:0:0:0" ||
    hostClean === "fd00:ec2::254" ||
    hostClean === "metadata.google.internal" ||
    hostClean.endsWith(".metadata.google.internal") ||
    effectiveIpv4 === "0.0.0.0" ||
    effectiveIpv4 === "100.100.100.200" ||
    /^0(?:\.\d{1,3}){3}$/.test(effectiveIpv4) ||
    /^169\.254(?:\.\d{1,3}){2}$/.test(effectiveIpv4) ||
    /^fe[89ab][0-9a-f]:/i.test(hostClean) ||
    /^(?:22[4-9]|23\d)(?:\.\d{1,3}){3}$/.test(effectiveIpv4) ||
    /^ff[0-9a-f]{2}:/i.test(hostClean)
  ) {
    return false;
  }
  const isLoopback =
    hostClean === "localhost" ||
    hostClean.endsWith(".localhost") ||
    hostClean === "::1" ||
    hostClean === "0:0:0:0:0:0:0:1" ||
    /^127(?:\.\d{1,3}){3}$/.test(effectiveIpv4);
  if (isLoopback && portNum !== 11434) {
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
  const trimmedLower = (raw || "").trim().toLowerCase();
  if (!trimmedLower || VISION_RESERVED_KEYS.has(trimmedLower)) {
    return "";
  }
  const slug = trimmedLower
    .normalize("NFD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/[^a-z0-9_]+/g, "_")
    .replace(/^_+|_+$/g, "");
  if (!slug || !/[a-z0-9]/.test(slug) || VISION_RESERVED_KEYS.has(slug)) {
    return "";
  }
  return slug;
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

export type AiSettingsSubSectionId =
  | "all"
  | "engine"
  | "objects"
  | "genai"
  | "faces"
  | "lpr"
  | "state_rois";

export interface AiSettingsSubSectionMeta {
  id: AiSettingsSubSectionId;
  labelEn: string;
  labelPt: string;
}

export const DEFAULT_AI_SUBSECTION: AiSettingsSubSectionId = "engine";

export const AI_SETTINGS_SUBSECTIONS: AiSettingsSubSectionMeta[] = [
  { id: "engine", labelEn: "1. Engine & Tracking", labelPt: "1. Motor & Rastreamento" },
  { id: "objects", labelEn: "2. Classes & Confidence", labelPt: "2. Classes & Confiança" },
  { id: "genai", labelEn: "3. Vision AI Judge & FP", labelPt: "3. Juiz IA & Falsos Positivos" },
  { id: "faces", labelEn: "4. Face Recognition", labelPt: "4. Reconhecimento Facial" },
  { id: "lpr", labelEn: "5. License Plates (LPR)", labelPt: "5. Leitura de Placas (LPR)" },
  { id: "state_rois", labelEn: "6. State Monitor (ROI)", labelPt: "6. Estado de Área (ROI)" },
  { id: "all", labelEn: "All Modules", labelPt: "Todos os Módulos" },
];

export function isAiSubSectionVisible(
  activeSection: AiSettingsSubSectionId,
  targetSection: Exclude<AiSettingsSubSectionId, "all">
): boolean {
  return activeSection === "all" || activeSection === targetSection;
}

export function resolveSettingsScopeBadge(
  kind: "global_only" | "camera_or_global" | "camera_only",
  scope: "global" | string,
  inspectorCam: string,
  isPt = false
): { label: string; isCameraScope: boolean; effectiveCamera: string } {
  const cleanCam = (inspectorCam || "").trim() || "camera";
  if (kind === "global_only") {
    return {
      label: isPt ? "ESCOPO: GLOBAL" : "SCOPE: GLOBAL",
      isCameraScope: false,
      effectiveCamera: cleanCam,
    };
  }
  if (kind === "camera_only") {
    return {
      label: isPt
        ? `ESCOPO: CÂMERA (${cleanCam.toUpperCase()})`
        : `SCOPE: CAMERA (${cleanCam.toUpperCase()})`,
      isCameraScope: true,
      effectiveCamera: cleanCam,
    };
  }
  if (scope === "global") {
    return {
      label: isPt ? "ESCOPO: PADRÃO GLOBAL" : "SCOPE: GLOBAL DEFAULTS",
      isCameraScope: false,
      effectiveCamera: cleanCam,
    };
  }
  return {
    label: isPt
      ? `ESCOPO: ${cleanCam.toUpperCase()}`
      : `SCOPE: ${cleanCam.toUpperCase()}`,
    isCameraScope: true,
    effectiveCamera: cleanCam,
  };
}

export function isBundledCocoSsdModel(
  model?: Partial<ModelConfig> | null
): boolean {
  return checkBundledCocoSsdModel(model);
}

export function filterFalsePositiveSignaturesByScope(
  signatures: FalsePositiveSignatureItem[],
  scope: "global" | string,
  inspectorCam: string,
  filterMode: "scope" | "all"
): FalsePositiveSignatureItem[] {
  if (!Array.isArray(signatures)) return [];
  if (scope === "global" || filterMode === "all") {
    return signatures;
  }
  const target = (inspectorCam || "").trim().toLowerCase();
  if (!target) return signatures;
  return signatures.filter(
    (sig) => (sig.camera || "").trim().toLowerCase() === target
  );
}

export function formatFalsePositiveReason(
  reason?: string | null,
  dominantColor?: string | null,
  isPt = false
): { text: string; isTechnicalFallback: boolean } {
  const raw = (reason || dominantColor || "").trim();
  if (!raw || raw.toLowerCase() === "operator_marked") {
    return {
      text: isPt
        ? "Marcado manualmente pelo operador"
        : "Marked manually by operator",
      isTechnicalFallback: true,
    };
  }
  const colorMatch = /^color\s*:\s*(.+)$/i.exec(raw);
  if (colorMatch) {
    const colorName = colorMatch[1].trim();
    return {
      text: isPt
        ? `Assinatura estática (cor dominante: ${colorName})`
        : `Static region signature (dominant color: ${colorName})`,
      isTechnicalFallback: true,
    };
  }
  return {
    text: raw,
    isTechnicalFallback: false,
  };
}
