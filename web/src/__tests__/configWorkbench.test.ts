import { describe, expect, test } from "bun:test";
import type { NinjaConfigResponse } from "../types/nvr";
import {
  buildGenAiApiKeyPatch,
  buildMqttPasswordPatch,
  buildSecretFieldPatch,
} from "../utils/configYaml";
import {
  createConfigWorkbenchSession,
  createFetchConfigTransport,
  createInMemoryConfigTransport,
} from "../utils/configWorkbench";
import {
  buildCloudPasswordPatch,
  buildOnvifPasswordPatch,
} from "../views/settings/ConsoleAndStreamsPanels";

function buildSampleConfig(): NinjaConfigResponse {
  return {
    version: "0.18-0",
    safe_mode: false,
    mqtt: {
      enabled: true,
      host: "192.0.2.5",
      port: 1883,
      topic_prefix: "ninjanvr",
      client_id: "ninjanvr",
      stats_interval: 60,
      user: "mqtt_op",
      password: "initial_mqtt_secret_pw",
      has_password: true,
    },
    genai: {
      enabled: true,
      provider: "gemini",
      api_key: "initial_genai_secret_key",
      has_api_key: true,
      api_key_env: "GEMINI_API_KEY",
      model: "gemini-2.5-flash",
      verify_ambiguous_alerts: true,
      ambiguous_score_min: 0.45,
      ambiguous_score_max: 0.72,
      auto_suppress_false_positives: true,
    },
    detect: {
      enabled: true,
      width: 1280,
      height: 720,
      fps: 5,
      min_initialized: 2,
      min_initialized_night: 4,
      max_disappeared: 25,
      stationary: {
        interval: 40,
        threshold: 45,
        classifier: false,
      },
    },
    lpr: {
      enabled: false,
      min_confidence: 0.75,
      known_plates: {},
    },
    profiles: {
      armed: {
        disabled_alerts_cameras: [],
        disabled_record_cameras: [],
      },
      home: {
        disabled_alerts_cameras: ["cam_b"],
        cameras: {
          cam_b: { record: { enabled: false } },
        },
      },
      disarmed: {
        disabled_alerts_cameras: ["cam_a", "cam_b"],
        disabled_record_cameras: ["cam_b"],
      },
    },
    ui: {
      theme: "dark",
      floorplan: {
        opacity: 0.45,
        width_m: 20,
        height_m: 12,
        rooms: [],
        walls: [],
      },
    },
    cameras: {
      cam_a: {
        name: "cam_a",
        enabled: true,
        ffmpeg: {
          inputs: [
            {
              path: "rtsp://admin:cam_a_rtsp_pw@192.0.2.10:554/stream1",
              roles: ["detect", "record"],
            },
          ],
        },
        onvif: {
          host: "192.0.2.10",
          port: 2020,
          user: "cam_admin",
          password: "initial_onvif_secret_pw",
          has_password: true,
          cloud_password: "initial_tapo_cloud_pw",
          has_cloud_password: true,
          autotracking: true,
          timeout_sec: 5,
        },
        hardware: {
          protocol: "tapo",
          profile_token: "profile_1",
          video_source_token: "vs_1",
          auto_reboot_on_rtsp_stall: true,
          cloud_password: "initial_tapo_cloud_pw",
          has_cloud_password: true,
        },
        map: {
          x: 0.5,
          y: 0.5,
          angle_deg: 0,
          fov_deg: 90,
          range: 0.25,
        },
        detect: {
          enabled: true,
          width: 1280,
          height: 720,
          fps: 5,
          min_initialized: 2,
          max_disappeared: 25,
        },
        motion: {
          enabled: true,
          threshold: 30,
          contour_area: 10,
          mask: {},
        },
        zones: {
          gate_zone: {
            coordinates: "0.1,0.1,0.8,0.1,0.8,0.8,0.1,0.8",
            objects: ["person", "car"],
            siren: true,
          },
        },
        record: { enabled: true },
        snapshots: { enabled: true },
      },
      cam_b: {
        name: "cam_b",
        enabled: true,
        ffmpeg: {
          inputs: [
            {
              path: "rtsp://admin:cam_b_rtsp_pw@198.51.100.20:554/sub",
              roles: ["detect"],
            },
          ],
        },
        detect: { enabled: true, width: 1280, height: 720, fps: 5 },
        motion: { enabled: true, threshold: 25, contour_area: 12, mask: {} },
        zones: {},
        record: { enabled: true },
        snapshots: { enabled: true },
      },
    },
  };
}

