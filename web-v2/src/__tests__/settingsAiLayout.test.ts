import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import { AiDetectionObjectsSection } from "../views/settings/VisionAndAiPanels";
import type { NinjaConfigResponse } from "../types/nvr";

const camera = {
  enabled: true,
  detect: { enabled: true, width: 1280, height: 720, fps: 5 },
  motion: { enabled: true, threshold: 30, contour_area: 10, mask: [] },
  objects: { track: ["person"], filters: {} },
  zones: {},
  state_rois: {},
  record: {
    enabled: true,
    continuous: { days: 1 },
    motion: { days: 1 },
    alerts: { pre_capture: 5, post_capture: 5, retain: { days: 1, mode: "motion" } },
    detections: { pre_capture: 5, post_capture: 5, retain: { days: 1, mode: "motion" } },
  },
  snapshots: { enabled: true },
};

const config = {
  version: "test",
  safe_mode: false,
  detectors: { detector01: { type: "openvino", device: "CPU" } },
  model: {
    width: 300,
    height: 300,
    input_tensor: "nhwc",
    input_pixel_format: "bgr",
    path: "/openvino-model/ssdlite_mobilenet_v2.xml",
  },
  objects: { track: ["person"], filters: {} },
  cameras: { sala: camera, quarto: camera },
} as unknown as NinjaConfigResponse;

function render(scope: string, extra: Record<string, unknown> = {}): string {
  return renderToStaticMarkup(
    React.createElement(AiDetectionObjectsSection, {
      draftConfig: config,
      updateDraft: () => {},
      scope,
      inspectorCam: "sala",
      notify: () => {},
      ...extra,
    })
  );
}

describe("AI & Object Filters layout", () => {
  test("opens on the Engine & Tracking module instead of every module", () => {
    const html = render("sala");
    expect(html).toContain("AI DETECTION MODEL");
    expect(html).toContain("TRACKING RESPONSIVENESS");
    expect(html).not.toContain("VISION AI JUDGE");
    expect(html).not.toContain("FACE RECOGNITION");
    expect(html).not.toContain("NEW STATE MONITOR");
  });

  test("groups global cards before camera cards with one divider each", () => {
    const html = render("sala");
    const globalAt = html.indexOf("GLOBAL · ALL 2 CAMERAS");
    const cameraAt = html.indexOf("SALA · THIS CAMERA ONLY");
    expect(globalAt).toBeGreaterThan(-1);
    expect(cameraAt).toBeGreaterThan(globalAt);
    expect(html.indexOf("AI DETECTION MODEL")).toBeGreaterThan(globalAt);
    expect(html.indexOf("AI DETECTION MODEL")).toBeLessThan(cameraAt);
    expect(html.indexOf("OBJECT DETECTION")).toBeGreaterThan(cameraAt);
    expect(html).not.toContain("nvr-scope-pill");
  });

  test("camera group becomes inherited defaults under GLOBAL DEFAULTS scope", () => {
    const html = render("global");
    expect(html).toContain("GLOBAL DEFAULTS · INHERITED BY CAMERAS");
    expect(html).not.toContain("THIS CAMERA ONLY");
  });

  test("State Monitor keeps its own camera even under GLOBAL DEFAULTS scope", () => {
    const html = render("global", { initialSubSection: "state_rois" });
    expect(html).toContain("SALA · THIS CAMERA ONLY");
    expect(html).not.toContain("INHERITED BY CAMERAS");
  });

  test("operational data cards start collapsed with a count", () => {
    const genai = render("sala", { initialSubSection: "genai" });
    expect(genai).toContain("FALSE-POSITIVE MEMORY");
    expect(genai).toContain('aria-expanded="false"');
    expect(genai).not.toContain("nvr-fp-memory-body\" class");

    const faces = render("sala", { initialSubSection: "faces" });
    expect(faces).toContain("ENROLLED FACES");
    expect(faces).not.toContain('id="nvr-face-gallery-body"');

    const lpr = render("sala", { initialSubSection: "lpr" });
    expect(lpr).toContain("KNOWN PLATES");
    expect(lpr).not.toContain('id="nvr-known-plates-body"');
  });
});
