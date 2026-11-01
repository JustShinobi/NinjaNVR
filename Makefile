-include .env
export

IMAGE         ?= ghcr.io/justshinobi/ninjanvr:v0.1.0
K3S_HOST      ?= localhost
K8S_NAMESPACE ?= frigate
K8S_DEPLOY    ?= frigate

.PHONY: verify check test build build-ui image deploy fast-ui logs status help

verify:
	./scripts/ci/verify

check: verify

test:
	cargo test --workspace

build-ui:
	bun run --cwd web-v2 typecheck
	bun run --cwd web-v2 build

build: build-ui
	cargo build --release --bin ninjanvr

image: build
	@tmpdir=$$(mktemp -d) && \
	cp target/release/ninjanvr "$$tmpdir/ninjanvr" && \
	cp -r web "$$tmpdir/web" && \
	cp -r web-v2/dist "$$tmpdir/web-v2" && \
	printf "FROM ghcr.io/blakeblackshear/frigate:0.18.2 AS upstream-assets\nFROM debian:trixie-slim\nRUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata curl libsqlite3-0 libssl3 && rm -rf /var/lib/apt/lists/*\nCOPY --from=upstream-assets /usr/local/go2rtc/bin/go2rtc /usr/local/go2rtc/bin/go2rtc\nCOPY --from=upstream-assets /usr/lib/ffmpeg/8.0/bin/ffmpeg /usr/local/bin/ffmpeg\nCOPY --from=upstream-assets /openvino-model /openvino-model\nCOPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino_c.so.2541 /usr/local/lib/ninjanvr/openvino/\nCOPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino.so.2541 /usr/local/lib/ninjanvr/openvino/\nCOPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino_intel_cpu_plugin.so /usr/local/lib/ninjanvr/openvino/\nCOPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino_ir_frontend.so.2541 /usr/local/lib/ninjanvr/openvino/\nCOPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libopenvino_onnx_frontend.so.2541 /usr/local/lib/ninjanvr/openvino/\nCOPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libtbb.so.12 /usr/local/lib/ninjanvr/openvino/\nCOPY --from=upstream-assets /usr/local/lib/python3.11/dist-packages/openvino/libs/libhwloc.so.15 /usr/local/lib/ninjanvr/openvino/\nCOPY ninjanvr /usr/local/bin/ninjanvr\nCOPY web /opt/ninjanvr/web\nCOPY web-v2 /opt/ninjanvr/web-v2\nENV CONFIG_FILE=/config/config.yml\nENV NINJANVR_WEB_DIR=/opt/ninjanvr/web\nENV NINJANVR_WEB_V2_DIR=/opt/ninjanvr/web-v2\nENV NINJANVR_MEDIA_DIR=/media/frigate\nENV NINJANVR_OPENVINO_LIB_DIR=/usr/local/lib/ninjanvr/openvino\nENV LD_LIBRARY_PATH=/usr/local/lib/ninjanvr/openvino\nENV OMP_NUM_THREADS=1\nENV OV_CPU_THREADS_NUM=1\nEXPOSE 5000 8971 8554 8555/tcp 8555/udp\nENTRYPOINT [\"/usr/local/bin/ninjanvr\"]\n" > "$$tmpdir/Dockerfile" && \
	docker build -t $(IMAGE) "$$tmpdir" && \
	rm -rf "$$tmpdir"

deploy: image
	docker save $(IMAGE) | ssh $(K3S_HOST) "k3s ctr images import -"
	kubectl -n $(K8S_NAMESPACE) rollout restart deploy/$(K8S_DEPLOY)
	kubectl -n $(K8S_NAMESPACE) rollout status deploy/$(K8S_DEPLOY) --timeout=60s

fast-ui: build-ui
	@pod=$$(kubectl -n $(K8S_NAMESPACE) get pod -l app.kubernetes.io/name=$(K8S_DEPLOY) -o jsonpath="{.items[0].metadata.name}") && \
	kubectl -n $(K8S_NAMESPACE) exec "$$pod" -- rm -rf /opt/ninjanvr/web-v2/assets-v2 && \
	kubectl -n $(K8S_NAMESPACE) cp web-v2/dist/. "$$pod":/opt/ninjanvr/web-v2

logs:
	kubectl -n $(K8S_NAMESPACE) logs -l app.kubernetes.io/name=$(K8S_DEPLOY) --tail=100

status:
	kubectl -n $(K8S_NAMESPACE) get pods,svc,ingress
	kubectl -n $(K8S_NAMESPACE) top pod
