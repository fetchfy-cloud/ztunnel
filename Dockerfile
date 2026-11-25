# Image of the ztunnel of this fork: the official image of the same Istio
# version with the binary replaced, so that everything else (iptables, CA
# certificates, ISTIO_META_ISTIO_VERSION, entrypoint) is what the Istio charts
# expect.
#   https://github.com/istio/istio/blob/release-1.32/pilot/docker/Dockerfile.ztunnel
#
# The official image runs Ubuntu 24.04 (glibc 2.39); the binary is built on
# Debian bookworm (glibc 2.36) so that it links against an older glibc. Rust is
# the version of the Istio build-tools image for 1.32.

ARG ISTIO_VERSION=1.32.0

FROM docker.io/library/rust:1.97.1-bookworm AS build
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends protobuf-compiler cmake clang \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release && cp out/rust/release/ztunnel /ztunnel

FROM docker.io/istio/ztunnel:${ISTIO_VERSION}-distroless
COPY --from=build /ztunnel /usr/local/bin/ztunnel
