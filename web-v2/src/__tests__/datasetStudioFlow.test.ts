import { describe, expect, test } from "bun:test";
import type { DatasetSampleItem, DatasetSummaryResponse } from "../types/nvr";
import {
  activeModelDisplayName,
  buildDatasetPipeline,
  buildDatasetReadinessReport,
  canConfirmSample,
  computeDatasetQueueCounts,
  datasetCameraCounts,
  datasetClassCoverage,
  datasetQueueStatus,
  filterDatasetQueue,
  minAnnotationScore,
  normalizeDragBox,
  resolveDatasetNextAction,
  stepDatasetQueue,
  stepStickyDatasetQueue,
} from "../utils/datasetStudio";
import { selectBatchAutoAnnotateCandidates } from "../views/settings/VisionAndAiPanels";

function sample(
  id: string,
  over: Partial<DatasetSampleItem> = {}
): DatasetSampleItem {
  return {
    id,
    camera: "sala",
    split: "train",
    sample_kind: "ai_annotated",
    image_path: `/media/${id}.webp`,
    width: 1920,
    height: 1080,
    annotations: [],
    verified: true,
    verified_by: "ai",
    night_mode: false,
    created_at: 1000,
    ...over,
  };
}

const box = (score: number, source = "ai_teacher") => ({
  class_id: 0,
  label: "person",
  cx: 0.5,
  cy: 0.5,
  w: 0.2,
  h: 0.4,
  score,
  source,
});

const QUEUE: DatasetSampleItem[] = [
  sample("ai_high", { annotations: [box(0.92)], created_at: 5 }),
  sample("ai_low", { annotations: [box(0.92), box(0.41)], created_at: 4, camera: "quarto" }),
  sample("ai_none", { annotations: [], created_at: 6 }),
  sample("manual", { verified_by: "manual", sample_kind: "true_positive", created_at: 3 }),
  sample("legacy_verified", { verified_by: null, sample_kind: "true_positive", created_at: 2 }),
  sample("pending", { verified: false, verified_by: null, sample_kind: "true_positive", created_at: 7 }),
  sample("bg", { sample_kind: "hard_negative", verified_by: "manual", created_at: 1 }),
];

