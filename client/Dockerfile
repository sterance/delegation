# ---- build stage ----
# Using the official rust image means a volunteer only needs Docker
# installed, not a matching Rust toolchain or glibc version on their host.
# This also sidesteps the whole "distro rustc is years out of date"
# problem from earlier entirely. Same version as rust-toolchain.toml.
FROM rust:1.99.1-slim-bookworm AS builder
WORKDIR /build
# Build context is now the repo root (see client/docker-compose.yml's
# `context: ..`), since this crate depends on the shared protocol/
# workspace member — COPY paths below are relative to that root, not
# to client/ itself.
COPY Cargo.toml Cargo.lock* ./
COPY protocol ./protocol
COPY client ./client
COPY server ./server
RUN cargo build --release -p delegation-client --bin delegation-client

# ---- runtime stage ----
FROM debian:bookworm-slim AS runtime
# `lspci` (pciutils, which also pulls in the pci.ids name database) turns
# AMD/Intel GPU PCI IDs into model names for the hardware report.
# rustls-tls-webpki-roots (enabled in Cargo.toml) bundles its own CA
# roots, so no ca-certificates package is needed purely for TLS to work.
# libssl3 and libgomp1 are for llama-server, which the client downloads
# (pinned release, checked by hash) the first time a model is installed.
# The release is built on Ubuntu; it needs glibc 2.34 and GCC 12's
# libstdc++, both of which bookworm has. The CUDA builds need nothing
# more here: the NVIDIA Container Toolkit mounts the driver
# (docker-compose.gpu.yml), and the CUDA runtime comes from the pinned
# `cudart` archive.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pciutils libssl3 libgomp1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/delegation-client /usr/local/bin/delegation-client
COPY client/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# The identity file (keypair + client_id) lives here. Mount a named
# volume at this path — without it, every `docker compose up` after a
# container recreation looks like a brand new, unenrolled machine.
VOLUME /root/.config/delegation-client
# Models and the llama.cpp runtimes, kept apart from the identity because
# they're large. docker-compose.yml mounts the `client-models` volume here.
VOLUME /root/.local/share/delegation-client

ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

# ---- AMD/Intel GPUs: the Vulkan loader and Mesa's drivers ----
# Only in this stage (docker-compose.amd.yml builds it), because Mesa pulls
# in LLVM: about 250 MB (47 packages on bookworm) that CPU-only and NVIDIA machines don't
# need.
FROM runtime AS vulkan
RUN apt-get update \
    && apt-get install -y --no-install-recommends libvulkan1 mesa-vulkan-drivers \
    && rm -rf /var/lib/apt/lists/*

# ---- the default image (last, so a plain build picks it) ----
FROM runtime AS standard
