# Changelog

## 0.1.2 — 2026-09-26

CLI:

- `spotify search queen` and other searches whose results hold a line break work again:
  spotify_player output with raw control characters inside strings now parses. When output still
  cannot be parsed, `spotify_player_failed` says whether it was malformed JSON or plain text, with
  the command, stdout and parse error in `details`.
- `spotify track <album>` shows the album type, release date, track count, length and track list,
  and `spotify track <artist>` its top tracks, albums and singles and related artists, instead of
  an empty template. `--json` adds `kind`, `item` and those lists next to `raw`; album items gain
  `album_type`.
- Spotify references are checked before anything is sent to Spotify: ids must be 22 letters and
  digits, and playlist commands take only playlists. Anything else is `invalid_input` (exit 2)
  with the accepted forms in the hint. `playlist add`/`remove` check every item first, so an edit
  never stops half way (episodes are `unsupported`). Spotify's "400 Bad Request" is
  `invalid_input` instead of `spotify_player_failed`.
- Limits that cannot be honoured are refused instead of quietly changed: `search --limit` and
  `podcast search --limit` take 1–10 (spotify_player returns at most 10 per kind), as does the
  `search_limit` setting (a larger value saved by an earlier release is applied as 10, with a note);
  `library --limit` and `playlist list --limit` take 1 or more; `trigger history --limit` 1–500.
- `spotify trigger add` rejects `--times 0`, a note over 1000 or a label over 80 characters, a
  percentage outside 0–100% and a `--track` that is not a track or episode as `invalid_input`
  (exit 2) before reading Spotify, whatever is playing (was an Automation or Spotify error).
- `spotify config set` errors name the key and what it accepts, with a copyable example, e.g.
  `verify_timeout_ms must be an integer from 200 to 15000, not "fast".` Setting one key no longer
  fails because another saved key is out of range.
- Usage errors keep clap's details in `--json`: the missing arguments, `Possible values: …`
  (`details.possible_values`), tips and the usage line (`details.usage`).
- `spotify queue add --type` offers only `track` and `episode`.
- New `spotify playlist fork --name <NAME>`. `playlist create --json` returns the bare `id` and
  `uri` (spotify_player 0.25 gave `spotify:playlist:spotify:playlist:…`); `playlist fork --json`
  also returns the new playlist's `id`, `uri` and `name`. `playlist create` without
  `--description`, or with a name starting with `-`, works.
- `spotify doctor` asks the backend for its version and adds the required check
  `cli_version_supported` when the backend names a `min_cli`.
- `playlist show` says "1 track" / "N tracks". Help examples fit in 100 columns, so the
  `spotify login` pipe example and `spotify report --pr` no longer wrap mid-command.

Daemon:

- The macOS Automation state now comes from what Apple Events actually do.
  `automation_permission_pending` is gone: while macOS's dialog is open, commands fail with
  `timeout` whose hint names the dialog, and after one timeout the daemon fails fast for 3 s,
  including commands already waiting, instead of stacking up. A reading that finds Spotify closed
  no longer counts as `granted`; `spotify doctor` shows an `unknown` state as optional with a fix,
  and waits out the 3 s window so it really re-checks. In the daemon's first two minutes a closed
  Spotify is polled every 3 s until the first Apple Event reaches it. A denied permission no longer
  waits 20 s for a launching Spotify.
- Telemetry is relayed in requests under the backend's 64 KiB limit instead of 40-event batches
  that could exceed it and be dropped.
- Daemon ops refuse search and history limits they cannot honour, for every client, and
  `track.info` returns the structured album/artist view itself.

Backend:

- Every error, including 405 `method_not_allowed` and 413 `payload_too_large` (bodies over
  512 KiB), is the JSON envelope with an `X-Request-Id` and a non-empty hint.
- `POST /api/v1/auth/logout` answers 204 for tokens IAM does not recognise (was 503).
- A refused SLT is 401 `slt_rejected` with advice about the SLT; a refused refresh token is 401
  `unauthenticated`; a plain IAM 400 is `invalid_input` (was 503 `dependency_unavailable`).
- Telemetry batches over 64 KiB get 413 `payload_too_large` instead of being dropped silently.
- A malformed bearer is 401 `unauthenticated` before `X-Org-ID` is checked.
- IAM webhook deliveries that fail verification get `webhook_unverified`, and the reason is
  logged.

Installer and website:

- The installer adds its PATH line under `# spotify-cli` unless that exact line is already there
  (older `# silicon-spotify` blocks count); another install directory gets its own line. Its closing
  lines ask you to click Allow only while Automation is not granted, and print the System
  Settings path when it was denied. The Honeycomb post-install says the same.
- The docs have a search box (press `/`), code blocks have a header strip whose Copy button no
  longer covers code, and page summaries are whole sentences. Website analytics are off under
  Global Privacy Control or Do Not Track, and the footer says what is collected.
- Docs: a Concepts glossary, installer options and uninstall steps, every error code with its
  exit code, and a `tccutil` reset scoped to spotify-daemon.

## 0.1.1 — 2026-09-26

- The daemon no longer hangs while macOS asks whether it may control Spotify. It raises the
  Automation dialog itself at startup, and until someone clicks Allow commands fail fast with
  `automation_permission_pending` (exit 4) instead of timing out. `spotify doctor` no longer
  reports the permission as granted after a timeout; `spotify daemon status` shows it; the
  installer waits for the answer.
- Backend: graceful shutdown on SIGTERM; `/healthz` reports `service: spotify-cli`.

## 0.1.0 — 2026-09-26

First release.

- `spotify` CLI: playback (verified spotify_player → AppleScript fallback), track details, lyrics,
  search, library, devices, playlists, podcasts, managed queue.
- Triggers (`--remaining`, `--elapsed`, `--end`, `--change`; current/every/track scopes) delivered
  as `spotify.trigger.fired` / `spotify.trigger.expired` Tings, durable and idempotent.
- IAM SLT login, live `login status --json`, logout, Ting recipient registration, testing planes.
- `spotify-daemon`: in-process AppleScript, Spotify notifications, warm spotify_player, outbox,
  telemetry relay, hourly updates.
- Backend `spotify-api`: SLT exchange, refresh, logout, me, Ting registration and sends with OBO
  proofs, bug reports, telemetry gateway, IAM webhooks.
- One-line installer, website and docs, Honeycomb package for six targets.
