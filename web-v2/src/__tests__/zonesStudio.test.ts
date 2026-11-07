import { describe, expect, test } from "bun:test";
import type { CameraConfig, NinjaConfigResponse } from "../types/nvr";
import {
  applyMotionProfile,
  commitVisionEditor,
  copyVisionItemToCamera,
  countChangedCameras,
  cropFromPoints,
  deleteVisionItem,
  detectMotionProfile,
  diffVisionConfig,
  editorStateFromItem,
  emptyEditorState,
  formatDurationShort,
  listCameraVisionItems,
  measureBox,
  parseCoordPoints,
  readMasks,
  rectFromCorners,
  resolveSaveStatus,
  revertVisionChange,
  tripwireGuide,
  tripwireSide,
  visionItemGroup,
  type Point,
} from "../utils/zonesStudio";

function cam(overrides: Partial<CameraConfig> = {}): CameraConfig {
  return {
    enabled: true,
    detect: { enabled: true, width: 1000, height: 500, fps: 5 },
    motion: { enabled: true, threshold: 30, contour_area: 10, mask: [] },
    zones: {},
    record: { enabled: true },
    snapshots: { enabled: true },
    ...overrides,
  } as CameraConfig;
}

function config(cameras: Record<string, CameraConfig>): NinjaConfigResponse {
  return { version: "test", safe_mode: false, cameras } as unknown as NinjaConfigResponse;
}

const clone = <T,>(v: T): T => JSON.parse(JSON.stringify(v)) as T;

const SQUARE: Point[] = [
  [0.1, 0.1],
  [0.4, 0.1],
  [0.4, 0.4],
  [0.1, 0.4],
];

describe("zones studio — item grouping and plain-language summaries", () => {
  test("groups items by what they do to alerts", () => {
    expect(visionItemGroup("zone")).toBe("alerts");
    expect(visionItemGroup("line")).toBe("alerts");
    expect(visionItemGroup("state")).toBe("alerts");
    expect(visionItemGroup("motion_mask")).toBe("ignored");
    expect(visionItemGroup("object_mask")).toBe("ignored");
    expect(visionItemGroup("privacy_mask")).toBe("privacy");
  });

  test("lists every item with a summary the operator can read", () => {
    const c = cam({
      zones: {
        entrada: { coordinates: "0.1,0.1,0.4,0.1,0.4,0.4", objects: ["person", "car"] },
        varanda: { coordinates: "0.5,0.5,0.9,0.5,0.9,0.9", objects: [], loitering_time: 30 },
      },
      lines: { portao: { points: [0.1, 0.5, 0.9, 0.5], direction: "a_to_b", objects: ["person"] } },
      state_rois: {
        porta: {
          crop: [0.2, 0.2, 0.4, 0.6],
          classes: ["closed", "open"],
          anomaly_state: "open",
          anomaly_timeout_sec: 300,
        },
        vaga: { crop: [0.5, 0.5, 0.7, 0.7], classes: ["empty", "occupied"] },
      },
      motion: { enabled: true, threshold: 30, contour_area: 10, mask: ["0,0,100,0,100,50,0,50"] },
      objects: { track: ["person"], mask: ["0.6,0.6,0.8,0.6,0.8,0.8"] },
      privacy_mask: ["0.7,0.1,0.9,0.1,0.9,0.3"],
    });
    const items = listCameraVisionItems(c, true);
    const byTitle = Object.fromEntries(items.map((i) => [i.title, i]));
    expect(byTitle.entrada.summary).toBe("Zona · pessoa, carro · alerta ao entrar");
    expect(byTitle.varanda.summary).toBe("Zona · qualquer objeto · alerta se ficar 30 s");
    expect(byTitle.portao.summary).toBe("Linha · pessoa cruza de A para B");
    expect(byTitle.porta.summary).toBe("Estado · alerta se ficar open por 5 min");
    expect(byTitle.vaga.summary).toBe("Estado · empty ↔ occupied · só mostra o estado");
    expect((items.filter((i) => i.group === "ignored")).length).toBe(2);
    expect((items.filter((i) => i.group === "privacy")).length).toBe(1);
    // Pixel mask coordinates are normalized with the camera's detect resolution.
    const motionMask = items.find((i) => i.ref.kind === "motion_mask");
    expect(motionMask?.outline[2]).toEqual([0.1, 0.1]);
    expect(listCameraVisionItems(c, false).find((i) => i.title === "entrada")?.summary).toBe(
      "Zone · person, car · alert on entry"
    );
  });

  test("formats durations and parses coordinates safely", () => {
    expect(formatDurationShort(30)).toBe("30 s");
    expect(formatDurationShort(300)).toBe("5 min");
    expect(formatDurationShort(5400)).toBe("1.5 h");
    expect(parseCoordPoints("", 1000, 500)).toEqual([]);
    expect(parseCoordPoints("500,250,NaN,1", 1000, 500)).toEqual([[0.5, 0.5]]);
  });
});

