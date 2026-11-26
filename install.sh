#!/usr/bin/env bash
# Build (or take a prebuilt binary), install to ~/.local/bin/llmsh, then run `llmsh init`.
#   ./install.sh [--no-init] [--bin PATH] [--prefix DIR]
set -euo pipefail
cd "$(dirname "$0")"
init=1 bin= prefix=${PREFIX:-$HOME/.local}
while (($#)); do
  case $1 in
    --no-init) init=0 ;;
    --bin) bin=${2:?--bin needs a path}; shift ;;
    --prefix) prefix=${2:?--prefix needs a dir}; shift ;;
    -h|--help) sed -n '2,3p' "$0"; exit 0 ;;
    *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done
if [[ -z $bin ]]; then
  if command -v cargo >/dev/null 2>&1; then cargo build --release
  else ./dev.sh build --release; fi
  bin=target/release/llmsh
fi
[[ -x $bin ]] || { echo "install.sh: $bin is not an executable" >&2; exit 1; }
dest=$prefix/bin/llmsh
mkdir -p "$prefix/bin"
install -m 755 "$bin" "$dest"
echo "llmsh -> $dest"
case ":$PATH:" in *":$prefix/bin:"*) ;; *) echo "warning: $prefix/bin is not on PATH" >&2 ;; esac
if ((init)) && [[ -t 0 && -t 1 ]]; then
  PATH=$prefix/bin:$PATH exec "$dest" init
fi
