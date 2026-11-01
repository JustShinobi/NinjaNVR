-include .env
export

VERSION       ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
IMAGE         ?= ghcr.io/justshinobi/ninjanvr:v$(VERSION)
K3S_HOST      ?= localhost
K8S_NAMESPACE ?= frigate
K8S_DEPLOY    ?= frigate

.PHONY: verify check privacy test build build-ui build-desktop image fast-ui logs status help

verify:
	./scripts/ci/verify

check: verify

privacy:
	./scripts/ci/check-privacy

test:
	cargo test --workspace

build-ui:
	bun run --cwd web-v2 typecheck
	bun run --cwd web-v2 build

build: build-ui
	cargo build --release --bin ninjanvr

build-desktop: build-ui
	cargo build --release -p ninjanvr-desktop

image:
	docker build -t $(IMAGE) .

fast-ui: build-ui
	@pod=$$(kubectl -n $(K8S_NAMESPACE) get pod -l app.kubernetes.io/name=$(K8S_DEPLOY) -o jsonpath="{.items[0].metadata.name}") && \
	kubectl -n $(K8S_NAMESPACE) exec "$$pod" -- rm -rf /opt/ninjanvr/web-v2/assets-v2 && \
	kubectl -n $(K8S_NAMESPACE) cp web-v2/dist/. "$$pod":/opt/ninjanvr/web-v2

logs:
	kubectl -n $(K8S_NAMESPACE) logs -l app.kubernetes.io/name=$(K8S_DEPLOY) --tail=100

status:
	kubectl -n $(K8S_NAMESPACE) get pods,svc,ingress
	kubectl -n $(K8S_NAMESPACE) top pod
