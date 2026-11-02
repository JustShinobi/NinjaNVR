# Contributing

## Prerequisites

- Rust `1.88+` (Edition 2024) with `rustfmt` and `clippy`
- Bun `1.1+`
- `pkg-config`, `libssl-dev`, `libsqlite3-dev`, `cmake`, `clang`

## Verification

Install frontend dependencies once:

```bash
bun install --cwd web-v2 --frozen-lockfile
```

Run the verification suite before submitting a pull request:

```bash
make verify
```

`make verify` runs:
1. `./scripts/ci/check-privacy` (blocks private hostnames, `DESIGN.md`, `CLAUDE.md`, and `web-v2/dist` from being tracked)
2. `cargo fmt --all --check`
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo test --workspace`
5. `bun run --cwd web-v2 typecheck && bun run --cwd web-v2 build`

Do not commit `web-v2/dist/`. Frontend assets are built automatically in CI and inside the multi-stage `Dockerfile`.

## Branching and Releases (`dev` -> `main`)

All pull requests and day-to-day commits target the `dev` branch and must follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat(scope):`, `fix(scope):`, `perf(scope):`, `refactor(scope):`, `docs(scope):`, `ci(scope):`, `chore(scope):`).

- **`dev` branch:** Every push cuts an incremental pre-release (`vX.Y.Z-rc.N`), publishes `ghcr.io/justshinobi/ninjanvr:vX.Y.Z-rc.N` and `:dev`, and creates a GitHub Pre-Release.
- **`main` branch:** Merging `dev` into `main` condenses all commits since the previous stable tag into `CHANGELOG.md`, cuts the stable release (`vX.Y.Z`), publishes `:vX.Y.Z` and `:latest`, and syncs `main` back into `dev`.
