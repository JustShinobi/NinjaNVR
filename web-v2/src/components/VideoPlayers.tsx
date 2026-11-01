import React, { useEffect, useRef, useState } from "react";
import Hls from "hls.js";
import { CameraConfig } from "../types/nvr";

interface LiveMsePlayerProps {
  camera: string;
  useMainStream?: boolean;
  cameraConfig?: CameraConfig;
  showVectors?: boolean;
  muted?: boolean;
  onClick?: () => void;
}

/**
 * Parses a Frigate normalized or pixel polygon string ("x1,y1,x2,y2,...") into SVG points ("0..1000" coordinate space).
 */
export function parsePolygonPoints(
  coordStr: string,
  detWidth: number,
  detHeight: number
): string {
  const nums = coordStr
    .split(",")
    .map((s) => parseFloat(s.trim()))
    .filter((n) => !Number.isNaN(n));
  const pairs: string[] = [];
  for (let i = 0; i + 1 < nums.length; i += 2) {
    let x = nums[i];
    let y = nums[i + 1];
    if (x > 1.0 || y > 1.0) {
      x = x / Math.max(1, detWidth);
      y = y / Math.max(1, detHeight);
    }
    const sx = Math.round(Math.max(0, Math.min(1, x)) * 1000);
    const sy = Math.round(Math.max(0, Math.min(1, y)) * 1000);
    pairs.push(`${sx},${sy}`);
  }
  return pairs.join(" ");
}

