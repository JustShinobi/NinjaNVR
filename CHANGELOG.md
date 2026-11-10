# Changelog

All notable changes to **NinjaNVR** will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [2.1.9] - 2026-11-04

### Features
- **web:** hide unreviewed alert ribbon in spotlight and tactical map (`a76c869`)
- **web:** add dual spotlight timeline modes with auto-pause and clean wall (`8b7cb86`)
- **web:** add share link management panel, revocation controls, and clipboard fallback (`6a91724`)
- **web:** unify live layouts into zero-unmount persistent video stage (#92) (`ae86776`)
- **web:** add dual-slot ping-pong MSE upgrade, frame bridge, and hover prewarm (#91) (`ffb6021`)
- **server,vod:** add MseStreamHub GOP ring-buffer, stream prewarm API, and lazy YuvFrame snapshots (#90) (`62f77a6`)
- **server,web:** serve direct event clip and mock_video MP4s in Explore and Review (`73caf53`)
- **web:** synchronize matrix stage page with timeline camera selection and add on-stage quick pills (`0c49e8c`)
- **web:** add floorplan window segments and declutter locked map labels (`4d3366e`)
- **web,server:** fix face profile deletion cleanup, rework state ROI UX, and add fleet/LPR test simulators (`055ef31`)
- **web:** add metric CAD floorplan editor with exact mouse mapping and wall sizing (`48b765c`)
- **web:** refine AI dataset studio workbench and model factory layout (`383d9b1`)
- **genai,web:** implement API validation and dynamic model dropdown for Vision AI Judge (`6d30be3`)
- **web,config:** add tactical light/daylight theme engine and high-contrast monitor boost (#89) (`c55dc33`)
- **web:** add hardware-safe dark surface token scale and eliminate black-crush (#88) (`858aadf`)
- **ai-training:** add 15-class security base model factory and fine-tuning pipeline (#82) (`8b60a01`)
- **server,web:** add active learning dataset studio, AI auto-labeler, and model hot-reload (#81) (`735e5ea`)
- **vision:** add ROI state classification with hysteresis and anomaly alerts (#58) (`37781b8`)
- **infer:** add license plate recognition with multi-frame voting and manual override (#57) (`b458305`)
- **infer:** add local face recognition gallery, sub-label assignment, and alert rules (#56) (`3020371`)
- **operations:** add ONVIF PTZ camera control HUD and preset-bound zones (#60) (`7cf7d41`)
- **server,db:** add false-positive dhash64 memory suppression and vision AI judge (#80) (`8be20ba`)
- **web:** add interactive Tactical Floorplan Studio v2 with room drawer and FOV handle (#78) (`7270676`)
- **audio:** add WebRTC backchannel push-to-talk with clean mic release (#61) (`9848f50`)
- **infer,track:** add dynamic multi-class YOLO decoder and parent-child attributes (#79) (`86c2e63`)
- **web:** add 100+ camera paginated matrix, camera groups, and adaptive picker (#72) (`51b4abd`)
- **infer,ci:** [AI-18] complete OrtApi session vtable execution in detect_region and run web-v2 tests in verify (#87) (`55e9436`)
- **infer,web:** [AI-18] add pluggable OpenVINO and ONNX Runtime dual-backend engine (#87) (`906df37`)
- **web:** add cold compaction codec/hwaccel/preset controls and 300s pre/post-capture limits (`1209314`)
- **media:** add pre/post-capture retention window guard and configurable cold compaction (`923d929`)
- **vision,media:** add automatic OSD clock masking and scale preview encoding by quality tier (`94813b0`)
- **web,docs:** add topbar Logout button and document OIDC/Zitadel SSO, ForwardAuth, RBAC, and API tokens (`1371c7c`)
- **web:** refine VOD/Live frame-step HUD and add Multi-Node Federation settings card (`4c0dfb6`)
- **web:** add interactive OIDC/ForwardAuth form and AI Situation Brief / MCP inspector panel (`4385133`)
- **web:** surface Wave 3 tiered storage, disk quota, hybrid stream, alert backup, and multi-detector UI controls (`7b46486`)
- **web:** surface Wave 2 IR Night/Tamper badges, Tripwire list, and High-Res Snapshot toggle (`95f9756`)
- **server:** [OP-08] multi-node NVR Federation ('Single Glass Console' aggregating multiple NinjaNVR instances) (#71) (`1120b02`)
- **media:** [AU-06] per-camera audio lip-sync delay offset (audio_offset_ms) and Apple-compliant SANs on auto-generated TLS cert (#70) (`56bab18`)
- **comms:** [SEC-06] MQTTS (TLS 1.2/1.3, system Root CAs, custom CA cert, and client mTLS) support (#69) (`694dc02`)
- **server:** [OP-07] Prometheus / VictoriaMetrics OpenMetrics endpoint (/api/metrics) and automatic SQLite maintenance (#68) (`1c9742e`)
- **web:** [UX-11] Browser Web Notifications, optional tactical audible alert chime, and Notification Tester (#67) (`782e058`)
- **web:** [UX-10] per-camera video rotation (0° / 90° Corridor Mode / 180° / 270°) in Live and VOD players (#66) (`a0eaba6`)
- **web:** [UX-09] forensic operator notes on Events/Reviews and multiline technical notes on Cameras (#65) (`02173fe`)
- **web:** [UX-08] frame-by-frame VOD stepping (< 1f / 1f >), timeline window zoom, and on-demand frame capture (#64) (`e712bc2`)
- **vision:** [AI-13] burned-in Privacy Masks (permanent redaction on snapshots, live view, and exports) (#63) (`f0a25e3`)
- **security:** [SEC-05] authenticated Camera Web-UI Reverse Proxy for isolated camera VLANs (#62) (`dcdba71`)
- **vision:** [AI-12] retroactive clip detection replay (Debug Reprocess) and dominant color attribute tagging (#59) (`5df102b`)
- **security:** [SEC-04] native OpenID Connect (OIDC) SSO and hardened Trusted-Proxy ForwardAuth (#55) (`164e03e`)
- **server:** [OP-05] native AI/LLM Context Summary API and MCP-compatible query endpoints (#44) (`ca7bb5b`)
- **web:** [UX-04] 'Daily Tactical Brief' accelerated sequential playback of unreviewed alerts (#43) (`d4b44c4`)
- **web:** [UX-02] interactive Tactical Map / Floorplan view with camera FOV cones and live activity pulse (#42) (`5bc3731`)
- **security:** [SEC-03] long-lived scoped API Bearer tokens (nnvr_*) for automation and metrics (#41) (`103a25e`)
- **security:** [SEC-02] expiring HMAC-signed shareable links for exported clips and review events (#40) (`f690b56`)
- **security:** [SEC-01] granular Role-Based Access Control (admin, operator, viewer) and per-camera user scoping (#39) (`3f90121`)
- **infer:** [AI-07] multi-detector routing and per-camera AI model assignment (#38) (`e88246a`)
- **operations:** [OP-02] dynamic runtime per-camera toggles (enable/disable camera, detect, record, alerts) (#37) (`3ea12d3`)
- **operations:** [OP-01] operational Security Profiles (ARMED, HOME, DISARMED) via UI, REST API, and MQTT (#36) (`6a199ed`)
- **storage:** [ST-06] automatic real-time backup/export of high-severity Alert clips (#35) (`6bb5882`)
- **media:** [ST-03] hybrid dual-stream recording and VOD resolution discontinuity support (#34) (`6cf3d9a`)
- **storage:** [ST-02] configurable retention by disk quota and storage runway estimator (#33) (`9384a92`)
- **storage:** [ST-01] native Hot-to-Cold tiered storage migration (#32) (`3f59df0`)
- **web:** [UX-06] boolean filter logic (AND/OR/NOT), zone/stationary filters, and saved filter presets (#31) (`69e3e03`)
- **web:** [UX-03] client-side 'Tactical Spotlight' dynamic Birdseye layout (#30) (`18e5e9a`)
- **audio:** [AU-02] dedicated Audio Detections category, review filter, and acoustic event triggers (#29) (`64c4e3e`)
- **track:** [AI-08] resolution-agnostic normalized area filters and per-zone overrides (#28) (`eb0c536`)
- **vision:** [AI-04] camera tampering and blind-feed detection (#27) (`0680a89`)
- **vision:** [AI-03] automatic Day/Night IR detection profile and spider false-positive filter (#26) (`31c49eb`)
- **track:** [AI-02] directional virtual tripwires and line-crossing detection in ByteTracker (#25) (`427ce23`)

### Bug Fixes
- **server:** support http2 authority and forwarded host in share csrf check (`6807d97`)
- **db:** acoplar ciclo de vida de share links a tokens, roles e retencao (`41eef90`)
- **web:** rotacionar e revogar share link anterior preservando saldo de views (`96dc816`)
- **server,media:** add share clip singleflight, mask hash invalidation, and path containment (`db5cbe4`)
- **server,web:** fix 12/36/100 fleet simulator stage layout, decoder caps, and on-demand MSE warmers (`889aab0`)
- **server,vod,web:** unify upstream MSE muxer, ignore audio sync frames, and fix slot blink (`c22cf0b`)
- **web:** align header brand lockup, relocate channel count, and auto-reconnect ws (`b060c53`)
- **web:** auto-persist LPR known plates and surface detected plates with one-click registration (`abdf6a8`)
- **server:** refine export RBAC scoping and ignore empty Gemini parts (`0a6a8ec`)
- **infer:** support regex character class escapes and uniform face embeddings (`41cb9a8`)
- **web:** adapt partial matrix page grid, preserve camera selection, and add timeline reticle (`62da69f`)
- **web:** move fleet simulator to settings, wire simulated fleet to timeline picker, and fix ROI preset overflow (`87e0cc6`)
- **media:** support structured privacy masks and prevent false VOD VBR discontinuities (`8aa3d78`)
- **track:** preserve velocity across brief occlusions and attach person sub-labels (`737fe08`)
- **vision:** reset masked pixel EMA state and add 16x16 spatial face embedding (`94abd9c`)
- **db:** preserve event metadata on upsert and fix xyxy IoU and review queries (`54d9f49`)
- **config:** preserve roundtrip fields, global track inheritance and floorplan scale (`5ca62bc`)
- **genai,web:** add direct provider API validation fallback and clear unvalidated model state (`8ccfa73`)
- **web:** enable native text selection across console while preserving drag controls (`1af41d7`)
- **web:** resolve settings workbench grid collapse and control overflow (`5dcf004`)
- **db,media:** index retention window queries and offload cleanup to spawn_blocking (`62b602f`)
- **vision:** suppress OSD clock ticks and false-positive motion badges (`6f7a5ce`)
- **oidc:** resolve external redirect_uri across HTTP/2 and X-Forwarded-Host and omit empty PKCE client_secret (`cb6b42c`)
- **wave4:** unstack default map cameras, promote AI/MCP panel to System top, and accept MCP notifications/initialized (`82cb45d`)
- **wave2:** calibrate IR lens tamper, stationary jitter, Spotlight hero switch, and audio trigger UI (`3bff21e`)
- **media:** decode compressed mp4a audio before computing segment RMS dBFS (`2f17f04`)
- **ci:** include cumulative commits since last stable tag in pre-release notes (`e0a3747`)
- **media:** replace chunks_exact(2) with as_chunks::<2>() for Rust 1.99 clippy (`5982033`)
- **review:** address second-pass code review findings across #32-#36 and #63 (`bd5996f`)

### Security & Hardening
- **server:** 8 security, bounds, and lifecycle hardening fixes (`6b99830`..`a54729c`)
- **media,db:** enforce fail-closed export audio stripping and unique id-aware api token creators (`62aac5f`)
- **web:** enforce strict share link rotation revocation, exact TTL, and export delete validation (`c4abf73`)

## [1.1.2] - 2026-11-02

### Breaking Changes
- **release:** auto-prune superseded pre-release tags and prepare v1.0.0 [major] (`f190609`)

### Features
- **vision:** [AI-06] configurable snapshot selection policy (best_overall, best_in_zone, first_trigger) (#24) (`9fd396f`)
- **ai-vision:** [AI-05] forensic 'Explain Alert' inspector showing exact trigger provenance (#23) (`727609f`)
- **vision:** [AI-01] capture high-resolution event snapshots from main stream via normalized bbox projection (#22) (`7c04ce7`)
- **operations:** [OP-04] interactive RTSP/stream connection tester and codec probe (#21) (`4a2e05c`)
- **operations:** [OP-03] record camera connect/disconnect events and render offline gaps on timeline (#20) (`a7dba4f`)
- **web:** [UX-07] persist and display 'Last Online Snapshot' with offline duration when camera drops (#19) (`1c414d1`)
- **web:** [UX-05] interactive digital zoom and pan on Live and VOD video players (#18) (`2e39a61`)
- **web:** [UX-01] indicate when a Review item or Event has already been exported (#17) (`482e77a`)
- **storage:** [ST-05] manual recording deletion by time range and disk-to-DB sync endpoint (#16) (`044fa91`)
- **storage:** [ST-04] degraded storage mount guard and auto-recovery (#15) (`def5f4f`)
- **export:** [AU-03] option to strip/mute audio track when exporting MP4 clips (#14) (`4568cf6`)
- **audio:** [AU-01] calculate segment RMS dBFS and render acoustic timeline track (#12) (`eb1bf89`)
- **settings:** build 7-section tactical settings workbench and guided AI/motion studio (`5c65fd2`)
- **web:** add full config types, persisted console preferences, and PT-BR i18n (`ae0b87f`)
- **backend:** add structured config persistence, user CRUD, and multi-architecture AI decoders (`f974a3d`)
- **timeline:** add 60fps requestAnimationFrame wheel interpolation and sliding world-space ticks (`0de0315`)
- **timeline:** render seek track on High-DPI 2D canvas and separate time ruler header (`0c2db5f`)
- **web:** add Zustand real-time store and extract format utilities (`399fcdc`)

### Bug Fixes
- **vod:** [AU-04] resilient fMP4 HLS VOD remuxing when segment audio is missing or corrupted (#13) (`c89a63d`)
- **player:** preserve user unmute state across VOD timeline seeks (`55cb0cb`)
- **export:** stage MP4 exports atomically, bypass CompressionLayer on media, and sync IN/OUT range (`3032686`)

## [0.9.1] - 2026-11-01

### Maintenance
- **release:** automate continuous SemVer tagging, changelog, GHCR build, and GitOps bump on main (`aabff8f`)
- **agents:** remove CLAUDE.md and consolidate repository guidelines in AGENTS.md (`9236b9c`)

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
