import { describe, expect, test } from "bun:test";
import type { NinjaStatsResponse } from "../types/nvr";
import {
  catalogInferenceMs,
  formatMeasured,
  parseMeasured,
  resolvePrimaryInferenceMs,
  sparklineCeiling,
} from "../utils/detectorTelemetry";

const det = (inference_speed: number, extra: Record<string, unknown> = {}) => ({
  inference_speed,
  detection_start: 0,
  pid: 1,
  cpu: "1.0",
  mem: "1.0",
  ...extra,
});

type S = Pick<NinjaStatsResponse, "detectors" | "primary_detector">;

describe("detector telemetry shows only real measurements", () => {
  test("uses the primary detector cameras run, not the idle auxiliary one", () => {
    const stats: S = {
      detectors: { detector01: det(0, { measured: false, in_use: false }) },
      primary_detector: {
        inference_speed: 85.8,
        measured: true,
        samples: 120,
        loaded: true,
        model_path: "/media/ninjanvr/models/ninjanvr_security_15cl.onnx",
        device: "CPU",
      },
    };
    expect(resolvePrimaryInferenceMs(stats)).toBe(85.8);
  });

  test("an unmeasured or missing detector yields no number", () => {
    expect(resolvePrimaryInferenceMs(null)).toBeNull();
    expect(
      resolvePrimaryInferenceMs({
        detectors: {},
        primary_detector: { inference_speed: 0, measured: false, samples: 0, loaded: false, model_path: "", device: "CPU" },
      })
    ).toBeNull();
    expect(resolvePrimaryInferenceMs({ detectors: { detector01: det(2.1, { measured: false }) } })).toBeNull();
  });

  test("older servers without primary_detector keep their first detector value", () => {
    expect(resolvePrimaryInferenceMs({ detectors: { detector01: det(7.4) } })).toBe(7.4);
  });

  test("formatting and parsing never invent defaults", () => {
    expect(formatMeasured(null)).toBe("—");
    expect(formatMeasured(85.83)).toBe("85.8");
    expect(formatMeasured(85.83, 1, " ms")).toBe("85.8 ms");
    expect(formatMeasured(null, 1, "%")).toBe("—");
    expect(parseMeasured("12.5")).toBe(12.5);
    expect(parseMeasured(undefined)).toBeNull();
    expect(parseMeasured("n/a")).toBeNull();
  });

  test("sparkline ceiling fits slow models", () => {
    expect(sparklineCeiling([2, 5, null])).toBe(20);
    expect(sparklineCeiling([80, 85.8])).toBe(103);
  });
});

describe("detector catalog latency", () => {
  test("is null until a loaded model has run", () => {
    expect(catalogInferenceMs(null)).toBeNull();
    expect(catalogInferenceMs({ loaded: false, avg_inference_ms: 7, inference_samples: 3 })).toBeNull();
    expect(catalogInferenceMs({ loaded: true, avg_inference_ms: 0, inference_samples: 0 })).toBeNull();
    expect(catalogInferenceMs({ loaded: true, avg_inference_ms: 85.8, inference_samples: 40 })).toBe(85.8);
    // Older server without the counter
    expect(catalogInferenceMs({ loaded: true, avg_inference_ms: 9.1 })).toBe(9.1);
  });
});
