/**
 * Pure logic for the Zones, Masks & Motion studio (settings category "vision").
 *
 * Everything the operator draws on a camera is modelled here as a "vision item", grouped by
 * what it does to alerts: items that raise alerts (zones, tripwires, state ROIs), items that
 * suppress false alarms (motion and object masks) and privacy redaction. The component layer
 * only renders these models; geometry, summaries, the save review diff and edits live here so
 * they stay covered by unit tests.
 */
import type {
  CameraConfig,
  CameraMotionConfig,
  LineConfig,
  NinjaConfigResponse,
  StateRoiConfig,
  ZoneConfig,
} from "../types/nvr";

export type Point = [number, number];

export type VisionItemKind =
  | "zone"
  | "line"
  | "state"
  | "motion_mask"
  | "object_mask"
  | "privacy_mask";

export type VisionItemGroup = "alerts" | "ignored" | "privacy";

export const MASK_KINDS = ["motion_mask", "object_mask", "privacy_mask"] as const;
export type MaskKind = (typeof MASK_KINDS)[number];

export function isMaskKind(kind: VisionItemKind): kind is MaskKind {
  return (MASK_KINDS as readonly string[]).includes(kind);
}

export function visionItemGroup(kind: VisionItemKind): VisionItemGroup {
  if (kind === "zone" || kind === "line" || kind === "state") return "alerts";
  if (kind === "privacy_mask") return "privacy";
  return "ignored";
}

/** Identifies one item: zones/lines/states by YAML key, masks by their index in the list. */
export interface VisionItemRef {
  kind: VisionItemKind;
  key: string;
}

export interface VisionItem {
  ref: VisionItemRef;
  group: VisionItemGroup;
  title: string;
  summary: string;
  /** Normalized outline used to highlight the item on the video stage. */
  outline: Point[];
}

const OBJECT_LABELS_PT: Record<string, string> = {
  person: "pessoa",
  car: "carro",
  motorcycle: "moto",
  bicycle: "bicicleta",
  truck: "caminhão",
  bus: "ônibus",
  dog: "cachorro",
  cat: "gato",
  bird: "pássaro",
  package: "pacote",
  license_plate: "placa",
  face: "rosto",
};

/** Labels offered as chips in zone and tripwire editors. */
export const ZONE_OBJECT_CHOICES = [
  "person",
  "car",
  "motorcycle",
  "bicycle",
  "truck",
  "bus",
  "dog",
  "cat",
  "package",
] as const;

export function objectLabel(label: string, pt: boolean): string {
  return pt ? OBJECT_LABELS_PT[label] ?? label : label;
}

export function formatObjectList(objects: string[] | undefined, pt: boolean): string {
  const list = (objects || []).filter(Boolean);
  if (list.length === 0) return pt ? "qualquer objeto" : "any object";
  return list.map((o) => objectLabel(o, pt)).join(", ");
}

export function formatDurationShort(sec: number): string {
  const s = Math.max(0, Math.round(Number.isFinite(sec) ? sec : 0));
  if (s < 60) return `${s} s`;
  if (s < 3600) {
    const m = s / 60;
    return Number.isInteger(m) ? `${m} min` : `${m.toFixed(1)} min`;
  }
  const h = s / 3600;
  return Number.isInteger(h) ? `${h} h` : `${h.toFixed(1)} h`;
}

export function extractMaskList(mask: unknown): string[] {
  if (!mask) return [];
  if (typeof mask === "string") return mask.trim() ? [mask.trim()] : [];
  if (Array.isArray(mask)) {
    return mask
      .map((item) => {
        if (typeof item === "string") return item.trim();
        if (item && typeof item === "object" && "coordinates" in item) {
          const c = (item as { coordinates?: unknown }).coordinates;
          return typeof c === "string" ? c.trim() : "";
        }
        return "";
      })
      .filter(Boolean);
  }
  if (typeof mask === "object") {
    return Object.values(mask as Record<string, unknown>)
      .map((val) => {
        if (typeof val === "string") return val.trim();
        if (val && typeof val === "object" && "coordinates" in val) {
          const c = (val as { coordinates?: unknown }).coordinates;
          return typeof c === "string" ? c.trim() : "";
        }
        return "";
      })
      .filter(Boolean);
  }
  return [];
}

/** Parses "x1,y1,x2,y2,..." (normalized or detect-pixel) into normalized points. */
export function parseCoordPoints(coords: string, detW: number, detH: number): Point[] {
  if (typeof coords !== "string" || !coords.trim()) return [];
  const w = Number.isFinite(detW) && detW > 0 ? detW : 1280;
  const h = Number.isFinite(detH) && detH > 0 ? detH : 720;
  const nums = coords.split(",").map((s) => parseFloat(s.trim()));
  const pixel = nums.some((n) => Number.isFinite(n) && n > 1);
  const out: Point[] = [];
  for (let i = 0; i + 1 < nums.length; i += 2) {
    const x = nums[i];
    const y = nums[i + 1];
    if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
    const nx = pixel ? x / w : x;
    const ny = pixel ? y / h : y;
    out.push([clamp01(nx), clamp01(ny)]);
  }
  return out;
}

