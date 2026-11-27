#!/usr/bin/env bash
# Run cargo inside the rust container: ./dev.sh build | test | clippy ...
set -euo pipefail
cd "$(dirname "$0")"
# Rootless docker maps container root to the host user; otherwise run as the host user.
user=(); docker info 2>/dev/null | grep -q rootless || user=(-u "$(id -u):$(id -g)")
# AISH_BASH53=1: same, in an image with bash 5.3 (own target dir), to match CI and
# current distros: AISH_BASH53=1 ./dev.sh test --workspace
image=rust:1.93.1-bookworm; target=/src/target
if [ -n "${AISH_BASH53:-}" ]; then
  image=aish-dev-bash53; target=/src/target-sid
  docker image inspect "$image" >/dev/null 2>&1 \
    || docker build -q -t "$image" -f build/Dockerfile.bash53 build >&2
fi
exec docker run --rm -i $([ -t 0 ] && echo -t) \
  "${user[@]}" \
  -v "$PWD":/src -w /src \
  -v aish-cargo:/usr/local/cargo/registry \
  -e CARGO_HOME=/usr/local/cargo -e CARGO_TARGET_DIR=$target \
  $image sh -c '
    case "$1" in fmt|clippy) rustup component add rustfmt clippy >/dev/null 2>&1 || true ;; esac
    exec cargo "$@"' sh "$@"
