# Release build image: musl targets + zig (linker for aarch64) + cargo-zigbuild + cargo-deb.
FROM rust:1.93.1-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends python3-pip xz-utils \
    && pip3 install --break-system-packages ziglang==0.13.0 \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    && cargo install --locked cargo-zigbuild cargo-deb
