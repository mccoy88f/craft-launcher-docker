# syntax=docker/dockerfile:1
# ArtCraft Launcher build service (cli-builder): builds a Craft App's MCP command-line tool from
# source for the launcher, when the app's release can't be confined to a work folder yet. See
# packaging/builder/build-loop.sh and packaging/docker/README.md.
#
# RUST_VERSION must stay >= the apps' `rust-version` (1.95 for DeckCraft and DesignCraft).
ARG RUST_VERSION=1.97
ARG DEBIAN_VERSION=bookworm

FROM rust:${RUST_VERSION}-${DEBIAN_VERSION}
# The same user as the launcher's, so both can use the shared builds folder.
RUN useradd --system --uid 10001 --home-dir /builds --shell /usr/sbin/nologin builder \
    && mkdir -p /builds \
    && chown builder:builder /builds
COPY packaging/builder/build-loop.sh /usr/local/bin/artcraft-cli-builder
USER builder
ENV ARTCRAFT_BUILDS_DIR=/builds \
    ARTCRAFT_BUILD_JOBS=2
VOLUME ["/builds"]
CMD ["sh", "/usr/local/bin/artcraft-cli-builder"]
