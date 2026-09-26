#!/bin/sh
# Honeycomb runs this once after `honeycomb install spotify` on macOS (never on updates).
# It runs non-interactively, possibly with no PATH and SILICON_HOME pointing at a packages home,
# so it uses absolute paths and never prompts. It installs spotify_player when Homebrew exists,
# starts Spotify.app hidden, and installs + starts the per-user daemon through launchd, pointing at
# Honeycomb's stable launcher (HONEYCOMB_BIN_DIR) so updates are picked up automatically.
set -u
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:${PATH:-}"
bin="${HONEYCOMB_BIN_DIR:-}"
[ -x "$bin/spotify" ] || bin="${HONEYCOMB_PACKAGE_DIR:-.}/bin"
echo "spotify ${HONEYCOMB_APP_VERSION:-?}: finishing macOS setup"
if ! command -v spotify_player >/dev/null 2>&1; then
  if command -v brew >/dev/null 2>&1; then
    brew install spotify_player || echo "warning: brew install spotify_player failed; run it yourself"
  else
    echo "warning: spotify_player is missing and Homebrew is not installed: brew install spotify_player"
  fi
fi
[ -d /Applications/Spotify.app ] || [ -d "$HOME/Applications/Spotify.app" ] || echo "warning: Spotify.app is not installed: https://www.spotify.com/download/mac/"
/usr/bin/open -g -j -a Spotify >/dev/null 2>&1 || true
mkdir -p "$HOME/.silicon-spotify" && chmod 700 "$HOME/.silicon-spotify"
printf '{"method":"honeycomb","dir":"%s","version":"%s"}\n' "$bin" "${HONEYCOMB_APP_VERSION:-}" >"$HOME/.silicon-spotify/install.json"
if env -u SILICON_HOME "$bin/spotify" daemon install --json >/dev/null 2>&1 || env -u SILICON_HOME "$bin/spotify" daemon start --json >/dev/null 2>&1; then
  echo "spotify-daemon is running. Next: spotify doctor · spotify auth login · spotify login '<SLT>'"
else
  echo "warning: could not start spotify-daemon now; it starts on the first spotify command (spotify daemon install for login start)"
fi
exit 0
