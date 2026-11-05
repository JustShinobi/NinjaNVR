import { describe, expect, test } from "bun:test";
import {
  MAX_LIVE_EVENTS,
  MAX_LIVE_REVIEWS,
  extractOidcCallbackError,
  playTacticalAlertChime,
  prependBoundedReviewAlert,
  sanitizeExportVideoUrl,
  upsertBoundedEventList,
} from "../App";
import {
  MAX_CAMERA_ACTIVITY_ENTRIES,
  MAX_STATS_HISTORY_SAMPLES,
  resolveValidCameraSelection,
  useNvrStore,
} from "../store/useNvrStore";
import {
  buildCleanYamlFromDraft,
  parseYamlScalarValue,
  validateLicensePlateFormat,
} from "../utils/configYaml";
import {
  findMatchingExport,
  formatDateTime,
  formatDuration,
  formatRelTime,
  formatTs,
  formatUptime,
  resolveHeaderTelemetryMeta,
  resolveInitialBool,
  resolveInitialFirstWeekday,
  resolveInitialHudPosition,
  resolveInitialLiveLayout,
  resolveInitialMatrixDensity,
  resolveInitialPlaybackSpeed,
  resolveInitialThemeMode,
} from "../utils/format";
import {
  computeLiveStageDescriptors,
  resolvePersistentTileStyle,
} from "../utils/liveStageLayout";
import {
  DualSlotMachine,
  StreamPrewarmCoordinator,
} from "../utils/msePlayerEngine";
import {
  buildSharePublicUrl,
  computeShareLinkRotationParams,
  formatShareLinkViews,
  isValidShareTokenSegment,
} from "../utils/shareLinks";
import {
  cieLightnessFromLuminance,
  quantizeHdmiLimitedRange,
  rgbToHex,
  simulateGamma24Transfer,
  srgbChannelToLinear,
} from "../utils/themeTokens";
import { createPushToTalkController } from "../components/VideoPlayers";
import {
  computeClientXTimestamp,
  parseLocalDayBounds,
  sanitizeTimelineThumbUrl,
} from "../components/TimelineScrubber";
import { formatReadableDate } from "../components/RecordingCalendarPopover";
import {
  getCameraPickerStatus,
  resolvePickerMode,
} from "../components/TacticalCameraPicker";
import type { EventItem, NinjaConfigResponse, ReviewItem } from "../types/nvr";

function makeEventItem(id: string): EventItem {
  return {
    id,
    camera: "front_door",
    label: "person",
    sub_label: null,
    zones: [],
    start_time: 1700000000,
    end_time: 1700000010,
    top_score: 0.9,
    score: 0.9,
    has_clip: true,
    has_snapshot: true,
    retain_indefinitely: false,
    false_positive: false,
    box: null,
  };
}

function makeReviewItem(id: string): ReviewItem {
  return {
    id,
    camera: "front_door",
    start_time: 1700000000,
    end_time: 1700000015,
    severity: "alert",
    thumb_path: "/clips/front_door.webp",
    has_been_reviewed: false,
    data: { objects: ["person"] },
  };
}

