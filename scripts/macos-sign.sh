#!/bin/sh
# Signs one macOS architecture's two binaries (spotify, spotify-daemon), checks the signatures and,
# when asked, has Apple notarize them. scripts/package-release.sh runs it for each macOS target
# before it packs anything, so the release archives, the Honeycomb package and SHA256SUMS all
# carry the signed binaries. It never changes anything but the two files it signs.
#
#   scripts/macos-sign.sh --preflight        check the settings below (run before a long build)
#   scripts/macos-sign.sh <dir> [<label>]    sign, verify and maybe notarize <dir>/spotify{,-daemon}
#
# Environment:
#   SPOTIFY_CODESIGN_IDENTITY  unset: ad-hoc signature (CI, contributors; macOS may ask for the
#                              Automation permission again after each new build).
#                              Set ("Developer ID Application: <name> (<team>)" or its SHA-1):
#                              Developer ID signature with the hardened runtime and a secure
#                              timestamp; spotify-daemon gets packaging/spotify-daemon.entitlements
#                              (Apple Events to Spotify). macOS then keeps the Allow across updates.
#   SPOTIFY_NOTARY_PROFILE     a keychain profile made with `xcrun notarytool store-credentials`:
#                              submit a zip of both binaries, wait, and fail unless Apple answers
#                              Accepted with a ticket for exactly these two files (their cdhashes).
#                              Needs the identity.
# Bare executables cannot be stapled (stapler takes apps, disk images and installer packages only):
# Gatekeeper looks the ticket up online the first time a downloaded binary runs.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
identity=${SPOTIFY_CODESIGN_IDENTITY:-}
[ "$identity" != - ] || identity= # "-" is codesign's name for ad-hoc
profile=${SPOTIFY_NOTARY_PROFILE:-}
entitlements="$root/packaging/spotify-daemon.entitlements"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
bundle_version=${version%%[-+]*} # what crates/daemon/build.rs embeds: MAJOR.MINOR.PATCH

fail() {
  echo "error: $*" >&2
  exit 1
}

preflight() {
  [ "$(uname -s)" = Darwin ] || fail "signing macOS binaries needs a Mac"
  if [ -n "$profile" ] && [ -z "$identity" ]; then
    fail "SPOTIFY_NOTARY_PROFILE is set but SPOTIFY_CODESIGN_IDENTITY is not: Apple notarizes only Developer ID signed code"
  fi
  if [ -n "$identity" ]; then
    # codesign picks the identity by substring, like this grep; it refuses a name that matches two.
    matches=$(security find-identity -v -p codesigning | grep -iF -- "$identity" || true)
    [ -n "$matches" ] ||
      fail "no valid code-signing identity matches SPOTIFY_CODESIGN_IDENTITY (see: security find-identity -v -p codesigning)"
    [ "$(printf '%s\n' "$matches" | awk '{print $2}' | sort -u | wc -l | tr -d ' ')" = 1 ] ||
      fail "SPOTIFY_CODESIGN_IDENTITY matches more than one code-signing identity; use the full name or its SHA-1 (see: security find-identity -v -p codesigning)"
    if [ -n "$profile" ]; then
      printf '%s\n' "$matches" | grep -q '"Developer ID Application: ' ||
        fail "SPOTIFY_CODESIGN_IDENTITY is not a Developer ID Application identity, and Apple notarizes only those"
    fi
    plutil -lint -s "$entitlements" || fail "$entitlements is not a valid plist"
  fi
  if [ -n "$profile" ]; then
    xcrun notarytool history --keychain-profile "$profile" >/dev/null ||
      fail "notary keychain profile '$profile' is not usable; its owner creates it with: xcrun notarytool store-credentials $profile"
  fi
  return 0
}

sign() { # file identifier
  if [ -z "$identity" ]; then
    codesign --force --identifier "$2" -s - "$1"
  elif [ "$2" = com.unlikefraction.spotify-daemon ]; then
    codesign --force --timestamp --options runtime --identifier "$2" --entitlements "$entitlements" -s "$identity" "$1"
  else
    codesign --force --timestamp --options runtime --identifier "$2" -s "$identity" "$1"
  fi
}

