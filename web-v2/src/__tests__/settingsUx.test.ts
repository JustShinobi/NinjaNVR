import { describe, expect, test } from "bun:test";
import {
  countChangedSections,
  formatUnsavedLabel,
  resolveScopeGroupLabel,
} from "../utils/settingsUx";

describe("resolveScopeGroupLabel", () => {
  test("global group names how many cameras share the setting", () => {
    expect(resolveScopeGroupLabel("global", "escritorio", "escritorio", 4)).toBe(
      "GLOBAL · ALL 4 CAMERAS"
    );
    expect(resolveScopeGroupLabel("global", "global", "sala", 4, true)).toBe(
      "GLOBAL · TODAS AS 4 CÂMERAS"
    );
    expect(resolveScopeGroupLabel("global", "x", "x", 1)).toBe("GLOBAL · 1 CAMERA");
  });

  test("global group degrades without a usable camera count", () => {
    expect(resolveScopeGroupLabel("global", "x", "x", 0)).toBe("GLOBAL · ALL CAMERAS");
    expect(resolveScopeGroupLabel("global", "x", "x", Number.NaN)).toBe(
      "GLOBAL · ALL CAMERAS"
    );
    expect(resolveScopeGroupLabel("global", "x", "x", -3, true)).toBe(
      "GLOBAL · TODAS AS CÂMERAS"
    );
  });

  test("camera group follows the header scope", () => {
    expect(resolveScopeGroupLabel("camera", "escritorio", "escritorio", 4)).toBe(
      "ESCRITORIO · THIS CAMERA ONLY"
    );
    expect(resolveScopeGroupLabel("camera", "sala", "sala", 4, true)).toBe(
      "SALA · SOMENTE ESTA CÂMERA"
    );
  });

  test("camera group becomes the inherited defaults when GLOBAL DEFAULTS is selected", () => {
    expect(resolveScopeGroupLabel("camera", "global", "escritorio", 4)).toBe(
      "GLOBAL DEFAULTS · INHERITED BY CAMERAS"
    );
    expect(resolveScopeGroupLabel("camera", "global", "escritorio", 4, true)).toBe(
      "PADRÃO GLOBAL · HERDADO PELAS CÂMERAS"
    );
  });

  test("camera group falls back when the camera id is blank", () => {
    expect(resolveScopeGroupLabel("camera", "cam", "  ", 2)).toBe(
      "CAMERA · THIS CAMERA ONLY"
    );
  });
});

describe("countChangedSections", () => {
  const base = {
    mqtt: { host: "127.0.0.1", port: 1883 },
    genai: { enabled: false },
    cameras: {
      sala: { detect: { fps: 5, width: 1280 }, objects: { track: ["person", "car"] } },
      quarto: { detect: { fps: 5 } },
    },
  };
  const clone = () => JSON.parse(JSON.stringify(base));

  test("identical configs have no changes", () => {
    expect(countChangedSections(base, clone())).toBe(0);
  });

  test("several edits inside one section count once", () => {
    const draft = clone();
    draft.mqtt.port = 1884;
    draft.mqtt.host = "broker";
    expect(countChangedSections(base, draft)).toBe(1);
  });

  test("defaults materialised alongside a toggle do not inflate the count", () => {
    const draft = clone();
    draft.cameras.sala.detect = { fps: 8, width: 1280, height: 720, stationary: { interval: 50 } };
    expect(countChangedSections(base, draft)).toBe(1);
  });

  test("per-camera sections are distinct per camera and per section", () => {
    const draft = clone();
    draft.cameras.sala.detect.fps = 8;
    draft.cameras.sala.objects.track = ["person"];
    draft.cameras.quarto.detect.fps = 8;
    draft.genai.enabled = true;
    expect(countChangedSections(base, draft)).toBe(4);
  });

  test("missing and undefined keys are equivalent; empty objects are not changes", () => {
    const draft = clone();
    draft.mqtt.user = undefined;
    draft.review = {};
    expect(countChangedSections(base, draft)).toBe(0);
  });

  test("added and removed sections count", () => {
    const draft = clone();
    draft.lpr = { enabled: true };
    delete draft.genai;
    draft.cameras.garagem = { detect: { fps: 5 } };
    expect(countChangedSections(base, draft)).toBe(3);
  });

  test("a type change between object and scalar is a change", () => {
    expect(countChangedSections({ a: { b: 1 } }, { a: 3 })).toBe(1);
    expect(countChangedSections({ a: [1] }, { a: { 0: 1 } })).toBe(1);
  });

  test("handles null and primitive roots", () => {
    expect(countChangedSections(null, null)).toBe(0);
    expect(countChangedSections(null, { a: 1 })).toBe(1);
  });
});

describe("formatUnsavedLabel", () => {
  test("hides when nothing changed and pluralizes in both locales", () => {
    expect(formatUnsavedLabel(0)).toBe("");
    expect(formatUnsavedLabel(-1)).toBe("");
    expect(formatUnsavedLabel(1)).toBe("1 UNSAVED SECTION");
    expect(formatUnsavedLabel(3)).toBe("3 UNSAVED SECTIONS");
    expect(formatUnsavedLabel(1, true)).toBe("1 SEÇÃO NÃO SALVA");
    expect(formatUnsavedLabel(2, true)).toBe("2 SEÇÕES NÃO SALVAS");
  });
});
