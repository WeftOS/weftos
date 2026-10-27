# Self-contained image for weft (clawft CLI).
#
# Builds the static musl `weft` binary from source in a multi-stage build
# — no dependency on published GitHub Release assets. The previous image
# downloaded a release tarball, which coupled the image tag to
# already-published binaries and broke whenever a (patch) release did not
# rebuild them (e.g. cargo-dist skipping the binary matrix). Building from
# the build context makes the image reproducible from any commit/tag.
#
# Build (from the repo root):
#   docker build -t weft:dev .
# Target: a small static-musl runtime image.

# ── Stage 1: build weft (static musl, default features) ───────────────
FROM rust:1.93-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
# Default features = channels + services + delegate + api (gateway-capable).
# The workspace reqwest uses rustls-tls, so this links fully static with
# no openssl — runs on a bare alpine runtime.
# `weaver` is the kernel daemon the gateway's /api/health reports on; the
# image runs one beside the gateway (scripts/docker/entrypoint.sh).
RUN cargo build --release -p clawft-cli --bin weft -p clawft-weave --bin weaver

# ── Stage 2: minimal runtime ──────────────────────────────────────────
FROM alpine:3.21

ARG VERSION=dev
LABEL org.opencontainers.image.title="weft" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.source="https://github.com/WeftOS/weftos"

RUN apk add --no-cache ca-certificates tzdata \
    && adduser -D -h /home/weft weft

COPY --from=builder /build/target/release/weft /usr/local/bin/weft
COPY --from=builder /build/target/release/weaver /usr/local/bin/weaver
COPY scripts/docker/entrypoint.sh /usr/local/bin/weft-entrypoint

# Pre-create .clawft weft-owned BEFORE the VOLUME — Docker would otherwise
# create the unmounted mountpoint root-owned and `weft gateway` (uid 1000)
# would die at "bootstrap app context: Permission denied". Also ship a
# default config enabling the auth-gated REST/WS API on the exposed port
# 8080 so `weft gateway` runs out-of-the-box (it refuses to start with no
# channel and the API off). The API stays Bearer-token gated; only
# /api/health and the token-bootstrap path are public. The bind is 0.0.0.0
# because the gateway default is loopback (ADR-102), which a published
# container port cannot reach; plain HTTP is acknowledged because the
# container boundary is the TLS-termination point (publish with
# `-p 127.0.0.1:8080:8080` or front it with a TLS proxy).
RUN mkdir -p /home/weft/.clawft /home/weft/.weftos \
    && printf '{"gateway":{"host":"0.0.0.0","api_enabled":true,"api_port":8080,"dangerously_plain_http":true}}\n' \
       > /home/weft/.clawft/config.json \
    && chown -R weft:weft /home/weft/.clawft /home/weft/.weftos

USER weft
WORKDIR /home/weft

VOLUME ["/home/weft/.clawft", "/home/weft/.weftos"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/weft", "status"]

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/weft-entrypoint"]
CMD ["gateway"]
