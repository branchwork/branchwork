# ── Stage 1: Build frontend ──────────────────────────────────────────
FROM node:20-alpine AS web
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

# ── Stage 2: Build Rust server ───────────────────────────────────────
FROM rust:1.88-alpine AS server
RUN apk add --no-cache musl-dev build-base perl

WORKDIR /src/server-rs

# Cache deps: copy manifests, create dummy src, build deps only
COPY server-rs/Cargo.toml server-rs/Cargo.lock ./

# Create minimal stubs so cargo can resolve the dependency graph
RUN mkdir -p src/bin src/agents src/api src/auth src/mcp src/saas && \
    echo 'fn main() {}' > src/main.rs && \
    echo 'fn main() {}' > src/bin/branchwork_runner.rs && \
    echo 'fn main() {}' > src/bin/session_daemon.rs

# Pre-build deps (this layer is cached until Cargo.toml/lock change)
RUN cargo build --release 2>/dev/null || true

# Copy real source + frontend dist
COPY server-rs/ ./
COPY --from=web /src/web/dist /src/web/dist

# Touch our crate sources to force a rebuild (not deps), then build both bins
RUN touch src/main.rs src/bin/branchwork_runner.rs src/bin/session_daemon.rs && \
    cargo build --release --bin branchwork-server --bin branchwork-runner

# ── Stage 3: Minimal runtime image ──────────────────────────────────
FROM alpine:3.21 AS runtime
RUN apk add --no-cache git ca-certificates tini && \
    addgroup -g 1000 branchwork && \
    adduser -u 1000 -G branchwork -D branchwork

COPY --from=server /src/server-rs/target/release/branchwork-server /usr/local/bin/
COPY --from=server /src/server-rs/target/release/branchwork-runner /usr/local/bin/

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
