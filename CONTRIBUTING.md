# Contributing to NinjaNVR

Thank you for your interest in contributing to **NinjaNVR**! This document outlines the development setup, architectural guidelines, and release conventions for the project.

---

## 🛠️ Prerequisites

- **Rust:** `1.88+` (Edition 2024) with `rustfmt` and `clippy`
- **Bun:** `1.1+` (for `web-v2` React 19 + TypeScript + Vite console)
- **System Libraries:** `pkg-config`, `libssl-dev`, `libsqlite3-dev`, `cmake`, `clang` / `gcc`

---

## 🚀 Development Workflow

1. **Install frontend dependencies:**
   ```bash
   bun install --cwd web-v2 --frozen-lockfile
   ```

2. **Run the full verification gate (`make verify`):**
   Every pull request must pass `./scripts/ci/verify` (`make verify`) with zero warnings:
   ```bash
   make verify
   ```
   This runs:
   - `./scripts/ci/check-privacy` (ensures no private domains, internal hostnames, or untracked build artifacts leak into commits)
   - `cargo fmt --all --check`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace`
   - `bun run --cwd web-v2 typecheck`
   - `bun run --cwd web-v2 build`

3. **Do not commit generated frontend bundles:**
   - `web-v2/dist/` is generated automatically during CI and inside the multi-stage `Dockerfile`. Keep `web-v2/dist/` untracked.

---

## 📏 Branching & Commit Convention (`dev` ➔ `main`)

NinjaNVR uses a two-track continuous release pipeline (`./scripts/ci/cut-release` in `.github/workflows/release.yml`):

1. **`dev` Branch (Default / Pre-Release Track):**
   - All pull requests and ongoing work target `dev`.
   - Every push to `dev` automatically cuts an incremental Pre-Release (`vX.Y.Z-rc.N`), publishes `ghcr.io/justshinobi/ninjanvr:vX.Y.Z-rc.N` (and `:dev`), and creates a GitHub Pre-Release.
2. **`main` Branch (Stable Release Track):**
   - When a cycle on `dev` is ready for a stable release, `dev` is merged into `main`.
   - Pushing to `main` automatically condenses all commits since the last stable release into `CHANGELOG.md`, cuts the stable tag (`vX.Y.Z`), publishes `ghcr.io/justshinobi/ninjanvr:vX.Y.Z` (and `:latest`), and syncs `main` back into `dev`.

All commit messages and PR titles **must** follow [Conventional Commits](https://www.conventionalcommits.org/):

- `feat(ui-v2): add multi-camera timeline zoom indicator` ➔ bumps **MINOR** version
- `fix(vod): clamp fMP4 segment lookup to requested end timestamp` ➔ bumps **PATCH** version
- `perf(vision): vectorize YUV420p EMA background update` ➔ bumps **PATCH** version
- `docs(readme): document Kubernetes probe endpoints` ➔ included in docs notes
- `chore(ci): update cargo cache key` ➔ non-release maintenance

---

## 🧭 Engineering & Design Principles

- **Zero-Copy First:** Avoid unnecessary frame allocations or pixel format conversions on the video hot path.
- **Low-Memory Discipline:** Keep thread counts, ring buffers, and decoder instances bounded.
- **Ecosystem Compatibility:** Preserve configuration (`config.yml`), SQLite schema, and MQTT topic compatibility so existing users can upgrade seamlessly.
- **Strict Privacy Guardrail:** Never commit private domain names, internal IP addresses, RTSP credentials, or local environment files (`.env`, `DESIGN.md`).
