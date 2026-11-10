import { describe, expect, test } from "bun:test";
import {
  coalesceArrayBuffers,
  computeCameraJitterSeed,
  computeLiveEdgeSeekTarget,
  computeMseReconnectDelayMs,
  createMseDualSlotSessionController,
  DualSlotMachine,
  LiveFrameBridge,
  MseBufferedRange,
  MseMediaSourceLike,
  MsePlatformAdapter,
  MseSlotMediaHandle,
  MseSourceBufferLike,
  MseWebSocketLike,
  resolveLivePosterUrl,
  StreamPrewarmCoordinator,
  VideoSlotIndex,
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
    // Out-of-bounds currentTime ahead of bufferedEnd (e.g., upstream tfdt reset) snaps back into range
    expect(computeLiveEdgeSeekTarget(0, 2.0, 45.0, false)).toBeCloseTo(1.85, 3);
  });

  test("dual_slot_machine_recovers_from_pod_rollout_disconnect_and_standby_upgrade_failure", () => {
    const machine = new DualSlotMachine();
    expect(machine.needsReconnect("cozinha_sub")).toBe(true);

    // 1. Initial connection on slot 0
    const t1 = machine.requestStream("cozinha_sub");
    expect(t1.action).toBe("start_active");
    expect(machine.needsReconnect("cozinha_sub")).toBe(false);

    machine.markSlotReady(0, t1.sessionId);
    expect(machine.getState().isAnyReady).toBe(true);
    expect(machine.needsReconnect("cozinha_sub")).toBe(false);

    // 2. Pod rollout or internet drop closes active slot 0 WebSocket
    const afterDrop = machine.markSlotError(0, t1.sessionId);
    expect(afterDrop.isAnyReady).toBe(false);
    expect(afterDrop.slots[0].status).toBe("error");
    expect(machine.needsReconnect("cozinha_sub")).toBe(true);

    // 3. Auto-reconnect requests cozinha_sub again on slot 0 with a fresh sessionId
    const t2 = machine.requestStream("cozinha_sub");
    expect(t2.action).toBe("start_active");
    expect(t2.targetSlot).toBe(0);
    expect(t2.sessionId).toBeGreaterThan(t1.sessionId);
    expect(machine.needsReconnect("cozinha_sub")).toBe(false);

    machine.markSlotReady(0, t2.sessionId);
    expect(machine.getState().isAnyReady).toBe(true);
    expect(machine.needsReconnect("cozinha_sub")).toBe(false);

    // 4. Operator upgrades to main stream ("cozinha") in standby slot 1, which fails transiently
    const t3 = machine.requestStream("cozinha");
    expect(t3.action).toBe("start_standby");
    expect(t3.targetSlot).toBe(1);
    expect(machine.needsReconnect("cozinha")).toBe(false);

    const afterStandbyErr = machine.markSlotError(1, t3.sessionId);
    // Slot 0 remains visible with cozinha_sub, but desired stream "cozinha" still needs reconnect
    expect(afterStandbyErr.isAnyReady).toBe(true);
    expect(afterStandbyErr.standbySlot).toBeNull();
    expect(machine.needsReconnect("cozinha")).toBe(true);

    // 5. Auto-reconnect retries "cozinha" in standby slot 1 and promotes cleanly
    const t4 = machine.requestStream("cozinha");
    expect(t4.action).toBe("start_standby");
    expect(t4.targetSlot).toBe(1);
    const promoted = machine.markSlotReady(1, t4.sessionId);
    expect(promoted.promoted).toBe(true);
    expect(promoted.activeSlot).toBe(1);
    expect(machine.needsReconnect("cozinha")).toBe(false);
  });

  test("compute_mse_reconnect_delay_ms_applies_capped_exponential_backoff_and_camera_jitter", () => {
    expect(computeMseReconnectDelayMs(0, 0)).toBe(1000);
    expect(computeMseReconnectDelayMs(1, 0)).toBe(2000);
    expect(computeMseReconnectDelayMs(2, 0)).toBe(3500);
    expect(computeMseReconnectDelayMs(3, 0)).toBe(5000);
    expect(computeMseReconnectDelayMs(10, 0)).toBe(5000);

    const seedA = computeCameraJitterSeed("escritorio");
    const seedB = computeCameraJitterSeed("cozinha");
    expect(seedA).toBeGreaterThanOrEqual(0);
    expect(seedA).toBeLessThanOrEqual(300);
    expect(seedB).toBeGreaterThanOrEqual(0);
    expect(seedB).toBeLessThanOrEqual(300);
    expect(seedA).not.toBe(seedB);
    expect(computeMseReconnectDelayMs(0, seedA)).toBe(1000 + seedA);
  });

  test("coalesce_array_buffers_merges_gop_burst_chunks_in_order", () => {
    expect(coalesceArrayBuffers([])).toBeNull();
    const c1 = new Uint8Array([1, 2, 3]).buffer;
    const c2 = new Uint8Array([4, 5]).buffer;
    const c3 = new Uint8Array([6, 7, 8, 9]).buffer;
    const merged = coalesceArrayBuffers([c1, c2, c3]);
    expect(merged).not.toBeNull();
    expect(Array.from(new Uint8Array(merged!))).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9]);
  });
});

