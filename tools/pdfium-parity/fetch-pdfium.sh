#!/bin/sh
# Downloads the pinned pdfium build for this machine into the user cache, once, and prints the
# path of its library. The builds come from github.com/bblanchon/pdfium-binaries; each one's
# SHA-256 is pinned here. Keep the version in step with pdfium-render's `pdfium_*` feature in
# Cargo.toml.
set -eu

version=7881
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    asset=pdfium-linux-x64.tgz
    sha256=1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d
    library=libpdfium.so
    ;;
  Darwin-arm64)
    asset=pdfium-mac-arm64.tgz
    sha256=52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40
    library=libpdfium.dylib
    ;;
  Darwin-x86_64)
    asset=pdfium-mac-x64.tgz
    sha256=6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b
    library=libpdfium.dylib
    ;;
  *)
    echo "fetch-pdfium: no pinned pdfium build for $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

directory="${XDG_CACHE_HOME:-$HOME/.cache}/qnn/pdfium/$version/${asset%.tgz}"
if [ ! -f "$directory/lib/$library" ]; then
  mkdir -p "$directory"
  archive="$directory/$asset.part"
  curl -fsSL "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$version/$asset" -o "$archive"
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$archive" | cut -d' ' -f1)
  else
    actual=$(shasum -a 256 "$archive" | cut -d' ' -f1)
  fi
  if [ "$actual" != "$sha256" ]; then
    rm -f "$archive"
    echo "fetch-pdfium: $asset has SHA-256 $actual, expected $sha256" >&2
    exit 1
  fi
  tar -xzf "$archive" -C "$directory"
  rm -f "$archive"
fi
echo "$directory/lib/$library"