export function formatCoordPoints(points: Point[], digits = 3): string {
  return points.map(([x, y]) => `${clamp01(x).toFixed(digits)},${clamp01(y).toFixed(digits)}`).join(",");
}

function clamp01(v: number): number {
  return Math.max(0, Math.min(1, Number.isFinite(v) ? v : 0));
}

export function linePoints(line: LineConfig, detW: number, detH: number): [Point, Point] {
  const pts = line.points || [0, 0, 0, 0];
  const pixel = pts.some((n) => n > 1);
  const w = detW > 0 ? detW : 1280;
  const h = detH > 0 ? detH : 720;
  const p = (x: number, y: number): Point => [clamp01(pixel ? x / w : x), clamp01(pixel ? y / h : y)];
  return [p(pts[0], pts[1]), p(pts[2], pts[3])];
}

export function rectFromCrop(crop: [number, number, number, number]): Point[] {
  const [x1, y1, x2, y2] = crop;
  return [
    [x1, y1],
    [x2, y1],
    [x2, y2],
    [x1, y2],
  ];
}

export function rectFromCorners(a: Point, b: Point): Point[] {
  const x1 = Math.min(a[0], b[0]);
  const x2 = Math.max(a[0], b[0]);
  const y1 = Math.min(a[1], b[1]);
  const y2 = Math.max(a[1], b[1]);
  return [
    [x1, y1],
    [x2, y1],
    [x2, y2],
    [x1, y2],
  ];
}

export function cropFromPoints(points: Point[]): [number, number, number, number] | null {
  if (points.length < 2) return null;
  const xs = points.map((p) => clamp01(p[0]));
  const ys = points.map((p) => clamp01(p[1]));
  const x1 = Math.min(...xs);
  const x2 = Math.max(...xs);
  const y1 = Math.min(...ys);
  const y2 = Math.max(...ys);
  if (x2 - x1 < 0.01 || y2 - y1 < 0.01) return null;
  const r = (v: number) => Number(v.toFixed(4));
  return [r(x1), r(y1), r(x2), r(y2)];
}

// ---------------------------------------------------------------------------------------------
// Tripwire sides
// ---------------------------------------------------------------------------------------------

/**
 * Which side of the tripwire a point is on. Mirrors the backend rule in
 * `ninjanvr-track`: a crossing counts as A→B when `(end - start) × (curr - prev) > 0`,
 * so side B is where `(end - start) × (P - start) > 0`. In screen coordinates (y down)
 * side A is on the left of someone walking from point 1 to point 2.
 */
export function tripwireSide(start: Point, end: Point, p: Point): "a" | "b" | "on" {
  const cross = (end[0] - start[0]) * (p[1] - start[1]) - (end[1] - start[1]) * (p[0] - start[0]);
  if (Math.abs(cross) < 1e-9) return "on";
  return cross > 0 ? "b" : "a";
}

export interface TripwireGuide {
  mid: Point;
  /** Label anchors for side A and side B, offset perpendicular to the line. */
  sideA: Point;
  sideB: Point;
}

export function tripwireGuide(start: Point, end: Point, offset = 0.07): TripwireGuide {
  const mid: Point = [(start[0] + end[0]) / 2, (start[1] + end[1]) / 2];
  const dx = end[0] - start[0];
  const dy = end[1] - start[1];
  const len = Math.hypot(dx, dy) || 1;
  // Unit normal pointing to side B (see tripwireSide).
  const nx = -dy / len;
  const ny = dx / len;
  return {
    mid,
    sideB: [clamp01(mid[0] + nx * offset), clamp01(mid[1] + ny * offset)],
    sideA: [clamp01(mid[0] - nx * offset), clamp01(mid[1] - ny * offset)],
  };
}

// ---------------------------------------------------------------------------------------------
// Item listing and summaries
// ---------------------------------------------------------------------------------------------

function zoneSummary(z: ZoneConfig, pt: boolean): string {
  const who = formatObjectList(z.objects, pt);
  const loiter = Number(z.loitering_time || 0);
  if (loiter > 0) {
    return pt
      ? `Zona · ${who} · alerta se ficar ${formatDurationShort(loiter)}`
      : `Zone · ${who} · alert after ${formatDurationShort(loiter)} inside`;
  }
  return pt ? `Zona · ${who} · alerta ao entrar` : `Zone · ${who} · alert on entry`;
}

export function directionPhrase(direction: LineConfig["direction"], pt: boolean): string {
  if (direction === "a_to_b") return pt ? "cruza de A para B" : "crosses from A to B";
  if (direction === "b_to_a") return pt ? "cruza de B para A" : "crosses from B to A";
  return pt ? "cruza em qualquer sentido" : "crosses either way";
}