describe("Phase 4A Core Players, Store, & Utils Security/Bug Regression Suite", () => {
  test("[P0-YAML-01] buildCleanYamlFromDraft escapes multiline strings, control chars, leading hyphens, YAML booleans/nulls, and numeric passwords", () => {
    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      mqtt: {
        enabled: true,
        host: "10.0.0.5",
        port: 1883,
        topic_prefix: "ninjanvr\n  injected_root_key: evil",
        client_id: "-leading-hyphen",
        stats_interval: 30,
        user: "true",
        password: "123456",
      },
      cameras: {
        front_door: {
          name: "front_door",
          enabled: true,
          onvif: {
            host: "192.0.2.10",
            port: 80,
            user: "~",
            password: "line1\n  admin: true\r\n\tcc",
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: {
            enabled: true,
            threshold: 30,
            lightning_threshold: Number.NaN,
            contour_area: 10,
            mask: {},
          },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    const yaml = buildCleanYamlFromDraft(draft, "");
    expect(yaml).toContain('topic_prefix: "ninjanvr\\n  injected_root_key: evil"');
    expect(yaml).not.toContain("\n  injected_root_key: evil\n");
    expect(yaml).toContain('client_id: "-leading-hyphen"');
    expect(yaml).toContain('user: "true"');
    expect(yaml).toContain('password: "123456"');
    expect(yaml).toContain('user: "~"');
    expect(yaml).toContain('password: "line1\\n  admin: true\\r\\n\\tcc"');
    expect(yaml).toContain("lightning_threshold: 0.8");
    expect(yaml).not.toContain("NaN");
  });

  test("[P1-YAML-02] parseYamlScalarValue and buildCleanYamlFromDraft preserve '#' inside quoted passwords/keys, restore MQTT password, and handle IPv6 RTSP URLs", () => {
    expect(parseYamlScalarValue('"my_pass #1 with spaces"')).toBe(
      "my_pass #1 with spaces"
    );
    expect(parseYamlScalarValue('"escaped \\"quote\\" and \\\\ slash #ok"')).toBe(
      'escaped "quote" and \\ slash #ok'
    );
    expect(parseYamlScalarValue("'single #1 ''quoted'' pw'")).toBe(
      "single #1 'quoted' pw"
    );
    expect(parseYamlScalarValue("unquoted_pw # trailing comment")).toBe(
      "unquoted_pw"
    );

    const rawYaml = `
mqtt:
  enabled: true
  host: 127.0.0.1
  port: 1883
  user: mqtt_op
  password: "mqtt_secret #99"
genai:
  enabled: true
  provider: openai
  base_url: "https://api_user:api_pw@ai.example.test:8443/v1"
  api_key: "sk-proj-abc #123"
go2rtc:
  streams:
    ipv6_cam:
      - "rtsp://v6user:v6pass@[2001:db8::1]:554/live"
cameras:
  "front.door":
    enabled: true
    onvif:
      host: "http://onvif_u:onvif_p@192.0.2.55:8000"
      port: 8000
      user: admin
      password: "onvif_secret #hash"
`;

    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      mqtt: {
        enabled: true,
        host: "127.0.0.1",
        port: 1883,
        topic_prefix: "ninjanvr",
        client_id: "ninjanvr",
        stats_interval: 60,
        user: "mqtt_op",
        has_password: true,
      },
      genai: {
        enabled: true,
        provider: "openai",
        base_url: "https://*:*@ai.example.test:8443/v1",
        api_key: "***",
        api_key_env: "OPENAI_API_KEY",
        model: "gpt-4o",
        verify_ambiguous_alerts: true,
        ambiguous_score_min: 0.45,
        ambiguous_score_max: 0.72,
        auto_suppress_false_positives: true,
      },
      go2rtc: {
        streams: {
          ipv6_cam: ["rtsp://*:*@[2001:db8::1]:554/sub_modified"],
        },
      },
      cameras: {
        "front.door": {
          enabled: true,
          onvif: {
            host: "http://*:*@192.0.2.55:8000",
            port: 8000,
            user: "admin",
            password: "***",
          },
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    const out = buildCleanYamlFromDraft(draft, rawYaml);
    expect(out).toContain('password: "mqtt_secret #99"');
    expect(out).toContain('api_key: "sk-proj-abc #123"');
    expect(out).toContain('password: "onvif_secret #hash"');
    expect(out).toContain(
      'base_url: "https://api_user:api_pw@ai.example.test:8443/v1"'
    );
    expect(out).toContain('host: "http://onvif_u:onvif_p@192.0.2.55:8000"');
    expect(out).toContain(
      '"rtsp://v6user:v6pass@[2001:db8::1]:554/sub_modified"'
    );

    // Verify alternation regex anchoring in validateLicensePlateFormat
    expect(validateLicensePlateFormat("ABCD9999", "^ABC1234|XYZ9876$")).toBe(
      false
    );
    expect(validateLicensePlateFormat("ABC1234", "^ABC1234|XYZ9876$")).toBe(
      true
    );
  });

  test("[P0-MSE-02] DualSlotMachine.requestStream aborts in-flight standby slot when active slot has errored", () => {
    const machine = new DualSlotMachine();

    // 1. Slot 0 becomes ready with cam1_sub
    const s1 = machine.requestStream("cam1_sub");
    machine.markSlotReady(0, s1.sessionId);

    // 2. Standby slot 1 starts connecting cam1
    const s2 = machine.requestStream("cam1");
    expect(s2.action).toBe("start_standby");
    expect(s2.targetSlot).toBe(1);

    // 3. Active slot 0 errors out while standby slot 1 is still connecting cam1
    machine.markSlotError(0, s1.sessionId);
    expect(machine.getState().slots[0].status).toBe("error");
    expect(machine.getState().standbySlot).toBe(1);

    // 4a. Re-requesting "cam1" while standby slot 1 is already connecting "cam1" is a noop
    const s2Dup = machine.requestStream("cam1");
    expect(s2Dup.action).toBe("noop");
    expect(s2Dup.targetSlot).toBe(1);
    expect(s2Dup.sessionId).toBe(s2.sessionId);

    // 4b. Requesting a different camera "cam2" starts on slot 0 and aborts standby slot 1
    const s3 = machine.requestStream("cam2");
    expect(s3.action).toBe("start_active");
    expect(s3.targetSlot).toBe(0);
    expect(s3.abortSlot).toBe(1);
    expect(machine.getState().standbySlot).toBeNull();
    expect(machine.getState().slots[1].status).toBe("idle");

    // 5. Stale ready from aborted slot 1 session is ignored
    const staleReady = machine.markSlotReady(1, s2.sessionId);
    expect(staleReady.promoted).toBe(false);
    expect(machine.getState().activeSlot).toBe(0);
  });

  test("[P1-MSE-04] createPushToTalkController immediately stops acquired microphone tracks if released or unmounted while SDP fetch is still in flight", async () => {
    let trackStopped = 0;
    let pcClosed = 0;
    let notifyFetchStarted!: () => void;
    const fetchStartedPromise = new Promise<void>((resolve) => {
      notifyFetchStarted = resolve;
    });
    let resolveFetch!: (val: {
      ok: boolean;
      status: number;
      text: () => Promise<string>;
    }) => void;

    const ctrl = createPushToTalkController("front_door", {
      getUserMedia: async () => ({
        getTracks: () => [
          {
            stop() {
              trackStopped += 1;
            },
          },
        ],
      }),
      createPeerConnection: () => ({
        close() {
          pcClosed += 1;
        },
      }),
      fetchImpl: () =>
        new Promise((resolve) => {
          resolveFetch = resolve;
          notifyFetchStarted();
        }),
    });

    const pressPromise = ctrl.press();
    await fetchStartedPromise;

    // Release while fetchImpl is still pending: microphone track and PC must stop immediately!
    ctrl.release();
    expect(trackStopped).toBe(1);
    expect(pcClosed).toBe(1);
    expect(ctrl.getState()).toBe("idle");

    // When the slow fetch finally resolves later, tracks are not stopped a second time
    resolveFetch({
      ok: true,
      status: 200,
      text: async () => "v=0\r\ns=answer\r\n",
    });
    const res = await pressPromise;
    expect(res).toBe(false);
    expect(trackStopped).toBe(1);
    expect(pcClosed).toBe(1);
  });

  test("[P1-APP-02 & P1-APP-04] App & useNvrStore bound live arrays, sanitize cameraActivity & export URLs, extract OIDC callback errors, and close AudioContext", () => {
    // 1. Bounded events & reviews
    const manyEvents = Array.from({ length: MAX_LIVE_EVENTS }, (_, i) =>
      makeEventItem(`ev_${i}`)
    );
    const nextEvents = upsertBoundedEventList(
      manyEvents,
      makeEventItem("ev_new")
    );
    expect(nextEvents.length).toBe(MAX_LIVE_EVENTS);
    expect(nextEvents[0].id).toBe("ev_new");

    const manyReviews = Array.from({ length: MAX_LIVE_REVIEWS }, (_, i) =>
      makeReviewItem(`rev_${i}`)
    );
    const nextReviews = prependBoundedReviewAlert(
      manyReviews,
      makeReviewItem("rev_new")
    );
    expect(nextReviews.length).toBe(MAX_LIVE_REVIEWS);
    expect(nextReviews[0].id).toBe("rev_new");

    // 2. Bounded statsHistory & prototype-pollution-safe cameraActivity in useNvrStore
    const oversizedHistory = Array.from(
      { length: MAX_STATS_HISTORY_SAMPLES + 50 },
      (_, idx) => ({
        cameras: {},
        detectors: {},
        camera_fps: idx,
        process_fps: 5,
        skipped_fps: 0,
        detection_fps: 1,
        cpu_usages: {},
        service: {
          uptime: idx,
          version: "1.0.0",
          latest_version: "1.0.0",
          storage: {},
          last_updated: idx,
        },
        processes: {},
      })
    );
    useNvrStore.getState().setStatsHistory(oversizedHistory);
    expect(useNvrStore.getState().statsHistory.length).toBe(
      MAX_STATS_HISTORY_SAMPLES
    );

    const pollutedActivity = JSON.parse(
      '{"__proto__":{"polluted":true},"front_door":{"motion":true,"objects":[]}}'
    );
    useNvrStore.getState().setCameraActivity(pollutedActivity);
    expect(
      Object.prototype.hasOwnProperty.call(
        useNvrStore.getState().cameraActivity,
        "__proto__"
      )
    ).toBe(false);
    expect(useNvrStore.getState().cameraActivity.front_door?.motion).toBe(true);
    expect(MAX_CAMERA_ACTIVITY_ENTRIES).toBe(256);

    // 3. OIDC error extraction prevents auto_redirect loops
    expect(
      extractOidcCallbackError(
        "?error=access_denied&error_description=User+denied+access"
      )
    ).toBe("User denied access");
    expect(extractOidcCallbackError("?oidc_error=invalid_state")).toBe(
      "invalid_state"
    );
    expect(extractOidcCallbackError("", "/api/auth/oidc/callback")).toBe(
      "SSO callback failed"
    );
    expect(extractOidcCallbackError("", "/")).toBeNull();

    // 4. Export videoUrl sanitization blocks javascript:, data:, protocol-relative, and path traversal URLs
    expect(
      sanitizeExportVideoUrl("javascript:alert(1)", "front door", 100, 200)
    ).toBe("/exports/front%20door_100_200.mp4");
    expect(
      sanitizeExportVideoUrl("https://evil.example/clip.mp4", "cam1", 100, 200)
    ).toBe("/exports/cam1_100_200.mp4");
    expect(
      sanitizeExportVideoUrl("/exports/../etc/passwd", "cam1", 100, 200)
    ).toBe("/exports/cam1_100_200.mp4");
    expect(
      sanitizeExportVideoUrl("/exports/cam1_100_200.mp4", "cam1", 100, 200)
    ).toBe("/exports/cam1_100_200.mp4");

    // 5. playTacticalAlertChime closes AudioContext when oscillator ends
    let closedCount = 0;
    let capturedOnEnded: (() => void) | null = null;
    const prevWin = (globalThis as unknown as { window?: unknown }).window;
    (globalThis as unknown as { window: unknown }).window = {
      AudioContext: class MockAudioContext {
        state = "running";
        currentTime = 0;
        destination = {};
        createOscillator() {
          const osc = {
            type: "sine",
            frequency: { setValueAtTime() {} },
            connect() {},
            start() {},
            stop() {},
            set onended(fn: () => void) {
              capturedOnEnded = fn;
            },
          };
          return osc;
        }
        createGain() {
          return {
            gain: {
              setValueAtTime() {},
              exponentialRampToValueAtTime() {},
            },
            connect() {},
          };
        }
        async close() {
          this.state = "closed";
          closedCount += 1;
        }
      },
    };
    try {
      playTacticalAlertChime();
      expect(capturedOnEnded).not.toBeNull();
      capturedOnEnded!();
      expect(closedCount).toBe(1);
    } finally {
      (globalThis as unknown as { window?: unknown }).window = prevWin;
    }
  });

  test("[P1-SHARE-01] buildSharePublicUrl and isValidShareTokenSegment reject percent-encoded path traversal and handle NaN/Infinity view counts", () => {
    expect(isValidShareTokenSegment("%2e%2e%2fadmin")).toBe(false);
    expect(isValidShareTokenSegment("tok%2fextra")).toBe(false);
    expect(isValidShareTokenSegment("tok%5cevil")).toBe(false);
    expect(isValidShareTokenSegment("tok<script>")).toBe(false);
    expect(
      buildSharePublicUrl("https://nvr.local", "/share/%2e%2e%2fadmin")
    ).toBe("");
    expect(
      buildSharePublicUrl("https://nvr.local", "/share/tok%2fextra")
    ).toBe("");
    expect(
      buildSharePublicUrl(
        "https://nvr.local",
        "https://nvr.local/share/sub/%2e%2e/tok"
      )
    ).toBe("");

    expect(
      formatShareLinkViews({ access_count: Number.NaN, max_views: 5 })
    ).toBe("0/5");
    expect(
      formatShareLinkViews({
        access_count: Number.POSITIVE_INFINITY,
        max_views: 5,
      })
    ).toBe("0/5");

    const rot = computeShareLinkRotationParams(
      {
        id: "shr_nan",
        kind: "export",
        target_id: "exp_1",
        camera: "cam1",
        created_by: "admin",
        created_at: 1000,
        expires_at: 2000,
        revoked: false,
        access_count: Number.NaN,
        max_views: 5,
      },
      1500
    );
    expect(rot?.remainingMaxViews).toBe(5);
  });

  test("[P1-TIMELINE-02 & P1-FORMAT-01] TimelineScrubber, RecordingCalendarPopover, TacticalCameraPicker, format, liveStageLayout, and themeTokens guard NaN/Infinity/zero-width inputs", () => {
    // TimelineScrubber dayBounds and zero-width track math
    const bounds = parseLocalDayBounds("2026-invalid-01", 1700001000);
    expect(Number.isFinite(bounds.dayStart)).toBe(true);
    expect(Number.isFinite(bounds.dayEnd)).toBe(true);
    expect(bounds.dayEnd).toBeGreaterThan(bounds.dayStart);

    expect(computeClientXTimestamp(150, 100, 0, 1000, 2000)).toBe(2000);
    expect(computeClientXTimestamp(150, 100, -50, 1000, 2000)).toBe(2000);
    expect(computeClientXTimestamp(150, 100, 100, 1000, 2000)).toBe(1500);

    // RecordingCalendarPopover & TacticalCameraPicker
    expect(formatReadableDate("2026-xx-01")).toBe("2026-xx-01");
    expect(resolvePickerMode(4, Number.NaN)).toBe("inline");
    expect(resolvePickerMode(10, Number.NaN)).toBe("combobox");

    // format.ts NaN/Infinity/negative/float guards
    expect(formatTs(Number.NaN)).toBe("--:--:--");
    expect(formatTs(Number.POSITIVE_INFINITY)).toBe("--:--:--");
    expect(formatDateTime(Number.NaN)).toBe("--:--:--");
    expect(formatDuration(Number.NaN)).toBe("1s");
    expect(formatDuration(Number.POSITIVE_INFINITY)).toBe("1s");
    expect(formatDuration(-100)).toBe("1s");
    expect(formatRelTime(Number.NaN)).toBe("0s ago");
    expect(formatRelTime(Number.POSITIVE_INFINITY)).toBe("0s ago");
    expect(formatUptime(Number.NaN)).toBe("0h 0m 0s");
    expect(formatUptime(Number.POSITIVE_INFINITY)).toBe("0h 0m 0s");
    expect(formatUptime(-45)).toBe("0h 0m 0s");
    expect(formatUptime(3665.789)).toBe("1h 1m 5s");
    expect(
      resolveHeaderTelemetryMeta(true, Number.POSITIVE_INFINITY).channelBadge
    ).toBe("0 CH");
    expect(findMatchingExport([], "cam1", Number.NaN, null)).toBeNull();

    // Storage access error resilience when localStorage getter throws SecurityError
    const prevWin = (globalThis as unknown as { window?: unknown }).window;
    (globalThis as unknown as { window: unknown }).window = {
      get localStorage(): Storage {
        throw new Error("SecurityError: localStorage disabled");
      },
    };
    try {
      expect(resolveInitialThemeMode()).toBe("dark");
      expect(resolveInitialHudPosition()).toBe("bottom-right");
      expect(resolveInitialLiveLayout()).toBe("2x2");
      expect(resolveInitialMatrixDensity()).toBe("2x2");
      expect(resolveInitialBool("k", true)).toBe(true);
      expect(resolveInitialPlaybackSpeed()).toBe(1);
      expect(resolveInitialFirstWeekday()).toBe("sunday");
    } finally {
      (globalThis as unknown as { window?: unknown }).window = prevWin;
    }

    // liveStageLayout & themeTokens NaN/Infinity guards
    const heroStyle = resolvePersistentTileStyle("focus", {
      camera: "cam1",
      role: "hero",
      renderMode: "mse",
      useMainStream: true,
      dockIndex: -1,
      dockTotal: Number.NaN,
      isTimelineSelected: true,
      isSpotlightHeld: false,
    });
    expect(heroStyle?.gridRow).toBe("1 / span 1");

    const dedupedGrid = computeLiveStageDescriptors({
      liveLayout: "2x2",
      visibleMatrixCams: ["cam1", "cam1", "cam2"],
      groupCams: ["cam1", "cam2"],
      selectedCamera: "cam1",
      hoveredDockCam: null,
      useMainStream: false,
      timeShiftTs: null,
      maxGridLiveDecoders: Number.NaN,
    });
    expect(dedupedGrid.map((d) => d.camera)).toEqual(["cam1", "cam2"]);

    expect(rgbToHex({ r: Number.NaN, g: 255, b: Number.NEGATIVE_INFINITY })).toBe(
      "#00ff00"
    );
    expect(srgbChannelToLinear(Number.NaN)).toBe(0);
    expect(cieLightnessFromLuminance(Number.NaN)).toBe(0);
    expect(
      quantizeHdmiLimitedRange({ r: Number.NaN, g: 20, b: 30 })
    ).toEqual({ r: 16, g: 20, b: 30 });
    expect(
      Number.isFinite(
        simulateGamma24Transfer("#192231", Number.POSITIVE_INFINITY).lStar
      )
    ).toBe(true);

    // StreamPrewarmCoordinator LRU cap
    const prewarm = new StreamPrewarmCoordinator(10_000);
    for (let i = 0; i < 300; i++) {
      expect(prewarm.shouldPrewarmStream(`cam_${i}`, 1000)).toBe(true);
    }
    // Oldest entry (cam_0) was evicted at 256 cap, while recent entry (cam_299) is still within cooldown
    expect(prewarm.shouldPrewarmStream("cam_299", 2000)).toBe(false);
    expect(prewarm.shouldPrewarmStream("cam_0", 2000)).toBe(true);
  });

  test("[R2-P0-YAML-01] buildCleanYamlFromDraft preserves '***' secret masks when rawYaml is scrubbed or empty, restores '#'/'?' & query-param URLs, and guards NaN integer fields", () => {
    const scrubbedRawYaml = `
mqtt:
  enabled: true
  host: 127.0.0.1
  port: 1883
  password: "***"
genai:
  enabled: true
  provider: gemini
  api_key: "***"
go2rtc:
  streams:
    hash_pw_cam:
      - "rtsp://admin:my#pass?1@192.0.2.77:554/stream1"
    query_pw_cam:
      - "rtsp://192.0.2.88:554/cam/realmonitor?channel=1&subtype=0&user=admin&password=q_secret_99"
cameras:
    "gate_4space":
        enabled: true
        onvif:
            host: 192.0.2.99
            password: "***"
`;

    const draft: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      mqtt: {
        enabled: true,
        host: "127.0.0.1",
        port: Number.NaN,
        topic_prefix: ".5",
        client_id: "1_000",
        stats_interval: Number.POSITIVE_INFINITY,
        user: "nan",
        has_password: true,
      },
      auth: {
        enabled: true,
        reset_admin_password: false,
        cookie_name: "n\u2028injected: true",
        cookie_secure: false,
        session_length: Number.NaN,
        refresh_time: Number.NaN,
        hash_iterations: Number.NaN,
        trusted_proxies: [],
      },
      genai: {
        enabled: true,
        provider: "gemini",
        api_key_env: "GEMINI_API_KEY",
        has_api_key: true,
        model: "gemini-2.5-flash",
        verify_ambiguous_alerts: true,
        ambiguous_score_min: 0.45,
        ambiguous_score_max: 0.72,
        auto_suppress_false_positives: true,
      },
      go2rtc: {
        streams: {
          hash_pw_cam: ["rtsp://*:*@192.0.2.77:554/stream1_modified"],
          query_pw_cam: [
            "rtsp://192.0.2.88:554/cam/realmonitor?channel=1&subtype=1&user=admin&password=***",
          ],
        },
      },
      cameras: {
        gate_4space: {
          enabled: true,
          onvif: {
            host: "192.0.2.99",
            port: Number.NaN,
            user: "inf",
            password: "***",
            has_password: true,
          },
          detect: {
            enabled: true,
            width: Number.NaN,
            height: Number.NaN,
            fps: Number.NaN,
          },
          motion: {
            enabled: true,
            threshold: Number.NaN,
            contour_area: Number.NaN,
            mask: {},
          },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true, quality: Number.NaN },
        },
      },
    };

    const yaml = buildCleanYamlFromDraft(draft, scrubbedRawYaml);
    // Masked secrets ("***") must be preserved in YAML so POST /api/config/save restores them from memory
    expect(yaml).toContain('password: "***"');
    expect(yaml).toContain('api_key: "***"');
    // RTSP URL with '#' and '?' in password and modified path is restored losslessly
    expect(yaml).toContain(
      '"rtsp://admin:my#pass?1@192.0.2.77:554/stream1_modified"'
    );
    // RTSP URL with query-param '?password=***' is restored losslessly
    expect(yaml).toContain(
      "rtsp://192.0.2.88:554/cam/realmonitor?channel=1&subtype=1&user=admin&password=q_secret_99"
    );
    // NaN integer fields fall back to safe defaults instead of serializing 0
    expect(yaml).toContain("port: 1883");
    expect(yaml).toContain("stats_interval: 60");
    expect(yaml).toContain("session_length: 86400");
    expect(yaml).toContain("width: 1280");
    expect(yaml).toContain("height: 720");
    expect(yaml).toContain("fps: 5");
    expect(yaml).toContain("quality: 70");
    // Leading-dot floats, underscored numbers, YAML 'nan'/'inf', and Unicode line-breaks are quoted/escaped
    expect(yaml).toContain('topic_prefix: ".5"');
    expect(yaml).toContain('client_id: "1_000"');
    expect(yaml).toContain('user: "nan"');
    expect(yaml).toContain('user: "inf"');
    expect(yaml).toContain('cookie_name: "n\\u2028injected: true"');
    expect(yaml).not.toContain("\u2028");
  });

  test("[R2-P1-STORE-01] resolveValidCameraSelection and useNvrStore.setConfig reconcile selectedCamera when default 'escritorio' is absent from config.cameras", () => {
    useNvrStore.setState({
      selectedCamera: "escritorio",
      simulatedFleetCount: 0,
    });
    const cfg: NinjaConfigResponse = {
      version: "0.18-0",
      safe_mode: false,
      cameras: {
        disabled_cam: {
          enabled: false,
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
        front_door: {
          enabled: true,
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
        back_patio: {
          enabled: true,
          detect: { enabled: true, width: 1280, height: 720, fps: 5 },
          motion: { enabled: true, threshold: 30, contour_area: 10, mask: {} },
          zones: {},
          record: { enabled: true },
          snapshots: { enabled: true },
        },
      },
    };

    expect(resolveValidCameraSelection("escritorio", cfg.cameras, 0)).toBe(
      "front_door"
    );
    expect(resolveValidCameraSelection("back_patio", cfg.cameras, 0)).toBe(
      "back_patio"
    );
    expect(
      resolveValidCameraSelection("front_door_sim_01", cfg.cameras, 12)
    ).toBe("front_door_sim_01");

    useNvrStore.getState().setConfig(cfg);
    expect(useNvrStore.getState().selectedCamera).toBe("front_door");
  });

  test("[R2-P1-MSE-01] DualSlotMachine rejects late markSlotReady on idle/errored slots and createPushToTalkController closes native RTCPeerConnection on in-flight release", async () => {
    const machine = new DualSlotMachine();
    expect(machine.requestStream("   ").action).toBe("noop");

    const s1 = machine.requestStream("cam1_sub");
    machine.markSlotError(0, s1.sessionId);
    // Late ready on an errored slot must be rejected and must not promote or mutate status to ready
    const lateErrReady = machine.markSlotReady(0, s1.sessionId);
    expect(lateErrReady.promoted).toBe(false);
    expect(machine.getState().slots[0].status).toBe("error");

    const s2 = machine.requestStream("cam1_sub");
    machine.markSlotReady(0, s2.sessionId);
    const s3 = machine.requestStream("cam1");
    // Abort standby slot 1 by requesting already-active "cam1_sub"
    const s4 = machine.requestStream("cam1_sub");
    expect(s4.abortSlot).toBe(1);
    expect(machine.getState().slots[1].status).toBe("idle");
    // Late ready for aborted s3.sessionId must keep slot 1 status as "idle"
    const lateIdleReady = machine.markSlotReady(1, s3.sessionId);
    expect(lateIdleReady.promoted).toBe(false);
    expect(machine.getState().slots[1].status).toBe("idle");

    // Verify native RTCPeerConnection (when deps.createPeerConnection is omitted) is closed immediately on in-flight release
    let nativePcClosed = 0;
    let notifyFetchStarted!: () => void;
    const fetchStarted = new Promise<void>((r) => {
      notifyFetchStarted = r;
    });
    let resolveFetch!: (val: {
      ok: boolean;
      status: number;
      text: () => Promise<string>;
    }) => void;
    const prevRtc = (globalThis as unknown as { RTCPeerConnection?: unknown })
      .RTCPeerConnection;
    (globalThis as unknown as { RTCPeerConnection: unknown }).RTCPeerConnection =
      class MockNativePeerConnection {
        localDescription = { type: "offer", sdp: "v=0\r\n" };
        addTrack() {}
        async createOffer() {
          return { type: "offer", sdp: "v=0\r\n" };
        }
        async setLocalDescription() {}
        async setRemoteDescription() {}
        close() {
          nativePcClosed += 1;
        }
      };
    try {
      const ctrl = createPushToTalkController("front_door", {
        getUserMedia: async () => ({
          getTracks: () => [{ stop() {} }],
        }),
        fetchImpl: () =>
          new Promise((resolve) => {
            resolveFetch = resolve;
            notifyFetchStarted();
          }),
      });
      const p = ctrl.press();
      await fetchStarted;
      ctrl.release();
      expect(nativePcClosed).toBe(1);
      resolveFetch({
        ok: true,
        status: 200,
        text: async () => "v=0\r\ns=answer\r\n",
      });
      expect(await p).toBe(false);
      expect(nativePcClosed).toBe(1);
    } finally {
      (
        globalThis as unknown as { RTCPeerConnection?: unknown }
      ).RTCPeerConnection = prevRtc;
    }
  });

  test("[R2-P1-TIMELINE-01] sanitizeTimelineThumbUrl, formatReadableDate, parseLocalDayBounds, getCameraPickerStatus, and findMatchingExport guard malformed inputs", () => {
    expect(
      sanitizeTimelineThumbUrl(
        "/media/node1/clips/front_door_1.webp",
        "front_door"
      )
    ).toBe("/clips/front_door_1.webp");
    expect(
      sanitizeTimelineThumbUrl("https://evil.example/track.webp", "front_door")
    ).toBe("/api/front_door/latest.webp");
    expect(
      sanitizeTimelineThumbUrl("//evil.example/track.webp", "front_door")
    ).toBe("/api/front_door/latest.webp");
    expect(
      sanitizeTimelineThumbUrl("/clips/%2e%2e/secret.webp", "front_door")
    ).toBe("/api/front_door/latest.webp");

    // Empty-segment and overflow date strings are rejected
    expect(formatReadableDate("2026--01")).toBe("2026--01");
    expect(formatReadableDate("2026-00-15")).toBe("2026-00-15");
    expect(formatReadableDate("2026-13-01")).toBe("2026-13-01");
    expect(formatReadableDate("2026-02-31")).toBe("2026-02-31");

    const nowSec = 1770000000;
    const emptySegBounds = parseLocalDayBounds("2026--01", nowSec);
    expect(emptySegBounds.isToday).toBe(true);

    // TacticalCameraPicker NaN FPS and prototype key guards
    expect(
      getCameraPickerStatus("cam1", {
        cam1: {
          camera_fps: Number.NaN,
          process_fps: 0,
          skipped_fps: 0,
          detection_fps: 0,
          detection_enabled: true,
          pid: 1,
          capture_pid: 1,
          ffmpeg_pid: 1,
        },
      })
    ).toBe("offline");
    expect(getCameraPickerStatus("toString", {}, {})).toBe("live");

    // findMatchingExport rejects empty timestamp segments ("cam1__9999999999")
    expect(
      findMatchingExport(
        [
          {
            id: "cam1__9999999999",
            camera: "cam1",
            name: "other",
            date: 100,
            video_path: "/exports/a.mp4",
            thumb_path: "",
            in_progress: false,
          },
        ],
        "cam1",
        1700000000,
        1700000020
      )
    ).toBeNull();
  });
});
