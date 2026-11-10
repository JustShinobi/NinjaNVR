-include .env
export

VERSION       ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
IMAGE         ?= ghcr.io/justshinobi/ninjanvr:v$(VERSION)
K3S_HOST      ?= localhost
K8S_NAMESPACE ?= ninjanvr
K8S_DEPLOY    ?= ninjanvr

.PHONY: verify check hooks privacy semver test build build-ui docs-dev docs-build image fast-ui logs status help

verify:
	./scripts/ci/verify

check: verify

hooks:
	./scripts/ci/install-hooks

privacy:
	./scripts/ci/check-privacy

semver:
	./scripts/ci/check-semver

test:
	cargo test --workspace

build-ui:
	bun run --cwd web typecheck
	bun run --cwd web build

docs-dev:
	bun run --cwd docs dev

docs-build:
	bun run --cwd docs build

build: build-ui
	cargo build --release --bin ninjanvr

image:
	docker build -t $(IMAGE) .

fast-ui: build-ui
	@pod=$$(kubectl -n $(K8S_NAMESPACE) get pod -l app.kubernetes.io/name=$(K8S_DEPLOY) -o jsonpath="{.items[0].metadata.name}") && \
	kubectl -n $(K8S_NAMESPACE) exec "$$pod" -- rm -rf /opt/ninjanvr/web/assets && \
	kubectl -n $(K8S_NAMESPACE) cp web/dist/. "$$pod":/opt/ninjanvr/web

logs:
	kubectl -n $(K8S_NAMESPACE) logs -l app.kubernetes.io/name=$(K8S_DEPLOY) --tail=100

status:
	kubectl -n $(K8S_NAMESPACE) get pods,svc,ingress
	kubectl -n $(K8S_NAMESPACE) top pod
