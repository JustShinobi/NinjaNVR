import React, { useCallback, useEffect, useRef, useState } from "react";
import Hls from "hls.js";
import { Camera, ChevronLeft, ChevronRight, Mic, MicOff } from "lucide-react";
import { useI18n } from "../i18n";
import { CameraConfig } from "../types/nvr";
import { resolveRealCameraSource, useNvrStore } from "../store/useNvrStore";
import { formatRelTime, formatTs, resolveEffectiveTheme } from "../utils/format";

export { resolveRealCameraSource };
import { resolveCanvasThemePalette } from "../utils/themeTokens";
import {
  captureVideoElementFrame,
  coalesceArrayBuffers,
  computeCameraJitterSeed,
  computeLiveEdgeSeekTarget,
  computeMseReconnectDelayMs,
  DualSlotMachine,
  MSE_HANDSHAKE_TIMEOUT_MS,
  MSE_STALL_TIMEOUT_MS,
  prewarmStream,
  resolveLivePosterUrl,
  VideoSlotIndex,
} from "../utils/msePlayerEngine";

export interface PushToTalkTrackLike {
  stop: () => void;
  kind?: string;
  enabled?: boolean;
}

export interface PushToTalkStreamLike {
  getTracks: () => PushToTalkTrackLike[];
}

export interface PushToTalkPeerConnectionLike {
  addTrack?: (track: PushToTalkTrackLike, stream: PushToTalkStreamLike) => unknown;
  createOffer?: () => Promise<{ type?: string; sdp?: string }>;
  setLocalDescription?: (desc: { type?: string; sdp?: string }) => Promise<void>;
  setRemoteDescription?: (desc: { type: "answer"; sdp: string }) => Promise<void>;
  localDescription?: { type?: string; sdp?: string } | null;
  close: () => void;
}

export interface PushToTalkDependencies {
  mode?: "webrtc" | "cgi_alaw" | null;
  getUserMedia?: (constraints: { audio: boolean }) => Promise<PushToTalkStreamLike>;
  createPeerConnection?: () => PushToTalkPeerConnectionLike;
  fetchImpl?: (
    url: string,
    init: {
      method: string;
      headers: Record<string, string>;
      credentials?: RequestCredentials;
      body: string;
    }
  ) => Promise<{ ok: boolean; status: number; text: () => Promise<string> }>;
}

export interface ActivePushToTalkSession {
  camera: string;
  stream: PushToTalkStreamLike;
  pc: PushToTalkPeerConnectionLike;
  answerSdp: string;
  stop: () => void;
}

export type PushToTalkState = "idle" | "connecting" | "talking" | "error";

export interface PushToTalkController {
  getState: () => PushToTalkState;
  getError: () => string | null;
  press: () => Promise<boolean>;
  release: () => void;
  unmount: () => void;
}

/**
 * Deterministically stops every microphone MediaStreamTrack and closes the
 * auxiliary WebRTC RTCPeerConnection so macOS/iOS/Android immediately extinguish
 * the system microphone-in-use indicator without touching the MSE video stream.
 */
export function closePushToTalkSession(
  stream: PushToTalkStreamLike | null | undefined,
  pc: PushToTalkPeerConnectionLike | null | undefined
): void {
  if (stream && typeof stream.getTracks === "function") {
    stream.getTracks().forEach((track) => {
      try {
        track.stop();
      } catch {
        // Ensure every track's .stop() is attempted even if one throws
      }
    });
  }
  if (pc && typeof pc.close === "function") {
    try {
      pc.close();
    } catch {
      // ignore close errors on already-closed peer connection
    }
  }
}

/**
 * Opens a lightweight auxiliary WebRTC backchannel session to `/api/go2rtc/webrtc?src=<camera>`
 * carrying only the local microphone audio track while leaving `<LiveMsePlayer>` uninterrupted.
 * Automatically releases all microphone tracks and closes the peer connection on any signaling failure.
 */
export async function startPushToTalkSession(
  camera: string,
  deps?: PushToTalkDependencies
): Promise<ActivePushToTalkSession> {
  const getUserMedia =
    deps?.getUserMedia ??
    ((constraints: { audio: boolean }) => {
      if (
        typeof navigator === "undefined" ||
        !navigator.mediaDevices ||
        typeof navigator.mediaDevices.getUserMedia !== "function"
      ) {
        return Promise.reject(
          new Error("Microphone capture (getUserMedia) is unavailable in this browser context")
        );
      }
      return navigator.mediaDevices.getUserMedia(constraints);
    });

  const createPeerConnection: () => PushToTalkPeerConnectionLike =
    deps?.createPeerConnection ??
    (() => {
      if (typeof RTCPeerConnection === "undefined") {
        throw new Error("RTCPeerConnection is unavailable in this browser context");
      }
      const nativePc = new RTCPeerConnection();
      return {
        get localDescription() {
          return nativePc.localDescription;
        },
        addTrack(track, mediaStream) {
          return nativePc.addTrack(
            track as MediaStreamTrack,
            mediaStream as MediaStream
          );
        },
        createOffer() {
          return nativePc.createOffer();
        },
        setLocalDescription(desc) {
          return nativePc.setLocalDescription(desc as RTCLocalSessionDescriptionInit);
        },
        setRemoteDescription(desc) {
          return nativePc.setRemoteDescription(desc);
        },
        close() {
          nativePc.close();
        },
      };
    });

  const fetchImpl =
    deps?.fetchImpl ??
    ((url, init) => fetch(url, init as RequestInit));

  const stream = await getUserMedia({ audio: true });
  let pc: PushToTalkPeerConnectionLike | null = null;

  if (deps?.mode === "cgi_alaw") {
    const encodeBytesToBase64 = (bytes: Uint8Array): string => {
      let bin = "";
      for (let i = 0; i < bytes.length; i++) {
        bin += String.fromCharCode(bytes[i]);
      }
      if (typeof btoa === "function") {
        return btoa(bin);
      }
      return bin;
    };

    try {
      const silenceProbeB64 = encodeBytesToBase64(new Uint8Array(320));
      const resp = await fetchImpl(
        `/api/${encodeURIComponent(camera)}/audio/talkback`,
        {
          method: "POST",
          headers: { "Content-Type": "audio/pcm;base64" },
          credentials: "include",
          body: silenceProbeB64,
        }
      );
      if (!resp.ok) {
        throw new Error(`HTTP audio talkback failed (${resp.status})`);
      }

      // Attach live WebAudio microphone capture when running in a browser context
      let audioCleanup: (() => void) | null = null;
      const AudioCtxCtor =
        typeof window !== "undefined"
          ? window.AudioContext ||
            (window as unknown as { webkitAudioContext?: typeof AudioContext })
              .webkitAudioContext
          : undefined;
      if (AudioCtxCtor) {
        try {
          const ctx = new AudioCtxCtor({ sampleRate: 8000 });
          const src = ctx.createMediaStreamSource(
            stream as unknown as MediaStream
          );
          const proc = ctx.createScriptProcessor(2048, 1, 1);
          let stopped = false;
          let inFlight = false;
          const pendingChunks: Uint8Array[] = [];

          const flushPendingAudio = async () => {
            if (inFlight || stopped || pendingChunks.length === 0) return;
            inFlight = true;
            try {
              while (!stopped && pendingChunks.length > 0) {
                const batch = pendingChunks.splice(0, pendingChunks.length);
                let totalLen = 0;
                for (const c of batch) totalLen += c.length;
                const merged = new Uint8Array(totalLen);
                let offset = 0;
                for (const c of batch) {
                  merged.set(c, offset);
                  offset += c.length;
                }
                const payloadB64 = encodeBytesToBase64(merged);
                await fetchImpl(
                  `/api/${encodeURIComponent(camera)}/audio/talkback`,
                  {
                    method: "POST",
                    headers: { "Content-Type": "audio/pcm;base64" },
                    credentials: "include",
                    body: payloadB64,
                  }
                ).catch(() => {});
              }
            } finally {
              inFlight = false;
            }
          };

          proc.onaudioprocess = (ev) => {
            if (stopped) return;
            const ch = ev.inputBuffer.getChannelData(0);
            if (!ch || ch.length === 0) return;
            const inRate =
              typeof ctx.sampleRate === "number" && ctx.sampleRate > 8000
                ? ctx.sampleRate
                : 8000;
            const step = inRate / 8000;
            const outSamples = Math.max(1, Math.floor(ch.length / step));
            const bytes = new Uint8Array(outSamples * 2);
            for (let i = 0; i < outSamples; i++) {
              const srcIdx = Math.min(ch.length - 1, Math.floor(i * step));
              const s = Math.max(-1, Math.min(1, ch[srcIdx]));
              const v = s < 0 ? Math.round(s * 32768) : Math.round(s * 32767);
              bytes[i * 2] = v & 0xff;
              bytes[i * 2 + 1] = (v >> 8) & 0xff;
            }
            if (pendingChunks.length >= 8) {
              pendingChunks.shift();
            }
            pendingChunks.push(bytes);
            void flushPendingAudio();
          };
          src.connect(proc);
          proc.connect(ctx.destination);
          audioCleanup = () => {
            stopped = true;
            pendingChunks.length = 0;
            try {
              proc.disconnect();
              src.disconnect();
              void ctx.close();
            } catch {
              // ignore
            }
          };
        } catch {
          // AudioContext optional in non-DOM/restricted contexts
        }
      }

      const cgiPc: PushToTalkPeerConnectionLike = {
        close() {
          audioCleanup?.();
        },
      };
      return {
        camera,
        stream,
        pc: cgiPc,
        answerSdp: "cgi_alaw",
        stop: () => closePushToTalkSession(stream, cgiPc),
      };
    } catch (err) {
      closePushToTalkSession(stream, null);
      throw err;
    }
  }

  try {
    const activePc = createPeerConnection();
    pc = activePc;
    for (const track of stream.getTracks()) {
      activePc.addTrack?.(track, stream);
    }

    const offer = (await activePc.createOffer?.()) ?? {
      type: "offer",
      sdp: "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n",
    };
    await activePc.setLocalDescription?.(offer);

    const offerSdp =
      activePc.localDescription?.sdp ||
      offer.sdp ||
      "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n";

    const resp = await fetchImpl(
      `/api/go2rtc/webrtc?src=${encodeURIComponent(camera)}`,
      {
        method: "POST",
        headers: { "Content-Type": "application/sdp" },
        credentials: "include",
        body: offerSdp,
      }
    );

    if (!resp.ok) {
      throw new Error(`WebRTC backchannel signaling failed (${resp.status})`);
    }

    const answerSdp = await resp.text();
    if (!answerSdp || !answerSdp.trim()) {
      throw new Error("Empty SDP answer from go2rtc WebRTC proxy");
    }

    await activePc.setRemoteDescription?.({ type: "answer", sdp: answerSdp });

    return {
      camera,
      stream,
      pc: activePc,
      answerSdp,
      stop: () => closePushToTalkSession(stream, activePc),
    };
  } catch (err) {
    closePushToTalkSession(stream, pc);
    throw err;
  }
}