describe("ConfigWorkbenchSession", () => {
  test("hydrates config, tracks listener snapshots, edits draft, and commits structured config with server-authoritative raw_yaml", async () => {
    const transport = createInMemoryConfigTransport({
      initialConfig: buildSampleConfig(),
    });
    let serverCommitEvents = 0;
    const session = createConfigWorkbenchSession(transport, {
      onServerCommitted: () => {
        serverCommitEvents += 1;
      },
    });

    let listenerCalls = 0;
    const unsubscribe = session.subscribe(() => {
      listenerCalls += 1;
    });

    const [initialCfg, initialRaw] = await Promise.all([
      transport.fetchStructuredConfig(),
      transport.fetchRawYaml(),
    ]);
    session.hydrateFromServer(initialCfg, initialRaw);

    const snap1 = session.getSnapshot();
    // Referential stability required by React useSyncExternalStore
    expect(session.getSnapshot()).toBe(snap1);
    expect(snap1.isDirty).toBe(false);
    expect(snap1.draftConfig?.cameras.cam_a.motion.threshold).toBe(30);
    expect(snap1.rawYaml).toContain('password: "***"');
    expect(snap1.rawYaml).not.toContain("initial_onvif_secret_pw");

    // Repeated hydration with identical config does not clobber or emit extra notifications
    const callsBeforeNoop = listenerCalls;
    session.hydrateFromServer(initialCfg);
    expect(listenerCalls).toBe(callsBeforeNoop);

    // Mutate draft on visual tab (lazy YAML synthesis)
    session.mutateDraft((next) => {
      next.cameras.cam_a.motion.threshold = 44;
    });
    const snapDirty = session.getSnapshot();
    expect(snapDirty.isDirty).toBe(true);
    expect(snapDirty.yamlStale).toBe(true);
    expect(snapDirty.revision).toBe(1);
    expect(snapDirty.draftConfig?.cameras.cam_a.motion.threshold).toBe(44);
    expect(snapDirty.baselineConfig?.cameras.cam_a.motion.threshold).toBe(30);

    // Switching to YAML tab synthesizes YAML lazily once
    const syncedYaml = session.ensureDraftYamlSynced();
    expect(syncedYaml).toContain("threshold: 44");
    expect(session.getSnapshot().yamlStale).toBe(false);

    // Commit structured draft and adopt server-authoritative config + raw_yaml
    const commitRes = await session.commitStructured({ restart: true });
    expect(commitRes.ok).toBe(true);
    expect(transport.restartCount).toBe(1);
    expect(serverCommitEvents).toBe(1);

    const snapSaved = session.getSnapshot();
    expect(snapSaved.isDirty).toBe(false);
    expect(snapSaved.isSaving).toBe(false);
    expect(snapSaved.yamlStale).toBe(false);
    expect(snapSaved.saveOutcome?.ok).toBe(true);
    expect(snapSaved.baselineConfig?.cameras.cam_a.motion.threshold).toBe(44);
    expect(snapSaved.draftConfig?.cameras.cam_a.motion.threshold).toBe(44);
    expect(snapSaved.rawYaml).toContain("threshold: 44");

    // Subsequent server raw YAML refresh updates clean rawYaml (fixing stale `curr || txt` bug)
    session.hydrateRawYamlFromServer("# authoritative reload\nversion: 0.18-0\n");
    expect(session.getSnapshot().rawYaml).toContain("# authoritative reload");

    unsubscribe();
  });

  test("falls back to commitRawYaml on HTTP 404 while preserving zones.siren, hardware, profiles, and detect.stationary", async () => {
    const transport = createInMemoryConfigTransport({
      initialConfig: buildSampleConfig(),
      structuredEndpointStatus: 404,
    });
    const session = createConfigWorkbenchSession(transport);
    const [cfg, raw] = await Promise.all([
      transport.fetchStructuredConfig(),
      transport.fetchRawYaml(),
    ]);
    session.hydrateFromServer(cfg, raw);

    session.mutateDraft((next) => {
      next.cameras.cam_a.motion.threshold = 52;
    });

    const outcome = await session.commitStructured({ restart: false });
    expect(outcome.ok).toBe(true);
    expect(transport.committedStructuredDrafts.length).toBe(0);
    expect(transport.committedRawYamls.length).toBe(1);

    const fallbackYaml = transport.committedRawYamls[0].yaml;
    expect(fallbackYaml).toContain("threshold: 52");
    expect(fallbackYaml).toContain("siren: true");
    expect(fallbackYaml).toContain("hardware:");
    expect(fallbackYaml).toContain("protocol: tapo");
    expect(fallbackYaml).toContain("profile_token: profile_1");
    expect(fallbackYaml).toContain("video_source_token: vs_1");
    expect(fallbackYaml).toContain("auto_reboot_on_rtsp_stall: true");
    expect(fallbackYaml).toContain("profiles:");
    expect(fallbackYaml).toContain("home:");
    expect(fallbackYaml).toContain("disarmed:");
    expect(fallbackYaml).toContain("min_initialized_night: 4");
    expect(fallbackYaml).toContain("interval: 40");
    expect(fallbackYaml).toContain("threshold: 45");
    expect(fallbackYaml).toContain("classifier: false");
    expect(session.getSnapshot().isDirty).toBe(false);
  });

  test("rebases out-of-band LPR, floorplan, camera hardware, and model patches into an active dirty draft without clobbering", async () => {
    const transport = createInMemoryConfigTransport({
      initialConfig: buildSampleConfig(),
    });
    const session = createConfigWorkbenchSession(transport);
    const [cfg, raw] = await Promise.all([
      transport.fetchStructuredConfig(),
      transport.fetchRawYaml(),
    ]);
    session.hydrateFromServer(cfg, raw);

    // 1. Operator edits camera A's motion threshold in ZonesWorkspace (isDirty = true)
    session.mutateDraft((next) => {
      next.cameras.cam_a.motion.threshold = 61;
    });
    expect(session.getSnapshot().isDirty).toBe(true);

    // 2. Out-of-band patch #1: Operator toggles LPR and adds a known plate in VisionAndAiPanels
    const lprRes = await session.applyOutOfBandPatch({
      lpr: {
        enabled: true,
        min_confidence: 0.84,
        known_plates: {
          BRA2E19: "Security Patrol",
        },
      },
    });
    expect(lprRes.ok).toBe(true);
    expect(session.getSnapshot().isDirty).toBe(true);
    expect(session.getSnapshot().draftConfig?.cameras.cam_a.motion.threshold).toBe(61);
    expect(session.getSnapshot().draftConfig?.lpr?.enabled).toBe(true);
    expect(session.getSnapshot().draftConfig?.lpr?.known_plates).toEqual({
      BRA2E19: "Security Patrol",
    });

    // 3. Out-of-band patch #2: Operator saves floorplan placements & rooms in FloorplanStudioCanvas
    const floorplanRes = await session.applyOutOfBandPatch({
      cameras: {
        cam_a: {
          map: {
            x: 0.24,
            y: 0.36,
            angle_deg: 135,
            fov_deg: 95,
            range: 0.34,
          },
        },
        cam_b: {
          map: {
            x: 0.78,
            y: 0.62,
            angle_deg: 270,
            fov_deg: 90,
            range: 0.28,
          },
        },
      },
      ui: {
        theme: "dark",
        floorplan: {
          opacity: 0.65,
          width_m: 24,
          height_m: 14,
          rooms: [
            { id: "lobby", label: "LOBBY", x: 0.1, y: 0.1, w: 0.4, h: 0.4 },
          ],
          walls: [{ points: [0.1, 0.5, 0.5, 0.5], kind: "wall" }],
        },
      },
    });
    expect(floorplanRes.ok).toBe(true);
    expect(session.getSnapshot().isDirty).toBe(true);

    // 4. Out-of-band patch #3: Operator applies camera hardware protocol/profile update
    const hwRes = await session.applyOutOfBandPatch({
      cameras: {
        cam_a: {
          hardware: {
            protocol: "dahua_cgi",
            profile_token: "Profile000",
            video_source_token: "VideoSource000",
            auto_reboot_on_rtsp_stall: true,
          },
        },
      },
    });
    expect(hwRes.ok).toBe(true);

    // 5. Out-of-band patch #4: Dataset Studio deploys custom YOLO model on server
    await session.applyOutOfBandPatch(
      {
        model: {
          path: "/config/models/custom_yolo11s.onnx",
          labelmap_path: "/openvino-model/ninjanvr_security_15cl.txt",
          width: 640,
          height: 640,
          input_tensor: "nchw",
          input_pixel_format: "rgb",
          model_type: "yolo-generic",
        },
      },
      { persistViaTransport: false }
    );
    expect(session.getSnapshot().isDirty).toBe(true);

    // 6. Operator now clicks Save in ZonesWorkspace -> ALL changes must be preserved!
    const finalSave = await session.commitStructured({ restart: false });
    expect(finalSave.ok).toBe(true);
    expect(session.getSnapshot().isDirty).toBe(false);

    const committedPayload =
      transport.committedStructuredDrafts[
        transport.committedStructuredDrafts.length - 1
      ];
    // Dirty motion threshold edit on cam_a preserved:
    expect(committedPayload.cameras.cam_a.motion.threshold).toBe(61);
    // Out-of-band LPR preserved:
    expect(committedPayload.lpr?.enabled).toBe(true);
    expect(committedPayload.lpr?.known_plates).toEqual({
      BRA2E19: "Security Patrol",
    });
    // Out-of-band floorplan placements & rooms preserved:
    expect(committedPayload.cameras.cam_a.map).toEqual({
      x: 0.24,
      y: 0.36,
      angle_deg: 135,
      fov_deg: 95,
      range: 0.34,
    });
    expect(committedPayload.ui?.floorplan?.opacity).toBe(0.65);
    expect(committedPayload.ui?.floorplan?.rooms[0]?.id).toBe("lobby");
    // Out-of-band camera hardware preserved:
    expect(committedPayload.cameras.cam_a.hardware?.protocol).toBe("dahua_cgi");
    expect(committedPayload.cameras.cam_a.hardware?.profile_token).toBe(
      "Profile000"
    );
    // Out-of-band deployed model preserved:
    expect(committedPayload.model?.path).toBe(
      "/config/models/custom_yolo11s.onnx"
    );
    // Sibling camera cam_b preserved:
    expect(committedPayload.cameras.cam_b.enabled).toBe(true);
  });

  test("handles secret sentinel transitions (preserve ***, replace plaintext, clear) across ONVIF, Tapo cloud_password, GenAI, and MQTT", async () => {
    const transport = createInMemoryConfigTransport({
      initialConfig: buildSampleConfig(),
    });
    const session = createConfigWorkbenchSession(transport);
    const [cfg, raw] = await Promise.all([
      transport.fetchStructuredConfig(),
      transport.fetchRawYaml(),
    ]);
    session.hydrateFromServer(cfg, raw);

    // Phase 1: Preserve existing secrets when saving unrelated edits (sentinels remain ***)
    session.mutateDraft((next) => {
      next.cameras.cam_a.onvif = {
        ...next.cameras.cam_a.onvif,
        ...buildOnvifPasswordPatch("", next.cameras.cam_a.onvif?.has_password, false),
        ...buildCloudPasswordPatch(
          "",
          next.cameras.cam_a.onvif?.has_cloud_password,
          false
        ),
      };
      next.cameras.cam_a.hardware = {
        ...next.cameras.cam_a.hardware,
        ...buildCloudPasswordPatch(
          "",
          next.cameras.cam_a.hardware?.has_cloud_password,
          false
        ),
      };
      next.genai = {
        ...next.genai!,
        ...buildGenAiApiKeyPatch("", next.genai?.has_api_key, false),
      };
      next.mqtt = {
        ...next.mqtt!,
        ...buildMqttPasswordPatch("", next.mqtt?.has_password, false),
      };
      next.cameras.cam_a.detect.fps = 7;
    });
    await session.commitStructured();

    const preservedDiskYaml = transport.getUnscrubbedYaml();
    expect(preservedDiskYaml).toContain("initial_onvif_secret_pw");
    expect(preservedDiskYaml).toContain("initial_tapo_cloud_pw");
    expect(preservedDiskYaml).toContain("initial_genai_secret_key");
    expect(preservedDiskYaml).toContain("initial_mqtt_secret_pw");
    expect(preservedDiskYaml).toContain("rtsp://admin:cam_a_rtsp_pw@192.0.2.10:554/stream1");

    // Phase 2: Replace all 4 secrets with new plaintext values
    session.mutateDraft((next) => {
      next.cameras.cam_a.onvif = {
        ...next.cameras.cam_a.onvif,
        ...buildOnvifPasswordPatch("rotated_onvif_pw", true, false),
        ...buildCloudPasswordPatch("rotated_tapo_cloud_pw", true, false),
      };
      next.cameras.cam_a.hardware = {
        ...next.cameras.cam_a.hardware,
        ...buildCloudPasswordPatch("rotated_tapo_cloud_pw", true, false),
      };
      next.genai = {
        ...next.genai!,
        ...buildGenAiApiKeyPatch("rotated_genai_api_key", true, false),
      };
      next.mqtt = {
        ...next.mqtt!,
        ...buildMqttPasswordPatch("rotated_mqtt_broker_pw", true, false),
      };
    });
    await session.commitStructured();

    const rotatedDiskYaml = transport.getUnscrubbedYaml();
    expect(rotatedDiskYaml).toContain("rotated_onvif_pw");
    expect(rotatedDiskYaml).toContain("rotated_tapo_cloud_pw");
    expect(rotatedDiskYaml).toContain("rotated_genai_api_key");
    expect(rotatedDiskYaml).toContain("rotated_mqtt_broker_pw");
    expect(rotatedDiskYaml).not.toContain("initial_onvif_secret_pw");
    expect(rotatedDiskYaml).not.toContain("initial_tapo_cloud_pw");
    expect(rotatedDiskYaml).not.toContain("initial_genai_secret_key");
    expect(rotatedDiskYaml).not.toContain("initial_mqtt_secret_pw");

    // Client snapshot remains scrubbed with "***" and has_* = true
    const afterRotateCfg = session.getSnapshot().draftConfig!;
    expect(afterRotateCfg.cameras.cam_a.onvif?.password).toBe("***");
    expect(afterRotateCfg.cameras.cam_a.onvif?.has_password).toBe(true);
    expect(afterRotateCfg.cameras.cam_a.hardware?.cloud_password).toBe("***");
    expect(afterRotateCfg.cameras.cam_a.hardware?.has_cloud_password).toBe(true);
    expect(afterRotateCfg.genai?.api_key).toBe("***");
    expect(afterRotateCfg.genai?.has_api_key).toBe(true);
    expect(afterRotateCfg.mqtt?.password).toBe("***");
    expect(afterRotateCfg.mqtt?.has_password).toBe(true);

    // Phase 3: Explicitly clear all 4 secrets
    session.mutateDraft((next) => {
      next.cameras.cam_a.onvif = {
        ...next.cameras.cam_a.onvif,
        ...buildSecretFieldPatch("password", "", true, true),
        ...buildSecretFieldPatch("cloud_password", "", true, true),
      };
      next.cameras.cam_a.hardware = {
        ...next.cameras.cam_a.hardware,
        ...buildSecretFieldPatch("cloud_password", "", true, true),
      };
      next.genai = {
        ...next.genai!,
        ...buildGenAiApiKeyPatch("", true, true),
      };
      next.mqtt = {
        ...next.mqtt!,
        ...buildMqttPasswordPatch("", true, true),
      };
    });
    await session.commitStructured();

    const clearedDiskYaml = transport.getUnscrubbedYaml();
    expect(clearedDiskYaml).not.toContain("rotated_onvif_pw");
    expect(clearedDiskYaml).not.toContain("rotated_tapo_cloud_pw");
    expect(clearedDiskYaml).not.toContain("rotated_genai_api_key");
    expect(clearedDiskYaml).not.toContain("rotated_mqtt_broker_pw");
    expect(clearedDiskYaml).not.toContain("cloud_password:");
    expect(clearedDiskYaml).not.toContain("api_key:");

    const afterClearCfg = session.getSnapshot().draftConfig!;
    expect(afterClearCfg.cameras.cam_a.onvif?.has_password).toBe(false);
    expect(afterClearCfg.cameras.cam_a.hardware?.has_cloud_password).toBe(false);
    expect(afterClearCfg.genai?.has_api_key).toBe(false);
    expect(afterClearCfg.mqtt?.has_password).toBe(false);
  });

  test("preserves concurrent edits made while commitStructured is in flight (draftRevision guard)", async () => {
    let releaseInFlightCommit!: () => void;
    const inFlightGate = new Promise<void>((resolve) => {
      releaseInFlightCommit = resolve;
    });

    const transport = createInMemoryConfigTransport({
      initialConfig: buildSampleConfig(),
      beforeCommitStructured: async () => {
        await inFlightGate;
      },
    });
    const session = createConfigWorkbenchSession(transport);
    const [cfg, raw] = await Promise.all([
      transport.fetchStructuredConfig(),
      transport.fetchRawYaml(),
    ]);
    session.hydrateFromServer(cfg, raw);

    // First edit (rev 1)
    session.mutateDraft((next) => {
      next.cameras.cam_a.motion.threshold = 38;
    });
    expect(session.getSnapshot().revision).toBe(1);

    // Start saving rev 1 (pauses inside beforeCommitStructured)
    const savePromise = session.commitStructured();
    expect(session.getSnapshot().isSaving).toBe(true);

    // Operator makes a second edit (rev 2) while rev 1 is still in flight!
    session.mutateDraft((next) => {
      next.cameras.cam_b.motion.threshold = 55;
    });
    expect(session.getSnapshot().revision).toBe(2);

    // Complete in-flight save of rev 1
    releaseInFlightCommit();
    const firstOutcome = await savePromise;
    expect(firstOutcome.ok).toBe(true);

    // Session must remain dirty because rev 2 has not been saved yet, and rev 2's edit must stay intact!
    const afterFirstSave = session.getSnapshot();
    expect(afterFirstSave.isSaving).toBe(false);
    expect(afterFirstSave.isDirty).toBe(true);
    expect(afterFirstSave.baselineConfig?.cameras.cam_a.motion.threshold).toBe(38);
    expect(afterFirstSave.baselineConfig?.cameras.cam_b.motion.threshold).toBe(25);
    expect(afterFirstSave.draftConfig?.cameras.cam_a.motion.threshold).toBe(38);
    expect(afterFirstSave.draftConfig?.cameras.cam_b.motion.threshold).toBe(55);

    // Saving a second time commits rev 2 and marks session clean
    const secondOutcome = await session.commitStructured();
    expect(secondOutcome.ok).toBe(true);
    expect(session.getSnapshot().isDirty).toBe(false);
    expect(
      session.getSnapshot().baselineConfig?.cameras.cam_b.motion.threshold
    ).toBe(55);
  });

  test("createFetchConfigTransport communicates with /api/config*, /api/config/save, and /api/restart", async () => {
    const sample = buildSampleConfig();
    const calls: Array<{ url: string; method: string; body?: string }> = [];
    const mockFetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method || "GET";
      const body = typeof init?.body === "string" ? init.body : undefined;
      calls.push({ url, method, body });

      if (url === "/api/config") {
        return new Response(JSON.stringify(sample), { status: 200 });
      }
      if (url === "/api/config/raw") {
        return new Response(JSON.stringify("version: 0.18-0\n"), { status: 200 });
      }
      if (url === "/api/config/structured") {
        return new Response(
          JSON.stringify({
            success: true,
            config: sample,
            raw_yaml: "version: 0.18-0\n# structured\n",
          }),
          { status: 200 }
        );
      }
      if (url.startsWith("/api/config/save")) {
        return new Response(
          JSON.stringify({
            success: true,
            config: sample,
            raw_yaml: "version: 0.18-0\n# raw_saved\n",
          }),
          { status: 200 }
        );
      }
      if (url === "/api/restart") {
        return new Response(JSON.stringify({ success: true }), { status: 200 });
      }
      return new Response("Not Found", { status: 404 });
    }) as typeof fetch;

    const transport = createFetchConfigTransport(mockFetch);
    const fetchedCfg = await transport.fetchStructuredConfig();
    expect(fetchedCfg.version).toBe("0.18-0");
    const fetchedYaml = await transport.fetchRawYaml();
    expect(fetchedYaml).toBe("version: 0.18-0\n");

    const structuredRes = await transport.commitStructured(sample);
    expect(structuredRes.ok).toBe(true);
    expect(structuredRes.rawYaml).toContain("# structured");

    const rawRes = await transport.commitRawYaml("version: 0.18-0\n", true);
    expect(rawRes.ok).toBe(true);
    expect(rawRes.rawYaml).toContain("# raw_saved");
    expect(rawRes.config?.version).toBe("0.18-0");

    expect(await transport.triggerRestart()).toBe(true);
    expect(calls.map((c) => `${c.method} ${c.url}`)).toEqual([
      "GET /api/config",
      "GET /api/config/raw",
      "POST /api/config/structured",
      "POST /api/config/save?save_option=restart",
      "POST /api/restart",
    ]);
  });

  test("preserves pending clear_password, clear_cloud_password, clear_api_key, and unsaved plaintext passwords across out-of-band floorplan, camera hardware, and external patches", async () => {
    const transport = createInMemoryConfigTransport({
      initialConfig: buildSampleConfig(),
    });
    const session = createConfigWorkbenchSession(transport);
    const [cfg, raw] = await Promise.all([
      transport.fetchStructuredConfig(),
      transport.fetchRawYaml(),
    ]);
    session.hydrateFromServer(cfg, raw);

    // Operator stages secret clears on cam_a, mqtt, genai and a new plaintext ONVIF password on cam_b (unsaved dirty draft)
    session.mutateDraft((next) => {
      next.cameras.cam_a.onvif = {
        ...next.cameras.cam_a.onvif,
        ...buildSecretFieldPatch("password", "", true, true),
        ...buildSecretFieldPatch("cloud_password", "", true, true),
      };
      next.cameras.cam_a.hardware = {
        ...next.cameras.cam_a.hardware,
        ...buildSecretFieldPatch("cloud_password", "", true, true),
      };
      next.cameras.cam_b.onvif = {
        host: "198.51.100.20",
        port: 80,
        user: "cam_b_admin",
        ...buildOnvifPasswordPatch("new_plaintext_cam_b_pw", false, false),
      };
      next.mqtt = {
        ...next.mqtt!,
        ...buildMqttPasswordPatch("", true, true),
      };
      next.genai = {
        ...next.genai!,
        ...buildGenAiApiKeyPatch("new_plaintext_genai_key", true, false),
      };
    });

    // Out-of-band patch #1: Floorplan map placement on cam_a & cam_b
    const fpOutcome = await session.applyOutOfBandPatch({
      cameras: {
        cam_a: {
          map: { x: 0.15, y: 0.25, angle_deg: 45, fov_deg: 90, range: 0.3 },
        },
        cam_b: {
          map: { x: 0.85, y: 0.75, angle_deg: 225, fov_deg: 90, range: 0.3 },
        },
      },
    });
    expect(fpOutcome.ok).toBe(true);

    // Out-of-band patch #2: Sibling non-secret hardware & MQTT interval patch
    const hwAndMqttOutcome = await session.applyOutOfBandPatch({
      cameras: {
        cam_a: {
          hardware: {
            protocol: "dahua_cgi",
            profile_token: "Profile_Main",
          },
        },
      },
      mqtt: {
        stats_interval: 120,
      },
      genai: {
        model: "gemini-2.5-pro",
      },
    });
    expect(hwAndMqttOutcome.ok).toBe(true);

    // Out-of-band patch #3: External (already persisted) LPR patch
    session.applyExternalPatch({
      lpr: {
        enabled: true,
        min_confidence: 0.8,
        known_plates: { ABC1D23: "Gate Pass" },
      },
    });

    // Verify all pending secret flags and unsaved plaintext values survived intact in draftConfig!
    const dirtyDraft = session.getSnapshot().draftConfig!;
    expect(dirtyDraft.cameras.cam_a.onvif?.clear_password).toBe(true);
    expect(dirtyDraft.cameras.cam_a.onvif?.clear_cloud_password).toBe(true);
    expect(dirtyDraft.cameras.cam_a.hardware?.clear_cloud_password).toBe(true);
    expect(dirtyDraft.cameras.cam_b.onvif?.password).toBe(
      "new_plaintext_cam_b_pw"
    );
    expect(dirtyDraft.mqtt?.clear_password).toBe(true);
    expect(dirtyDraft.genai?.api_key).toBe("new_plaintext_genai_key");

    // Non-secret out-of-band updates also merged cleanly
    expect(dirtyDraft.cameras.cam_a.map?.x).toBe(0.15);
    expect(dirtyDraft.cameras.cam_b.map?.x).toBe(0.85);
    expect(dirtyDraft.cameras.cam_a.hardware?.protocol).toBe("dahua_cgi");
    expect(dirtyDraft.mqtt?.stats_interval).toBe(120);
    expect(dirtyDraft.genai?.model).toBe("gemini-2.5-pro");

    // Committing the draft now clears cam_a/mqtt secrets and writes cam_b/genai new plaintext secrets to disk!
    const saveRes = await session.commitStructured();
    expect(saveRes.ok).toBe(true);
    const diskYaml = transport.getUnscrubbedYaml();
    expect(diskYaml).not.toContain("initial_onvif_secret_pw");
    expect(diskYaml).not.toContain("initial_tapo_cloud_pw");
    expect(diskYaml).not.toContain("initial_mqtt_secret_pw");
    expect(diskYaml).toContain("new_plaintext_cam_b_pw");
    expect(diskYaml).toContain("new_plaintext_genai_key");
  });

  test("enforces backend cameras keyset replacement in transport and expands sibling cameras in applyOutOfBandPatch", async () => {
    const transport = createInMemoryConfigTransport({
      initialConfig: buildSampleConfig(),
    });

    // Direct transport call with null camera entry fails with HTTP 400 (matching serde CameraConfig deserialization)
    const nullReject = await transport.commitStructuredPatch({
      cameras: { cam_a: {}, cam_b: null },
    });
    expect(nullReject.ok).toBe(false);
    expect(nullReject.status).toBe(400);
    expect(Object.keys(transport.getServerConfig().cameras)).toEqual([
      "cam_a",
      "cam_b",
    ]);

    // Session applyOutOfBandPatch with partial cameras expands untouched sibling cam_b as {} so backend retains it
    const session = createConfigWorkbenchSession(transport);
    const [cfg, raw] = await Promise.all([
      transport.fetchStructuredConfig(),
      transport.fetchRawYaml(),
    ]);
    session.hydrateFromServer(cfg, raw);

    const partialCamRes = await session.applyOutOfBandPatch({
      cameras: {
        cam_a: {
          motion: { threshold: 48 },
        },
      },
    });
    expect(partialCamRes.ok).toBe(true);
    const outboundPatch =
      transport.committedPatches[transport.committedPatches.length - 1];
    expect(outboundPatch.cameras).toEqual({
      cam_a: { motion: { threshold: 48 } },
      cam_b: {},
    });
    expect(Object.keys(transport.getServerConfig().cameras)).toEqual([
      "cam_a",
      "cam_b",
    ]);

    // Session applyOutOfBandPatch with explicit null deletes cam_b by omitting it from outbound cameras keyset
    const deleteCamRes = await session.applyOutOfBandPatch({
      cameras: {
        cam_b: null,
      },
    });
    expect(deleteCamRes.ok).toBe(true);
    const deleteOutbound =
      transport.committedPatches[transport.committedPatches.length - 1];
    expect(deleteOutbound.cameras).toEqual({
      cam_a: {},
    });
    expect(Object.keys(transport.getServerConfig().cameras)).toEqual(["cam_a"]);
    expect(
      Object.keys(session.getSnapshot().draftConfig?.cameras || {})
    ).toEqual(["cam_a"]);

    // Direct raw transport patch omitting cam_a replaces cameras keyset on backend
    const directReplace = await transport.commitStructuredPatch({
      cameras: {},
    });
    expect(directReplace.ok).toBe(true);
    expect(Object.keys(transport.getServerConfig().cameras)).toEqual([]);

    // commitStructured also rejects null camera entries with HTTP 400
    const badDraft = buildSampleConfig();
    (badDraft.cameras as Record<string, unknown>).cam_b = null;
    const structuredNullReject = await transport.commitStructured(badDraft);
    expect(structuredNullReject.ok).toBe(false);
    expect(structuredNullReject.status).toBe(400);
  });
});
