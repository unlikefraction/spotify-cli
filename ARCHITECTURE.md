# spotify-cli architecture

How it is built and why, measured against two inputs: the product spec (a Spotify CLI with
playback triggers delivered over Ting) and the ecosystem rules every IAM app follows.

## Shape

```text
            ┌──────────── this Mac (one OS user) ─────────────────────────────┐
 Carbon /   │ spotify (CLI) ──unix socket──▶ spotify-daemon ──NSAppleScript──▶ Spotify.app
 Silicon ──▶│   │  per-home store           │  distributed notifications ◀──┘
            │   │  $SILICON_HOME/.spotify   ├──▶ spotify_player (warm, pty) ──▶ Spotify Web API
            │   │                           │  triggers → outbox (SQLite)
            └───┼───────────────────────────┼──────────────────────────────────┘
                │ SLT, refresh, report      │ Silicon's own oat_ token
                ▼                           ▼
             backend (spotify-api, holds the IAM app secret)
                │ IAM: SLT exchange, introspection, OBO proofs      │ Ting: subscriptions, tings
```

- **Library (stateless)** — `silicon-spotify-client`: models, parsers, AppleScript builders and
  parser, spotify_player wrapper with failure classification, the verified controller, the pure
  trigger engine, the backend client. Feature `runtime` adds the per-home store and IPC client.
- **Daemon (always on)** — one per OS user because there is one Spotify.app per user; it serves
  every Silicon home. The CLI starts it on demand and replaces it when older.
- **CLI** — a client of the library and the daemon; also does login/config/report/docs directly.
- **Backend** — exists to keep the IAM app secret server-side, as IAM requires.

## Decisions

