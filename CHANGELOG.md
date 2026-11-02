# Changelog

All notable changes to **NinjaNVR** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.1] - 2026-11-01

### Documentation & Maintenance
- ci(release): automate continuous SemVer tagging, changelog, GHCR build, and GitOps bump on main (`aabff8f`)
- docs(agents): remove CLAUDE.md and consolidate repository guidelines in AGENTS.md (`9236b9c`)

## [0.9.0] - 2026-11-01

### Features
- **NinjaNVR Console (`v2`):** Promote the React 19 + TypeScript tactical operator console (`web-v2`) to the default UI across all deployments, featuring Live Matrix, 4-Cam Synchronized Playback, Review Queue triage, Explore, Instant MP4 Exports, Polygon Zone Editor, and Real-Time System Telemetry.
- **Interactive 24h Timeline & Calendar:** Add multi-day heatmap navigator, `RecordingCalendarPopover` with event indicators, mouse-wheel frame/time scrubbing, `Ctrl+Wheel` zoom, pointer drag panning, and hover storyboard previews.
- **Pure-Rust NVR Engine:** Complete 9-crate workspace architecture (`ninjanvr-config`, `ninjanvr-db`, `ninjanvr-media`, `ninjanvr-vision`, `ninjanvr-infer`, `ninjanvr-track`, `ninjanvr-vod`, `ninjanvr-comms`, `ninjanvr-server`) running in `<100 MiB` RAM for 4 cameras.
- **Open-Source CI/CD & Release Automation:** Add multi-stage Docker build (Bun UI + Rust server), automated `release-please` semantic versioning and changelog generation, GHCR container publishing, and GitOps deployment promotion.

### Bug Fixes
- **Timeline:** Clamp mouse wheel, touch drag, and pan strictly within selected day bounds and accumulate rapid multi-notch wheel ticks.
- **Database:** Use exact earliest segment `start_ts` per hour in recordings summary to eliminate false retention gaps.
- **Auth:** Gate WebSocket and polling requests until authentication state is verified and improve login form accessibility.
- **Design & Accessibility:** Bundle `JetBrains Mono` locally in WOFF2, enforce WCAG AA contrast ratios in dark mode, and refine timeline and storage alert hierarchy.

## [0.1.0] - 2026-10-15

### Features
- Initial Rust NVR core engine with RTSP ingestion, planar YUV420p SIMD motion detection, OpenVINO C FFI object detection, 2-stage ByteTrack tracking, SQLite WAL persistence, fMP4/HLS VOD remuxing, and Classic UI (`v1`) compatibility.
