# syntax=docker/dockerfile:1.6
FROM oven/bun:1-slim@sha256:1846adf22496bd711036aae8be147e57bdebbaea2e5264e3e0b5af1db187d9a4 AS ui-builder

WORKDIR /src/web
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile
COPY web ./
RUN bun run typecheck && bun run test && bun run build \
    && find /src/web/dist -type d -exec chmod 0755 {} + \
    && find /src/web/dist -type f -exec chmod 0644 {} +

FROM rust:1-trixie@sha256:223cc32ddca8f3ee90c436dbc94b6b5f052ea7f9653c5201e9612f08ad7d21b8 AS builder

WORKDIR /src
RUN DEBIAN_FRONTEND=noninteractive apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    pkg-config libssl-dev cmake clang ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG NINJANVR_RELEASE_VERSION=""
ENV NINJANVR_RELEASE_VERSION=${NINJANVR_RELEASE_VERSION}
RUN cargo build --release --locked --bin ninjanvr

FROM alexxit/go2rtc:1.9.15@sha256:e098de5c2f8040dc3d4784ed2d5cfdac3b0979f9349afe587ef6a670a14b9f4d AS go2rtc-bin
FROM mwader/static-ffmpeg:9.0.2-1@sha256:f5d100184884eb3e82b0eb14bb5ebf98da99826ac824989720297fb9ef21c822 AS ffmpeg-bin

FROM python:3.12-slim-trixie@sha256:5e2bdb1b0d542d5039cffb8181c65930093949194a65c365dfb1b0fe13a4fa5b AS openvino-assets

COPY docker/requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir --only-binary=:all: -r /tmp/requirements.txt \
    && mkdir -p /openvino-model /out/openvino \
    && OV_LIBS="$(python3 -c 'import openvino, pathlib; print(pathlib.Path(openvino.__file__).parent / "libs")')" \
    && cp -a \
       "${OV_LIBS}"/libopenvino_c.so.* \
       "${OV_LIBS}"/libopenvino.so.* \
       "${OV_LIBS}"/libopenvino_intel_cpu_plugin.so \
       "${OV_LIBS}"/libopenvino_intel_gpu_plugin.so \
       "${OV_LIBS}"/libopenvino_intel_npu_plugin.so \
       "${OV_LIBS}"/libopenvino_auto_plugin.so \
       "${OV_LIBS}"/libopenvino_ir_frontend.so.* \
       "${OV_LIBS}"/libopenvino_onnx_frontend.so.* \
       "${OV_LIBS}"/libtbb.so.* \
       "${OV_LIBS}"/libhwloc.so.* \
       /out/openvino/ \
    && cd /out/openvino \
    && ln -sf libopenvino_c.so.* libopenvino_c.so \
    && ln -sf libopenvino.so.* libopenvino.so \
    && ln -sf libopenvino_ir_frontend.so.* libopenvino_ir_frontend.so \
    && ln -sf libopenvino_onnx_frontend.so.* libopenvino_onnx_frontend.so

ADD --checksum=sha256:bd4e708db591e2c104a8ac3db734809578788f9e76bacfb7ddd227ab024a2a56 https://github.com/JustShinobi/NinjaNVR/releases/download/v1.1.2/openvino-ssdlite-mobilenet-v2.tar.gz /tmp/model.tar.gz

RUN tar --no-same-owner --no-same-permissions -C /openvino-model -xzf /tmp/model.tar.gz \
    && rm -f /tmp/model.tar.gz
COPY models/ninjanvr_security_15cl.txt /openvino-model/ninjanvr_security_15cl.txt
RUN find /openvino-model -type d -exec chmod 0755 {} + \
    && find /openvino-model -type f -exec chmod 0644 {} + \
    && chmod -R a+rX /out/openvino

FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2 AS runtime
RUN DEBIAN_FRONTEND=noninteractive apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    ca-certificates tzdata curl libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 ninjanvr \
    && useradd --uid 10001 --gid 10001 --no-log-init --home-dir /config --no-create-home --shell /usr/sbin/nologin ninjanvr \
    && install -d -m 0750 -o 10001 -g 10001 /config /media/ninjanvr /tmp/cache \
    && install -d -m 0755 /opt/ninjanvr/web /openvino-model /usr/local/lib/ninjanvr/openvino /usr/local/go2rtc/bin

COPY --chmod=0755 --from=go2rtc-bin /usr/local/bin/go2rtc /usr/local/go2rtc/bin/go2rtc
COPY --chmod=0755 --from=ffmpeg-bin /ffmpeg /usr/local/bin/ffmpeg
COPY --chmod=0755 --from=ffmpeg-bin /ffprobe /usr/local/bin/ffprobe
COPY --from=openvino-assets /openvino-model /openvino-model
COPY --from=openvino-assets /out/openvino/ /usr/local/lib/ninjanvr/openvino/
COPY --chmod=0755 --from=builder /src/target/release/ninjanvr /usr/local/bin/ninjanvr
COPY --chmod=0755 docker/entrypoint.sh /usr/local/bin/ninjanvr-entrypoint
COPY --from=ui-builder /src/web/dist /opt/ninjanvr/web

ENV HOME=/config
ENV XDG_CACHE_HOME=/tmp/cache
ENV CONFIG_FILE=/config/config.yml
ENV NINJANVR_WEB_DIR=/opt/ninjanvr/web
ENV NINJANVR_MEDIA_DIR=/media/ninjanvr
ENV NINJANVR_OPENVINO_LIB_DIR=/usr/local/lib/ninjanvr/openvino
ENV LD_LIBRARY_PATH=/usr/local/lib/ninjanvr/openvino
ENV OMP_NUM_THREADS=1
ENV OV_CPU_THREADS_NUM=1

# 5000: Auth-enforced HTTP API & health probes | 8971: HTTPS/TLS operator console
# 9091: Dedicated OpenMetrics (/metrics) & /healthz listener (set NINJANVR_METRICS_PORT=0 to disable)
# 8555: go2rtc WebRTC media (TCP/UDP); go2rtc API (:1984) & RTSP relay (:8554) bind to 127.0.0.1 by default
EXPOSE 5000 8971 9091 8555/tcp 8555/udp

# Drops privileges to 10001:10001 (ninjanvr) by default on fresh/10001-writable mounts,
# honors explicit Kubernetes securityContext.runAsNonRoot (--user 10001:10001), and
# preserves drop-in compatibility when upgrading over legacy root-owned HostPath mounts.
ENTRYPOINT ["/usr/local/bin/ninjanvr-entrypoint"]
