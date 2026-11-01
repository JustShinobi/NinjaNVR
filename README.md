# NinjaNVR 🥷📹

**NinjaNVR** is an ultra-low-memory, high-performance AI Network Video Recorder (NVR) written in **Rust**, engineered as a drop-in, zero-bloat replacement for [Frigate NVR](https://github.com/blakeblackshear/frigate).

It preserves **100% UI/UX, configuration (`config.yml`), SQLite schema (`frigate.db`), and Home Assistant MQTT compatibility** with Frigate `0.18.x`, while replacing Frigate's ~35 Python/FFmpeg/Nginx processes (`>2.3 GiB` RAM for 4 cameras) with a **single Rust binary + `go2rtc` (`<100 MiB` RAM for 4 cameras)**.

---

## ✨ Why NinjaNVR?

Traditional Python-based NVRs work around the Python GIL by spawning dozens of OS processes (`multiprocessing.forkserver`, `ZeroMQ` brokers, `/dev/shm` POSIX shared memory buffers, and 3–4 `ffmpeg` subprocesses per camera), resulting in massive memory footprints and constant `/proc` + `ffprobe` polling.

**NinjaNVR** re-architects the entire NVR pipeline in safe, concurrent Rust:

| Subsystem | Legacy Python NVR (Frigate) | **NinjaNVR (Rust)** |
| :--- | :--- | :--- |
| **Process Model** | ~17 Python processes + ~13 FFmpeg processes + 6 Nginx workers | **1 Rust binary (`ninjanvr`) + `go2rtc`** |
| **Memory Footprint (4 Cameras)** | `~2,200 – 2,500 MiB` RSS + `1.5 GiB` `/dev/shm` & `/tmp/cache` | **`~65 – 95 MiB` RSS total** (`-96%` RAM) |
| **Frame Pipeline** | `ffmpeg stdout` ➔ `pipe` ➔ `/dev/shm` ➔ `ZeroMQ` | **Zero-copy `Arc<YuvFrame>`** in-process ring pool |
| **24/7 & Event Recording** | `ffmpeg -f segment` to `/tmp/cache` + `/proc` scan + `ffprobe` every 10s | **Direct fMP4 writer & in-memory GOP ring buffer** (Zero `/tmp/cache`, zero `ffprobe`) |
| **Timeline VOD Playback** | Custom C Nginx (`ngx_http_vod_module`) + subrequests | **Native Rust HLS/fMP4 byte-range streamer (`ninjanvr-vod`)** |
| **Motion Detection** | OpenCV (`libopencv` ~400 MB) + NumPy array copies | **Zero-OpenCV Y-plane SIMD engine** (`<0.15 ms` per frame, O(1) histogram contrast + Connected Components Labeling) |
| **AI Object Detection** | 14 vendor-specific Python plugins + IPC queues | **Unified ONNX Runtime / OpenVINO engine (`ninjanvr-infer`)** supporting `SSDLite MobileNetV2`, `YOLOv10`, and `YOLO11` |
| **Multi-Object Tracking** | Python `norfair` | **Native Rust `ByteTrack` (8-D Kalman Filter + 2-stage IoU matching)** + Stationary Object Classifier |
| **Live View Streaming** | Legacy `jsmpeg` (`mpeg1video` re-encode) + `go2rtc` | **Pure WebRTC, MSE (fMP4), and HLS via `go2rtc`** (Zero idle re-encoding) |

---

## 🏗️ Workspace Architecture

```text
ninjanvr/
├── Cargo.toml                     # Cargo Workspace
├── crates/
│   ├── ninjanvr-config/           # Frigate-compatible config.yml parser, defaults & go2rtc supervisor
│   ├── ninjanvr-db/               # SQLite WAL engine (100% compatible with frigate.db schema)
│   ├── ninjanvr-media/            # RTSP ingestion, GOP ring buffer, fMP4 muxer & substream frame pool
│   ├── ninjanvr-vision/           # Zero-OpenCV Y-plane SIMD motion detector, region cropper & WebP snapshots
│   ├── ninjanvr-infer/            # AI object detection engine (OpenVINO / ONNX Runtime: MobileNet & YOLOv10/11)
│   ├── ninjanvr-track/            # ByteTrack multi-object tracker & stationary object heuristics
│   ├── ninjanvr-vod/              # Native HLS (.m3u8) & fragmented MP4 (moof+mdat) VOD server
│   ├── ninjanvr-comms/            # Real-time WebSocket (/ws) state hub & Home Assistant MQTT publisher
│   └── ninjanvr-server/           # Main binary: Axum HTTP (:5000) & HTTPS (:8971) server + go2rtc proxy
└── web/                           # Pre-built React SPA frontend (100% Frigate 0.18 UI/UX compatible)
```

---

## 🚀 Quick Start

### 1. Run with Docker / Kubernetes

NinjaNVR is a drop-in replacement for the Frigate container image. It mounts the exact same volumes (`/config` and `/media/frigate`) and exposes the same ports (`5000` internal HTTP, `8971` authenticated HTTPS, `8554` RTSP restream, `8555` WebRTC):

```bash
docker run -d \
  --name ninjanvr \
  --restart unless-stopped \
  -p 5000:5000 \
  -p 8971:8971 \
  -p 8554:8554 \
  -p 8555:8555/tcp \
  -p 8555:8555/udp \
  -v /path/to/config:/config \
  -v /path/to/storage:/media/frigate \
  ghcr.io/justshinobi/ninjanvr:latest
```

### 2. Build from Source

```bash
git clone https://github.com/JustShinobi/NinjaNVR.git
cd NinjaNVR
cargo build --release
./target/release/ninjanvr --config /config/config.yml
```

---

## 📄 License

Licensed under the [MIT License](LICENSE).
