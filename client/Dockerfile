# ---- build stage ----
# Using the official rust image means a volunteer only needs Docker
# installed, not a matching Rust toolchain or glibc version on their host.
# This also sidesteps the whole "distro rustc is years out of date"
# problem from earlier entirely: this image ships a current stable Rust,
# so the version pins in Cargo.toml (added to work around this project's
# sandboxed prototyping environment) aren't even necessary here — they're
# left in place because they don't hurt, but feel free to loosen them.
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
# Build context is now the repo root (see client/docker-compose.yml's
# `context: ..`), since this crate depends on the shared protocol/
# workspace member — COPY paths below are relative to that root, not
# to client/ itself.
COPY Cargo.toml Cargo.lock* ./
COPY protocol ./protocol
COPY client ./client
COPY server ./server
RUN cargo build --release -p delegation-client

# ---- runtime stage ----
FROM debian:bookworm-slim
# `hostname` is used by the client to label its heartbeats; debian-slim
# doesn't ship it by default. rustls-tls-webpki-roots (already enabled in
# Cargo.toml) bundles its own CA roots, so no ca-certificates package is
# needed here purely for TLS to work.
RUN apt-get update \
    && apt-get install -y --no-install-recommends hostname \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/delegation-client /usr/local/bin/delegation-client
COPY client/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# The identity file (keypair + client_id) lives here. Mount a named
# volume at this path — without it, every `docker compose up` after a
# container recreation looks like a brand new, unenrolled machine.
VOLUME /root/.config/delegation-client

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
