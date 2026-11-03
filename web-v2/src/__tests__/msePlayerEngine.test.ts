import { describe, expect, test } from "bun:test";
import {
  computeLiveEdgeSeekTarget,
  DualSlotMachine,
  LiveFrameBridge,
  resolveLivePosterUrl,
  StreamPrewarmCoordinator,
} from "../utils/msePlayerEngine";

describe("msePlayerEngine (#91 [UX-16])", () => {
  test("dual_slot_machine_keeps_active_slot_visible_until_standby_slot_decodes", () => {
    const machine = new DualSlotMachine();

    // 1. Initial mount starts in slot 0
    const t1 = machine.requestStream("escritorio_sub");
    expect(t1.action).toBe("start_active");
    expect(t1.targetSlot).toBe(0);
    expect(t1.abortSlot).toBeNull();
    expect(machine.getState().isAnyReady).toBe(false);

    // 2. Slot 0 fires loadeddata -> becomes ready & active
    const r1 = machine.markSlotReady(0, t1.sessionId);
    expect(r1.promoted).toBe(true);
    expect(r1.activeSlot).toBe(0);
    expect(r1.teardownSlot).toBeNull();
    expect(machine.getState().isAnyReady).toBe(true);
    expect(machine.getState().activeSlot).toBe(0);

    // 3. Upgrade from escritorio_sub to escritorio (main stream) -> starts in standby slot 1 while slot 0 stays active
    const t2 = machine.requestStream("escritorio");
    expect(t2.action).toBe("start_standby");
    expect(t2.targetSlot).toBe(1);
    expect(t2.abortSlot).toBeNull();
    const midState = machine.getState();
    expect(midState.activeSlot).toBe(0);
    expect(midState.standbySlot).toBe(1);
    expect(midState.isAnyReady).toBe(true);
    expect(midState.slots[0].status).toBe("ready");
    expect(midState.slots[0].streamSrc).toBe("escritorio_sub");
    expect(midState.slots[1].status).toBe("connecting");
    expect(midState.slots[1].streamSrc).toBe("escritorio");

    // 4. Standby slot 1 fires loadeddata -> atomically promoted to active, slot 0 marked for teardown
    const r2 = machine.markSlotReady(1, t2.sessionId);
    expect(r2.promoted).toBe(true);
    expect(r2.activeSlot).toBe(1);
    expect(r2.teardownSlot).toBe(0);

    const finalState = machine.getState();
    expect(finalState.activeSlot).toBe(1);
    expect(finalState.standbySlot).toBeNull();
    expect(finalState.isAnyReady).toBe(true);
    expect(finalState.slots[1].status).toBe("ready");
    expect(finalState.slots[1].streamSrc).toBe("escritorio");
    expect(finalState.slots[0].status).toBe("idle");
  });

  test("dual_slot_machine_cancels_in_flight_standby_on_rapid_camera_switch", () => {
    const machine = new DualSlotMachine();

    // Slot 0 playing cam1_sub
    const t1 = machine.requestStream("cam1_sub");
    machine.markSlotReady(0, t1.sessionId);

    // Switch to cam1 (main) in standby slot 1
    const t2 = machine.requestStream("cam1");
    expect(t2.action).toBe("start_standby");
    expect(t2.targetSlot).toBe(1);

    // Before cam1 finishes loading, operator switches rapidly to cam2 -> aborts cam1 in slot 1 and starts cam2 in slot 1
    const t3 = machine.requestStream("cam2");
    expect(t3.action).toBe("start_standby");
    expect(t3.targetSlot).toBe(1);
    expect(t3.abortSlot).toBe(1);
    expect(t3.sessionId).toBeGreaterThan(t2.sessionId);

    // Stale loadeddata from aborted cam1 session must be ignored
    const staleReady = machine.markSlotReady(1, t2.sessionId);
    expect(staleReady.promoted).toBe(false);
    expect(machine.getState().activeSlot).toBe(0);

    // Fresh loadeddata from cam2 session promotes slot 1 and tears down slot 0
    const freshReady = machine.markSlotReady(1, t3.sessionId);
    expect(freshReady.promoted).toBe(true);
    expect(freshReady.activeSlot).toBe(1);
    expect(freshReady.teardownSlot).toBe(0);
    expect(machine.getState().slots[1].streamSrc).toBe("cam2");
  });

  test("live_frame_bridge_stores_retrieves_and_expires_captured_frames_with_lru_cap", () => {
    const bridge = new LiveFrameBridge(3, 15_000);
    const t0 = 1_000_000;

    bridge.store("cam1", {
      dataUrl: "data:image/jpeg;base64,CAM1",
      width: 1280,
      height: 720,
      capturedAtMs: t0,
    });
    bridge.store("cam2", {
      dataUrl: "data:image/jpeg;base64,CAM2",
      width: 1920,
      height: 1080,
      capturedAtMs: t0 + 100,
    });
    bridge.store("cam3", {
      dataUrl: "data:image/jpeg;base64,CAM3",
      width: 1280,
      height: 720,
      capturedAtMs: t0 + 200,
    });

    // Access cam1 so cam2 becomes the oldest LRU entry
    expect(bridge.getFresh("cam1", t0 + 500)?.dataUrl).toBe("data:image/jpeg;base64,CAM1");

    // Storing cam4 exceeds maxEntries=3 -> evicts cam2
    bridge.store("cam4", {
      dataUrl: "data:image/jpeg;base64,CAM4",
      width: 1280,
      height: 720,
      capturedAtMs: t0 + 600,
    });
    expect(bridge.size()).toBe(3);
    expect(bridge.getFresh("cam2", t0 + 700)).toBeNull();
    expect(bridge.getFresh("cam1", t0 + 700)?.dataUrl).toBe("data:image/jpeg;base64,CAM1");
    expect(bridge.getFresh("cam4", t0 + 700)?.dataUrl).toBe("data:image/jpeg;base64,CAM4");

    // After 15,000ms TTL, cam1 expires
    expect(bridge.getFresh("cam1", t0 + 16_000)).toBeNull();
  });

  test("resolve_live_poster_url_prefers_captured_frame_and_omits_360p_cap_on_main_stream", () => {
    const bridge = new LiveFrameBridge(5, 15_000);
    const now = 5_000_000;

    // 1. Without captured frame: main stream omits height=360, substream includes height=360
    expect(resolveLivePosterUrl("escritorio", true, 123, now, bridge)).toBe(
      "/api/escritorio/latest.jpg?cache=123"
    );
    expect(resolveLivePosterUrl("escritorio", false, 123, now, bridge)).toBe(
      "/api/escritorio/latest.jpg?height=360&cache=123"
    );

    // 2. With fresh captured frame in bridge: returns exact dataUrl immediately
    bridge.store("escritorio", {
      dataUrl: "data:image/jpeg;base64,LIVE_GPU_FRAME",
      width: 1920,
      height: 1080,
      capturedAtMs: now - 500,
    });
    expect(resolveLivePosterUrl("escritorio", true, 123, now, bridge)).toBe(
      "data:image/jpeg;base64,LIVE_GPU_FRAME"
    );
    expect(resolveLivePosterUrl("escritorio", false, 123, now, bridge)).toBe(
      "data:image/jpeg;base64,LIVE_GPU_FRAME"
    );

    // 3. After TTL expires, falls back to server URL
    expect(resolveLivePosterUrl("escritorio", true, 456, now + 20_000, bridge)).toBe(
      "/api/escritorio/latest.jpg?cache=456"
    );
  });

  test("stream_prewarm_coordinator_enforces_cooldown_and_deduplicates_requests", () => {
    const coord = new StreamPrewarmCoordinator(10_000);
    const t0 = 2_000_000;

    expect(coord.shouldPrewarmStream("escritorio", t0)).toBe(true);
    // Duplicate within 10s cooldown is blocked
    expect(coord.shouldPrewarmStream("escritorio", t0 + 3_000)).toBe(false);
    // Different camera stream is allowed immediately
    expect(coord.shouldPrewarmStream("sala", t0 + 3_000)).toBe(true);
    // After 10s cooldown expires, re-warming is allowed
    expect(coord.shouldPrewarmStream("escritorio", t0 + 10_001)).toBe(true);
  });

  test("compute_live_edge_seek_target_snaps_on_initial_gop_burst_and_drift", () => {
    // 1. Initial GOP burst (1.40s buffered) snaps to bufferedEnd - 0.08 = 1.32
    expect(computeLiveEdgeSeekTarget(0, 1.4, 0, true)).toBeCloseTo(1.32, 3);
    // Very short buffer (< 0.25s) on initial burst does not seek
    expect(computeLiveEdgeSeekTarget(0, 0.15, 0, true)).toBeNull();

    // 2. Steady-state playback with small lag (0.6s <= 1.8s) does not seek
    expect(computeLiveEdgeSeekTarget(0, 5.0, 4.4, false)).toBeNull();
    // Steady-state playback with drift > 1.8s snaps to bufferedEnd - 0.15 = 4.85
    expect(computeLiveEdgeSeekTarget(0, 5.0, 2.5, false)).toBeCloseTo(4.85, 3);
  });
});