interface FakeTimerEntry {
  id: number;
  ms: number;
  cb: () => void;
}

function createFakeMseHarness() {
  let nextTimerId = 1;
  const timers = new Map<number, FakeTimerEntry>();
  const createdUrls: string[] = [];
  const revokedUrls: string[] = [];
  const capturedSlots: VideoSlotIndex[] = [];

  class FakeSlotMedia implements MseSlotMediaHandle {
    public currentTime = 0;
    public readyState = 3;
    public paused = true;
    public seeking = false;
    public src = "";
    public disableRemotePlayback = false;
    public bufferedRange: MseBufferedRange | null = { start: 0, end: 1.5 };
    public playCalls = 0;
    public resetCalls = 0;
    private readonly listeners = new Map<string, Set<() => void>>();

    public getBufferedRange(): MseBufferedRange | null {
      return this.bufferedRange ? { ...this.bufferedRange } : null;
    }

    public play(): void {
      this.paused = false;
      this.playCalls += 1;
    }

    public pause(): void {
      this.paused = true;
    }

    public resetSrc(): void {
      this.paused = true;
      this.src = "";
      this.resetCalls += 1;
    }

    public addEventListener(type: string, listener: () => void): void {
      let set = this.listeners.get(type);
      if (!set) {
        set = new Set();
        this.listeners.set(type, set);
      }
      set.add(listener);
    }

    public removeEventListener(type: string, listener: () => void): void {
      this.listeners.get(type)?.delete(listener);
    }

    public emit(type: string): void {
      for (const fn of Array.from(this.listeners.get(type) ?? [])) {
        fn();
      }
    }
  }

  class FakeSourceBuffer implements MseSourceBufferLike {
    public mode: "segments" | "sequence" = "segments";
    public updating = false;
    public readonly appended: Uint8Array[] = [];
    public readonly removedRanges: Array<[number, number]> = [];
    public failNextAppendWithQuota = false;
    public manualUpdateEnd = false;
    private readonly listeners = new Map<string, Set<() => void>>();

    public appendBuffer(data: ArrayBuffer): void {
      if (this.failNextAppendWithQuota) {
        this.failNextAppendWithQuota = false;
        const quotaErr = new Error("SourceBuffer full");
        quotaErr.name = "QuotaExceededError";
        throw quotaErr;
      }
      this.appended.push(new Uint8Array(data.slice(0)));
      this.updating = true;
      if (!this.manualUpdateEnd) {
        this.updating = false;
        this.emit("updateend");
      }
    }

    public remove(start: number, end: number): void {
      this.removedRanges.push([start, end]);
      this.updating = true;
      if (!this.manualUpdateEnd) {
        this.updating = false;
        this.emit("updateend");
      }
    }

    public finishUpdate(): void {
      this.updating = false;
      this.emit("updateend");
    }

    public addEventListener(
      type: "updateend" | "error",
      listener: () => void
    ): void {
      let set = this.listeners.get(type);
      if (!set) {
        set = new Set();
        this.listeners.set(type, set);
      }
      set.add(listener);
    }

    public removeEventListener(
      type: "updateend" | "error",
      listener: () => void
    ): void {
      this.listeners.get(type)?.delete(listener);
    }

    public emit(type: "updateend" | "error"): void {
      for (const fn of Array.from(this.listeners.get(type) ?? [])) {
        fn();
      }
    }
  }

  class FakeMediaSource implements MseMediaSourceLike {
    public readyState: "closed" | "open" | "ended" = "open";
    public readonly sourceBuffers: FakeSourceBuffer[] = [];
    public readonly addedMimeCodecs: string[] = [];
    private readonly listeners = new Map<string, Set<() => void>>();

    public addSourceBuffer(mimeCodec: string): MseSourceBufferLike {
      this.addedMimeCodecs.push(mimeCodec);
      const sb = new FakeSourceBuffer();
      this.sourceBuffers.push(sb);
      return sb;
    }

    public removeSourceBuffer(sb: MseSourceBufferLike): void {
      const idx = this.sourceBuffers.indexOf(sb as FakeSourceBuffer);
      if (idx >= 0) {
        this.sourceBuffers.splice(idx, 1);
      }
    }

    public addEventListener(type: "sourceopen", listener: () => void): void {
      let set = this.listeners.get(type);
      if (!set) {
        set = new Set();
        this.listeners.set(type, set);
      }
      set.add(listener);
    }

    public removeEventListener(type: "sourceopen", listener: () => void): void {
      this.listeners.get(type)?.delete(listener);
    }
  }

  class FakeWebSocket implements MseWebSocketLike {
    public binaryType: BinaryType = "blob";
    public onopen: (() => void) | null = null;
    public onmessage: ((ev: { data: unknown }) => void) | null = null;
    public onerror: (() => void) | null = null;
    public onclose: (() => void) | null = null;
    public readonly sent: Array<string | ArrayBuffer> = [];
    public closed = false;
    public readonly url: string;

    constructor(url: string) {
      this.url = url;
    }

    public send(data: string | ArrayBuffer): void {
      this.sent.push(data);
    }

    public close(): void {
      this.closed = true;
    }
  }

  const slotMedias: [FakeSlotMedia, FakeSlotMedia] = [
    new FakeSlotMedia(),
    new FakeSlotMedia(),
  ];
  const mediaSources: FakeMediaSource[] = [];
  const webSockets: FakeWebSocket[] = [];

  const adapter: MsePlatformAdapter = {
    getSlotMedia(slot: VideoSlotIndex) {
      return slotMedias[slot];
    },
    createMediaSource() {
      const ms = new FakeMediaSource();
      mediaSources.push(ms);
      return ms;
    },
    isTypeSupported() {
      return true;
    },
    createObjectURL() {
      const u = `blob:nvr-slot-${createdUrls.length}`;
      createdUrls.push(u);
      return u;
    },
    revokeObjectURL(url: string) {
      revokedUrls.push(url);
    },
    createWebSocket(url: string) {
      const ws = new FakeWebSocket(url);
      webSockets.push(ws);
      return ws;
    },
    setTimeout(cb: () => void, ms: number) {
      const id = nextTimerId++;
      timers.set(id, { id, ms, cb });
      return id;
    },
    clearTimeout(id: number) {
      timers.delete(id);
    },
    resolveWebSocketUrl(streamOrWsUrl: string) {
      return streamOrWsUrl.startsWith("ws")
        ? streamOrWsUrl
        : `ws://nvr.local/live/mse/api/ws?src=${encodeURIComponent(streamOrWsUrl)}`;
    },
    captureSlotFrame(slot: VideoSlotIndex) {
      capturedSlots.push(slot);
    },
  };

  const runTimerByMs = (targetMs: number): boolean => {
    for (const [id, entry] of timers.entries()) {
      if (entry.ms === targetMs) {
        timers.delete(id);
        entry.cb();
        return true;
      }
    }
    return false;
  };

  return {
    adapter,
    slotMedias,
    mediaSources,
    webSockets,
    timers,
    createdUrls,
    revokedUrls,
    capturedSlots,
    runTimerByMs,
  };
}

