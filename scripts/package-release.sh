#!/bin/sh
# Builds release artifacts for all six targets:
#   dist/<tag>/spotify-<tag>-<triple>.tar.gz   (GitHub release; used by install.sh and the updater)
#   dist/<tag>/SHA256SUMS
#   dist/<tag>/spotify-<version>.tar.gz        (Honeycomb package: honeycomb.yaml + targets/)
#   dist/<tag>/install.sh
# Needs: rustup targets, cargo-zigbuild + zig (Linux musl), cargo-xwin (Windows msvc).
# Optional: SPOTIFY_CODESIGN_IDENTITY="Developer ID Application: …" to sign macOS binaries.
set -eu
# No AppleDouble (._*) or extended-attribute entries in any archive.
export COPYFILE_DISABLE=1
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
tag="v$version"
out="$root/dist/$tag"
rm -rf "$out" && mkdir -p "$out"
pkg="$out/honeycomb" && mkdir -p "$pkg/targets"
sed "s/\"version\": \"[^\"]*\"/\"version\": \"$version\"/" honeycomb.yaml >"$pkg/honeycomb.yaml"

build() { # triple tool
  case "$2" in
    cargo) cargo build --locked --release -p silicon-spotify-cli -p silicon-spotify-daemon --target "$1" ;;
    zig) cargo zigbuild --locked --release -p silicon-spotify-cli -p silicon-spotify-daemon --target "$1" ;;
    # ring forces plain clang on Windows arm64, so cargo-xwin must use its clang backend there.
    xwin) case "$1" in
            aarch64-*) XWIN_CROSS_COMPILER=clang cargo xwin build --locked --release -p silicon-spotify-cli -p silicon-spotify-daemon --target "$1" ;;
            *) cargo xwin build --locked --release -p silicon-spotify-cli -p silicon-spotify-daemon --target "$1" ;;
          esac ;;
  esac
}

check_arch() { # file expected-substring
  file "$1" | grep -q "$2" || { echo "error: $1 is not $2: $(file "$1")" >&2; exit 1; }
}

for spec in \
  "aarch64-apple-darwin cargo macos-aarch64 arm64" \
  "x86_64-apple-darwin cargo macos-x86_64 x86_64" \
  "aarch64-unknown-linux-musl zig linux-aarch64 aarch64" \
  "x86_64-unknown-linux-musl zig linux-x86_64 x86-64" \
  "aarch64-pc-windows-msvc xwin windows-aarch64 Aarch64" \
  "x86_64-pc-windows-msvc xwin windows-x86_64 x86-64"; do
  set -- $spec
  triple=$1 tool=$2 target=$3 arch=$4
  echo "==> $triple"
  build "$triple" "$tool"
  dir="target/$triple/release"
  ext=""; case "$triple" in *windows*) ext=".exe" ;; esac
  for bin in spotify spotify-daemon; do check_arch "$dir/$bin$ext" "$arch"; done
  case "$triple" in
    *apple-darwin)
      if [ -n "${SPOTIFY_CODESIGN_IDENTITY:-}" ]; then
        for bin in spotify spotify-daemon; do codesign --force --options runtime --timestamp --identifier "com.unlikefraction.$bin" -s "$SPOTIFY_CODESIGN_IDENTITY" "$dir/$bin"; done
      else
        for bin in spotify spotify-daemon; do codesign --force --identifier "com.unlikefraction.$bin" -s - "$dir/$bin"; done
      fi ;;
  esac
  mkdir -p "$pkg/targets/$target/bin"
  cp "$dir/spotify$ext" "$dir/spotify-daemon$ext" "$pkg/targets/$target/bin/"
  case "$target" in macos-*) cp packaging/honeycomb-install-macos.sh "$pkg/targets/$target/install.sh" ;; esac
  case "$triple" in
    *windows*) (cd "$dir" && zip -q "$out/spotify-$tag-$triple.zip" spotify.exe spotify-daemon.exe) ;;
    *) tar --no-mac-metadata -czf "$out/spotify-$tag-$triple.tar.gz" -C "$dir" spotify spotify-daemon 2>/dev/null || tar -czf "$out/spotify-$tag-$triple.tar.gz" -C "$dir" spotify spotify-daemon ;;
  esac
done

if command -v honeycomb >/dev/null 2>&1; then
  honeycomb pack "$pkg" --output "$out/spotify-$version.tar.gz" --json
else
  (cd "$pkg" && tar --no-mac-metadata -czf "$out/spotify-$version.tar.gz" honeycomb.yaml targets 2>/dev/null || tar -czf "$out/spotify-$version.tar.gz" honeycomb.yaml targets)
fi
cp scripts/install.sh "$out/install.sh"
(cd "$out" && shasum -a 256 spotify-* install.sh >SHA256SUMS)
if command -v honeycomb >/dev/null 2>&1; then honeycomb validate "$out/spotify-$version.tar.gz" --json || echo "warning: honeycomb validate failed" >&2; fi
echo "Artifacts in $out:"; ls -1 "$out"
