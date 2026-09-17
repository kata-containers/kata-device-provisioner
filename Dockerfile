# ── build stage ────────────────────────────────────────────────────────────────
FROM rust:1.95-slim AS builder

# The build context has no .git — the Makefile passes the commit in.
ARG GIT_SHA=unknown
ENV GIT_SHA=$GIT_SHA

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    git \
    musl-tools \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
# No glob on Cargo.lock: it is committed, and --locked below is meaningless
# if a missing lockfile silently passes the COPY.
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN target="$(uname -m)-unknown-linux-musl" \
    && rustup target add "$target" \
    && cargo build --release --locked --target "$target" \
    && cp "target/$target/release/kata-device-provisioner" /kata-device-provisioner

# ── runtime stage ──────────────────────────────────────────────────────────────
FROM gcr.io/distroless/static-debian12

LABEL org.opencontainers.image.source="https://github.com/kata-containers/kata-device-provisioner" \
      org.opencontainers.image.description="Sets CC mode and VFIO binding on Kata GPU nodes, then exits"

COPY --from=builder /kata-device-provisioner /kata-device-provisioner

ENTRYPOINT ["/kata-device-provisioner"]
