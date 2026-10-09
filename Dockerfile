FROM rust:1.98-trixie AS app-build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY engine ./engine
COPY control ./control
COPY ffi ./ffi
COPY torrent-policy ./torrent-policy
COPY telemetry ./telemetry
COPY runtime-host ./runtime-host
COPY third_party/rqbit ./third_party/rqbit
RUN CARGO_BUILD_JOBS=1 cargo build --release --locked -p playback-gateway

FROM debian:trixie-slim AS media-build
ARG FFMPEG_VERSION=9.0.2
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config nasm curl ca-certificates gnupg xz-utils \
    libgnutls28-dev libx264-dev libzimg-dev libva-dev libdrm-dev libvpl-dev \
    && apt-get clean
WORKDIR /src
RUN curl --fail --silent --show-error --location https://ffmpeg.org/ffmpeg-devel.asc -o release-key.asc \
    && gpg --batch --import release-key.asc \
    && curl --fail --silent --show-error --location "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" -o ffmpeg.tar.xz \
    && curl --fail --silent --show-error --location "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz.asc" -o ffmpeg.tar.xz.asc \
    && gpg --batch --status-fd=1 --verify ffmpeg.tar.xz.asc ffmpeg.tar.xz | grep 'VALIDSIG FCF986EA15E6E293A5644F10B4322F04D67658D8 ' \
    && tar -xf ffmpeg.tar.xz --strip-components=1 \
    && ./configure --prefix=/opt/ffmpeg --disable-debug --disable-doc --disable-ffplay \
       --disable-autodetect --enable-gpl --enable-gnutls --enable-libx264 --enable-libzimg \
       --enable-vaapi --enable-libdrm --enable-libvpl \
    && make -j4 && make install

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl libgnutls30t64 libx264-164 libzimg2 libva2 libva-drm2 libdrm2 libvpl2 \
    mesa-va-drivers intel-media-va-driver \
    && apt-get clean \
    && groupadd --gid 10001 gateway && useradd --uid 10001 --gid gateway --no-create-home gateway \
    && install -d -o gateway -g gateway -m 0700 /var/lib/playback-gateway /var/lib/playback-gateway/media
COPY --from=app-build /src/target/release/playback-gateway /usr/local/bin/playback-gateway
COPY vendor/torrent-runtime/torrent-worker /usr/local/bin/torrent-worker
COPY vendor/torrent-runtime /usr/share/doc/playback-gateway/torrent-runtime
COPY TORRENT_RUNTIME_REF /usr/share/doc/playback-gateway/TORRENT_RUNTIME_REF
RUN chmod 0755 /usr/local/bin/torrent-worker
COPY --from=media-build /opt/ffmpeg /opt/ffmpeg
COPY --from=media-build /src/ffmpeg.tar.xz /src/ffmpeg.tar.xz.asc /usr/share/doc/playback-gateway/
COPY LICENSE PROVENANCE.md /usr/share/doc/playback-gateway/
COPY THIRD_PARTY /usr/share/doc/playback-gateway/third-party/
COPY third_party/rqbit/LICENSE third_party/rqbit/LICENSE-APACHE-2.0.txt third_party/rqbit/UPSTREAM.json /usr/share/doc/playback-gateway/third-party/rqbit/
ENV DATA_DIR=/var/lib/playback-gateway BIND_ADDRESS=0.0.0.0:8090 \
    TORRENT_WORKER=/usr/local/bin/torrent-worker FFMPEG_PATH=/opt/ffmpeg/bin/ffmpeg FFPROBE_PATH=/opt/ffmpeg/bin/ffprobe MAX_OUTPUTS=8
USER 10001:10001
EXPOSE 8090
VOLUME ["/var/lib/playback-gateway"]
HEALTHCHECK --interval=15s --timeout=4s --start-period=15s --retries=3 \
    CMD ["curl", "--fail", "--silent", "--max-time", "3", "http://127.0.0.1:8090/ready"]
ENTRYPOINT ["/usr/local/bin/playback-gateway"]
