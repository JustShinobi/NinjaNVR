# <img src="web-v2/public/favicon.svg" width="32" height="32" align="top" alt="NinjaNVR Icon" /> NinjaNVR

[![CI](https://github.com/JustShinobi/NinjaNVR/actions/workflows/ci.yml/badge.svg)](https://github.com/JustShinobi/NinjaNVR/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/JustShinobi/NinjaNVR?include_prereleases&label=release)](https://github.com/JustShinobi/NinjaNVR/releases)
[![Container](https://img.shields.io/badge/ghcr.io-justshinobi%2Fninjanvr-blue?logo=docker)](https://github.com/JustShinobi/NinjaNVR/pkgs/container/ninjanvr)
[![License: MIT](https://img.shields.io/badge/License-MIT-emerald.svg)](LICENSE)

NinjaNVR is a network video recorder and operator console written in Rust (Edition 2024) and React 19 (`web-v2`).

It runs the entire NVR pipeline inside a single `ninjanvr` binary paired with `go2rtc` (`1.9.15`) and `FFmpeg` (`8.1.2`): RTSP supervision, planar YUV420p SIMD motion detection, OpenVINO (`2026.4.1`) C-API object detection, ByteTrack multi-object tracking, SQLite WAL persistence, on-the-fly ISO-BMFF fMP4/HLS remuxing, and an Axum HTTP/TLS server. On a 4-camera setup (1080p/4K record + 720p detect at 5 FPS), total resident memory stays between 65 MiB and 95 MiB with ~3–4% CPU usage on a modern x86_64 core.

## Architecture

Decoded substream frames live in an in-process `Arc<YuvFrame>` ring pool rather than `/dev/shm` POSIX shared memory or Python IPC queues. Continuous and event recordings are written directly to 10-second MP4 segments on disk without `/tmp/cache` staging or periodic `ffprobe` subprocesses.

| Component | Implementation |
| :--- | :--- |
| Process model | Single `ninjanvr` binary + `go2rtc 1.9.15` + `FFmpeg 8.1.2` |
| Memory footprint (4 cameras) | 65 to 95 MiB RSS (~3.8% full-system CPU at 20 FPS aggregate detect) |
| Frame transport | In-process `Arc<YuvFrame>` ring buffer |
| Recording | Direct 10s MP4 segment writer (`-c:v copy`) + native Rust `mdhd` duration parser |
| Timeline VOD | Native Rust ISO-BMFF fMP4 (`init-v1-a1.mp4` + `seg-N-v1-a1.m4s`) and HLS streamer |
| Motion detection | Planar YUV420p SIMD engine (`< 0.15 ms/frame`), 3x3 Gaussian blur, EMA contrast cap, CCL |
| Object detection | `libopenvino_c.so` (`2026.4.1`) C FFI (`SSDLite MobileNetV2`, `YOLOv5/v8/v10/v11`, `YOLOX`) |
| Object tracking | 2-stage `ByteTrack` (8-D Kalman filter), stationary heuristics, polygon zones, directional tripwires |
| Client | Web SPA (`web-v2`, React 19 + TypeScript + Vite) |

### Workspace Layout

```text
crates/
  ninjanvr-config/    YAML configuration parser, credential scrubber, go2rtc supervisor
  ninjanvr-db/        SQLite WAL storage, RBAC, API tokens, and PBKDF2-HMAC-SHA256 auth
  ninjanvr-media/     10s MP4 segment recorder, tiered retention, quota cleanup, MP4 clip exporter
  ninjanvr-vision/    Planar YUV420p SIMD motion detector, IR/tamper checks, privacy masks, WebP/JPEG encoders
  ninjanvr-infer/     OpenVINO C FFI inference engine, multi-detector pool, and NMS decoder
  ninjanvr-track/     ByteTrack tracker, stationary classifier, polygon zones, and line-crossing tripwires
  ninjanvr-vod/       ISO-BMFF fMP4 HLS VOD remuxer with audio fallback and hybrid stream support
  ninjanvr-comms/     WebSocket (/ws) state hub and MQTT/MQTTS publisher
  ninjanvr-server/    Axum HTTP (:5000) and TLS (:8971) server, OIDC/JWT auth gate, and REST/MCP APIs
web-v2/               NinjaNVR Console (React 19 + TypeScript + Vite)
```

## Hardware Compatibility

If you are evaluating NinjaNVR for existing NVR hardware, check the support matrix below before migrating.

### 1. Host CPU Architecture
* **`x86_64` (`linux/amd64`)**: Officially supported and published to `ghcr.io/justshinobi/ninjanvr`.
* **`ARM64` (`aarch64` / Raspberry Pi / Rockchip)**: No prebuilt container images are published today. The Rust workspace compiles on ARM64 Linux from source, but the bundled OpenVINO runtime in the container targets `x86_64`.

### 2. AI Inference Accelerators (`ninjanvr-infer`)
NinjaNVR uses the **Intel OpenVINO C Runtime (`2026.4.1`)** exclusively for AI inference (`.xml`/`.bin` IR and `.onnx` models):

| Hardware / Accelerator | Status | Notes |
| :--- | :--- | :--- |
| **x86_64 CPU (Intel & AMD)** | **Supported (Default)** | Uses `libopenvino_intel_cpu_plugin.so` (AVX2 / AVX-512 / VNNI). Because SIMD motion detection gates inference, 4 cameras at 5 FPS typically use < 4% CPU without a discrete accelerator. |
| **Intel iGPU / Arc (`device: GPU`)** | **Experimental** | `libopenvino_intel_gpu_plugin.so` is bundled. Requires passing `/dev/dri` into the container and host/container OpenCL ICD runtime drivers. Automatically falls back to `CPU` if initialization fails. |
| **Intel Core Ultra NPU (`device: NPU`)** | **Experimental** | `libopenvino_intel_npu_plugin.so` is bundled. Requires passing `/dev/accel` into the container and Linux NPU driver support. Automatically falls back to `CPU` if unavailable. |
| **Google Coral EdgeTPU (USB / PCIe)** | **Not Supported** | Coral `libedgetpu` / TFLite models are not used. Switch `detectors:` in `config.yml` to `type: openvino` (`device: CPU`). |
| **Hailo-8 / Hailo-8L** | **Not Supported** | HailoRT is not integrated. |
| **NVIDIA GPU (CUDA / TensorRT)** | **Not Supported** | ONNX Runtime CUDA / TensorRT execution providers are not bundled in the runtime image. |

### 3. Video Decoding, Recording & Cameras
* **Recording (`record` role)**: Uses FFmpeg stream copy (`-c:v copy`) directly to 10-second MP4 segments on disk. H.264 and H.265 main streams (1080p/4K) add negligible CPU load.
* **Substream decoding (`detect` role)**: Decodes low-resolution camera substreams (e.g., `640x360` or `1280x720` at `5 FPS`) into raw `yuv420p` frames. Custom `hwaccel_args` in `config.yml` are passed through to FFmpeg when configured.
* **Camera protocols**: Any stream supported by `go2rtc 1.9.15` and `FFmpeg 8.1.2` (RTSP, RTSPS, ONVIF, RTMP, HTTP-FLV, MJPEG).

## Capabilities

### Vision, AI & Audio Pipeline
* **Multi-Detector Pool**: Route different cameras to separate OpenVINO detector instances (`SSDLite MobileNetV2`, `YOLOv5`, `YOLOv8`, `YOLOv10`, `YOLO11`, `YOLOX`) with per-camera model assignment.
* **Zones & Directional Tripwires**: Polygon zones with entry inertia, loitering timers, resolution-agnostic normalized area bounds, and directional A↔B virtual tripwires.
* **Night IR & Tamper Guards**: Automatic monochrome IR detection profile, transient spider/insect flash filter, and camera blind-feed/tamper alerts.
* **High-Res Snapshots & Privacy Masks**: Projects normalized sub-stream bounding boxes onto main-stream frames for high-resolution snapshots and burns permanent black/pixelated privacy masks into live views, snapshots, and exports.
* **Acoustic Detection**: Segment RMS/dBFS calculation, timeline audio waveform track, and dBFS threshold event triggers.

### Storage & Resilience
* **Direct MP4 Recording & Native VOD**: 10-second MP4 segments remuxed on the fly into fMP4 HLS (`master.m3u8`) in pure Rust, resilient to missing or corrupted segment audio tracks.
* **Hybrid Dual-Stream & Tiered Storage**: Continuous 24/7 substream recording with main-stream promotion on active events, plus automatic hot-to-cold storage migration (NVMe/SSD to HDD/NAS).
* **Quota & Degraded Mount Guards**: Retention pruning by age, per-camera/global disk quota (`max_size_gb`), or minimum free space (`min_free_gb`), storage runway estimation, real-time offsite alert clip backup, and degraded mount protection (keeps the Live Wall running if the recording disk drops offline).

### Web Console (`web-v2`), Security & Integrations
* **Tactical Operator Console**: Keyboard-driven triage (`1–7` workspaces, `J/K` event stepping, `R` mark reviewed, `E` export, `< / >` single-frame step), digital zoom/pan, per-camera rotation (`0°/90°/180°/270°`), and last-online snapshot persistence.
* **Live Matrix, Spotlight & Floorplan**: WebRTC/MSE live matrix, client-side Tactical Spotlight (zero-CPU dynamic Birdseye with bounding boxes), interactive Floorplan Map with live camera FOV cones, 4-camera lockstep VOD sync, and accelerated Daily Tactical Brief playback.
* **Forensic Triage**: "Explain Alert" trigger provenance inspector (exact score, area %, zone, and frame that promoted a track), boolean filter logic (`AND`/`OR`/`NOT`), saved filter presets, operator notes, and expiring HMAC-signed shareable clip links.
* **Authentication & Access Control**: Local PBKDF2-SHA256 login, native OpenID Connect (OIDC / Zitadel / Authelia) SSO, Trusted-Proxy ForwardAuth, RBAC (`admin`, `operator`, `viewer`) with per-camera scoping, and scoped API Bearer tokens (`nnvr_*`).
* **Observability & Federation**: Operational security profiles (`ARMED`, `HOME`, `DISARMED`), MQTT & MQTTS (TLS 1.2/1.3 + client mTLS), Prometheus `/api/metrics`, LLM/MCP context endpoints, authenticated camera Web-UI reverse proxy, and multi-node NVR federation.

## Usage

### Container (`ghcr.io`)

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
  -v /path/to/storage:/media/ninjanvr \
  ghcr.io/justshinobi/ninjanvr:latest
```

Exposed ports:
* `8971/tcp`: Authenticated HTTPS console and API
* `5000/tcp`: Internal HTTP API and healthcheck (`/api/version`)
* `8554/tcp`: RTSP restream (`go2rtc`)
* `8555/tcp`, `8555/udp`: WebRTC media

### Build from Source

Requires Rust `1.88+` (Edition 2024), Bun `1.1+`, `pkg-config`, `libssl-dev`, `libsqlite3-dev`, `cmake`, and `clang`.

```bash
git clone https://github.com/JustShinobi/NinjaNVR.git
cd NinjaNVR

bun install --cwd web-v2 --frozen-lockfile
make build

./target/release/ninjanvr --config ./config/config.example.yml
```

## Switching from Frigate

NinjaNVR was initially inspired by Frigate's YAML camera configuration style, but has evolved into an independent Rust + React 19 architecture. It is **not** a drop-in replacement. If you are migrating an existing setup, note the following differences:

1. **Storage & Database Paths**: Mount your storage volume at `/media/ninjanvr` (instead of `/media/frigate`). The default SQLite database path is `/config/ninjanvr.db`, the default session cookie is `ninjanvr_token`, and default MQTT topics use the `ninjanvr` prefix (configurable via `mqtt.topic_prefix`).
2. **Detector Configuration**: Replace Coral (`edgetpu`) or other external detector plugins in `config.yml` with `type: openvino` (`device: CPU`, `GPU`, or `NPU`). The default `SSDLite MobileNetV2` FP16 model is pre-installed at `/openvino-model/ssdlite_mobilenet_v2.xml`.
3. **Database & API Extensions**: While base `cameras:` and `go2rtc:` YAML blocks remain familiar for manual setup, NinjaNVR uses its own recording engine, extended SQLite tables, and console APIs. Dedicated migration tooling for importing legacy configurations may be introduced in future releases.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and commit conventions. Run `make verify` before opening a pull request.

## License

[MIT](LICENSE)
