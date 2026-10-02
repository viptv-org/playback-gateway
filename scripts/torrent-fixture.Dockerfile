# SPDX-License-Identifier: GPL-2.0-only
# Qualification only; uses the binaries from an explicitly selected local image.
ARG GATEWAY_IMAGE
FROM rust:1.98-trixie AS fixture-build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY engine ./engine
COPY control ./control
COPY ffi ./ffi
ENV CARGO_BUILD_JOBS=2
RUN cargo test --locked -p playback-gateway --lib --no-run \
    && mkdir /out \
    && executable="$(find target/debug/deps -maxdepth 1 -type f -executable -name 'playback_gateway-*')" \
    && test "$(printf '%s\n' "$executable" | wc -l)" -eq 1 \
    && cp "$executable" /out/gateway-lib-fixture

FROM ${GATEWAY_IMAGE}
COPY --from=fixture-build /out /fixtures
# Compile-time fixture asset paths point here; only generated test media is copied.
# The compiled path starts at /src/control/../engine, so its parent must exist.
USER 0:0
RUN mkdir -p /src/control
USER 10001:10001
COPY engine/assets/rar-media /src/engine/assets/rar-media
ENV PLAYBACK_TEST_FFMPEG=/opt/ffmpeg/bin/ffmpeg \
    PLAYBACK_TEST_FFPROBE=/opt/ffmpeg/bin/ffprobe
ENTRYPOINT ["/fixtures/gateway-lib-fixture"]