describe("MseDualSlotSessionController across MsePlatformAdapter seam", () => {
  test("orchestrates dual-slot connect -> codec handshake -> binary chunk coalescing -> first frame ready -> 80ms outgoing slot crossfade teardown", () => {
    const h = createFakeMseHarness();
    const ctrl = createMseDualSlotSessionController({
      adapter: h.adapter,
      jitterSeed: 0,
    });

    // 1. Connect initial stream in slot 0
    ctrl.connectStream("escritorio_sub", { isCameraOrModeSwitch: true });
    expect(h.webSockets.length).toBe(1);
    const ws0 = h.webSockets[0];
    expect(ws0.url).toBe("ws://nvr.local/live/mse/api/ws?src=escritorio_sub");
    expect(ws0.binaryType).toBe("arraybuffer");

    // 2. Open WebSocket & complete codec handshake
    ws0.onopen?.();
    expect(ws0.sent.length).toBe(1);
    expect(JSON.parse(String(ws0.sent[0])).type).toBe("mse");

    // Queue two binary chunks before SourceBuffer finishes updating to verify coalescing
    ws0.onmessage?.({
      data: JSON.stringify({
        type: "mse",
        value: 'video/mp4; codecs="avc1.640029,mp4a.40.2"',
      }),
    });
    expect(h.mediaSources[0].addedMimeCodecs).toEqual([
      'video/mp4; codecs="avc1.640029,mp4a.40.2"',
    ]);
    const sb0 = h.mediaSources[0].sourceBuffers[0];
    sb0.manualUpdateEnd = true;

    ws0.onmessage?.({ data: new Uint8Array([10, 20]).buffer });
    // While sb0 is updating, send two more chunks that must coalesce on updateend
    ws0.onmessage?.({ data: new Uint8Array([30, 40]).buffer });
    ws0.onmessage?.({ data: new Uint8Array([50]).buffer });
    expect(sb0.appended.length).toBe(1);
    expect(Array.from(sb0.appended[0])).toEqual([10, 20]);

    sb0.manualUpdateEnd = false;
    sb0.finishUpdate();
    expect(sb0.appended.length).toBe(2);
    expect(Array.from(sb0.appended[1])).toEqual([30, 40, 50]);

    // First frame ready in slot 0
    expect(ctrl.getSnapshot().mseReady).toBe(true);
    expect(ctrl.getSnapshot().activeSlot).toBe(0);
    expect(ctrl.getSnapshot().outgoingSlot).toBeNull();
    expect(h.slotMedias[0].playCalls).toBeGreaterThan(0);

    // 3. Upgrade stream to "escritorio" in standby slot 1 while slot 0 remains visible
    ctrl.connectStream("escritorio", { isCameraOrModeSwitch: true });
    expect(h.webSockets.length).toBe(2);
    expect(ctrl.getSnapshot().activeSlot).toBe(0);
    expect(ctrl.getSnapshot().mseReady).toBe(true);

    const ws1 = h.webSockets[1];
    ws1.onopen?.();
    ws1.onmessage?.({
      data: JSON.stringify({
        type: "mse",
        value: 'video/mp4; codecs="avc1.640029"',
      }),
    });
    ws1.onmessage?.({ data: new Uint8Array([99, 100]).buffer });

    // Slot 1 promoted to active; slot 0 enters 80ms outgoing crossfade window before teardown
    expect(ctrl.getSnapshot().activeSlot).toBe(1);
    expect(ctrl.getSnapshot().outgoingSlot).toBe(0);
    expect(ws0.closed).toBe(false);

    // 4. Fire 80ms crossfade timer -> outgoingSlot cleared & slot 0 WebSocket/ObjectURL torn down
    expect(h.runTimerByMs(80)).toBe(true);
    expect(ctrl.getSnapshot().outgoingSlot).toBeNull();
    expect(ws0.closed).toBe(true);
    expect(h.revokedUrls).toContain("blob:nvr-slot-0");
  });

  test("handles QuotaExceededError by evicting [0, max(0.5, currentTime - 2)] and retrying the chunk without dropping data", () => {
    const h = createFakeMseHarness();
    const ctrl = createMseDualSlotSessionController({
      adapter: h.adapter,
      jitterSeed: 0,
    });

    ctrl.connectStream("cam_quota");
    const ws = h.webSockets[0];
    ws.onopen?.();
    ws.onmessage?.({
      data: JSON.stringify({
        type: "mse",
        value: 'video/mp4; codecs="avc1.640029"',
      }),
    });

    const sb = h.mediaSources[0].sourceBuffers[0];
    h.slotMedias[0].currentTime = 12;
    h.slotMedias[0].bufferedRange = { start: 0, end: 12 };

    // Simulate QuotaExceededError on next appendBuffer call
    sb.failNextAppendWithQuota = true;
    const gopPayload = new Uint8Array([7, 8, 9, 10]).buffer;
    ws.onmessage?.({ data: gopPayload });

    // Buffer [0, 10] (12 - 2) was evicted and chunk was retried on updateend without data loss
    expect(sb.removedRanges).toEqual([[0, 10]]);
    expect(sb.appended.length).toBe(1);
    expect(Array.from(sb.appended[0])).toEqual([7, 8, 9, 10]);
    expect(ctrl.getSnapshot().streamDisconnected).toBe(false);
  });

  test("evicts 18s sliding window buffer when currentTime > 24", () => {
    const h = createFakeMseHarness();
    const ctrl = createMseDualSlotSessionController({
      adapter: h.adapter,
      jitterSeed: 0,
    });

    ctrl.connectStream("cam_sliding");
    const ws = h.webSockets[0];
    ws.onopen?.();
    ws.onmessage?.({
      data: JSON.stringify({
        type: "mse",
        value: 'video/mp4; codecs="avc1.640029"',
      }),
    });

    // Initial GOP burst
    ws.onmessage?.({ data: new Uint8Array([1]).buffer });

    const sb = h.mediaSources[0].sourceBuffers[0];
    // Advance steady-state playback beyond 24s threshold (to 30s)
    h.slotMedias[0].currentTime = 30;
    h.slotMedias[0].bufferedRange = { start: 0, end: 30.1 };

    ws.onmessage?.({ data: new Uint8Array([2, 3]).buffer });

    // Evicts [0, currentTime - 18] = [0, 12]
    expect(sb.removedRanges).toEqual([[0, 12]]);
  });

  test("schedules exponential backoff retry on watchdog timeout or WebSocket close and recovers immediately on recoverIfStalled", () => {
    const h = createFakeMseHarness();
    const ctrl = createMseDualSlotSessionController({
      adapter: h.adapter,
      jitterSeed: 0,
    });

    ctrl.connectStream("cam_retry");
    expect(h.webSockets.length).toBe(1);

    // 1. Handshake watchdog timeout (8000ms) fires -> marks slot error & schedules 1000ms backoff (attempt 0)
    expect(h.runTimerByMs(8000)).toBe(true);
    expect(ctrl.getSnapshot().streamDisconnected).toBe(true);
    expect(ctrl.getSnapshot().retryCount).toBe(1);

    // Fire 1000ms reconnect timer -> opens 2nd WebSocket
    expect(h.runTimerByMs(1000)).toBe(true);
    expect(h.webSockets.length).toBe(2);

    // 2. Second WebSocket closes unexpectedly -> schedules 2000ms backoff (attempt 1)
    h.webSockets[1].onclose?.();
    expect(ctrl.getSnapshot().retryCount).toBe(2);
    expect(h.runTimerByMs(2000)).toBe(true);
    expect(h.webSockets.length).toBe(3);

    // 3. Third WebSocket closes -> schedules 3500ms backoff (attempt 2), but recoverIfStalled recovers immediately
    h.webSockets[2].onclose?.();
    expect(ctrl.getSnapshot().retryCount).toBe(3);

    ctrl.recoverIfStalled("cam_retry");
    expect(ctrl.getSnapshot().retryCount).toBe(0);
    expect(h.webSockets.length).toBe(4);

    // Cleanup releases all timers and sockets
    ctrl.cleanupAll();
    expect(h.webSockets[3].closed).toBe(true);
  });

  test("cleanupAll resets DualSlotMachine and controller state so a subsequent connectStream(sameUrl) opens a fresh WebSocket (React 19 StrictMode remount)", () => {
    const h = createFakeMseHarness();
    const snapshots: boolean[] = [];
    const ctrl = createMseDualSlotSessionController({
      adapter: h.adapter,
      jitterSeed: 0,
      onStateChange: (snap) => {
        snapshots.push(snap.mseReady);
      },
    });

    // 1. Connect stream and decode first frame in slot 0
    ctrl.connectStream("escritorio_sub", { isCameraOrModeSwitch: true });
    expect(h.webSockets.length).toBe(1);
    const ws0 = h.webSockets[0];
    ws0.onopen?.();
    ws0.onmessage?.({
      data: JSON.stringify({
        type: "mse",
        value: 'video/mp4; codecs="avc1.640029"',
      }),
    });
    ws0.onmessage?.({ data: new Uint8Array([1, 2, 3]).buffer });
    expect(ctrl.getSnapshot().mseReady).toBe(true);
    expect(ctrl.getSnapshot().machineState.slots[0].status).toBe("ready");

    // 2. Simulate unmount cleanup (cleanupAll) -> resets machine & controller state
    ctrl.cleanupAll();
    expect(ws0.closed).toBe(true);
    const afterCleanup = ctrl.getSnapshot();
    expect(afterCleanup.mseReady).toBe(false);
    expect(afterCleanup.streamDisconnected).toBe(false);
    expect(afterCleanup.outgoingSlot).toBeNull();
    expect(afterCleanup.activeSlot).toBe(0);
    expect(afterCleanup.machineState.activeSlot).toBe(0);
    expect(afterCleanup.machineState.standbySlot).toBeNull();
    expect(afterCleanup.machineState.slots[0].status).toBe("idle");
    expect(afterCleanup.machineState.slots[1].status).toBe("idle");
    expect(snapshots[snapshots.length - 1]).toBe(false);

    // 3. Remount with the exact same URL -> must NOT noop; opens a new WebSocket and reconnects cleanly
    ctrl.connectStream("escritorio_sub", { isCameraOrModeSwitch: true });
    expect(h.webSockets.length).toBe(2);
    const ws1 = h.webSockets[1];
    expect(ws1.closed).toBe(false);
    ws1.onopen?.();
    ws1.onmessage?.({
      data: JSON.stringify({
        type: "mse",
        value: 'video/mp4; codecs="avc1.640029"',
      }),
    });
    ws1.onmessage?.({ data: new Uint8Array([4, 5, 6]).buffer });
    expect(ctrl.getSnapshot().mseReady).toBe(true);
    expect(ctrl.getSnapshot().activeSlot).toBe(0);
  });

  test("forceReset and recoverIfStalled clean up both active and in-flight standby slot sockets, ObjectURLs, and watchdogs before starting the new session", () => {
    const h = createFakeMseHarness();
    const ctrl = createMseDualSlotSessionController({
      adapter: h.adapter,
      jitterSeed: 0,
    });

    // 1. Slot 0 connects and becomes ready with "cam1_sub"
    ctrl.connectStream("cam1_sub", { isCameraOrModeSwitch: true });
    const ws0 = h.webSockets[0];
    ws0.onopen?.();
    ws0.onmessage?.({
      data: JSON.stringify({
        type: "mse",
        value: 'video/mp4; codecs="avc1.640029"',
      }),
    });
    ws0.onmessage?.({ data: new Uint8Array([1, 2]).buffer });
    expect(ctrl.getSnapshot().mseReady).toBe(true);
    expect(ctrl.getSnapshot().activeSlot).toBe(0);

    // 2. Start standby upgrade to "cam1" in slot 1 (in flight, not yet promoted)
    ctrl.connectStream("cam1", { isCameraOrModeSwitch: true });
    expect(h.webSockets.length).toBe(2);
    const ws1Standby = h.webSockets[1];
    expect(ctrl.getSnapshot().machineState.standbySlot).toBe(1);
    expect(ws0.closed).toBe(false);
    expect(ws1Standby.closed).toBe(false);

    // 3. Calling connectStream(..., { forceReset: true }) while standby slot 1 is in flight must close BOTH ws0 and ws1Standby
    ctrl.connectStream("cam1", { forceReset: true });
    expect(ws0.closed).toBe(true);
    expect(ws1Standby.closed).toBe(true);
    expect(h.revokedUrls).toContain("blob:nvr-slot-0");
    expect(h.revokedUrls).toContain("blob:nvr-slot-1");
    expect(h.webSockets.length).toBe(3);

    // 4. Promote ws2 in slot 0, start another standby upgrade in slot 1, then call recoverIfStalled("cam2")
    const ws2 = h.webSockets[2];
    ws2.onopen?.();
    ws2.onmessage?.({
      data: JSON.stringify({
        type: "mse",
        value: 'video/mp4; codecs="avc1.640029"',
      }),
    });
    ws2.onmessage?.({ data: new Uint8Array([9, 10]).buffer });
    expect(ctrl.getSnapshot().mseReady).toBe(true);

    ctrl.connectStream("cam1_main", { isCameraOrModeSwitch: true });
    expect(h.webSockets.length).toBe(4);
    const ws3Standby = h.webSockets[3];
    expect(ws2.closed).toBe(false);
    expect(ws3Standby.closed).toBe(false);

    // recoverIfStalled with a stream that needs reconnect tears down both ws2 and in-flight ws3Standby
    ctrl.recoverIfStalled("cam2");
    expect(ws2.closed).toBe(true);
    expect(ws3Standby.closed).toBe(true);
    expect(h.webSockets.length).toBe(5);
    expect(h.webSockets[4].url).toBe("ws://nvr.local/live/mse/api/ws?src=cam2");
  });
});
