import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Camera,
  CheckCircle2,
  Copy,
  Cpu,
  Crosshair,
  FileCode,
  HardDrive,
  RotateCcw,
  Save,
  Shield,
  Sliders,
} from "lucide-react";
import { useI18n } from "../i18n";
import {
  CameraConfig,
  FfprobeItem,
  LineConfig,
  NinjaConfigResponse,
  StateRoiConfig,
} from "../types/nvr";
import { buildCleanYamlFromDraft } from "../utils/configYaml";
import { TacticalCameraPicker } from "../components/TacticalCameraPicker";
import { useNvrStore } from "../store/useNvrStore";
import { countChangedSections, formatUnsavedLabel } from "../utils/settingsUx";
import {
  ChannelsStreamsSection,
  ConsoleUiSection,
} from "./settings/ConsoleAndStreamsPanels";
import {
  AiDetectionObjectsSection,
  VisionZonesMotionSection,
} from "./settings/VisionAndAiPanels";
import { DatasetStudioSection } from "./settings/DatasetStudio";
import {
  CommsAuthUsersSection,
  RecordingReviewSection,
} from "./settings/RecordingAndSecurityPanels";

export type SettingsCategory =
  | "console"
  | "channels"
  | "vision"
  | "ai"
  | "dataset"
  | "recording"
  | "security"
  | "yaml";

export function normalizeZonePoint(
  pt: [number, number],
  detectWidth: number = 1280,
  detectHeight: number = 720
): [number, number] {
  const safeW =
    Number.isFinite(detectWidth) && detectWidth > 1 ? detectWidth : 1280;
  const safeH =
    Number.isFinite(detectHeight) && detectHeight > 1 ? detectHeight : 720;
  const rawX = Number.isFinite(pt?.[0]) ? pt[0] : 0;
  const rawY = Number.isFinite(pt?.[1]) ? pt[1] : 0;
  const isPixelSpace = rawX > 1.5 || rawY > 1.5;
  const normX = isPixelSpace ? rawX / safeW : rawX;
  const normY = isPixelSpace ? rawY / safeH : rawY;
  return [
    Number(Math.max(0, Math.min(1, normX)).toFixed(4)),
    Number(Math.max(0, Math.min(1, normY)).toFixed(4)),
  ];
}

export function deduplicatePolygonVertices(
  points: Array<[number, number]>,
  minVertexDist: number = 1e-4
): Array<[number, number]> {
  const safeMinDist =
    Number.isFinite(minVertexDist) && minVertexDist >= 0 ? minVertexDist : 1e-4;
  const out: Array<[number, number]> = [];
  for (const pt of points) {
    if (!Array.isArray(pt) || pt.length < 2) continue;
    if (!Number.isFinite(pt[0]) || !Number.isFinite(pt[1])) continue;
    const prev = out[out.length - 1];
    if (
      prev &&
      Math.hypot(pt[0] - prev[0], pt[1] - prev[1]) <= safeMinDist
    ) {
      continue;
    }
    out.push([pt[0], pt[1]]);
  }
  while (out.length >= 2) {
    const first = out[0];
    const last = out[out.length - 1];
    if (Math.hypot(first[0] - last[0], first[1] - last[1]) <= safeMinDist) {
      out.pop();
    } else {
      break;
    }
  }
  return out;
}

export function computePolygonSignedArea(
  points: Array<[number, number]>
): number {
  if (!Array.isArray(points) || points.length < 3) return 0;
  let sum = 0;
  for (let i = 0; i < points.length; i++) {
    const [x1, y1] = points[i];
    const [x2, y2] = points[(i + 1) % points.length];
    if (
      !Number.isFinite(x1) ||
      !Number.isFinite(y1) ||
      !Number.isFinite(x2) ||
      !Number.isFinite(y2)
    ) {
      return 0;
    }
    sum += x1 * y2 - x2 * y1;
  }
  return sum / 2;
}

function pointOnSegment(
  p: [number, number],
  q: [number, number],
  r: [number, number]
): boolean {
  return (
    r[0] >= Math.min(p[0], q[0]) - 1e-9 &&
    r[0] <= Math.max(p[0], q[0]) + 1e-9 &&
    r[1] >= Math.min(p[1], q[1]) - 1e-9 &&
    r[1] <= Math.max(p[1], q[1]) + 1e-9
  );
}

function segmentsProperlyIntersect(
  a: [number, number],
  b: [number, number],
  c: [number, number],
  d: [number, number]
): boolean {
  const cross = (
    p: [number, number],
    q: [number, number],
    r: [number, number]
  ) => (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0]);
  const o1 = cross(a, b, c);
  const o2 = cross(a, b, d);
  const o3 = cross(c, d, a);
  const o4 = cross(c, d, b);
  if (o1 * o2 < -1e-9 && o3 * o4 < -1e-9) {
    return true;
  }
  if (Math.abs(o1) <= 1e-9 && pointOnSegment(a, b, c)) return true;
  if (Math.abs(o2) <= 1e-9 && pointOnSegment(a, b, d)) return true;
  if (Math.abs(o3) <= 1e-9 && pointOnSegment(c, d, a)) return true;
  if (Math.abs(o4) <= 1e-9 && pointOnSegment(c, d, b)) return true;
  return false;
}

