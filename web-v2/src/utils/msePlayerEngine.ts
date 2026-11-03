export type VideoSlotIndex = 0 | 1;
export type VideoSlotStatus = "idle" | "connecting" | "ready" | "error";

export interface VideoSlotState {
  streamSrc: string | null;
  status: VideoSlotStatus;
  sessionId: number;
}

export interface DualSlotSnapshot {
  activeSlot: VideoSlotIndex;
  standbySlot: VideoSlotIndex | null;
  isAnyReady: boolean;
  slots: [VideoSlotState, VideoSlotState];
}

export interface SlotRequestTransition {
  action: "noop" | "start_active" | "start_standby";
  targetSlot: VideoSlotIndex;
  abortSlot: VideoSlotIndex | null;
  sessionId: number;
}

export interface SlotReadyTransition {
  promoted: boolean;
  activeSlot: VideoSlotIndex;
  teardownSlot: VideoSlotIndex | null;
}

/**
 * Deterministic state machine for double-buffered (ping-pong) `<video>` MSE transitions.
 * Keeps the active `<video>` slot visible and playing while a stream upgrade (`{cam}_sub` -> `{cam}`)
 * or camera switch connects and decodes its first keyframe in the hidden standby slot.
 */
export class DualSlotMachine {
  private activeSlot: VideoSlotIndex = 0;
  private standbySlot: VideoSlotIndex | null = null;
  private nextSessionId = 1;
  private slots: [VideoSlotState, VideoSlotState] = [
    { streamSrc: null, status: "idle", sessionId: 0 },
    { streamSrc: null, status: "idle", sessionId: 0 },
  ];

  public getState(): DualSlotSnapshot {
    return {
      activeSlot: this.activeSlot,
      standbySlot: this.standbySlot,
      isAnyReady: this.slots[this.activeSlot].status === "ready",
      slots: [{ ...this.slots[0] }, { ...this.slots[1] }],
    };
  }

  public requestStream(streamSrc: string): SlotRequestTransition {
    const trimmed = streamSrc.trim();
    const activeState = this.slots[this.activeSlot];

    // If the active slot already has this stream ready
    if (activeState.status === "ready" && activeState.streamSrc === trimmed) {
      if (this.standbySlot !== null) {
        const aborted = this.standbySlot;
        this.slots[aborted] = {
          streamSrc: null,
          status: "idle",
          sessionId: this.slots[aborted].sessionId,
        };
        this.standbySlot = null;
        return {
          action: "noop",
          targetSlot: this.activeSlot,
          abortSlot: aborted,
          sessionId: activeState.sessionId,
        };
      }
      return {
        action: "noop",
        targetSlot: this.activeSlot,
        abortSlot: null,
        sessionId: activeState.sessionId,
      };
    }

    // If the active slot is not yet ready ("idle", "connecting", or "error"), load directly in activeSlot
    if (activeState.status !== "ready") {
      if (activeState.status === "connecting" && activeState.streamSrc === trimmed) {
        return {
          action: "noop",
          targetSlot: this.activeSlot,
          abortSlot: null,
          sessionId: activeState.sessionId,
        };
      }
      const hadInFlight = activeState.status === "connecting";
      const sessionId = this.nextSessionId++;
      this.slots[this.activeSlot] = {
        streamSrc: trimmed,
        status: "connecting",
        sessionId,
      };
      return {
        action: "start_active",
        targetSlot: this.activeSlot,
        abortSlot: hadInFlight ? this.activeSlot : null,
        sessionId,
      };
    }

    // Active slot is "ready" with a live frame -> use the other slot as standby
    const otherSlot: VideoSlotIndex = this.activeSlot === 0 ? 1 : 0;
    const otherState = this.slots[otherSlot];
    if (
      this.standbySlot === otherSlot &&
      otherState.status === "connecting" &&
      otherState.streamSrc === trimmed
    ) {
      return {
        action: "noop",
        targetSlot: otherSlot,
        abortSlot: null,
        sessionId: otherState.sessionId,
      };
    }

    const abortSlot =
      this.standbySlot === otherSlot && otherState.status !== "idle" ? otherSlot : null;
    const sessionId = this.nextSessionId++;
    this.standbySlot = otherSlot;
    this.slots[otherSlot] = {
      streamSrc: trimmed,
      status: "connecting",
      sessionId,
    };

    return {
      action: "start_standby",
      targetSlot: otherSlot,
      abortSlot,
      sessionId,
    };
  }