function lineSummary(l: LineConfig, pt: boolean): string {
  return `${pt ? "Linha" : "Line"} · ${formatObjectList(l.objects, pt)} ${directionPhrase(l.direction, pt)}`;
}

function stateSummary(r: StateRoiConfig, pt: boolean): string {
  const classes = (r.classes || []).join(" ↔ ");
  const alert = (r.anomaly_state || "").trim();
  if (!alert) {
    return pt ? `Estado · ${classes} · só mostra o estado` : `State · ${classes} · status only`;
  }
  const dur = formatDurationShort(r.anomaly_timeout_sec ?? 300);
  return pt
    ? `Estado · alerta se ficar ${alert} por ${dur}`
    : `State · alert if ${alert} for ${dur}`;
}

export function maskKindLabel(kind: MaskKind, pt: boolean): string {
  if (kind === "motion_mask") return pt ? "Ignorar movimento" : "Ignore motion";
  if (kind === "object_mask") return pt ? "Ignorar objetos" : "Ignore objects";
  return pt ? "Tarja de privacidade" : "Privacy redaction";
}

export function readMasks(cam: CameraConfig | undefined, kind: MaskKind): string[] {
  if (!cam) return [];
  if (kind === "motion_mask") return extractMaskList(cam.motion?.mask);
  if (kind === "object_mask") return extractMaskList(cam.objects?.mask);
  return extractMaskList(cam.privacy_mask);
}

export function writeMasks(cam: CameraConfig, kind: MaskKind, list: string[]): void {
  if (kind === "motion_mask") {
    cam.motion.mask = list;
  } else if (kind === "object_mask") {
    if (!cam.objects) cam.objects = { track: ["person"], filters: {}, mask: [] };
    cam.objects.mask = list;
  } else {
    cam.privacy_mask = list;
  }
}

export function listCameraVisionItems(cam: CameraConfig | undefined, pt: boolean): VisionItem[] {
  if (!cam) return [];
  const detW = cam.detect?.width || 1280;
  const detH = cam.detect?.height || 720;
  const items: VisionItem[] = [];
  for (const [name, z] of Object.entries(cam.zones || {})) {
    items.push({
      ref: { kind: "zone", key: name },
      group: "alerts",
      title: name,
      summary: zoneSummary(z, pt),
      outline: parseCoordPoints(String(z.coordinates || ""), detW, detH),
    });
  }
  for (const [name, l] of Object.entries(cam.lines || {})) {
    items.push({
      ref: { kind: "line", key: name },
      group: "alerts",
      title: name,
      summary: lineSummary(l, pt),
      outline: linePoints(l, detW, detH),
    });
  }
  for (const [name, r] of Object.entries(cam.state_rois || {})) {
    items.push({
      ref: { kind: "state", key: name },
      group: "alerts",
      title: name,
      summary: stateSummary(r, pt),
      outline: rectFromCrop(r.crop || [0.25, 0.25, 0.75, 0.75]),
    });
  }
  for (const kind of MASK_KINDS) {
    readMasks(cam, kind).forEach((coords, idx) => {
      items.push({
        ref: { kind, key: String(idx) },
        group: visionItemGroup(kind),
        title: `${maskKindLabel(kind, pt)} ${idx + 1}`,
        summary:
          kind === "motion_mask"
            ? pt
              ? "Movimento aqui não acorda a detecção"
              : "Motion here does not wake detection"
            : kind === "object_mask"
            ? pt
              ? "Objetos com a base aqui são descartados"
              : "Objects whose base is here are dropped"
            : pt
            ? "Tarja no console, fotos e exportações"
            : "Redacted in console, snapshots and exports",
        outline: parseCoordPoints(coords, detW, detH),
      });
    });
  }
  return items;
}

// ---------------------------------------------------------------------------------------------
// Motion sensitivity
// ---------------------------------------------------------------------------------------------

export type MotionProfileId = "indoor" | "balanced" | "outdoor";

export interface MotionProfile {
  id: MotionProfileId;
  threshold: number;
  contour_area: number;
  lightning_threshold: number;
}

export const MOTION_PROFILES: MotionProfile[] = [
  { id: "indoor", threshold: 22, contour_area: 8, lightning_threshold: 0.85 },
  { id: "balanced", threshold: 30, contour_area: 10, lightning_threshold: 0.8 },
  { id: "outdoor", threshold: 45, contour_area: 25, lightning_threshold: 0.65 },
];

export function detectMotionProfile(
  motion: Pick<CameraMotionConfig, "threshold" | "contour_area"> | undefined
): MotionProfileId | "custom" {
  const t = motion?.threshold ?? 30;
  const c = motion?.contour_area ?? 10;
  const hit = MOTION_PROFILES.find((p) => p.threshold === t && p.contour_area === c);
  return hit ? hit.id : "custom";
}