describe("Dataset Studio queue: AI vs manual verification provenance", () => {
  test("classifies each sample by kind and verifier", () => {
    expect(datasetQueueStatus(QUEUE[0])).toBe("ai");
    expect(datasetQueueStatus(QUEUE[3])).toBe("manual");
    // A verified sample without a recorded verifier predates provenance: treat as operator.
    expect(datasetQueueStatus(QUEUE[4])).toBe("manual");
    expect(datasetQueueStatus(QUEUE[5])).toBe("pending");
    expect(datasetQueueStatus(QUEUE[6])).toBe("background");
  });

  test("counts add up to the total, with background kept out of the status tabs", () => {
    const c = computeDatasetQueueCounts(QUEUE);
    expect(c).toEqual({ pending: 1, ai: 3, manual: 2, background: 1, objects: 6, all: 7 });
    expect(c.pending + c.ai + c.manual + c.background).toBe(c.all);
  });

  test("AI tab orders by lowest box confidence first, unscored samples last", () => {
    const ai = filterDatasetQueue(QUEUE, { tab: "ai", camera: "all", kind: "all" });
    expect(ai.map((s) => s.id)).toEqual(["ai_low", "ai_high", "ai_none"]);
    expect(minAnnotationScore(QUEUE[1])).toBe(0.41);
    expect(minAnnotationScore(QUEUE[2])).toBeNull();
  });

  test("other tabs are newest first and respect camera and kind filters", () => {
    expect(filterDatasetQueue(QUEUE, { tab: "all", camera: "all", kind: "all" })[0].id).toBe("pending");
    expect(
      filterDatasetQueue(QUEUE, { tab: "all", camera: "quarto", kind: "all" }).map((s) => s.id)
    ).toEqual(["ai_low"]);
    expect(
      filterDatasetQueue(QUEUE, { tab: "all", camera: "all", kind: "background" }).map((s) => s.id)
    ).toEqual(["bg"]);
    expect(
      filterDatasetQueue(QUEUE, { tab: "all", camera: "all", kind: "objects" }).some((s) => s.id === "bg")
    ).toBe(false);
    expect(datasetCameraCounts(QUEUE)).toEqual([
      ["quarto", 1],
      ["sala", 6],
    ]);
  });

  test("queue stepping clamps at both ends and recovers from a missing selection", () => {
    const list = QUEUE.slice(0, 3);
    expect(stepDatasetQueue(list, "ai_high", 1)).toBe("ai_low");
    expect(stepDatasetQueue(list, "ai_high", -1)).toBe("ai_high");
    expect(stepDatasetQueue(list, "ai_none", 1)).toBe("ai_none");
    expect(stepDatasetQueue(list, "gone", 1)).toBe("ai_high");
    expect(stepDatasetQueue([], "ai_high", 1)).toBeNull();
  });

  test("navigation resumes from the queue slot of a sample that just left the tab", () => {
    const list = QUEUE.slice(0, 3); // ai_high, ai_low, ai_none
    // "edited" sat at index 1 and is now gone: next lands on what replaced it.
    expect(stepStickyDatasetQueue(list, "edited", 1, 1)).toBe("ai_low");
    expect(stepStickyDatasetQueue(list, "edited", 1, -1)).toBe("ai_high");
    expect(stepStickyDatasetQueue(list, "edited", 9, 1)).toBe("ai_none");
    expect(stepStickyDatasetQueue(list, "ai_high", 0, 1)).toBe("ai_low");
    expect(stepStickyDatasetQueue([], "edited", 0, 1)).toBeNull();
  });

  test("a frame with objects needs at least one box before it can be confirmed", () => {
    expect(canConfirmSample(QUEUE[0])).toBe(true);
    expect(canConfirmSample(QUEUE[5])).toBe(false);
    expect(canConfirmSample(QUEUE[6])).toBe(true);
  });

  test("batch AI never re-annotates hand-checked samples", () => {
    expect(selectBatchAutoAnnotateCandidates(QUEUE, "pending").map((s) => s.id)).toEqual(["pending"]);
    const redo = selectBatchAutoAnnotateCandidates(QUEUE, "all_positives").map((s) => s.id);
    expect(redo).toEqual(["ai_high", "ai_low", "ai_none", "legacy_verified", "pending"]);
    expect(redo).not.toContain("manual");
    expect(redo).not.toContain("bg");
  });
});

describe("Dataset Studio drawing", () => {
  test("normalizes a drag in any direction and clamps to the frame", () => {
    expect(normalizeDragBox([0.6, 0.8], [0.2, 0.1])).toEqual([0.2, 0.1, 0.6, 0.8]);
    expect(normalizeDragBox([-0.2, 0.5], [0.3, 1.4])).toEqual([0, 0.5, 0.3, 1]);
  });

  test("ignores stray clicks smaller than the minimum box", () => {
    expect(normalizeDragBox([0.5, 0.5], [0.505, 0.6])).toBeNull();
    expect(normalizeDragBox([0.5, 0.5], [0.5, 0.5], 0)).toEqual([0.5, 0.5, 0.5, 0.5]);
  });
});

const READY: Partial<DatasetSummaryResponse> = {
  total_samples: 71,
  true_positive_count: 4,
  ai_annotated_count: 55,
  hard_negative_count: 12,
  ai_verified_count: 55,
  manual_verified_count: 4,
  pending_count: 0,
  train_count: 58,
  val_count: 13,
  night_mode_count: 3,
  by_camera: { escritorio: 27, sala: 19, quarto: 15, cozinha: 10 },
  by_class: { person: 180, face: 30, car: 4 },
};