describe("zones studio — tripwire sides match the backend crossing rule", () => {
  test("side A is left of start→end, side B is where A→B crossings land", () => {
    const start: Point = [0.1, 0.5];
    const end: Point = [0.9, 0.5];
    // Backend: moving downward across a left→right line is A→B, so above is A, below is B.
    expect(tripwireSide(start, end, [0.5, 0.2])).toBe("a");
    expect(tripwireSide(start, end, [0.5, 0.8])).toBe("b");
    expect(tripwireSide(start, end, [0.5, 0.5])).toBe("on");
    // Swapping the points swaps the sides (the "Inverter A e B" action).
    expect(tripwireSide(end, start, [0.5, 0.2])).toBe("b");
  });

  test("guide label anchors sit on the matching side", () => {
    const start: Point = [0.2, 0.7];
    const end: Point = [0.8, 0.3];
    const g = tripwireGuide(start, end);
    expect(tripwireSide(start, end, g.sideA)).toBe("a");
    expect(tripwireSide(start, end, g.sideB)).toBe("b");
    expect(g.mid).toEqual([0.5, 0.5]);
  });
});

describe("zones studio — motion profiles and ruler", () => {
  test("detects and applies scene profiles", () => {
    const m: CameraConfig["motion"] = { enabled: true, threshold: 30, contour_area: 10, mask: [] };
    expect(detectMotionProfile(m)).toBe("balanced");
    applyMotionProfile(m, "outdoor");
    expect(m).toEqual({
      enabled: true,
      threshold: 45,
      contour_area: 25,
      lightning_threshold: 0.65,
      improve_contrast: true,
      mask: [],
    });
    expect(detectMotionProfile(m)).toBe("outdoor");
    expect(detectMotionProfile({ threshold: 31, contour_area: 10 })).toBe("custom");
  });

  test("measures a box in detect pixels", () => {
    const r = measureBox([0.1, 0.1], [0.3, 0.5], 1000, 500);
    expect([r.widthPx, r.heightPx, r.areaPx, r.ratio]).toEqual([200, 200, 40000, 1]);
    expect(r.areaFraction).toBeCloseTo(0.08);
  });

  test("builds state ROI rectangles from two clicks", () => {
    const rect = rectFromCorners([0.6, 0.7], [0.2, 0.3]);
    expect(rect[0]).toEqual([0.2, 0.3]);
    expect(rect[2]).toEqual([0.6, 0.7]);
    expect(cropFromPoints(rect)).toEqual([0.2, 0.3, 0.6, 0.7]);
    expect(cropFromPoints([[0.2, 0.2], [0.205, 0.5]])).toBeNull();
  });
});

