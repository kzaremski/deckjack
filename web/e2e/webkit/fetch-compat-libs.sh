#!/bin/sh
# Fetch the three Ubuntu 24.04 libraries Playwright's WebKit needs on a host
# that does not have them at those sonames (Arch, for one), without root.
#
#   ICU 74            libicu74_74.2-1ubuntu3.1           (noble-updates/main)
#   libxml2.so.2      libxml2_2.9.14+dfsg-1.3ubuntu3.10   (noble-security/main)
#   flite 2.2         libflite1_2.2-6build3               (noble/universe)
#
# `sudo npx playwright install-deps` is the supported route, and it only
# knows apt. This is the same set of packages, from Ubuntu's own archive,
# unpacked into a cache directory instead of /usr, and found by WebKit
# through `minibrowser.sh`. Nothing outside the target directory is touched.
#
# Each package is pinned by SHA-256, taken from the archive's Packages index
# on 2026-11-16. A superseded package can vanish from the archive's pool; if a
# download 404s, take the new Filename and SHA256 for the same package from
#   https://archive.ubuntu.com/ubuntu/dists/<suite>/<component>/binary-amd64/Packages.gz
# and update the table below. The pin is what makes a changed file fail
# rather than load.
#
# Usage: fetch-compat-libs.sh [TARGET_DIR]
# Default target: ${XDG_CACHE_HOME:-$HOME/.cache}/deckjack-e2e/webkit-compat
# which is where web/e2e/playwright.config.ts looks.
set -eu

target="${1:-${XDG_CACHE_HOME:-$HOME/.cache}/deckjack-e2e/webkit-compat}"
archive="https://archive.ubuntu.com/ubuntu"

case "$(uname -m)" in
  x86_64) ;;
  *)
    echo "fetch-compat-libs: only x86_64 is pinned here; $(uname -m) is not" >&2
    exit 1
    ;;
esac

for tool in curl sha256sum bsdtar; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "fetch-compat-libs: needs $tool on PATH" >&2
    exit 1
  fi
done

# A fresh, empty directory for the downloads, removed on the way out.
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM
mkdir "$work/debs" "$work/root"

fetch() {
  path="$1"
  sum="$2"
  file="$work/debs/$(basename "$path")"
  echo "fetching $(basename "$path")"
  curl -fsSL --proto '=https' --tlsv1.2 --max-time 600 -o "$file" "$archive/$path"
  echo "$sum  $file" | sha256sum -c --quiet -
  # A .deb is an ar archive holding data.tar.*; bsdtar reads both.
  bsdtar -xOf "$file" 'data.tar.*' | bsdtar -xf - -C "$work/root"
}

fetch pool/main/i/icu/libicu74_74.2-1ubuntu3.1_amd64.deb \
  c9a70989678660eed9a1e904c74fa043da8bec8e2036856fc16e31ced79b04f8
fetch pool/main/libx/libxml2/libxml2_2.9.14+dfsg-1.3ubuntu3.10_amd64.deb \
  c5a065f85c1e6f605741e365e40cad71c3ceb0e85e2791400bd35b8896e12ad1
fetch pool/universe/f/flite/libflite1_2.2-6build3_amd64.deb \
  367f1d0da5cd38759a0515eafc27aa133b2d7bf99308cac34831df0212e96b75

# Only the shared libraries: no docs, no binaries, no maintainer scripts.
mkdir -p "$target"
cp -a "$work/root/usr/lib/x86_64-linux-gnu/." "$target/"
echo "WebKit compat libraries in $target"
