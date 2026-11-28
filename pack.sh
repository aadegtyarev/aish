#!/usr/bin/env bash
# Static musl binaries, tarballs and .deb packages in docker -> dist/
#   ./pack.sh [--arch x86_64|aarch64|all]
set -euo pipefail
cd "$(dirname "$0")"
arch=all
while (($#)); do
  case $1 in
    --arch) arch=${2:?--arch needs a value}; shift ;;
    -h|--help) sed -n '2,3p' "$0"; exit 0 ;;
    *) echo "pack.sh: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
case $arch in all) arch="x86_64 aarch64" ;; x86_64|aarch64) ;; *) echo "pack.sh: bad arch $arch" >&2; exit 2 ;; esac
docker build -q -t aish-pack build >/dev/null
user=(); docker info 2>/dev/null | grep -q rootless || user=(-u "$(id -u):$(id -g)")
mkdir -p dist
# shellcheck disable=SC2086 # $arch is a word list
exec docker run --rm "${user[@]}" -v "$PWD":/src -w /src \
  -v aish-cargo:/usr/local/cargo/registry \
  -v aish-pack-target:/pack-target \
  -e CARGO_HOME=/usr/local/cargo -e CARGO_TARGET_DIR=/pack-target -e HOME=/tmp \
  -e XDG_CACHE_HOME=/pack-target/cache \
  aish-pack ./build/pack-inner.sh $arch