export function hasSelfIntersectingEdges(
  points: Array<[number, number]>
): boolean {
  if (!Array.isArray(points) || points.length < 3) return false;
  const n = points.length;
  for (let i = 0; i < n; i++) {
    const a = points[i];
    const b = points[(i + 1) % n];
    const cAdj = points[(i + 2) % n];
    const ux = b[0] - a[0];
    const uy = b[1] - a[1];
    const vx = cAdj[0] - b[0];
    const vy = cAdj[1] - b[1];
    const uLen = Math.hypot(ux, uy);
    const vLen = Math.hypot(vx, vy);
    if (uLen > 1e-7 && vLen > 1e-7) {
      const adjCross = (ux * vy - uy * vx) / (uLen * vLen);
      const adjDot = (ux * vx + uy * vy) / (uLen * vLen);
      if (Math.abs(adjCross) <= 1e-6 && adjDot < -1e-6) {
        return true;
      }
    }
    if (n < 4) continue;
    for (let j = i + 2; j < n; j++) {
      if (Math.hypot(a[0] - points[j][0], a[1] - points[j][1]) <= 1e-4) {
        return true;
      }
      if (i === 0 && j === n - 1) continue;
      const c = points[j];
      const d = points[(j + 1) % n];
      if (segmentsProperlyIntersect(a, b, c, d)) {
        return true;
      }
    }
  }
  return false;
}

export function isNonDegeneratePolygon(
  points: Array<[number, number]>,
  minArea: number = 1e-5,
  minSpan: number = 0.002
): boolean {
  const safeMinArea =
    Number.isFinite(minArea) && minArea > 0 ? minArea : 1e-5;
  const safeMinSpan =
    Number.isFinite(minSpan) && minSpan > 0 ? minSpan : 0.002;
  const deduped = deduplicatePolygonVertices(points);
  if (deduped.length < 3) return false;
  let minX = Infinity;
  let minY = Infinity;
  let maxX = -Infinity;
  let maxY = -Infinity;
  for (const [x, y] of deduped) {
    if (x < minX) minX = x;
    if (y < minY) minY = y;
    if (x > maxX) maxX = x;
    if (y > maxY) maxY = y;
  }
  if (maxX - minX < safeMinSpan || maxY - minY < safeMinSpan) {
    return false;
  }
  if (hasSelfIntersectingEdges(deduped)) {
    return false;
  }
  return Math.abs(computePolygonSignedArea(deduped)) >= safeMinArea;
}

export function sanitizePolygonCoordString(
  coordStr: string,
  detectWidth: number = 1280,
  detectHeight: number = 720
): string | null {
  if (typeof coordStr !== "string") return null;
  const trimmed = coordStr.trim();
  if (!trimmed) return null;
  const tokens = trimmed.split(",").map((s) => s.trim());
  if (
    tokens.length < 6 ||
    tokens.length % 2 !== 0 ||
    tokens.some((tok) => tok === "" || !Number.isFinite(Number(tok)))
  ) {
    return null;
  }
  const parts = tokens.map((tok) => Number(tok));
  const safeW =
    Number.isFinite(detectWidth) && detectWidth > 1 ? detectWidth : 1280;
  const safeH =
    Number.isFinite(detectHeight) && detectHeight > 1 ? detectHeight : 720;
  const isPixelSpace = parts.some((v) => v > 1.5);
  const rawPts: Array<[number, number]> = [];
  for (let i = 0; i < parts.length; i += 2) {
    const nx = isPixelSpace ? parts[i] / safeW : parts[i];
    const ny = isPixelSpace ? parts[i + 1] / safeH : parts[i + 1];
    rawPts.push([
      Number(Math.max(0, Math.min(1, nx)).toFixed(3)),
      Number(Math.max(0, Math.min(1, ny)).toFixed(3)),
    ]);
  }
  const deduped = deduplicatePolygonVertices(rawPts);
  if (!isNonDegeneratePolygon(deduped)) {
    return null;
  }
  return deduped.map(([x, y]) => `${x.toFixed(3)},${y.toFixed(3)}`).join(",");
}

export function sanitizeMaskPolygonsValue(
  maskVal: unknown,
  detectWidth: number = 1280,
  detectHeight: number = 720
): { sanitized: unknown; rejectedCount: number } {
  const isUnsafeObjectKey = (k: string) =>
    k === "__proto__" || k === "constructor" || k === "prototype";

  if (maskVal === null || maskVal === undefined) {
    return { sanitized: maskVal, rejectedCount: 0 };
  }
  if (typeof maskVal === "string") {
    if (!maskVal.trim()) return { sanitized: "", rejectedCount: 0 };
    const clean = sanitizePolygonCoordString(maskVal, detectWidth, detectHeight);
    if (!clean) return { sanitized: [], rejectedCount: 1 };
    return { sanitized: clean, rejectedCount: 0 };
  }
  if (Array.isArray(maskVal)) {
    if (maskVal.length === 0) return { sanitized: [], rejectedCount: 0 };
    let rejectedCount = 0;
    const out: unknown[] = [];
    for (const item of maskVal) {
      if (typeof item === "string") {
        if (!item.trim()) continue;
        const clean = sanitizePolygonCoordString(
          item,
          detectWidth,
          detectHeight
        );
        if (clean) {
          out.push(clean);
        } else {
          rejectedCount += 1;
        }
      } else if (Array.isArray(item)) {
        const flat = item.flat();
        const hasInvalidElem = flat.some(
          (v) =>
            v === null ||
            v === undefined ||
            typeof v === "boolean" ||
            (typeof v === "string" && v.trim() === "") ||
            !Number.isFinite(Number(v))
        );
        if (hasInvalidElem) {
          rejectedCount += 1;
          continue;
        }
        const clean = sanitizePolygonCoordString(
          flat.map((v) => Number(v)).join(","),
          detectWidth,
          detectHeight
        );
        if (clean) {
          out.push(clean);
        } else {
          rejectedCount += 1;
        }
      } else if (
        item &&
        typeof item === "object" &&
        "coordinates" in (item as Record<string, unknown>)
      ) {
        const objItem = item as Record<string, unknown>;
        const rawCoords =
          typeof objItem.coordinates === "string" ? objItem.coordinates : "";
        const clean = sanitizePolygonCoordString(
          rawCoords,
          detectWidth,
          detectHeight
        );
        if (clean) {
          out.push({ ...objItem, coordinates: clean });
        } else {
          rejectedCount += 1;
        }
      } else {
        rejectedCount += 1;
      }
    }
    return { sanitized: out, rejectedCount };
  }
  if (typeof maskVal === "object") {
    const objVal = maskVal as Record<string, unknown>;
    const nextObj: Record<string, unknown> = {};
    let rejectedCount = 0;
    const metaKeys = new Set(["mode", "style", "pixelate", "block_size"]);
    for (const [k, v] of Object.entries(objVal)) {
      if (isUnsafeObjectKey(k)) continue;
      if (metaKeys.has(k)) {
        nextObj[k] = v;
        continue;
      }
      if (typeof v === "string") {
        if (!v.trim()) continue;
        const clean = sanitizePolygonCoordString(
          v,
          detectWidth,
          detectHeight
        );
        if (clean) {
          nextObj[k] = clean;
        } else {
          rejectedCount += 1;
        }
      } else if (Array.isArray(v)) {
        const sub = sanitizeMaskPolygonsValue(v, detectWidth, detectHeight);
        rejectedCount += sub.rejectedCount;
        nextObj[k] = sub.sanitized;
      } else if (
        v &&
        typeof v === "object" &&
        "coordinates" in (v as Record<string, unknown>)
      ) {
        const subObj = v as Record<string, unknown>;
        const rawCoords =
          typeof subObj.coordinates === "string" ? subObj.coordinates : "";
        const clean = sanitizePolygonCoordString(
          rawCoords,
          detectWidth,
          detectHeight
        );
        if (clean) {
          nextObj[k] = { ...subObj, coordinates: clean };
        } else {
          rejectedCount += 1;
        }
      } else {
        nextObj[k] = v;
      }
    }
    return { sanitized: nextObj, rejectedCount };
  }
  return { sanitized: maskVal, rejectedCount: 0 };
}