/**
 * Stateful Push-to-Talk lifecycle controller that guarantees deterministic
 * microphone track release on pointer release, SDP failure, or component unmount.
 */
export function createPushToTalkController(
  camera: string,
  deps?: PushToTalkDependencies,
  onStateChange?: (state: PushToTalkState, error: string | null) => void
): PushToTalkController {
  let state: PushToTalkState = "idle";
  let error: string | null = null;
  let activeStream: PushToTalkStreamLike | null = null;
  let activePc: PushToTalkPeerConnectionLike | null = null;
  let sessionToken = 0;
  let unmounted = false;

  const notify = (nextState: PushToTalkState, nextError: string | null = null) => {
    state = nextState;
    error = nextError;
    if (!unmounted) {
      onStateChange?.(state, error);
    }
  };

  const closedObjects = new WeakSet<object>();
  const closeOnce = (
    streamToClose: PushToTalkStreamLike | null | undefined,
    pcToClose: PushToTalkPeerConnectionLike | null | undefined
  ) => {
    const filteredStream =
      streamToClose && !closedObjects.has(streamToClose) ? streamToClose : null;
    const filteredPc =
      pcToClose && !closedObjects.has(pcToClose) ? pcToClose : null;
    if (filteredStream) closedObjects.add(filteredStream);
    if (filteredPc) closedObjects.add(filteredPc);
    if (filteredStream || filteredPc) {
      closePushToTalkSession(filteredStream, filteredPc);
    }
  };

  const cleanupActive = () => {
    const streamToClose = activeStream;
    const pcToClose = activePc;
    activeStream = null;
    activePc = null;
    closeOnce(streamToClose, pcToClose);
  };

  return {
    getState: () => state,
    getError: () => error,
    press: async () => {
      if (unmounted || state === "connecting" || state === "talking") {
        return false;
      }
      cleanupActive();
      const myToken = ++sessionToken;
      notify("connecting", null);

      try {
        const baseGetUserMedia = deps?.getUserMedia;
        const baseCreatePc = deps?.createPeerConnection;
        const wrappedDeps: PushToTalkDependencies = {
          ...deps,
          ...(baseGetUserMedia
            ? {
                getUserMedia: async (constraints) => {
                  const s = await baseGetUserMedia(constraints);
                  if (unmounted || myToken !== sessionToken) {
                    closeOnce(s, null);
                    throw new Error("Push-to-Talk cancelled during microphone acquisition");
                  }
                  activeStream = s;
                  return s;
                },
              }
            : {
                getUserMedia: async (constraints) => {
                  if (
                    typeof navigator === "undefined" ||
                    !navigator.mediaDevices ||
                    typeof navigator.mediaDevices.getUserMedia !== "function"
                  ) {
                    throw new Error(
                      "Microphone capture (getUserMedia) is unavailable in this browser context"
                    );
                  }
                  const s = await navigator.mediaDevices.getUserMedia(constraints);
                  if (unmounted || myToken !== sessionToken) {
                    closeOnce(s, null);
                    throw new Error("Push-to-Talk cancelled during microphone acquisition");
                  }
                  activeStream = s;
                  return s;
                },
              }),
          ...(baseCreatePc
            ? {
                createPeerConnection: () => {
                  const created = baseCreatePc();
                  if (unmounted || myToken !== sessionToken) {
                    closeOnce(null, created);
                    throw new Error("Push-to-Talk cancelled during peer connection creation");
                  }
                  activePc = created;
                  return created;
                },
              }
            : {
                createPeerConnection: () => {
                  if (typeof RTCPeerConnection === "undefined") {
                    throw new Error("RTCPeerConnection is unavailable in this browser context");
                  }
                  const nativePc = new RTCPeerConnection();
                  const wrappedPc: PushToTalkPeerConnectionLike = {
                    get localDescription() {
                      return nativePc.localDescription;
                    },
                    addTrack(track, mediaStream) {
                      return nativePc.addTrack(
                        track as MediaStreamTrack,
                        mediaStream as MediaStream
                      );
                    },
                    createOffer() {
                      return nativePc.createOffer();
                    },
                    setLocalDescription(desc) {
                      return nativePc.setLocalDescription(
                        desc as RTCLocalSessionDescriptionInit
                      );
                    },
                    setRemoteDescription(desc) {
                      return nativePc.setRemoteDescription(desc);
                    },
                    close() {
                      nativePc.close();
                    },
                  };
                  if (unmounted || myToken !== sessionToken) {
                    closeOnce(null, wrappedPc);
                    throw new Error("Push-to-Talk cancelled during peer connection creation");
                  }
                  activePc = wrappedPc;
                  return wrappedPc;
                },
              }),
        };
        const session = await startPushToTalkSession(camera, wrappedDeps);
        if (unmounted || myToken !== sessionToken) {
          closeOnce(session.stream, session.pc);
          cleanupActive();
          return false;
        }
        activeStream = session.stream;
        activePc = session.pc;
        notify("talking", null);
        return true;
      } catch (err) {
        // startPushToTalkSession already closes its stream and pc on internal failure;
        // mark them in closedObjects so cleanupActive does not double-stop tracks.
        if (activeStream) closedObjects.add(activeStream);
        if (activePc) closedObjects.add(activePc);
        activeStream = null;
        activePc = null;
        if (!unmounted && myToken === sessionToken) {
          const msg = err instanceof Error ? err.message : String(err);
          notify("error", msg);
        }
        return false;
      }
    },
    release: () => {
      sessionToken += 1;
      cleanupActive();
      if (!unmounted) {
        notify("idle", null);
      }
    },
    unmount: () => {
      unmounted = true;
      sessionToken += 1;
      cleanupActive();
      state = "idle";
      error = null;
    },
  };
}

export interface PushToTalkButtonProps {
  camera: string;
  height?: number;
  supported?: boolean;
  mode?: "webrtc" | "cgi_alaw" | null;
  onStateChange?: (state: PushToTalkState, error: string | null) => void;
}

export const PushToTalkButton: React.FC<PushToTalkButtonProps> = ({
  camera,
  height = 20,
  supported = true,
  mode = "webrtc",
  onStateChange,
}) => {
  const [pttState, setPttState] = useState<PushToTalkState>("idle");
  const [pttError, setPttError] = useState<string | null>(null);
  const controllerRef = useRef<PushToTalkController | null>(null);
  const onStateChangeRef = useRef(onStateChange);
  onStateChangeRef.current = onStateChange;

  useEffect(() => {
    if (!supported) {
      if (controllerRef.current) {
        controllerRef.current.unmount();
        controllerRef.current = null;
      }
      setPttState("idle");
      setPttError(null);
      return;
    }
    const ctrl = createPushToTalkController(
      camera,
      { mode: mode || "webrtc" },
      (nextState, nextErr) => {
        setPttState(nextState);
        setPttError(nextErr);
        onStateChangeRef.current?.(nextState, nextErr);
      }
    );
    controllerRef.current = ctrl;
    return () => {
      ctrl.unmount();
      controllerRef.current = null;
    };
  }, [camera, supported, mode]);

  if (!supported) {
    return null;
  }

  const isTalking = pttState === "talking";
  const isConnecting = pttState === "connecting";
  const isError = pttState === "error";

  return (
    <button
      type="button"
      data-testid={`ptt-btn-${camera}`}
      className={`nvr-btn nvr-tile-action mono ${isTalking ? "active" : ""}`}
      aria-label={`Push to talk on ${camera}`}
      aria-pressed={isTalking || isConnecting}
      title={
        isTalking
          ? `Transmitting microphone audio to ${camera} — release to end`
          : isError && pttError
          ? `Push-to-Talk error: ${pttError}`
          : `Hold to talk to ${camera} via WebRTC backchannel (no video reload)`
      }
      style={{
        height,
        padding: "0 6px",
        fontSize: "var(--text-xs)",
        borderColor: isTalking
          ? "var(--signal-alert)"
          : isError
          ? "var(--signal-motion)"
          : undefined,
        color: isTalking
          ? "var(--signal-alert-text)"
          : isError
          ? "var(--signal-motion-text)"
          : undefined,
        background: isTalking ? "rgba(239, 68, 68, 0.24)" : undefined,
      }}
      onClick={(e) => {
        e.stopPropagation();
      }}
      onPointerDown={(e) => {
        e.stopPropagation();
        try {
          e.currentTarget.setPointerCapture(e.pointerId);
        } catch {
          // ignore
        }
        void controllerRef.current?.press();
      }}
      onPointerUp={(e) => {
        e.stopPropagation();
        try {
          if (e.currentTarget.hasPointerCapture?.(e.pointerId)) {
            e.currentTarget.releasePointerCapture(e.pointerId);
          }
        } catch {
          // ignore
        }
        controllerRef.current?.release();
      }}
      onPointerCancel={(e) => {
        e.stopPropagation();
        try {
          if (e.currentTarget.hasPointerCapture?.(e.pointerId)) {
            e.currentTarget.releasePointerCapture(e.pointerId);
          }
        } catch {
          // ignore
        }
        controllerRef.current?.release();
      }}
      onLostPointerCapture={() => {
        if (isTalking || isConnecting) {
          controllerRef.current?.release();
        }
      }}
      onKeyDown={(e) => {
        if ((e.key === " " || e.key === "Enter") && !e.repeat) {
          e.preventDefault();
          e.stopPropagation();
          void controllerRef.current?.press();
        }
      }}
      onKeyUp={(e) => {
        if (e.key === " " || e.key === "Enter") {
          e.preventDefault();
          e.stopPropagation();
          controllerRef.current?.release();
        }
      }}
      onBlur={() => {
        if (isTalking || isConnecting) {
          controllerRef.current?.release();
        }
      }}
    >
      {isError ? <MicOff size={11} /> : <Mic size={11} />}
      {isTalking ? "TX LIVE" : isConnecting ? "TX..." : isError ? "PTT ERR" : "PTT"}
    </button>
  );
};

