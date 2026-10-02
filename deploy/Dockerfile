# Knowell container image.
#
# Build context: the repository root.
#   docker build -f deploy/Dockerfile -t knowell .                      # from source
#   docker build -f deploy/Dockerfile --target runtime-prebuilt \
#       -t knowell .                                                    # release workflow
#
# `runtime` (the default target) compiles `know` from source. `runtime-prebuilt` only
# copies release binaries from docker-bin/linux/<amd64|arm64>/know, so the image holds
# exactly the binary that was tested, checksummed and attested.
#
# Base images are pinned by digest; Dependabot (docker ecosystem) proposes updates.

ARG RUST_IMAGE=rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663
ARG NODE_IMAGE=node:24-bookworm-slim@sha256:0e0ff40c39bc087845bfb27465a0df4ea419520094bc35842ff83dd8cbe6f9b6
ARG RUNTIME_IMAGE=debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

# --- panel: static SPA ------------------------------------------------------------------
FROM ${NODE_IMAGE} AS panel
WORKDIR /panel
COPY panel/package.json panel/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-audit --no-fund
COPY panel/ ./
RUN npm run build

# --- build: the `know` binary -----------------------------------------------------------
FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY . .
COPY --from=panel /panel/dist ./panel/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p knowell \
    && install -D -m 0755 target/release/know /out/know

# --- runtime-base: what both runtime targets share --------------------------------------
FROM ${RUNTIME_IMAGE} AS runtime-base
# ca-certificates: HTTPS to embedding providers and git remotes.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 knowell \
    && useradd --system --uid 10001 --gid 10001 --home-dir /data --shell /usr/sbin/nologin knowell \
    && install -d -o knowell -g knowell -m 0700 /data /data/.knowell
# HOME=/data puts the default data and config location (~/.knowell) on the volume;
# /data/.knowell is created here so the volume (and a mounted config.toml) stay writable
# by the non-root user.
ENV HOME=/data
WORKDIR /data
VOLUME ["/data"]
USER knowell:knowell
LABEL org.opencontainers.image.source="https://github.com/knowell-dev/knowell" \
      org.opencontainers.image.description="Knowell: semantic code intelligence and shared memory for AI coding agents" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
ENTRYPOINT ["know"]
CMD ["serve", "--role", "hub"]

# --- runtime-prebuilt: release binaries (used by the release workflow) -------------------
FROM runtime-base AS runtime-prebuilt
ARG TARGETOS
ARG TARGETARCH
COPY --chmod=0755 docker-bin/${TARGETOS}/${TARGETARCH}/know /usr/local/bin/know

# --- runtime: built from source (default target) ----------------------------------------
FROM runtime-base AS runtime
COPY --from=build --chmod=0755 /out/know /usr/local/bin/know
