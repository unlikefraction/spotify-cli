# Going live: runbook

Everything below is **outward-facing and mutating**. Run the steps in order; each names who can
run it. Nothing here is run by the build.

| # | Step | Needs |
| --- | --- | --- |
| 1 | Create the GitHub repo and push | `gh` as `unlikefraction` |
| 2 | Backend infrastructure (CloudFormation) | AWS credentials for the production account, region `us-east-1` |
| 3 | DNS `backend.spotify` → instance IP; `spotify` → Vercel | DNS provider for unlikefraction.com |
| 4 | Register the IAM app through Honeycomb; save the secret | `unlikefraction` owner/admin (`honeycomb login`) |
| 5 | Runtime secret → Secrets Manager; deploy the backend | AWS |
| 6 | Approve the IAM webhook (step-up) | direct Carbon owner/admin of `unlikefraction` |
| 7 | Register Ting types | Ting session of an `unlikefraction` owner/admin |
| 8 | Space Station tables → keys into the secret; redeploy | `unlikefraction` member |
| 9 | Release binaries (GitHub release + Honeycomb package) | macOS build host with zig and cargo-xwin; Developer ID certificate and notarytool profile |
| 10 | Website + docs on Vercel | Vercel CLI |
| 11 | End-to-end check with a real Silicon | a Silicon with `iam` 4.x |

## 1. Repository

```sh
gh repo create unlikefraction/spotify-cli --public --source . --push
```

## 2. Infrastructure

```sh
aws --region us-east-1 cloudformation deploy --stack-name unlikefraction-spotify-production \
  --template-file deploy/stack.yaml --capabilities CAPABILITY_NAMED_IAM \
  --parameter-overrides Vpc=<vpc-id> Subnet=<public-subnet-id> AllocateElasticIp=false \
  --tags Service=silicon-spotify Environment=production
aws --region us-east-1 cloudformation describe-stacks --stack-name unlikefraction-spotify-production --query 'Stacks[0].Outputs'
```

Set `AllocateElasticIp=true` only if the EIP quota allows; without it the IP changes on
stop/start and the A record must follow.

## 3. DNS (Namecheap, change only these records)

- `A backend.spotify` → `PublicIp` output, TTL 300
- `A spotify` → `76.76.21.21` (or what Vercel prints in step 10)

Port 80 must stay open for Caddy's ACME challenge.

## 4. IAM application (Honeycomb)

```sh
secret=$(openssl rand -hex 32)  # SPOTIFY_IAM_WEBHOOK_SECRET in step 5
jq --arg s "$secret" '.webhook_secret = $s' deploy/honeycomb-application.json \
  > deploy/honeycomb-application.local.json   # gitignored; never commit the real secret
honeycomb --json --idempotency-key spotify-create-0001 apps create deploy/honeycomb-application.local.json
# → save "app_secret" (ask_…, 47 chars) immediately; it is shown once.
#   Lost response: rerun the same command with the same key within 10 minutes.
honeycomb --json apps get spotify
```

The app starts `private` (only `unlikefraction` members can log in); private apps are exempt from Ting's
critical-scope review. Going public later: `honeycomb publication request spotify …` (Ting
provider approval + a Honeycomb validator).

## 5. Runtime secret and backend deploy

Write a private JSON file (never pass values on the command line):

```json
{"SPOTIFY_IAM_APP_SECRET":"ask_…","SPOTIFY_IAM_WEBHOOK_SECRET":"…","SPOTIFY_BACKUP_BUCKET":"<ArtifactBucket>",
 "SPOTIFY_PUBLIC_ORIGIN":"https://backend.spotify.unlikefraction.com","RUST_LOG":"info"}
```

```sh
aws --region us-east-1 secretsmanager put-secret-value --secret-id unlikefraction-spotify/production/runtime --secret-string file:///secure/path/runtime.json
cargo zigbuild --release -p silicon-spotify --bin spotify-api --target aarch64-unknown-linux-musl
python3 deploy/deploy.py --caddy /path/to/caddy_linux_arm64
curl -fsS https://backend.spotify.unlikefraction.com/healthz
curl -fsS https://backend.spotify.unlikefraction.com/api/v1/iam
```

## 6. Webhook approval

```sh
honeycomb apps webhook status spotify
iam -o json step-up application.webhook.approve <APP_UUID> | jq -r .step_up_token > /secure/stepup
honeycomb apps webhook approve spotify --endpoint <PENDING_ENDPOINT_UUID> --revision <N> --step-up-file /secure/stepup
```

