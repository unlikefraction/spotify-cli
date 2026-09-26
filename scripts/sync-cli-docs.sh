#!/bin/sh
# Copies the canonical guides in docs/ into crates/cli/docs/ so the CLI binary bundles them
# (include_str!). Run after editing docs/; CI fails if they differ.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$root/crates/cli/docs"
for topic in usage triggers playback queue auth config daemon errors ting development api telemetry versioning; do
  cp "$root/docs/$topic.md" "$root/crates/cli/docs/$topic.md"
done
if [ "${1:-}" = "--check" ]; then
  git -C "$root" diff --quiet -- crates/cli/docs || { echo "crates/cli/docs is out of date; run scripts/sync-cli-docs.sh" >&2; exit 1; }
fi
echo "synced $(ls "$root/crates/cli/docs" | wc -l | tr -d ' ') guides"
