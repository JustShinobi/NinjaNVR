# Changelog

All notable changes to **NinjaNVR** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.1.2] - 2026-11-02

### Features
- feat(vision): [AI-06] configurable snapshot selection policy (best_overall, best_in_zone, first_trigger) (#24) (`9fd396f`)
- feat(ai-vision): [AI-05] forensic 'Explain Alert' inspector showing exact trigger provenance (#23) (`727609f`)
- feat(vision): [AI-01] capture high-resolution event snapshots from main stream via normalized bbox projection (#22) (`7c04ce7`)
- feat(operations): [OP-04] interactive RTSP/stream connection tester and codec probe (#21) (`4a2e05c`)
- feat(operations): [OP-03] record camera connect/disconnect events and render offline gaps on timeline (#20) (`a7dba4f`)
- feat(ui-v2): [UX-07] persist and display 'Last Online Snapshot' with offline duration when camera drops (#19) (`1c414d1`)
- feat(ui-v2): [UX-05] interactive digital zoom and pan on Live and VOD video players (#18) (`2e39a61`)
- feat(ui-v2): [UX-01] indicate when a Review item or Event has already been exported (#17) (`482e77a`)
- feat(storage): [ST-05] manual recording deletion by time range and disk-to-DB sync endpoint (#16) (`044fa91`)
- feat(storage): [ST-04] degraded storage mount guard and auto-recovery (#15) (`def5f4f`)
- feat(export): [AU-03] option to strip/mute audio track when exporting MP4 clips (#14) (`4568cf6`)
- feat(audio): [AU-01] calculate segment RMS dBFS and render acoustic timeline track (#12) (`eb1bf89`)
- feat(settings): build 7-section tactical settings workbench and guided AI/motion studio (`5c65fd2`)
- feat(ui-v2): add full config types, persisted console preferences, and PT-BR i18n (`ae0b87f`)
- feat(backend): add structured config persistence, user CRUD, and multi-architecture AI decoders (`f974a3d`)
- feat(timeline): add 60fps requestAnimationFrame wheel interpolation and sliding world-space ticks (`0de0315`)
- feat(timeline): render seek track on High-DPI 2D canvas and separate time ruler header (`0c2db5f`)
- feat(ui-v2): add Zustand real-time store and extract format utilities (`399fcdc`)

### Bug Fixes
- fix(vod): [AU-04] resilient fMP4 HLS VOD remuxing when segment audio is missing or corrupted (#13) (`c89a63d`)
- fix(player): preserve user unmute state across VOD timeline seeks (`55cb0cb`)
- fix(export): stage MP4 exports atomically, bypass CompressionLayer on media, and sync IN/OUT range (`3032686`)

### Documentation & Maintenance
- ci(release): adopt cumulative decimal feat/fix versioning in cut-release (`58868c6`)
- ci(release): auto-prune superseded pre-release tags and prepare v1.0.0 [major] (`f190609`)
- chore(deps): migrate workspace to RustCrypto digest 0.11, rcgen 0.14, reqwest 0.13, rand 0.10, Vite 8 & TS 7 (`1b2ed63`)
- chore(deps): bump lucide-react to 1.66.0 and pin Vite 6 / TS 5 in dependabot (`3dbfd80`)
- chore(deps): bump axum-extra, tokio-tungstenite, nix, metadata-action, and switch dependabot to bun (`9360f62`)
- chore(deps): bump workspace crates and GitHub Actions to latest compatible versions (`4603073`)
- refactor(ui-v2): extract 7 modular workspace views and eliminate player seek bar overlays (`c179526`)
- chore(workspace): remove unused ninjanvr-desktop Tauri stub crate (`06fee89`)
- docs(readme): strip AI-slop formatting, emojis, and filler from README and CONTRIBUTING (`15ab90f`)
- ci(release): implement dev pre-release track and main condensed release flow (`018cab9`)

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
