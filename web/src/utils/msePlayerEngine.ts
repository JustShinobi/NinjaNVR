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

  public reset(): void {
    this.activeSlot = 0;
    this.standbySlot = null;
    this.slots = [
      { streamSrc: null, status: "idle", sessionId: 0 },
      { streamSrc: null, status: "idle", sessionId: 0 },
    ];
  }

  public requestStream(streamSrc: string): SlotRequestTransition {
    const trimmed = streamSrc.trim();
    const activeState = this.slots[this.activeSlot];
    if (!trimmed) {
      return {
        action: "noop",
        targetSlot: this.activeSlot,
        abortSlot: null,
        sessionId: activeState.sessionId,
      };
    }

    // If the active slot already has this stream ready
    if (activeState.status === "ready" && activeState.streamSrc === trimmed) {
      if (this.standbySlot !== null) {
        const aborted = this.standbySlot;
        this.slots[aborted] = {
          streamSrc: null,
          status: "idle",
          sessionId: 0,
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
      if (this.standbySlot !== null) {
        const sbSlot = this.standbySlot;
        const sbState = this.slots[sbSlot];
        if (sbState.status === "connecting" && sbState.streamSrc === trimmed) {
          return {
            action: "noop",
            targetSlot: sbSlot,
            abortSlot: null,
            sessionId: sbState.sessionId,
          };
        }
        this.slots[sbSlot] = {
          streamSrc: null,
          status: "idle",
          sessionId: 0,
        };
        this.standbySlot = null;
        const sessionId = this.nextSessionId++;
        this.slots[this.activeSlot] = {
          streamSrc: trimmed,
          status: "connecting",
          sessionId,
        };
        return {
          action: "start_active",
          targetSlot: this.activeSlot,
          abortSlot: sbState.status !== "idle" ? sbSlot : activeState.status === "connecting" ? this.activeSlot : null,
          sessionId,
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
    if (
      (sessionId !== undefined && slotState.sessionId !== sessionId) ||
      (slotState.status !== "connecting" && slotState.status !== "ready")
    ) {
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
        sessionId: 0,
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
    if (
      slotState.status === "idle" ||
      (sessionId !== undefined && slotState.sessionId !== sessionId)
    ) {
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

  /**
   * Returns `true` when neither the active slot nor the standby slot is currently
   * `"ready"` or `"connecting"` for `desiredStreamSrc`. Used by `LiveMsePlayer` to
   * determine whether an automatic reconnect or stream-upgrade retry is required.
   */
  public needsReconnect(desiredStreamSrc: string): boolean {
    const trimmed = desiredStreamSrc.trim();
    if (!trimmed) return false;
    const activeState = this.slots[this.activeSlot];
    if (
      (activeState.status === "ready" || activeState.status === "connecting") &&
      activeState.streamSrc === trimmed
    ) {
      return false;
    }
    if (this.standbySlot !== null) {
      const standbyState = this.slots[this.standbySlot];
      if (
        (standbyState.status === "ready" || standbyState.status === "connecting") &&
        standbyState.streamSrc === trimmed
      ) {
        return false;
      }
    }
    return true;
  }
}

export const MSE_RECONNECT_BASE_DELAYS_MS = [1_000, 2_000, 3_500, 5_000] as const;
export const MSE_HANDSHAKE_TIMEOUT_MS = 8_000;
export const MSE_STALL_TIMEOUT_MS = 8_000;
export const MSE_OUTGOING_CROSSFADE_MS = 80;
export const MSE_SLIDING_BUFFER_THRESHOLD_SEC = 24;
export const MSE_SLIDING_BUFFER_WINDOW_SEC = 18;
export const MSE_DEFAULT_CODEC_CANDIDATES = [
  'video/mp4; codecs="avc1.640029,mp4a.40.2"',
  'video/mp4; codecs="avc1.64002A,mp4a.40.2"',
  'video/mp4; codecs="avc1.640028,mp4a.40.2"',
  'video/mp4; codecs="avc1.4D401F,mp4a.40.2"',
  'video/mp4; codecs="avc1.4D0029,mp4a.40.2"',
  'video/mp4; codecs="avc1.42E01E,mp4a.40.2"',
  'video/mp4; codecs="avc1.640029"',
] as const;

/**
 * Deterministic camera-name hash in `[0, 300]` ms used to stagger reconnect attempts
 * across multi-camera video-wall tiles so they do not thunder-herd simultaneously.
 */
export function computeCameraJitterSeed(camera: string): number {
  const trimmed = camera.trim();
  let hash = 0;
  for (let i = 0; i < trimmed.length; i++) {
    hash = (hash * 31 + trimmed.charCodeAt(i)) | 0;
  }
  return Math.abs(hash) % 301;
}

/**
 * Computes capped exponential backoff (`1s -> 2s -> 3.5s -> 5s max`) plus a bounded
 * camera-staggered jitter (`0..300ms`) for automatic MSE WebSocket recovery.
 */
export function computeMseReconnectDelayMs(attempt: number, jitterSeed = 0): number {
  const safeAttempt = Number.isFinite(attempt) ? Math.max(0, Math.floor(attempt)) : 0;
  const idx = Math.min(MSE_RECONNECT_BASE_DELAYS_MS.length - 1, safeAttempt);
  const base = MSE_RECONNECT_BASE_DELAYS_MS[idx];
  const safeJitter = Number.isFinite(jitterSeed)
    ? Math.abs(Math.floor(jitterSeed)) % 301
    : 0;
  return base + safeJitter;
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
    if (
      !key ||
      !payload.dataUrl ||
      !payload.dataUrl.startsWith("data:image/") ||
      /[\u0000-\u0020\u007f]/.test(payload.dataUrl) ||
      !Number.isFinite(payload.width) ||
      !Number.isFinite(payload.height) ||
      payload.width <= 0 ||
      payload.height <= 0
    ) {
      return null;
    }
    const safeCapturedAt =
      typeof payload.capturedAtMs === "number" && Number.isFinite(payload.capturedAtMs)
        ? payload.capturedAtMs
        : Date.now();
    const entry: CapturedFrameEntry = {
      camera: key,
      dataUrl: payload.dataUrl,
      width: Math.round(payload.width),
      height: Math.round(payload.height),
      capturedAtMs: safeCapturedAt,
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
    if (
      !Number.isFinite(nowMs) ||
      !Number.isFinite(entry.capturedAtMs) ||
      nowMs - entry.capturedAtMs > this.ttlMs
    ) {
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
    if (this.lastWarmedAt.has(key)) {
      this.lastWarmedAt.delete(key);
    }
    this.lastWarmedAt.set(key, nowMs);
    while (this.lastWarmedAt.size > 256) {
      const oldest = this.lastWarmedAt.keys().next().value;
      if (oldest === undefined) break;
      this.lastWarmedAt.delete(oldest);
    }
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
    if (!Number.isFinite(currentTime) || bufferedEnd > 0.25 || currentTime < bufferedStart) {
      return Math.max(bufferedStart, Number((bufferedEnd - 0.08).toFixed(3)));
    }
    return null;
  }
  if (
    !Number.isFinite(currentTime) ||
    bufferedEnd - currentTime > 1.8 ||
    currentTime < bufferedStart - 0.5 ||
    currentTime > bufferedEnd + 0.5
  ) {
    return Math.max(bufferedStart, Number((bufferedEnd - 0.15).toFixed(3)));
  }
  return null;
}

/**
 * Coalesces a batch of queued fMP4 binary chunks (`init_segment` + `moof+mdat` fragments)
 * into a single contiguous `ArrayBuffer` so an entire GOP burst can be appended to
 * `SourceBuffer` in a single `appendBuffer` call instead of dozens of async `updateend` cycles.
 */
export function coalesceArrayBuffers(chunks: ArrayBuffer[]): ArrayBuffer | null {
  if (chunks.length === 0) return null;
  if (chunks.length === 1) return chunks[0];
  let totalBytes = 0;
  for (let i = 0; i < chunks.length; i++) {
    totalBytes += chunks[i].byteLength;
  }
  const merged = new Uint8Array(totalBytes);
  let offset = 0;
  for (let i = 0; i < chunks.length; i++) {
    merged.set(new Uint8Array(chunks[i]), offset);
    offset += chunks[i].byteLength;
  }
  return merged.buffer;
}

export interface MseBufferedRange {
  start: number;
  end: number;
}

export interface MseSourceBufferLike {
  mode?: "segments" | "sequence";
  readonly updating: boolean;
  appendBuffer(data: ArrayBuffer): void;
  remove(start: number, end: number): void;
  abort?(): void;
  addEventListener(type: "updateend" | "error", listener: () => void): void;
  removeEventListener(type: "updateend" | "error", listener: () => void): void;
}

export interface MseMediaSourceLike {
  readonly readyState: "closed" | "open" | "ended";
  addSourceBuffer(mimeCodec: string): MseSourceBufferLike;
  removeSourceBuffer?(sourceBuffer: MseSourceBufferLike): void;
  addEventListener(type: "sourceopen", listener: () => void): void;
  removeEventListener(type: "sourceopen", listener: () => void): void;
}

export interface MseWebSocketLike {
  binaryType: BinaryType;
  onopen: (() => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onerror: (() => void) | null;
  onclose: (() => void) | null;
  send(data: string | ArrayBuffer): void;
  close(): void;
}

export interface MseSlotMediaHandle {
  currentTime: number;
  readonly readyState: number;
  readonly paused: boolean;
  readonly seeking?: boolean;
  src: string;
  disableRemotePlayback?: boolean;
  getBufferedRange(): MseBufferedRange | null;
  play(): Promise<void> | void;
  pause(): void;
  resetSrc(): void;
  addEventListener(type: string, listener: () => void): void;
  removeEventListener(type: string, listener: () => void): void;
}

export interface MsePlatformAdapter {
  getSlotMedia(slot: VideoSlotIndex): MseSlotMediaHandle | null;
  createMediaSource(): MseMediaSourceLike | null;
  isTypeSupported?(mimeCodec: string): boolean;
  createObjectURL(mediaSource: MseMediaSourceLike): string;
  revokeObjectURL(url: string): void;
  createWebSocket(url: string): MseWebSocketLike;
  setTimeout(cb: () => void, ms: number): number;
  clearTimeout(id: number): void;
  resolveWebSocketUrl?(streamOrWsUrl: string): string;
  captureSlotFrame?(slot: VideoSlotIndex): void;
  onOverlayGeometryUpdate?(): void;
}

export interface MseDualSlotControllerSnapshot {
  activeSlot: VideoSlotIndex;
  outgoingSlot: VideoSlotIndex | null;
  mseReady: boolean;
  streamDisconnected: boolean;
  retryCount: number;
  currentStreamUrl: string;
  machineState: DualSlotSnapshot;
}

export type MseDualSlotStateListener = (
  snapshot: MseDualSlotControllerSnapshot
) => void;

export interface MseDualSlotSessionControllerOptions {
  adapter: MsePlatformAdapter;
  getCameraName?: () => string;
  jitterSeed?: number;
  handshakeTimeoutMs?: number;
  stallTimeoutMs?: number;
  crossfadeTeardownMs?: number;
  onStateChange?: MseDualSlotStateListener;
}

export function resolveMseWebSocketUrl(streamOrWsUrl: string): string {
  const trimmed = streamOrWsUrl.trim();
  if (!trimmed) return "";
  if (trimmed.startsWith("ws://") || trimmed.startsWith("wss://")) {
    return trimmed;
  }
  const proto =
    typeof window !== "undefined" && window.location?.protocol === "https:"
      ? "wss:"
      : "ws:";
  const host =
    typeof window !== "undefined" && window.location?.host
      ? window.location.host
      : "localhost";
  if (trimmed.startsWith("/")) {
    return `${proto}//${host}${trimmed}`;
  }
  return `${proto}//${host}/live/mse/api/ws?src=${encodeURIComponent(trimmed)}`;
}

export function extractMseMimeCodecFromMessage(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) return null;
  if (trimmed.startsWith("video/") || trimmed.startsWith("audio/")) {
    return trimmed;
  }
  if (!trimmed.startsWith("{")) {
    return null;
  }
  try {
    const parsed = JSON.parse(trimmed) as {
      type?: unknown;
      value?: unknown;
      mime?: unknown;
      codec?: unknown;
      codecs?: unknown;
    } | null;
    if (!parsed || typeof parsed !== "object") return null;
    if (parsed.type === "mse" && typeof parsed.value === "string" && parsed.value.trim()) {
      return parsed.value.trim();
    }
    const candidate =
      typeof parsed.mime === "string"
        ? parsed.mime
        : typeof parsed.codecs === "string"
        ? parsed.codecs
        : typeof parsed.codec === "string"
        ? parsed.codec
        : typeof parsed.value === "string"
        ? parsed.value
        : "";
    const clean = candidate.trim();
    if (!clean) return null;
    if (clean.includes("/")) return clean;
    return `video/mp4; codecs="${clean}"`;
  } catch {
    return null;
  }
}

export function createDomSlotMediaHandle(
  videoEl: HTMLVideoElement
): MseSlotMediaHandle {
  return {
    get currentTime() {
      return videoEl.currentTime;
    },
    set currentTime(val: number) {
      videoEl.currentTime = val;
    },
    get readyState() {
      return videoEl.readyState;
    },
    get paused() {
      return videoEl.paused;
    },
    get seeking() {
      return videoEl.seeking;
    },
    get src() {
      return videoEl.src;
    },
    set src(val: string) {
      videoEl.src = val;
    },
    get disableRemotePlayback() {
      return videoEl.disableRemotePlayback;
    },
    set disableRemotePlayback(val: boolean | undefined) {
      if (val !== undefined) {
        try {
          videoEl.disableRemotePlayback = val;
        } catch {
          // ignore if readonly in synthetic DOM
        }
      }
    },
    getBufferedRange(): MseBufferedRange | null {
      try {
        if (videoEl.buffered && videoEl.buffered.length > 0) {
          return {
            start: videoEl.buffered.start(0),
            end: videoEl.buffered.end(videoEl.buffered.length - 1),
          };
        }
      } catch {
        // ignore buffered range read errors
      }
      return null;
    },
    play() {
      try {
        const p = videoEl.play();
        if (p && typeof p.catch === "function") {
          return p.catch(() => {});
        }
        return p;
      } catch {
        return undefined;
      }
    },
    pause() {
      try {
        videoEl.pause();
      } catch {
        // ignore
      }
    },
    resetSrc() {
      try {
        videoEl.pause();
        videoEl.removeAttribute("src");
        videoEl.load();
      } catch {
        // ignore
      }
    },
    addEventListener(type: string, listener: () => void) {
      videoEl.addEventListener(type, listener);
    },
    removeEventListener(type: string, listener: () => void) {
      videoEl.removeEventListener(type, listener);
    },
  };
}

export function createDomMsePlatformAdapter(params: {
  getVideoElement: (slot: VideoSlotIndex) => HTMLVideoElement | null;
  getCameraName?: () => string;
  onOverlayGeometryUpdate?: () => void;
  frameBridge?: LiveFrameBridge;
}): MsePlatformAdapter {
  const resolveMediaSourceCtor = (): typeof MediaSource | undefined => {
    if (typeof window === "undefined") return undefined;
    return (
      window.MediaSource ||
      (
        window as unknown as {
          ManagedMediaSource?: typeof MediaSource;
        }
      ).ManagedMediaSource
    );
  };

  return {
    getSlotMedia(slot: VideoSlotIndex): MseSlotMediaHandle | null {
      const el = params.getVideoElement(slot);
      return el ? createDomSlotMediaHandle(el) : null;
    },
    createMediaSource(): MseMediaSourceLike | null {
      const Ctor = resolveMediaSourceCtor();
      if (!Ctor) return null;
      return new Ctor() as unknown as MseMediaSourceLike;
    },
    isTypeSupported(mimeCodec: string): boolean {
      const Ctor = resolveMediaSourceCtor();
      if (Ctor && typeof Ctor.isTypeSupported === "function") {
        return Ctor.isTypeSupported(mimeCodec);
      }
      return true;
    },
    createObjectURL(mediaSource: MseMediaSourceLike): string {
      return URL.createObjectURL(mediaSource as unknown as MediaSource);
    },
    revokeObjectURL(url: string): void {
      try {
        URL.revokeObjectURL(url);
      } catch {
        // ignore
      }
    },
    createWebSocket(url: string): MseWebSocketLike {
      const rawWs = new WebSocket(url);
      const wrapper: MseWebSocketLike = {
        get binaryType() {
          return rawWs.binaryType;
        },
        set binaryType(val: BinaryType) {
          rawWs.binaryType = val;
        },
        onopen: null,
        onmessage: null,
        onerror: null,
        onclose: null,
        send(data: string | ArrayBuffer) {
          rawWs.send(data);
        },
        close() {
          rawWs.close();
        },
      };
      rawWs.onopen = () => {
        wrapper.onopen?.();
      };
      rawWs.onmessage = (ev: MessageEvent) => {
        wrapper.onmessage?.({ data: ev.data });
      };
      rawWs.onerror = () => {
        wrapper.onerror?.();
      };
      rawWs.onclose = () => {
        wrapper.onclose?.();
      };
      return wrapper;
    },
    setTimeout(cb: () => void, ms: number): number {
      return typeof window !== "undefined"
        ? window.setTimeout(cb, ms)
        : (globalThis.setTimeout(cb, ms) as unknown as number);
    },
    clearTimeout(id: number): void {
      if (typeof window !== "undefined") {
        window.clearTimeout(id);
      } else {
        globalThis.clearTimeout(id);
      }
    },
    resolveWebSocketUrl(streamOrWsUrl: string): string {
      return resolveMseWebSocketUrl(streamOrWsUrl);
    },
    captureSlotFrame(slot: VideoSlotIndex): void {
      const cam = params.getCameraName?.() || "";
      if (!cam) return;
      captureVideoElementFrame(
        cam,
        params.getVideoElement(slot),
        Date.now(),
        params.frameBridge ?? defaultLiveFrameBridge
      );
    },
    onOverlayGeometryUpdate: params.onOverlayGeometryUpdate,
  };
}

/**
 * Deep dual-slot MSE session orchestrator.
 * Owns `DualSlotMachine`, double-buffered `MediaSource` + `SourceBuffer` lifecycles,
 * binary fMP4 chunk queueing & coalescing, `QuotaExceededError` eviction-and-retry,
 * 18s sliding buffer eviction (`currentTime - 18` when `currentTime > 24`),
 * 80ms outgoing-slot crossfade teardown, handshake/stall watchdogs, and exponential backoff.
 */
export class MseDualSlotSessionController {
  private readonly adapter: MsePlatformAdapter;
  private readonly getCameraName?: () => string;
  private readonly explicitJitterSeed?: number;
  private readonly handshakeTimeoutMs: number;
  private readonly stallTimeoutMs: number;
  private readonly crossfadeTeardownMs: number;
  private readonly slotMachine = new DualSlotMachine();
  private readonly listeners = new Set<MseDualSlotStateListener>();

  private activeSlot: VideoSlotIndex = 0;
  private outgoingSlot: VideoSlotIndex | null = null;
  private mseReady = false;
  private streamDisconnected = false;
  private retryCount = 0;
  private currentStreamUrl = "";
  private cleanedUp = false;

  private slotCleanups: [(() => void) | null, (() => void) | null] = [
    null,
    null,
  ];
  private slotTeardownTimers: [number | null, number | null] = [null, null];
  private reconnectTimer: number | null = null;

  constructor(options: MseDualSlotSessionControllerOptions) {
    this.adapter = options.adapter;
    this.getCameraName = options.getCameraName;
    this.explicitJitterSeed = options.jitterSeed;
    this.handshakeTimeoutMs =
      options.handshakeTimeoutMs ?? MSE_HANDSHAKE_TIMEOUT_MS;
    this.stallTimeoutMs = options.stallTimeoutMs ?? MSE_STALL_TIMEOUT_MS;
    this.crossfadeTeardownMs =
      options.crossfadeTeardownMs ?? MSE_OUTGOING_CROSSFADE_MS;
    if (options.onStateChange) {
      this.listeners.add(options.onStateChange);
    }
  }

  public getSnapshot(): MseDualSlotControllerSnapshot {
    return {
      activeSlot: this.activeSlot,
      outgoingSlot: this.outgoingSlot,
      mseReady: this.mseReady,
      streamDisconnected: this.streamDisconnected,
      retryCount: this.retryCount,
      currentStreamUrl: this.currentStreamUrl,
      machineState: this.slotMachine.getState(),
    };
  }

  public getMachineState(): DualSlotSnapshot {
    return this.slotMachine.getState();
  }

  public subscribe(listener: MseDualSlotStateListener): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  public needsReconnect(streamOrWsUrl?: string): boolean {
    const target = (streamOrWsUrl ?? this.currentStreamUrl).trim();
    return this.slotMachine.needsReconnect(target);
  }

  private emitState(): void {
    const snap = this.getSnapshot();
    for (const listener of this.listeners) {
      try {
        listener(snap);
      } catch {
        // ignore listener error
      }
    }
  }

  private notify(): void {
    this.emitState();
  }

  private resolveJitterSeed(): number {
    if (
      typeof this.explicitJitterSeed === "number" &&
      Number.isFinite(this.explicitJitterSeed)
    ) {
      return Math.abs(Math.floor(this.explicitJitterSeed)) % 301;
    }
    const cam = this.getCameraName?.() || "";
    return cam ? computeCameraJitterSeed(cam) : 0;
  }

  public clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      this.adapter.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  public clearSlotTeardownTimer(slot: VideoSlotIndex): void {
    const t = this.slotTeardownTimers[slot];
    if (t !== null) {
      this.adapter.clearTimeout(t);
      this.slotTeardownTimers[slot] = null;
    }
  }

  public scheduleReconnect(immediate = false): void {
    if (this.cleanedUp) return;
    const jitterMs = this.resolveJitterSeed();
    if (immediate) {
      this.clearReconnectTimer();
      this.retryCount = 0;
      this.notify();
      this.reconnectTimer = this.adapter.setTimeout(() => {
        this.reconnectTimer = null;
        if (this.cleanedUp) return;
        const target = this.currentStreamUrl;
        if (target && this.slotMachine.needsReconnect(target)) {
          this.connectStream(target, { isCameraOrModeSwitch: false });
        }
      }, jitterMs);
      return;
    }
    if (this.reconnectTimer !== null) {
      return;
    }
    const delayMs = computeMseReconnectDelayMs(this.retryCount, jitterMs);
    this.retryCount += 1;
    this.notify();
    this.reconnectTimer = this.adapter.setTimeout(() => {
      this.reconnectTimer = null;
      if (this.cleanedUp) return;
      const target = this.currentStreamUrl;
      if (target && this.slotMachine.needsReconnect(target)) {
        this.connectStream(target, { isCameraOrModeSwitch: false });
      }
    }, delayMs);
  }

  public startSlotSession(
    slot: VideoSlotIndex,
    streamOrWsUrl: string,
    sessionId?: number
  ): void {
    this.cleanedUp = false;
    const resolvedSessionId =
      sessionId ?? this.slotMachine.getState().slots[slot].sessionId;
    const slotMedia = this.adapter.getSlotMedia(slot);
    const mediaSource = this.adapter.createMediaSource();

    if (!slotMedia || !mediaSource) {
      const snap = this.slotMachine.markSlotError(slot, resolvedSessionId);
      if (!snap.isAnyReady) {
        this.mseReady = false;
        this.streamDisconnected = true;
      }
      this.notify();
      return;
    }

    this.clearSlotTeardownTimer(slot);
    if (this.outgoingSlot === slot) {
      this.outgoingSlot = null;
      this.notify();
    }

    if (slot === this.slotMachine.getState().activeSlot) {
      this.adapter.captureSlotFrame?.(slot);
    }

    this.slotCleanups[slot]?.();
    this.slotCleanups[slot] = null;

    let closed = false;
    let failedOnce = false;
    let quotaRecoveries = 0;
    let lastEvictedEnd = 0;
    let ws: MseWebSocketLike | null = null;
    let sourceBuffer: MseSourceBufferLike | null = null;
    let watchdogTimer: number | null = null;
    const queue: ArrayBuffer[] = [];

    const wsUrl = this.adapter.resolveWebSocketUrl
      ? this.adapter.resolveWebSocketUrl(streamOrWsUrl)
      : resolveMseWebSocketUrl(streamOrWsUrl);

    slotMedia.disableRemotePlayback = true;
    const objUrl = this.adapter.createObjectURL(mediaSource);

    let initialBurstSeekDone = false;
    let promotedOnce = false;
    let sourceOpened = false;

    const clearWatchdog = () => {
      if (watchdogTimer !== null) {
        this.adapter.clearTimeout(watchdogTimer);
        watchdogTimer = null;
      }
    };

    const handleSlotFailure = () => {
      if (closed || failedOnce) return;
      failedOnce = true;
      clearWatchdog();
      if (slot === this.slotMachine.getState().activeSlot) {
        this.adapter.captureSlotFrame?.(slot);
      }
      const snap = this.slotMachine.markSlotError(slot, resolvedSessionId);
      if (!snap.isAnyReady) {
        this.mseReady = false;
        this.streamDisconnected = true;
      }
      this.slotCleanups[slot]?.();
      this.slotCleanups[slot] = null;
      this.notify();
      if (
        this.currentStreamUrl &&
        this.slotMachine.needsReconnect(this.currentStreamUrl)
      ) {
        this.scheduleReconnect(false);
      }
    };

    const resetWatchdog = (timeoutMs: number) => {
      clearWatchdog();
      if (closed) return;
      watchdogTimer = this.adapter.setTimeout(() => {
        watchdogTimer = null;
        handleSlotFailure();
      }, timeoutMs);
    };

    resetWatchdog(this.handshakeTimeoutMs);

    const flushQueue = () => {
      if (
        closed ||
        !sourceBuffer ||
        sourceBuffer.updating ||
        queue.length === 0 ||
        mediaSource.readyState !== "open"
      ) {
        return;
      }
      const batch = queue.splice(0, queue.length);
      const merged = coalesceArrayBuffers(batch);
      if (!merged) return;
      try {
        sourceBuffer.appendBuffer(merged);
        quotaRecoveries = 0;
      } catch (err) {
        const errName = (err as { name?: string } | null)?.name;
        const errMsg = (err as { message?: string } | null)?.message || "";
        const isQuota =
          errName === "QuotaExceededError" ||
          errMsg.includes("QuotaExceededError");
        if (
          isQuota &&
          quotaRecoveries < 2 &&
          !closed &&
          mediaSource.readyState === "open" &&
          !sourceBuffer.updating
        ) {
          try {
            const buffered = slotMedia.getBufferedRange();
            const curTime = slotMedia.currentTime;
            const hasCurTime = Number.isFinite(curTime) && curTime > 0;
            const canEvict =
              hasCurTime ||
              (buffered !== null && buffered.end > buffered.start + 1);
            if (canEvict) {
              quotaRecoveries += 1;
              if (queue.length < 256) {
                queue.unshift(merged);
              }
              const evictStart =
                !hasCurTime && buffered ? buffered.start : 0;
              const refTime = hasCurTime
                ? curTime
                : buffered
                ? buffered.end
                : 0;
              const evictEnd = Math.max(evictStart + 0.5, refTime - 2);
              sourceBuffer.remove(evictStart, evictEnd);
              return;
            }
          } catch {
            // fall through to failure recovery
          }
        }
        handleSlotFailure();
      }
    };

    const tryPromoteSlot = () => {
      if (closed || promotedOnce) return;
      const machineSnap = this.slotMachine.getState();
      const isStandbyUpgrade =
        machineSnap.isAnyReady && machineSnap.standbySlot === slot;
      if (isStandbyUpgrade && (slotMedia.seeking || slotMedia.readyState < 2)) {
        return;
      }
      promotedOnce = true;
      const res = this.slotMachine.markSlotReady(slot, resolvedSessionId);
      if (res.promoted) {
        this.retryCount = 0;
        if (
          this.currentStreamUrl &&
          !this.slotMachine.needsReconnect(this.currentStreamUrl)
        ) {
          this.clearReconnectTimer();
        }
        resetWatchdog(this.stallTimeoutMs);
        this.activeSlot = res.activeSlot;
        this.mseReady = true;
        this.streamDisconnected = false;
        this.adapter.onOverlayGeometryUpdate?.();
        void slotMedia.play();
        if (res.teardownSlot !== null) {
          const oldSlot = res.teardownSlot;
          const cleanupToRun = this.slotCleanups[oldSlot];
          this.clearSlotTeardownTimer(oldSlot);
          this.outgoingSlot = oldSlot;
          this.notify();
          this.slotTeardownTimers[oldSlot] = this.adapter.setTimeout(() => {
            this.slotTeardownTimers[oldSlot] = null;
            if (this.outgoingSlot === oldSlot) {
              this.outgoingSlot = null;
            }
            if (cleanupToRun && this.slotCleanups[oldSlot] === cleanupToRun) {
              cleanupToRun();
              this.slotCleanups[oldSlot] = null;
            }
            this.notify();
          }, this.crossfadeTeardownMs);
        } else {
          this.notify();
        }
      }
    };

    const onUpdateEnd = () => {
      if (closed || !sourceBuffer) return;
      try {
        const buffered = slotMedia.getBufferedRange();
        if (buffered) {
          const seekTarget = computeLiveEdgeSeekTarget(
            buffered.start,
            buffered.end,
            slotMedia.currentTime,
            !initialBurstSeekDone
          );
          if (seekTarget !== null) {
            slotMedia.currentTime = seekTarget;
            initialBurstSeekDone = true;
          }
        }
        if (slotMedia.paused) {
          void slotMedia.play();
        }
        if (slotMedia.readyState >= 2) {
          tryPromoteSlot();
        }
        const curTime = slotMedia.currentTime;
        if (
          Number.isFinite(curTime) &&
          curTime > MSE_SLIDING_BUFFER_THRESHOLD_SEC &&
          !sourceBuffer.updating &&
          mediaSource.readyState === "open"
        ) {
          const evictEnd = curTime - MSE_SLIDING_BUFFER_WINDOW_SEC;
          const removeStart =
            buffered && buffered.start > 0 ? buffered.start : 0;
          if (evictEnd > removeStart && evictEnd > lastEvictedEnd + 0.5) {
            lastEvictedEnd = evictEnd;
            try {
              sourceBuffer.remove(removeStart, evictEnd);
              return;
            } catch {
              // ignore buffer trim race
            }
          }
        } else if (
          buffered &&
          buffered.end - buffered.start > MSE_SLIDING_BUFFER_WINDOW_SEC &&
          !sourceBuffer.updating &&
          mediaSource.readyState === "open"
        ) {
          const evictEnd = buffered.end - 10;
          if (evictEnd > buffered.start && evictEnd > lastEvictedEnd + 0.5) {
            lastEvictedEnd = evictEnd;
            try {
              sourceBuffer.remove(buffered.start, evictEnd);
              return;
            } catch {
              // ignore buffer trim race
            }
          }
        }
      } catch {
        // ignore buffered range error
      }
      flushQueue();
    };

    const onSourceOpen = () => {
      if (closed || sourceOpened) return;
      sourceOpened = true;
      ws = this.adapter.createWebSocket(wsUrl);
      ws.binaryType = "arraybuffer";

      ws.onopen = () => {
        if (closed) return;
        resetWatchdog(this.handshakeTimeoutMs);
        const supportedCheck = (mime: string) =>
          this.adapter.isTypeSupported
            ? this.adapter.isTypeSupported(mime)
            : true;
        const codecs = MSE_DEFAULT_CODEC_CANDIDATES.filter((c) =>
          supportedCheck(c)
        )
          .map((c) => c.replace('video/mp4; codecs="', "").replace('"', ""))
          .join(",");

        ws?.send(
          JSON.stringify({
            type: "mse",
            value:
              codecs ||
              "avc1.640029,avc1.640028,avc1.4D401F,avc1.4D0029,avc1.42E01E,mp4a.40.2",
          })
        );
      };

      ws.onerror = () => {
        handleSlotFailure();
      };

      ws.onclose = () => {
        handleSlotFailure();
      };

      ws.onmessage = (ev) => {
        if (closed) return;
        if (typeof ev.data === "string") {
          const mimeCodec = extractMseMimeCodecFromMessage(ev.data);
          if (mimeCodec && !sourceBuffer && mediaSource.readyState === "open") {
            try {
              sourceBuffer = mediaSource.addSourceBuffer(mimeCodec);
              sourceBuffer.mode = "segments";
              sourceBuffer.addEventListener("updateend", onUpdateEnd);
              sourceBuffer.addEventListener("error", handleSlotFailure);
              flushQueue();
            } catch {
              handleSlotFailure();
            }
          }
        } else if (ev.data instanceof ArrayBuffer) {
          if (promotedOnce) {
            resetWatchdog(this.stallTimeoutMs);
          }
          if (queue.length < 256) {
            queue.push(ev.data);
          }
          flushQueue();
        }
      };
    };

    mediaSource.addEventListener("sourceopen", onSourceOpen);
    slotMedia.src = objUrl;
    if (mediaSource.readyState === "open" && !sourceOpened) {
      onSourceOpen();
    }

    const onLoadedData = () => {
      if (closed) return;
      const buffered = slotMedia.getBufferedRange();
      if (buffered && !initialBurstSeekDone) {
        const seekTarget = computeLiveEdgeSeekTarget(
          buffered.start,
          buffered.end,
          slotMedia.currentTime,
          true
        );
        if (seekTarget !== null) {
          slotMedia.currentTime = seekTarget;
          initialBurstSeekDone = true;
        }
      }
      void slotMedia.play();
      tryPromoteSlot();
    };

    const onCanPlayOrSeeked = () => {
      if (closed) return;
      tryPromoteSlot();
    };

    const onVideoError = () => {
      handleSlotFailure();
    };

    const onVideoResize = () => {
      this.adapter.onOverlayGeometryUpdate?.();
    };

    slotMedia.addEventListener("loadeddata", onLoadedData);
    slotMedia.addEventListener("canplay", onCanPlayOrSeeked);
    slotMedia.addEventListener("seeked", onCanPlayOrSeeked);
    slotMedia.addEventListener("playing", onCanPlayOrSeeked);
    slotMedia.addEventListener("error", onVideoError);
    slotMedia.addEventListener("resize", onVideoResize);

    this.slotCleanups[slot] = () => {
      closed = true;
      clearWatchdog();
      this.clearSlotTeardownTimer(slot);
      queue.length = 0;
      slotMedia.removeEventListener("loadeddata", onLoadedData);
      slotMedia.removeEventListener("canplay", onCanPlayOrSeeked);
      slotMedia.removeEventListener("seeked", onCanPlayOrSeeked);
      slotMedia.removeEventListener("playing", onCanPlayOrSeeked);
      slotMedia.removeEventListener("error", onVideoError);
      slotMedia.removeEventListener("resize", onVideoResize);
      try {
        mediaSource.removeEventListener("sourceopen", onSourceOpen);
      } catch {
        // ignore
      }
      try {
        sourceBuffer?.removeEventListener("updateend", onUpdateEnd);
        sourceBuffer?.removeEventListener("error", handleSlotFailure);
        if (sourceBuffer && mediaSource.readyState === "open") {
          sourceBuffer.abort?.();
          mediaSource.removeSourceBuffer?.(sourceBuffer);
        }
      } catch {
        // ignore
      }
      if (ws) {
        ws.onopen = null;
        ws.onmessage = null;
        ws.onerror = null;
        ws.onclose = null;
        try {
          ws.close();
        } catch {
          // ignore
        }
        ws = null;
      }
      slotMedia.resetSrc();
      this.adapter.revokeObjectURL(objUrl);
    };
  }

  public connectStream(
    wsUrl: string,
    options?: { forceReset?: boolean; isCameraOrModeSwitch?: boolean }
  ): void {
    this.cleanedUp = false;
    const trimmed = wsUrl.trim();
    if (!trimmed) return;
    this.currentStreamUrl = trimmed;

    if (options?.forceReset) {
      this.retryCount = 0;
      this.clearReconnectTimer();
      this.adapter.captureSlotFrame?.(this.activeSlot);
      this.slotCleanups[0]?.();
      this.slotCleanups[0] = null;
      this.slotCleanups[1]?.();
      this.slotCleanups[1] = null;
      this.clearSlotTeardownTimer(0);
      this.clearSlotTeardownTimer(1);
      this.outgoingSlot = null;
      const snap = this.slotMachine.getState();
      this.slotMachine.markSlotError(0, snap.slots[0].sessionId);
      this.slotMachine.markSlotError(1, snap.slots[1].sessionId);
    } else if (options?.isCameraOrModeSwitch) {
      this.retryCount = 0;
      this.clearReconnectTimer();
    }

    const transition = this.slotMachine.requestStream(trimmed);
    if (transition.abortSlot !== null) {
      this.slotCleanups[transition.abortSlot]?.();
      this.slotCleanups[transition.abortSlot] = null;
    }
    if (transition.action === "noop") {
      return;
    }
    if (
      transition.action === "start_active" &&
      (options?.isCameraOrModeSwitch || options?.forceReset)
    ) {
      this.mseReady = false;
      this.streamDisconnected = false;
      this.notify();
    }
    this.startSlotSession(transition.targetSlot, trimmed, transition.sessionId);
  }

  public recoverIfStalled(wsUrl?: string): void {
    const targetUrl = (wsUrl ?? this.currentStreamUrl).trim();
    if (!targetUrl) return;
    this.currentStreamUrl = targetUrl;

    if (
      !this.mseReady ||
      this.streamDisconnected ||
      this.slotMachine.needsReconnect(targetUrl)
    ) {
      this.adapter.captureSlotFrame?.(this.activeSlot);
      this.slotCleanups[0]?.();
      this.slotCleanups[0] = null;
      this.slotCleanups[1]?.();
      this.slotCleanups[1] = null;
      this.clearSlotTeardownTimer(0);
      this.clearSlotTeardownTimer(1);
      this.outgoingSlot = null;
      const snap = this.slotMachine.getState();
      this.slotMachine.markSlotError(0, snap.slots[0].sessionId);
      this.slotMachine.markSlotError(1, snap.slots[1].sessionId);
      this.clearReconnectTimer();
      this.retryCount = 0;
      this.connectStream(targetUrl, { isCameraOrModeSwitch: false });
      return;
    }

    const activeMedia = this.adapter.getSlotMedia(this.activeSlot);
    if (activeMedia) {
      try {
        const buffered = activeMedia.getBufferedRange();
        if (buffered) {
          const seekTarget = computeLiveEdgeSeekTarget(
            buffered.start,
            buffered.end,
            activeMedia.currentTime,
            false
          );
          if (seekTarget !== null) {
            activeMedia.currentTime = seekTarget;
          }
        }
      } catch {
        // ignore
      }
      void activeMedia.play();
    }
  }

  public cleanupAll(): void {
    this.cleanedUp = true;
    this.clearReconnectTimer();
    this.clearSlotTeardownTimer(0);
    this.clearSlotTeardownTimer(1);
    this.adapter.captureSlotFrame?.(this.activeSlot);
    this.slotCleanups[0]?.();
    this.slotCleanups[1]?.();
    this.slotCleanups = [null, null];
    this.slotMachine.reset();
    this.mseReady = false;
    this.streamDisconnected = false;
    this.outgoingSlot = null;
    this.activeSlot = 0;
    this.emitState();
  }
}

export function createMseDualSlotSessionController(
  options: MseDualSlotSessionControllerOptions
): MseDualSlotSessionController {
  return new MseDualSlotSessionController(options);
}
