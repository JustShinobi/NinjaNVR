# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.9.x   | :white_check_mark: |
| < 0.9.0 | :x:                |

## Security Architecture

NinjaNVR is designed with defense-in-depth for self-hosted and edge CCTV deployments:

- **Authenticated TLS Port (`:8971`):** Serves the web console and REST/WebSocket APIs behind PBKDF2-HMAC-SHA256 password hashing and signed HTTP-only JWT session cookies.
- **Automatic Credential Scrubbing (`ninjanvr-config`):** Strips RTSP, MQTT, and URL credentials (`rtsp://*:*@...`) from all `/api/config` responses and log streams.
- **Memory Safety:** Written in safe Rust (`edition = "2024"`) with bounds-checked ISO-BMFF MP4 parsing and parameterized SQLite queries (`rusqlite`), isolating only the `libopenvino_c.so` C FFI boundary in `ninjanvr-infer`.

## Reporting a Vulnerability

Please **do not** report security vulnerabilities through public GitHub issues.

Instead, please use [GitHub Private Vulnerability Reporting](https://github.com/JustShinobi/NinjaNVR/security/advisories/new) on this repository. Include:

1. A description of the vulnerability and affected crate/component (`ninjanvr-server`, `ninjanvr-vod`, `web-v2`, etc.).
2. Steps to reproduce or a proof-of-concept request/configuration.
3. Potential impact and any suggested mitigation.

We aim to acknowledge vulnerability reports within **48 hours** and provide a remediation release as quickly as possible.
