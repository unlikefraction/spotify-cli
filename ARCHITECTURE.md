# spotify-cli architecture

How it is built and why, measured against two inputs: the product spec (a Spotify CLI with
playback triggers delivered over Ting) and the ecosystem rules every IAM app follows.

## Shape

```text
            ┌──────────── this Mac (one OS user) ─────────────────────────────┐
 Carbon /   │ spotify (CLI) ──unix socket──▶ spotify-daemon ──NSAppleScript──▶ Spotify.app
 Silicon ──▶│   │  per-home store           │  distributed notifications ◀──┘
            │   │  $SILICON_HOME/.spotify   ├──▶ spotify_player (warm, pty) ──▶ Spotify Web API
            │   │                           ├──▶ Spotify Web API directly (starts of songs,
            │   │                           │    episodes, shows and Liked Songs, lookups, search
            │   │                           │    fallback; spotify_player's cached token)
            │   │                           │  triggers → outbox (SQLite)
            └───┼───────────────────────────┼──────────────────────────────────┘
                │ SLT, refresh, report      │ Silicon's own oat_ token
                ▼                           ▼
             backend (spotify-api, holds the IAM app secret)
                │ IAM: SLT exchange, introspection, OBO consent/tokens      │ Ting: subscriptions, tings
```

- **Library (stateless)** — `silicon-spotify-client`: models, parsers, AppleScript builders and
  parser, spotify_player wrapper with failure classification, the Web API start (`webapi`), the
  focus hand-back (`focus`), the verified controller, the pure trigger engine, the backend client.
  Feature `runtime` adds the per-home store and IPC client.
- **Daemon (always on)** — one per OS user because there is one Spotify.app per user; it serves
  every Silicon home. The CLI starts it on demand and replaces it when older.
- **CLI** — a client of the library and the daemon; also does login/config/report/docs directly,
  and answers the discovery commands (`spotify` alone, `how`, `commands --json`, `completions`)
  offline from its own command tree and bundled guides.
- **Backend** — exists to keep the IAM app secret server-side, as IAM requires.

## Decisions

