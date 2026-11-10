import { describe, expect, test } from "bun:test";
import type { CameraConfig, NinjaConfigResponse } from "../types/nvr";
import {
  commitVisionEditor,
  deleteVisionItem,
  emptyEditorState,
  extractMaskList,
  extractMaskMeta,
  listCameraVisionItems,
  readMasks,
  validateDrawnShape,
  writeMasks,
  type Point,
} from "../utils/zonesStudio";
import { sanitizeConfigGeometryDraft } from "../utils/visionGeometry";
import { extractMaskPolygons, mapClientPointToFrame } from "../components/VideoPlayers";

function cam(overrides: Partial<CameraConfig> = {}): CameraConfig {
  return {
    enabled: true,
    detect: { enabled: true, width: 1280, height: 720, fps: 5 },
    motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
    zones: {},
    record: { enabled: true },
    snapshots: { enabled: true },
    ...overrides,
  } as CameraConfig;
}

function config(cameras: Record<string, CameraConfig>): NinjaConfigResponse {
  return { version: "test", safe_mode: false, cameras } as unknown as NinjaConfigResponse;
}

const SQUARE: Point[] = [
  [0.1, 0.1],
  [0.4, 0.1],
  [0.4, 0.4],
  [0.1, 0.4],
];

/** Same four corners clicked out of order: the edges cross like a bow tie. */
const BOW_TIE: Point[] = [
  [0.1, 0.1],
  [0.4, 0.4],
  [0.4, 0.1],
  [0.1, 0.4],
];

describe("click mapping onto the rendered frame", () => {
  test("maps a click inside a letterboxed frame to normalized coordinates", () => {
    const rect = { left: 629, top: 239, width: 971, height: 546 };
    const [nx, ny] = mapClientPointToFrame(900, 600, rect);
    expect(nx).toBeCloseTo(271 / 971, 6);
    expect(ny).toBeCloseTo(361 / 546, 6);
  });

  test("uses the zoomed on-screen box so the point lands under the cursor", () => {
    // 1.5x digital zoom centered on a 1000x500 frame at the origin: the painted frame grows to
    // 1500x750 and shifts by -250,-125. A click on the frame center must still map to 0.5,0.5,
    // whereas the unzoomed box would have mapped it elsewhere.
    const zoomed = { left: -250, top: -125, width: 1500, height: 750 };
    expect(mapClientPointToFrame(500, 250, zoomed)).toEqual([0.5, 0.5]);
    const [zx, zy] = mapClientPointToFrame(200, 400, zoomed);
    expect(zx).toBeCloseTo(0.3, 6);
    expect(zy).toBeCloseTo(0.7, 6);
  });

  test("clamps clicks in the letterbox bars to the frame edge and survives a zero-size box", () => {
    const rect = { left: 100, top: 100, width: 400, height: 200 };
    expect(mapClientPointToFrame(50, 50, rect)).toEqual([0, 0]);
    expect(mapClientPointToFrame(900, 900, rect)).toEqual([1, 1]);
    const [x, y] = mapClientPointToFrame(10, 10, { left: 0, top: 0, width: 0, height: Number.NaN });
    expect(Number.isFinite(x) && Number.isFinite(y)).toBe(true);
  });
});