interface ZoomableVideoStageProps {
  disabled?: boolean;
  forceHudVisible?: boolean;
  extraHud?: React.ReactNode;
  children: React.ReactNode;
}

export const ZoomableVideoStage: React.FC<ZoomableVideoStageProps> = ({
  disabled = false,
  forceHudVisible = false,
  extraHud,
  children,
}) => {
  const stageRef = useRef<HTMLDivElement | null>(null);
  const [scale, setScale] = useState<number>(1.0);
  const [pan, setPan] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  const [isDragging, setIsDragging] = useState<boolean>(false);
  const [isHovered, setIsHovered] = useState<boolean>(false);
  const dragOriginRef = useRef<{
    startX: number;
    startY: number;
    originX: number;
    originY: number;
  } | null>(null);

  const clampPan = useCallback(
    (nextScale: number, rawX: number, rawY: number) => {
      if (!Number.isFinite(nextScale) || nextScale <= 1.0) return { x: 0, y: 0 };
      const safeX = Number.isFinite(rawX) ? rawX : 0;
      const safeY = Number.isFinite(rawY) ? rawY : 0;
      const el = stageRef.current;
      const w = el?.clientWidth || 640;
      const h = el?.clientHeight || 360;
      const maxX = ((nextScale - 1) * w) / 2;
      const maxY = ((nextScale - 1) * h) / 2;
      return {
        x: Math.max(-maxX, Math.min(maxX, safeX)),
        y: Math.max(-maxY, Math.min(maxY, safeY)),
      };
    },
    []
  );

  const applyZoom = useCallback(
    (nextRawScale: number, anchorX?: number, anchorY?: number) => {
      const safeRaw = Number.isFinite(nextRawScale) ? nextRawScale : 1.0;
      const nextScale = Math.round(Math.max(1.0, Math.min(5.0, safeRaw)) * 100) / 100;
      setScale((prevScale) => {
        if (nextScale <= 1.0) {
          setPan({ x: 0, y: 0 });
          return 1.0;
        }
        setPan((prevPan) => {
          const el = stageRef.current;
          if (el && anchorX !== undefined && anchorY !== undefined && prevScale > 0) {
            const rect = el.getBoundingClientRect();
            const cx = anchorX - (rect.left + rect.width / 2);
            const cy = anchorY - (rect.top + rect.height / 2);
            const ratio = nextScale / prevScale;
            const targetX = cx - (cx - prevPan.x) * ratio;
            const targetY = cy - (cy - prevPan.y) * ratio;
            return clampPan(nextScale, targetX, targetY);
          }
          return clampPan(nextScale, prevPan.x, prevPan.y);
        });
        return nextScale;
      });
    },
    [clampPan]
  );

  const resetZoom = useCallback(() => {
    setScale(1.0);
    setPan({ x: 0, y: 0 });
    setIsDragging(false);
    dragOriginRef.current = null;
  }, []);

  useEffect(() => {
    const el = stageRef.current;
    if (!el || disabled) return;
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey && !e.altKey && scale <= 1.0) return;
      e.preventDefault();
      e.stopPropagation();
      const step = e.deltaY < 0 ? 0.25 : -0.25;
      applyZoom(scale + step, e.clientX, e.clientY);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [disabled, scale, applyZoom]);

  return (
    <div
      ref={stageRef}
      style={{
        position: "relative",
        width: "100%",
        height: "100%",
        overflow: "hidden",
        cursor:
          !disabled && scale > 1.0 ? (isDragging ? "grabbing" : "grab") : "inherit",
      }}
      onDoubleClick={(e) => {
        if (disabled) return;
        if (scale > 1.0) {
          e.stopPropagation();
          resetZoom();
        }
      }}
      onPointerDown={(e) => {
        if (disabled || scale <= 1.0) return;
        if ((e.target as HTMLElement).closest("[data-zoom-hud]")) return;
        e.stopPropagation();
        try {
          e.currentTarget.setPointerCapture(e.pointerId);
        } catch {
          // ignore
        }
        setIsDragging(true);
        dragOriginRef.current = {
          startX: e.clientX,
          startY: e.clientY,
          originX: pan.x,
          originY: pan.y,
        };
      }}
      onPointerMove={(e) => {
        if (!isDragging || !dragOriginRef.current) return;
        e.stopPropagation();
        const dx = e.clientX - dragOriginRef.current.startX;
        const dy = e.clientY - dragOriginRef.current.startY;
        setPan(
          clampPan(
            scale,
            dragOriginRef.current.originX + dx,
            dragOriginRef.current.originY + dy
          )
        );
      }}
      onPointerUp={(e) => {
        if (!isDragging) return;
        e.stopPropagation();
        try {
          if (e.currentTarget.hasPointerCapture?.(e.pointerId)) {
            e.currentTarget.releasePointerCapture(e.pointerId);
          }
        } catch {
          // ignore
        }
        setIsDragging(false);
        dragOriginRef.current = null;
      }}
      onPointerCancel={(e) => {
        try {
          if (e.currentTarget.hasPointerCapture?.(e.pointerId)) {
            e.currentTarget.releasePointerCapture(e.pointerId);
          }
        } catch {
          // ignore
        }
        setIsDragging(false);
        dragOriginRef.current = null;
      }}
      onLostPointerCapture={() => {
        setIsDragging(false);
        dragOriginRef.current = null;
      }}
      onPointerEnter={() => setIsHovered(true)}
      onPointerLeave={() => setIsHovered(false)}
    >
      <div
        style={{
          position: "relative",
          width: "100%",
          height: "100%",
          transform: `translate3d(${pan.x}px, ${pan.y}px, 0) scale(${scale})`,
          transformOrigin: "center center",
          transition: isDragging ? "none" : "transform 0.1s ease-out",
        }}
      >
        {children}
      </div>

      {!disabled && (
        <div
          data-zoom-hud="true"
          className="mono"
          onClick={(e) => e.stopPropagation()}
          style={{
            position: "absolute",
            top: 8,
            right: 8,
            zIndex: 14,
            display: "inline-flex",
            alignItems: "center",
            gap: 4,
            background: "rgba(9, 13, 20, 0.88)",
            border: "1px solid var(--border-strong)",
            borderRadius: 3,
            padding: "3px 5px",
            fontSize: "var(--text-xs)",
            fontVariantNumeric: "tabular-nums",
            opacity: isHovered || scale > 1.0 || forceHudVisible ? 1 : 0,
            transition: "opacity 0.16s ease",
            pointerEvents: isHovered || scale > 1.0 || forceHudVisible ? "auto" : "none",
          }}
        >
          {extraHud && (
            <>
              {extraHud}
              <span
                style={{
                  width: 1,
                  height: 14,
                  background: "var(--border-strong)",
                  margin: "0 2px",
                }}
              />
            </>
          )}
          <button
            type="button"
            className="nvr-btn mono"
            style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
            title="Zoom out (-0.25x)"
            disabled={scale <= 1.0}
            onClick={() => applyZoom(scale - 0.25)}
          >
            -
          </button>
          <button
            type="button"
            className="nvr-btn mono"
            style={{
              height: 20,
              padding: "0 6px",
              fontSize: "var(--text-xs)",
              color: scale > 1.0 ? "var(--signal-accent-text)" : "var(--text-secondary)",
            }}
            title="Click to reset digital zoom to 1.0x (Ctrl+Wheel to zoom)"
            onClick={resetZoom}
          >
            {scale.toFixed(2)}x{scale > 1.0 ? " [RESET]" : ""}
          </button>
          <button
            type="button"
            className="nvr-btn mono"
            style={{ height: 20, padding: "0 5px", fontSize: "var(--text-xs)" }}
            title="Zoom in (+0.25x)"
            disabled={scale >= 5.0}
            onClick={() => applyZoom(scale + 0.25)}
          >
            +
          </button>
        </div>
      )}
    </div>
  );
};

interface LiveMsePlayerProps {
  camera: string;
  useMainStream?: boolean;
  cameraConfig?: CameraConfig;
  showVectors?: boolean;
  draftPoints?: Array<[number, number]>;
  onPlotPoint?: (nx: number, ny: number) => void;
  muted?: boolean;
  onClick?: () => void;
  /** Stroke/fill color for the in-progress draft shape (defaults to amber). */
  draftColor?: string;
  /** Extra layer positioned exactly over the contained video picture (0..100% space). */
  stageOverlay?: React.ReactNode;
  pushToTalkSupported?: boolean;
  pushToTalkMode?: "webrtc" | "cgi_alaw" | null;
}

/**
 * Safely extracts an array of polygon coordinate strings from `motion.mask`
 * (which may be a string, string[], or Record<string, { coordinates: string } | string>).
 */
export function extractMaskPolygons(mask: unknown): string[] {
  if (!mask) return [];
  if (typeof mask === "string") return mask.trim() ? [mask.trim()] : [];
  if (Array.isArray(mask)) {
    return mask
      .map((item) => {
        if (typeof item === "string") return item;
        if (item && typeof item === "object" && "coordinates" in item) {
          const c = (item as { coordinates?: unknown }).coordinates;
          return typeof c === "string" ? c : "";
        }
        return "";
      })
      .filter(Boolean);
  }
  if (typeof mask === "object") {
    return Object.values(mask as Record<string, unknown>)
      .map((val) => {
        if (typeof val === "string") return val;
        if (val && typeof val === "object" && "coordinates" in val) {
          const c = (val as { coordinates?: unknown }).coordinates;
          return typeof c === "string" ? c : "";
        }
        return "";
      })
      .filter(Boolean);
  }
  return [];
}

/**
 * Parses a normalized or pixel polygon string ("x1,y1,x2,y2,...") into SVG points ("0..1000" coordinate space).
 */
export function parsePolygonPoints(
  coordStr: string,
  detWidth: number,
  detHeight: number
): string {
  if (typeof coordStr !== "string" || !coordStr.trim()) {
    return "";
  }
  const safeW =
    typeof detWidth === "number" && Number.isFinite(detWidth) && detWidth > 0
      ? detWidth
      : 1280;
  const safeH =
    typeof detHeight === "number" && Number.isFinite(detHeight) && detHeight > 0
      ? detHeight
      : 720;
  const nums = coordStr.split(",").map((s) => parseFloat(s.trim()));
  const pairs: string[] = [];
  for (let i = 0; i + 1 < nums.length; i += 2) {
    let x = nums[i];
    let y = nums[i + 1];
    if (!Number.isFinite(x) || !Number.isFinite(y)) {
      continue;
    }
    if (x > 1.0 || y > 1.0) {
      x = x / safeW;
      y = y / safeH;
    }
    const sx = Math.round(Math.max(0, Math.min(1, x)) * 1000);
    const sy = Math.round(Math.max(0, Math.min(1, y)) * 1000);
    pairs.push(`${sx},${sy}`);
  }
  return pairs.join(" ");
}