verify() { # file identifier
  codesign --verify --strict --verbose=2 "$1"
  details=$(codesign -dvvv "$1" 2>&1)
  printf '%s\n' "$details" | grep -qxF "Identifier=$2" || fail "$1: identifier is not $2"
  if [ -n "$identity" ]; then
    printf '%s\n' "$details" | grep -q '^CodeDirectory .*flags=0x[0-9a-f]*([^)]*runtime' || fail "$1: no hardened runtime"
    printf '%s\n' "$details" | grep -q '^Timestamp=' || fail "$1: no secure timestamp"
    printf '%s\n' "$details" | grep -q '^TeamIdentifier=[A-Z0-9]\{10\}$' || fail "$1: no team identifier"
    if ! printf '%s\n' "$details" | grep -q '^Authority=Developer ID Application: '; then
      [ -z "$profile" ] || fail "$1: Apple notarizes only Developer ID Application signatures"
      echo "warning: $1 is not signed with a Developer ID Application certificate; Gatekeeper will refuse it" >&2
    fi
  fi
  [ "$2" = com.unlikefraction.spotify-daemon ] || return 0
  # The daemon's embedded Info.plist, bound into the signature.
  printf '%s\n' "$details" | grep -q '^Info.plist entries=' || fail "$1: no Info.plist bound to the signature"
  plist="$work/$2.Info.plist"
  rm -f "$plist"
  segedit "$1" -extract __TEXT __info_plist "$plist" || fail "$1: no __TEXT,__info_plist section"
  [ "$(plutil -extract CFBundleIdentifier raw -o - "$plist")" = "$2" ] || fail "$1: Info.plist CFBundleIdentifier is not $2"
  [ "$(plutil -extract CFBundleShortVersionString raw -o - "$plist")" = "$bundle_version" ] ||
    fail "$1: Info.plist CFBundleShortVersionString is not $bundle_version"
  plutil -extract NSAppleEventsUsageDescription raw -o - "$plist" >/dev/null || fail "$1: Info.plist has no NSAppleEventsUsageDescription"
  if [ -n "$identity" ]; then
    # Exactly the declared entitlements, nothing more.
    actual=$(codesign -d --entitlements - --xml "$1" 2>/dev/null | plutil -convert json -o - - 2>/dev/null || true)
    expected=$(plutil -convert json -o - "$entitlements")
    [ "$actual" = "$expected" ] || fail "$1: entitlements are $actual, expected $expected"
  fi
}

notarize() { # dir label
  zip="$work/spotify-$2.zip"
  rm -f "$zip"
  (cd "$1" && zip -q "$zip" spotify spotify-daemon)
  echo "==> notarizing $2 (xcrun notarytool submit --wait: usually a few minutes, at most an hour)"
  xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait --timeout 60m --output-format plist >"$work/notary.plist" || true
  status=$(plutil -extract status raw -o - "$work/notary.plist" 2>/dev/null || echo "no answer")
  id=$(plutil -extract id raw -o - "$work/notary.plist" 2>/dev/null || true)
  if [ "$status" != Accepted ]; then
    echo "error: notarization of $2 ended with status '$status'${id:+ (submission $id)}" >&2
    if [ -n "$id" ]; then xcrun notarytool log "$id" --keychain-profile "$profile" >&2 || true; fi
    exit 1
  fi
  echo "    accepted (submission $id)"
  # The ticket must cover exactly the files that ship: nothing touches them after this point
  # (bare executables are not stapled), so their cdhashes must be in the ticket.
  xcrun notarytool log "$id" --keychain-profile "$profile" "$work/notary-log.json" >/dev/null ||
    fail "cannot read the notary log of submission $id"
  for bin in spotify spotify-daemon; do
    cdhash=$(codesign -dvvv "$1/$bin" 2>&1 | sed -n 's/^CDHash=//p')
    [ -n "$cdhash" ] && grep -q "\"$cdhash\"" "$work/notary-log.json" ||
      fail "$1/$bin (cdhash ${cdhash:-unknown}) is not in the ticket of submission $id"
    echo "    ticket covers $bin (cdhash $cdhash)"
  done
}

if [ "${1:-}" = --preflight ]; then
  preflight
  exit 0
fi
[ $# -ge 1 ] && [ -d "$1" ] || fail "usage: $0 --preflight | <dir with spotify and spotify-daemon> [<label>]"
dir=$1
label=${2:-$(basename "$dir")}
preflight
work=$(mktemp -d "${TMPDIR:-/tmp}/spotify-sign.XXXXXX")
trap 'rm -rf "$work"' EXIT
for bin in spotify spotify-daemon; do
  [ -f "$dir/$bin" ] || fail "$dir/$bin is missing"
  sign "$dir/$bin" "com.unlikefraction.$bin"
  verify "$dir/$bin" "com.unlikefraction.$bin"
done
if [ -z "$identity" ]; then
  echo "    $label: ad-hoc signed (SPOTIFY_CODESIGN_IDENTITY unset), not notarized"
elif [ -n "$profile" ]; then
  notarize "$dir" "$label"
  echo "    $label: Developer ID signed and notarized"
else
  echo "warning: $label is signed with SPOTIFY_CODESIGN_IDENTITY but NOT notarized (SPOTIFY_NOTARY_PROFILE unset): Gatekeeper refuses it when downloaded with a browser" >&2
fi
