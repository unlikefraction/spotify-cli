#!/bin/sh
# Creates the four Space Station tables (org unlikefraction). Each key is printed ONCE on stdout: put them
# straight into the backend secret (unlikefraction-spotify/production/runtime) and nowhere else.
# Needs `spacestation login --org unlikefraction` as an org member.
set -eu
for table in spotifybackend spotifyclidaemon spotifyfrontendanalytics spotifyfrontendevents; do
  printf '%s: ' "$table" >&2
  spacestation --org unlikefraction tables create "$table"
done
