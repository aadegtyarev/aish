#!/usr/bin/env bash
# Install aish. From a checkout: build (or --bin PATH) into ~/.local/bin. Piped from the web
# (curl -fsSL https://aadegtyarev.github.io/aish/install.sh | bash): apt on Debian/Ubuntu,
# else the release tarball. Then `aish init` when there is a terminal.
#   install.sh [--no-init] [--yes] [--bin PATH] [--prefix DIR]
set -euo pipefail
site=https://aadegtyarev.github.io/aish
repo=https://github.com/aadegtyarev/aish
tmp=
die() { echo "install.sh: $*" >&2; exit 1; }
cleanup() { [[ -n $tmp ]] && rm -rf "$tmp"; tmp=; }
trap cleanup EXIT
ask() { # ask "question" -> 0 on yes; reads the terminal even under curl | bash
  ((yes)) && return 0
  { : </dev/tty; } 2>/dev/null || die "no terminal to ask on; rerun with --yes"
  local a; read -r -p "$1 [y/N] " a </dev/tty
  [[ $a == [yY]* ]]
}

install_apt() {
  echo "This adds the aish apt repository (key in /etc/apt/keyrings/aish.asc,"
  echo "source in /etc/apt/sources.list.d/aish.list) and runs: sudo apt update && sudo apt install aish"
  ask "Continue with sudo?" || die "cancelled"
  tmp=$(mktemp -d)
  curl -fsSL "$site/aish.asc" -o "$tmp/aish.asc"
  sudo install -d -m 755 /etc/apt/keyrings
  sudo install -m 644 "$tmp/aish.asc" /etc/apt/keyrings/aish.asc
  echo "deb [arch=amd64,arm64 signed-by=/etc/apt/keyrings/aish.asc] $site stable main" | sudo tee /etc/apt/sources.list.d/aish.list >/dev/null
  sudo apt-get update
  sudo apt-get install -y aish
  cleanup
  dest=/usr/bin/aish
  local first; first=$(command -v aish || true)
  [[ $first == "$dest" ]] || echo "warning: 'aish' on PATH is $first, not $dest; the package one is $dest" >&2
}

install_tarball() {
  local arch ver name
  case $(uname -m) in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) die "unsupported architecture $(uname -m)" ;;
  esac
  tmp=$(mktemp -d)
  ver=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$repo/releases/latest")
  ver=${ver##*/v}; [[ $ver == [0-9]* ]] || die "cannot find the latest release"
  name=aish-$ver-$arch-linux.tar.gz
  curl -fsSL "$repo/releases/download/v$ver/$name" -o "$tmp/$name"
  curl -fsSL "$repo/releases/download/v$ver/SHA256SUMS" -o "$tmp/SHA256SUMS"
  (cd "$tmp" && grep -F " $name" SHA256SUMS | sha256sum -c - >/dev/null) || die "checksum mismatch for $name"
  tar -xzf "$tmp/$name" -C "$tmp"
  bin=$tmp/aish-$ver-$arch-linux/aish
  install_bin
}

install_bin() {
  [[ -x $bin ]] || die "$bin is not an executable"
  mkdir -p "$prefix/bin"
  install -m 755 "$bin" "$dest"
  echo "aish -> $dest"
  case ":$PATH:" in *":$prefix/bin:"*) ;; *) echo "warning: $prefix/bin is not on PATH" >&2 ;; esac
}

main() {
  local init=1 yes=0 bin="" prefix=${PREFIX:-$HOME/.local} src="" dest
  while (($#)); do
    case $1 in
      --no-init) init=0 ;;
      --yes|-y) yes=1 ;;
      --bin) bin=${2:?--bin needs a path}; shift ;;
      --prefix) prefix=${2:?--prefix needs a dir}; shift ;;
      -h|--help) echo "usage: install.sh [--no-init] [--yes] [--bin PATH] [--prefix DIR]"; exit 0 ;;
      *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
    esac
    shift
  done
  dest=$prefix/bin/aish
  # A checkout only when this runs as a real file next to Cargo.toml (never curl | bash).
  if [[ -f ${BASH_SOURCE[0]:-} && -f $(dirname "${BASH_SOURCE[0]}")/Cargo.toml ]]; then
    src=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
  fi
  if [[ -n $src ]]; then
    cd "$src"
    if [[ -z $bin && -x ./aish && ! -d ./aish ]]; then bin=./aish; fi   # release tarball
    if [[ -z $bin ]]; then
      if command -v cargo >/dev/null 2>&1; then cargo build --release
      else ./dev.sh build --release; fi
      bin=target/release/aish
    fi
    install_bin
  else
    command -v curl >/dev/null 2>&1 || die "curl is required"
    local dpa=""
    command -v dpkg >/dev/null 2>&1 && dpa=$(dpkg --print-architecture 2>/dev/null || true)
    if [[ -z $bin && -r /etc/os-release && ( $dpa == amd64 || $dpa == arm64 ) ]] \
      && grep -qiE '^ID(_LIKE)?=.*(debian|ubuntu)' /etc/os-release \
      && command -v apt-get >/dev/null 2>&1 && command -v sudo >/dev/null 2>&1; then
      install_apt
    elif [[ -n $bin ]]; then install_bin
    else install_tarball
    fi
  fi
  if ((init)) && { : </dev/tty; } 2>/dev/null; then
    cleanup
    PATH=$(dirname "$dest"):$PATH exec "$dest" init </dev/tty >/dev/tty
  fi
}

main "$@"
