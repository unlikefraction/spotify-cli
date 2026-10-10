# Deploying Spotify

Spotify is authored and published by `si:tos` in [Silicon Apps](https://apps.teamofsilicons.com). The [developer portal](https://developers.teamofsilicons.com) and `silicon-apps` CLI manage app details, account configuration, packages, and releases. Every user is a Carbon or Silicon account.

## Sign in as the publisher

```sh
silicon-apps login status --json
token=$(silicon-accounts login --app silicon-apps --json | jq -r .slt)
silicon-apps login --slt "$token"
unset token
```

Check that the reported account is `si:tos` before publishing. Keep the app secret from initial app creation on the backend, in AWS Secrets Manager `unlikefraction-spotify/production/runtime`. Never put it in a package, frontend, or command-line argument. Use `silicon-accounts app config` to configure sign-in through the same account or through app credentials read from stdin.

Enable `remember_browser`. Register `https://spotify.unlikefraction.com/` as the browser redirect URI and `https://spotify.unlikefraction.com` as an allowed origin. Browser state and PKCE bind authorization codes to the browser that started sign-in. Backend token exchanges and refreshes keep encrypted replay receipts so a lost response does not reuse a one-use credential with Accounts.

## Backend

The existing CloudFormation stack is `unlikefraction-spotify-production` in `us-east-1`. The service runs on a Linux ARM64 EC2 instance with Caddy and systemd. Deployments use S3 and SSM and retain the previous release for rollback.

```sh
cargo zigbuild --release --locked --target aarch64-unknown-linux-musl -p silicon-spotify
python3 deploy/deploy.py --caddy /path/to/caddy-linux-arm64
curl -fsS https://backend.spotify.unlikefraction.com/healthz
```

Use `.env.example` for current keys. Retain the encryption key and SQLite database together across releases and restores. Secrets use the new Silicon Accounts URL and newly created app secret. Old session credentials cannot be reused across identity systems; users sign in once through Silicon Accounts.

## Build native packages

```sh
cargo fmt --all -- --check
cargo check --locked --workspace
scripts/package-release.sh
```

`apps.yaml` uses the Silicon Apps schema: `schema_version`, `app_id`, `version`, `command`, and a `binary` plus optional `install_script` for each target. The CLI must answer `--help`, `accounts --json`, and `login status --json` without a saved login. Builds validate native formats and package safety. macOS releases are signed with Developer ID and notarized before publication.

## Publish through Silicon Apps

```sh
silicon-apps capabilities --json
silicon-apps validate PACKAGE_DIRECTORY
silicon-apps pack PACKAGE_DIRECTORY --output spotify.tar.gz
silicon-apps upload spotify --target linux-aarch64 spotify.tar.gz
silicon-apps packages spotify --json
silicon-apps release spotify --version VERSION --package PACKAGE_ID
silicon-apps promote spotify DEVELOPMENT_RELEASE_ID --version VERSION
silicon-apps readiness spotify
silicon-apps publish spotify
```

Repeat `--package` for each accepted target. Versions are immutable. Use an idempotency key when retrying a mutation. The store currently has Linux validation workers; publish macOS and Windows downloads on the matching GitHub release until their workers are available. `silicon-apps capabilities` is the source of truth for target readiness.

Silicon Apps owns updates for store installations. Native download users rerun the installer. There is no second network updater in Spotify.

## Frontend

The site uses source from [Silicon UI](https://ui.teamofsilicons.com). Run `docs-site/deploy.sh --prod` to build and publish the Spotify website.

## Live verification

Check the public health route, the site, and the three package discovery commands. With a real Silicon Accounts session, verify login, a subsequent invocation, access-token refresh, and explicit logout. Check browser restoration after a reload. Keep credentials out of logs and remove any temporary verification session when finished.

Ting must support account-scoped App verification and User verification before remote notifications can succeed. An older Ting deployment that requires an `org_id` is incompatible; do not invent an organization to bypass it. Delivery errors retain queued work for retry after the provider is migrated.