describe("zones studio — editor commit", () => {
  test("creates a zone with dwell alert and rejects duplicates and missing input", () => {
    const draft = config({ sala: cam() });
    const editor = { ...emptyEditorState("zone"), name: "Portão da Garagem", alertMode: "dwell" as const, dwellSec: 20 };
    expect(commitVisionEditor(draft, "sala", editor, SQUARE.slice(0, 2))).toEqual({ ok: false, error: "need_points" });
    expect(commitVisionEditor(draft, "sala", { ...editor, name: "x" }, SQUARE)).toEqual({ ok: false, error: "need_name" });
    const res = commitVisionEditor(draft, "sala", editor, SQUARE);
    expect(res).toEqual({ ok: true, ref: { kind: "zone", key: "portao_da_garagem" } });
    expect(draft.cameras.sala.zones.portao_da_garagem).toEqual({
      coordinates: "0.100,0.100,0.400,0.100,0.400,0.400,0.100,0.400",
      objects: ["person", "car"],
      inertia: 3,
      loitering_time: 20,
    });
    expect(commitVisionEditor(draft, "sala", editor, SQUARE)).toEqual({ ok: false, error: "name_taken" });
    expect(commitVisionEditor(draft, "nope", editor, SQUARE)).toEqual({ ok: false, error: "missing_camera" });
  });

  test("editing renames in place and keeps fields the studio does not edit", () => {
    const draft = config({
      sala: cam({ zones: { porta: { coordinates: "0.1,0.1,0.4,0.1,0.4,0.4", min_score: 0.8, preset: "p1" } } }),
    });
    const loaded = editorStateFromItem(draft.cameras.sala, { kind: "zone", key: "porta" });
    expect(loaded?.state.preset).toBe("p1");
    const res = commitVisionEditor(draft, "sala", { ...loaded!.state, name: "Porta frente", preset: "" }, loaded!.points);
    expect(res.ok).toBe(true);
    expect(Object.keys(draft.cameras.sala.zones)).toEqual(["porta_frente"]);
    expect(draft.cameras.sala.zones.porta_frente.min_score).toBe(0.8);
    expect(draft.cameras.sala.zones.porta_frente.preset).toBeUndefined();
  });

  test("tripwire keeps point order and direction", () => {
    const draft = config({ sala: cam() });
    const editor = { ...emptyEditorState("line"), name: "portao", direction: "b_to_a" as const, objects: ["car"] };
    const res = commitVisionEditor(draft, "sala", editor, [[0.9, 0.2], [0.1, 0.8]]);
    expect(res.ok).toBe(true);
    expect(draft.cameras.sala.lines?.portao).toEqual({ points: [0.9, 0.2, 0.1, 0.8], direction: "b_to_a", objects: ["car"] });
  });

  test("state ROI needs two states and only alerts on a known state", () => {
    const draft = config({ sala: cam() });
    const base = { ...emptyEditorState("state"), name: "porta" };
    expect(commitVisionEditor(draft, "sala", { ...base, roiClasses: ["open"] }, SQUARE)).toEqual({ ok: false, error: "need_states" });
    const res = commitVisionEditor(draft, "sala", { ...base, roiAlertState: "ajar" }, SQUARE);
    expect(res.ok).toBe(true);
    expect(draft.cameras.sala.state_rois?.porta).toEqual({
      crop: [0.1, 0.1, 0.4, 0.4],
      classes: ["closed", "open"],
      interval_sec: 5,
      hysteresis: 3,
      anomaly_timeout_sec: 300,
    });
  });

  test("masks are appended, replaced by index and deleted", () => {
    const draft = config({ sala: cam() });
    const editor = emptyEditorState("privacy_mask");
    commitVisionEditor(draft, "sala", editor, SQUARE);
    commitVisionEditor(draft, "sala", editor, SQUARE.map(([x, y]) => [x + 0.5, y] as Point));
    expect((readMasks(draft.cameras.sala, "privacy_mask")).length).toBe(2);
    commitVisionEditor(draft, "sala", { ...editor, originalKey: "0" }, SQUARE.map(([x, y]) => [x, y + 0.5] as Point));
    expect(readMasks(draft.cameras.sala, "privacy_mask")[0]).toBe("0.100,0.600,0.400,0.600,0.400,0.900,0.100,0.900");
    deleteVisionItem(draft, "sala", { kind: "privacy_mask", key: "1" });
    expect((readMasks(draft.cameras.sala, "privacy_mask")).length).toBe(1);
    commitVisionEditor(draft, "sala", emptyEditorState("object_mask"), SQUARE);
    expect((readMasks(draft.cameras.sala, "object_mask")).length).toBe(1);
  });

  test("copies an item to another camera with a free name and normalized coordinates", () => {
    const draft = config({
      sala: cam({ zones: { entrada: { coordinates: "100,50,400,50,400,200", objects: ["person"] } } }),
      cozinha: cam({ detect: { enabled: true, width: 1920, height: 1080, fps: 5 }, zones: { entrada: { coordinates: "0,0,0.1,0,0.1,0.1" } } }),
    });
    const ref = copyVisionItemToCamera(draft, "sala", { kind: "zone", key: "entrada" }, "cozinha");
    expect(ref).toEqual({ kind: "zone", key: "entrada_2" });
    expect(draft.cameras.cozinha.zones.entrada_2.coordinates).toBe("0.100,0.100,0.400,0.100,0.400,0.400");
    expect(copyVisionItemToCamera(draft, "sala", { kind: "zone", key: "entrada" }, "sala")).toBeNull();
  });
});