  public markSlotReady(slot: VideoSlotIndex, sessionId?: number): SlotReadyTransition {
    const slotState = this.slots[slot];
    if (sessionId !== undefined && slotState.sessionId !== sessionId) {
      return {
        promoted: false,
        activeSlot: this.activeSlot,
        teardownSlot: null,
      };
    }

    this.slots[slot] = {
      ...slotState,
      status: "ready",
    };

    if (this.standbySlot === slot) {
      const prevActive = this.activeSlot;
      this.activeSlot = slot;
      this.standbySlot = null;
      this.slots[prevActive] = {
        streamSrc: null,
        status: "idle",
        sessionId: this.slots[prevActive].sessionId,
      };
      return {
        promoted: true,
        activeSlot: this.activeSlot,
        teardownSlot: prevActive,
      };
    }

    return {
      promoted: slot === this.activeSlot,
      activeSlot: this.activeSlot,
      teardownSlot: null,
    };
  }

  public markSlotError(slot: VideoSlotIndex, sessionId?: number): DualSlotSnapshot {
    const slotState = this.slots[slot];
    if (sessionId !== undefined && slotState.sessionId !== sessionId) {
      return this.getState();
    }
    this.slots[slot] = {
      ...slotState,
      status: "error",
    };
    if (this.standbySlot === slot) {
      this.standbySlot = null;
    }
    return this.getState();
  }
}

export interface CapturedFrameEntry {
  camera: string;
  dataUrl: string;
  width: number;
  height: number;
  capturedAtMs: number;
}

/**
 * In-memory LRU cache holding GPU-decoded `<video>` frames captured right before
 * layout transitions or tile clicks so newly mounted stages start at native resolution
 * with 0ms backward time jump.
 */
export class LiveFrameBridge {
  private readonly maxEntries: number;
  private readonly ttlMs: number;
  private readonly entries = new Map<string, CapturedFrameEntry>();

  constructor(maxEntries = 12, ttlMs = 15_000) {
    this.maxEntries = Math.max(1, maxEntries);
    this.ttlMs = Math.max(100, ttlMs);
  }

  public store(
    camera: string,
    payload: {
      dataUrl: string;
      width: number;
      height: number;
      capturedAtMs?: number;
    }
  ): CapturedFrameEntry | null {
    const key = camera.trim();
    if (!key || !payload.dataUrl) {
      return null;
    }
    const entry: CapturedFrameEntry = {
      camera: key,
      dataUrl: payload.dataUrl,
      width: payload.width,
      height: payload.height,
      capturedAtMs: payload.capturedAtMs ?? Date.now(),
    };

    if (this.entries.has(key)) {
      this.entries.delete(key);
    }
    this.entries.set(key, entry);

    while (this.entries.size > this.maxEntries) {
      const oldestKey = this.entries.keys().next().value;
      if (oldestKey === undefined) break;
      this.entries.delete(oldestKey);
    }

    return entry;
  }

  public getFresh(camera: string, nowMs = Date.now()): CapturedFrameEntry | null {
    const key = camera.trim();
    const entry = this.entries.get(key);
    if (!entry) {
      return null;
    }
    if (nowMs - entry.capturedAtMs > this.ttlMs) {
      this.entries.delete(key);
      return null;
    }
    // Refresh LRU position
    this.entries.delete(key);
    this.entries.set(key, entry);
    return entry;
  }

  public size(): number {
    return this.entries.size;
  }

  public clear(): void {
    this.entries.clear();
  }
}

export const defaultLiveFrameBridge = new LiveFrameBridge(12, 15_000);