/**
 * Converts a polygon coordinate string into normalized [x, y] vertex tuples (0..1) for interactive editing.
 */
export function coordStringToPoints(
  coordStr: string,
  detWidth: number,
  detHeight: number
): Array<[number, number]> {
  if (typeof coordStr !== "string" || !coordStr.trim()) {
    return [];
  }
  const safeW =
    typeof detWidth === "number" && Number.isFinite(detWidth) && detWidth > 0
      ? detWidth
      : 1280;
  const safeH =
    typeof detHeight === "number" && Number.isFinite(detHeight) && detHeight > 0
      ? detHeight
      : 720;
  const nums = coordStr.split(",").map((s) => parseFloat(s.trim()));
  const out: Array<[number, number]> = [];
  for (let i = 0; i + 1 < nums.length; i += 2) {
    let x = nums[i];
    let y = nums[i + 1];
    if (!Number.isFinite(x) || !Number.isFinite(y)) {
      continue;
    }
    if (x > 1.0 || y > 1.0) {
      x = x / safeW;
      y = y / safeH;
    }
    out.push([Math.max(0, Math.min(1, x)), Math.max(0, Math.min(1, y))]);
  }
  return out;
}

export interface ContainedBox {
  left: number;
  top: number;
  width: number;
  height: number;
}

export function computeContainedBox(
  containerW: number,
  containerH: number,
  mediaW: number,
  mediaH: number
): ContainedBox {
  const safeCW =
    typeof containerW === "number" && Number.isFinite(containerW) && containerW > 0
      ? containerW
      : 0;
  const safeCH =
    typeof containerH === "number" && Number.isFinite(containerH) && containerH > 0
      ? containerH
      : 0;
  if (
    safeCW <= 0 ||
    safeCH <= 0 ||
    typeof mediaW !== "number" ||
    typeof mediaH !== "number" ||
    !Number.isFinite(mediaW) ||
    !Number.isFinite(mediaH) ||
    mediaW <= 0 ||
    mediaH <= 0
  ) {
    return { left: 0, top: 0, width: safeCW, height: safeCH };
  }
  const containerRatio = safeCW / safeCH;
  const mediaRatio = mediaW / mediaH;
  if (containerRatio > mediaRatio) {
    const height = safeCH;
    const width = safeCH * mediaRatio;
    return {
      left: (safeCW - width) / 2,
      top: 0,
      width,
      height,
    };
  }
  const width = safeCW;
  const height = safeCW / mediaRatio;
  return {
    left: 0,
    top: (safeCH - height) / 2,
    width,
    height,
  };
}

export function resolveDesiredLiveStreamSrc(
  realCamera: string,
  useMainStream: boolean,
  configuredCameras?: Record<string, CameraConfig> | null,
  go2rtcStreams?: Record<string, unknown> | null
): string {
  const trimmed = realCamera.trim();
  if (!trimmed || useMainStream) {
    return trimmed;
  }
  const subName = `${trimmed}_sub`;
  const camCfg = configuredCameras?.[trimmed];
  const explicitStream1 = camCfg?.live?.streams?.["Stream 1"]?.trim();
  if (explicitStream1 && explicitStream1 === trimmed) {
    return trimmed;
  }
  if (go2rtcStreams && Object.keys(go2rtcStreams).length > 0) {
    const subDef = go2rtcStreams[subName];
    if (subDef === undefined) {
      return go2rtcStreams[trimmed] !== undefined || camCfg !== undefined
        ? trimmed
        : subName;
    }
    const firstUrl = Array.isArray(subDef)
      ? typeof subDef[0] === "string"
        ? subDef[0]
        : ""
      : typeof subDef === "string"
      ? subDef
      : "";
    const cleanUrl = firstUrl.split("#")[0]?.trim() || "";
    if (
      cleanUrl === `rtsp://127.0.0.1:8554/${trimmed}` ||
      cleanUrl === `rtsp://localhost:8554/${trimmed}`
    ) {
      return trimmed;
    }
  }
  const detectInput = camCfg?.ffmpeg?.inputs?.find((inp) =>
    inp.roles?.includes("detect")
  );
  if (detectInput?.path) {
    const cleanDet = detectInput.path.split("#")[0]?.trim() || "";
    if (
      cleanDet === `rtsp://127.0.0.1:8554/${trimmed}` ||
      cleanDet === `rtsp://localhost:8554/${trimmed}`
    ) {
      return trimmed;
    }
  }
  return subName;
}

