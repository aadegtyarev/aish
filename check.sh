#!/usr/bin/env bash
# Every local (and CI) check, in docker. ./check.sh [--quick] | bench
#   --quick: fmt, clippy and bash 5.2 tests only.  bench: PTY startup/echo latency vs plain bash.
set -euo pipefail
cd "$(dirname "$0")"
user=(); docker info 2>/dev/null | grep -q rootless || user=(-u "$(id -u):$(id -g)")
image=aish-check
docker image inspect $image >/dev/null 2>&1 || docker build -q -t $image -f build/Dockerfile.check build >&2
# Run a command in the tools image (rust + deny, machete, typos, shellcheck, actionlint).
tools() {
  docker run --rm "${user[@]}" -v "$PWD":/src -w /src \
    -e CARGO_HOME=/src/target/cargo-home -e CARGO_TARGET_DIR=/src/target -e HOME=/tmp $image "$@"
}
step() { echo "== $1"; shift; "$@" || { echo "FAILED: $*" >&2; exit 1; }; }

if [ "${1:-}" = bench ]; then
  tools sh -c 'cargo build --release -q && python3 build/bench.py target/release/aish'
  exit
fi

step fmt ./dev.sh fmt --all --check
step clippy ./dev.sh clippy --workspace --all-targets -- -D warnings
step "test (bash 5.2)" ./dev.sh test --workspace
[ "${1:-}" = --quick ] && exit
step "test (bash 5.3)" env AISH_BASH53=1 ./dev.sh test --workspace
step deny tools cargo deny check
step machete tools cargo machete
step typos tools typos
step shellcheck tools shellcheck dev.sh pack.sh install.sh check.sh build/*.sh
step actionlint tools actionlint
echo "all checks passed"
