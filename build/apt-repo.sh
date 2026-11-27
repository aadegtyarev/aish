#!/usr/bin/env bash
# Build/refresh a signed apt repository tree: build/apt-repo.sh <repo-dir> <deb files...>
# Needs apt-ftparchive (apt-utils) and gpg; the signing key comes from $APT_SIGNING_KEY
# (ASCII-armored secret key, no passphrase). Older .debs already in <repo-dir>/pool stay.
set -euo pipefail
repo=${1:?usage: apt-repo.sh <repo-dir> <deb files...>}; shift
(($#)) || { echo "apt-repo.sh: no .deb files" >&2; exit 2; }
: "${APT_SIGNING_KEY:?APT_SIGNING_KEY is empty}"
here=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$repo/pool/main/a/aish"
cp -f "$@" "$repo/pool/main/a/aish/"
cp -f "$here/install.sh" "$repo/install.sh"
cat > "$repo/index.html" <<'HTML'
<!doctype html><meta charset="utf-8"><title>aish</title>
<h1>aish</h1>
<p>bash with LLM help. Install:</p>
<pre>curl -fsSL https://aadegtyarev.github.io/aish/install.sh | bash</pre>
<p><a href="https://github.com/aadegtyarev/aish">github.com/aadegtyarev/aish</a></p>
HTML
touch "$repo/.nojekyll"

cd "$repo"
for arch in amd64 arm64; do
  d=dists/stable/main/binary-$arch
  mkdir -p "$d"
  apt-ftparchive --arch "$arch" packages pool > "$d/Packages"
  gzip -9nf -k "$d/Packages"
done
rm -f dists/stable/Release dists/stable/InRelease dists/stable/Release.gpg
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=aish \
  -o APT::FTPArchive::Release::Label=aish \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Architectures="amd64 arm64" \
  -o APT::FTPArchive::Release::Components=main \
  release dists/stable > dists/stable/Release.tmp
mv dists/stable/Release.tmp dists/stable/Release

gnupghome=$(mktemp -d)
trap 'rm -rf "$gnupghome"' EXIT
chmod 700 "$gnupghome"
export GNUPGHOME=$gnupghome
printf '%s\n' "$APT_SIGNING_KEY" | gpg --batch --quiet --import
fpr=$(gpg --batch --list-secret-keys --with-colons | awk -F: '/^fpr:/{print $10; exit}')
sign=(gpg --batch --yes --pinentry-mode loopback --passphrase '' -u "$fpr")
"${sign[@]}" --clearsign -o dists/stable/InRelease dists/stable/Release
"${sign[@]}" --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release
gpg --batch --yes --armor --export "$fpr" > aish.asc
echo "apt repo ready: $repo ($(find pool -name '*.deb' | wc -l) packages)"
