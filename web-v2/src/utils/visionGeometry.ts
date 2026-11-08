/**
 * Pure geometry sanitizers for drawn zones, masks, tripwires and state ROIs.
 *
 * Shared by the settings workspace (which sanitizes every draft before it is stored or saved)
 * and the Zones studio editor (which validates a shape before adding it, so a shape the
 * sanitizer would drop is reported to the operator instead of vanishing silently).
 */
import type { CameraConfig, LineConfig, NinjaConfigResponse, StateRoiConfig } from "../types/nvr";

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
