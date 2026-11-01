# NinjaNVR 🥷📹

[![CI](https://github.com/JustShinobi/NinjaNVR/actions/workflows/ci.yml/badge.svg)](https://github.com/JustShinobi/NinjaNVR/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/JustShinobi/NinjaNVR?include_prereleases&label=release)](https://github.com/JustShinobi/NinjaNVR/releases)
[![Container](https://img.shields.io/badge/ghcr.io-justshinobi%2Fninjanvr-blue?logo=docker)](https://github.com/JustShinobi/NinjaNVR/pkgs/container/ninjanvr)
[![License: MIT](https://img.shields.io/badge/License-MIT-emerald.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](Cargo.toml)

**NinjaNVR** is an ultra-low-footprint, high-performance **AI Network Video Recorder (NVR)** and **Tactical Video-Wall Console** built in **Rust**, **React 19**, and **Tauri v2**.

Designed from the ground up for 24/7 security operators, edge servers, and homelab clusters, NinjaNVR delivers sub-second WebRTC/MSE live streaming, SIMD-accelerated motion detection, OpenVINO AI object detection, 2-stage ByteTrack multi-object tracking, and a native pure-Rust fMP4/HLS VOD engine—all inside a single Rust binary consuming **`~65–95 MiB` of RAM** across 4 cameras.

---

## ✨ Key Capabilities

### ⚡ Ultra-Low-Footprint Rust Engine
- **Single-Process Architecture:** Consolidates RTSP ingestion supervision, motion analysis, AI inference, multi-object tracking, SQLite WAL persistence, fMP4 VOD remuxing, and HTTPS/WebSocket serving into a single `ninjanvr` binary alongside `go2rtc`.
- **Zero-Copy Frame Pipeline:** Shares decoded YUV420p frames across motion, inference, and snapshot workers via `Arc<YuvFrame>` ring buffers with zero `/dev/shm` IPC overhead.
- **Direct MP4 Recording & Pure-Rust VOD (`ninjanvr-vod`):** Writes 10-second MP4 segments directly to disk without `/tmp/cache` staging or `ffprobe` polling, and remuxes arbitrary timeline ranges on the fly into ISO-BMFF fragmented MP4 (`init-v1-a1.mp4` + `seg-N-v1-a1.m4s`) for instant HLS/MSE scrubbing.

### 🧠 Native Vision & AI Pipeline
- **Zero-OpenCV SIMD Motion Engine (`ninjanvr-vision`):** Planar YUV420p motion detector with 3×3 Gaussian blur, EMA contrast capping, and Connected Components Labeling (CCL) executing in `< 0.15 ms` per frame.
- **OpenVINO FFI Inference (`ninjanvr-infer`):** Dynamic `libopenvino_c.so` integration (`INFERENCE_NUM_THREADS=1`) supporting `SSDLite MobileNetV2`, `YOLOv10`, and `YOLO11` with configurable NMS and per-label thresholds.
- **2-Stage ByteTrack (`ninjanvr-track`):** 8-D Kalman filter multi-object tracker with high/low confidence association, stationary object classification, and polygon zone entry hysteresis.

### 🖥️ NinjaNVR Console (`v2`) & Tauri v2 Desktop
- **Tactical Operator Workspace (`web-v2`):** Low-luminance, high-contrast dark interface engineered for continuous CCTV monitoring with full keyboard navigation (`1–7` workspaces, `J/K` event stepping, `R` mark reviewed, `E` instant export).
- **Synchronized Multi-Camera Playback (`4-Cam Sync`):** Scrub, zoom (`Ctrl+Wheel`), and pan across 24-hour activity timelines with multi-camera lockstep playback, calendar activity indicators, and hover storyboard previews.
- **Triage & Instant MP4 Export:** Filter alerts vs. detections in the `Review Queue`, inspect AI bounding-box snapshots in `Explore`, draw interactive polygon masks/zones, and remux MP4 clips with custom `IN`/`OUT` markers in seconds.
- **Native Desktop Shell (`ninjanvr-desktop`):** Cross-platform **Tauri v2** desktop application packaging the NinjaNVR Console for dedicated operator workstations and multi-monitor video walls.

---

## 📊 Architectural Profile

| Subsystem | Traditional Multi-Process NVR | **NinjaNVR (`v0.9+`)** |
| :--- | :--- | :--- |
| **Runtime Model** | ~30+ Python, FFmpeg, ZeroMQ & Nginx OS processes | **1 Rust binary (`ninjanvr`) + `go2rtc`** |
| **Memory Footprint (4 Cameras)** | `~2,200 – 2,500 MiB` RSS + `1.5 GiB` `/dev/shm` | **`~65 – 95 MiB` RSS total** (`-96%` RAM) |
| **Frame Pipeline** | `ffmpeg stdout` ➔ `pipe` ➔ `/dev/shm` ➔ IPC | **In-process `Arc<YuvFrame>`** ring pool |
| **24/7 & Event Recording** | `/tmp/cache` segmenting + periodic `ffprobe` scans | **Direct MP4 writer & pure-Rust `mdhd` parser** |
| **Timeline VOD Playback** | External C Nginx VOD module + subrequests | **Native Rust ISO-BMFF fMP4/HLS streamer (`ninjanvr-vod`)** |
| **Motion Detection** | `libopencv` + NumPy array copies | **Zero-OpenCV Y-plane SIMD engine** (`< 0.15 ms/frame`) |
| **Multi-Object Tracking** | Python object trackers | **Native Rust 2-stage `ByteTrack` + Stationary Classifier** |
| **Operator Client** | Browser-only SPA | **NinjaNVR Console (`v2`) Web SPA + Tauri v2 Native Desktop** |

---

## 🏗️ Workspace Architecture

NinjaNVR is organized as a modular 10-crate Rust Cargo workspace paired with the React 19 Console (`web-v2`):

```text
NinjaNVR/
├── Cargo.toml                     # Cargo Workspace (Edition 2024)
├── crates/
│   ├── ninjanvr-config/           # YAML configuration parser, credential scrubber & go2rtc supervisor
│   ├── ninjanvr-db/               # SQLite WAL persistence & PBKDF2-HMAC-SHA256 authentication
│   ├── ninjanvr-media/            # Direct 10s MP4 segment recorder, retention manager & clip exporter
│   ├── ninjanvr-vision/           # Planar YUV420p SIMD motion detector, CCL & WebP/JPEG encoders
│   ├── ninjanvr-infer/            # Dynamic OpenVINO C FFI AI detector (SSDLite MobileNetV2 & YOLOv10/11)
│   ├── ninjanvr-track/            # 2-stage ByteTracker with stationary classification & zone hysteresis
│   ├── ninjanvr-vod/              # Pure-Rust ISO-BMFF fMP4 HLS VOD remuxer (init.mp4 + seg.m4s)
│   ├── ninjanvr-comms/            # Real-time WebSocket (/ws) state hub & MQTT publisher
│   ├── ninjanvr-server/           # Axum HTTP (:5000) & TLS (:8971) server, JWT auth & UI router
│   └── ninjanvr-desktop/          # Tauri v2 native desktop & video-wall shell
├── web-v2/                        # NinjaNVR Console (React 19 + TypeScript + Vite SPA)
└── web/                           # Classic UI (v1) compatibility bundle
```

---

## 🚀 Quick Start

### 1. Run with Docker

Published container images are automatically built and pushed to GitHub Container Registry (`ghcr.io/justshinobi/ninjanvr`) on every release:

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

- **Ports:**
  - `8971/tcp`: Authenticated HTTPS Console (NinjaNVR Console `v2` by default)
  - `5000/tcp`: Internal HTTP API & healthcheck endpoint (`/api/version`)
  - `8554/tcp`: RTSP restream (`go2rtc`)
  - `8555/tcp` & `8555/udp`: WebRTC media transport

### 2. Build from Source (Server & Web Console)

**Prerequisites:** Rust `1.88+` (2024 Edition), [Bun](https://bun.sh/) `1.1+`, `pkg-config`, `libssl-dev`, `libsqlite3-dev`, `clang`.

```bash
git clone https://github.com/JustShinobi/NinjaNVR.git
cd NinjaNVR

# 1. Build NinjaNVR Console (web-v2)
bun install --cwd web-v2 --frozen-lockfile
make build-ui

# 2. Build release binary
cargo build --release --bin ninjanvr

# 3. Run server
./target/release/ninjanvr --config ./config/config.example.yml
```

### 3. Build Native Desktop Client (Tauri v2)

```bash
cargo build --release -p ninjanvr-desktop
```

---

## 🔄 Ecosystem & Migration Compatibility

While **NinjaNVR** is an independent platform with its own backend architecture and Console UI (`v2`), it maintains first-class interoperability for users migrating from existing smart-home setups:

- **Configuration & Storage Drop-In:** Reads standard `config.yml` camera/detector definitions and existing SQLite databases (`recordings`, `event`, `reviewsegment`, `export`, `previews`, `user`) without destructive schema changes.
- **Home Assistant & MQTT:** Publishes real-time event, motion, and telemetry payloads over MQTT (`ninjanvr-comms`) compatible with existing Home Assistant automations.
- **Dual-UI Switcher:** Serves **NinjaNVR Console (`v2`)** by default, while retaining the Classic UI (`v1`) accessible on demand via `/api/ui/switch?target=v1` during migration.

---

## 🤝 Contributing & Development

Contributions, bug reports, and feature requests are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) before submitting a pull request.

Run the full verification suite locally before opening a PR:

```bash
make verify
```

This executes privacy guardrails, `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test --workspace`, and `web-v2` TypeScript type-checking + production bundling.

---

## 🙏 Acknowledgments

- **[go2rtc](https://github.com/AlexxIT/go2rtc)** by AlexxIT for rock-solid RTSP/WebRTC/MSE stream handling.
- **[OpenVINO™ Toolkit](https://github.com/openvinotoolkit/openvino)** for low-latency CPU/iGPU neural network inference.
- **[Frigate NVR](https://github.com/blakeblackshear/frigate)** for inspiring the initial configuration schema, smart-home MQTT conventions, and classic UI compatibility layer from which NinjaNVR evolved.

---

## 📄 License

Licensed under the [MIT License](LICENSE).
