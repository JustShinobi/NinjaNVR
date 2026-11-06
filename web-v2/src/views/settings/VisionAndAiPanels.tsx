import React, { useEffect, useState } from "react";
import { SettingsScopeDivider } from "../../components/SettingsScopeDivider";
import { CardCollapseToggle } from "../../components/CardCollapseToggle";
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

export function selectBatchAutoAnnotateCandidates(
  samples: DatasetSampleItem[],
  mode: "pending" | "all_positives" = "pending"
): DatasetSampleItem[] {
  if (!Array.isArray(samples)) return [];
  const positives = samples.filter(
    (s) => s && typeof s.id === "string" && s.sample_kind !== "hard_negative"
  );
  if (mode === "all_positives") {
    // Re-running the Teacher never overwrites a sample an operator checked by hand.
    return positives.filter((s) => !(s.verified && s.verified_by === "manual"));
  }
  // Pending = not verified yet. AI-verified samples are done; re-annotating
  // them is the explicit "all_positives" mode.
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
  if (!progress || progress.total <= 0) {
    return { pct: 0, remaining: 0, elapsedSec: 0, etaSec: null };
  }
  const total = Math.max(1, toSafeCount(progress.total));
  const completed = Math.min(total, toSafeCount(progress.completed));
  const pct = Math.max(0, Math.min(100, Math.round((completed / total) * 100)));
  const remaining = Math.max(0, total - completed);
  const endRef =
    typeof progress.finishedAtMs === "number" && Number.isFinite(progress.finishedAtMs)
      ? progress.finishedAtMs
      : nowMs;
  const elapsedMs = Math.max(0, endRef - (progress.startedAtMs || endRef));
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
  if (
    !cleaned ||
    !/[a-zA-Z0-9]/.test(cleaned) ||
    cleaned.toLowerCase() === "unknown" ||
    VISION_RESERVED_KEYS.has(cleaned.toLowerCase())
  ) {
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

/** The AI section opens on one module; "All Modules" stays available last. */
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
  const cleanCam = (inspectorCam || "escritorio").trim() || "escritorio";
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
  const modelType = (model?.model_type || "ssd").trim().toLowerCase();
  const modelPath = (
    model?.path || "/openvino-model/ssdlite_mobilenet_v2.xml"
  )
    .trim()
    .toLowerCase();
  return modelType === "ssd" && modelPath.includes("ssdlite_mobilenet_v2");
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
      const lSlug = slugifyRoiLocationName(zoneNameInput);
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
      const zSlug = slugifyRoiLocationName(zoneNameInput);
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
          <span style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            {inspectorCam.toUpperCase()} //{" "}
            {isPt ? "CONSTRUTOR DE ZONAS & MÁSCARAS" : "POLYGON VECTOR STUDIO"}
            <span className="nvr-yaml-key-tag">cameras.{inspectorCam}.zones · lines · motion.mask</span>
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
                  className={`nvr-btn ${polyType === id ? "active" : ""} sm`}
                  style={{ minWidth: 0 }}
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
                        className={`nvr-btn ${active ? "active" : ""} sm`}
                        title={isPt ? preset.descPt : preset.descEn}
                        style={{ justifyContent: "flex-start", gap: 5, minWidth: 0, width: "100%", overflow: "hidden" }}
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
                    className="nvr-input mono sm"
                    style={{ width: "100%" }}
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
                      className="nvr-input mono sm"
                      style={{ width: "100%" }}
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
                      className={`nvr-btn mono ${!roiAnomalyState.trim() ? "active" : ""} sm`}
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
                          className={`nvr-btn mono ${isSel ? "active" : ""} sm`}
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
                          } sm`}
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
                        className="nvr-input mono sm"
                        style={{ width: "100%" }}
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
                          className="nvr-input mono sm"
                          style={{ width: "100%" }}
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
                          className="nvr-input mono sm"
                          style={{ width: "100%" }}
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
                          className="nvr-input mono sm"
                          style={{ width: "100%" }}
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
                          className="nvr-input mono sm"
                          style={{ width: "100%" }}
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
                          className="nvr-input mono sm"
                          style={{ width: "100%" }}
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
                  className="nvr-input mono sm"
                  style={{ flex: 1 }}
                  placeholder="Tripwire ID (e.g. driveway_line)"
                  aria-label="Tripwire Name"
                  value={zoneNameInput}
                  onChange={(e) => setZoneNameInput(e.target.value)}
                />
                <select
                  className="nvr-select mono sm"
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
                      className="nvr-input mono sm"
                      style={{ width: "100%" }}
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
                      className="nvr-input mono sm"
                      style={{ width: "100%" }}
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
                      className="nvr-input mono sm"
                      style={{ width: "100%" }}
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
                    className="nvr-input mono sm"
                    style={{ flex: 1 }}
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
                        className={`nvr-btn mono ${active ? "active" : ""} sm`}
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
                className="nvr-btn success sm"
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
                  className="nvr-btn sm"
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
                className="nvr-btn sm"
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
                className="nvr-btn sm"
                disabled={draftPoints.length === 0}
                onClick={() => setDraftPoints((p) => p.slice(0, -1))}
              >
                Undo
              </button>
              <button
                type="button"
                className="nvr-btn danger sm"
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
                      className="nvr-btn sm"
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
                      className="nvr-btn danger sm"
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
                      className="nvr-btn sm"
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
                      className="nvr-btn danger sm"
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
                      className="nvr-btn sm"
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
                      className="nvr-btn danger sm"
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
                      className="nvr-btn danger sm"
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
                      className="nvr-btn sm"
                      onClick={() => {
                        setPolyType("motion_mask");
                        setDraftPoints(coordStringToPoints(m, detW, detH));
                      }}
                    >
                      <Edit3 size={10} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger sm"
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
                      className="nvr-btn sm"
                      onClick={() => {
                        setPolyType("object_mask");
                        setDraftPoints(coordStringToPoints(m, detW, detH));
                      }}
                    >
                      <Edit3 size={10} />
                    </button>
                    <button
                      type="button"
                      className="nvr-btn danger sm"
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
              <span className="nvr-yaml-key-tag">cameras.{inspectorCam}.motion</span>
              <ToggleSwitch
                checked={motionCfg.enabled ?? true}
                label="Enable Motion Detector"
                onChange={(v) => updateMotionField("enabled", v)}
              />
            </div>

            <div style={{ display: "flex", alignItems: "center", gap: 5 }}>
              <button
                type="button"
                className={`nvr-btn ${isIndoorMotion ? "active" : ""} sm`}
                onClick={() => applyMotionPreset(22, 8, 0.85, "Indoor / High Sensitivity")}
              >
                {isPt ? "Interno (Sensível)" : "Indoor (Sensitive)"}
              </button>
              <button
                type="button"
                className={`nvr-btn ${isBalancedMotion ? "active" : ""} sm`}
                onClick={() => applyMotionPreset(30, 10, 0.8, "Balanced Default")}
              >
                {isPt ? "Equilibrado (Padrão)" : "Balanced (Default)"}
              </button>
              <button
                type="button"
                className={`nvr-btn ${isOutdoorMotion ? "active" : ""} sm`}
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
                className="nvr-btn sm"
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
                className="nvr-input mono sm"
                style={{ width: 54 }}
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
                className="nvr-input mono sm"
                style={{ width: 54 }}
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
  onOpenVectorStudio?: () => void;
  /** Module shown first; defaults to a single module rather than "All Modules". */
  initialSubSection?: AiSettingsSubSectionId;
  /** Operational data cards (FP memory, enrolled faces, known plates) start collapsed. */
  initialDataCardsOpen?: boolean;
}> = ({
  draftConfig,
  updateDraft,
  scope,
  inspectorCam,
  notify,
  onOpenVectorStudio,
  initialSubSection = DEFAULT_AI_SUBSECTION,
  initialDataCardsOpen = false,
}) => {
  const { locale } = useI18n();
  const isPt = locale === "pt";
  const stats = useNvrStore((s) => s.stats);
  const [activeSubSection, setActiveSubSection] =
    useState<AiSettingsSubSectionId>(initialSubSection);
  const [customLabel, setCustomLabel] = useState("");
  const [selectedFilterLabel, setSelectedFilterLabel] = useState("person");
  const [showAdvancedModel, setShowAdvancedModel] = useState(false);
  const [showAdvancedByteTrack, setShowAdvancedByteTrack] = useState(false);
  const [showAdvancedFilterBox, setShowAdvancedFilterBox] = useState(false);
  const [isCustomResolution, setIsCustomResolution] = useState(false);
  const [fpSignatures, setFpSignatures] = useState<FalsePositiveSignatureItem[]>([]);
  const [fpLoading, setFpLoading] = useState(false);
  const [fpFilterMode, setFpFilterMode] = useState<"scope" | "all">("scope");
  const [showFpMemory, setShowFpMemory] = useState(initialDataCardsOpen);
  const [showFaceGallery, setShowFaceGallery] = useState(initialDataCardsOpen);
  const [showKnownPlates, setShowKnownPlates] = useState(initialDataCardsOpen);

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
  const aiSectionMountedRef = React.useRef(true);
  const genaiValidateSeqRef = React.useRef(0);

  useEffect(() => {
    aiSectionMountedRef.current = true;
    return () => {
      aiSectionMountedRef.current = false;
      genaiValidateSeqRef.current += 1;
    };
  }, []);

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
    if (genaiValidating) return;
    const seq = ++genaiValidateSeqRef.current;
    setGenaiValidating(true);
    setGenaiValidationState({ status: "idle" });
    const prov = (genaiCfg.provider || "gemini").trim().toLowerCase();
    const directKey =
      genaiCfg.api_key && genaiCfg.api_key !== "***" && genaiCfg.api_key !== "*"
        ? genaiCfg.api_key.trim()
        : "";
    const customBase = (genaiCfg.base_url || "").trim().replace(/\/+$/, "");

    const applyValidModels = (models: string[], customMsg?: string) => {
      if (!aiSectionMountedRef.current || seq !== genaiValidateSeqRef.current) {
        return;
      }
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
      if (!aiSectionMountedRef.current || seq !== genaiValidateSeqRef.current) {
        return;
      }
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
        if (!aiSectionMountedRef.current || seq !== genaiValidateSeqRef.current) {
          return;
        }
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
      if (!aiSectionMountedRef.current || seq !== genaiValidateSeqRef.current) {
        return;
      }
      setGenaiModels([]);
      const errMsg = isPt
        ? "Erro de conexão ao validar API do provedor"
        : "Network error while validating provider API";
      setGenaiValidationState({ status: "invalid", message: errMsg });
      notify(errMsg);
    } finally {
      if (aiSectionMountedRef.current && seq === genaiValidateSeqRef.current) {
        setGenaiValidating(false);
      }
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
  const [enrollFileName, setEnrollFileName] = useState<string>("");
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
        setEnrollFileName("");
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
  const [roiTargetCam, setRoiTargetCam] = useState<string>(
    inspectorCam || "escritorio"
  );

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
      if (scope !== "global") {
        setRoiTargetCam(inspectorCam);
      }
    }
  }, [inspectorCam, scope]);

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

  const effectiveRoiCam =
    scope === "global"
      ? roiTargetCam || inspectorCam || "escritorio"
      : inspectorCam || "escritorio";
  const scopeCameraCount = Object.keys(draftConfig.cameras || {}).length;
  const scopeDivider = (group: "global" | "camera") => (
    <SettingsScopeDivider
      group={group}
      scope={scope}
      inspectorCam={inspectorCam}
      cameraCount={scopeCameraCount}
      isPt={isPt}
    />
  );
  const isCocoSsdActive = isBundledCocoSsdModel(modelCfg);
  const filteredFpSignatures = filterFalsePositiveSignaturesByScope(
    fpSignatures,
    scope,
    inspectorCam,
    fpFilterMode
  );
  const previewRoiCrop = parseRoiCropInput(newRoiCrop);

  return (
    <div className="nvr-card-grid" style={{ gap: 10 }}>
      {/* Tactical Sub-Navigation Bar to Eliminate Vertical Scroll Overload */}
      <div
        className="nvr-settings-subnav"
        role="navigation"
        aria-label={
          isPt
            ? "Sub-navegação de módulos de IA e Filtros"
            : "AI & Object Filters Module Navigation"
        }
      >
        <div className="nvr-settings-subnav-pills">
          {AI_SETTINGS_SUBSECTIONS.map((sec) => {
            const isActive = activeSubSection === sec.id;
            return (
              <button
                key={sec.id}
                type="button"
                className={`nvr-btn mono ${isActive ? "active" : ""} sm`}
                onClick={() => setActiveSubSection(sec.id)}
              >
                {isPt ? sec.labelPt : sec.labelEn}
              </button>
            );
          })}
        </div>
      </div>

      {/* Guided OpenVINO AI Detector & Model Card */}
      {isAiSubSectionVisible(activeSubSection, "engine") && (
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
              {primaryDet.device || "CPU"} ACTIVE · {liveInferMs}ms
            </span>
          </div>
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
      </>
      )}

      {/* Guided Detect & ByteTrack Behavior Card */}
      {isAiSubSectionVisible(activeSubSection, "engine") && (
      <>
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
            <div style={{ display: "grid", gridTemplateColumns: "repeat(3, minmax(0, 1fr))", gap: 6 }}>
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

            {/* Collapsible Advanced ByteTrack Frame Parameters */}
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
      </div>
      </>
      )}

      {/* Tracked Objects & Intuitive Confidence Sliders */}
      {isAiSubSectionVisible(activeSubSection, "objects") && (
      <>
      {scopeDivider("camera")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "OBJETOS A DETECTAR" : "OBJECTS TO DETECT"}</span>
            <span className="nvr-yaml-key-tag">objects.track</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span className="nvr-badge live">
              {trackedLabels.length} {isPt ? "CLASSES ATIVAS" : "ACTIVE CLASSES"}
            </span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
            <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)", marginRight: 4 }}>
              {isPt ? "CLASSES:" : "CLASSES:"}
            </span>
            {Array.from(new Set([...STANDARD_LABELS, ...trackedLabels])).map((lbl) => {
              const isTracked = trackedLabels.includes(lbl);
              return (
                <button
                  key={lbl}
                  type="button"
                  className={`nvr-btn mono ${isTracked ? "active" : ""} sm`}
                  onClick={() => toggleTrackLabel(lbl)}
                >
                  {isTracked ? `✓ ${lbl}` : `+ ${lbl}`}
                </button>
              );
            })}
            <input
              className="nvr-input mono sm"
              style={{ width: 155 }}
              placeholder={isPt ? "Outra classe COCO..." : "Custom COCO label..."}
              value={customLabel}
              onChange={(e) => setCustomLabel(e.target.value)}
            />
            <button
              type="button"
              className="nvr-btn sm"
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
                <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
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
                  <span className="nvr-yaml-key-tag">objects.attribute_labels</span>
                  {isCocoSsdActive && (
                    <span className="nvr-badge motion">
                      {isPt
                        ? "REQUER MODELO YOLO 15 CLASSES"
                        : "REQUIRES 15-CLASS YOLO MODEL"}
                    </span>
                  )}
                </div>
                <span style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isCocoSsdActive
                    ? isPt
                      ? "O modelo atual (MobileNetV2 SSD COCO) detecta objetos primários (person, car). Atributos filhos (face, placa e logos de entrega) entram em operação ao ativar o modelo YOLO de 15 classes."
                      : "Active model (MobileNetV2 SSD COCO) detects primary classes (person, car). Child attributes (face, license_plate, courier logos) activate when using a 15-Class Security YOLO model."
                    : isPt
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
                  className="nvr-select mono sm"
                  aria-label="Snapshot Selection Policy" style={{ width: 248, maxWidth: "100%" }}
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

            <div
              className={isCocoSsdActive ? "nvr-cfg-disabled-zone" : ""}
              style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}
            >
              {Array.from(
                new Set([...DEFAULT_ATTRIBUTE_LABELS, ...attributeLabels])
              ).map((attrLbl) => {
                const isActive = attributeLabels.includes(attrLbl);
                return (
                  <button
                    key={attrLbl}
                    type="button"
                    className={`nvr-btn mono ${isActive ? "active" : ""} sm`}
                    style={{ borderStyle: isCocoSsdActive ? "dashed" : "solid" }}
                    onClick={() => toggleAttributeLabel(attrLbl)}
                  >
                    {isActive ? `✓ ${attrLbl}` : `+ ${attrLbl}`}
                  </button>
                );
              })}
            </div>
          </div>
        </div>
      </div>

      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "SENSIBILIDADE DE CONFIANÇA" : "CONFIDENCE SENSITIVITY"}</span>
            <span className="nvr-yaml-key-tag">objects.filters</span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: 8 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <span className="mono" style={{ fontSize: "var(--text-xs)", fontWeight: 700, color: "var(--signal-accent-text)", display: "flex", alignItems: "center", gap: 5 }}>
                  <Sparkles size={12} />
                  {isPt ? "CALIBRAR CLASSE:" : "TUNE CLASS:"}
                </span>
                {trackedLabels.map((lbl) => {
                  const clsFilter =
                    (scope === "global"
                      ? draftConfig.objects?.filters?.[lbl]
                      : draftConfig.cameras?.[inspectorCam]?.objects?.filters?.[lbl]) || {
                      threshold: 0.7,
                    };
                  const pct = Math.round((clsFilter.threshold ?? 0.7) * 100);
                  return (
                    <button
                      key={lbl}
                      type="button"
                      className={`nvr-btn mono ${selectedFilterLabel === lbl ? "active" : ""} sm`}
                      onClick={() => setSelectedFilterLabel(lbl)}
                    >
                      {lbl} ({pct}%)
                    </button>
                  );
                })}
                {trackedLabels.length === 1 && (
                  <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                    {isPt
                      ? "(Ative mais classes ao lado para calibrá-las individualmente)"
                      : "(Enable more classes to tune them individually)"}
                  </span>
                )}
              </div>

              <div style={{ display: "flex", gap: 5, alignItems: "center", flexWrap: "wrap" }}>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "permissive" ? "active" : ""} sm`}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.4, threshold: 0.58 })
                  }
                >
                  {isPt ? "Permissivo (58%)" : "Permissive (58%)"}
                </button>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "balanced" ? "active" : ""} sm`}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.5, threshold: 0.7 })
                  }
                >
                  {isPt ? "Equilibrado (70%)" : "Balanced (70%)"}
                </button>
                <button
                  type="button"
                  className={`nvr-btn ${activeConfPreset === "strict" ? "active" : ""} sm`}
                  onClick={() =>
                    updateObjectFilter({ min_score: 0.65, threshold: 0.82 })
                  }
                >
                  {isPt ? "Rigoroso (82%)" : "Strict (82%)"}
                </button>
              </div>
            </div>

            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              <div className="nvr-field-row" style={{ padding: "4px 0" }}>
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
              className="nvr-btn mono sm"
              style={{ width: "100%", justifyContent: "space-between" }}
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
      </>
      )}

      {/* Vision AI Judge (genai) Configuration Card */}
      {isAiSubSectionVisible(activeSubSection, "genai") && (
      <>
      {scopeDivider("global")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "JUIZ DE IA VISUAL" : "VISION AI JUDGE"}</span>
            <span className="nvr-yaml-key-tag">genai</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span
              className={`nvr-badge ${
                genaiCfg.enabled && Boolean(genaiCfg.model)
                  ? "live"
                  : genaiCfg.enabled
                  ? "motion"
                  : ""
              }`}
            >
              <Sparkles size={11} />
              {(genaiCfg.provider || "gemini").toUpperCase()} ·{" "}
              {genaiCfg.model
                ? genaiCfg.model
                : isPt
                ? "MODELO NÃO SELECIONADO"
                : "NO MODEL SELECTED"}{" "}
              ·{" "}
              {genaiCfg.enabled
                ? genaiCfg.verify_ambiguous_alerts
                  ? "ACTIVE"
                  : isPt
                  ? "ATIVO (AUTO-VERIFICAÇÃO OFF)"
                  : "ACTIVE (AUTO-VERIFY OFF)"
                : isPt
                ? "EM ESPERA (OFF)"
                : "STANDBY (OFF)"}
            </span>
          </div>
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
                  ? "Fluxo guiado: 1. Provedor → 2. Credencial API (Chave direta ou Variável Env) → 3. Validar API → 4. Modelo Visual."
                  : "Guided flow: 1. Provider → 2. API Credential (Direct Key or Env Var fallback) → 3. Validate API → 4. Vision Model."}
              </div>
            </div>
            <ToggleSwitch
              checked={genaiCfg.enabled}
              label="Enable Vision AI Judge"
              onChange={(v) => updateGenAi({ enabled: v })}
            />
          </div>

          <div
            className={!genaiCfg.enabled ? "nvr-cfg-disabled-zone" : ""}
            style={{ display: "flex", flexDirection: "column", gap: 8 }}
          >
            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
                gap: 8,
                alignItems: "end",
              }}
            >
              <div>
                <div
                  className="mono"
                  style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
                >
                  {isPt ? "1. Provedor LLM" : "1. Provider"}
                </div>
                <select
                  className="nvr-select mono"
                  aria-label="Vision AI Judge Provider"
                  style={{ width: "100%", marginTop: 2 }}
                  value={genaiCfg.provider || "gemini"}
                  onChange={(e) => {
                    genaiValidateSeqRef.current += 1;
                    setGenaiValidating(false);
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
                  {isPt ? "2A. Chave da API Direta" : "2A. Direct API Key"}
                  {genaiCfg.has_api_key
                    ? isPt
                      ? " (Salva)"
                      : " (Saved)"
                    : ""}
                </div>
                <div style={{ display: "flex", gap: 4, marginTop: 2 }}>
                  <input
                    type="password"
                    className="nvr-input mono"
                    aria-label="Vision AI Judge API Key"
                    style={{ flex: 1, minWidth: 0, width: "100%" }}
                    placeholder={
                      genaiCfg.has_api_key
                        ? isPt
                          ? "•••••••• (chave salva)"
                          : "•••••••• (saved key)"
                        : genaiCfg.provider === "ollama"
                        ? isPt
                          ? "Opcional (Ollama local)"
                          : "Optional (Local Ollama)"
                        : isPt
                        ? `Cole a chave ou use ${genaiCfg.api_key_env || "GEMINI_API_KEY"}`
                        : `Paste key or use ${genaiCfg.api_key_env || "GEMINI_API_KEY"}`
                    }
                    value={
                      genaiCfg.api_key === "***" || genaiCfg.api_key === "*"
                        ? ""
                        : genaiCfg.api_key || ""
                    }
                    onChange={(e) => {
                      const val = e.target.value;
                      genaiValidateSeqRef.current += 1;
                      setGenaiValidating(false);
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
                      title={
                        isPt
                          ? "Limpar chave da API salva"
                          : "Clear saved Vision AI Judge API key"
                      }
                      aria-label="Clear Vision AI Judge API Key"
                      onClick={() => {
                        genaiValidateSeqRef.current += 1;
                        setGenaiValidating(false);
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
                  {isPt ? "2B. Variável Env (Fallback)" : "2B. Fallback Env Var"}
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
                    genaiValidateSeqRef.current += 1;
                    setGenaiValidating(false);
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
                    ? "2C. Endpoint Base URL (Opcional)"
                    : "2C. Endpoint Base URL (Optional)"}
                </div>
                <input
                  className="nvr-input mono"
                  aria-label="Vision AI Judge Base URL"
                  style={{ width: "100%", marginTop: 2 }}
                  placeholder={
                    genaiCfg.provider === "ollama"
                      ? "http://127.0.0.1:11434"
                      : isPt
                      ? "Opcional (Proxy LLM / OpenRouter)..."
                      : "Optional endpoint override (LLM proxy)..."
                  }
                  value={genaiCfg.base_url || ""}
                  onChange={(e) => {
                    genaiValidateSeqRef.current += 1;
                    setGenaiValidating(false);
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
                    genaiValidationState.status === "valid" || Boolean(genaiCfg.model)
                      ? "success"
                      : "primary"
                  }`}
                  aria-label="Validate Vision AI Judge API" style={{ marginTop: 2, whiteSpace: "nowrap" }}
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
                    : genaiCfg.model
                    ? isPt
                      ? "Revalidar API & Atualizar Lista"
                      : "Re-Validate API & Refresh List"
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
                    ? "4. Modelo Visual Ativo"
                    : "4. Active Vision Model"}
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
                        ? "— Valide a API no Passo 3 para carregar os modelos —"
                        : "— Validate API in Step 3 to load available models —"
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
          </div>
        </div>
      </div>

      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "AUTO-VERIFICAÇÃO & SUPRESSÃO" : "AUTO-VERIFY & SUPPRESSION"}</span>
            <span className="nvr-yaml-key-tag">genai.verify_ambiguous_alerts</span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div
            className={!genaiCfg.enabled ? "nvr-cfg-disabled-zone" : ""}
            style={{ display: "flex", flexDirection: "column", gap: 8 }}
          >
            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
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

              <div
                className={`nvr-field-row ${
                  !genaiCfg.verify_ambiguous_alerts ? "nvr-cfg-disabled-zone" : ""
                }`}
                style={{ borderBottom: "none", padding: "2px 0" }}
              >
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

            <div
              className={!genaiCfg.verify_ambiguous_alerts ? "nvr-cfg-disabled-zone" : ""}
              style={{ display: "flex", flexDirection: "column", gap: 4 }}
            >
              <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
                <div className="nvr-field-meta">
                  <div className="nvr-field-label mono">
                    {isPt ? "Pontuação Ambígua Mín" : "Ambiguous Score Min"} (
                    {Math.round((genaiCfg.ambiguous_score_min ?? 0.45) * 100)}%)
                    {!genaiCfg.verify_ambiguous_alerts && (
                      <span className="nvr-yaml-key-tag">
                        {isPt ? "Auto-Verify desligado" : "Auto-Verify OFF"}
                      </span>
                    )}
                  </div>
                </div>
                <input
                  type="range"
                  min={25}
                  max={80}
                  disabled={!genaiCfg.verify_ambiguous_alerts}
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
                    {isPt ? "Pontuação Ambígua Máx" : "Ambiguous Score Max"} (
                    {Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}%)
                    {!genaiCfg.verify_ambiguous_alerts && (
                      <span className="nvr-yaml-key-tag">
                        {isPt ? "Auto-Verify desligado" : "Auto-Verify OFF"}
                      </span>
                    )}
                  </div>
                </div>
                <input
                  type="range"
                  min={40}
                  max={95}
                  disabled={!genaiCfg.verify_ambiguous_alerts}
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
      </div>
      </>
      )}

      {/* Active False-Positive Memory Signatures Table */}
      {isAiSubSectionVisible(activeSubSection, "genai") && (
      <div
        className={`nvr-cfg-card ${showFpMemory ? "" : "is-collapsed"}`}
        style={{ gridColumn: "1 / -1" }}
      >
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <CardCollapseToggle
              open={showFpMemory}
              onToggle={() => setShowFpMemory((v) => !v)}
              title={isPt ? "MEMÓRIA DE FALSOS POSITIVOS" : "FALSE-POSITIVE MEMORY"}
              count={filteredFpSignatures.length}
              controlsId="nvr-fp-memory-body"
            />
            <span className="nvr-yaml-key-tag">IoU &gt;= 0.72 · dhash64 &lt;= 12</span>
            <span className="nvr-yaml-key-tag">SQLite Live DB</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            {scope !== "global" && (
              <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
                <button
                  type="button"
                  className={`nvr-btn mono ${fpFilterMode === "scope" ? "active" : ""} sm`}
                  onClick={() => setFpFilterMode("scope")}
                >
                  {inspectorCam.toUpperCase()} (
                  {
                    filterFalsePositiveSignaturesByScope(
                      fpSignatures,
                      scope,
                      inspectorCam,
                      "scope"
                    ).length
                  }
                  )
                </button>
                <button
                  type="button"
                  className={`nvr-btn mono ${fpFilterMode === "all" ? "active" : ""} sm`}
                  onClick={() => setFpFilterMode("all")}
                >
                  {isPt ? "TODAS" : "ALL CAMERAS"} ({fpSignatures.length})
                </button>
              </div>
            )}
            <button
              type="button"
              className="nvr-btn mono sm"
              onClick={() => void loadFpSignatures()}
            >
              {fpLoading
                ? isPt
                  ? "Atualizando..."
                  : "Refreshing..."
                : isPt
                ? "Atualizar"
                : "Refresh"}
            </button>
          </div>
        </div>
        {showFpMemory && (
        <div id="nvr-fp-memory-body" className="nvr-cfg-card-body" style={{ padding: 10, gap: 6 }}>
          {filteredFpSignatures.length === 0 ? (
            <div
              className="mono"
              style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}
            >
              {fpSignatures.length > 0 && scope !== "global" && fpFilterMode === "scope"
                ? isPt
                  ? `Nenhuma assinatura de falso positivo em ${inspectorCam.toUpperCase()}. Clique em "TODAS (${fpSignatures.length})" acima para ver assinaturas de outras câmeras.`
                  : `No false-positive signatures recorded on ${inspectorCam.toUpperCase()}. Click "ALL CAMERAS (${fpSignatures.length})" above to view signatures across other cameras.`
                : isPt
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
                    <th style={{ padding: "4px 6px" }}>
                      {isPt ? "CÂMERA" : "CAMERA"}
                    </th>
                    <th style={{ padding: "4px 6px" }}>
                      {isPt ? "CLASSE" : "LABEL"}
                    </th>
                    <th style={{ padding: "4px 6px" }}>
                      {isPt ? "REGIÃO (NORM BBOX)" : "SPATIAL BBOX"}
                    </th>
                    <th style={{ padding: "4px 6px" }}>DHASH64</th>
                    <th style={{ padding: "4px 6px" }}>HITS</th>
                    <th style={{ padding: "4px 6px" }}>
                      {isPt ? "ÚLTIMA VEZ" : "LAST SEEN"}
                    </th>
                    <th style={{ padding: "4px 6px" }}>
                      {isPt ? "MOTIVO / AUDITORIA" : "REASON"}
                    </th>
                    <th style={{ padding: "4px 6px", textAlign: "right" }}>
                      {isPt ? "AÇÃO" : "ACTION"}
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {filteredFpSignatures.map((sig) => {
                    const bx = Number.isFinite(sig.norm_bbox?.[0])
                      ? Math.max(0, Math.min(0.95, sig.norm_bbox[0]))
                      : 0;
                    const by = Number.isFinite(sig.norm_bbox?.[1])
                      ? Math.max(0, Math.min(0.95, sig.norm_bbox[1]))
                      : 0;
                    const bw = Number.isFinite(sig.norm_bbox?.[2])
                      ? Math.max(0.05, Math.min(1 - bx, sig.norm_bbox[2]))
                      : 0.2;
                    const bh = Number.isFinite(sig.norm_bbox?.[3])
                      ? Math.max(0.05, Math.min(1 - by, sig.norm_bbox[3]))
                      : 0.2;
                    const formattedReason = formatFalsePositiveReason(
                      sig.reason,
                      sig.dominant_color,
                      isPt
                    );
                    return (
                      <tr
                        key={sig.id}
                        style={{ borderBottom: "1px solid var(--border-hairline)" }}
                      >
                        <td style={{ padding: "5px 6px", fontWeight: 700, whiteSpace: "nowrap" }}>
                          {sig.camera.toUpperCase()}
                        </td>
                        <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                          {sig.label.toUpperCase()}
                        </td>
                        <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                          <div style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                            <svg
                              width="32"
                              height="18"
                              viewBox="0 0 32 18"
                              aria-hidden="true"
                              style={{
                                background: "var(--bg-input)",
                                border: "1px solid var(--border-strong)",
                                borderRadius: 2,
                                flexShrink: 0,
                              }}
                            >
                              <rect
                                x={bx * 32}
                                y={by * 18}
                                width={Math.max(2, bw * 32)}
                                height={Math.max(2, bh * 18)}
                                fill="rgba(245, 158, 11, 0.35)"
                                stroke="var(--signal-motion)"
                                strokeWidth="1"
                              />
                            </svg>
                            <span>{formatSignatureBbox(sig.norm_bbox)}</span>
                          </div>
                        </td>
                        <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                          {formatDhash64Hex(sig.dhash64)}
                        </td>
                        <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                          {sig.hit_count}
                        </td>
                        <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                          {formatDateTime(sig.last_seen_at)}
                        </td>
                        <td
                          title={formattedReason.text}
                          style={{
                            padding: "5px 6px",
                            color: formattedReason.isTechnicalFallback
                              ? "var(--text-muted)"
                              : "var(--text-secondary)",
                            maxWidth: 440,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          {formattedReason.text}
                        </td>
                        <td style={{ padding: "5px 6px", textAlign: "right" }}>
                          <button
                            type="button"
                            className="nvr-btn danger mono sm"
                            aria-label={`Delete false-positive signature ${sig.id}`}
                            onClick={() => void handleDeleteFpSignature(sig.id)}
                          >
                            <Trash2 size={11} />
                          </button>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </div>
        )}
      </div>
      )}

      {/* Face Recognition & Enrolled Gallery Card */}
      {isAiSubSectionVisible(activeSubSection, "faces") && (
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
              gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
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
                  <span
                    className="nvr-btn mono sm"
                  >
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
                      if (!file) {
                        setEnrollImageB64("");
                        setEnrollFileName("");
                        return;
                      }
                      setEnrollFileName(file.name);
                      const reader = new FileReader();
                      reader.onload = () => {
                        if (typeof reader.result === "string") {
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
                    className="nvr-btn danger mono sm"
                    aria-label={`Delete face profile ${prof.name}`}
                    onClick={() => handleDeleteProfile(prof.name)}
                  >
                    <Trash2 size={11} />
                  </button>
                </div>
              ))}
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
          {/* Alert Suppression for Known Residents (exclude_sub_labels) */}
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
      )}

      {/* License Plate Recognition (LPR) & Known Plates Card */}
      {isAiSubSectionVisible(activeSubSection, "lpr") && (
      <>
      {scopeDivider("global")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "RECONHECIMENTO DE PLACAS (LPR)" : "LICENSE PLATE RECOGNITION (LPR)"}</span>
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
              gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
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
                  onClick={() =>
                    updateLpr({ format_regex: DEFAULT_LPR_FORMAT_REGEX })
                  }
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
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
                {isPt ? "Modelo Detector de Placa (Opcional)" : "Plate Detector Model (Optional)"}
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
              <div
                className="mono"
                style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}
              >
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

          {/* Collapsible Diagnostic Drawer: Test LPR Scenario (Simulate Vehicle Pass) */}
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
                    className="nvr-input mono sm"
                    aria-label="Simulate LPR Plate" style={{ width: 110, fontFamily: "var(--font-mono)" }}
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
                  {isPt
                    ? "DIRETÓRIO DE PLACAS CONHECIDAS"
                    : "KNOWN PLATES REGISTRY"}
                </span>
                <span className="nvr-yaml-key-tag">lpr.known_plates</span>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
                <input
                  className="nvr-input mono sm"
                  aria-label="Known plate number" style={{ width: 120, fontFamily: "var(--font-mono)" }}
                  placeholder="ABC1D23"
                  value={newKnownPlate}
                  onChange={(e) => setNewKnownPlate(e.target.value.toUpperCase())}
                />
                <input
                  className="nvr-input sm"
                  aria-label="Known plate vehicle label" style={{ width: 190 }}
                  placeholder={isPt ? "Ex: SUV da Família" : "e.g. Family SUV"}
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
                    className="nvr-btn danger mono sm"
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
                          className="nvr-btn primary mono sm"
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
      )}

      {/* ROI State Classifier Card (Gate Open/Closed, Parking Spot Occupied, Anomaly Mode) */}
      {isAiSubSectionVisible(activeSubSection, "state_rois") && (
      <>
      <SettingsScopeDivider
        group="camera"
        scope={effectiveRoiCam}
        inspectorCam={effectiveRoiCam}
        cameraCount={scopeCameraCount}
        isPt={isPt}
      />
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "NOVO MONITOR DE ESTADO" : "NEW STATE MONITOR"}</span>
            <span className="nvr-yaml-key-tag">
              cameras.{effectiveRoiCam}.state_rois
            </span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            {scope === "global" && (
              <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "Câmera Alvo:" : "Target Camera:"}
                </span>
                <select
                  className="nvr-select mono sm"
                  aria-label="ROI Target Camera"
                  value={effectiveRoiCam}
                  onChange={(e) => setRoiTargetCam(e.target.value)}
                >
                  {(Object.keys(draftConfig.cameras || {}).length > 0
                    ? Object.keys(draftConfig.cameras || {})
                    : [effectiveRoiCam]
                  ).map((camKey) => (
                    <option key={camKey} value={camKey}>
                      {camKey.toUpperCase()}
                    </option>
                  ))}
                </select>
              </div>
            )}
            {onOpenVectorStudio && (
              <button
                type="button"
                className="nvr-btn mono sm"
                onClick={onOpenVectorStudio}
                title={
                  isPt
                    ? "Abrir Estúdio Vetorial (Aba 03) para desenhar o ROI diretamente sobre o vídeo ao vivo"
                    : "Open Vector Studio (Tab 03) to draw the ROI directly on the live camera video"
                }
              >
                {isPt
                  ? "Desenhar no Vídeo Ao Vivo (Aba 03) ↗"
                  : "Plot on Live Video (Tab 03) ↗"}
              </button>
            )}
          </div>
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
              gridTemplateColumns: "minmax(0, 1fr) auto",
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
                    : "e.g. Office Door, Garage Gate"
                }
                value={newRoiName}
                onChange={(e) => setNewRoiName(e.target.value)}
              />
            </div>

            {/* Visual 16:9 Viewport Crop Preview & Region Presets */}
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <svg
                width="64"
                height="36"
                viewBox="0 0 64 36"
                aria-label="ROI Crop Viewport Preview"
                style={{
                  background: "var(--bg-input)",
                  border: "1px solid var(--border-strong)",
                  borderRadius: 2,
                  flexShrink: 0,
                }}
              >
                <rect
                  x={previewRoiCrop[0] * 64}
                  y={previewRoiCrop[1] * 36}
                  width={Math.max(3, (previewRoiCrop[2] - previewRoiCrop[0]) * 64)}
                  height={Math.max(3, (previewRoiCrop[3] - previewRoiCrop[1]) * 36)}
                  fill="rgba(245, 158, 11, 0.3)"
                  stroke="var(--signal-motion)"
                  strokeWidth="1.2"
                />
              </svg>
              <div style={{ display: "flex", flexDirection: "column", gap: 3, minWidth: 0 }}>
                <span className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "PASSO 3 — REGIÃO NO QUADRO:" : "STEP 3 — FRAME REGION:"}{" "}
                  <strong>[{previewRoiCrop.map((v) => v.toFixed(2)).join(", ")}]</strong>
                </span>
                <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
                  {(
                    [
                      ["0.200,0.300,0.650,0.850", isPt ? "Centro" : "Center"],
                      ["0.050,0.200,0.450,0.900", isPt ? "Esquerda" : "Left"],
                      ["0.550,0.200,0.950,0.900", isPt ? "Direita" : "Right"],
                      ["0.150,0.500,0.850,0.950", isPt ? "Piso/Vaga" : "Lower/Spot"],
                    ] as const
                  ).map(([cropPreset, label]) => (
                    <button
                      key={cropPreset}
                      type="button"
                      className={`nvr-btn mono ${newRoiCrop === cropPreset ? "active" : ""} sm`}
                      onClick={() => setNewRoiCrop(cropPreset)}
                    >
                      {label}
                    </button>
                  ))}
                </div>
              </div>
            </div>

            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              <div
                className="mono"
                style={{
                  fontSize: "var(--text-xs)",
                  color: "var(--text-secondary)",
                }}
              >
                {isPt ? "PASSO 4 — REGRA DE ALERTA:" : "STEP 4 — ALERT RULE:"}
              </div>
              <div style={{ display: "flex", gap: 4, flexWrap: "wrap", alignItems: "center" }}>
                <button
                  type="button"
                  className={`nvr-btn mono ${!newRoiAnomalyState.trim() ? "active" : ""} sm`}
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
                      className={`nvr-btn mono ${isSel ? "active" : ""} sm`}
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
                      } sm`}
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
                  const cam = next.cameras[effectiveRoiCam];
                  if (!cam) return;
                  if (!cam.state_rois) cam.state_rois = {};
                  cam.state_rois[slug] = roiCfg;
                });
                notify(`Added ROI state classifier '${slug}' on ${effectiveRoiCam}`);
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
              {isPt
                ? "Ajustes Técnicos Avançados (Coordenadas & Histerese)"
                : "Advanced Technical Tuning (Coordinates & Hysteresis)"}
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
        </div>
      </div>

      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "MONITORES CONFIGURADOS" : "CONFIGURED MONITORS"}</span>
            <span className="nvr-yaml-key-tag">cameras.{effectiveRoiCam}.state_rois</span>
          </div>
          <span className="nvr-badge">
            {Object.keys(draftConfig.cameras?.[effectiveRoiCam]?.state_rois || {}).length}
          </span>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 10 }}>
          {Object.entries(draftConfig.cameras?.[effectiveRoiCam]?.state_rois || {}).length === 0 && (
            <div className="nvr-field-desc">
              {isPt
                ? "Nenhum monitor nesta câmera. Crie um ao lado."
                : "No monitors on this camera yet. Create one alongside."}
            </div>
          )}
          {Object.entries(draftConfig.cameras?.[effectiveRoiCam]?.state_rois || {}).length > 0 && (
            <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
              {Object.entries(draftConfig.cameras?.[effectiveRoiCam]?.state_rois || {}).map(
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
                    <div style={{ display: "flex", gap: 4 }}>
                      <button
                        type="button"
                        className="nvr-btn sm"
                        title={
                          isPt
                            ? `Carregar ROI ${rName} para edição`
                            : `Load ROI ${rName} into editor`
                        }
                        onClick={() => {
                          setNewRoiName(rName);
                          const c = rCfg.crop || [0.25, 0.25, 0.75, 0.75];
                          setNewRoiCrop(c.map((v) => Number(v).toFixed(3)).join(","));
                          setNewRoiClasses(
                            (rCfg.classes || ["closed", "open"]).join(", ")
                          );
                          setNewRoiIntervalSec(rCfg.interval_sec ?? 5);
                          setNewRoiHysteresis(rCfg.hysteresis ?? 3);
                          setNewRoiAnomalyState(rCfg.anomaly_state || "");
                          setNewRoiAnomalyTimeoutSec(rCfg.anomaly_timeout_sec ?? 300);
                        }}
                      >
                        <Edit3 size={11} />
                      </button>
                      <button
                        type="button"
                        className="nvr-btn danger sm"
                        title={`Remove ROI ${rName}`}
                        onClick={() =>
                          updateDraft((next) => {
                            if (next.cameras[effectiveRoiCam]?.state_rois) {
                              delete next.cameras[effectiveRoiCam].state_rois![rName];
                            }
                          })
                        }
                      >
                        <Trash2 size={11} />
                      </button>
                    </div>
                  </div>
                )
              )}
            </div>
          )}
        </div>
      </div>
      </>
      )}
    </div>
  );
};