export const LiveMsePlayer: React.FC<LiveMsePlayerProps> = ({
  camera,
  useMainStream = false,
  cameraConfig,
  showVectors = false,
  muted = true,
  onClick,
}) => {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const [mseReady, setMseReady] = useState(false);
  const [posterTick, setPosterTick] = useState(() => Date.now());

  // Periodic fallback snapshot refresh when MSE is still handshaking
  useEffect(() => {
    if (mseReady) return;
    const timer = window.setInterval(() => {
      setPosterTick(Date.now());
    }, 2000);
    return () => window.clearInterval(timer);
  }, [mseReady]);

  useEffect(() => {
    const videoEl = videoRef.current;
    if (!videoEl || typeof window === "undefined" || !("MediaSource" in window)) {
      return;
    }

    let closed = false;
    let ws: WebSocket | null = null;
    let mediaSource: MediaSource | null = null;
    let sourceBuffer: SourceBuffer | null = null;
    const queue: ArrayBuffer[] = [];

    const streamSrc = useMainStream ? camera : `${camera}_sub`;
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const wsUrl = `${proto}//${window.location.host}/live/mse/api/ws?src=${encodeURIComponent(
      streamSrc
    )}`;

    setMseReady(false);
    mediaSource = new MediaSource();
    const objUrl = URL.createObjectURL(mediaSource);
    videoEl.src = objUrl;

    const flushQueue = () => {
      if (closed || !sourceBuffer || sourceBuffer.updating || queue.length === 0) {
        return;
      }
      const chunk = queue.shift();
      if (!chunk) return;
      try {
        sourceBuffer.appendBuffer(chunk);
        // Keep live latency tight (< 2.5s) and trim old buffer
        if (videoEl.buffered.length > 0) {
          const end = videoEl.buffered.end(videoEl.buffered.length - 1);
          const start = videoEl.buffered.start(0);
          if (end - videoEl.currentTime > 2.5) {
            videoEl.currentTime = end - 0.35;
          }
          if (end - start > 18 && !sourceBuffer.updating) {
            try {
              sourceBuffer.remove(start, end - 10);
            } catch {
              // ignore buffer trim race
            }
          }
        }
      } catch {
        // ignore transient append error
      }
    };

    mediaSource.addEventListener("sourceopen", () => {
      if (closed) return;
      ws = new WebSocket(wsUrl);
      ws.binaryType = "arraybuffer";

      ws.onopen = () => {
        const codecs = [
          'video/mp4; codecs="avc1.640029,mp4a.40.2"',
          'video/mp4; codecs="avc1.64002A,mp4a.40.2"',
          'video/mp4; codecs="avc1.4D401F,mp4a.40.2"',
          'video/mp4; codecs="avc1.42E01E,mp4a.40.2"',
          'video/mp4; codecs="avc1.640029"',
        ]
          .filter((c) => MediaSource.isTypeSupported(c))
          .map((c) => c.replace('video/mp4; codecs="', "").replace('"', ""))
          .join(",");

        ws?.send(
          JSON.stringify({
            type: "mse",
            value: codecs || "avc1.640029,avc1.4D401F,avc1.42E01E,mp4a.40.2",
          })
        );
      };

      ws.onmessage = (ev) => {
        if (closed) return;
        if (typeof ev.data === "string") {
          try {
            const msg = JSON.parse(ev.data);
            if (msg.type === "mse" && typeof msg.value === "string" && mediaSource) {
              if (!sourceBuffer && mediaSource.readyState === "open") {
                sourceBuffer = mediaSource.addSourceBuffer(msg.value);
                sourceBuffer.mode = "segments";
                sourceBuffer.addEventListener("updateend", flushQueue);
              }
            }
          } catch {
            // ignore malformed JSON
          }
        } else if (ev.data instanceof ArrayBuffer) {
          if (queue.length < 30) {
            queue.push(ev.data);
          }
          flushQueue();
        }
      };
    });

    const onLoadedData = () => {
      if (!closed) {
        setMseReady(true);
        videoEl.play().catch(() => {});
      }
    };
    videoEl.addEventListener("loadeddata", onLoadedData);

    return () => {
      closed = true;
      videoEl.removeEventListener("loadeddata", onLoadedData);
      try {
        ws?.close();
      } catch {
        // ignore
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
  }, [camera, useMainStream]);

  const detW = cameraConfig?.detect?.width || 1280;
  const detH = cameraConfig?.detect?.height || 720;
  const masks = cameraConfig?.motion?.mask || [];
  const zones = cameraConfig?.zones || {};

  return (
    <div
      style={{
        position: "relative",
        width: "100%",
        height: "100%",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        cursor: onClick ? "pointer" : "default",
      }}
      onClick={onClick}
    >
      {/* Zero-black-screen instant snapshot underlay while MSE connects */}
      <img
        className="nvr-cam-poster"
        src={`/api/${camera}/latest.webp?height=360&cache=${posterTick}`}
        alt={camera}
        style={{
          position: "absolute",
          inset: 0,
          opacity: mseReady ? 0 : 1,
          transition: "opacity 0.2s ease",
          pointerEvents: "none",
        }}
      />
      <video
        ref={videoRef}
        autoPlay
        playsInline
        muted={muted}
        style={{
          width: "100%",
          height: "100%",
          objectFit: "contain",
          opacity: mseReady ? 1 : 0,
          transition: "opacity 0.2s ease",
        }}
      />
      {showVectors && (
        <svg
          viewBox="0 0 1000 1000"
          preserveAspectRatio="none"
          style={{
            position: "absolute",
            inset: 0,
            width: "100%",
            height: "100%",
            pointerEvents: "none",
            zIndex: 4,
          }}
        >
          {masks.map((m, idx) => (
            <polygon
              key={`mask-${idx}`}
              points={parsePolygonPoints(m, detW, detH)}
              fill="rgba(239, 68, 68, 0.24)"
              stroke="#ef4444"
              strokeWidth="3"
              strokeDasharray="8 4"
            />
          ))}
          {Object.entries(zones).map(([zName, zCfg]) => (
            <g key={`zone-${zName}`}>
              <polygon
                points={parsePolygonPoints(zCfg.coordinates, detW, detH)}
                fill="rgba(14, 165, 233, 0.18)"
                stroke="#0ea5e9"
                strokeWidth="3"
              />
            </g>
          ))}
        </svg>
      )}
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
}) => {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const [useFallbackMp4, setUseFallbackMp4] = useState(false);

  const safeStart = Math.floor(startTime);
  const safeEnd = Math.ceil(Math.max(endTime, startTime + 12));
  const hlsUrl = `/vod/${encodeURIComponent(camera)}/start/${safeStart}/end/${safeEnd}/index.m3u8`;
  const mp4FallbackUrl = fallbackPreviewId
    ? `/api/review/${encodeURIComponent(fallbackPreviewId)}/preview?format=mp4`
    : `/api/${encodeURIComponent(camera)}/latest.webp`;

  useEffect(() => {
    setUseFallbackMp4(false);
  }, [camera, safeStart, safeEnd, fallbackPreviewId]);

  useEffect(() => {
    const videoEl = videoRef.current;
    if (!videoEl || useFallbackMp4) return;

    let hls: Hls | null = null;
    if (Hls.isSupported()) {
      hls = new Hls({
        enableWorker: true,
        lowLatencyMode: false,
      });
      hls.loadSource(hlsUrl);
      hls.attachMedia(videoEl);
      hls.on(Hls.Events.MANIFEST_PARSED, () => {
        videoEl.playbackRate = playbackRate;
        if (autoPlay) {
          videoEl.play().catch(() => {});
        }
      });
      hls.on(Hls.Events.ERROR, (_ev, data) => {
        if (data.fatal && fallbackPreviewId) {
          setUseFallbackMp4(true);
        }
      });
    } else if (videoEl.canPlayType("application/vnd.apple.mpegurl")) {
      videoEl.src = hlsUrl;
      videoEl.playbackRate = playbackRate;
      if (autoPlay) {
        videoEl.play().catch(() => {});
      }
    }

    return () => {
      if (hls) {
        hls.destroy();
      }
    };
  }, [hlsUrl, useFallbackMp4, fallbackPreviewId, autoPlay]);

  useEffect(() => {
    if (videoRef.current) {
      videoRef.current.playbackRate = playbackRate;
    }
  }, [playbackRate]);

  if (useFallbackMp4 && fallbackPreviewId) {
    return (
      <video
        ref={videoRef}
        src={mp4FallbackUrl}
        controls={controls}
        autoPlay={autoPlay}
        playsInline
        muted={muted}
        loop
        onTimeUpdate={(e) => onTimeUpdate?.(e.currentTarget.currentTime)}
        style={{ width: "100%", height: "100%", objectFit: "contain", background: "#000" }}
      />
    );
  }

  return (
    <video
      ref={videoRef}
      controls={controls}
      autoPlay={autoPlay}
      playsInline
      muted={muted}
      onTimeUpdate={(e) => onTimeUpdate?.(e.currentTarget.currentTime)}
      style={{ width: "100%", height: "100%", objectFit: "contain", background: "#000" }}
    />
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

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 300;
    const h = height;
    canvas.width = w * dpr;
    canvas.height = h * dpr;
    ctx.scale(dpr, dpr);

    ctx.clearRect(0, 0, w, h);

    // Subtle horizontal grid lines
    ctx.strokeStyle = "rgba(255, 255, 255, 0.05)";
    ctx.lineWidth = 1;
    for (const frac of [0.25, 0.5, 0.75]) {
      const y = Math.round(h * frac) + 0.5;
      ctx.beginPath();
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
      ctx.stroke();
    }

    if (values.length < 2) return;
    const peak = Math.max(maxVal || 0, ...values, 1);

    ctx.beginPath();
    values.forEach((v, idx) => {
      const x = (idx / (values.length - 1)) * (w - 4) + 2;
      const y = h - 4 - Math.min(1, Math.max(0, v / peak)) * (h - 10);
      if (idx === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.75;
    ctx.stroke();

    // Fill area under curve
    ctx.lineTo(w - 2, h - 2);
    ctx.lineTo(2, h - 2);
    ctx.closePath();
    ctx.fillStyle = `${color}22`;
    ctx.fill();
  }, [values, color, maxVal, height]);

  return (
    <canvas
      ref={canvasRef}
      style={{ width: "100%", height: `${height}px`, display: "block" }}
    />
  );
};
