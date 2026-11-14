import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "bun:test";
import { NinjaConfigResponse } from "../types/nvr";
import {
  buildCleanYamlFromDraft,
  buildGenAiProviderChangePatch,
  formatLicensePlateBadge,
  normalizeDiscoveredGenAiModels,
  normalizeLicensePlate,
  resolveKnownPlateLabel,
  validateLicensePlateFormat,
} from "../utils/configYaml";
import {
  applyEventSubLabelUpdate,
  applyReviewSubLabelUpdate,
  clearFaceProfileFromEvents,
  clearFaceProfileFromReviews,
  formatAiVerificationBadge,
  formatDetectedAttributePill,
  formatEventLabelWithSubLabel,
  formatSubLabelBadge,
  matchesLicensePlateFilter,
} from "../views/ExploreWorkspace";
import {
  TacticalPtzHudOverlay,
  buildPtzMovePayload,
  expandSimulatedFleet,
  formatRoiStateBadge,
  isCameraPtzEnabled,
  resolveCameraPtzPresets,
  resolveCameraRoiStateBadges,
  resolveRealCameraSource,
  SIMULATED_FLEET_GROUPS,
} from "../views/LiveWorkspace";
import { useNvrStore } from "../store/useNvrStore";
import {
  buildModelUploadPayload,
  computeDatasetReadinessMetrics,
  DEFAULT_ATTRIBUTE_LABELS,
  buildStateRoiConfig,
  formatDhash64Hex,
  formatSignatureBbox,
  formatYoloAnnotationLine,
  NINJANVR_SECURITY_CLASSES,
  normalizeFaceProfileName,
  parseFacesApiResponse,
  parseRoiClassesInput,
  parseRoiCropInput,
  removeKnownPlateEntry,
  slugifyRoiLocationName,
  STANDARD_LABELS,
  STATE_ROI_SCENARIO_PRESETS,
  toggleAttributeLabelList,
  toggleSubLabelRuleList,
  upsertKnownPlateEntry,
  xyxyToYoloNormalized,
  yoloNormalizedToXyxy,
} from "../views/settings/VisionAndAiPanels";

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

  test("preserves objects.attribute_labels and snapshots.selection: best_attribute in YAML serialization", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      objects: {
        track: ["person", "car", "package"],
        attribute_labels: ["face", "license_plate", "correios", "mercado_livre"],
        filters: {},
      },
      snapshots: {
        enabled: true,
        bounding_box: true,
        quality: 85,
        selection: "best_attribute",
      },
      cameras: {
        portao: {
          name: "portao",
          enabled: true,
          ffmpeg: {
            inputs: [{ path: "rtsp://192.0.2.88:554/sub", roles: ["detect"] }],
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          objects: {
            track: ["person", "car"],
            attribute_labels: ["face", "license_plate", "correios"],
          },
          zones: {},
          record: { enabled: true },
          snapshots: {
            enabled: true,
            selection: "best_attribute",
          },
        },
      },
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("attribute_labels:");
    expect(yamlText).toContain("- face");
    expect(yamlText).toContain("- license_plate");
    expect(yamlText).toContain("- correios");
    expect(yamlText).toContain("- mercado_livre");
    expect(yamlText).toContain("selection: best_attribute");
  });

  test("formats sub_label badges, detected_attributes pills, and attribute label chip lists", () => {
    expect(STANDARD_LABELS).toContain("package");
    expect(STANDARD_LABELS).toContain("waste_bin");
    expect(DEFAULT_ATTRIBUTE_LABELS).toContain("face");
    expect(DEFAULT_ATTRIBUTE_LABELS).toContain("license_plate");
    expect(DEFAULT_ATTRIBUTE_LABELS).toContain("correios");

    // Toggle off an existing attribute label and toggle on a new one
    const withoutFace = toggleAttributeLabelList(
      ["face", "license_plate", "correios"],
      "face"
    );
    expect(withoutFace).toEqual(["license_plate", "correios"]);
    const withDhl = toggleAttributeLabelList(withoutFace, "dhl");
    expect(withDhl).toEqual(["license_plate", "correios", "dhl"]);

    // Format primary label with explicit sub_label or child attribute fallback
    expect(formatEventLabelWithSubLabel("car", "correios", [])).toBe(
      "CAR · CORREIOS"
    );
    expect(
      formatEventLabelWithSubLabel("person", null, [
        {
          label: "face",
          score: 0.92,
          norm_bbox: [0.2, 0.1, 0.3, 0.25],
          area_px: 1600,
        },
      ])
    ).toBe("PERSON · FACE");
    expect(formatEventLabelWithSubLabel("person", null, [])).toBe("PERSON");

    expect(
      formatDetectedAttributePill({
        label: "license_plate",
        score: 0.94,
        norm_bbox: [0.4, 0.6, 0.55, 0.68],
        area_px: 2400,
      })
    ).toBe("LICENSE_PLATE 94% · 2400px²");
  });

  test("serializes ui.floorplan (rooms, walls, opacity, background_data_url) and camera.map placements", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      ui: {
        time_format: "browser",
        unit_system: "metric",
        floorplan: {
          background_data_url: "data:image/svg+xml;base64,PHN2Zy8+",
          opacity: 0.45,
          rooms: [
            {
              id: "escritorio",
              label: "ESCRITÓRIO",
              x: 0.08,
              y: 0.1,
              w: 0.38,
              h: 0.36,
              color: "#0ea5e9",
            },
          ],
          walls: [
            { points: [0.08, 0.46, 0.46, 0.46], label: "Parede Sul", kind: "wall" },
            { points: [0.46, 0.16, 0.46, 0.32], label: "Janela Leste", kind: "window" },
          ],
        },
      },
      cameras: {
        escritorio_cam: {
          enabled: true,
          map: {
            x: 0.22,
            y: 0.18,
            angle_deg: 135,
            fov_deg: 95,
            range: 0.32,
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("ui:");
    expect(yamlText).toContain("floorplan:");
    expect(yamlText).toContain('background_data_url: "data:image/svg+xml;base64,PHN2Zy8+"');
    expect(yamlText).toContain("opacity: 0.45");
    expect(yamlText).toContain("id: escritorio");
    expect(yamlText).toContain("label: ESCRITÓRIO");
    expect(yamlText).toContain("x: 0.08");
    expect(yamlText).toContain("y: 0.1");
    expect(yamlText).toContain("w: 0.38");
    expect(yamlText).toContain("h: 0.36");
    expect(yamlText).toContain('color: "#0ea5e9"');
    expect(yamlText).toContain("walls:");
    expect(yamlText).toContain("points:");
    expect(yamlText).toContain("0.46");
    expect(yamlText).toContain("label: Janela Leste");
    expect(yamlText).toContain("kind: window");
    expect(yamlText).toContain("map:");
    expect(yamlText).toContain("angle_deg: 135");
    expect(yamlText).toContain("fov_deg: 95");
    expect(yamlText).toContain("range: 0.32");
  });

  test("serializes genai Vision AI Judge configuration and formats false-positive memory & AI badges", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      genai: {
        enabled: true,
        provider: "ollama",
        base_url: "http://127.0.0.1:11434",
        api_key_env: "OLLAMA_API_KEY",
        model: "qwen2.5vl:7b",
        verify_ambiguous_alerts: true,
        ambiguous_score_min: 0.45,
        ambiguous_score_max: 0.72,
        auto_suppress_false_positives: true,
        timeout_ms: 5000,
      },
      cameras: {},
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("genai:");
    expect(yamlText).toContain("provider: ollama");
    expect(yamlText).toContain('base_url: "http://127.0.0.1:11434"');
    expect(yamlText).toContain("api_key_env: OLLAMA_API_KEY");
    expect(yamlText).toContain('model: "qwen2.5vl:7b"');
    expect(yamlText).toContain("verify_ambiguous_alerts: true");
    expect(yamlText).toContain("ambiguous_score_min: 0.45");
    expect(yamlText).toContain("ambiguous_score_max: 0.72");
    expect(yamlText).toContain("auto_suppress_false_positives: true");
    expect(yamlText).toContain("timeout_ms: 5000");

    expect(
      formatAiVerificationBadge({
        is_true_positive: false,
        confidence: 0.96,
        reason: "Static patio fern",
      })
    ).toBe("AI JUDGE: FALSE POSITIVE (96%)");
    expect(
      formatAiVerificationBadge({
        is_true_positive: true,
        confidence: 0.91,
        reason: "Person walking",
      })
    ).toBe("AI JUDGE: TRUE POSITIVE (91%)");

    expect(formatDhash64Hex(0x1234abcd)).toBe("0x000000001234abcd");
    expect(formatDhash64Hex(null)).toBe("—");
    expect(formatSignatureBbox([0.15, 0.25, 0.35, 0.75])).toBe(
      "[0.15, 0.25, 0.35, 0.75]"
    );

    expect(buildGenAiProviderChangePatch("gemini")).toEqual({
      provider: "gemini",
      api_key_env: "GEMINI_API_KEY",
      model: "",
    });
    expect(buildGenAiProviderChangePatch("openai")).toEqual({
      provider: "openai",
      api_key_env: "OPENAI_API_KEY",
      model: "",
    });
    expect(buildGenAiProviderChangePatch("ollama")).toEqual({
      provider: "ollama",
      api_key_env: "OLLAMA_API_KEY",
      base_url: "http://127.0.0.1:11434",
      model: "",
    });

    expect(
      normalizeDiscoveredGenAiModels({
        valid: true,
        models: [
          "models/gemini-2.5-flash",
          "gemini-2.5-pro",
          "gemini-2.5-flash",
        ],
      })
    ).toEqual(["gemini-2.5-flash", "gemini-2.5-pro"]);
  });

  test("serializes camera onvif PTZ settings, restores scrubbed ONVIF password, and serializes zone.preset", () => {
    const rawYaml = `
cameras:
  ptz_gate:
    enabled: true
    onvif:
      host: 192.0.2.90
      port: 8000
      user: onvif_admin
      password: original_onvif_secret_pw
      autotracking: true
      return_preset: entry_gate
      timeout_sec: 6
`;

    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      cameras: {
        ptz_gate: {
          enabled: true,
          onvif: {
            host: "192.0.2.90",
            port: 8000,
            user: "onvif_admin",
            password: "***",
            has_password: true,
            autotracking: true,
            return_preset: "entry_gate",
            timeout_sec: 6,
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {
            gate_zone: {
              coordinates: "0.1,0.1,0.8,0.1,0.8,0.9,0.1,0.9",
              objects: ["person", "car"],
              inertia: 3,
              loitering_time: 2,
              preset: "entry_gate",
            },
          },
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    const yamlText = buildCleanYamlFromDraft(draft, rawYaml);
    expect(yamlText).toContain("onvif:");
    expect(yamlText).toContain("host: 192.0.2.90");
    expect(yamlText).toContain("port: 8000");
    expect(yamlText).toContain("user: onvif_admin");
    expect(yamlText).toContain("password: original_onvif_secret_pw");
    expect(yamlText).not.toContain('password: "***"');
    expect(yamlText).toContain("autotracking: true");
    expect(yamlText).toContain("return_preset: entry_gate");
    expect(yamlText).toContain("timeout_sec: 6");
    expect(yamlText).toContain("preset: entry_gate");
  });

  test("renders TacticalPtzHudOverlay D-Pad and preset selector on PTZ-enabled cameras", () => {
    const nonPtzCam = {
      enabled: true,
      detect: { enabled: true, width: 1280, height: 720, fps: 5 },
      motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
      zones: {},
      record: { enabled: true },
      snapshots: { enabled: true },
    };
    expect(isCameraPtzEnabled(nonPtzCam, null)).toBe(false);
    expect(
      renderToStaticMarkup(
        React.createElement(TacticalPtzHudOverlay, {
          camera: "fixed_cam",
          cameraConfig: nonPtzCam,
        })
      )
    ).toBe("");

    const ptzCam = {
      ...nonPtzCam,
      onvif: {
        host: "192.0.2.90",
        port: 80,
        autotracking: true,
        return_preset: "home",
        timeout_sec: 5,
      },
      zones: {
        gate: {
          coordinates: "0.1,0.1,0.5,0.1,0.5,0.5,0.1,0.5",
          preset: "entry_gate",
        },
      },
    };
    const ptzInfo = {
      enabled: true,
      active_preset: "entry_gate",
      presets: [
        { token: "entry_gate", name: "entry_gate" },
        { token: "driveway", name: "driveway" },
      ],
    };

    expect(isCameraPtzEnabled(ptzCam, ptzInfo)).toBe(true);
    expect(resolveCameraPtzPresets(ptzCam, ptzInfo)).toEqual([
      "entry_gate",
      "driveway",
      "home",
    ]);

    expect(buildPtzMovePayload("left", 0.5, "continuous")).toEqual({
      mode: "continuous",
      action: "left",
      pan: -0.5,
      tilt: 0,
      zoom: 0,
      speed: 0.5,
    });
    expect(buildPtzMovePayload("zoom_in", 0.4, "relative")).toEqual({
      mode: "relative",
      action: "zoom_in",
      pan: 0,
      tilt: 0,
      zoom: 0.4,
      speed: 0.4,
    });

    const hudMarkup = renderToStaticMarkup(
      React.createElement(TacticalPtzHudOverlay, {
        camera: "ptz_gate",
        cameraConfig: ptzCam,
        ptzInfo,
      })
    );
    expect(hudMarkup).toContain("PTZ:entry_gate");
    expect(hudMarkup).toContain('aria-label="PTZ Pan Left ptz_gate"');
    expect(hudMarkup).toContain('aria-label="PTZ Tilt Up ptz_gate"');
    expect(hudMarkup).toContain('aria-label="PTZ Tilt Down ptz_gate"');
    expect(hudMarkup).toContain('aria-label="PTZ Pan Right ptz_gate"');
    expect(hudMarkup).toContain('aria-label="PTZ Zoom In ptz_gate"');
    expect(hudMarkup).toContain('aria-label="PTZ Zoom Out ptz_gate"');
    expect(hudMarkup).toContain('aria-label="PTZ Stop ptz_gate"');
    expect(hudMarkup).toContain('aria-label="PTZ Preset Selector ptz_gate"');
    expect(hudMarkup).toContain(">driveway</option>");
  });

  test("serializes face_recognition and review.alerts.exclude_sub_labels in configYaml and validates face gallery helpers", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      face_recognition: {
        enabled: true,
        model_path: "/config/models/arcface_r50.onnx",
        similarity_threshold: 0.68,
        min_face_area: 750,
        unknown_alert: true,
      },
      review: {
        alerts: {
          enabled: true,
          labels: ["person", "car"],
          sub_labels: ["unknown"],
          exclude_sub_labels: ["Bruno", "Alice"],
          required_zones: [],
          cutoff_time: 40,
        },
        detections: {
          enabled: true,
          required_zones: [],
          cutoff_time: 30,
        },
      },
      cameras: {
        front_door: {
          name: "front_door",
          enabled: true,
          ffmpeg: {
            inputs: [{ path: "rtsp://192.0.2.50:554/sub", roles: ["detect"] }],
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          objects: { track: ["person"] },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("face_recognition:");
    expect(yamlText).toContain("model_path: /config/models/arcface_r50.onnx");
    expect(yamlText).toContain("similarity_threshold: 0.68");
    expect(yamlText).toContain("min_face_area: 750");
    expect(yamlText).toContain("unknown_alert: true");
    expect(yamlText).toContain("exclude_sub_labels:");
    expect(yamlText).toContain("- Bruno");
    expect(yamlText).toContain("- Alice");
    expect(yamlText).toContain("sub_labels:");
    expect(yamlText).toContain("- unknown");

    // Validate sub_label badge formatting for known vs unknown
    expect(formatSubLabelBadge(null)).toBeNull();
    expect(formatSubLabelBadge("Bruno", 0.88)).toEqual({
      text: "ID: BRUNO 88%",
      isUnknown: false,
      isKnown: true,
    });
    expect(formatSubLabelBadge("unknown", 0.42)).toEqual({
      text: "UNKNOWN VISITOR 42%",
      isUnknown: true,
      isKnown: false,
    });

    // Validate face profile normalization, exclude_sub_labels toggle, and /api/faces parser
    expect(normalizeFaceProfileName("  Bruno  ")).toBe("Bruno");
    expect(normalizeFaceProfileName("../etc/passwd")).toBe("");
    expect(normalizeFaceProfileName("unknown")).toBe("");

    const toggledIn = toggleSubLabelRuleList(["Alice"], "Bruno");
    expect(toggledIn).toEqual(["Alice", "Bruno"]);
    const toggledOut = toggleSubLabelRuleList(toggledIn, "bruno");
    expect(toggledOut).toEqual(["Alice"]);

    const parsedProfiles = parseFacesApiResponse({
      profiles: [
        {
          name: "Bruno",
          sample_count: 2,
          sample_files: ["a.jpg", "b.jpg"],
          updated_at: 1700000000,
        },
      ],
    });
    expect(parsedProfiles).toEqual([
      {
        name: "Bruno",
        sample_count: 2,
        sample_files: ["a.jpg", "b.jpg"],
        updated_at: 1700000000,
      },
    ]);

    // Validate event & review sub_label state update helpers
    const updatedEvents = applyEventSubLabelUpdate(
      [
        {
          id: "ev_1",
          camera: "front_door",
          label: "person",
          sub_label: null,
          zones: [],
          start_time: 1000,
          end_time: 1005,
          top_score: 0.9,
          score: 0.9,
          has_clip: true,
          has_snapshot: true,
          retain_indefinitely: false,
          false_positive: false,
          box: null,
        },
      ],
      "ev_1",
      "Bruno",
      0.91
    );
    expect(updatedEvents[0].sub_label).toBe("Bruno");
    expect(updatedEvents[0].sub_label_score).toBe(0.91);

    const updatedReviews = applyReviewSubLabelUpdate(
      [
        {
          id: "rev_1",
          camera: "front_door",
          start_time: 1000,
          end_time: 1010,
          severity: "alert",
          thumb_path: "/thumb.webp",
          has_been_reviewed: false,
          data: { objects: ["person"], sub_labels: [] },
        },
      ],
      "rev_1",
      "Bruno"
    );
    expect(updatedReviews[0].data.sub_labels).toEqual(["Bruno"]);
  });

  test("serializes lpr configuration with known_plates and validates Mercosul/legacy plates", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      lpr: {
        enabled: true,
        detector_model_path: "/config/models/lpr_det.onnx",
        ocr_model_path: "/config/models/lpr_ocr.onnx",
        min_confidence: 0.82,
        format_regex: "^[A-Z]{3}[0-9][A-Z0-9][0-9]{2}$",
        known_plates: {
          ABC1D23: "Family SUV",
          XYZ9876: "Delivery Van",
        },
      },
      cameras: {},
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("lpr:");
    expect(yamlText).toContain("detector_model_path: /config/models/lpr_det.onnx");
    expect(yamlText).toContain("ocr_model_path: /config/models/lpr_ocr.onnx");
    expect(yamlText).toContain("min_confidence: 0.82");
    expect(yamlText).toContain("known_plates:");
    expect(yamlText).toContain("ABC1D23: Family SUV");
    expect(yamlText).toContain("XYZ9876: Delivery Van");

    expect(normalizeLicensePlate("abc-1d23 ")).toBe("ABC1D23");
    expect(validateLicensePlateFormat("ABC1D23")).toBe(true);
    expect(validateLicensePlateFormat("ABC-1234")).toBe(true);
    expect(validateLicensePlateFormat("12")).toBe(false);
    expect(
      validateLicensePlateFormat("ABC1234", "^[A-Z]{3}[0-9][A-Z][0-9]{2}$")
    ).toBe(false);

    let known = upsertKnownPlateEntry({}, "abc-1d23", "Family SUV");
    expect(known).toEqual({ ABC1D23: "Family SUV" });
    expect(resolveKnownPlateLabel("abc1d23", known)).toBe("Family SUV");
    expect(formatLicensePlateBadge("abc-1d23", known)).toBe(
      "ABC1D23 · FAMILY SUV"
    );
    known = removeKnownPlateEntry(known, "ABC1D23");
    expect(known).toEqual({});

    expect(
      matchesLicensePlateFilter(
        {
          sub_label: "Family SUV",
          data: { recognized_license_plate: "ABC1D23" },
        },
        "abc-1d"
      )
    ).toBe(true);
    expect(
      matchesLicensePlateFilter(
        {
          sub_label: null,
          data: { recognized_license_plate: "ABC1D23" },
        },
        "XYZ9999"
      )
    ).toBe(false);
  });

  test("serializes state_rois in configYaml and formats ROI state badges in LiveWorkspace", () => {
    const gateRoi = buildStateRoiConfig({
      cropInput: "0.10, 0.20, 0.60, 0.80",
      classesInput: "closed, open",
      intervalSec: 5,
      hysteresis: 3,
      anomalyState: "open",
      anomalyTimeoutSec: 180,
    });
    expect(gateRoi).toEqual({
      crop: [0.1, 0.2, 0.6, 0.8],
      classes: ["closed", "open"],
      interval_sec: 5,
      hysteresis: 3,
      anomaly_state: "open",
      anomaly_timeout_sec: 180,
    });

    expect(
      parseRoiCropInput("", [
        [0.8, 0.9],
        [0.2, 0.3],
      ])
    ).toEqual([0.2, 0.3, 0.8, 0.9]);
    expect(parseRoiClassesInput("Empty, Occupied")).toEqual(["empty", "occupied"]);

    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      cameras: {
        portao: {
          name: "portao",
          enabled: true,
          ffmpeg: {
            inputs: [{ path: "rtsp://192.0.2.88:554/sub", roles: ["detect"] }],
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          state_rois: {
            gate: gateRoi,
            parking_spot: {
              crop: [0.4, 0.5, 0.9, 0.95],
              classes: ["empty", "occupied"],
              interval_sec: 10,
              hysteresis: 4,
            },
          },
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("state_rois:");
    expect(yamlText).toContain("gate:");
    expect(yamlText).toContain("anomaly_state: open");
    expect(yamlText).toContain("anomaly_timeout_sec: 180");
    expect(yamlText).toContain("hysteresis: 3");
    expect(yamlText).toContain("parking_spot:");
    expect(yamlText).toContain("- occupied");

    // Verify LiveWorkspace ROI badge status formatting and anomaly highlighting
    const normalBadge = formatRoiStateBadge("gate", "closed", gateRoi);
    expect(normalBadge.label).toBe("GATE: CLOSED");
    expect(normalBadge.isAnomaly).toBe(false);
    expect(normalBadge.color).toBe("var(--text-primary)");

    const anomalyBadge = formatRoiStateBadge("gate", "open", gateRoi);
    expect(anomalyBadge.label).toBe("GATE: OPEN");
    expect(anomalyBadge.isAnomaly).toBe(true);
    expect(anomalyBadge.color).toBe("var(--signal-alert-text)");

    const resolvedBadges = resolveCameraRoiStateBadges(
      draft.cameras.portao,
      {
        motion: false,
        objects: [],
        states: {
          gate: "open",
          parking_spot: "empty",
        },
      },
      undefined
    );
    expect(resolvedBadges.length).toBe(2);
    expect(resolvedBadges[0].label).toBe("GATE: OPEN");
    expect(resolvedBadges[0].isAnomaly).toBe(true);
    expect(resolvedBadges[0].color).toBe("var(--signal-alert-text)");
    expect(resolvedBadges[1].label).toBe("PARKING_SPOT: EMPTY");
    expect(resolvedBadges[1].isAnomaly).toBe(false);
  });

  test("converts YOLO bounding boxes ([x1,y1,x2,y2] <-> [cx,cy,w,h]) and computes dataset readiness gauge percentages", () => {
    expect(NINJANVR_SECURITY_CLASSES.length).toBe(15);
    expect(NINJANVR_SECURITY_CLASSES[0]).toBe("person");
    expect(NINJANVR_SECURITY_CLASSES[1]).toBe("face");
    expect(NINJANVR_SECURITY_CLASSES[7]).toBe("license_plate");
    expect(NINJANVR_SECURITY_CLASSES[14]).toBe("umbrella");

    const yolo = xyxyToYoloNormalized([0.1, 0.2, 0.5, 0.8]);
    expect(yolo).toEqual({ cx: 0.3, cy: 0.5, w: 0.4, h: 0.6 });
    const roundtrip = yoloNormalizedToXyxy(yolo);
    expect(roundtrip).toEqual([0.1, 0.2, 0.5, 0.8]);

    expect(
      formatYoloAnnotationLine({
        class_id: 11,
        label: "package",
        cx: 0.65,
        cy: 0.8,
        w: 0.2,
        h: 0.2,
      })
    ).toBe("11 0.650000 0.800000 0.200000 0.200000");

    const readinessOptimal = computeDatasetReadinessMetrics({
      total_samples: 100,
      true_positive_count: 50,
      ai_annotated_count: 30,
      hard_negative_count: 20,
      train_count: 80,
      val_count: 20,
      verified_count: 90,
    });
    expect(readinessOptimal.positivePct).toBe(80);
    expect(readinessOptimal.hardNegativePct).toBe(20);
    expect(readinessOptimal.balanceScorePct).toBe(100);
    expect(readinessOptimal.isBalanced).toBe(true);
    expect(readinessOptimal.trainPct).toBe(80);
    expect(readinessOptimal.valPct).toBe(20);
    expect(readinessOptimal.verifiedPct).toBe(90);

    const payload = buildModelUploadPayload({
      filename: "custom_yolo11s.onnx",
      modelBase64: "T05OWF9W5USEVUSUM=",
    });
    expect(payload.filename).toBe("custom_yolo11s.onnx");
    expect(payload.model_type).toBe("yolo-generic");
    expect(payload.width).toBe(640);
    expect(payload.height).toBe(640);
    expect(payload.labelmap?.split("\n").length).toBe(15);
  });

  test("serializes ui.theme and ui.high_contrast in buildCleanYamlFromDraft (#89)", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      ui: {
        time_format: "24h",
        unit_system: "metric",
        theme: "light",
        high_contrast: true,
      },
      cameras: {},
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain("ui:");
    expect(yamlText).toContain("time_format: 24h");
    expect(yamlText).toContain("unit_system: metric");
    expect(yamlText).toContain("theme: light");
    expect(yamlText).toContain("high_contrast: true");
  });

  test("clears deleted face profile sub_labels from events and reviews case-insensitively and validates State ROI presets (#56, #58)", () => {
    const eventsAfterDelete = clearFaceProfileFromEvents(
      [
        {
          id: "ev_erik",
          camera: "escritorio",
          label: "person",
          sub_label: "Erik",
          sub_label_score: 0.92,
          zones: [],
          start_time: 1000,
          end_time: 1005,
          top_score: 0.95,
          score: 0.95,
          has_clip: true,
          has_snapshot: true,
          retain_indefinitely: false,
          false_positive: false,
          box: null,
          data: { sub_label: "Erik", sub_label_score: 0.92 },
        },
        {
          id: "ev_alice",
          camera: "sala",
          label: "person",
          sub_label: "Alice",
          sub_label_score: 0.88,
          zones: [],
          start_time: 1010,
          end_time: 1015,
          top_score: 0.91,
          score: 0.91,
          has_clip: true,
          has_snapshot: true,
          retain_indefinitely: false,
          false_positive: false,
          box: null,
          data: { sub_label: "Alice", sub_label_score: 0.88 },
        },
      ],
      "eRIK"
    );
    expect(eventsAfterDelete[0].sub_label).toBeNull();
    expect(eventsAfterDelete[0].sub_label_score).toBeNull();
    expect(eventsAfterDelete[0].data?.sub_label).toBeNull();
    expect(eventsAfterDelete[1].sub_label).toBe("Alice");

    const reviewsAfterDelete = clearFaceProfileFromReviews(
      [
        {
          id: "rev_erik",
          camera: "escritorio",
          start_time: 1000,
          end_time: 1010,
          severity: "alert",
          thumb_path: "/thumb.webp",
          has_been_reviewed: false,
          data: {
            objects: ["person"],
            sub_labels: ["Erik"],
            sub_label_score: 0.92,
          },
        },
      ],
      "ERIK"
    );
    expect(reviewsAfterDelete[0].data.sub_labels).toEqual([]);
    expect(reviewsAfterDelete[0].data.sub_label_score).toBeNull();

    // State ROI scenario presets & location name slugification (#58)
    expect(slugifyRoiLocationName("Porta do Escritório")).toBe("porta_do_escritorio");
    expect(slugifyRoiLocationName("Portão da Garagem #1")).toBe("portao_da_garagem_1");
    expect(STATE_ROI_SCENARIO_PRESETS.map((p) => p.id)).toEqual([
      "door_gate",
      "parking_spot",
      "package_area",
      "fixed_object",
      "custom",
    ]);
  });

  test("expands simulated camera fleet (12 / 36 / 100 channels) and resolves real physical camera sources (#72)", () => {
    const baseCams = ["escritorio", "sala", "quarto", "cozinha"];
    expect(resolveRealCameraSource("escritorio", baseCams)).toBe("escritorio");
    expect(resolveRealCameraSource("escritorio_sim_01", baseCams)).toBe("escritorio");
    expect(resolveRealCameraSource("sala_sim_02", baseCams)).toBe("sala");
    expect(resolveRealCameraSource("quarto_sim_07", baseCams)).toBe("quarto");
    expect(
      resolveRealCameraSource("cozinha_sim_04", {
        escritorio: {},
        sala: {},
        quarto: {},
        cozinha: {},
      })
    ).toBe("cozinha");

    const fleet12 = expandSimulatedFleet(baseCams, {}, {}, 12);
    expect(fleet12.cameraNames.length).toBe(12);
    expect(fleet12.cameraNames.slice(0, 6)).toEqual([
      "escritorio",
      "sala",
      "quarto",
      "cozinha",
      "escritorio_sim_01",
      "sala_sim_02",
    ]);
    for (const grp of SIMULATED_FLEET_GROUPS) {
      expect(fleet12.cameraGroups[grp]).toBeDefined();
      expect(fleet12.cameraGroups[grp].cameras.length).toBe(3);
    }

    const fleet36 = expandSimulatedFleet(baseCams, {}, {}, 36);
    expect(fleet36.cameraNames.length).toBe(36);
    expect(fleet36.cameraGroups["Interno"].cameras.length).toBe(9);

    const fleet100 = expandSimulatedFleet(baseCams, {}, {}, 100);
    expect(fleet100.cameraNames.length).toBe(100);
    expect(fleet100.cameraGroups["Estacionamento"].cameras.length).toBe(25);

    useNvrStore.getState().setSimulatedFleetCount(36);
    expect(useNvrStore.getState().simulatedFleetCount).toBe(36);
    useNvrStore.getState().setSimulatedFleetCount(0);
    expect(useNvrStore.getState().simulatedFleetCount).toBe(0);
  });

  test("serializes global and per-camera record.preview.quality: off with hybrid active_objects retention", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      record: {
        enabled: true,
        expire_interval: 60,
        continuous: { days: 3 },
        motion: { days: 0 },
        preview: { quality: "off" },
        alerts: {
          pre_capture: 15,
          post_capture: 15,
          retain: { days: 7, mode: "active_objects" },
        },
        detections: {
          pre_capture: 15,
          post_capture: 15,
          retain: { days: 7, mode: "active_objects" },
        },
      },
      cameras: {
        sala: {
          name: "sala",
          enabled: true,
          ffmpeg: {
            inputs: [{ path: "rtsp://127.0.0.1:8554/sala", roles: ["record", "detect"] }],
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: {
            enabled: true,
            continuous: { days: 3 },
            motion: { days: 0 },
            preview: { quality: "off" },
            alerts: {
              pre_capture: 15,
              post_capture: 15,
              retain: { days: 7, mode: "active_objects" },
            },
            detections: {
              pre_capture: 15,
              post_capture: 15,
              retain: { days: 7, mode: "active_objects" },
            },
          },
          snapshots: { enabled: true, quality: 100 },
        },
      },
    };

    const yamlText = buildCleanYamlFromDraft(draft, "");
    expect(yamlText).toContain('quality: "off"');
    expect(yamlText).toContain("mode: active_objects");
    expect(yamlText).toContain("days: 7");
  });
});
