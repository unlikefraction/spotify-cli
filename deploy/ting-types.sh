#!/bin/sh
# Registers spotify-cli's Ting types in one Ting context (production by default; set
# IAM_TEST_APP_SECRET + IAM_TEST_KEY for a testing environment). Needs a Ting session of an
# owner/admin of org `unlikefraction` (`ting login --token-stdin`), and the app `spotify` in Honeycomb.
# Idempotent: re-registering the same description returns 200. Types cannot be deleted.
set -eu
command -v ting >/dev/null 2>&1 || { echo "error: the ting CLI is required (honeycomb install ting)" >&2; exit 1; }
register() {
  ting --org unlikefraction types register --type "$1" --description "$2" --json
}
register spotify.trigger.fired "A spotify-cli playback trigger reached its checkpoint (time remaining, time elapsed, track end or track change)."
register spotify.trigger.expired "A one-shot spotify-cli trigger can no longer fire because its track stopped or changed before the checkpoint."
ting --org unlikefraction types list --app spotify --json