export function captureVideoElementFrame(
  camera: string,
  videoEl: HTMLVideoElement | null,
  nowMs = Date.now(),
  bridge: LiveFrameBridge = defaultLiveFrameBridge
): string | null {
  if (
    !videoEl ||
    typeof document === "undefined" ||
    videoEl.videoWidth <= 0 ||
    videoEl.videoHeight <= 0 ||
    videoEl.readyState < 2
  ) {
    return null;
  }
  try {
    const canvas = document.createElement("canvas");
    canvas.width = videoEl.videoWidth;
    canvas.height = videoEl.videoHeight;
    const ctx = canvas.getContext("2d");
    if (!ctx) return null;
    ctx.drawImage(videoEl, 0, 0, canvas.width, canvas.height);
    const dataUrl = canvas.toDataURL("image/jpeg", 0.88);
    if (!dataUrl || !dataUrl.startsWith("data:image/")) {
      return null;
    }
    bridge.store(camera, {
      dataUrl,
      width: canvas.width,
      height: canvas.height,
      capturedAtMs: nowMs,
    });
    return dataUrl;
  } catch {
    return null;
  }
}

export function getFreshCapturedFrame(
  camera: string,
  nowMs = Date.now(),
  bridge: LiveFrameBridge = defaultLiveFrameBridge
): CapturedFrameEntry | null {
  return bridge.getFresh(camera, nowMs);
}

export function resolveLivePosterUrl(
  realCamera: string,
  useMainStream: boolean,
  posterTick: number,
  nowMs = Date.now(),
  bridge: LiveFrameBridge = defaultLiveFrameBridge
): string {
  const captured = bridge.getFresh(realCamera, nowMs);
  if (captured) {
    return captured.dataUrl;
  }
  const encodedCam = encodeURIComponent(realCamera);
  if (useMainStream) {
    return `/api/${encodedCam}/latest.jpg?cache=${posterTick}`;
  }
  return `/api/${encodedCam}/latest.jpg?height=360&cache=${posterTick}`;
}

/**
 * Deduplicates and cooldown-throttles (`10s` per stream) `POST /api/go2rtc/streams/{stream}/prewarm`
 * requests triggered when an operator hovers or selects a camera tile.
 */
export class StreamPrewarmCoordinator {
  private readonly lastWarmedAt = new Map<string, number>();
  private readonly defaultCooldownMs: number;

  constructor(defaultCooldownMs = 10_000) {
    this.defaultCooldownMs = Math.max(0, defaultCooldownMs);
  }

  public shouldPrewarmStream(
    streamSrc: string,
    nowMs = Date.now(),
    cooldownMs = this.defaultCooldownMs
  ): boolean {
    const key = streamSrc.trim();
    if (!key) return false;
    const prev = this.lastWarmedAt.get(key);
    if (prev !== undefined && nowMs - prev < cooldownMs) {
      return false;
    }
    this.lastWarmedAt.set(key, nowMs);
    return true;
  }

  public prewarmStream(
    streamSrc: string,
    nowMs = Date.now(),
    cooldownMs = this.defaultCooldownMs
  ): boolean {
    if (!this.shouldPrewarmStream(streamSrc, nowMs, cooldownMs)) {
      return false;
    }
    if (typeof fetch === "function") {
      void fetch(`/api/go2rtc/streams/${encodeURIComponent(streamSrc.trim())}/prewarm`, {
        method: "POST",
        credentials: "include",
      }).catch(() => {});
    }
    return true;
  }

  public clear(): void {
    this.lastWarmedAt.clear();
  }
}

export const defaultPrewarmCoordinator = new StreamPrewarmCoordinator(10_000);

export function prewarmStream(streamSrc: string, nowMs = Date.now()): boolean {
  return defaultPrewarmCoordinator.prewarmStream(streamSrc, nowMs);
}

/**
 * Computes the optimal `videoEl.currentTime` seek target on initial GOP burst
 * (`bufferedEnd - 0.08`) or steady-state drift recovery (`bufferedEnd - currentTime > 1.8`).
 */
export function computeLiveEdgeSeekTarget(
  bufferedStart: number,
  bufferedEnd: number,
  currentTime: number,
  isInitialBurst: boolean
): number | null {
  if (!Number.isFinite(bufferedEnd) || !Number.isFinite(bufferedStart) || bufferedEnd <= bufferedStart) {
    return null;
  }
  if (isInitialBurst) {
    if (bufferedEnd > 0.25) {
      return Math.max(bufferedStart, Number((bufferedEnd - 0.08).toFixed(3)));
    }
    return null;
  }
  if (bufferedEnd - currentTime > 1.8) {
    return Math.max(bufferedStart, Number((bufferedEnd - 0.15).toFixed(3)));
  }
  return null;
}