export function applyMotionProfile(motion: CameraMotionConfig, id: MotionProfileId): void {
  const p = MOTION_PROFILES.find((x) => x.id === id);
  if (!p) return;
  motion.threshold = p.threshold;
  motion.contour_area = p.contour_area;
  motion.lightning_threshold = p.lightning_threshold;
  motion.improve_contrast = true;
}

/** Motion tuning fields the studio edits; `mask` is managed as a vision item instead. */
export const MOTION_TUNING_KEYS = [
  "enabled",
  "threshold",
  "contour_area",
  "lightning_threshold",
  "improve_contrast",
  "frame_height",
] as const;

// ---------------------------------------------------------------------------------------------
// Ruler
// ---------------------------------------------------------------------------------------------

export interface MeasureReadout {
  widthPx: number;
  heightPx: number;
  areaPx: number;
  /** Fraction of the detect frame, 0..1. */
  areaFraction: number;
  ratio: number;
}

export function measureBox(a: Point, b: Point, detW: number, detH: number): MeasureReadout {
  const w = Math.max(1, Math.round(Math.abs(b[0] - a[0]) * detW));
  const h = Math.max(1, Math.round(Math.abs(b[1] - a[1]) * detH));
  const area = w * h;
  return {
    widthPx: w,
    heightPx: h,
    areaPx: area,
    areaFraction: area / Math.max(1, detW * detH),
    ratio: w / h,
  };
}

// ---------------------------------------------------------------------------------------------
// Editor commit
// ---------------------------------------------------------------------------------------------

export interface VisionEditorState {
  kind: VisionItemKind;
  /** Key of the item being edited, or null when creating a new one. */
  originalKey: string | null;
  name: string;
  objects: string[];
  /** Zone: alert on entry, or only after dwelling `dwellSec` seconds. */
  alertMode: "enter" | "dwell";
  dwellSec: number;
  inertia: number;
  preset: string;
  direction: "any" | "a_to_b" | "b_to_a";
  roiClasses: string[];
  roiAlertState: string;
  roiTimeoutSec: number;
  roiIntervalSec: number;
  roiHysteresis: number;
}

export function emptyEditorState(kind: VisionItemKind): VisionEditorState {
  return {
    kind,
    originalKey: null,
    name: "",
    objects: ["person", "car"],
    alertMode: "enter",
    dwellSec: 30,
    inertia: 3,
    preset: "",
    direction: "any",
    roiClasses: ["closed", "open"],
    roiAlertState: "open",
    roiTimeoutSec: 300,
    roiIntervalSec: 5,
    roiHysteresis: 3,
  };
}

/** Loads an existing item into editor state plus its normalized points. */
export function editorStateFromItem(
  cam: CameraConfig,
  ref: VisionItemRef
): { state: VisionEditorState; points: Point[] } | null {
  const detW = cam.detect?.width || 1280;
  const detH = cam.detect?.height || 720;
  const base = emptyEditorState(ref.kind);
  if (ref.kind === "zone") {
    const z = cam.zones?.[ref.key];
    if (!z) return null;
    const loiter = Number(z.loitering_time || 0);
    return {
      state: {
        ...base,
        originalKey: ref.key,
        name: ref.key,
        objects: [...(z.objects || [])],
        alertMode: loiter > 0 ? "dwell" : "enter",
        dwellSec: loiter > 0 ? loiter : 30,
        inertia: z.inertia ?? 3,
        preset: z.preset || "",
      },
      points: parseCoordPoints(String(z.coordinates || ""), detW, detH),
    };
  }
  if (ref.kind === "line") {
    const l = cam.lines?.[ref.key];
    if (!l) return null;
    return {
      state: {
        ...base,
        originalKey: ref.key,
        name: ref.key,
        objects: [...(l.objects || [])],
        direction: l.direction || "any",
      },
      points: linePoints(l, detW, detH),
    };
  }
  if (ref.kind === "state") {
    const r = cam.state_rois?.[ref.key];
    if (!r) return null;
    return {
      state: {
        ...base,
        originalKey: ref.key,
        name: ref.key,
        roiClasses: [...(r.classes || ["closed", "open"])],
        roiAlertState: (r.anomaly_state || "").trim(),
        roiTimeoutSec: r.anomaly_timeout_sec ?? 300,
        roiIntervalSec: r.interval_sec ?? 5,
        roiHysteresis: r.hysteresis ?? 3,
      },
      points: rectFromCrop(r.crop || [0.25, 0.25, 0.75, 0.75]),
    };
  }
  const list = readMasks(cam, ref.kind);
  const idx = Number(ref.key);
  if (!Number.isInteger(idx) || idx < 0 || idx >= list.length) return null;
  return {
    state: { ...base, originalKey: ref.key },
    points: parseCoordPoints(list[idx], detW, detH),
  };
}

