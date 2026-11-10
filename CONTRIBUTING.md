# Contributing

## Prerequisites

- Rust `1.88+` (Edition 2024) with `rustfmt` and `clippy`
- Bun `1.1+`
- `pkg-config`, `libssl-dev`, `libsqlite3-dev`, `cmake`, `clang`

## Verification

Install frontend dependencies and activate the repository Git hooks once:

```bash
bun install --cwd web --frozen-lockfile
make hooks
```

Run the verification suite before submitting a pull request:

```bash
make verify
```

`make verify` runs:
1. `./scripts/ci/install-hooks --quiet` (activates `.githooks/pre-commit`, `.githooks/commit-msg`, and `.githooks/pre-push`)
2. `./scripts/ci/check-privacy` (blocks private hostnames, `DESIGN.md`, `CLAUDE.md`, and `web/dist` from being tracked)
3. `./scripts/ci/check-semver` (verifies `Cargo.toml`, `Cargo.lock`, `web/package.json`, and `CHANGELOG.md` SemVer synchronization)
4. `cargo fmt --all --check`
5. `cargo clippy --workspace --all-targets -- -D warnings`
6. `cargo test --workspace`
7. `bun run --cwd web typecheck && bun run --cwd web build`

Do not commit `web/dist/`. Frontend assets are built automatically in CI and inside the multi-stage `Dockerfile`.

## Branching and Releases (`dev` -> `main`)

All pull requests and day-to-day commits target the `dev` branch and must follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat(scope):`, `fix(scope):`, `perf(scope):`, `refactor(scope):`, `docs(scope):`, `ci(scope):`, `chore(scope):`).

- **`dev` branch:** Every push cuts an incremental pre-release (`vX.Y.Z-rc.N`), publishes `ghcr.io/justshinobi/ninjanvr:vX.Y.Z-rc.N` and `:dev`, and creates a GitHub Pre-Release.
- **`main` branch:** Merging `dev` into `main` condenses all commits since the previous stable tag into `CHANGELOG.md`, cuts the stable release (`vX.Y.Z`), publishes `:vX.Y.Z` and `:latest`, and syncs `main` back into `dev`.
