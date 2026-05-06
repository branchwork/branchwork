# ── Stage 1: Build frontend ──────────────────────────────────────────
# JS/CSS is architecture-independent, so we build it once on
# $BUILDPLATFORM (always amd64 in CI) and feed the dist/ into both
# arch slices of stage 2. Cross-arch web optimization is out of scope.
FROM --platform=$BUILDPLATFORM node:20-alpine AS web
WORKDIR /src
RUN corepack enable && corepack prepare pnpm@9 --activate

# Install deps first (cache-friendly)
COPY pnpm-workspace.yaml pnpm-lock.yaml package.json ./
COPY web/package.json web/
RUN pnpm install --frozen-lockfile

# Build
COPY web/ web/
COPY tsconfig.base.json ./
RUN pnpm --filter @branchwork/web build

# ── Stage 2: Build Rust server (cross-compile via zig) ──────────────
# Pinned to $BUILDPLATFORM (amd64) so the host toolchain runs natively
# and we cross-compile to both arches via cargo-zigbuild. Avoids the
# QEMU emulation tax on the arm64 cargo build (was ~76m wall clock,
# see docs/build-perf-2026-05-05-baseline.md).
FROM --platform=$BUILDPLATFORM rust:1.88-alpine AS server
RUN apk add --no-cache \
    musl-dev build-base perl \
    python3 py3-pip \
    curl xz

# Install zig (cross-linker + libc) and cargo-zigbuild wrapper.
RUN curl -L https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz \
    | tar -xJ -C /usr/local && \
    ln -s /usr/local/zig-linux-x86_64-0.13.0/zig /usr/local/bin/zig
RUN cargo install --locked cargo-zigbuild

# Pre-add the rust-std for both targets so cargo doesn't fetch them
# during the build layer.
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

WORKDIR /src/server-rs

# Cache deps: copy manifests, create dummy src for every bin target,
# build deps only. The `[[bin]] branchwork-runner` declaration plus
# auto-discovered src/bin/session_daemon.rs require their own stubs
# alongside src/main.rs.
COPY server-rs/Cargo.toml server-rs/Cargo.lock ./
RUN mkdir -p src/bin src/agents src/api src/auth src/mcp src/saas && \
    echo 'fn main() {}' > src/main.rs && \
    echo 'fn main() {}' > src/bin/branchwork_runner.rs && \
    echo 'fn main() {}' > src/bin/session_daemon.rs
RUN cargo zigbuild \
    --target x86_64-unknown-linux-musl \
    --target aarch64-unknown-linux-musl \
    --release 2>/dev/null || true

# Real source + frontend dist
COPY server-rs/ ./
COPY --from=web /src/web/dist /src/web/dist
# Asset embedded by `include_str!` in src/saas/install_runner.rs. The
# relative path resolves to /src/deploy/install-runner.sh from inside
# the crate, so it must live alongside the server-rs tree.
COPY deploy/install-runner.sh /src/deploy/install-runner.sh

# Touch our crate sources to invalidate cargo's bin-level cache (deps
# stay cached), then build both bins for both targets in a single
# zigbuild invocation.
RUN touch src/main.rs src/bin/branchwork_runner.rs src/bin/session_daemon.rs && \
    cargo zigbuild \
      --target x86_64-unknown-linux-musl \
      --target aarch64-unknown-linux-musl \
      --release \
      --bin branchwork-server \
      --bin branchwork-runner

# Re-shape per-arch outputs into a stable /out/<docker-arch>/ tree so
# stage 3 can slice with a single COPY using $TARGETARCH (which is
# Docker's "amd64"/"arm64", not Rust's "x86_64"/"aarch64").
RUN mkdir -p /out/amd64 /out/arm64 && \
    cp target/x86_64-unknown-linux-musl/release/branchwork-server  /out/amd64/ && \
    cp target/x86_64-unknown-linux-musl/release/branchwork-runner  /out/amd64/ && \
    cp target/aarch64-unknown-linux-musl/release/branchwork-server /out/arm64/ && \
    cp target/aarch64-unknown-linux-musl/release/branchwork-runner /out/arm64/

# ── Stage 3: Minimal runtime image (per-arch slice) ─────────────────
# `$TARGETARCH` is "amd64" or "arm64" depending on which platform
# buildx is materializing this layer for. Stage 2 already mapped Rust
# triples → docker arch names under /out/, so this is a single COPY.
#
# Brief example used `FROM scratch`; we keep alpine here because the
# binaries shell out to git/gh at runtime and need ca-certificates +
# tini for proper signal handling. Static-linked musl binaries don't
# need libc, so the alpine layer is purely for the runtime utilities.
FROM alpine:3.21 AS runtime
ARG TARGETARCH
RUN apk add --no-cache git ca-certificates tini && \
    addgroup -g 1000 branchwork && \
    adduser -u 1000 -G branchwork -D branchwork

COPY --from=server /out/${TARGETARCH}/branchwork-server /usr/local/bin/
COPY --from=server /out/${TARGETARCH}/branchwork-runner /usr/local/bin/

# Data dir for the server (plans, DB, sockets) and home dir for the runner
# (outbox DB, session sockets). Both owned by `branchwork` so a volume
# mount on either path inherits the right uid/gid.
RUN mkdir -p /data /home/runneruser && \
    chown branchwork:branchwork /data /home/runneruser
VOLUME ["/data"]

USER branchwork
EXPOSE 3100

ENV BRANCHWORK_DATA_DIR=/data

ENTRYPOINT ["tini", "--"]
CMD ["branchwork-server", "--port", "3100", "--claude-dir", "/data"]
