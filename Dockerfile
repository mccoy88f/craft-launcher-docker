# syntax=docker/dockerfile:1
# ArtCraft Launcher server: the launcher's web page plus the Craft Apps' browser builds, for
# docker compose behind a domain. See packaging/docker/README.md.
#
#   docker build -t artcraft-launcher-server .
#   docker run -p 8080:8080 -e ARTCRAFT_ADMIN_PASSWORD=… -v artcraft-data:/data artcraft-launcher-server
#
# RUST_VERSION must stay >= the workspace `rust-version` in Cargo.toml. TRUNK_VERSION matches the
# `TRUNK_VERSION` CI builds the web page with (.github/workflows/ci.yml).
ARG RUST_VERSION=1.97
ARG DEBIAN_VERSION=bookworm

FROM rust:${RUST_VERSION}-slim-${DEBIAN_VERSION} AS build
ARG TRUNK_VERSION=0.21.14
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add wasm32-unknown-unknown
# trunk, checked against the checksum its release publishes.
RUN set -eu; \
    case "$(uname -m)" in \
        x86_64) arch=x86_64 ;; \
        aarch64) arch=aarch64 ;; \
        *) echo "the build stage supports amd64 and arm64" >&2; exit 1 ;; \
    esac; \
    asset="trunk-${arch}-unknown-linux-musl.tar.gz"; \
    url="https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}"; \
    cd /tmp; \
    curl -fsSL --retry 3 -A ArtCraft-Launcher-build "$url/$asset" -o "$asset"; \
    curl -fsSL --retry 3 -A ArtCraft-Launcher-build "$url/$asset.sha256" -o "$asset.sha256"; \
    printf '%s  %s\n' "$(cut -d' ' -f1 "$asset.sha256")" "$asset" | sha256sum -c -; \
    tar -xzf "$asset" -C /usr/local/bin trunk; \
    rm "$asset" "$asset.sha256"; \
    trunk --version

WORKDIR /src
COPY . .
# Optional build provenance (the About line), as for the desktop builds.
ARG ARTCRAFT_LAUNCHER_BUILD_SHA=
ARG ARTCRAFT_LAUNCHER_BUILD_DATE=
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/root/.cache/trunk \
    set -eu; \
    (cd apps/artcraft-launcher-web && trunk build --release --locked); \
    test -s dist/web/index.html; \
    cargo build --release --locked -p artcraft-launcher-server; \
    mkdir -p /out; \
    cp target/release/artcraft-launcher-server /out/; \
    cp -r dist/web /out/web

FROM debian:${DEBIAN_VERSION}-slim AS runtime
# ca-certificates: the server checks GitHub's certificate against the system's roots.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin artcraft \
    && mkdir -p /data /work \
    && chown artcraft:artcraft /data /work
COPY --from=build /out/artcraft-launcher-server /usr/local/bin/artcraft-launcher-server
COPY --from=build /out/web /srv/artcraft-launcher
COPY LICENSE-MIT LICENSE-APACHE NOTICE ATTRIBUTION.md /usr/share/doc/artcraft-launcher/
USER artcraft
ENV ARTCRAFT_LISTEN=0.0.0.0:8080 \
    ARTCRAFT_DATA_DIR=/data \
    ARTCRAFT_WORK_DIR=/work \
    ARTCRAFT_WEB_DIR=/srv/artcraft-launcher
# /data: installed apps and settings; /work: the apps' MCP work folders.
VOLUME ["/data", "/work"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["artcraft-launcher-server", "--healthcheck"]
CMD ["artcraft-launcher-server"]
