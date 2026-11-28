#!/usr/bin/env bash
# Run cargo inside the rust container: ./dev.sh build | test | clippy ...
set -euo pipefail
cd "$(dirname "$0")"
# Rootless docker maps container root to the host user; otherwise run as the host user.
tty=(); [ -t 0 ] && tty=(-t)
user=(); docker info 2>/dev/null | grep -q rootless || user=(-u "$(id -u):$(id -g)")
# AISH_BASH53=1: same, in an image with bash 5.3 (own target dir), to match CI and
# current distros: AISH_BASH53=1 ./dev.sh test --workspace
image=rust:1.93.1-bookworm; target=/src/target
if [ -n "${AISH_BASH53:-}" ]; then
  image=aish-dev-bash53; target=/src/target-sid
  docker image inspect "$image" >/dev/null 2>&1 \
    || docker build -q -t "$image" -f build/Dockerfile.bash53 build >&2
fi
exec docker run --rm -i "${tty[@]}" \
  "${user[@]}" \
  -v "$PWD":/src -w /src \
  -e CARGO_HOME=/src/target/cargo-home -e CARGO_TARGET_DIR=$target \
  $image sh -c '
    case "$1" in fmt|clippy) rustup component add rustfmt clippy >/dev/null 2>&1 || true ;; esac
    exec cargo "$@"' sh "$@"
