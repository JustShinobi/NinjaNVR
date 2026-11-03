# syntax=docker/dockerfile:1.6
FROM oven/bun:1-slim AS ui-builder

WORKDIR /src/web-v2
COPY web-v2/package.json web-v2/bun.lock ./
RUN bun install --frozen-lockfile
COPY web-v2 ./
RUN bun run typecheck && bun run build

FROM rust:1-bookworm AS builder

WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libsqlite3-dev cmake clang ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG NINJANVR_RELEASE_VERSION=""
ENV NINJANVR_RELEASE_VERSION=${NINJANVR_RELEASE_VERSION}
RUN cargo build --release --bin ninjanvr

FROM python:3.11-slim-bookworm AS openvino-assets

RUN pip install --no-cache-dir openvino==2025.4.1 \
    && mkdir -p /usr/local/go2rtc/bin /openvino-model

ADD --chmod=0755 https://github.com/AlexxIT/go2rtc/releases/download/v1.9.14/go2rtc_linux_amd64 /usr/local/go2rtc/bin/go2rtc
ADD --checksum=sha256:bd4e708db591e2c104a8ac3db734809578788f9e76bacfb7ddd227ab024a2a56 https://github.com/JustShinobi/NinjaNVR/releases/download/v1.1.2/openvino-ssdlite-mobilenet-v2.tar.gz /tmp/model.tar.gz

RUN tar --no-same-owner -C /openvino-model -xzf /tmp/model.tar.gz \
    && rm -f /tmp/model.tar.gz

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tzdata curl ffmpeg libsqlite3-0 libssl3 \
    && ln -sf /usr/bin/ffmpeg /usr/local/bin/ffmpeg \
    && ln -sf /usr/bin/ffprobe /usr/local/bin/ffprobe \
    && rm -rf /var/lib/apt/lists/*

COPY --from=openvino-assets /usr/local/go2rtc/bin/go2rtc /usr/local/go2rtc/bin/go2rtc
COPY --from=openvino-assets /openvino-model /openvino-model
COPY --from=openvino-assets /usr/local/lib/python3.11/site-packages/openvino/libs/libopenvino_c.so.2541 /usr/local/lib/ninjanvr/openvino/
COPY --from=openvino-assets /usr/local/lib/python3.11/site-packages/openvino/libs/libopenvino.so.2541 /usr/local/lib/ninjanvr/openvino/
COPY --from=openvino-assets /usr/local/lib/python3.11/site-packages/openvino/libs/libopenvino_intel_cpu_plugin.so /usr/local/lib/ninjanvr/openvino/
COPY --from=openvino-assets /usr/local/lib/python3.11/site-packages/openvino/libs/libopenvino_ir_frontend.so.2541 /usr/local/lib/ninjanvr/openvino/
COPY --from=openvino-assets /usr/local/lib/python3.11/site-packages/openvino/libs/libopenvino_onnx_frontend.so.2541 /usr/local/lib/ninjanvr/openvino/
COPY --from=openvino-assets /usr/local/lib/python3.11/site-packages/openvino/libs/libtbb.so.12 /usr/local/lib/ninjanvr/openvino/
COPY --from=openvino-assets /usr/local/lib/python3.11/site-packages/openvino/libs/libhwloc.so.15 /usr/local/lib/ninjanvr/openvino/
COPY --from=builder /src/target/release/ninjanvr /usr/local/bin/ninjanvr
COPY --from=ui-builder /src/web-v2/dist /opt/ninjanvr/web-v2

ENV CONFIG_FILE=/config/config.yml
ENV NINJANVR_WEB_DIR=/opt/ninjanvr/web-v2
ENV NINJANVR_WEB_V2_DIR=/opt/ninjanvr/web-v2
ENV NINJANVR_MEDIA_DIR=/media/ninjanvr
ENV NINJANVR_OPENVINO_LIB_DIR=/usr/local/lib/ninjanvr/openvino
ENV LD_LIBRARY_PATH=/usr/local/lib/ninjanvr/openvino
ENV OMP_NUM_THREADS=1
ENV OV_CPU_THREADS_NUM=1

EXPOSE 5000 8971 8554 8555/tcp 8555/udp

ENTRYPOINT ["/usr/local/bin/ninjanvr"]