export type CommitError =
  | "need_points"
  | "need_name"
  | "name_taken"
  | "need_states"
  | "missing_camera";

export type CommitResult = { ok: true; ref: VisionItemRef } | { ok: false; error: CommitError };

export function minPointsFor(kind: VisionItemKind): number {
  if (kind === "line" || kind === "state") return 2;
  return 3;
}

export function slugifyItemName(raw: string): string {
  return (raw || "")
    .trim()
    .toLowerCase()
    .normalize("NFD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9_]+/g, "_")
    .replace(/^_+|_+$/g, "");
}

function namedMapFor(cam: CameraConfig, kind: "zone" | "line" | "state"): Record<string, unknown> {
  if (kind === "zone") return cam.zones || {};
  if (kind === "line") return cam.lines || {};
  return cam.state_rois || {};
}

/**
 * Writes the editor state into `draft.cameras[camera]`. Mutates `draft`; callers pass the
 * cloned draft from `updateDraft`.
 */
export function commitVisionEditor(
  draft: NinjaConfigResponse,
  camera: string,
  editor: VisionEditorState,
  points: Point[]
): CommitResult {
  const cam = draft.cameras?.[camera];
  if (!cam) return { ok: false, error: "missing_camera" };
  if (points.length < minPointsFor(editor.kind)) return { ok: false, error: "need_points" };

  if (isMaskKind(editor.kind)) {
    const list = readMasks(cam, editor.kind);
    const coords = formatCoordPoints(points);
    const idx = editor.originalKey === null ? -1 : Number(editor.originalKey);
    let key: number;
    if (Number.isInteger(idx) && idx >= 0 && idx < list.length) {
      list[idx] = coords;
      key = idx;
    } else {
      list.push(coords);
      key = list.length - 1;
    }
    writeMasks(cam, editor.kind, list);
    return { ok: true, ref: { kind: editor.kind, key: String(key) } };
  }

  const slug = slugifyItemName(editor.name);
  if (slug.length < 2) return { ok: false, error: "need_name" };
  const map = namedMapFor(cam, editor.kind);
  if (slug !== editor.originalKey && Object.prototype.hasOwnProperty.call(map, slug)) {
    return { ok: false, error: "name_taken" };
  }

  if (editor.kind === "zone") {
    if (!cam.zones) cam.zones = {};
    const prev = editor.originalKey ? cam.zones[editor.originalKey] : undefined;
    if (editor.originalKey && editor.originalKey !== slug) delete cam.zones[editor.originalKey];
    const preset = editor.preset.trim();
    const next: ZoneConfig = {
      ...(prev || {}),
      coordinates: formatCoordPoints(points),
      objects: [...editor.objects],
      inertia: Math.max(1, Math.round(editor.inertia || 3)),
      loitering_time: editor.alertMode === "dwell" ? Math.max(1, Math.round(editor.dwellSec || 0)) : 0,
    };
    if (preset) next.preset = preset;
    else delete next.preset;
    cam.zones[slug] = next;
    return { ok: true, ref: { kind: "zone", key: slug } };
  }

  if (editor.kind === "line") {
    if (!cam.lines) cam.lines = {};
    if (editor.originalKey && editor.originalKey !== slug) delete cam.lines[editor.originalKey];
    const [p1, p2] = points;
    const r = (v: number) => Number(clamp01(v).toFixed(4));
    cam.lines[slug] = {
      points: [r(p1[0]), r(p1[1]), r(p2[0]), r(p2[1])],
      direction: editor.direction,
      objects: [...editor.objects],
    };
    return { ok: true, ref: { kind: "line", key: slug } };
  }

  const classes = Array.from(
    new Set(editor.roiClasses.map((c) => c.trim().toLowerCase()).filter(Boolean))
  );
  if (classes.length < 2) return { ok: false, error: "need_states" };
  const crop = cropFromPoints(points);
  if (!crop) return { ok: false, error: "need_points" };
  if (!cam.state_rois) cam.state_rois = {};
  if (editor.originalKey && editor.originalKey !== slug) delete cam.state_rois[editor.originalKey];
  const alert = editor.roiAlertState.trim().toLowerCase();
  const roi: StateRoiConfig = {
    crop,
    classes,
    interval_sec: Math.max(1, Math.round(editor.roiIntervalSec || 5)),
    hysteresis: Math.max(1, Math.round(editor.roiHysteresis || 3)),
    anomaly_timeout_sec: Math.max(1, Math.round(editor.roiTimeoutSec || 300)),
  };
  if (alert && classes.includes(alert)) roi.anomaly_state = alert;
  cam.state_rois[slug] = roi;
  return { ok: true, ref: { kind: "state", key: slug } };
}

export function deleteVisionItem(draft: NinjaConfigResponse, camera: string, ref: VisionItemRef): void {
  const cam = draft.cameras?.[camera];
  if (!cam) return;
  if (ref.kind === "zone") delete cam.zones?.[ref.key];
  else if (ref.kind === "line") delete cam.lines?.[ref.key];
  else if (ref.kind === "state") delete cam.state_rois?.[ref.key];
  else {
    const list = readMasks(cam, ref.kind);
    const idx = Number(ref.key);
    if (Number.isInteger(idx) && idx >= 0 && idx < list.length) {
      list.splice(idx, 1);
      writeMasks(cam, ref.kind, list);
    }
  }
}

/**
 * Copies one item to another camera using normalized coordinates, so it lands in the same
 * place regardless of each camera's detect resolution. Named items get a numeric suffix when
 * the target already has that key. Returns the new ref, or null when nothing was copied.
 */
export function copyVisionItemToCamera(
  draft: NinjaConfigResponse,
  fromCamera: string,
  ref: VisionItemRef,
  toCamera: string
): VisionItemRef | null {
  const src = draft.cameras?.[fromCamera];
  const dst = draft.cameras?.[toCamera];
  if (!src || !dst || fromCamera === toCamera) return null;
  const loaded = editorStateFromItem(src, ref);
  if (!loaded) return null;
  const state = { ...loaded.state, originalKey: null };
  if (!isMaskKind(ref.kind)) {
    const map = namedMapFor(dst, ref.kind as "zone" | "line" | "state");
    let name = ref.key;
    let n = 2;
    while (Object.prototype.hasOwnProperty.call(map, name)) {
      name = `${ref.key}_${n}`;
      n += 1;
    }
    state.name = name;
  }
  const res = commitVisionEditor(draft, toCamera, state, loaded.points);
  return res.ok ? res.ref : null;
}

// ---------------------------------------------------------------------------------------------
// Save review diff
// ---------------------------------------------------------------------------------------------

export type VisionChangeType = "added" | "removed" | "changed";

export interface VisionChange {
  id: string;
  camera: string;
  kind: VisionItemKind | "motion";
  /** Item key for named kinds, coordinate string for masks, "motion" for tuning. */
  key: string;
  type: VisionChangeType;
  title: string;
  detail: string;
  /** YAML-ish preview lines, prefixed with "+", "-" or " ". */
  yaml: string[];
}

/** Rounds to 4 decimals so f32 noise from the backend (0.18000000715…) compares equal to 0.18. */
function round4(n: number): number {
  return Math.round(n * 10_000) / 10_000;
}

/** Normalizes "x1,y1,x2,y2,…" coordinate strings to 4 decimals; other strings pass through. */
export function normalizeCoordString(s: string): string {
  if (!/^[\s\d.,eE+-]+$/.test(s) || !s.includes(",")) return s;
  const nums = s.split(",").map((x) => Number(x.trim()));
  if (nums.some((n) => !Number.isFinite(n))) return s;
  return nums.map(round4).join(",");
}

function stable(v: unknown): string {
  return JSON.stringify(v, (_k, val) => {
    if (typeof val === "number") return Number.isFinite(val) ? round4(val) : val;
    if (typeof val === "string") return normalizeCoordString(val);
    if (val && typeof val === "object" && !Array.isArray(val)) {
      return Object.fromEntries(
        Object.entries(val as Record<string, unknown>)
          .filter(([, x]) => x !== undefined && x !== null)
          .sort(([a], [b]) => a.localeCompare(b))
      );
    }
    return val;
  });
}

function yamlValue(v: unknown): string {
  if (Array.isArray(v)) return `[${v.map((x) => yamlValue(x)).join(", ")}]`;
  if (v === null || v === undefined) return "null";
  if (typeof v === "number") return String(round4(v));
  if (typeof v === "string") return normalizeCoordString(v);
  if (typeof v === "object") return stable(v);
  return String(v);
}

function yamlBlock(prefix: "+" | "-", key: string, value: unknown): string[] {
  if (value && typeof value === "object" && !Array.isArray(value)) {
    return [
      `${prefix} ${key}:`,
      ...Object.entries(value as Record<string, unknown>)
        .filter(([, v]) => v !== undefined)
        .map(([k, v]) => `${prefix}   ${k}: ${yamlValue(v)}`),
    ];
  }
  return [`${prefix} ${key}: ${yamlValue(value)}`];
}

function changedFields(a: Record<string, unknown>, b: Record<string, unknown>): string[] {
  const keys = Array.from(new Set([...Object.keys(a), ...Object.keys(b)])).sort();
  return keys.filter((k) => stable(a[k]) !== stable(b[k]));
}

const FIELD_LABELS_PT: Record<string, string> = {
  coordinates: "área redesenhada",
  points: "linha redesenhada",
  crop: "local redesenhado",
  objects: "objetos",
  inertia: "confirmação de entrada",
  loitering_time: "tempo para alertar",
  preset: "preset PTZ",
  direction: "sentido",
  classes: "estados",
  anomaly_state: "estado de alerta",
  anomaly_timeout_sec: "tempo de alerta",
  interval_sec: "intervalo",
  hysteresis: "leituras para confirmar",
  enabled: "ligado/desligado",
  threshold: "mudança por pixel",
  contour_area: "tamanho mínimo",
  lightning_threshold: "mudança brusca de luz",
  improve_contrast: "contraste à noite",
  frame_height: "resolução da análise",
};

function fieldList(fields: string[], pt: boolean): string {
  return fields.map((f) => (pt ? FIELD_LABELS_PT[f] ?? f : f)).join(", ");
}

function kindLabel(kind: VisionItemKind | "motion", pt: boolean): string {
  switch (kind) {
    case "zone":
      return pt ? "Zona" : "Zone";
    case "line":
      return pt ? "Linha" : "Line";
    case "state":
      return pt ? "Estado" : "State";
    case "motion":
      return pt ? "Sensibilidade" : "Sensitivity";
    default:
      return maskKindLabel(kind, pt);
  }
}

export function visionKindLabel(kind: VisionItemKind | "motion", pt: boolean): string {
  return kindLabel(kind, pt);
}

function diffNamed<T extends object>(
  camera: string,
  kind: "zone" | "line" | "state",
  yamlSection: string,
  saved: Record<string, T> | undefined,
  draft: Record<string, T> | undefined,
  pt: boolean,
  out: VisionChange[]
): void {
  const a = saved || {};
  const b = draft || {};
  const keys = Array.from(new Set([...Object.keys(a), ...Object.keys(b)])).sort();
  for (const key of keys) {
    const before = a[key];
    const after = b[key];
    const path = `cameras.${camera}.${yamlSection}.${key}`;
    if (before === undefined && after !== undefined) {
      out.push({
        id: `${camera}:${kind}:${key}`,
        camera,
        kind,
        key,
        type: "added",
        title: key,
        detail: pt ? "Nova." : "New.",
        yaml: [`  ${path}`, ...yamlBlock("+", key, after)],
      });
    } else if (before !== undefined && after === undefined) {
      out.push({
        id: `${camera}:${kind}:${key}`,
        camera,
        kind,
        key,
        type: "removed",
        title: key,
        detail: pt ? "Removida." : "Removed.",
        yaml: [`  ${path}`, ...yamlBlock("-", key, before)],
      });
    } else if (before !== undefined && after !== undefined && stable(before) !== stable(after)) {
      const fields = changedFields(before as Record<string, unknown>, after as Record<string, unknown>);
      out.push({
        id: `${camera}:${kind}:${key}`,
        camera,
        kind,
        key,
        type: "changed",
        title: key,
        detail: `${pt ? "Mudou" : "Changed"}: ${fieldList(fields, pt)}.`,
        yaml: [
          `  ${path}`,
          ...fields.flatMap((f) => [
            `-   ${f}: ${yamlValue((before as Record<string, unknown>)[f])}`,
            `+   ${f}: ${yamlValue((after as Record<string, unknown>)[f])}`,
          ]),
        ],
      });
    }
  }
}

const MASK_YAML_PATH: Record<MaskKind, string> = {
  motion_mask: "motion.mask",
  object_mask: "objects.mask",
  privacy_mask: "privacy_mask",
};

export function profileLabel(id: MotionProfileId | "custom", pt: boolean): string {
  if (id === "indoor") return pt ? "Interno" : "Indoor";
  if (id === "balanced") return pt ? "Equilibrado" : "Balanced";
  if (id === "outdoor") return pt ? "Externo" : "Outdoor";
  return pt ? "Personalizado" : "Custom";
}

/** Lists every zone/line/state/mask/motion difference between the saved and draft configs. */
export function diffVisionConfig(
  saved: NinjaConfigResponse | null | undefined,
  draft: NinjaConfigResponse | null | undefined,
  pt: boolean
): VisionChange[] {
  const out: VisionChange[] = [];
  if (!draft) return out;
  const cameras = Array.from(
    new Set([...Object.keys(saved?.cameras || {}), ...Object.keys(draft.cameras || {})])
  ).sort();
  for (const camera of cameras) {
    const a = saved?.cameras?.[camera];
    const b = draft.cameras?.[camera];
    if (!b) continue;
    diffNamed(camera, "zone", "zones", a?.zones, b.zones, pt, out);
    diffNamed(camera, "line", "lines", a?.lines, b.lines, pt, out);
    diffNamed(camera, "state", "state_rois", a?.state_rois, b.state_rois, pt, out);
    for (const kind of MASK_KINDS) {
      const before = readMasks(a, kind);
      const after = readMasks(b, kind);
      const beforeNorm = new Set(before.map(normalizeCoordString));
      const afterNorm = new Set(after.map(normalizeCoordString));
      const path = `cameras.${camera}.${MASK_YAML_PATH[kind]}`;
      after
        .filter((c) => !beforeNorm.has(normalizeCoordString(c)))
        .forEach((coords) =>
          out.push({
            id: `${camera}:${kind}:+${coords}`,
            camera,
            kind,
            key: coords,
            type: "added",
            title: pt ? "área desenhada" : "drawn area",
            detail: pt ? "Nova área." : "New area.",
            yaml: [`  ${path}`, `+ - ${coords}`],
          })
        );
      before
        .filter((c) => !afterNorm.has(normalizeCoordString(c)))
        .forEach((coords) =>
          out.push({
            id: `${camera}:${kind}:-${coords}`,
            camera,
            kind,
            key: coords,
            type: "removed",
            title: pt ? "área desenhada" : "drawn area",
            detail: pt ? "Área removida." : "Area removed.",
            yaml: [`  ${path}`, `- - ${coords}`],
          })
        );
    }
    const pick = (m: CameraMotionConfig | undefined) =>
      Object.fromEntries(MOTION_TUNING_KEYS.map((k) => [k, m?.[k]])) as Record<string, unknown>;
    const mA = pick(a?.motion);
    const mB = pick(b.motion);
    const fields = changedFields(mA, mB);
    if (a && fields.length > 0) {
      const pA = detectMotionProfile(a.motion);
      const pB = detectMotionProfile(b.motion);
      const detail =
        pA !== pB
          ? `${pt ? "Perfil" : "Profile"} ${profileLabel(pA, pt)} → ${profileLabel(pB, pt)}.`
          : `${pt ? "Mudou" : "Changed"}: ${fieldList(fields, pt)}.`;
      out.push({
        id: `${camera}:motion`,
        camera,
        kind: "motion",
        key: "motion",
        type: "changed",
        title: pt ? "Movimento" : "Motion",
        detail,
        yaml: [
          `  cameras.${camera}.motion`,
          ...fields.flatMap((f) => [`-   ${f}: ${yamlValue(mA[f])}`, `+   ${f}: ${yamlValue(mB[f])}`]),
        ],
      });
    }
  }
  return out;
}

/** Undoes one reviewed change in the draft by restoring the saved value. Mutates `draft`. */
export function revertVisionChange(
  draft: NinjaConfigResponse,
  saved: NinjaConfigResponse | null | undefined,
  change: VisionChange
): void {
  const cam = draft.cameras?.[change.camera];
  const base = saved?.cameras?.[change.camera];
  if (!cam) return;
  const clone = <T,>(v: T): T => (v === undefined ? v : (JSON.parse(JSON.stringify(v)) as T));
  if (change.kind === "motion") {
    if (!base) return;
    const target = cam.motion as unknown as Record<string, unknown>;
    const source = base.motion as unknown as Record<string, unknown>;
    for (const k of MOTION_TUNING_KEYS) {
      if (source[k] === undefined) delete target[k];
      else target[k] = clone(source[k]);
    }
    return;
  }
  if (isMaskKind(change.kind)) {
    const list = readMasks(cam, change.kind);
    const key = normalizeCoordString(change.key);
    if (change.type === "added") {
      writeMasks(
        cam,
        change.kind,
        list.filter((c) => normalizeCoordString(c) !== key)
      );
    } else if (change.type === "removed" && !list.some((c) => normalizeCoordString(c) === key)) {
      writeMasks(cam, change.kind, [...list, change.key]);
    }
    return;
  }
  const section = change.kind === "zone" ? "zones" : change.kind === "line" ? "lines" : "state_rois";
  const savedMap = (base?.[section] || {}) as Record<string, unknown>;
  const target = ((cam as unknown as Record<string, unknown>)[section] || {}) as Record<string, unknown>;
  if (Object.prototype.hasOwnProperty.call(savedMap, change.key)) {
    target[change.key] = clone(savedMap[change.key]);
  } else {
    delete target[change.key];
  }
  (cam as unknown as Record<string, unknown>)[section] = target;
}

export function countChangedCameras(changes: VisionChange[]): number {
  return new Set(changes.map((c) => c.camera)).size;
}

// ---------------------------------------------------------------------------------------------
// Save status
// ---------------------------------------------------------------------------------------------

export type SaveStatusKind = "busy" | "error" | "pending" | "ok" | "none";

/**
 * Which save banner the studio shows. A failed save stays visible (even while the draft is
 * still dirty) until the next attempt, so the operator never loses the error; a success is
 * only shown once nothing else is pending.
 */
export function resolveSaveStatus(params: {
  isSaving: boolean;
  isDirty: boolean;
  outcome: { ok: boolean } | null;
}): SaveStatusKind {
  if (params.isSaving) return "busy";
  if (params.outcome && !params.outcome.ok) return "error";
  if (params.isDirty) return "pending";
  if (params.outcome?.ok) return "ok";
  return "none";
}