export const LiveMsePlayer: React.FC<LiveMsePlayerProps> = ({
  camera,
  useMainStream = false,
  cameraConfig,
  showVectors = false,
  draftPoints,
  onPlotPoint,
  muted = true,
  onClick,
  draftColor = "#f59e0b",
  stageOverlay,
  pushToTalkSupported,
  pushToTalkMode = "webrtc",
}) => {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const videoRef0 = useRef<HTMLVideoElement | null>(null);
  const videoRef1 = useRef<HTMLVideoElement | null>(null);
  const imgRef = useRef<HTMLImageElement | null>(null);
  const slotMachineRef = useRef<DualSlotMachine>(new DualSlotMachine());
  const slotCleanupsRef = useRef<[(() => void) | null, (() => void) | null]>([
    null,
    null,
  ]);
  const slotTeardownTimersRef = useRef<[number | null, number | null]>([
    null,
    null,
  ]);
  const clearSlotTeardownTimer = useCallback((slot: VideoSlotIndex) => {
    const t = slotTeardownTimersRef.current[slot];
    if (t !== null && typeof window !== "undefined") {
      window.clearTimeout(t);
      slotTeardownTimersRef.current[slot] = null;
    }
  }, []);
  const [activeSlot, setActiveSlot] = useState<VideoSlotIndex>(0);
  const [outgoingSlot, setOutgoingSlot] = useState<VideoSlotIndex | null>(null);
  const [mseReady, setMseReady] = useState(false);
  const [streamDisconnected, setStreamDisconnected] = useState(false);
  const [headerLastSeenTs, setHeaderLastSeenTs] = useState<number>(0);
  const [posterTick, setPosterTick] = useState(() => Date.now());
  const [overlayBox, setOverlayBox] = useState<ContainedBox | null>(null);
  const [pttState, setPttState] = useState<PushToTalkState>("idle");
  const configuredCameras = useNvrStore((s) => s.config?.cameras);
  const go2rtcStreams = useNvrStore((s) => s.config?.go2rtc?.streams);
  const wsOnline = useNvrStore((s) => s.wsOnline);
  const realCamera = resolveRealCameraSource(camera, configuredCameras);
  const realCameraRef = useRef<string>(realCamera);
  realCameraRef.current = realCamera;
  const desiredStreamSrc = resolveDesiredLiveStreamSrc(
    realCamera,
    useMainStream,
    configuredCameras,
    go2rtcStreams
  );
  const desiredStreamSrcRef = useRef<string>(desiredStreamSrc);
  desiredStreamSrcRef.current = desiredStreamSrc;
  const reconnectTimerRef = useRef<number | null>(null);
  const reconnectAttemptRef = useRef<number>(0);
  const unmountedRef = useRef<boolean>(false);
  const connectDesiredStreamRef = useRef<
    (streamSrc: string, isCameraOrModeSwitch?: boolean) => void
  >(() => {});
  const camStats = useNvrStore(
    (s) => s.stats?.cameras?.[camera] ?? s.stats?.cameras?.[realCamera]
  );

  const clearReconnectTimer = useCallback(() => {
    if (reconnectTimerRef.current !== null && typeof window !== "undefined") {
      window.clearTimeout(reconnectTimerRef.current);
      reconnectTimerRef.current = null;
    }
  }, []);

  const scheduleReconnect = useCallback(
    (immediate = false) => {
      if (unmountedRef.current || typeof window === "undefined") return;
      const jitterMs = computeCameraJitterSeed(realCameraRef.current);
      if (immediate) {
        clearReconnectTimer();
        reconnectAttemptRef.current = 0;
        reconnectTimerRef.current = window.setTimeout(() => {
          reconnectTimerRef.current = null;
          if (unmountedRef.current) return;
          const target = desiredStreamSrcRef.current;
          if (slotMachineRef.current.needsReconnect(target)) {
            connectDesiredStreamRef.current(target, false);
          }
        }, jitterMs);
        return;
      }
      if (reconnectTimerRef.current !== null) {
        return;
      }
      const delayMs = computeMseReconnectDelayMs(
        reconnectAttemptRef.current,
        jitterMs
      );
      reconnectAttemptRef.current += 1;
      reconnectTimerRef.current = window.setTimeout(() => {
        reconnectTimerRef.current = null;
        if (unmountedRef.current) return;
        const target = desiredStreamSrcRef.current;
        if (slotMachineRef.current.needsReconnect(target)) {
          connectDesiredStreamRef.current(target, false);
        }
      }, delayMs);
    },
    [clearReconnectTimer]
  );

  const getSlotVideoEl = useCallback((slot: VideoSlotIndex): HTMLVideoElement | null => {
    return slot === 0 ? videoRef0.current : videoRef1.current;
  }, []);

  const getActiveVideoEl = useCallback((): HTMLVideoElement | null => {
    const curSlot = slotMachineRef.current.getState().activeSlot;
    return getSlotVideoEl(curSlot);
  }, [getSlotVideoEl]);

  const isOffline =
    streamDisconnected || (camStats !== undefined && camStats.camera_fps === 0 && !mseReady);
  const lastSeenTs =
    (camStats?.last_frame_ts && camStats.last_frame_ts > 0
      ? camStats.last_frame_ts
      : headerLastSeenTs) || 0;

  const detW = cameraConfig?.detect?.width || 1280;
  const detH = cameraConfig?.detect?.height || 720;

  const updateOverlayGeometry = useCallback(() => {
    const el = containerRef.current;
    if (!el) return;
    const cw = el.clientWidth;
    const ch = el.clientHeight;
    const v = getActiveVideoEl();
    const im = imgRef.current;
    const mw =
      (v && v.videoWidth > 0 ? v.videoWidth : 0) ||
      (im && im.naturalWidth > 0 ? im.naturalWidth : 0) ||
      detW;
    const mh =
      (v && v.videoHeight > 0 ? v.videoHeight : 0) ||
      (im && im.naturalHeight > 0 ? im.naturalHeight : 0) ||
      detH;
    setOverlayBox(computeContainedBox(cw, ch, mw, mh));
  }, [detW, detH, getActiveVideoEl]);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    updateOverlayGeometry();
    const ro = new ResizeObserver(() => {
      updateOverlayGeometry();
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [updateOverlayGeometry]);

  // Periodic fallback snapshot refresh when MSE is still handshaking or camera is offline
  useEffect(() => {
    if (mseReady) return;
    const probeLatestFrame = () => {
      if (typeof navigator !== "undefined" && navigator.onLine === false) {
        return;
      }
      fetch(`/api/${encodeURIComponent(realCamera)}/latest.jpg`, {
        method: "HEAD",
        credentials: "include",
      })
        .then((res) => {
          if (!res.ok) return;
          const hdr = res.headers.get("x-last-frame-timestamp");
          if (hdr) {
            const parsed = Number(hdr);
            if (Number.isFinite(parsed) && parsed > 0) {
              setHeaderLastSeenTs(parsed);
            }
          }
          setPosterTick(Date.now());
        })
        .catch(() => {});
    };
    probeLatestFrame();
    const timer = window.setInterval(probeLatestFrame, 2000);
    return () => window.clearInterval(timer);
  }, [realCamera, mseReady]);

  const startSlotSession = useCallback(
    (slot: VideoSlotIndex, streamSrc: string, sessionId: number) => {
      const videoEl = getSlotVideoEl(slot);
      const MediaSourceCtor =
        typeof window !== "undefined"
          ? window.MediaSource ||
            (
              window as unknown as {
                ManagedMediaSource?: typeof MediaSource;
              }
            ).ManagedMediaSource
          : undefined;
      if (!videoEl || !MediaSourceCtor) {
        slotMachineRef.current.markSlotError(slot, sessionId);
        return;
      }

      clearSlotTeardownTimer(slot);
      setOutgoingSlot((cur) => (cur === slot ? null : cur));

      if (slot === slotMachineRef.current.getState().activeSlot) {
        captureVideoElementFrame(realCameraRef.current, videoEl);
      }

      slotCleanupsRef.current[slot]?.();
      slotCleanupsRef.current[slot] = null;

      let closed = false;
      let failedOnce = false;
      let quotaRecoveries = 0;
      let ws: WebSocket | null = null;
      let mediaSource: MediaSource | null = null;
      let sourceBuffer: SourceBuffer | null = null;
      let watchdogTimer: number | null = null;
      const queue: ArrayBuffer[] = [];

      const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
      const wsUrl = `${proto}//${window.location.host}/live/mse/api/ws?src=${encodeURIComponent(
        streamSrc
      )}`;

      mediaSource = new MediaSourceCtor();
      const objUrl = URL.createObjectURL(mediaSource);
      videoEl.src = objUrl;

      let initialBurstSeekDone = false;
      let promotedOnce = false;

      const clearWatchdog = () => {
        if (watchdogTimer !== null) {
          window.clearTimeout(watchdogTimer);
          watchdogTimer = null;
        }
      };

      const handleSlotFailure = () => {
        if (closed || failedOnce) return;
        failedOnce = true;
        clearWatchdog();
        if (slot === slotMachineRef.current.getState().activeSlot) {
          captureVideoElementFrame(realCameraRef.current, videoEl);
        }
        const snap = slotMachineRef.current.markSlotError(slot, sessionId);
        if (!snap.isAnyReady) {
          setMseReady(false);
          setStreamDisconnected(true);
        }
        slotCleanupsRef.current[slot]?.();
        slotCleanupsRef.current[slot] = null;
        if (slotMachineRef.current.needsReconnect(desiredStreamSrcRef.current)) {
          scheduleReconnect(false);
        }
      };

      const resetWatchdog = (timeoutMs: number) => {
        clearWatchdog();
        if (closed) return;
        watchdogTimer = window.setTimeout(() => {
          watchdogTimer = null;
          handleSlotFailure();
        }, timeoutMs);
      };

      resetWatchdog(MSE_HANDSHAKE_TIMEOUT_MS);

      const flushQueue = () => {
        if (
          closed ||
          !sourceBuffer ||
          sourceBuffer.updating ||
          queue.length === 0 ||
          !mediaSource ||
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
          if (
            errName === "QuotaExceededError" &&
            quotaRecoveries < 2 &&
            !closed &&
            mediaSource &&
            mediaSource.readyState === "open" &&
            !sourceBuffer.updating &&
            videoEl.buffered.length > 0
          ) {
            try {
              const start = videoEl.buffered.start(0);
              const end = videoEl.buffered.end(videoEl.buffered.length - 1);
              if (end > start + 1) {
                quotaRecoveries += 1;
                if (queue.length < 256) {
                  queue.unshift(merged);
                }
                sourceBuffer.remove(start, Math.max(start + 0.5, end - 2));
                return;
              }
            } catch {
              // fall through to failure recovery
            }
          }
          handleSlotFailure();
        }
      };

      const onUpdateEnd = () => {
        if (closed || !sourceBuffer) return;
        try {
          if (videoEl.buffered.length > 0) {
            const end = videoEl.buffered.end(videoEl.buffered.length - 1);
            const start = videoEl.buffered.start(0);
            const seekTarget = computeLiveEdgeSeekTarget(
              start,
              end,
              videoEl.currentTime,
              !initialBurstSeekDone
            );
            if (seekTarget !== null) {
              videoEl.currentTime = seekTarget;
              initialBurstSeekDone = true;
            }
            if (videoEl.paused) {
              videoEl.play().catch(() => {});
            }
            if (videoEl.readyState >= 2) {
              tryPromoteSlot();
            }
            if (
              end - start > 18 &&
              !sourceBuffer.updating &&
              mediaSource &&
              mediaSource.readyState === "open"
            ) {
              try {
                sourceBuffer.remove(start, end - 10);
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
        if (closed) return;
        ws = new WebSocket(wsUrl);
        ws.binaryType = "arraybuffer";

        ws.onopen = () => {
          if (closed) return;
          resetWatchdog(MSE_HANDSHAKE_TIMEOUT_MS);
          const codecs = [
            'video/mp4; codecs="avc1.640029,mp4a.40.2"',
            'video/mp4; codecs="avc1.64002A,mp4a.40.2"',
            'video/mp4; codecs="avc1.640028,mp4a.40.2"',
            'video/mp4; codecs="avc1.4D401F,mp4a.40.2"',
            'video/mp4; codecs="avc1.4D0029,mp4a.40.2"',
            'video/mp4; codecs="avc1.42E01E,mp4a.40.2"',
            'video/mp4; codecs="avc1.640029"',
          ]
            .filter((c) => MediaSource.isTypeSupported(c))
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
            let msg: { type?: unknown; value?: unknown } | null = null;
            try {
              msg = JSON.parse(ev.data);
            } catch {
              return;
            }
            if (msg && msg.type === "mse" && typeof msg.value === "string" && mediaSource) {
              if (!sourceBuffer && mediaSource.readyState === "open") {
                try {
                  sourceBuffer = mediaSource.addSourceBuffer(msg.value);
                  sourceBuffer.mode = "segments";
                  sourceBuffer.addEventListener("updateend", onUpdateEnd);
                  sourceBuffer.addEventListener("error", handleSlotFailure);
                  flushQueue();
                } catch {
                  handleSlotFailure();
                }
              }
            }
          } else if (ev.data instanceof ArrayBuffer) {
            if (promotedOnce) {
              resetWatchdog(MSE_STALL_TIMEOUT_MS);
            }
            if (queue.length < 256) {
              queue.push(ev.data);
            }
            flushQueue();
          }
        };
      };

      mediaSource.addEventListener("sourceopen", onSourceOpen);

      const tryPromoteSlot = () => {
        if (closed || promotedOnce) return;
        const machineSnap = slotMachineRef.current.getState();
        const isStandbyUpgrade =
          machineSnap.isAnyReady && machineSnap.standbySlot === slot;
        // When upgrading in standby while another slot is already playing smoothly on screen,
        // wait until any live-edge seek completes so the promoted slot never stutters on frame 1.
        if (isStandbyUpgrade && (videoEl.seeking || videoEl.readyState < 2)) {
          return;
        }
        promotedOnce = true;
        const res = slotMachineRef.current.markSlotReady(slot, sessionId);
        if (res.promoted) {
          reconnectAttemptRef.current = 0;
          if (!slotMachineRef.current.needsReconnect(desiredStreamSrcRef.current)) {
            clearReconnectTimer();
          }
          resetWatchdog(MSE_STALL_TIMEOUT_MS);
          setActiveSlot(res.activeSlot);
          setMseReady(true);
          setStreamDisconnected(false);
          updateOverlayGeometry();
          videoEl.play().catch(() => {});
          if (res.teardownSlot !== null) {
            const oldSlot = res.teardownSlot;
            const cleanupToRun = slotCleanupsRef.current[oldSlot];
            clearSlotTeardownTimer(oldSlot);
            setOutgoingSlot(oldSlot);
            slotTeardownTimersRef.current[oldSlot] = window.setTimeout(() => {
              slotTeardownTimersRef.current[oldSlot] = null;
              setOutgoingSlot((cur) => (cur === oldSlot ? null : cur));
              if (
                cleanupToRun &&
                slotCleanupsRef.current[oldSlot] === cleanupToRun
              ) {
                cleanupToRun();
                slotCleanupsRef.current[oldSlot] = null;
              }
            }, 80);
          }
        }
      };

      const onLoadedData = () => {
        if (closed) return;
        if (videoEl.buffered.length > 0 && !initialBurstSeekDone) {
          const end = videoEl.buffered.end(videoEl.buffered.length - 1);
          const start = videoEl.buffered.start(0);
          const seekTarget = computeLiveEdgeSeekTarget(
            start,
            end,
            videoEl.currentTime,
            true
          );
          if (seekTarget !== null) {
            videoEl.currentTime = seekTarget;
            initialBurstSeekDone = true;
          }
        }
        videoEl.play().catch(() => {});
        tryPromoteSlot();
      };

      const onCanPlayOrSeeked = () => {
        if (closed) return;
        tryPromoteSlot();
      };

      const onVideoError = () => {
        handleSlotFailure();
      };

      videoEl.addEventListener("loadeddata", onLoadedData);
      videoEl.addEventListener("canplay", onCanPlayOrSeeked);
      videoEl.addEventListener("seeked", onCanPlayOrSeeked);
      videoEl.addEventListener("playing", onCanPlayOrSeeked);
      videoEl.addEventListener("error", onVideoError);
      videoEl.addEventListener("resize", updateOverlayGeometry);

      slotCleanupsRef.current[slot] = () => {
        closed = true;
        clearWatchdog();
        clearSlotTeardownTimer(slot);
        queue.length = 0;
        videoEl.removeEventListener("loadeddata", onLoadedData);
        videoEl.removeEventListener("canplay", onCanPlayOrSeeked);
        videoEl.removeEventListener("seeked", onCanPlayOrSeeked);
        videoEl.removeEventListener("playing", onCanPlayOrSeeked);
        videoEl.removeEventListener("error", onVideoError);
        videoEl.removeEventListener("resize", updateOverlayGeometry);
        try {
          mediaSource?.removeEventListener("sourceopen", onSourceOpen);
        } catch {
          // ignore
        }
        try {
          sourceBuffer?.removeEventListener("updateend", onUpdateEnd);
          sourceBuffer?.removeEventListener("error", handleSlotFailure);
          if (sourceBuffer && mediaSource && mediaSource.readyState === "open") {
            sourceBuffer.abort();
            mediaSource.removeSourceBuffer(sourceBuffer);
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
        try {
          videoEl.pause();
          videoEl.removeAttribute("src");
          videoEl.load();
        } catch {
          // ignore
        }
        URL.revokeObjectURL(objUrl);
      };
    },
    [
      clearReconnectTimer,
      clearSlotTeardownTimer,
      getSlotVideoEl,
      scheduleReconnect,
      updateOverlayGeometry,
    ]
  );

  const connectDesiredStream = useCallback(
    (streamSrc: string, isCameraOrModeSwitch = false) => {
      const transition = slotMachineRef.current.requestStream(streamSrc);
      if (transition.abortSlot !== null) {
        slotCleanupsRef.current[transition.abortSlot]?.();
        slotCleanupsRef.current[transition.abortSlot] = null;
      }
      if (transition.action === "noop") {
        return;
      }
      if (transition.action === "start_active" && isCameraOrModeSwitch) {
        setMseReady(false);
        setStreamDisconnected(false);
      }
      startSlotSession(transition.targetSlot, streamSrc, transition.sessionId);
    },
    [startSlotSession]
  );
  connectDesiredStreamRef.current = connectDesiredStream;

  // Double-buffered ping-pong transition when realCamera or desiredStreamSrc changes
  useEffect(() => {
    reconnectAttemptRef.current = 0;
    clearReconnectTimer();
    connectDesiredStream(desiredStreamSrc, true);
  }, [desiredStreamSrc, clearReconnectTimer, connectDesiredStream]);

  // Fast-path MSE recovery when global telemetry WebSocket (/ws) or camera FPS comes back online after pod rollout
  useEffect(() => {
    if (!wsOnline && !(camStats && camStats.camera_fps > 0)) return;
    if (slotMachineRef.current.needsReconnect(desiredStreamSrcRef.current)) {
      scheduleReconnect(true);
    }
  }, [wsOnline, camStats?.camera_fps, camStats?.last_frame_ts, scheduleReconnect]);

  // Fast-path MSE recovery on browser network restore ("online") or tab wake ("visibilitychange")
  useEffect(() => {
    if (typeof window === "undefined") return;
    const onNetworkOnline = () => {
      if (slotMachineRef.current.needsReconnect(desiredStreamSrcRef.current)) {
        scheduleReconnect(true);
      }
    };
    const onVisibilityChange = () => {
      if (typeof document !== "undefined" && document.visibilityState !== "visible") {
        return;
      }
      if (slotMachineRef.current.needsReconnect(desiredStreamSrcRef.current)) {
        scheduleReconnect(true);
        return;
      }
      const v = getActiveVideoEl();
      if (v) {
        try {
          if (v.buffered.length > 0) {
            const end = v.buffered.end(v.buffered.length - 1);
            const start = v.buffered.start(0);
            const seekTarget = computeLiveEdgeSeekTarget(
              start,
              end,
              v.currentTime,
              false
            );
            if (seekTarget !== null) {
              v.currentTime = seekTarget;
            }
          }
        } catch {
          // ignore
        }
        v.play().catch(() => {});
      }
    };
    window.addEventListener("online", onNetworkOnline);
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", onVisibilityChange);
    }
    return () => {
      window.removeEventListener("online", onNetworkOnline);
      if (typeof document !== "undefined") {
        document.removeEventListener("visibilitychange", onVisibilityChange);
      }
    };
  }, [getActiveVideoEl, scheduleReconnect]);

  // Capture live GPU-decoded frame into LiveFrameBridge and release decoders on unmount
  useEffect(() => {
    unmountedRef.current = false;
    return () => {
      unmountedRef.current = true;
      clearReconnectTimer();
      clearSlotTeardownTimer(0);
      clearSlotTeardownTimer(1);
      captureVideoElementFrame(realCameraRef.current, getActiveVideoEl());
      slotCleanupsRef.current[0]?.();
      slotCleanupsRef.current[1]?.();
      slotCleanupsRef.current = [null, null];
    };
  }, [clearReconnectTimer, clearSlotTeardownTimer, getActiveVideoEl]);

  const masks = extractMaskPolygons(cameraConfig?.motion?.mask);
  const objectMasks = extractMaskPolygons(cameraConfig?.objects?.mask);
  const privacyMasks = extractMaskPolygons(cameraConfig?.privacy_mask);
  const zones = cameraConfig?.zones || {};
  const rawRotate = cameraConfig?.live?.rotate;
  const rotateDeg =
    rawRotate === 90 || rawRotate === 180 || rawRotate === 270 ? rawRotate : 0;
  const isCorridorRot = rotateDeg === 90 || rotateDeg === 270;
  const rotateTransform = rotateDeg
    ? `rotate(${rotateDeg}deg)${isCorridorRot ? " scale(0.5625)" : ""}`
    : undefined;

  const handleCaptureLiveStill = (e: React.MouseEvent) => {
    e.stopPropagation();
    const vid = getActiveVideoEl();
    const img = imgRef.current;
    const canvas = document.createElement("canvas");
    if (vid && vid.videoWidth > 0 && vid.videoHeight > 0 && mseReady) {
      canvas.width = vid.videoWidth;
      canvas.height = vid.videoHeight;
      const ctx = canvas.getContext("2d");
      ctx?.drawImage(vid, 0, 0, canvas.width, canvas.height);
    } else if (img && img.naturalWidth > 0 && img.naturalHeight > 0) {
      canvas.width = img.naturalWidth;
      canvas.height = img.naturalHeight;
      const ctx = canvas.getContext("2d");
      ctx?.drawImage(img, 0, 0, canvas.width, canvas.height);
    } else {
      return;
    }
    const link = document.createElement("a");
    link.download = `${camera}_${Math.floor(Date.now() / 1000)}.png`;
    link.href = canvas.toDataURL("image/png");
    link.click();
  };

  const posterSrc = resolveLivePosterUrl(realCamera, useMainStream, posterTick);
  const effCfg = cameraConfig ?? configuredCameras?.[realCamera];
  const effProto = (
    effCfg?.hardware?.protocol ||
    effCfg?.onvif?.protocol ||
    ""
  )
    .trim()
    .toLowerCase();
  const resolvedPttSupported = Boolean(onPlotPoint)
    ? false
    : pushToTalkSupported !== undefined
    ? pushToTalkSupported
    : configuredCameras === undefined && cameraConfig === undefined
    ? true
    : Boolean(
        effCfg?.onvif?.has_cloud_password ||
          effCfg?.hardware?.has_cloud_password ||
          effProto === "dahua_cgi" ||
          effProto === "intelbras"
      );
  const resolvedPttMode =
    pushToTalkMode ??
    (effProto === "dahua_cgi" || effProto === "intelbras" ? "cgi_alaw" : "webrtc");

  return (
    <div
      ref={containerRef}
      style={{
        position: "relative",
        width: "100%",
        height: "100%",
        minWidth: 0,
        minHeight: 0,
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        overflow: "hidden",
        background: "var(--video-stage-bg)",
        cursor: onPlotPoint ? "crosshair" : onClick ? "pointer" : "default",
      }}
      onPointerEnter={() => {
        if (!useMainStream && Boolean(onClick)) {
          prewarmStream(realCamera);
        }
      }}
      onClick={
        onPlotPoint
          ? (e) => {
              const rect = e.currentTarget.getBoundingClientRect();
              const box = overlayBox || {
                left: 0,
                top: 0,
                width: rect.width,
                height: rect.height,
              };
              const relX = e.clientX - rect.left - box.left;
              const relY = e.clientY - rect.top - box.top;
              const nx = Math.max(0, Math.min(1, relX / Math.max(1, box.width)));
              const ny = Math.max(0, Math.min(1, relY / Math.max(1, box.height)));
              onPlotPoint(nx, ny);
            }
          : onClick
      }
    >
      {/* Zero-black-screen instant snapshot underlay while MSE connects — strictly object-fit: contain */}
      <ZoomableVideoStage
        disabled={Boolean(onPlotPoint)}
        forceHudVisible={pttState !== "idle"}
        extraHud={
          <>
            <PushToTalkButton
              camera={realCamera}
              height={20}
              supported={resolvedPttSupported}
              mode={resolvedPttMode}
              onStateChange={(nextState) => setPttState(nextState)}
            />
            <button
              type="button"
              className="nvr-btn mono"
              style={{ height: 20, padding: "0 6px", fontSize: "var(--text-xs)" }}
              title="Save high-resolution PNG photo of current frame"
              onClick={handleCaptureLiveStill}
            >
              <Camera size={11} />
              Photo
            </button>
          </>
        }
      >
        {(pttState === "talking" || pttState === "connecting") && (
          <div
            data-testid={`ptt-indicator-${camera}`}
            className="mono"
            style={{
              position: "absolute",
              top: 8,
              left: 8,
              zIndex: 14,
              display: "inline-flex",
              alignItems: "center",
              gap: 6,
              background: "rgba(127, 29, 29, 0.92)",
              border: "1px solid var(--signal-alert)",
              color: "#fecaca",
              padding: "3px 8px",
              borderRadius: 3,
              fontSize: "var(--text-xs)",
              fontWeight: 700,
              fontVariantNumeric: "tabular-nums",
              pointerEvents: "none",
            }}
          >
            <span className="nvr-dot alert" />
            {pttState === "talking"
              ? `TX // MIC LIVE (${camera.toUpperCase()})`
              : `TX // CONNECTING (${camera.toUpperCase()})`}
          </div>
        )}
        <img
          ref={imgRef}
          className="nvr-cam-poster"
          src={posterSrc}
          alt={camera}
          onLoad={updateOverlayGeometry}
          style={{
            position: "absolute",
            inset: 0,
            width: "100%",
            height: "100%",
            objectFit: "contain",
            transform: rotateTransform,
            opacity: mseReady ? 0 : 1,
            filter: isOffline ? "grayscale(0.65) brightness(0.55)" : "none",
            transition: "opacity 0.2s ease, filter 0.2s ease",
            pointerEvents: "none",
          }}
        />
        {isOffline && (
          <div
            className="mono"
            style={{
              position: "absolute",
              left: 10,
              bottom: 10,
              zIndex: 10,
              background: "rgba(9, 13, 20, 0.88)",
              border: "1px solid var(--signal-alert)",
              color: "var(--signal-alert-text)",
              padding: "3px 8px",
              borderRadius: 3,
              fontSize: "var(--text-xs)",
              fontWeight: 700,
              fontVariantNumeric: "tabular-nums",
              pointerEvents: "none",
            }}
          >
            STREAM OFFLINE •{" "}
            {lastSeenTs > 0
              ? `LAST SEEN ${formatTs(lastSeenTs)} (${formatRelTime(lastSeenTs)})`
              : "WAITING FOR SIGNAL"}
          </div>
        )}
        <video
          ref={videoRef0}
          data-slot="0"
          autoPlay
          playsInline
          muted={muted && activeSlot === 0}
          style={{
            position: "absolute",
            inset: 0,
            width: "100%",
            height: "100%",
            objectFit: "contain",
            transform: rotateTransform,
            zIndex: activeSlot === 0 ? 2 : 1,
            opacity: mseReady && (activeSlot === 0 || outgoingSlot === 0) ? 1 : 0,
            transition: "none",
            pointerEvents: "none",
          }}
        />
        <video
          ref={videoRef1}
          data-slot="1"
          autoPlay
          playsInline
          muted={muted && activeSlot === 1}
          style={{
            position: "absolute",
            inset: 0,
            width: "100%",
            height: "100%",
            objectFit: "contain",
            transform: rotateTransform,
            zIndex: activeSlot === 1 ? 2 : 1,
            opacity: mseReady && (activeSlot === 1 || outgoingSlot === 1) ? 1 : 0,
            transition: "none",
            pointerEvents: "none",
          }}
        />
        {(showVectors ||
          privacyMasks.length > 0 ||
          (draftPoints && draftPoints.length > 0)) &&
          overlayBox && (
          <svg
            viewBox="0 0 1000 1000"
            preserveAspectRatio="none"
            style={{
              position: "absolute",
              left: `${overlayBox.left}px`,
              top: `${overlayBox.top}px`,
              width: `${overlayBox.width}px`,
              height: `${overlayBox.height}px`,
              pointerEvents: "none",
              zIndex: 4,
            }}
          >
            {privacyMasks.map((m, idx) => (
              <polygon
                key={`privmask-${idx}`}
                points={parsePolygonPoints(m, detW, detH)}
                fill="#0c0e12"
                stroke="#475569"
                strokeWidth="2"
              />
            ))}
            {showVectors &&
              masks.map((m, idx) => (
              <polygon
                key={`mask-${idx}`}
                points={parsePolygonPoints(m, detW, detH)}
                fill="rgba(239, 68, 68, 0.24)"
                stroke="#ef4444"
                strokeWidth="3"
                strokeDasharray="8 4"
              />
            ))}
            {showVectors &&
              objectMasks.map((m, idx) => (
              <polygon
                key={`objmask-${idx}`}
                points={parsePolygonPoints(m, detW, detH)}
                fill="rgba(168, 85, 247, 0.22)"
                stroke="#a855f7"
                strokeWidth="3"
                strokeDasharray="5 5"
              />
            ))}
            {showVectors &&
              Object.entries(zones).map(([zName, zCfg]) => (
              <g key={`zone-${zName}`}>
                <polygon
                  points={parsePolygonPoints(zCfg.coordinates, detW, detH)}
                  fill="rgba(14, 165, 233, 0.18)"
                  stroke="#0ea5e9"
                  strokeWidth="3"
                />
              </g>
            ))}
            {showVectors &&
              Object.entries(cameraConfig?.lines || {}).map(([lName, lCfg]) => {
              const pts = lCfg.points || [0, 0, 0, 0];
              const isNorm =
                pts[0] <= 1.0 && pts[1] <= 1.0 && pts[2] <= 1.0 && pts[3] <= 1.0;
              const x1 = Math.round((isNorm ? pts[0] : pts[0] / Math.max(1, detW)) * 1000);
              const y1 = Math.round((isNorm ? pts[1] : pts[1] / Math.max(1, detH)) * 1000);
              const x2 = Math.round((isNorm ? pts[2] : pts[2] / Math.max(1, detW)) * 1000);
              const y2 = Math.round((isNorm ? pts[3] : pts[3] / Math.max(1, detH)) * 1000);
              const mx = (x1 + x2) / 2;
              const my = (y1 + y2) / 2;
              const dx = x2 - x1;
              const dy = y2 - y1;
              const len = Math.hypot(dx, dy) || 1;
              const sign = lCfg.direction === "b_to_a" ? -1 : 1;
              const nx = (-dy / len) * 28 * sign;
              const ny = (dx / len) * 28 * sign;
              return (
                <g key={`line-${lName}`}>
                  <line
                    x1={x1}
                    y1={y1}
                    x2={x2}
                    y2={y2}
                    stroke="#10b981"
                    strokeWidth="4"
                  />
                  <line
                    x1={mx}
                    y1={my}
                    x2={mx + nx}
                    y2={my + ny}
                    stroke="#34d399"
                    strokeWidth="3"
                  />
                  <circle cx={mx + nx} cy={my + ny} r="5" fill="#10b981" />
                </g>
              );
            })}
            {draftPoints && draftPoints.length > 0 && (
              <g>
                <polygon
                  points={draftPoints
                    .map(([x, y]) => `${Math.round(x * 1000)},${Math.round(y * 1000)}`)
                    .join(" ")}
                  fill={draftColor}
                  fillOpacity={0.22}
                  stroke={draftColor}
                  strokeWidth="3"
                />
                {draftPoints.map(([x, y], idx) => (
                  <circle
                    key={idx}
                    cx={Math.round(x * 1000)}
                    cy={Math.round(y * 1000)}
                    r="7"
                    fill={draftColor}
                    stroke="#000"
                    strokeWidth="2"
                  />
                ))}
              </g>
            )}
          </svg>
        )}
        {stageOverlay && overlayBox && (
          <div
            style={{
              position: "absolute",
              left: `${overlayBox.left}px`,
              top: `${overlayBox.top}px`,
              width: `${overlayBox.width}px`,
              height: `${overlayBox.height}px`,
              pointerEvents: "none",
              zIndex: 5,
            }}
          >
            {stageOverlay}
          </div>
        )}
      </ZoomableVideoStage>
    </div>
  );
};

interface VodHlsPlayerProps {
  camera: string;
  startTime: number;
  endTime: number;
  fallbackPreviewId?: string;
  playbackRate?: number;
  autoPlay?: boolean;
  muted?: boolean;
  controls?: boolean;
  onTimeUpdate?: (currentSec: number) => void;
  onEnded?: () => void;
}

function safeSessionStorageGet(key: string): string | null {
  try {
    if (typeof window !== "undefined") {
      return window.sessionStorage?.getItem(key) ?? null;
    }
  } catch {
    // ignore storage access errors
  }
  return null;
}

function safeSessionStorageSet(key: string, val: string): void {
  try {
    if (typeof window !== "undefined") {
      window.sessionStorage?.setItem(key, val);
    }
  } catch {
    // ignore storage access/quota errors
  }
}

export const VodHlsPlayer: React.FC<VodHlsPlayerProps> = ({
  camera,
  startTime,
  endTime,
  fallbackPreviewId,
  playbackRate = 1,
  autoPlay = true,
  muted = true,
  controls = true,
  onTimeUpdate,
  onEnded,
}) => {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const endedFiredRef = useRef(false);
  const configuredCameras = useNvrStore((s) => s.config?.cameras);
  const realCamera = resolveRealCameraSource(camera, configuredCameras);
  const rawVodRotateDeg = useNvrStore(
    (s) =>
      s.config?.cameras?.[camera]?.live?.rotate ||
      s.config?.cameras?.[realCamera]?.live?.rotate ||
      0
  );
  const vodRotateDeg =
    rawVodRotateDeg === 90 || rawVodRotateDeg === 180 || rawVodRotateDeg === 270
      ? rawVodRotateDeg
      : 0;
  const vodIsCorridorRot = vodRotateDeg === 90 || vodRotateDeg === 270;
  const vodRotateTransform = vodRotateDeg
    ? `rotate(${vodRotateDeg}deg)${vodIsCorridorRot ? " scale(0.5625)" : ""}`
    : undefined;
  const [useFallbackMp4, setUseFallbackMp4] = useState(false);
  const [userMuted, setUserMuted] = useState<boolean>(() => {
    const saved = safeSessionStorageGet("ninjanvr_vod_muted");
    if (saved === "false") return false;
    if (saved === "true") return true;
    return muted;
  });

  const safePlaybackRate =
    typeof playbackRate === "number" &&
    Number.isFinite(playbackRate) &&
    playbackRate > 0
      ? Math.min(16, Math.max(0.1, playbackRate))
      : 1;
  const finiteStart =
    typeof startTime === "number" && Number.isFinite(startTime)
      ? startTime
      : Math.floor(Date.now() / 1000) - 60;
  const finiteEnd =
    typeof endTime === "number" && Number.isFinite(endTime)
      ? endTime
      : finiteStart + 12;
  const safeStart = Math.floor(finiteStart);
  const safeEnd = Math.ceil(Math.max(finiteEnd, finiteStart + 12));
  const hlsUrl = `/vod/${encodeURIComponent(realCamera)}/start/${safeStart}/end/${safeEnd}/index.m3u8`;
  const mp4FallbackUrl = fallbackPreviewId
    ? `/api/review/${encodeURIComponent(fallbackPreviewId)}/preview?format=mp4`
    : `/api/${encodeURIComponent(realCamera)}/latest.webp`;

  useEffect(() => {
    setUseFallbackMp4(false);
    endedFiredRef.current = false;
  }, [realCamera, safeStart, safeEnd, fallbackPreviewId]);

  useEffect(() => {
    const videoEl = videoRef.current;
    if (!videoEl) return;
    if (useFallbackMp4) {
      return () => {
        try {
          videoEl.pause();
          videoEl.removeAttribute("src");
          videoEl.load();
        } catch {
          // ignore
        }
      };
    }

    let hls: Hls | null = null;
    if (Hls.isSupported()) {
      hls = new Hls({
        enableWorker: true,
        lowLatencyMode: false,
      });
      hls.loadSource(hlsUrl);
      hls.attachMedia(videoEl);
      hls.on(Hls.Events.MANIFEST_PARSED, () => {
        videoEl.playbackRate = safePlaybackRate;
        videoEl.muted = userMuted;
        if (autoPlay) {
          videoEl.play().catch(() => {
            videoEl.muted = true;
            setUserMuted(true);
            videoEl.play().catch(() => {});
          });
        }
      });
      let mediaErrorRecoveries = 0;
      hls.on(Hls.Events.ERROR, (_ev, data) => {
        if (!data.fatal) {
          if (data.type === Hls.ErrorTypes.MEDIA_ERROR && mediaErrorRecoveries < 4) {
            mediaErrorRecoveries += 1;
            hls?.recoverMediaError();
          }
          return;
        }
        if (data.type === Hls.ErrorTypes.MEDIA_ERROR && mediaErrorRecoveries < 4) {
          mediaErrorRecoveries += 1;
          hls?.recoverMediaError();
          return;
        }
        if (data.type === Hls.ErrorTypes.NETWORK_ERROR && mediaErrorRecoveries < 2) {
          mediaErrorRecoveries += 1;
          hls?.startLoad();
          return;
        }
        if (fallbackPreviewId) {
          setUseFallbackMp4(true);
        }
      });
    } else if (videoEl.canPlayType("application/vnd.apple.mpegurl")) {
      videoEl.src = hlsUrl;
      videoEl.playbackRate = safePlaybackRate;
      if (autoPlay) {
        videoEl.play().catch(() => {});
      }
    }

    return () => {
      if (hls) {
        try {
          hls.stopLoad();
          hls.detachMedia();
        } catch {
          // ignore
        }
        hls.destroy();
      }
      try {
        videoEl.pause();
        videoEl.removeAttribute("src");
        videoEl.load();
      } catch {
        // ignore
      }
    };
  }, [hlsUrl, useFallbackMp4, fallbackPreviewId, autoPlay]);

  useEffect(() => {
    if (videoRef.current) {
      videoRef.current.playbackRate = safePlaybackRate;
    }
  }, [safePlaybackRate, useFallbackMp4]);

  const stepVodFrame = (dir: -1 | 1) => {
    const vid = videoRef.current;
    if (!vid) return;
    vid.pause();
    const frameDur = 1 / 15;
    const cur = Number.isFinite(vid.currentTime) ? vid.currentTime : 0;
    vid.currentTime = Math.max(0, cur + dir * frameDur);
  };

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      const tag = target?.tagName?.toLowerCase();
      if (tag === "input" || tag === "textarea" || tag === "select" || target?.isContentEditable) {
        return;
      }
      if (e.ctrlKey || e.metaKey || e.altKey) return;
      if (e.key === ",") {
        e.preventDefault();
        stepVodFrame(-1);
      } else if (e.key === ".") {
        e.preventDefault();
        stepVodFrame(1);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const handleCaptureVodStill = () => {
    const vid = videoRef.current;
    if (!vid || vid.videoWidth === 0 || vid.videoHeight === 0) return;
    const canvas = document.createElement("canvas");
    canvas.width = vid.videoWidth;
    canvas.height = vid.videoHeight;
    const ctx = canvas.getContext("2d");
    ctx?.drawImage(vid, 0, 0, canvas.width, canvas.height);
    const ts = Math.floor(safeStart + (vid.currentTime || 0));
    const link = document.createElement("a");
    link.download = `${camera}_${ts}.png`;
    link.href = canvas.toDataURL("image/png");
    link.click();
  };

  const { locale } = useI18n();
  const isPt = locale === "pt";

  const vodFrameControls = (
    <>
      <button
        type="button"
        className="nvr-btn mono"
        style={{ height: 20, padding: "0 6px", fontSize: "var(--text-xs)" }}
        title={
          isPt
            ? "Recuar 1 quadro de vídeo (Atalho: tecla ,)"
            : "Step 1 frame backward (Hotkey: ,)"
        }
        onClick={() => stepVodFrame(-1)}
      >
        <ChevronLeft size={12} />
        {isPt ? "Quadro Ant." : "Prev Frame"}
      </button>
      <button
        type="button"
        className="nvr-btn mono"
        style={{ height: 20, padding: "0 6px", fontSize: "var(--text-xs)" }}
        title={
          isPt
            ? "Avançar 1 quadro de vídeo (Atalho: tecla .)"
            : "Step 1 frame forward (Hotkey: .)"
        }
        onClick={() => stepVodFrame(1)}
      >
        {isPt ? "Próx. Quadro" : "Next Frame"}
        <ChevronRight size={12} />
      </button>
      <button
        type="button"
        className="nvr-btn mono"
        style={{ height: 20, padding: "0 6px", fontSize: "var(--text-xs)" }}
        title={
          isPt
            ? "Salvar foto PNG na resolução original deste quadro"
            : "Save native-resolution PNG photo of this frame"
        }
        onClick={handleCaptureVodStill}
      >
        <Camera size={11} />
        {isPt ? "Salvar Foto" : "Save Photo"}
      </button>
    </>
  );

  const handleTimeUpdateInternal = (curSec: number, durSec: number) => {
    onTimeUpdate?.(curSec);
    if (!onEnded || endedFiredRef.current) return;
    const clipLen = Math.max(4, safeEnd - safeStart);
    if (
      (Number.isFinite(durSec) && durSec > 0 && curSec >= durSec - 0.25) ||
      curSec >= clipLen
    ) {
      endedFiredRef.current = true;
      onEnded();
    }
  };

  if (useFallbackMp4 && fallbackPreviewId) {
    return (
      <ZoomableVideoStage extraHud={vodFrameControls}>
        <video
          ref={videoRef}
          src={mp4FallbackUrl}
          controls={controls}
          autoPlay={autoPlay}
          playsInline
          muted={userMuted}
          loop={!onEnded}
          onVolumeChange={(e) => {
            const next = e.currentTarget.muted;
            setUserMuted(next);
            safeSessionStorageSet("ninjanvr_vod_muted", String(next));
          }}
          onTimeUpdate={(e) =>
            handleTimeUpdateInternal(e.currentTarget.currentTime, e.currentTarget.duration)
          }
          onEnded={() => {
            if (onEnded && !endedFiredRef.current) {
              endedFiredRef.current = true;
              onEnded();
            }
          }}
          style={{
            width: "100%",
            height: "100%",
            objectFit: "contain",
            transform: vodRotateTransform,
            background: "var(--video-stage-bg)",
          }}
        />
      </ZoomableVideoStage>
    );
  }

  return (
    <ZoomableVideoStage extraHud={vodFrameControls}>
      <video
        ref={videoRef}
        controls={controls}
        autoPlay={autoPlay}
        playsInline
        muted={userMuted}
        onVolumeChange={(e) => {
          const next = e.currentTarget.muted;
          setUserMuted(next);
          safeSessionStorageSet("ninjanvr_vod_muted", String(next));
        }}
        onTimeUpdate={(e) =>
          handleTimeUpdateInternal(e.currentTarget.currentTime, e.currentTarget.duration)
        }
        onEnded={() => {
          if (onEnded && !endedFiredRef.current) {
            endedFiredRef.current = true;
            onEnded();
          }
        }}
        style={{
          width: "100%",
          height: "100%",
          objectFit: "contain",
          transform: vodRotateTransform,
          background: "var(--video-stage-bg)",
        }}
      />
    </ZoomableVideoStage>
  );
};

interface CanvasSparklineProps {
  values: number[];
  color: string;
  maxVal?: number;
  unit?: string;
  height?: number;
}

/**
 * High-density HTML5 Canvas sparkline with zero DOM/SVG layout thrashing.
 */
export const CanvasSparkline: React.FC<CanvasSparklineProps> = ({
  values,
  color,
  maxVal,
  height = 68,
}) => {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const themeMode = useNvrStore((s) => s.themeMode);
  const highContrast = useNvrStore((s) => s.highContrast);
  const effectiveTheme = resolveEffectiveTheme(themeMode);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const palette = resolveCanvasThemePalette(effectiveTheme);
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 300;
    const h = height;
    canvas.width = w * dpr;
    canvas.height = h * dpr;
    ctx.scale(dpr, dpr);

    ctx.clearRect(0, 0, w, h);

    ctx.strokeStyle = palette.sparklineGridStroke;
    ctx.lineWidth = 1;
    for (const frac of [0.25, 0.5, 0.75]) {
      const y = Math.round(h * frac) + 0.5;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }

    if (values.length < 2) return;
    const safeValues = values.map((v) =>
      typeof v === "number" && Number.isFinite(v) ? v : 0
    );
    const safeMaxVal =
      typeof maxVal === "number" && Number.isFinite(maxVal) ? maxVal : 0;
    const peak = Math.max(safeMaxVal, ...safeValues, 1);

    ctx.beginPath();
    safeValues.forEach((v, idx) => {
      const x = (idx / (safeValues.length - 1)) * (w - 4) + 2;
      const y = h - 4 - Math.min(1, Math.max(0, v / peak)) * (h - 10);
      if (idx === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.75;
    ctx.stroke();

    ctx.lineTo(w - 2, h - 2);
    ctx.lineTo(2, h - 2);
    ctx.closePath();
    ctx.fillStyle = `${color}22`;
    ctx.fill();
  }, [values, color, maxVal, height, effectiveTheme, highContrast]);

  return (
    <canvas
      ref={canvasRef}
      style={{ width: "100%", height: `${height}px`, display: "block" }}
    />
  );
};
