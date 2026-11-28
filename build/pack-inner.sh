#!/usr/bin/env bash
# Runs inside the aish-pack image (also used by the release workflow): ./build/pack-inner.sh ARCH...
set -euo pipefail
ver=$(cargo metadata --no-deps --format-version 1 | python3 -c \
  'import sys,json;print([p for p in json.load(sys.stdin)["packages"] if p["name"]=="aish"][0]["version"])')
T=${CARGO_TARGET_DIR:-target}
rm -rf dist; mkdir -p dist
for a in "$@"; do
  case $a in x86_64) deb=amd64 ;; aarch64) deb=arm64 ;; esac
  triple=$a-unknown-linux-musl
  cargo zigbuild --release --locked --target "$triple"
  bin=$T/$triple/release/aish
  # cargo-deb reads target/release/aish per the asset path; --target rewrites it itself
  cargo deb --no-build --no-strip --target "$triple" --output "dist/aish_${ver}_${deb}.deb"
  d=aish-$ver-$a-linux
  rm -rf "/tmp/$d"; mkdir -p "/tmp/$d"
  cp "$bin" README.md LICENSE install.sh "/tmp/$d/"
  tar -C /tmp -czf "dist/$d.tar.gz" "$d"
  ls -l "$bin"
done
(cd dist && sha256sum -- * > SHA256SUMS)
