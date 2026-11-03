# syntax=docker/dockerfile:1.6
FROM oven/bun:1-slim AS ui-builder

WORKDIR /src/web-v2
COPY web-v2/package.json web-v2/bun.lock ./
RUN bun install --frozen-lockfile
COPY web-v2 ./
RUN bun run typecheck && bun run build

FROM rust:1-trixie AS builder

WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libsqlite3-dev cmake clang ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
ARG NINJANVR_RELEASE_VERSION=""
ENV NINJANVR_RELEASE_VERSION=${NINJANVR_RELEASE_VERSION}
RUN cargo build --release --bin ninjanvr

FROM alexxit/go2rtc:1.9.15 AS go2rtc-bin
FROM mwader/static-ffmpeg:9.0.2-1 AS ffmpeg-bin

FROM python:3.12-slim-trixie AS openvino-assets

COPY docker/requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt \
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

RUN tar --no-same-owner -C /openvino-model -xzf /tmp/model.tar.gz \
    && rm -f /tmp/model.tar.gz

FROM debian:trixie-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tzdata curl libsqlite3-0 libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=go2rtc-bin /usr/local/bin/go2rtc /usr/local/go2rtc/bin/go2rtc
COPY --from=ffmpeg-bin /ffmpeg /usr/local/bin/ffmpeg
COPY --from=ffmpeg-bin /ffprobe /usr/local/bin/ffprobe
COPY --from=openvino-assets /openvino-model /openvino-model
COPY --from=openvino-assets /out/openvino/ /usr/local/lib/ninjanvr/openvino/
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