describe("zones studio — save review diff and per-change undo", () => {
  const saved = config({
    sala: cam({
      zones: { entrada: { coordinates: "0.1,0.1,0.4,0.1,0.4,0.4", objects: ["person"] } },
      motion: { enabled: true, threshold: 30, contour_area: 10, mask: ["0.6,0.3,0.9,0.3,0.9,0.5"] },
    }),
    cozinha: cam(),
  });

  function edited(): NinjaConfigResponse {
    const d = clone(saved);
    d.cameras.sala.zones.entrada.objects = ["person", "car"];
    d.cameras.sala.lines = { portao: { points: [0.1, 0.5, 0.9, 0.5], direction: "a_to_b" } };
    d.cameras.sala.motion.mask = [];
    applyMotionProfile(d.cameras.cozinha.motion, "outdoor");
    return d;
  }

  test("lists additions, removals and changes with readable details", () => {
    const changes = diffVisionConfig(saved, edited(), true);
    expect(changes.map((c) => `${c.camera}:${c.kind}:${c.type}`)).toEqual([
      "cozinha:motion:changed",
      "sala:zone:changed",
      "sala:line:added",
      "sala:motion_mask:removed",
    ]);
    expect(changes[0].detail).toBe("Perfil Equilibrado → Externo.");
    expect(changes[1].detail).toBe("Mudou: objetos.");
    expect(changes[1].yaml).toContain("+   objects: [person, car]");
    expect(countChangedCameras(changes)).toBe(2);
    expect(diffVisionConfig(saved, clone(saved), true)).toEqual([]);
  });

  test("ignores f32 noise from the backend so untouched items are not reported", () => {
    const fromBackend = config({
      sala: cam({
        lines: { portao: { points: [0.18000000715255737, 0.6200000047683716, 0.82, 0.41], direction: "a_to_b" } },
        state_rois: { porta: { crop: [0.41999998688697815, 0.18000000715255737, 0.7, 0.64], classes: ["closed", "open"], anomaly_state: null } },
        zones: { entrada: { coordinates: "0.1,0.546,0.399,0.5,0.44,0.92", preset: null } },
        motion: { enabled: true, threshold: 30, contour_area: 10, mask: ["0.62,0.34,0.92,0.34,0.92,0.52"] },
      }),
    });
    const sanitized = clone(fromBackend);
    sanitized.cameras.sala.lines!.portao.points = [0.18, 0.62, 0.82, 0.41];
    sanitized.cameras.sala.state_rois!.porta = { crop: [0.42, 0.18, 0.7, 0.64], classes: ["closed", "open"] };
    sanitized.cameras.sala.zones.entrada = { coordinates: "0.100,0.546,0.399,0.500,0.440,0.920" };
    sanitized.cameras.sala.motion.mask = ["0.620,0.340,0.920,0.340,0.920,0.520"];
    expect(diffVisionConfig(fromBackend, sanitized, true)).toEqual([]);

    sanitized.cameras.sala.lines!.portao.direction = "any";
    const changes = diffVisionConfig(fromBackend, sanitized, true);
    expect(changes.map((c) => c.detail)).toEqual(["Mudou: sentido."]);
  });

  test("undo restores exactly one change and leaves the rest pending", () => {
    const draft = edited();
    for (const change of diffVisionConfig(saved, draft, true)) {
      const before = diffVisionConfig(saved, draft, true).length;
      revertVisionChange(draft, saved, change);
      expect(diffVisionConfig(saved, draft, true).length).toBe(before - 1);
    }
    expect(diffVisionConfig(saved, draft, true)).toEqual([]);
    expect(draft.cameras.sala.lines?.portao).toBeUndefined();
    expect(readMasks(draft.cameras.sala, "motion_mask")).toEqual(["0.6,0.3,0.9,0.3,0.9,0.5"]);
  });
});

describe("zones studio — save status feedback", () => {
  test("always tells the operator where the save stands", () => {
    expect(resolveSaveStatus({ isSaving: true, isDirty: true, outcome: null })).toBe("busy");
    expect(resolveSaveStatus({ isSaving: false, isDirty: true, outcome: null })).toBe("pending");
    expect(resolveSaveStatus({ isSaving: false, isDirty: false, outcome: { ok: true } })).toBe("ok");
    expect(resolveSaveStatus({ isSaving: false, isDirty: false, outcome: null })).toBe("none");
  });

  test("a failed save stays visible while the draft is still unsaved", () => {
    expect(resolveSaveStatus({ isSaving: false, isDirty: true, outcome: { ok: false } })).toBe("error");
    expect(resolveSaveStatus({ isSaving: true, isDirty: true, outcome: { ok: false } })).toBe("busy");
  });
});