export function sanitizeTripwireLine(
  line: LineConfig,
  detectWidth: number = 1280,
  detectHeight: number = 720
): LineConfig | null {
  if (!line || !Array.isArray(line.points) || line.points.length < 4) {
    return null;
  }
  const [x1, y1, x2, y2] = line.points;
  if (
    !Number.isFinite(x1) ||
    !Number.isFinite(y1) ||
    !Number.isFinite(x2) ||
    !Number.isFinite(y2)
  ) {
    return null;
  }
  const safeW =
    Number.isFinite(detectWidth) && detectWidth > 1 ? detectWidth : 1280;
  const safeH =
    Number.isFinite(detectHeight) && detectHeight > 1 ? detectHeight : 720;
  const isPixelSpace = x1 > 1.5 || y1 > 1.5 || x2 > 1.5 || y2 > 1.5;
  const nx1 = Number(
    Math.max(0, Math.min(1, isPixelSpace ? x1 / safeW : x1)).toFixed(4)
  );
  const ny1 = Number(
    Math.max(0, Math.min(1, isPixelSpace ? y1 / safeH : y1)).toFixed(4)
  );
  const nx2 = Number(
    Math.max(0, Math.min(1, isPixelSpace ? x2 / safeW : x2)).toFixed(4)
  );
  const ny2 = Number(
    Math.max(0, Math.min(1, isPixelSpace ? y2 / safeH : y2)).toFixed(4)
  );
  if (Math.hypot(nx2 - nx1, ny2 - ny1) < 0.005) {
    return null;
  }
  return {
    ...line,
    points: [nx1, ny1, nx2, ny2],
  };
}

export function sanitizeStateRoiConfig(
  roi: StateRoiConfig,
  detectWidth: number = 1280,
  detectHeight: number = 720
): StateRoiConfig {
  const rawCrop = Array.isArray(roi?.crop) ? roi.crop : [0.25, 0.25, 0.75, 0.75];
  const [rx1, ry1, rx2, ry2] = [
    Number.isFinite(rawCrop[0]) ? rawCrop[0] : 0.25,
    Number.isFinite(rawCrop[1]) ? rawCrop[1] : 0.25,
    Number.isFinite(rawCrop[2]) ? rawCrop[2] : 0.75,
    Number.isFinite(rawCrop[3]) ? rawCrop[3] : 0.75,
  ];
  const safeW =
    Number.isFinite(detectWidth) && detectWidth > 1 ? detectWidth : 1280;
  const safeH =
    Number.isFinite(detectHeight) && detectHeight > 1 ? detectHeight : 720;
  const isPixelSpace = rx1 > 1.5 || ry1 > 1.5 || rx2 > 1.5 || ry2 > 1.5;
  const cx1 = Math.max(0, Math.min(1, isPixelSpace ? rx1 / safeW : rx1));
  const cy1 = Math.max(0, Math.min(1, isPixelSpace ? ry1 / safeH : ry1));
  const cx2 = Math.max(0, Math.min(1, isPixelSpace ? rx2 / safeW : rx2));
  const cy2 = Math.max(0, Math.min(1, isPixelSpace ? ry2 / safeH : ry2));
  const minX = Math.min(cx1, cx2);
  const minY = Math.min(cy1, cy2);
  const maxX = Math.max(cx1, cx2);
  const maxY = Math.max(cy1, cy2);
  const spanX = Math.max(0.01, maxX - minX);
  const spanY = Math.max(0.01, maxY - minY);
  const x1 = Number(Math.max(0, Math.min(1 - spanX, minX)).toFixed(4));
  const y1 = Number(Math.max(0, Math.min(1 - spanY, minY)).toFixed(4));
  const x2 = Number(Math.min(1, x1 + spanX).toFixed(4));
  const y2 = Number(Math.min(1, y1 + spanY).toFixed(4));
  return {
    ...roi,
    crop: [x1, y1, x2, y2],
  };
}