describe("Dataset Studio readiness report", () => {
  test("flags thin cameras as a warning, not a blocker", () => {
    const r = buildDatasetReadinessReport(READY, true);
    const cams = r.checks.find((c) => c.id === "cameras");
    expect(cams?.severity).toBe("warn");
    expect(cams?.detail).toContain("cozinha");
    expect(r.verdict).toBe("warnings");
    expect(r.blockCount).toBe(0);
    expect(r.checks.find((c) => c.id === "verification")?.detail).toContain("55 pela IA");
  });

  test("pending samples, missing validation and tiny datasets block training", () => {
    const r = buildDatasetReadinessReport(
      { ...READY, total_samples: 10, ai_annotated_count: 0, true_positive_count: 8, hard_negative_count: 2, pending_count: 3, val_count: 0, train_count: 10 },
      false
    );
    const blocked = r.checks.filter((c) => c.severity === "block").map((c) => c.id);
    expect(blocked).toEqual(["verification", "minimum", "split"]);
    expect(r.verdict).toBe("blocked");
  });

  test("no night frames and skewed balance are warnings", () => {
    const r = buildDatasetReadinessReport(
      { ...READY, total_samples: 60, night_mode_count: 0, hard_negative_count: 1, by_camera: { sala: 60 } },
      true
    );
    expect(r.checks.find((c) => c.id === "night")?.severity).toBe("warn");
    expect(r.checks.find((c) => c.id === "balance")?.severity).toBe("warn");
  });

  test("an empty summary never claims readiness", () => {
    const r = buildDatasetReadinessReport(null, true);
    expect(r.verdict).toBe("blocked");
    expect(r.checks.find((c) => c.id === "minimum")?.severity).toBe("block");
  });

  test("class coverage is sorted by box count with relative bar widths", () => {
    expect(datasetClassCoverage(READY)).toEqual([
      { label: "person", count: 180, pct: 100 },
      { label: "face", count: 30, pct: 17 },
      { label: "car", count: 4, pct: 2 },
    ]);
  });
});

describe("Dataset Studio pipeline rail and next action", () => {
  test("one primary action, driven by what is left to do", () => {
    const counts = computeDatasetQueueCounts(QUEUE);
    const ready = buildDatasetReadinessReport(READY, true);
    expect(resolveDatasetNextAction(counts, ready, "annotate", true)).toEqual({
      kind: "batch-annotate",
      label: "Verificar 1 com IA",
    });
    const done = { ...counts, pending: 0 };
    expect(resolveDatasetNextAction(done, ready, "annotate", true).kind).toBe("goto-readiness");
    expect(resolveDatasetNextAction(done, ready, "readiness", true).kind).toBe("goto-train");
    expect(resolveDatasetNextAction(done, ready, "train", false).label).toBe("Download dataset .zip");
    const blocked = buildDatasetReadinessReport({ ...READY, val_count: 0 }, true);
    expect(resolveDatasetNextAction(done, blocked, "readiness", true).kind).toBe("collect");
    expect(resolveDatasetNextAction(computeDatasetQueueCounts([]), ready, "annotate", true).kind).toBe("collect");
  });

  test("rail reports pending vs verified and readiness tone", () => {
    const counts = computeDatasetQueueCounts(QUEUE);
    const steps = buildDatasetPipeline(counts, buildDatasetReadinessReport(READY, true), "ssdlite_mobilenet_v2", true);
    expect(steps.map((s) => s.id)).toEqual(["annotate", "readiness", "train", "deploy"]);
    expect(steps[0].detail).toBe("1 pendentes · 6 verificadas");
    expect(steps[0].tone).toBe("warn");
    expect(steps[1].tone).toBe("warn");
    expect(steps[3].target).toBe("train");
    expect(steps[3].detail).toBe("Ativo: ssdlite_mobilenet_v2");
  });

  test("active model name drops folders and extensions", () => {
    expect(activeModelDisplayName("/openvino-model/ssdlite_mobilenet_v2.xml")).toBe("ssdlite_mobilenet_v2");
    expect(activeModelDisplayName("C:\\models\\ninjanvr_yolo11s.onnx")).toBe("ninjanvr_yolo11s");
    expect(activeModelDisplayName("")).toBe("—");
  });
});
