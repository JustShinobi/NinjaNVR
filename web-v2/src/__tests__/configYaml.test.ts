import { describe, expect, test } from "bun:test";
import { buildCleanYamlFromDraft } from "../utils/configYaml";
import { NinjaConfigResponse } from "../types/nvr";

describe("buildCleanYamlFromDraft dual-backend detectors & ninjanvr defaults", () => {
  test("serializes openvino and onnxruntime detector entries with CUDA/TensorRT/ROCM devices", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      cameras: {},
      detectors: {
        detector01: {
          type: "openvino",
          device: "CPU",
        },
        detector02: {
          type: "onnxruntime",
          device: "CUDA",
          model: {
            path: "/config/models/yolo11s.onnx",
            labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
            width: 640,
            height: 640,
            input_tensor: "nchw",
            input_pixel_format: "rgb",
            model_type: "yolo-generic",
          },
        },
      },
    };

    const yamlText = buildCleanYamlFromDraft(draft);
    expect(yamlText).toContain("type: openvino");
    expect(yamlText).toContain("type: onnxruntime");
    expect(yamlText).toContain("device: CUDA");
    expect(yamlText).toContain("path: /config/models/yolo11s.onnx");
    expect(yamlText).toContain("topic_prefix: ninjanvr");
    expect(yamlText).toContain("cookie_name: ninjanvr_token");
    expect(yamlText).toContain("path: /config/ninjanvr.db");
    expect(yamlText.toLowerCase()).not.toContain("frigate");
  });
});
