# Contributing to NinjaNVR

Thank you for your interest in contributing to **NinjaNVR**!

## Development Workflow

1. **Prerequisites:**
   - Rust `1.88+` (Edition 2024)
   - `pkg-config`, `libsqlite3-dev`, `clang` / `gcc`
2. **Verification Contract:**
   Before submitting a pull request, ensure all workspace checks pass with zero warnings:
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```
3. **Design Principles:**
   - **Zero-Copy First:** Avoid unnecessary frame allocations or pixel format conversions on the hot path.
   - **Drop-In Compatibility:** Maintain backwards compatibility with Frigate `config.yml`, `frigate.db` SQLite tables, REST/WebSocket API payloads, and Home Assistant MQTT topics.
   - **No Secret Leakage:** Never commit real IP addresses, RTSP credentials, tokens, or private hostnames.
