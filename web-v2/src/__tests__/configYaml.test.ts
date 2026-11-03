import { describe, expect, test } from "bun:test";
import { NinjaConfigResponse } from "../types/nvr";
import { buildCleanYamlFromDraft } from "../utils/configYaml";

describe("buildCleanYamlFromDraft", () => {
  test("restores scrubbed RTSP credentials from originalRawYaml and serializes all config sections", () => {
    const rawYaml = `
go2rtc:
  streams:
    front_door:
      - rtsp://admin:supersecret@192.0.2.10:554/h264Preview_01_main
    side_gate:
      - rtsps://operator:gatepass@192.0.2.20:322/live/ch0
`;

    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      mqtt: {
        enabled: true,
        host: "10.0.0.5",
        port: 1883,
        topic_prefix: "ninjanvr",
        client_id: "ninjanvr-prod",
        stats_interval: 30,
        user: "mqtt_user",
        password: "mqtt_password",
      },
      auth: {
        enabled: true,
        reset_admin_password: false,
        cookie_name: "ninjanvr_token",
        cookie_secure: true,
        session_length: 43200,
        refresh_time: 900,
        hash_iterations: 600000,
        trusted_proxies: ["127.0.0.1/32"],
      },
      tls: {
        enabled: true,
      },
      database: {
        path: "/config/ninjanvr.db",
      },
      detectors: {
        detector01: {
          type: "openvino",
          device: "GPU",
        },
      },
      model: {
        width: 320,
        height: 320,
        input_tensor: "nhwc",
        input_pixel_format: "bgr",
        path: "/openvino-model/ssdlite_mobilenet_v2.xml",
        labelmap_path: "/openvino-model/coco_91cl_bkgr.txt",
        model_type: "ssd",
      },
      detect: {
        enabled: true,
        width: 1280,
        height: 720,
        fps: 5,
        min_initialized: 2,
        max_disappeared: 25,
      },
      objects: {
        track: ["person", "car"],
        filters: {
          person: {
            min_area: 500,
            max_area: 100000,
            min_ratio: 0.2,
            max_ratio: 5.0,
            min_score: 0.65,
            threshold: 0.75,
          },
        },
      },
      record: {
        enabled: true,
        expire_interval: 60,
        continuous: { days: 7 },
        motion: { days: 14 },
        cold_compaction: {
          enabled: true,
          after_hours: 72,
          crf: 32,
          idle_only: true,
          codec: "h264",
          hwaccel: "vaapi",
          preset: "veryfast",
        },
      },
      snapshots: {
        enabled: true,
        timestamp: true,
        bounding_box: true,
        crop: false,
        quality: 85,
      },
      go2rtc: {
        streams: {
          front_door: ["rtsp://*:*@192.0.2.10:554/h264Preview_01_main"],
          side_gate: "rtsps://*:*@192.0.2.20:322/live/ch1_modified",
        },
      },
      cameras: {
        front_door: {
          name: "front_door",
          enabled: true,
          ffmpeg: {
            inputs: [
              {
                path: "rtsp://*:*@192.0.2.10:554/h264Preview_01_main",
                input_args: "preset-rtsp-restream",
                roles: ["record", "detect"],
              },
            ],
          },
          detect: {
            enabled: true,
            width: 1280,
            height: 720,
            fps: 6,
            min_initialized: 2,
            max_disappeared: 25,
            stationary: {
              interval: 40,
              threshold: 45,
              classifier: true,
            },
          },
          motion: {
            enabled: true,
            threshold: 28,
            lightning_threshold: 0.75,
            improve_contrast: true,
            contour_area: 12,
            frame_height: 100,
            mask: {
              sky: {
                friendly_name: "Sky Mask",
                enabled: true,
                coordinates: "0,0,1,0,1,0.2,0,0.2",
              },
            },
          },
          objects: {
            track: ["person", "car"],
            filters: {},
            mask: {},
          },
          record: {
            enabled: true,
            continuous: { days: 7 },
            motion: { days: 14 },
            preview: { quality: "high" },
            cold_compaction: {
              enabled: true,
              after_hours: 48,
              crf: 33,
              idle_only: true,
            },
          },
          snapshots: {
            enabled: true,
            timestamp: false,
            bounding_box: true,
            crop: false,
            quality: 80,
            selection: "best_overall",
          },
          zones: {
            driveway: {
              coordinates: "0.1,0.5,0.9,0.5,0.9,0.95,0.1,0.95",
              inertia: 3,
              loitering_time: 5,
            },
          },
          lines: {
            entry_gate: {
              points: [0.2, 0.5, 0.8, 0.5],
              direction: "a_to_b",
            },
          },
          privacy_mask: ["0.0,0.0,0.1,0.0,0.1,0.1,0.0,0.1"],
        },
      },
    };

    const yaml = buildCleanYamlFromDraft(draft, rawYaml);

    // Exact URL credential match restored
    expect(yaml).toContain("rtsp://admin:supersecret@192.0.2.10:554/h264Preview_01_main");
    // Host-matched URL credential restored even when path changed
    expect(yaml).toContain("rtsps://operator:gatepass@192.0.2.20:322/live/ch1_modified");
    expect(yaml).not.toContain("rtsp://*:*@");
    expect(yaml).not.toContain("rtsps://*:*@");

    // MQTT, Auth, Detector, Camera, Cold compaction sections serialized
    expect(yaml).toContain("client_id: ninjanvr-prod");
    expect(yaml).toContain("user: mqtt_user");
    expect(yaml).toContain("cookie_name: ninjanvr_token");
    expect(yaml).toContain("device: GPU");
    expect(yaml).toContain("friendly_name: Sky Mask");
    expect(yaml).toContain("loitering_time: 5");
    expect(yaml).toContain("direction: a_to_b");
    expect(yaml).toContain("after_hours: 72");
    expect(yaml).toContain("version: 0.18-0");
  });

  test("applies fallback defaults when optional sections are omitted", () => {
    const minimalDraft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      cameras: {
        patio: {
          name: "patio",
          enabled: true,
          ffmpeg: {
            inputs: [{ path: "rtsp://192.0.2.50:554/live", roles: [] }],
          },
          detect: { enabled: true, width: 640, height: 360, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          objects: { track: ["person"] },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    const yaml = buildCleanYamlFromDraft(minimalDraft, "");
    expect(yaml).toContain("topic_prefix: ninjanvr");
    expect(yaml).toContain("path: /config/ninjanvr.db");
    expect(yaml).toContain("type: openvino");
    expect(yaml).toContain('"Stream 1": patio_sub');
    expect(yaml).toContain("input_args: preset-rtsp-restream");
    expect(yaml).toContain("- detect");
  });

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

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("type: openvino");
    expect(yamlText).toContain("type: onnxruntime");
    expect(yamlText).toContain("device: CUDA");
    expect(yamlText).toContain("path: /config/models/yolo11s.onnx");
    expect(yamlText).toContain("topic_prefix: ninjanvr");
    expect(yamlText).toContain("cookie_name: ninjanvr_token");
    expect(yamlText).toContain("path: /config/ninjanvr.db");
    expect(yamlText.toLowerCase()).not.toContain("frigate");
  });

  test("serializes camera_groups and camera.ui order and dashboard settings", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      camera_groups: {
        perimeter: {
          cameras: ["north_gate", "south_gate"],
          icon: "shield",
          order: 1,
        },
      },
      cameras: {
        north_gate: {
          enabled: true,
          ui: { order: 2, dashboard: true },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
        south_gate: {
          enabled: true,
          ui: { order: 1, dashboard: false },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("camera_groups:");
    expect(yamlText).toContain("perimeter:");
    expect(yamlText).toContain("- north_gate");
    expect(yamlText).toContain("- south_gate");
    expect(yamlText).toContain("icon: shield");
    expect(yamlText).toContain("dashboard: false");
    expect(yamlText).toContain("order: 2");
  });
});
