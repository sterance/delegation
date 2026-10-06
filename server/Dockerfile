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

# SQLite database lives here — mount a volume at this path or every
# container recreation starts with a totally empty client registry.
VOLUME /app/data
ENV DB_PATH=/app/data/delegation.db
WORKDIR /app

EXPOSE 7070
CMD ["delegation-server"]
