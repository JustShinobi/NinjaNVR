# <img src="web-v2/public/favicon.svg" width="32" height="32" align="top" alt="NinjaNVR Icon" /> NinjaNVR

[![CI](https://github.com/JustShinobi/NinjaNVR/actions/workflows/ci.yml/badge.svg)](https://github.com/JustShinobi/NinjaNVR/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/JustShinobi/NinjaNVR?include_prereleases&label=release)](https://github.com/JustShinobi/NinjaNVR/releases)
[![Container](https://img.shields.io/badge/ghcr.io-justshinobi%2Fninjanvr-blue?logo=docker)](https://github.com/JustShinobi/NinjaNVR/pkgs/container/ninjanvr)
[![License: MIT](https://img.shields.io/badge/License-MIT-emerald.svg)](LICENSE)

NinjaNVR is a network video recorder and operator console written in Rust (Edition 2024) and React 19.

It runs the entire NVR pipeline inside a single `ninjanvr` process paired with `go2rtc`: RTSP stream supervision, planar YUV420p SIMD motion detection, OpenVINO object detection, ByteTrack multi-object tracking, SQLite WAL persistence, on-the-fly ISO-BMFF fMP4/HLS remuxing, and an Axum HTTP/TLS server. On a 4-camera setup (1080p/4K record + 720p detect), total resident memory stays between 65 MiB and 95 MiB.

## Architecture

Decoded substream frames live in an in-process `Arc<YuvFrame>` ring pool rather than `/dev/shm` POSIX shared memory or IPC queues. Continuous and event recordings are written directly to 10-second MP4 segments on disk without `/tmp/cache` staging or periodic `ffprobe` subprocesses.

| Component | Implementation |
| :--- | :--- |
| Process model | Single `ninjanvr` binary + `go2rtc` |
| Memory footprint (4 cameras) | 65 to 95 MiB RSS |
| Frame transport | In-process `Arc<YuvFrame>` ring buffer |
| Recording | Direct MP4 segment writer + native `mdhd` duration parser |
| Timeline VOD | Native Rust ISO-BMFF fMP4 (`init.mp4` + `seg.m4s`) and HLS streamer |
| Motion detection | Planar YUV420p SIMD engine (`< 0.15 ms/frame`), 3x3 Gaussian blur, EMA contrast cap, CCL |
| Object detection | `libopenvino_c.so` C FFI (`SSDLite MobileNetV2`, `YOLOv10`, `YOLO11`) |
| Object tracking | 2-stage `ByteTrack` (8-D Kalman filter) + stationary object heuristics + zone hysteresis |
| Client | Web SPA (`web-v2`, React 19 + TypeScript + Vite) |

### Workspace Layout

```text
crates/
  ninjanvr-config/    YAML configuration parser, credential scrubber, go2rtc supervisor
  ninjanvr-db/        SQLite WAL storage and PBKDF2-HMAC-SHA256 authentication
  ninjanvr-media/     10s MP4 segment recorder, retention cleanup, MP4 clip exporter
  ninjanvr-vision/    Planar YUV420p SIMD motion detector, CCL, WebP/JPEG encoders
  ninjanvr-infer/     OpenVINO C FFI inference engine and NMS decoder
  ninjanvr-track/     ByteTrack tracker, stationary object classifier, polygon zones
  ninjanvr-vod/       ISO-BMFF fMP4 HLS VOD remuxer
  ninjanvr-comms/     WebSocket (/ws) state hub and MQTT publisher
  ninjanvr-server/    Axum HTTP (:5000) and TLS (:8971) server
web-v2/               NinjaNVR Console (React 19 + TypeScript + Vite)
```

## Web Console (`web-v2`)

The default interface (`web-v2`) is built for dark-room monitoring and keyboard-driven triage (`1-7` view switching, `J/K` event stepping, `R` mark reviewed, `E` clip export):

- **Live Matrix and 4-Cam Sync:** Low-latency WebRTC/MSE live streams and lockstep multi-camera VOD playback.
- **24h Timeline:** Mouse-wheel frame scrubbing, `Ctrl+Wheel` zoom, pointer drag panning, multi-day activity calendar, and hover storyboard previews.
- **Review, Explore, and Exports:** Alert vs. detection filtering, AI bounding-box inspection, interactive polygon zone/mask editing, and direct MP4 range remuxing.

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
- `8971/tcp`: Authenticated HTTPS console and API
- `5000/tcp`: Internal HTTP API and healthcheck (`/api/version`)
- `8554/tcp`: RTSP restream (`go2rtc`)
- `8555/tcp`, `8555/udp`: WebRTC media

### Build from Source

Requires Rust `1.88+` (Edition 2024), Bun `1.1+`, `pkg-config`, `libssl-dev`, `libsqlite3-dev`, `cmake`, and `clang`.

```bash
git clone https://github.com/JustShinobi/NinjaNVR.git
cd NinjaNVR

bun install --cwd web-v2 --frozen-lockfile
make build

./target/release/ninjanvr --config ./config/config.example.yml
```

## Migration and Architecture

NinjaNVR was initially inspired by Frigate's YAML camera configuration style, but has evolved into an independent Rust + React 19 architecture with its own recording engine, database schema extensions, and AI pipeline. While basic camera and detector blocks remain familiar for manual setup, NinjaNVR is not a drop-in replacement; dedicated migration tooling for importing legacy configurations may be introduced in future releases.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup and commit conventions. Run `make verify` before opening a pull request.

## License

[MIT](LICENSE)
