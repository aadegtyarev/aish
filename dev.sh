#!/usr/bin/env bash
# Run cargo inside the rust container: ./dev.sh build | test | clippy ...
set -euo pipefail
cd "$(dirname "$0")"
# Rootless docker maps container root to the host user; otherwise run as the host user.
user=(); docker info 2>/dev/null | grep -q rootless || user=(-u "$(id -u):$(id -g)")
exec docker run --rm -i $([ -t 0 ] && echo -t) \
  "${user[@]}" \
  -v "$PWD":/src -w /src \
  -v aish-cargo:/usr/local/cargo/registry \
  -e CARGO_HOME=/usr/local/cargo -e CARGO_TARGET_DIR=/src/target ${NARROW_UTF8:+-e NARROW_UTF8} \
  rust:1.93.1-bookworm sh -c '
    case "$1" in fmt|clippy) rustup component add rustfmt clippy >/dev/null 2>&1 || true ;; esac
    exec cargo "$@"' sh "$@"
