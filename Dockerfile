# syntax=docker/dockerfile:1.26.0@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

FROM rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS builder

ARG VERSION="dev"

WORKDIR /build

ENV SDSYNC_BUILD_VERSION=$VERSION

COPY Cargo.toml Cargo.lock ./
COPY crates/synology-drive-sync-ffi/Cargo.toml ./crates/synology-drive-sync-ffi/Cargo.toml
COPY crates/synology-drive-sync-ffi/src/lib.rs ./crates/synology-drive-sync-ffi/src/lib.rs
COPY build.rs ./
COPY src ./src
# `config init` embeds the starter configuration with include_str!, so the
# example is a build input rather than a documentation-only file.
COPY config.example.toml ./

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --locked --package synology-drive-sync --bin synology-drive-sync && \
    cp target/release/synology-drive-sync /tmp/synology-drive-sync

FROM debian:bookworm-slim@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171 AS runtime

ARG BUILD_DATE="unknown"
ARG VERSION="dev"
ARG VCS_REF="unknown"

LABEL org.opencontainers.image.title="synology-drive-sync" \
      org.opencontainers.image.description="One-way folder sync to Synology Drive through File Station WebAPI" \
      org.opencontainers.image.url="https://github.com/supermarsx/synology-drive-sync" \
      org.opencontainers.image.source="https://github.com/supermarsx/synology-drive-sync" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.created="$BUILD_DATE" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.revision="$VCS_REF"

RUN apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --system --gid 10001 sdsync && \
    useradd --system --uid 10001 --gid 10001 --home-dir /home/sdsync \
      --create-home --shell /usr/sbin/nologin sdsync

COPY --from=builder --chown=root:root /tmp/synology-drive-sync /usr/local/bin/synology-drive-sync
COPY --chown=root:root packaging/docker/entrypoint.sh /usr/local/bin/container-entrypoint
COPY --chown=root:root license.md /usr/share/licenses/synology-drive-sync/LICENSE
COPY --chown=root:root third_party_licenses.html /usr/share/licenses/synology-drive-sync/THIRD_PARTY_LICENSES.html

RUN chmod 0755 /usr/local/bin/synology-drive-sync /usr/local/bin/container-entrypoint

USER 10001:10001
WORKDIR /work

ENV HOME=/home/sdsync

ENTRYPOINT ["/usr/local/bin/container-entrypoint"]
CMD ["--help"]

# The CLI runs finite sync jobs. This side-effect-free executable probe does not
# require a configured DSM endpoint.
HEALTHCHECK --interval=5m --timeout=10s --start-period=5s --retries=1 \
  CMD ["/usr/local/bin/synology-drive-sync", "--version"]

STOPSIGNAL SIGTERM
