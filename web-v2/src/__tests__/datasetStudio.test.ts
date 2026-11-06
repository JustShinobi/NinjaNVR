import { describe, expect, test } from "bun:test";
import {
  applySecurity15ClassPreset,
  buildDatasetSampleFilterOptions,
  buildLocalFinetuneCliCommand,
  buildModelUploadPayload,
  COLAB_MODEL_FACTORY_URL,
  computeBatchProgressMetrics,
  computeDatasetReadinessMetrics,
  formatYoloAnnotationLine,
  NINJANVR_SECURITY_15CL_LABELMAP_PATH,
  NINJANVR_SECURITY_CLASSES,
  selectBatchAutoAnnotateCandidates,
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

  test("applies 15-class Security Labelmap preset and generates local RTX freeze=10 CLI command", () => {
    const updatedModel = applySecurity15ClassPreset({
      path: "/openvino-model/ssdlite_mobilenet_v2.xml",
      labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
      width: 300,
      height: 300,
      input_tensor: "nhwc",
      input_pixel_format: "bgr",
      model_type: "ssd",
    });
    expect(updatedModel.labelmap_path).toBe(NINJANVR_SECURITY_15CL_LABELMAP_PATH);
    expect(updatedModel.labelmap_path).toBe("/openvino-model/ninjanvr_security_15cl.txt");
    expect(updatedModel.model_type).toBe("yolo-generic");
    expect(updatedModel.width).toBe(640);
    expect(updatedModel.height).toBe(640);
    expect(updatedModel.input_tensor).toBe("nchw");
    expect(updatedModel.input_pixel_format).toBe("rgb");

    const preserved320 = applySecurity15ClassPreset({
      path: "/config/models/custom_yolo11t_320.onnx",
      labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
      width: 320,
      height: 320,
      input_tensor: "nchw",
      input_pixel_format: "rgb",
      model_type: "yolox",
    });
    expect(preserved320.path).toBe("/config/models/custom_yolo11t_320.onnx");
    expect(preserved320.labelmap_path).toBe("/openvino-model/ninjanvr_security_15cl.txt");
    expect(preserved320.model_type).toBe("yolo-generic");
    expect(preserved320.width).toBe(320);

    const cliCmd = buildLocalFinetuneCliCommand({
      nvrUrl: "https://nvr.example.com:8971/",
      datasetPath: "ninjanvr-dataset.zip",
      modelArch: "yolo11s.pt",
      imgsz: 640,
      epochs: 30,
      freeze: 10,
      nvrToken: "$NINJANVR_API_TOKEN",
    });
    expect(cliCmd).toBe(
      "python3 scripts/ai/train_ninjanvr_model.py --mode finetune --dataset ninjanvr-dataset.zip --model yolo11s.pt --imgsz 640 --epochs 30 --freeze 10 --nvr-url https://nvr.example.com:8971 --nvr-token $NINJANVR_API_TOKEN"
    );
    expect(COLAB_MODEL_FACTORY_URL).toContain("notebooks/NinjaNVR_Model_Factory.ipynb");
  });
});

describe("Dataset Studio Sample Queue filter options", () => {
  test("every option carries its own summary count, independent of the active filter", () => {
    const opts = buildDatasetSampleFilterOptions(
      {
        total_samples: 60,
        true_positive_count: 12,
        ai_annotated_count: 40,
        hard_negative_count: 8,
      },
      false
    );
    expect(opts.map((o) => [o.value, o.count])).toEqual([
      ["all", 60],
      ["true_positive", 12],
      ["ai_annotated", 40],
      ["hard_negative", 8],
    ]);
    expect(opts[0].label).toBe("ALL SAMPLES");
  });

  test("falls back to zero counts and never reports a total below the kind sum", () => {
    expect(buildDatasetSampleFilterOptions(null, false).every((o) => o.count === 0)).toBe(true);
    const inconsistent = buildDatasetSampleFilterOptions(
      { total_samples: 3, true_positive_count: 2, ai_annotated_count: 2, hard_negative_count: 1 },
      true
    );
    expect(inconsistent[0].count).toBe(5);
    expect(inconsistent[0].label).toBe("TODAS AS AMOSTRAS");
  });

  test("selectBatchAutoAnnotateCandidates and computeBatchProgressMetrics track batch AI annotation progress and ETA", () => {
    const mockSamples = [
      {
        id: "ds_1",
        camera: "escritorio",
        split: "train" as const,
        sample_kind: "true_positive" as const,
        image_path: "/media/ninjanvr/dataset/ds_1.webp",
        width: 1920,
        height: 1080,
        annotations: [
          {
            class_id: 0,
            label: "person",
            cx: 0.5,
            cy: 0.5,
            w: 0.2,
            h: 0.4,
            score: 0.9,
            source: "event",
          },
        ],
        verified: false,
        night_mode: false,
        created_at: 1000,
      },
      {
        id: "ds_2",
        camera: "sala",
        split: "train" as const,
        sample_kind: "ai_annotated" as const,
        image_path: "/media/ninjanvr/dataset/ds_2.webp",
        width: 1920,
        height: 1080,
        annotations: [
          {
            class_id: 0,
            label: "person",
            cx: 0.5,
            cy: 0.5,
            w: 0.2,
            h: 0.4,
            score: 0.95,
            source: "ai_teacher",
          },
        ],
        verified: true,
        night_mode: false,
        created_at: 1001,
      },
      {
        id: "ds_3",
        camera: "sala",
        split: "val" as const,
        sample_kind: "hard_negative" as const,
        image_path: "/media/ninjanvr/dataset/ds_3.webp",
        width: 1920,
        height: 1080,
        annotations: [],
        verified: true,
        night_mode: false,
        created_at: 1002,
      },
    ];

    const pending = selectBatchAutoAnnotateCandidates(mockSamples, "pending");
    expect(pending.map((s) => s.id)).toEqual(["ds_1"]);

    const allPos = selectBatchAutoAnnotateCandidates(mockSamples, "all_positives");
    expect(allPos.map((s) => s.id)).toEqual(["ds_1", "ds_2"]);

    const metricsRunning = computeBatchProgressMetrics(
      {
        active: true,
        mode: "all_positives",
        total: 10,
        completed: 4,
        succeeded: 4,
        failed: 0,
        boxesGenerated: 16,
        currentSampleId: "ds_5",
        currentCamera: "escritorio",
        startedAtMs: 10_000,
        finishedAtMs: null,
        cancelled: false,
      },
      18_000
    );
    expect(metricsRunning.pct).toBe(40);
    expect(metricsRunning.remaining).toBe(6);
    expect(metricsRunning.elapsedSec).toBe(8);
    expect(metricsRunning.etaSec).toBe(12);
  });
});
