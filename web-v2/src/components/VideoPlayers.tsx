import React, { useCallback, useEffect, useRef, useState } from "react";
import Hls from "hls.js";
import { CameraConfig } from "../types/nvr";
import { useNvrStore } from "../store/useNvrStore";
import { formatRelTime, formatTs } from "../utils/format";

interface ZoomableVideoStageProps {
  disabled?: boolean;
  children: React.ReactNode;
}

export const ZoomableVideoStage: React.FC<ZoomableVideoStageProps> = ({
  disabled = false,
  children,
}) => {
  const stageRef = useRef<HTMLDivElement | null>(null);
  const [scale, setScale] = useState<number>(1.0);
  const [pan, setPan] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  const [isDragging, setIsDragging] = useState<boolean>(false);
  const dragOriginRef = useRef<{
    startX: number;
    startY: number;
    originX: number;
    originY: number;
  } | null>(null);

  const clampPan = useCallback(
    (nextScale: number, rawX: number, rawY: number) => {
      if (nextScale <= 1.0) return { x: 0, y: 0 };
      const el = stageRef.current;
      const w = el?.clientWidth || 640;
      const h = el?.clientHeight || 360;
      const maxX = ((nextScale - 1) * w) / 2;
      const maxY = ((nextScale - 1) * h) / 2;
      return {
        x: Math.max(-maxX, Math.min(maxX, rawX)),
        y: Math.max(-maxY, Math.min(maxY, rawY)),
      };
    },
    []
  );

  const applyZoom = useCallback(
    (nextRawScale: number, anchorX?: number, anchorY?: number) => {
      const nextScale = Math.round(Math.max(1.0, Math.min(5.0, nextRawScale)) * 100) / 100;
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
        setIsDragging(false);
        dragOriginRef.current = null;
      }}
      onPointerCancel={() => {
        setIsDragging(false);
        dragOriginRef.current = null;
      }}
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
            zIndex: 12,
            display: "inline-flex",
            alignItems: "center",
            gap: 3,
            background: "rgba(9, 13, 20, 0.82)",
            border: "1px solid var(--border-strong)",
            borderRadius: 3,
            padding: "2px 4px",
            fontSize: "var(--text-xs)",
            fontVariantNumeric: "tabular-nums",
          }}
        >
          <button
            type="button"
            className="nvr-btn mono"
            style={{ height: 18, padding: "0 5px", fontSize: "var(--text-xs)" }}
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
              height: 18,
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
            style={{ height: 18, padding: "0 5px", fontSize: "var(--text-xs)" }}
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
}

/**
 * Safely extracts an array of polygon coordinate strings from Frigate's `motion.mask`
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

/**
 * Converts a polygon coordinate string into normalized [x, y] vertex tuples (0..1) for interactive editing.
 */
export function coordStringToPoints(
  coordStr: string,
  detWidth: number,
  detHeight: number
): Array<[number, number]> {
  const nums = coordStr
    .split(",")
    .map((s) => parseFloat(s.trim()))
    .filter((n) => !Number.isNaN(n));
  const out: Array<[number, number]> = [];
  for (let i = 0; i + 1 < nums.length; i += 2) {
    let x = nums[i];
    let y = nums[i + 1];
    if (x > 1.0 || y > 1.0) {
      x = x / Math.max(1, detWidth);
      y = y / Math.max(1, detHeight);
    }
    out.push([Math.max(0, Math.min(1, x)), Math.max(0, Math.min(1, y))]);
  }
  return out;
}

interface ContainedBox {
  left: number;
  top: number;
  width: number;
  height: number;
}

