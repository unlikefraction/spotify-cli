#!/bin/sh
# Builds the site and deploys the static output to Vercel (project spotify-cli, domain
# spotify.unlikefraction.com). Usage: docs-site/deploy.sh [--prod]
set -eu
cd "$(dirname "$0")"
node build.mjs
node check.mjs
cp vercel.json dist/vercel.json
VERCEL=${VERCEL:-$(command -v vercel || ls "$HOME"/.npm/_npx/*/node_modules/.bin/vercel 2>/dev/null | head -1)}
SCOPE=${VERCEL_SCOPE:-shubham-guptas-projects-7ecb7811}
cd dist
"$VERCEL" link --yes --project spotify-cli --scope "$SCOPE" >/dev/null
# `vercel link` writes a VERCEL_OIDC_TOKEN into .env.local; never keep it next to the site.
rm -f .env.local
"$VERCEL" deploy --yes --scope "$SCOPE" "$@"