| Decision | Why |
| --- | --- |
| The Web API first (through spotify_player, a Web API client, and directly for song, episode, show and Liked Songs starts), **verify** against Spotify.app, AppleScript fallback, report `via`/`fallback` (exceptions under `auto`: exact seeks go to AppleScript first; state-dependent spotify_player commands run only when its view matches Spotify.app) | The spec says try spotify_player then AppleScript. Live QA showed where that order hurts: spotify_player's relative seek and its track, Liked Songs and show starts act on stale or empty state (or cannot run), and `like`/`repeat` act on whatever its instance believes is current, so those paths check first, go straight to AppleScript (seeks) or to the Web API directly (those starts). spotify_player hands commands to its instance asynchronously and exits 0 before (or without) any effect, and starting a single track by id leaves the desktop app with no track, so "fails" must mean "did not take effect". Verification makes the rule precise and observable. |
| Songs, episodes, shows and Liked Songs start through the **Web API directly** (`PUT /me/player/play` with the album, show, Liked Songs or `--context` as `context_uri` and the item as `offset`, sent to the device Spotify.app plays on), verified by reading Spotify.app; AppleScript is the fallback, with the focus handed back | AppleScript's `play track` brings Spotify.app to the front and takes the focus from whatever the user works in; a Web API start does not. Naming a list and an offset (never a bare list of ids) leaves Spotify.app with the album loaded, so it plays on. The devices are listed at every start: an active speaker or phone that Spotify.app controls keeps playing (as Spotify.app's own play button would), a start while Spotify.app plays on a device the Web API does not list names no device, and otherwise Spotify.app on this Mac is chosen by this Mac's name, never "the only computer" when the name is known, so a start never lands on another Mac, the Web Player or spotify_player's own device. The request reuses spotify_player's cached token (no second Spotify sign-in) and one process-wide rate-limit pause shared with the daemon's lookups and searches. It succeeds only once Spotify.app plays the item or its relinked release. A 5xx or lost answer is looked for in Spotify.app before anything else, so a start that went through is never made twice; a rate limit, missing Premium, a restricted or unregistered device or no effect falls back to AppleScript, which works everywhere and plays a song in the album the lookup found. Liked Songs' shuffle is set after its start, because Spotify switches to the shuffle it keeps per list when the list starts. |
| Focus hand-back after AppleScript starts (`keep_spotify_in_background`, default true): note the front app, watch it during and 1 s after the start, give it back (`lsappinfo setfront`, else `open -a` for regular apps that still run), re-hide Spotify.app if it was hidden | Some starts still go through AppleScript (the restore after a failed start, explicit AppleScript strategy, and every fallback, including managed-queue starts). Live on macOS 27, Spotify.app activates ~0.1 s after `play track` returns and `setfront` from a background process is usually ignored, so the watcher runs past the return and falls back to `open -a`. LaunchServices and `NSRunningApplication.hide` via JXA need no new permission (no Apple Events). |
| Search falls back to the Web API's own `GET /v1/search` when spotify_player's search fails, and says which answered (`via`) | spotify_player cannot parse some of Spotify's answers (a playlist with a null field fails the whole search). The same token and rate-limit pause answer instead; refusals the Web API would repeat (a rate limit, no sign-in, bad input) are not searched again. |
| In-process `NSAppleScript` on the main thread (objc2) | `osascript` costs ~0.1 s CPU per call; the daemon polls, so it compiles each script once and runs it in-process (~50 ms, mostly Spotify's own reply time). |
| Spotify's distributed notification + adaptive polling | Notifications arrive instantly on play/pause/track change but not on seeks; adaptive readings catch seeks and give precise checkpoint timing without constant polling. |
| Warm spotify_player on a daemon-owned pty that answers terminal queries | spotify_player CLI calls take ~1.5 s without a running instance and ~20 ms with one. The Homebrew build has no daemon mode, and the TUI refuses to start without a terminal answering its cursor/attribute queries. Streaming/media keys/notifications are disabled so it never becomes a playback device. |
| Managed queue in the daemon | Neither the Web API nor AppleScript can remove or reorder Spotify's queue, and spotify_player's CLI cannot even append. "CRUD queue" is only possible with our own queue; Spotify's upcoming list is shown read-only. Automatic starts, managed `next` and resuming the interrupted context use the same verified controller as `play`, honoring the configured strategy (Web API first under `auto`). |
| Trigger engine as pure code | Every rule (plays, completion, expiry, no retroactive fires, restart-safe play ids) is unit-tested without Spotify. |
| Current-scope triggers expire loudly (`spotify.trigger.expired`) | A Silicon waiting for "30 s left" on a song that got skipped must not wait forever. |
| Custody: the Silicon's session stays in its home; the daemon borrows it under the same lock | Matches IAM's guidance (session storage in the app's daemon) and the DM precedent; ordinary login tokens stay local; the backend encrypts separately approved OBO root tokens. Refresh keys are derived from the refresh token so any process can replay an uncertain refresh safely. |
| Backend stores each approved OBO root separately, encrypted; Ting verifies its reusable access token on every operation | Feature consent selects provider context. Stable notification keys deduplicate writes. |
| Ting consent and registration are separate from login | `spotify ting authorize` requests feature permission; `spotify ting complete` stores it and registers. Ordinary logout preserves consent. |
| Durable outbox with stable keys `<recipient>/<trigger>/<play>/<outcome>` | Retries never duplicate a notification (Ting dedupes keys for 14 days). |
| Controls work without IAM login; triggers need it | Playing music is local; notifying someone needs an identity. `--local` triggers work without Ting. |
| Auto-start the Spotify sign-in when a Silicon needs it (rate-limited) | The spec: "if not authenticated, run the authentication script when silicon tries to use it". A Carbon must click Agree, so the error says a browser tab was opened and to retry. |
| Hourly updater in the daemon for script installs; Honeycomb installs update through Honeycomb | The IAM app rules require an hourly checker in the daemon; Honeycomb's worker already updates its own installs. Updates are verified against SHA256SUMS and swapped atomically; the CLI restarts older daemons. |
| Telemetry via the backend gateway, keys only on the backend | Keys never ship in binaries; opt-out at config, env (incl. Stemcell's `SPACE_STATION_TELEMETRY=0`), header and server levels. |
| One error shape and IAM exit-code taxonomy | Agents branch on stable codes and get the exact fix. |
| The CLI describes itself: grouped help, `spotify how`, `Next:` hints, `spotify commands --json`, orientation with no arguments, completions, unknown subcommands answered with the real ones; drift tests tie guides to the grammar | Silicons and Carbons should find a command without knowing its name. The manifest (arguments with types and allowed values, examples, output fields, errors, requirements, and whether a command changes anything: `mutates`, `changes`, `read_only_when`) lets a Silicon plan a call without parsing help text; `how` answers offline from the same help, guides and settings, with examples made for the question (a song name becomes a search, a setting a `config set`). Tests fail when a guide or README shows a command, flag, value or topic the CLI does not have, or when a help example does not parse, so docs and help cannot drift apart. Hints go to stderr and never into `--json`, so pipes and agents are unaffected. |
| rustls with `ring` for CLI/daemon | Pure-Rust-friendly crypto so all six Honeycomb targets cross-compile (zig for Linux musl, xwin for Windows). |

## Spec coverage

| Spec item | Where |
| --- | --- |
| play / pause | `spotify play`, `resume`, `pause`, `toggle`, `next`, `previous`, `seek`, `volume`, `shuffle`, `repeat` |
| current song details | `spotify status [--full]`, `spotify track` |
| lyrics | `spotify lyrics` (the current song), `spotify lyrics <uri\|link\|id>` (any song; nothing has to play; by name: `spotify search '<words>' --type track`, then `spotify lyrics <uri>`) |
| CRUD queue | `spotify queue add/list/move/remove/clear` (managed) + Spotify's upcoming (read) |
| CRUD playlists | `spotify playlist list/show/create/delete/add/remove/play/import/fork/sync` (rename: unsupported by the tools, explained) |
| search | `spotify search --type … --limit … [--play]` |
| podcast | `spotify podcast search/play/saved/now` |
| trigger (X s left, 25% left, 50% passed, song over) | `spotify trigger add --remaining 30s / 25% / --elapsed 50% / --end` (+ `--change`, scopes, times, notes) |
| all triggers via Ting | daemon outbox → backend → Ting (`spotify.trigger.fired` / `.expired`) |
| silicon login via IAM SLT | `spotify iam --json`, `login`, `login status --json`, `logout` |
| spotify_player first, AppleScript fallback | `crates/client/src/control.rs` (song, episode, show and Liked Songs starts: `webapi.rs`; focus hand-back: `focus.rs`) |
| backend for auth and registering tings | `src/` (`/api/v1/auth/*`, `/api/v1/ting/subscription`, `/api/v1/tings`) |
| installation installs dependencies and keeps them running | `scripts/install.sh`, `packaging/honeycomb-install-macos.sh`, launchd agent, warm spotify_player |
| run authentication when needed | daemon auto-starts `spotify_player authenticate` on `spotify_auth_required` |
| clear errors | `docs/errors.md`, `crates/client/src/error.rs` |
| IAM app rules: config set JSON, report with PR, docs in CLI, command tree, telemetry, BYO, testing, updates, versioning | `config`, `report`, `docs` (`--section`), `commands` (`--json` manifest), `how`, `docs/telemetry.md`, `docs/config.md`, `testing`, updater, `docs/versioning.md` |

## Known limits

- Spotify control needs macOS (AppleScript and Spotify.app). Other platforms get the CLI for
  login/config/docs/report and a clear `platform_unsupported` elsewhere.
- Web API features need spotify_player signed in (a Carbon clicks Agree once) and some need
  Premium; AppleScript control works without either. Without them, every start goes through
  AppleScript, and Spotify.app comes to the front for about half a second before the focus goes
  back; a radio cannot start.
- A device that takes no Web API commands (restricted) is started through AppleScript. Liked
  Songs and show starts are verified by "something new plays", since AppleScript cannot read the
  list that plays; spotify_player's own device is recognised only by its default name.
- Playlist rename/description edits, removing items from Spotify's native queue, and episode
  lists of a show are not possible through spotify_player or AppleScript.
- Ad-hoc signed macOS binaries (development builds) make macOS ask for the Automation permission
  again after each build. Releases are Developer ID signed with the hardened runtime and notarized
  (`SPOTIFY_CODESIGN_IDENTITY`, `SPOTIFY_NOTARY_PROFILE`), so the answer survives updates. Bare
  executables cannot be stapled; Gatekeeper checks the ticket online.
