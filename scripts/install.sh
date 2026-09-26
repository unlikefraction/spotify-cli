#!/bin/sh
# spotify-cli installer.
#
#   curl -fsSL https://spotify.unlikefraction.com/install.sh | sh
#
# Installs `spotify` and `spotify-daemon`, makes sure the dependencies are installed and running
# (Spotify.app, spotify_player), starts the daemon now and at login. It never logs you in:
# afterwards run `spotify doctor`, `spotify auth login` (Spotify, once) and, for triggers,
# `spotify login '<SLT>'`.
#
# Environment:
#   SPOTIFY_VERSION=v0.1.0        install this release (default: latest)
#   SPOTIFY_INSTALL_DIR=/path     where the binaries go (default: /usr/local/bin if writable, else ~/.local/bin)
#   SPOTIFY_ARCHIVE=/path.tar.gz  install from a local release archive (skips download and checksum)
#   SPOTIFY_INSTALL_FROM_SOURCE=1 build with cargo from the repository instead
#   SPOTIFY_SKIP_DEPS=1           do not install Spotify.app / spotify_player
#   SPOTIFY_NO_START=1            install only; do not register or start the daemon
#
# The whole script is one function so a truncated download can never run half an install.

set -eu

install_silicon_spotify() {
  repository="https://github.com/unlikefraction/spotify-cli"
  say() { printf '%s\n' "$*"; }
  step() { printf '\033[1m[%s]\033[0m %s\n' "$1" "$2"; }
  fail() { printf 'error: %s\n' "$1" >&2; [ -n "${2:-}" ] && printf 'hint: %s\n' "$2" >&2; exit 1; }
  have() { command -v "$1" >/dev/null 2>&1; }

  for tool in curl tar uname mktemp; do
    have "$tool" || fail "$tool is required" "Install it and rerun."
  done

  os=$(uname -s)
  case "$(uname -m)" in
    arm64 | aarch64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) fail "unsupported CPU architecture $(uname -m)" ;;
  esac
  case "$os" in
    Darwin) triple="$arch-apple-darwin" ;;
    Linux) triple="$arch-unknown-linux-musl" ;;
    *) fail "unsupported OS $os" "On Windows install with Honeycomb: honeycomb install spotify" ;;
  esac

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT INT TERM

  # 1. Binaries --------------------------------------------------------------------------------
  step 1/5 "Getting spotify-cli"
  if [ "${SPOTIFY_INSTALL_FROM_SOURCE:-}" = 1 ]; then
    have cargo || fail "cargo is required to build from source" "Install Rust from https://rustup.rs"
    cargo install --locked --root "$tmp/root" --git "$repository" silicon-spotify-cli silicon-spotify-daemon >&2
    cp "$tmp/root/bin/spotify" "$tmp/root/bin/spotify-daemon" "$tmp/"
    version=source
  else
    if [ -n "${SPOTIFY_ARCHIVE:-}" ]; then
      [ -f "$SPOTIFY_ARCHIVE" ] || fail "SPOTIFY_ARCHIVE=$SPOTIFY_ARCHIVE does not exist"
      archive="$SPOTIFY_ARCHIVE"
      version=local
    else
      version=${SPOTIFY_VERSION:-}
      if [ -z "$version" ]; then
        latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$repository/releases/latest") || fail "cannot reach GitHub" "Check the network, or set SPOTIFY_VERSION."
        version=${latest##*/}
      fi
      case "$version" in '' | . | .. | *[!a-zA-Z0-9._-]*) fail "unexpected release tag '$version'" ;; esac
      name="spotify-$version-$triple.tar.gz"
      base="$repository/releases/download/$version"
      curl -fsSL --proto '=https' "$base/$name" -o "$tmp/$name" || fail "download failed: $base/$name" "Check that release $version has a $triple build."
      curl -fsSL --proto '=https' "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "SHA256SUMS missing for $version"
      expected=$(awk -v f="$name" '$2 == f || $2 == "*"f { print $1 }' "$tmp/SHA256SUMS")
      [ ${#expected} -eq 64 ] || fail "no checksum for $name in SHA256SUMS"
      if have shasum; then actual=$(shasum -a 256 "$tmp/$name" | awk '{print $1}'); else actual=$(sha256sum "$tmp/$name" | awk '{print $1}'); fi
      [ "$expected" = "$actual" ] || fail "checksum mismatch for $name (expected $expected, got $actual)" "Do not use this download; retry later or report it."
      archive="$tmp/$name"
    fi
    tar -xzf "$archive" -C "$tmp" spotify spotify-daemon || fail "the archive does not contain spotify and spotify-daemon"
  fi

  # 2. Install -----------------------------------------------------------------------------------
  step 2/5 "Installing spotify and spotify-daemon"
  dir=${SPOTIFY_INSTALL_DIR:-}
  if [ -z "$dir" ]; then
    if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then dir=/usr/local/bin; else dir="$HOME/.local/bin"; fi
  fi
  mkdir -p "$dir"
  for bin in spotify spotify-daemon; do
    cp "$tmp/$bin" "$dir/.$bin.new"
    chmod 0755 "$dir/.$bin.new"
    mv -f "$dir/.$bin.new" "$dir/$bin"
  done
  case ":$PATH:" in
    *":$dir:"*) ;;
    *)
      # The user's login shell, not the shell running this script (curl | sh runs bash on macOS).
      case "${SHELL:-}" in
        */zsh) rc="$HOME/.zshrc"; line="export PATH=\"$dir:\$PATH\"" ;;
        */bash) rc="$HOME/.bash_profile"; [ "$os" = Linux ] && rc="$HOME/.bashrc"; line="export PATH=\"$dir:\$PATH\"" ;;
        */fish) rc="$HOME/.config/fish/config.fish"; line="fish_add_path $dir"; mkdir -p "$HOME/.config/fish" ;;
        *) rc="$HOME/.profile"; line="export PATH=\"$dir:\$PATH\"" ;;
      esac
      if ! grep -q 'silicon-spotify' "$rc" 2>/dev/null; then
        printf '\n# silicon-spotify\n%s\n' "$line" >>"$rc"
        say "  added $dir to PATH in $rc (open a new shell, or: export PATH=\"$dir:\$PATH\")"
      fi
      ;;
  esac
  mkdir -p "$HOME/.silicon-spotify" && chmod 700 "$HOME/.silicon-spotify"
  printf '{"method":"script","dir":"%s","version":"%s","installed_at":"%s"}\n' "$dir" "$version" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$HOME/.silicon-spotify/install.json"
  say "  $("$dir/spotify" --version) → $dir"

  if [ "$os" != Darwin ]; then
    say "Installed the CLI. Spotify control needs macOS; login, config, docs and report work here."
    return 0
  fi

  # 3. Dependencies ------------------------------------------------------------------------------
  step 3/5 "Checking dependencies (Spotify.app, spotify_player)"
  brew=""
  for candidate in brew /opt/homebrew/bin/brew /usr/local/bin/brew; do
    if have "$candidate" || [ -x "$candidate" ]; then brew=$(command -v "$candidate" 2>/dev/null || echo "$candidate"); break; fi
  done
  if [ -d /Applications/Spotify.app ] || [ -d "$HOME/Applications/Spotify.app" ]; then
    say "  Spotify.app: installed"
  elif [ "${SPOTIFY_SKIP_DEPS:-}" = 1 ]; then
    say "  Spotify.app: missing (skipped; install from https://www.spotify.com/download/mac/)"
  elif [ -n "$brew" ]; then
    say "  Spotify.app: installing with Homebrew…"
    "$brew" install --cask spotify >&2 || say "  could not install Spotify.app; install it from https://www.spotify.com/download/mac/"
  else
    say "  Spotify.app: missing. Install it from https://www.spotify.com/download/mac/ (or install Homebrew and rerun)."
  fi
  if have spotify_player || [ -x /opt/homebrew/bin/spotify_player ] || [ -x /usr/local/bin/spotify_player ] || [ -x "$HOME/.cargo/bin/spotify_player" ]; then
    say "  spotify_player: installed"
  elif [ "${SPOTIFY_SKIP_DEPS:-}" = 1 ]; then
    say "  spotify_player: missing (skipped; brew install spotify_player)"
  elif [ -n "$brew" ]; then
    say "  spotify_player: installing with Homebrew…"
    "$brew" install spotify_player >&2 || fail "brew install spotify_player failed" "Run it yourself to see why."
  elif have cargo; then
    say "  spotify_player: building with cargo (a few minutes)…"
    cargo install --locked spotify_player >&2 || fail "cargo install spotify_player failed"
  else
    say "  spotify_player: missing. Install Homebrew (https://brew.sh) then: brew install spotify_player"
  fi

  if [ "${SPOTIFY_NO_START:-}" = 1 ]; then
    say "Installed. Start the daemon with: spotify daemon install"
    return 0
  fi

  # 4. Running -----------------------------------------------------------------------------------
  step 4/5 "Starting Spotify and spotify-daemon (and at every login)"
  /usr/bin/open -g -j -a Spotify 2>/dev/null || true
  if "$dir/spotify" daemon install >/dev/null 2>"$tmp/daemon.err"; then
    say "  spotify-daemon: running, starts at login (launchd agent com.unlikefraction.spotify.daemon)"
  elif "$dir/spotify" daemon start >/dev/null 2>>"$tmp/daemon.err"; then
    say "  spotify-daemon: running (no launchd GUI session; run \`spotify daemon install\` after you log in)"
  else
    say "  spotify-daemon: could not start:"; sed 's/^/    /' "$tmp/daemon.err"
  fi

  # 5. Next --------------------------------------------------------------------------------------
  step 5/5 "Done"
  say ""
  say "Next:"
  say "  spotify doctor              check everything (and see what macOS may ask you to allow)"
  say "  spotify auth login          sign spotify_player in to Spotify (browser; a Carbon clicks Agree once)"
  say "  spotify status              what is playing"
  say "  spotify login '<SLT>'       Silicons: identity for Ting triggers (spotify login --help)"
  say ""
  say "macOS will ask once whether spotify-daemon may control Spotify: click OK."
  say "Docs: https://spotify.unlikefraction.com/docs · spotify docs"
}

install_silicon_spotify "$@"