| Decision | Why |
| --- | --- |
| spotify_player first, **verify** against Spotify.app, AppleScript fallback, report `via`/`fallback` | The spec says try spotify_player then AppleScript. spotify_player hands commands to its instance asynchronously and exits 0 before (or without) any effect, and starting a single track by id leaves the desktop app with no track, so "fails" must mean "did not take effect". Verification makes the rule precise and observable. |
| In-process `NSAppleScript` on the main thread (objc2) | `osascript` costs ~0.1 s CPU per call; the daemon polls, so it compiles each script once and runs it in-process (~50 ms, mostly Spotify's own reply time). |
| Spotify's distributed notification + adaptive polling | Notifications arrive instantly on play/pause/track change but not on seeks; adaptive readings catch seeks and give precise checkpoint timing without constant polling. |
| Warm spotify_player on a daemon-owned pty that answers terminal queries | spotify_player CLI calls take ~1.5 s without a running instance and ~20 ms with one. The Homebrew build has no daemon mode, and the TUI refuses to start without a terminal answering its cursor/attribute queries. Streaming/media keys/notifications are disabled so it never becomes a playback device. |
| Managed queue in the daemon | Neither the Web API nor AppleScript can remove or reorder Spotify's queue, and spotify_player's CLI cannot even append. "CRUD queue" is only possible with our own queue; Spotify's upcoming list is shown read-only. The interrupted context is resumed when the queue drains. |
| Trigger engine as pure code | Every rule (plays, completion, expiry, no retroactive fires, restart-safe play ids) is unit-tested without Spotify. |
| Current-scope triggers expire loudly (`spotify.trigger.expired`) | A Silicon waiting for "30 s left" on a song that got skipped must not wait forever. |
| Custody: the Silicon's session stays in its home; the daemon borrows it under the same lock | Matches IAM's guidance (session storage in the app's daemon) and the DM precedent; the backend holds no IAM tokens (not a credential vault). Refresh keys are derived from the refresh token so any process can replay an uncertain refresh safely. |
| Backend mints a fresh OBO proof per Ting send, bound to exact bytes; `for`/`org_id` come from the verified session | Ting's contract; a Silicon can only notify itself. |
| Ting registration at explicit login only | Re-registering re-activates a grant the recipient may have revoked; `spotify ting register` is the explicit retry. |
| Durable outbox with stable keys `<recipient>/<trigger>/<play>/<outcome>` | Retries never duplicate a notification (Ting dedupes keys for 14 days). |
| Controls work without IAM login; triggers need it | Playing music is local; notifying someone needs an identity. `--local` triggers work without Ting. |
| Auto-start the Spotify sign-in when a Silicon needs it (rate-limited) | The spec: "if not authenticated, run the authentication script when silicon tries to use it". A Carbon must click Agree, so the error says a browser tab was opened and to retry. |
| Hourly updater in the daemon for script installs; Honeycomb installs update through Honeycomb | The IAM app rules require an hourly checker in the daemon; Honeycomb's worker already updates its own installs. Updates are verified against SHA256SUMS and swapped atomically; the CLI restarts older daemons. |
| Telemetry via the backend gateway, keys only on the backend | Keys never ship in binaries; opt-out at config, env (incl. Stemcell's `SPACE_STATION_TELEMETRY=0`), header and server levels. |
| One error shape and IAM exit-code taxonomy | Agents branch on stable codes and get the exact fix. |
| rustls with `ring` for CLI/daemon | Pure-Rust-friendly crypto so all six Honeycomb targets cross-compile (zig for Linux musl, xwin for Windows). |

## Spec coverage

| Spec item | Where |
| --- | --- |
| play / pause | `spotify play`, `resume`, `pause`, `toggle`, `next`, `previous`, `seek`, `volume`, `shuffle`, `repeat` |
| current song details | `spotify status [--full]`, `spotify track` |
| lyrics | `spotify lyrics` |
| CRUD queue | `spotify queue add/list/move/remove/clear` (managed) + Spotify's upcoming (read) |
| CRUD playlists | `spotify playlist list/show/create/delete/add/remove/play/import/fork/sync` (rename: unsupported by the tools, explained) |
| search | `spotify search --type … --limit … [--play]` |
| podcast | `spotify podcast search/play/saved/now` |
| trigger (X s left, 25% left, 50% passed, song over) | `spotify trigger add --remaining 30s / 25% / --elapsed 50% / --end` (+ `--change`, scopes, times, notes) |
| all triggers via Ting | daemon outbox → backend → Ting (`spotify.trigger.fired` / `.expired`) |
| silicon login via IAM SLT | `spotify iam --json`, `login`, `login status --json`, `logout` |
| spotify_player first, AppleScript fallback | `crates/client/src/control.rs` |
| backend for auth and registering tings | `src/` (`/api/v1/auth/*`, `/api/v1/ting/subscription`, `/api/v1/tings`) |
| installation installs dependencies and keeps them running | `scripts/install.sh`, `packaging/honeycomb-install-macos.sh`, launchd agent, warm spotify_player |
| run authentication when needed | daemon auto-starts `spotify_player authenticate` on `spotify_auth_required` |
| clear errors | `docs/errors.md`, `crates/client/src/error.rs` |
| IAM app rules: config set JSON, report with PR, docs in CLI, command tree, telemetry, BYO, testing, updates, versioning | `config`, `report`, `docs`, `commands`, `docs/telemetry.md`, `docs/config.md`, `testing`, updater, `docs/versioning.md` |

## Known limits

- Spotify control needs macOS (AppleScript and Spotify.app). Other platforms get the CLI for
  login/config/docs/report and a clear `platform_unsupported` elsewhere.
- Web API features need spotify_player signed in (a Carbon clicks Agree once) and some need
  Premium; AppleScript control works without either.
- Playlist rename/description edits, removing items from Spotify's native queue, and episode
  lists of a show are not possible through spotify_player or AppleScript.
- Ad-hoc signed macOS binaries make macOS ask for the Automation permission again after updates;
  a Developer ID certificate fixes that (`SPOTIFY_CODESIGN_IDENTITY`).