export function sanitizeCameraGeometryDraft(camCfg: CameraConfig): {
  cleaned: CameraConfig;
  rejectedCount: number;
} {
  const isUnsafeKey = (k: string) =>
    k === "__proto__" || k === "constructor" || k === "prototype";
  const detW = camCfg?.detect?.width || 1280;
  const detH = camCfg?.detect?.height || 720;
  let rejectedCount = 0;
  const cleaned: CameraConfig = { ...camCfg };

  if (cleaned.zones && typeof cleaned.zones === "object") {
    const nextZones: Record<string, CameraConfig["zones"][string]> = {};
    for (const [zName, zCfg] of Object.entries(cleaned.zones)) {
      if (isUnsafeKey(zName)) continue;
      const cleanCoords = sanitizePolygonCoordString(
        zCfg?.coordinates || "",
        detW,
        detH
      );
      if (cleanCoords) {
        nextZones[zName] = { ...zCfg, coordinates: cleanCoords };
      } else {
        rejectedCount += 1;
      }
    }
    cleaned.zones = nextZones;
  }

  if (cleaned.motion) {
    const mRes = sanitizeMaskPolygonsValue(cleaned.motion.mask, detW, detH);
    rejectedCount += mRes.rejectedCount;
    cleaned.motion = { ...cleaned.motion, mask: mRes.sanitized };
  }

  if (cleaned.objects) {
    const nextObjects = { ...cleaned.objects };
    if (nextObjects.mask !== undefined) {
      const oRes = sanitizeMaskPolygonsValue(nextObjects.mask, detW, detH);
      rejectedCount += oRes.rejectedCount;
      nextObjects.mask = oRes.sanitized;
    }
    if (nextObjects.filters && typeof nextObjects.filters === "object") {
      const nextFilters: NonNullable<CameraConfig["objects"]>["filters"] = {};
      for (const [fLabel, fCfg] of Object.entries(nextObjects.filters)) {
        if (isUnsafeKey(fLabel)) continue;
        if (fCfg && fCfg.mask !== undefined) {
          const fRes = sanitizeMaskPolygonsValue(fCfg.mask, detW, detH);
          rejectedCount += fRes.rejectedCount;
          nextFilters[fLabel] = { ...fCfg, mask: fRes.sanitized };
        } else {
          nextFilters[fLabel] = fCfg;
        }
      }
      nextObjects.filters = nextFilters;
    }
    cleaned.objects = nextObjects;
  }

  if (cleaned.privacy_mask !== undefined) {
    const pRes = sanitizeMaskPolygonsValue(cleaned.privacy_mask, detW, detH);
    rejectedCount += pRes.rejectedCount;
    cleaned.privacy_mask = pRes.sanitized;
  }

  if (cleaned.lines && typeof cleaned.lines === "object") {
    const nextLines: Record<string, LineConfig> = {};
    for (const [lName, lCfg] of Object.entries(cleaned.lines)) {
      if (isUnsafeKey(lName)) continue;
      const cleanLine = sanitizeTripwireLine(lCfg, detW, detH);
      if (cleanLine) {
        nextLines[lName] = cleanLine;
      } else {
        rejectedCount += 1;
      }
    }
    cleaned.lines = nextLines;
  }

  if (cleaned.state_rois && typeof cleaned.state_rois === "object") {
    const nextRois: Record<string, StateRoiConfig> = {};
    for (const [rName, rCfg] of Object.entries(cleaned.state_rois)) {
      if (isUnsafeKey(rName)) continue;
      nextRois[rName] = sanitizeStateRoiConfig(rCfg, detW, detH);
    }
    cleaned.state_rois = nextRois;
  }

  return { cleaned, rejectedCount };
}

export function sanitizeConfigGeometryDraft(
  cfg: NinjaConfigResponse
): { cleaned: NinjaConfigResponse; rejectedCount: number } {
  const isUnsafeKey = (k: string) =>
    k === "__proto__" || k === "constructor" || k === "prototype";
  const cloned: NinjaConfigResponse = JSON.parse(JSON.stringify(cfg));
  let rejectedCount = 0;
  const globalW = cloned.detect?.width || 1280;
  const globalH = cloned.detect?.height || 720;

  if (cloned.motion && cloned.motion.mask !== undefined) {
    const gmRes = sanitizeMaskPolygonsValue(
      cloned.motion.mask,
      globalW,
      globalH
    );
    rejectedCount += gmRes.rejectedCount;
    cloned.motion = { ...cloned.motion, mask: gmRes.sanitized };
  }

  if (cloned.objects) {
    const nextGlobalObjects = { ...cloned.objects };
    if (nextGlobalObjects.mask !== undefined) {
      const goRes = sanitizeMaskPolygonsValue(
        nextGlobalObjects.mask,
        globalW,
        globalH
      );
      rejectedCount += goRes.rejectedCount;
      nextGlobalObjects.mask = goRes.sanitized;
    }
    if (
      nextGlobalObjects.filters &&
      typeof nextGlobalObjects.filters === "object"
    ) {
      const nextGlobalFilters: NonNullable<
        NinjaConfigResponse["objects"]
      >["filters"] = {};
      for (const [fLabel, fCfg] of Object.entries(nextGlobalObjects.filters)) {
        if (isUnsafeKey(fLabel)) continue;
        if (fCfg && fCfg.mask !== undefined) {
          const gfRes = sanitizeMaskPolygonsValue(
            fCfg.mask,
            globalW,
            globalH
          );
          rejectedCount += gfRes.rejectedCount;
          nextGlobalFilters[fLabel] = { ...fCfg, mask: gfRes.sanitized };
        } else {
          nextGlobalFilters[fLabel] = fCfg;
        }
      }
      nextGlobalObjects.filters = nextGlobalFilters;
    }
    cloned.objects = nextGlobalObjects;
  }

  if (cloned.cameras && typeof cloned.cameras === "object") {
    for (const [camName, camCfg] of Object.entries(cloned.cameras)) {
      if (isUnsafeKey(camName)) continue;
      const res = sanitizeCameraGeometryDraft(camCfg);
      cloned.cameras[camName] = res.cleaned;
      rejectedCount += res.rejectedCount;
    }
  }
  return { cleaned: cloned, rejectedCount };
}

