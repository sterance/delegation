# Dashboard build. Node exists only in this stage — the runtime image
# below receives nothing but the static dist/ output, and no Node
# process ever runs in production. package*.json are copied first so
# the `npm ci` layer stays cached until dependencies actually change.
FROM node:20-alpine AS frontend
WORKDIR /build/frontend
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend ./
RUN npm run build

# rusqlite's `bundled` feature compiles SQLite from source as part of
# `cargo build`, which needs a C compiler in the builder stage —
# build-essential provides that. The runtime stage needs nothing extra:
# bundled SQLite is statically linked into the binary, so there's no
# system SQLite dependency and no musl/Alpine native-build story here
# at all (this is the thing that made the node:sqlite-vs-better-sqlite3
# question from the Node version moot).
FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY protocol ./protocol
COPY server ./server
COPY client ./client
RUN cargo build --release -p delegation-server

FROM debian:bookworm-slim
COPY --from=builder /build/target/release/delegation-server /usr/local/bin/delegation-server
COPY --from=frontend /build/frontend/dist /app/frontend/dist
ENV FRONTEND_DIR=/app/frontend/dist

# SQLite database lives here — mount a volume at this path or every
# container recreation starts with a totally empty client registry.
VOLUME /app/data
ENV DB_PATH=/app/data/delegation.db
WORKDIR /app

EXPOSE 7070
CMD ["delegation-server"]
