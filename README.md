# <img src="web-v2/public/favicon.svg" width="32" height="32" align="top" alt="NinjaNVR Icon" /> NinjaNVR

**Next-gen AI Network Video Recorder and operator console written in Rust — with built-in face & plate recognition, interactive CAD floorplan, multi-camera sync playback, and a local AI training studio.**

[![CI](https://github.com/JustShinobi/NinjaNVR/actions/workflows/ci.yml/badge.svg)](https://github.com/JustShinobi/NinjaNVR/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/JustShinobi/NinjaNVR?include_prereleases&label=release)](https://github.com/JustShinobi/NinjaNVR/releases)
[![Container](https://img.shields.io/badge/ghcr.io-justshinobi%2Fninjanvr-blue?logo=docker)](https://github.com/JustShinobi/NinjaNVR/pkgs/container/ninjanvr)
[![License: MIT](https://img.shields.io/badge/License-MIT-emerald.svg)](LICENSE)

[Why NinjaNVR?](#why-ninjanvr) · [Key Features](#key-features) · [Console Showcase](#console-showcase) · [Quickstart](#quickstart) · [Hardware Support](#hardware--accelerator-support) · [Architecture](#under-the-hood-architecture)

<p align="center">
  <img src="docs/public/assets/hero-showcase.gif" alt="NinjaNVR Operator Console Showcase — Live Matrix, CAD Floorplan, Forensic Triage, and Local AI Dataset Studio" width="100%" />
</p>

**NinjaNVR** replaces sprawling multi-process Python scripts, gigabytes of `/dev/shm` shared memory, and paid cloud model subscriptions with a single native Rust engine and a modern React 19 operator console (`web-v2`).

On a standard 4-camera setup (1080p/4K continuous recording + 720p AI detection at 5 FPS), NinjaNVR uses **65–95 MiB of RAM** and **~3–4% of a modern x86_64 CPU**—with **no dedicated USB accelerator required**.

---

## Why NinjaNVR?

Most open-source NVRs were designed around multi-process IPC queues, temporary RAM disks, and generic 80-class academic datasets. NinjaNVR was engineered from the ground up to eliminate those operational bottlenecks:

| Capability | Traditional Python / IPC NVRs | NinjaNVR |
| :--- | :--- | :--- |
| **Runtime & Memory (4 Cams)** | Multi-process Python (`1.5 GiB – 4.0 GiB+` RAM) | **Single Rust binary (`65 MiB – 95 MiB` RAM)** |
| **Disk & Memory Staging** | Requires custom `/dev/shm` sizing and `/tmp/cache` SSD wear | **Zero `/dev/shm`**: in-process memory ring + direct 10s MP4 disk writes |
| **CPU Efficiency** | High idle load without dedicated USB/PCIe accelerators | **`< 4%` CPU on standard x86_64** (SIMD motion gates AI in `< 0.15 ms/frame`) |
| **AI Model Fine-Tuning** | Paid cloud subscription required to train or manage datasets | **100% Free & Local**: built-in Dataset Studio, auto-labeler, and hot-reload |
| **Detection Taxonomy** | Generic 80-class COCO (*giraffe, toothbrush, zebra*) | **Security-first 15-class taxonomy** (`person`, `face`, `car`, `license_plate`, `package`...) |
| **False-Positive Suppression** | Manual threshold tweaking and constant config restarts | **Perceptual `dhash64` memory** + optional **Vision AI Judge** (Ollama / OpenAI / Gemini) |
| **Live Reconfiguration** | Editing YAML requires full container restart (recording blackout) | **Runtime GUI & Model Hot-Reload** with zero recording downtime |
| **Enterprise & Home Auth** | Basic local user or external reverse-proxy workarounds | **Native OIDC / SSO**, per-camera RBAC, API tokens, and camera VLAN proxy |

---

## Console Showcase

| Live Matrix, Zones & State ROIs | Interactive CAD Floorplan & Live FOV Cones |
| :---: | :---: |
| [<img src="docs/public/assets/live-matrix.png" alt="Live Matrix & Vectors" width="100%" />](docs/public/assets/live-matrix.png) | [<img src="docs/public/assets/floorplan-cad.png" alt="Interactive CAD Floorplan Studio" width="100%" />](docs/public/assets/floorplan-cad.png) |
| **Forensic Review, Face/LPR & "Explain Alert"** | **Local AI Dataset Studio & Model Hot-Reload** |
| [<img src="docs/public/assets/review-forensic.png" alt="Forensic Review & Explain Alert" width="100%" />](docs/public/assets/review-forensic.png) | [<img src="docs/public/assets/ai-studio.png" alt="Active Learning AI Dataset Studio" width="100%" />](docs/public/assets/ai-studio.png) |

---

## Key Features

### 1. Operator Console & Multi-Camera Playback (`web-v2`)
* **Adaptive Live Matrix (Up to 100+ Cameras):** Low-latency WebRTC/MSE streaming, camera groups, per-camera rotation (`0°/90° Corridor/180°/270°`), interactive digital zoom/pan, **ONVIF PTZ HUD with Autotracking**, and **WebRTC Push-to-Talk** two-way audio.
* **Interactive CAD Floorplan Studio:** Draw metric floorplans (`m`) with walls, doors, and windows; position cameras with live directional FOV cones that pulse in real time during active detections.
* **Spotlight & 4-Cam Lockstep Sync:** Client-side dynamic "Birdseye" view that automatically promotes active cameras with zero server CPU transcoding, plus frame-synchronized 4-camera VOD playback.
* **60 FPS Canvas Timeline & Keyboard Triage:** High-DPI 24-hour scrubber with acoustic dBFS waveform tracks, offline gap indicators, hover previews, single-frame stepping (`< 1f / 1f >`), and keyboard shortcuts (`J/K` step, `R` mark reviewed, `E` export).
* **Daily Brief & Dual-Theme Engine:** Catch up on an entire night in seconds with accelerated sequential alert playback, rendered on calibrated **Dark**, **Daylight/Light**, and **High-Contrast** monitor surfaces.

### 2. Multi-Layer Security AI (100% Local)
* **Object Tracking & Directional Tripwires:** 2-stage `ByteTrack` Kalman tracking with polygon zone inertia, loitering timers, stationary object classification, and directional `A↔B` virtual line-crossing tripwires.
* **Local Face Recognition & LPR / ANPR:** Enroll known individuals into the local Face Gallery (`sub_labels` + unknown-person alerts) and recognize vehicle license plates with multi-frame voting and regex validation.
* **State ROIs (Open / Closed Monitoring):** Monitor fixed regions (garage doors, gates, loading bays) with hysteresis and automatic anomaly alerts when left open past a timeout.
* **"Explain Alert" Forensic Inspector:** Inspect the exact trigger frame, confidence score, bounding-box area %, aspect ratio, and zone rule that promoted a track.
* **False-Positive Memory & Vision AI Judge:** Dismiss a false alarm once to suppress matching spatial/perceptual `dhash64` signatures, or let a local/cloud VLM (`Ollama`, `Gemini`, `OpenAI`) verify ambiguous borderline detections before alerting.
* **Night IR, Spider Filter & Tamper Guard:** Automatic day/night IR profile switching, transient insect/spider-web flash suppression, OSD clock auto-masking, and blind-feed/tamper detection.

### 3. Built-In Dataset Studio & Model Factory (No Cloud Paywall)
* **One-Click Triage-to-Dataset:** Promote real events as *True Positives* or *Hard Negatives* directly from the Review queue.
* **AI Auto-Labeler & Bounding-Box Editor:** Annotate frames in the browser and curate balanced train/validation splits across day and night conditions.
* **Zero-Downtime Model Hot-Reload:** Upload or switch OpenVINO/ONNX models (`SSDLite`, `YOLOv5/v8/v10/v11`, `YOLOX`, or the 15-class NinjaNVR Security model) live without restarting the NVR.

### 4. Resilient Storage, Security & Integrations
* **Zero-Transcode Recording & Hybrid Streams:** Writes `-c:v copy` 10-second MP4 segments directly to disk. Supports continuous 24/7 low-bitrate substream recording with automatic main-stream promotion during active events.
* **Hot-to-Cold Tiering & Cold Compaction:** Automatically migrate older recordings from fast NVMe/SSD (`hot_dir`) to high-capacity HDD/NAS (`cold_dir`), with optional background H.264/HEVC re-compaction, disk quotas (`max_size_gb` / `min_free_gb`), and real-time offsite alert backup.
* **Degraded Mount Guard:** If a secondary recording disk or NAS drops offline, the Live Wall and AI detection stay online and recover automatically when the mount returns.
* **Home Assistant, MQTT/MQTTS & MCP Ready:** Full MQTT discovery (`ARMED`, `HOME`, `DISARMED` security profiles, live counts, and snapshots), Prometheus `/api/metrics`, Multi-Node NVR Federation, and native **MCP / LLM context endpoints** for AI assistants.
* **Zero-Trust Ready:** Local PBKDF2-SHA256 auth, **OpenID Connect (OIDC)** SSO, Trusted-Proxy ForwardAuth, per-camera RBAC, expiring HMAC-signed shareable clip links, burned-in privacy masks, and an authenticated reverse proxy for cameras on isolated VLANs.

---

## Quickstart

### Run with Docker (`ghcr.io`)

```bash
docker run -d \
  --name ninjanvr \
  --restart unless-stopped \
  -p 8971:8971 \
  -p 5000:5000 \
  -p 8554:8554 \
  -p 8555:8555/tcp \
  -p 8555:8555/udp \
  -v /path/to/config:/config \
  -v /path/to/storage:/media/ninjanvr \
  ghcr.io/justshinobi/ninjanvr:latest
```

| Port | Purpose |
| :--- | :--- |
| `8971/tcp` | **HTTPS Operator Console (`web-v2`)** and authenticated API |
| `5000/tcp` | Internal HTTP API & container healthcheck (`/api/version`) |
| `8554/tcp` | RTSP restreaming (`go2rtc`) |
| `8555/tcp+udp` | Low-latency WebRTC media & Push-to-Talk backchannel |

---

## Hardware & Accelerator Support

NinjaNVR ships with a **Dual-Backend Inference Engine**: **Intel OpenVINO (`2026.4.1`)** is bundled out of the box, and **ONNX Runtime (`libonnxruntime.so`)** can be dynamically loaded for NVIDIA/AMD GPUs.

| Hardware / Accelerator | Backend & Config | Status | Notes |
| :--- | :--- | :--- | :--- |
| **x86_64 CPU (Intel & AMD)** | `type: openvino`<br>`device: CPU` | **Supported (Default)** | Uses AVX2 / AVX-512 / VNNI. Because SIMD motion detection gates inference, 4 cameras at 5 FPS typically use `< 4%` CPU with no discrete accelerator. |
| **NVIDIA GPU (CUDA / TensorRT)** | `type: onnxruntime`<br>`device: CUDA` or `TensorRT` | **Supported** | Dynamically loads `libonnxruntime.so` with `/dev/nvidia0` passthrough; falls back to CPU automatically if unavailable. |
| **AMD GPU (ROCm)** | `type: onnxruntime`<br>`device: ROCM` | **Supported** | Dynamically loads `libonnxruntime.so` with `/dev/kfd` + `/dev/dri` passthrough; falls back to CPU automatically. |
| **Intel iGPU / Arc** | `type: openvino`<br>`device: GPU` | **Experimental** | Bundled plugin. Requires `/dev/dri` passthrough and OpenCL ICD runtime drivers. |
| **Intel Core Ultra NPU** | `type: openvino`<br>`device: NPU` | **Experimental** | Bundled plugin. Requires `/dev/accel` passthrough and Linux NPU driver support. |
| **ARM64 (`aarch64`)** | Source build | **Community** | Compiles from source on ARM64 Linux; prebuilt `ghcr.io` images currently target `x86_64` (`linux/amd64`). |

* **Any RTSP / ONVIF Camera:** Supports H.264 and H.265 cameras via RTSP, RTSPS, ONVIF, RTMP, HTTP-FLV, and MJPEG (`go2rtc 1.9.15` + `FFmpeg 8.1.2`).

---

## Under the Hood: Architecture

For engineers evaluating how NinjaNVR achieves its resource profile:

* **In-Process Zero-Copy Frame Ring (`Arc<YuvFrame>`):** Decoded substream frames remain in process memory instead of being serialized across `/dev/shm` POSIX shared memory or Python multiprocessing pipes.
* **Sub-Millisecond SIMD Motion Gate (`< 0.15 ms/frame`):** A custom planar YUV420p engine applies a 3×3 Gaussian blur, EMA contrast cap, and Connected-Component Labeling (CCL) to wake the neural detector only on relevant motion contours.
* **Pure-Rust ISO-BMFF fMP4 & HLS VOD Engine:** Continuous 10-second MP4 segments are remuxed on the fly into fragmented MP4 (`init-v1-a1.mp4` + `seg-N-v1-a1.m4s`) and `master.m3u8` streams directly in Rust—without spawning per-viewer FFmpeg transcoders or `ffprobe` subprocesses.

### Workspace Layout

```text
crates/
  ninjanvr-config/    YAML configuration parser, credential scrubber, go2rtc supervisor
  ninjanvr-db/        SQLite WAL storage, RBAC, false-positive signatures, and PBKDF2-SHA256 auth
  ninjanvr-media/     10s MP4 segment recorder, hot/cold tiering, cold compaction, MP4 clip exporter
  ninjanvr-vision/    Planar YUV420p SIMD motion detector, IR/tamper checks, State ROIs, privacy masks
  ninjanvr-infer/     Dual-backend OpenVINO + ONNX Runtime C FFI engine, Face/LPR & multi-arch YOLO decoders
  ninjanvr-track/     2-stage ByteTrack Kalman tracker, stationary heuristics, polygon zones, tripwires
  ninjanvr-vod/       Pure-Rust ISO-BMFF fMP4 HLS VOD remuxer with VBR & hybrid-stream discontinuity support
  ninjanvr-comms/     Real-time WebSocket (/ws) state hub and MQTT / MQTTS (TLS + mTLS) publisher
  ninjanvr-server/    Axum HTTP (:5000) & TLS (:8971) server, OIDC/JWT gate, GenAI judge, Federation & MCP APIs
web-v2/               NinjaNVR Console (React 19 + TypeScript + Vite)
```

### Migrating Existing YAML Configurations

NinjaNVR maintains backwards-compatible parsing for standard `cameras:` and `go2rtc:` YAML blocks so you can migrate existing camera definitions quickly:

1. **Storage & Database Paths:** Mount your recording volume at `/media/ninjanvr` (SQLite database defaults to `/config/ninjanvr.db`; MQTT topic prefix defaults to `ninjanvr`).
2. **Detector Config:** Set your detector in `config.yml` to `type: openvino` (`device: CPU`, `GPU`, or `NPU`) or `type: onnxruntime` (`device: CUDA`, `TensorRT`, or `ROCM`). The default `SSDLite MobileNetV2` FP16 model is pre-installed at `/openvino-model/ssdlite_mobilenet_v2.xml`.

### Build from Source

Requires Rust `1.88+` (Edition 2024), Bun `1.1+`, `pkg-config`, `libssl-dev`, `libsqlite3-dev`, `cmake`, and `clang`.

```bash
git clone https://github.com/JustShinobi/NinjaNVR.git
cd NinjaNVR

bun install --cwd web-v2 --frozen-lockfile
make build

./target/release/ninjanvr --config ./config/config.example.yml
```

---

## Contributing & License

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and commit conventions. Run `make verify` before opening a pull request.

Licensed under the [MIT License](LICENSE).
