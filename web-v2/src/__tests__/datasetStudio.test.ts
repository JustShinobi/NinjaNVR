import { describe, expect, test } from "bun:test";
import {
  buildModelUploadPayload,
  computeDatasetReadinessMetrics,
  formatYoloAnnotationLine,
  NINJANVR_SECURITY_CLASSES,
  xyxyToYoloNormalized,
  yoloNormalizedToXyxy,
} from "../views/settings/VisionAndAiPanels";

describe("Dataset Studio YOLO Normalization & Readiness Gauges", () => {
  test("defines the canonical 15-class NinjaNVR Security taxonomy", () => {
    expect(Array.from(NINJANVR_SECURITY_CLASSES)).toEqual([
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
    ]);
  });

  test("normalizes [x1, y1, x2, y2] <-> [cx, cy, w, h] with clamping and formatting", () => {
    const yolo = xyxyToYoloNormalized([0.2, 0.1, 0.6, 0.9]);
    expect(yolo).toEqual({ cx: 0.4, cy: 0.5, w: 0.4, h: 0.8 });
    expect(yoloNormalizedToXyxy(yolo)).toEqual([0.2, 0.1, 0.6, 0.9]);

    // Inverted drag coordinates are automatically ordered
    const inverted = xyxyToYoloNormalized([0.75, 0.9, 0.55, 0.7]);
    expect(inverted).toEqual({ cx: 0.65, cy: 0.8, w: 0.2, h: 0.2 });

    expect(
      formatYoloAnnotationLine({
        class_id: 0,
        label: "person",
        cx: 0.3,
        cy: 0.5,
        w: 0.4,
        h: 0.6,
        score: 0.97,
        source: "ai_teacher",
      })
    ).toBe("0 0.300000 0.500000 0.400000 0.600000");
  });

  test("computes 80/20 TP vs Hard Negative readiness gauge metrics and upload payloads", () => {
    const empty = computeDatasetReadinessMetrics(null);
    expect(empty.totalSamples).toBe(0);
    expect(empty.balanceScorePct).toBe(0);
    expect(empty.isBalanced).toBe(false);

    const balanced = computeDatasetReadinessMetrics({
      total_samples: 50,
      true_positive_count: 25,
      ai_annotated_count: 15,
      hard_negative_count: 10,
      train_count: 40,
      val_count: 10,
      verified_count: 45,
    });
    expect(balanced.positiveCount).toBe(40);
    expect(balanced.hardNegativeCount).toBe(10);
    expect(balanced.positivePct).toBe(80);
    expect(balanced.hardNegativePct).toBe(20);
    expect(balanced.balanceScorePct).toBe(100);
    expect(balanced.isBalanced).toBe(true);
    expect(balanced.trainPct).toBe(80);
    expect(balanced.valPct).toBe(20);
    expect(balanced.verifiedPct).toBe(90);

    const skewed = computeDatasetReadinessMetrics({
      total_samples: 20,
      true_positive_count: 20,
      ai_annotated_count: 0,
      hard_negative_count: 0,
      train_count: 16,
      val_count: 4,
      verified_count: 10,
    });
    expect(skewed.positivePct).toBe(100);
    expect(skewed.hardNegativePct).toBe(0);
    expect(skewed.isBalanced).toBe(false);
    expect(skewed.balanceScorePct).toBe(75);

    const upload = buildModelUploadPayload({
      filename: "ninjanvr_yolo11s.onnx",
      modelBase64: "U1lOVEhFVElDX09OTlg=",
      width: 640,
      height: 640,
    });
    expect(upload.filename).toBe("ninjanvr_yolo11s.onnx");
    expect(upload.activate).toBe(true);
    expect(upload.input_tensor).toBe("nchw");
    expect(upload.input_pixel_format).toBe("rgb");
  });
});