## 7. Ting types (per Ting context)

```sh
ting login --token-stdin   # Ting-bound SLT of an unlikefraction owner/admin
deploy/ting-types.sh
```

Repeat inside each testing environment (with `IAM_TEST_APP_SECRET`/`IAM_TEST_KEY` for Ting).

## 8. Telemetry tables

```sh
spacestation login --org unlikefraction
deploy/spacestation-tables.sh     # add the four keys to the runtime secret, then redeploy (step 5)
```

## 9. Release

Once per Mac (the owner, interactively; never paste the credentials anywhere else):

```sh
security find-identity -v -p codesigning     # lists "Developer ID Application: Shubham Gupta (LTBSK59BJ2)"
xcrun notarytool store-credentials spotify-cli --apple-id <apple-id> --team-id LTBSK59BJ2   # asks for an app-specific password
```

Each release:

```sh
export SPOTIFY_CODESIGN_IDENTITY="Developer ID Application: Shubham Gupta (LTBSK59BJ2)"
export SPOTIFY_NOTARY_PROFILE=spotify-cli
scripts/package-release.sh                                    # dist/v0.1.0/*
gh release create v0.1.0 dist/v0.1.0/spotify-v0.1.0-* dist/v0.1.0/SHA256SUMS dist/v0.1.0/install.sh --title v0.1.0 --notes-file CHANGELOG.md
honeycomb validate dist/v0.1.0/spotify-0.1.0.tar.gz
honeycomb releases upload spotify dist/v0.1.0/spotify-0.1.0.tar.gz --channel prod \
  --revision "$(honeycomb --json apps get spotify | jq -r .revision)"
```

For each macOS target, `scripts/package-release.sh` runs `scripts/macos-sign.sh` before it packs
anything, so the archives, the Honeycomb package and `SHA256SUMS` all carry the signed binaries:

- `codesign --force --timestamp --options runtime` with identifiers `com.unlikefraction.spotify`
  and `com.unlikefraction.spotify-daemon`; the daemon also gets
  `packaging/spotify-daemon.entitlements` (`com.apple.security.automation.apple-events`, which
  the hardened runtime needs for Apple Events) and carries its embedded Info.plist
  (`crates/daemon/build.rs`).
- Checks each signature (`codesign --verify --strict`, identifier, runtime flag, secure
  timestamp, team, the daemon's Info.plist and exact entitlements) and stops on any mismatch.
- With `SPOTIFY_NOTARY_PROFILE`: zips both binaries, runs
  `xcrun notarytool submit --keychain-profile … --wait`, and stops unless the status is
  `Accepted` (printing `notarytool log` otherwise) and the ticket lists both binaries' cdhashes.
  The profile is checked before the build starts, and `SPOTIFY_NOTARY_PROFILE` without
  `SPOTIFY_CODESIGN_IDENTITY` is an error.
- Also before the build: `SPOTIFY_CODESIGN_IDENTITY` must match exactly one identity in
  `security find-identity -v -p codesigning`, and a Developer ID Application one when notarizing.

Bare executables cannot be stapled, so Gatekeeper looks the ticket up online the first time a
browser-downloaded binary runs (install.sh and the daemon's updater download with curl or plain
HTTP, which set no quarantine flag, so Gatekeeper does not assess those). Check a binary with
`spctl --assess --type open --context context:primary-signature -vv <file>`
(`source=Notarized Developer ID`); `spctl --type execute` rejects every bare executable, notarized
or not. On the build Mac, Gatekeeper can keep answering `Unnotarized Developer ID` for a binary
it assessed before notarization (a cached answer that cleared within about ten minutes in
testing); that does not mean the ticket is missing.

Without `SPOTIFY_CODESIGN_IDENTITY` (CI, contributors) the binaries are ad-hoc signed and not
notarized: macOS then asks for the Automation permission again after each update. The script's
last lines say which kind it built.

## 10. Website

```sh
docs-site/deploy.sh --prod     # builds locally (the build reads ../docs, ../Cargo.toml), checks, uploads dist/
vercel domains add spotify.unlikefraction.com   # once
```

## 11. End to end

On a Mac with Spotify, as a Silicon:

```sh
curl -fsSL https://spotify.unlikefraction.com/install.sh | sh
spotify doctor
iam -o json silicon-login --app-id spotify --grant-org unlikefraction --approve-scopes | jq -r .slt | spotify login --token-file -
spotify trigger test          # a spotify.trigger.fired Ting reaches the Silicon's flow
```