describe("drawn shape validation", () => {
  test("rejects a zone whose edges cross instead of reporting it as added", () => {
    expect(validateDrawnShape("zone", SQUARE)).toBeNull();
    expect(validateDrawnShape("zone", BOW_TIE)).toBe("bad_shape");
    expect(validateDrawnShape("privacy_mask", BOW_TIE)).toBe("bad_shape");

    const draft = config({ sala: cam() });
    const editor = { ...emptyEditorState("zone"), name: "porta" };
    expect(commitVisionEditor(draft, "sala", editor, BOW_TIE)).toEqual({ ok: false, error: "bad_shape" });
    expect(draft.cameras.sala.zones).toEqual({});
  });

  test("a zone accepted by the editor survives the draft sanitizer", () => {
    const draft = config({ sala: cam() });
    const editor = { ...emptyEditorState("zone"), name: "porta" };
    expect(commitVisionEditor(draft, "sala", editor, SQUARE).ok).toBe(true);
    const { cleaned, rejectedCount } = sanitizeConfigGeometryDraft(draft);
    expect(rejectedCount).toBe(0);
    expect(Object.keys(cleaned.cameras.sala.zones)).toEqual(["porta"]);
  });

  test("rejects a collinear area and a tripwire too short to cross", () => {
    expect(
      validateDrawnShape("motion_mask", [
        [0.1, 0.3],
        [0.4, 0.3],
        [0.8, 0.3],
      ])
    ).toBe("bad_shape");
    expect(
      validateDrawnShape("line", [
        [0.5, 0.5],
        [0.501, 0.5],
      ])
    ).toBe("line_too_short");
    expect(
      validateDrawnShape("line", [
        [0.2, 0.7],
        [0.6, 0.7],
      ])
    ).toBeNull();

    const draft = config({ sala: cam() });
    const line = { ...emptyEditorState("line"), name: "entrada" };
    const res = commitVisionEditor(draft, "sala", line, [
      [0.5, 0.5],
      [0.501, 0.5],
    ]);
    expect(res).toEqual({ ok: false, error: "line_too_short" });
  });
});

describe("object-form masks with rendering options", () => {
  const pixelated = {
    mode: "pixelate",
    block_size: 12,
    janela: "0.1,0.1,0.4,0.1,0.4,0.4,0.1,0.4",
  };

  test("option keys are not listed as polygons", () => {
    expect(extractMaskList(pixelated)).toEqual(["0.1,0.1,0.4,0.1,0.4,0.4,0.1,0.4"]);
    expect(extractMaskPolygons(pixelated)).toEqual(["0.1,0.1,0.4,0.1,0.4,0.4,0.1,0.4"]);
    expect(extractMaskMeta(pixelated)).toEqual({ mode: "pixelate", block_size: 12 });
    expect(extractMaskMeta(["0.1,0.1,0.4,0.1,0.4,0.4"])).toBeNull();
    expect(extractMaskMeta({})).toBeNull();

    const items = listCameraVisionItems(cam({ privacy_mask: pixelated }), false);
    const privacy = items.filter((i) => i.ref.kind === "privacy_mask");
    expect(privacy.length).toBe(1);
    expect(privacy[0].outline.length).toBe(4);
  });

  test("adding a polygon keeps pixelation and survives the sanitizer", () => {
    const draft = config({ sala: cam({ privacy_mask: { ...pixelated } }) });
    const editor = emptyEditorState("privacy_mask");
    const res = commitVisionEditor(draft, "sala", editor, [
      [0.6, 0.6],
      [0.9, 0.6],
      [0.9, 0.9],
    ]);
    expect(res.ok).toBe(true);
    const stored = draft.cameras.sala.privacy_mask as Record<string, unknown>;
    expect(stored.mode).toBe("pixelate");
    expect(stored.block_size).toBe(12);
    expect(readMasks(draft.cameras.sala, "privacy_mask").length).toBe(2);

    const { cleaned, rejectedCount } = sanitizeConfigGeometryDraft(draft);
    expect(rejectedCount).toBe(0);
    expect((cleaned.cameras.sala.privacy_mask as Record<string, unknown>).mode).toBe("pixelate");
  });

  test("deleting a polygon keeps the options; plain lists stay plain lists", () => {
    const draft = config({ sala: cam({ privacy_mask: { ...pixelated } }) });
    deleteVisionItem(draft, "sala", { kind: "privacy_mask", key: "0" });
    expect(draft.cameras.sala.privacy_mask).toEqual({ mode: "pixelate", block_size: 12 });
    expect(readMasks(draft.cameras.sala, "privacy_mask")).toEqual([]);

    const plain = cam();
    writeMasks(plain, "motion_mask", ["0.1,0.1,0.4,0.1,0.4,0.4"]);
    expect(plain.motion.mask).toEqual(["0.1,0.1,0.4,0.1,0.4,0.4"]);
  });
});