interface ZonesWorkspaceProps {
  cameraNames: string[];
  config: NinjaConfigResponse | null;
  setConfig: (cfg: NinjaConfigResponse | null) => void;
  zonesSubTab: "visual" | "yaml";
  setZonesSubTab: (tab: "visual" | "yaml") => void;
  inspectorCam: string;
  setInspectorCam: (cam: string) => void;
  rawYaml: string;
  setRawYaml: (yaml: string) => void;
  draftPoints: Array<[number, number]>;
  setDraftPoints: React.Dispatch<React.SetStateAction<Array<[number, number]>>>;
  ffprobeInfo: FfprobeItem[] | null;
  handleSaveConfig: (saveOption: "saveonly" | "restart") => Promise<void>;
  notify: (msg: string) => void;
}

export const ZonesWorkspace: React.FC<ZonesWorkspaceProps> = ({
  cameraNames,
  config,
  setConfig,
  zonesSubTab,
  setZonesSubTab,
  inspectorCam,
  setInspectorCam,
  rawYaml,
  setRawYaml,
  draftPoints,
  setDraftPoints,
  ffprobeInfo,
  handleSaveConfig,
  notify,
}) => {
  const { t, locale } = useI18n();
  const isPt = locale === "pt";
  const [activeCategory, setActiveCategory] = useState<SettingsCategory>(() =>
    zonesSubTab === "yaml" ? "yaml" : "vision"
  );
  const [scope, setScope] = useState<"global" | string>("camera");
  const [draftConfig, setDraftConfig] = useState<NinjaConfigResponse | null>(null);
  const [baselineYaml, setBaselineYaml] = useState<string>("");
  const [isDirty, setIsDirty] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const draftRevisionRef = useRef<number>(0);
  const isMountedRef = useRef<boolean>(true);
  const showYamlKeys = useNvrStore((s) => s.showYamlKeys);
  const setShowYamlKeys = useNvrStore((s) => s.setShowYamlKeys);

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    if (zonesSubTab === "yaml" && activeCategory !== "yaml") {
      setActiveCategory("yaml");
    } else if (zonesSubTab === "visual" && activeCategory === "yaml") {
      setActiveCategory("vision");
    }
  }, [zonesSubTab, activeCategory]);

  // Sync incoming config into draftConfig when no local unsaved changes exist
  useEffect(() => {
    if (config && !isDirty && !isSaving) {
      setDraftConfig(JSON.parse(JSON.stringify(config)));
    }
  }, [config, isDirty, isSaving]);

  useEffect(() => {
    if (rawYaml && !isDirty && !isSaving && baselineYaml !== rawYaml) {
      setBaselineYaml(rawYaml);
    }
  }, [rawYaml, baselineYaml, isDirty, isSaving]);

  const allCamKeys = Object.keys(draftConfig?.cameras || {});
  const displayCams = allCamKeys.length > 0 ? allCamKeys : cameraNames;
  const effectiveInspectorCam =
    displayCams.length > 0 && !displayCams.includes(inspectorCam)
      ? displayCams[0]
      : inspectorCam;

  useEffect(() => {
    if (
      displayCams.length > 0 &&
      inspectorCam &&
      !displayCams.includes(inspectorCam)
    ) {
      setInspectorCam(displayCams[0]);
      setDraftPoints([]);
    }
  }, [displayCams, inspectorCam, setInspectorCam, setDraftPoints]);

  const currentCamDetectW =
    draftConfig?.cameras?.[effectiveInspectorCam]?.detect?.width || 1280;
  const currentCamDetectH =
    draftConfig?.cameras?.[effectiveInspectorCam]?.detect?.height || 720;

  const setSanitizedDraftPoints: React.Dispatch<
    React.SetStateAction<Array<[number, number]>>
  > = useCallback(
    (action) => {
      setDraftPoints((prev) => {
        const rawNext = typeof action === "function" ? action(prev) : action;
        if (!Array.isArray(rawNext) || rawNext.length === 0) return [];
        const normalized = rawNext.map((pt) =>
          normalizeZonePoint(pt, currentCamDetectW, currentCamDetectH)
        );
        return deduplicatePolygonVertices(normalized, 0.0005);
      });
    },
    [setDraftPoints, currentCamDetectW, currentCamDetectH]
  );

  const updateDraft = (updater: (next: NinjaConfigResponse) => void) => {
    if (!draftConfig) return;
    const cloned: NinjaConfigResponse = JSON.parse(JSON.stringify(draftConfig));
    updater(cloned);
    const { cleaned, rejectedCount } = sanitizeConfigGeometryDraft(cloned);
    if (rejectedCount > 0) {
      notify(
        isPt
          ? "Polígono ou linha degenerada (colinear / área zero) ignorada para proteger máscaras de privacidade"
          : "Rejected degenerate (collinear / zero-area) polygon or line to protect backend masks"
      );
    }
    draftRevisionRef.current += 1;
    setDraftConfig(cleaned);
    const nextYaml = buildCleanYamlFromDraft(cleaned, baselineYaml || rawYaml);
    setRawYaml(nextYaml);
    setIsDirty(true);
  };

  const handleDiscardDraft = () => {
    draftRevisionRef.current += 1;
    if (config) {
      setDraftConfig(JSON.parse(JSON.stringify(config)));
    }
    if (baselineYaml) {
      setRawYaml(baselineYaml);
    }
    setDraftPoints([]);
    setIsDirty(false);
    notify(
      isPt
        ? "Alterações descartadas e restauradas ao estado salvo"
        : "Reverted unsaved configuration changes"
    );
  };

  const handleSaveStructured = async (restartAfter = false) => {
    if (!draftConfig || isSaving) return;
    const saveRev = draftRevisionRef.current;
    const { cleaned: payloadConfig, rejectedCount } =
      sanitizeConfigGeometryDraft(draftConfig);
    if (rejectedCount > 0) {
      notify(
        isPt
          ? "Polígono degenerado removido antes de salvar"
          : "Stripped degenerate polygon before saving configuration"
      );
    }
    setIsSaving(true);
    try {
      const res = await fetch("/api/config/structured", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify(payloadConfig),
      });

      if (res.status === 404) {
        // Fallback for pods running an older backend binary without /api/config/structured
        const generatedYaml = buildCleanYamlFromDraft(
          payloadConfig,
          baselineYaml || rawYaml
        );
        const saveRes = await fetch(
          `/api/config/save?save_option=${restartAfter ? "restart" : "saveonly"}`,
          {
            method: "POST",
            headers: { "Content-Type": "text/plain" },
            credentials: "include",
            body: generatedYaml,
          }
        );
        const saveData = await saveRes.json().catch(() => ({}));
        if (!isMountedRef.current) return;
        if (saveRes.ok && saveData.success !== false) {
          setConfig(JSON.parse(JSON.stringify(payloadConfig)));
          setBaselineYaml(generatedYaml);
          if (draftRevisionRef.current === saveRev) {
            setRawYaml(generatedYaml);
            setIsDirty(false);
          }
          if (restartAfter) {
            await fetch("/api/restart", { method: "POST", credentials: "include" });
            if (!isMountedRef.current) return;
          }
          notify(
            restartAfter
              ? isPt
                ? "Configuração salva e NVR reiniciado"
                : "Configuration saved and NVR restart triggered"
              : isPt
              ? "Configuração salva em config.yml"
              : "Configuration saved to config.yml"
          );
        } else {
          notify(saveData.message || "Failed to save configuration");
        }
        return;
      }

      const data = await res.json().catch(() => ({}));
      if (!isMountedRef.current) return;
      if (res.ok && data.success !== false) {
        const nextSavedConfig: NinjaConfigResponse =
          data.config || payloadConfig;
        setConfig(JSON.parse(JSON.stringify(nextSavedConfig)));
        if (draftRevisionRef.current === saveRev) {
          setDraftConfig(JSON.parse(JSON.stringify(nextSavedConfig)));
        }
        const nextSavedYaml =
          typeof data.raw_yaml === "string" && data.raw_yaml.trim()
            ? data.raw_yaml
            : buildCleanYamlFromDraft(
                nextSavedConfig,
                baselineYaml || rawYaml
              );
        setBaselineYaml(nextSavedYaml);
        if (draftRevisionRef.current === saveRev) {
          setRawYaml(nextSavedYaml);
          setIsDirty(false);
        }
        if (restartAfter) {
          await fetch("/api/restart", { method: "POST", credentials: "include" });
          if (!isMountedRef.current) return;
          notify(
            isPt
              ? "Configuração salva e pipeline reiniciado"
              : "Structured configuration saved and pipeline restart triggered"
          );
        } else {
          notify(
            isPt
              ? "Configuração salva em config.yml"
              : "Structured configuration saved to config.yml"
          );
        }
      } else {
        notify(data.message || "Failed to save structured configuration");
      }
    } catch {
      if (isMountedRef.current) {
        notify("Network error while saving structured configuration");
      }
    } finally {
      if (isMountedRef.current) {
        setIsSaving(false);
      }
    }
  };

  const handleSaveRawYaml = async (saveOption: "saveonly" | "restart") => {
    if (isSaving) return;
    const saveRev = draftRevisionRef.current;
    const savedYamlSnapshot = rawYaml;
    setIsSaving(true);
    try {
      await handleSaveConfig(saveOption);
      if (!isMountedRef.current) return;
      setBaselineYaml(savedYamlSnapshot);
      if (draftRevisionRef.current === saveRev) {
        setIsDirty(false);
      }
    } finally {
      if (isMountedRef.current) {
        setIsSaving(false);
      }
    }
  };

  const switchInspectorCamera = (cam: string) => {
    setInspectorCam(cam);
    setDraftPoints([]);
  };

  const selectCategory = (cat: SettingsCategory) => {
    setActiveCategory(cat);
    setZonesSubTab(cat === "yaml" ? "yaml" : "visual");
    setDraftPoints([]);
  };

  const unsavedCount = useMemo(
    () => (isDirty && config && draftConfig ? countChangedSections(config, draftConfig) : 0),
    [isDirty, config, draftConfig]
  );
  const unsavedLabel = isDirty
    ? activeCategory === "yaml" || unsavedCount === 0
      ? isPt
        ? "NÃO SALVO"
        : "UNSAVED"
      : formatUnsavedLabel(unsavedCount, isPt)
    : "";

  const supportsGlobalAndCamScope =
    activeCategory === "ai" || activeCategory === "recording";
  const supportsCameraOnlyBar =
    activeCategory === "channels" || activeCategory === "vision";

  const categories: Array<{
    id: SettingsCategory;
    num: string;
    title: string;
    sub: string;
    icon: React.ReactNode;
  }> = [
    {
      id: "console",
      num: "01",
      title: isPt ? "Console & Interface" : "Console & UI",
      sub: isPt ? "Layout, HUD & Idioma" : "Layout, HUD & Regional",
      icon: <Sliders size={14} />,
    },
    {
      id: "channels",
      num: "02",
      title: isPt ? "Canais & Streams" : "Channels & Streams",
      sub: `${displayCams.length} ${isPt ? "Câmeras" : "Cameras"} · go2rtc`,
      icon: <Camera size={14} />,
    },
    {
      id: "vision",
      num: "03",
      title: isPt ? "Zonas, Máscaras & Mov." : "Zones, Masks & Motion",
      sub: isPt ? "Estúdio Vetorial & Tuner" : "Vector Studio & Tuner",
      icon: <Crosshair size={14} />,
    },
    {
      id: "ai",
      num: "04",
      title: isPt ? "IA & Filtros de Objeto" : "AI & Object Filters",
      sub: "OpenVINO & ByteTrack",
      icon: <Cpu size={14} />,
    },
    {
      id: "dataset",
      num: "05",
      title: isPt ? "Estúdio de Dataset IA" : "AI Dataset Studio",
      sub: isPt ? "Auto-Labeler YOLO & .onnx" : "YOLO Auto-Labeler & .onnx",
      icon: <Cpu size={14} />,
    },
    {
      id: "recording",
      num: "06",
      title: isPt ? "Gravação & Revisão" : "Recording & Review",
      sub: isPt ? "Retenção & Regras de Alerta" : "Retention & Alert Rules",
      icon: <HardDrive size={14} />,
    },
    {
      id: "security",
      num: "07",
      title: isPt ? "Comms, Auth & Usuários" : "Comms, Auth & Users",
      sub: isPt ? "MQTT, TLS & Contas" : "MQTT, TLS & Accounts",
      icon: <Shield size={14} />,
    },
    {
      id: "yaml",
      num: "08",
      title: "Raw config.yml",
      sub: isPt ? "Editor YAML Direto" : "Direct YAML Studio",
      icon: <FileCode size={14} />,
    },
  ];

  return (
    <div className="nvr-settings-shell">
      {/* Left Tactical Category Navigation Rail */}
      <aside className="nvr-settings-rail" aria-label="Settings Categories">
        <div className="nvr-panel-header">
          <span>{isPt ? "BANCADA DE CONFIGURAÇÕES" : "SETTINGS WORKBENCH"}</span>
        </div>
        {categories.map((cat) => {
          const isActive = activeCategory === cat.id;
          return (
            <button
              key={cat.id}
              type="button"
              className={`nvr-settings-nav-item ${isActive ? "active" : ""}`}
              onClick={() => selectCategory(cat.id)}
            >
              <div style={{ display: "flex", alignItems: "center", gap: 9, minWidth: 0, flex: 1 }}>
                <span
                  className="mono"
                  style={{
                    fontSize: "var(--text-xs)",
                    color: isActive
                      ? "var(--signal-accent-text)"
                      : "var(--text-muted)",
                    fontWeight: 700,
                    flexShrink: 0,
                  }}
                >
                  {cat.num}
                </span>
                <div style={{ minWidth: 0, flex: 1 }}>
                  <div
                    style={{
                      fontSize: "var(--text-sm)",
                      fontWeight: 600,
                      color: isActive
                        ? "var(--text-primary)"
                        : "var(--text-secondary)",
                      whiteSpace: "nowrap",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                    }}
                  >
                    {cat.title}
                  </div>
                  <div
                    className="mono"
                    style={{
                      fontSize: "var(--text-xs)",
                      color: "var(--text-muted)",
                      whiteSpace: "nowrap",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                    }}
                  >
                    {cat.sub}
                  </div>
                </div>
              </div>
              {cat.icon}
            </button>
          );
        })}
      </aside>

      {/* Right Main Configuration Stage */}
      <section className={`nvr-settings-stage ${showYamlKeys ? "show-yaml-keys" : ""}`}>
        {/* Contextual Scope & Save Action Header */}
        <div className="nvr-settings-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span
              className="mono"
              style={{
                fontSize: "var(--text-xs)",
                fontWeight: 700,
                color: "var(--text-secondary)",
                textTransform: "uppercase",
              }}
            >
              {categories.find((c) => c.id === activeCategory)?.num} //{" "}
              {categories.find((c) => c.id === activeCategory)?.title}
            </span>

            {supportsGlobalAndCamScope && (
              <>
                <span className="nvr-divider" aria-hidden="true" />
                <button
                  type="button"
                  className={`nvr-btn mono ${scope === "global" ? "active" : ""} sm`}
                  onClick={() => {
                    setScope("global");
                    setDraftPoints([]);
                  }}
                >
                  {isPt ? "PADRÃO GLOBAL" : "GLOBAL DEFAULTS"}
                </button>
                <TacticalCameraPicker
                  cameras={displayCams}
                  selectedCamera={scope === "global" ? "" : effectiveInspectorCam}
                  onSelect={(cam) => {
                    setScope("camera");
                    switchInspectorCamera(cam);
                  }}
                  cameraGroups={draftConfig?.camera_groups}
                  ariaLabel="Settings camera scope selector"
                />
              </>
            )}

            {supportsCameraOnlyBar && (
              <>
                <span className="nvr-divider" aria-hidden="true" />
                <TacticalCameraPicker
                  cameras={displayCams}
                  selectedCamera={effectiveInspectorCam}
                  onSelect={(cam) => switchInspectorCamera(cam)}
                  cameraGroups={draftConfig?.camera_groups}
                  ariaLabel="Settings channel selector"
                />
                <a
                  href={`/api/camera-proxy/${encodeURIComponent(effectiveInspectorCam)}/`}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="nvr-btn mono"
                  style={{
                    height: 24,
                    padding: "0 8px",
                    fontSize: "var(--text-xs)",
                    textDecoration: "none",
                    display: "inline-flex",
                    alignItems: "center",
                  }}
                  title="Open camera's native Web UI via authenticated SSRF-safe reverse proxy"
                >
                  Open Camera Web UI ↗
                </a>
              </>
            )}
          </div>

          {/* Action Buttons */}
          <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
            {activeCategory === "console" ? (
              <span className="nvr-badge live">
                <CheckCircle2 size={11} />
                {isPt ? "SALVAMENTO AUTOMÁTICO" : "AUTO-SAVED IN BROWSER"}
              </span>
            ) : activeCategory === "yaml" ? (
              <>
                {unsavedLabel && (
                  <span className="nvr-badge motion" role="status">
                    {unsavedLabel}
                  </span>
                )}
                {isDirty && (
                  <button
                    type="button"
                    className="nvr-btn"
                    onClick={handleDiscardDraft}
                    disabled={isSaving}
                  >
                    <RotateCcw size={12} />
                    {isPt ? "Descartar" : "Discard"}
                  </button>
                )}
                <button
                  type="button"
                  className="nvr-btn"
                  onClick={() => {
                    navigator.clipboard?.writeText(rawYaml);
                    notify("Copied config.yml to clipboard");
                  }}
                >
                  <Copy size={12} />
                  {t.zones.copyYaml}
                </button>
                <button
                  type="button"
                  className="nvr-btn primary"
                  disabled={isSaving}
                  onClick={() => handleSaveRawYaml("saveonly")}
                >
                  {t.zones.saveOnly}
                </button>
                <button
                  type="button"
                  className="nvr-btn success"
                  disabled={isSaving}
                  onClick={() => handleSaveRawYaml("restart")}
                >
                  {t.zones.saveAndRestart}
                </button>
              </>
            ) : (
              <>
                <button
                  type="button"
                  className={`nvr-btn mono ${showYamlKeys ? "active" : ""}`}
                  aria-pressed={showYamlKeys}
                  onClick={() => setShowYamlKeys(!showYamlKeys)}
                  title={
                    isPt
                      ? "Mostrar as chaves do config.yml ao lado de cada configuração"
                      : "Show the config.yml key next to each setting"
                  }
                >
                  {isPt ? "Chaves YAML" : "YAML keys"}
                </button>
                {unsavedLabel && (
                  <span className="nvr-badge motion" role="status">
                    {unsavedLabel}
                  </span>
                )}
                {isDirty && (
                  <button
                    type="button"
                    className="nvr-btn"
                    onClick={handleDiscardDraft}
                    disabled={isSaving}
                  >
                    <RotateCcw size={12} />
                    {isPt ? "Descartar" : "Discard"}
                  </button>
                )}
                <button
                  type="button"
                  className="nvr-btn primary"
                  onClick={() => handleSaveStructured(false)}
                  disabled={isSaving}
                >
                  <Save size={12} />
                  {t.zones.saveOnly}
                </button>
                <button
                  type="button"
                  className="nvr-btn success"
                  onClick={() => handleSaveStructured(true)}
                  disabled={isSaving}
                >
                  {t.zones.saveAndRestart}
                </button>
              </>
            )}
          </div>
        </div>

        {/* Main Body Content */}
        {activeCategory === "yaml" ? (
          <div style={{ flex: 1, display: "flex", flexDirection: "column" }}>
            <textarea
              className="nvr-code-editor"
              value={rawYaml}
              onChange={(e) => {
                draftRevisionRef.current += 1;
                setRawYaml(e.target.value);
                setIsDirty(true);
              }}
              spellCheck={false}
              aria-label="config.yml editor"
            />
          </div>
        ) : activeCategory === "vision" && draftConfig ? (
          <VisionZonesMotionSection
            draftConfig={draftConfig}
            updateDraft={updateDraft}
            scope={effectiveInspectorCam}
            inspectorCam={effectiveInspectorCam}
            draftPoints={draftPoints}
            setDraftPoints={setSanitizedDraftPoints}
            notify={notify}
          />
        ) : (
          <div className="nvr-settings-body">
            {activeCategory === "console" && <ConsoleUiSection notify={notify} />}

            {draftConfig && activeCategory === "channels" && (
              <ChannelsStreamsSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                inspectorCam={effectiveInspectorCam}
                setInspectorCam={switchInspectorCamera}
                ffprobeInfo={ffprobeInfo}
                notify={notify}
              />
            )}

            {draftConfig && activeCategory === "ai" && (
              <AiDetectionObjectsSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                scope={scope === "global" ? "global" : effectiveInspectorCam}
                inspectorCam={effectiveInspectorCam}
                notify={notify}
                onOpenVectorStudio={() => selectCategory("vision")}
              />
            )}

            {activeCategory === "dataset" && (
              <DatasetStudioSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                notify={notify}
              />
            )}

            {draftConfig && activeCategory === "recording" && (
              <RecordingReviewSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                scope={scope === "global" ? "global" : effectiveInspectorCam}
                inspectorCam={effectiveInspectorCam}
                notify={notify}
              />
            )}

            {draftConfig && activeCategory === "security" && (
              <CommsAuthUsersSection
                draftConfig={draftConfig}
                updateDraft={updateDraft}
                notify={notify}
              />
            )}
          </div>
        )}
      </section>
    </div>
  );
};