function computeContainedBox(
  containerW: number,
  containerH: number,
  mediaW: number,
  mediaH: number
): ContainedBox {
  if (containerW <= 0 || containerH <= 0 || mediaW <= 0 || mediaH <= 0) {
    return { left: 0, top: 0, width: containerW, height: containerH };
  }
  const containerRatio = containerW / containerH;
  const mediaRatio = mediaW / mediaH;
  if (containerRatio > mediaRatio) {
    const height = containerH;
    const width = containerH * mediaRatio;
    return {
      left: (containerW - width) / 2,
      top: 0,
      width,
      height,
    };
  }
  const width = containerW;
  const height = containerW / mediaRatio;
  return {
    left: 0,
    top: (containerH - height) / 2,
    width,
    height,
  };
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
}) => {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const imgRef = useRef<HTMLImageElement | null>(null);
  const [mseReady, setMseReady] = useState(false);
  const [streamDisconnected, setStreamDisconnected] = useState(false);
  const [headerLastSeenTs, setHeaderLastSeenTs] = useState<number>(0);
  const [posterTick, setPosterTick] = useState(() => Date.now());
  const [overlayBox, setOverlayBox] = useState<ContainedBox | null>(null);
  const camStats = useNvrStore((s) => s.stats?.cameras?.[camera]);

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
    const v = videoRef.current;
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
  }, [detW, detH]);

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
    fetch(`/api/${encodeURIComponent(camera)}/latest.jpg`, {
      method: "HEAD",
      credentials: "include",
    })
      .then((res) => {
        const hdr = res.headers.get("x-last-frame-timestamp");
        if (hdr) {
          const parsed = Number(hdr);
          if (Number.isFinite(parsed) && parsed > 0) {
            setHeaderLastSeenTs(parsed);
          }
        }
      })
      .catch(() => {});
    const timer = window.setInterval(() => {
      setPosterTick(Date.now());
    }, 2000);
    return () => window.clearInterval(timer);
  }, [camera, mseReady]);

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
    setStreamDisconnected(false);
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
        if (!closed) {
          setStreamDisconnected(false);
        }
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

      ws.onerror = () => {
        if (!closed) {
          setMseReady(false);
          setStreamDisconnected(true);
        }
      };

      ws.onclose = () => {
        if (!closed) {
          setMseReady(false);
          setStreamDisconnected(true);
        }
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
        updateOverlayGeometry();
        videoEl.play().catch(() => {});
      }
    };
    videoEl.addEventListener("loadeddata", onLoadedData);
    videoEl.addEventListener("resize", updateOverlayGeometry);

    return () => {
      closed = true;
      videoEl.removeEventListener("loadeddata", onLoadedData);
      videoEl.removeEventListener("resize", updateOverlayGeometry);
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
  }, [camera, useMainStream, updateOverlayGeometry]);

  const masks = extractMaskPolygons(cameraConfig?.motion?.mask);
  const objectMasks = extractMaskPolygons(cameraConfig?.objects?.mask);
  const privacyMasks = extractMaskPolygons(cameraConfig?.privacy_mask);
  const zones = cameraConfig?.zones || {};
  const rotateDeg = cameraConfig?.live?.rotate || 0;
  const isCorridorRot = rotateDeg === 90 || rotateDeg === 270;
  const rotateTransform = rotateDeg
    ? `rotate(${rotateDeg}deg)${isCorridorRot ? " scale(0.5625)" : ""}`
    : undefined;

  const handleCaptureLiveStill = (e: React.MouseEvent) => {
    e.stopPropagation();
    const vid = videoRef.current;
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
        background: "#040507",
        cursor: onPlotPoint ? "crosshair" : onClick ? "pointer" : "default",
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
      <ZoomableVideoStage disabled={Boolean(onPlotPoint)}>
        <img
          ref={imgRef}
          className="nvr-cam-poster"
          src={`/api/${camera}/latest.jpg?height=360&cache=${posterTick}`}
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
          ref={videoRef}
          autoPlay
          playsInline
          muted={muted}
          style={{
            width: "100%",
            height: "100%",
            objectFit: "contain",
            transform: rotateTransform,
            opacity: mseReady ? 1 : 0,
            transition: "opacity 0.2s ease",
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
            {Object.entries(cameraConfig?.lines || {}).map(([lName, lCfg]) => {
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
                  fill="rgba(245, 158, 11, 0.25)"
                  stroke="#f59e0b"
                  strokeWidth="3"
                />
                {draftPoints.map(([x, y], idx) => (
                  <circle
                    key={idx}
                    cx={Math.round(x * 1000)}
                    cy={Math.round(y * 1000)}
                    r="7"
                    fill="#fbbf24"
                    stroke="#000"
                    strokeWidth="2"
                  />
                ))}
              </g>
            )}
          </svg>
        )}
      </ZoomableVideoStage>
      <button
        type="button"
        className="nvr-btn nvr-tile-action mono"
        style={{
          position: "absolute",
          top: 8,
          left: 8,
          zIndex: 14,
          height: 22,
          padding: "0 6px",
          fontSize: "var(--text-xs)",
          background: "rgba(9, 13, 20, 0.86)",
        }}
        title="Capture native-resolution PNG still frame"
        onClick={handleCaptureLiveStill}
      >
        Capture Frame
      </button>
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
  const vodRotateDeg = useNvrStore(
    (s) => s.config?.cameras?.[camera]?.live?.rotate || 0
  );
  const vodIsCorridorRot = vodRotateDeg === 90 || vodRotateDeg === 270;
  const vodRotateTransform = vodRotateDeg
    ? `rotate(${vodRotateDeg}deg)${vodIsCorridorRot ? " scale(0.5625)" : ""}`
    : undefined;
  const [useFallbackMp4, setUseFallbackMp4] = useState(false);
  const [userMuted, setUserMuted] = useState<boolean>(() => {
    if (typeof window !== "undefined") {
      const saved = window.sessionStorage?.getItem("ninjanvr_vod_muted");
      if (saved === "false") return false;
      if (saved === "true") return true;
    }
    return muted;
  });

  const safeStart = Math.floor(startTime);
  const safeEnd = Math.ceil(Math.max(endTime, startTime + 12));
  const hlsUrl = `/vod/${encodeURIComponent(camera)}/start/${safeStart}/end/${safeEnd}/index.m3u8`;
  const mp4FallbackUrl = fallbackPreviewId
    ? `/api/review/${encodeURIComponent(fallbackPreviewId)}/preview?format=mp4`
    : `/api/${encodeURIComponent(camera)}/latest.webp`;

  useEffect(() => {
    setUseFallbackMp4(false);
    endedFiredRef.current = false;
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

  const stepVodFrame = (dir: -1 | 1) => {
    const vid = videoRef.current;
    if (!vid) return;
    vid.pause();
    const frameDur = 1 / 15;
    vid.currentTime = Math.max(0, vid.currentTime + dir * frameDur);
  };

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement | null)?.tagName?.toLowerCase();
      if (tag === "input" || tag === "textarea" || tag === "select") return;
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

  const vodFrameControls = (
    <div
      style={{
        position: "absolute",
        top: 8,
        left: 8,
        zIndex: 14,
        display: "flex",
        alignItems: "center",
        gap: 4,
      }}
    >
      <button
        type="button"
        className="nvr-btn mono"
        style={{
          height: 22,
          padding: "0 6px",
          fontSize: "var(--text-xs)",
          background: "rgba(9, 13, 20, 0.88)",
        }}
        title="Step 1 frame backward (Hotkey: ,)"
        onClick={() => stepVodFrame(-1)}
      >
        &lt; 1f
      </button>
      <button
        type="button"
        className="nvr-btn mono"
        style={{
          height: 22,
          padding: "0 6px",
          fontSize: "var(--text-xs)",
          background: "rgba(9, 13, 20, 0.88)",
        }}
        title="Step 1 frame forward (Hotkey: .)"
        onClick={() => stepVodFrame(1)}
      >
        1f &gt;
      </button>
      <button
        type="button"
        className="nvr-btn mono"
        style={{
          height: 22,
          padding: "0 6px",
          fontSize: "var(--text-xs)",
          background: "rgba(9, 13, 20, 0.88)",
        }}
        title="Capture native-resolution PNG still frame"
        onClick={handleCaptureVodStill}
      >
        Capture Frame
      </button>
    </div>
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
      <ZoomableVideoStage>
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
            window.sessionStorage?.setItem("ninjanvr_vod_muted", String(next));
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
            background: "#000",
          }}
        />
        {vodFrameControls}
      </ZoomableVideoStage>
    );
  }

  return (
    <ZoomableVideoStage>
      <video
        ref={videoRef}
        controls={controls}
        autoPlay={autoPlay}
        playsInline
        muted={userMuted}
        onVolumeChange={(e) => {
          const next = e.currentTarget.muted;
          setUserMuted(next);
          window.sessionStorage?.setItem("ninjanvr_vod_muted", String(next));
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
          background: "#000",
        }}
      />
      {vodFrameControls}
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
