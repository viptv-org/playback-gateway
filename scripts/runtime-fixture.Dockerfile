# SPDX-License-Identifier: GPL-2.0-only
# Qualification only; never a deployment image.
ARG GATEWAY_IMAGE
FROM rust:1.98-trixie AS fixture-build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY engine ./engine
COPY control ./control
ENV CARGO_BUILD_JOBS=2
RUN cargo test --locked -p playback-gateway --tests --no-run \
    && mkdir /out \
    && for name in runtime service; do \
       executable="$(find target/debug/deps -maxdepth 1 -type f -executable -name "$name-*")"; \
       test "$(printf '%s\n' "$executable" | wc -l)" -eq 1; \
       cp "$executable" "/out/$name-fixture"; done

FROM ${GATEWAY_IMAGE}
COPY --from=fixture-build /out /fixtures
ENV PLAYBACK_TEST_ISOLATED_NETWORK=container \
    PLAYBACK_TEST_SERVICE_BINARY=/usr/local/bin/playback-gateway \
    PLAYBACK_TEST_FFMPEG=/opt/ffmpeg/bin/ffmpeg \
    PLAYBACK_TEST_FFPROBE=/opt/ffmpeg/bin/ffprobe
ENTRYPOINT ["/fixtures/runtime-fixture"]
