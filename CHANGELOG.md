# Changelog

## 0.1.4 — 2026-09-26

Daemon and its spotify_player:

- The warm spotify_player re-reads playback every 20 s instead of every 3 s. The 3 s poll (10
  GETs per 30-second window) kept the owner's own client ID rate-limited even while idle: a 429
  about every 30 s, each `Retry-After` (6–15 s) freezing spotify_player's view and stalling
  commands for that long (`spotify track` and `queue add --search` took about 9 s, `like` refused
  a song the view had not caught up with). 20 s is 1–2 GETs per window, so commands keep most of
  the quota. A positive `playback_refresh_duration_in_ms` in your `app.toml` is now kept when it
  is slower, and raised to 10 s when faster. `spotify daemon status` reads
  "playback refresh every 20 s" (`refresh_ms: 20000`). The first daemon start after the update
  replaces the 0.1.3 copy.
- spotify_player's view can now be up to about 20 s behind a change made in Spotify.app. Commands
  that depend on it already compare it with Spotify.app first. The queue's resume point, which
  took the interrupted context from that view unchecked, now reads the Web API once when the view
  names another item than the Web API's queue. A list started moments before the queue
  interrupts it is now the one resumed, not the list before it.

Playback control:

- Relinked songs (Spotify plays the same recording from another release under another id, so
  Spotify.app and the Web API name different ids) are recognised: by the Web API item's
  `linked_from`, or by the same title, a length within 1 s and the same album name.
  `spotify status --full` counts such a song as the current item: `web.relinked: true` (new,
  present only when true), no `web.stale` and no `web_state_stale` warning, served from
  spotify_player's memory instead of a fresh Web API read on every call. Before, it was stale
  forever, with a hint to retry in a few seconds. The checks before spotify_player's relative
  seek, `next` and `previous`, and the context kept to restore after a failed start, also count
  it as Spotify.app's song.
- `like` and `unlike` on a relinked song refuse at once, without the volume nudge and the wait,
  with `track_mismatch` that is now not retryable: spotify_player would save or remove the
  substitute's id, not the one Spotify.app shows. `details` add `relinked: true` and `matched_by`
  (`linked_from` or `title_length_album`); the hint points to the heart in Spotify.app.
- The `web_state_stale` hint for another item now says the Web API usually catches up within
  seconds, and that if it keeps reporting another item, `web` describes that item.
- `play --liked` without `--random` plays Liked Songs in list order from its first song. A
  shuffle Spotify kept for Liked Songs (from an earlier `--random`, or set in Spotify.app) is
  turned off, and stays off, and the list is started again; before, it played shuffled and
  always from the same song.
- `play --liked --random` switches Liked Songs' shuffle off and on, so Spotify draws a new order,
  then skips once, so it starts at a random song every time; before, a kept shuffle started on
  the same song again and again.
- Each of those steps is checked in Spotify.app: one that does not show is a retryable
  `verification_failed` naming the step (Liked Songs keeps playing), never a success. A Liked
  Songs start that does not show within 2.5 s fails then (with `--random` it took about 5 s, and
  a start that showed late counted as a success without its shuffle step).

Queue:

- Queued episodes get their name, show and length more reliably. What the caller knows (the
  search hit of `queue add --search`) is kept field by field and the rest is looked up, so an
  episode picked with `--search` gets its show; an empty name or show counts as missing.
- Facts a lookup did not get are looked up again: after a rate limit, in the background once
  `Retry-After` has passed (for up to 10 minutes), and after other failures up to 3 tries per
  item; `spotify queue` also looks them up itself for up to 2.5 s. Before, an episode queued
  while Spotify was rate-limiting stayed `episode <id>` for good. When a lookup is left to the
  background, the `queue add` reply carries `metadata_pending` (the queued item ids) and a
  `note`.
- The `spotify queue` note on repeats of the item playing now names its kind ("the track
  playing now") and gives a cause (repeat-one, no context, or nothing else up next in its album
  or playlist) only when spotify_player's view of that item shows it and Spotify.app's repeat flag
  agrees; otherwise it names none. Before, it blamed an episode without a context or repeat-one,
  also for a track at the end of its album with repeat off.

## 0.1.3 — 2026-09-26

Playback control:

- `like` and `unlike` never change the wrong song. spotify_player can only like the track it
  believes is playing, which could be an earlier one, so they now check that it is the song
  Spotify.app plays. If it is not, they nudge it once (an inaudible `playback volume <current>`)
  and wait up to `verify_timeout_ms` + 1.1 s. If it still does not match, they refuse with the new
  `track_mismatch` and change nothing (before, `like` could save the previous song, or report
  success for a podcast episode and do nothing). Episodes, ads and local files are refused as
  `unsupported`.
- Before a spotify_player command whose effect depends on its own memory (play, pause, toggle,
  shuffle, next, previous, seek), that memory is compared with Spotify.app. When they disagree,
  AppleScript acts at once and `fallback.reason.code` is the new `state_mismatch`; under strategy
  `spotify_player` that is the error. `toggle` sends an explicit pause or play chosen from
  Spotify.app's state.
- Seeks are exact: AppleScript sets the position first; spotify_player's relative seek is only
  the fallback, and under strategy `spotify_player` it is refused with `state_mismatch` when
  spotify_player is on another item. `play <track|episode|show>` goes to AppleScript only, so a
  start never leaves Spotify.app stopped with nothing loaded.
- `play --liked` plays the Liked Songs list itself through AppleScript, so `next` and `previous`
  stay in it; it no longer stops playback. `--random` turns shuffle on for Liked Songs (Spotify
  keeps it) and starts at a random song; `--limit` applies only when spotify_player starts the
  list. A failed start that leaves Spotify.app empty puts back what was playing, under any
  strategy (`details.restored`, or `details.restore_failed`).
- Refusals the running spotify_player logs (429, 403) are noticed at once instead of after the
  verify timeout: `fallback.reason` is `rate_limited`, `no_effect` or `no_active_device`, with
  `details.request` and `details.spotify_player`. `play`, `pause`, `toggle`, `volume` and
  `shuffle` wait at most 1.5 s before falling back. With the state checks and AppleScript-first
  seeks, `toggle`, `seek` and `previous` no longer take 3.6–6.8 s when spotify_player's command
  would have no effect.
- `previous` behaves the same on every path: from 3 s in, or when there is no item before it,
  it restarts the item; otherwise it goes to the previous item. The outcome's new `result`
  (`restarted` or `previous_item`) and the first line of the human output ("Back to the start of
  this item." / "Back to the previous item.") say which.
- `repeat` follows spotify_player's real order (off → track → context), confirms each step and
  checks the result in Spotify.app. It no longer reports success while leaving another mode, and
  a `repeat off` it cannot confirm never passes through repeat-one. When a refused step leaves
  repeat-one on, it returns that failure instead of an AppleScript success that leaves the song
  repeating. When spotify_player refuses for now, `repeat track` returns its error (e.g.
  `rate_limited`, retryable) instead of `unsupported`.
- AppleScript volume lands on the requested level (it was one below for most levels); 19, 39,
  59, 79 and 99 land one above. Verification is exact.
- `spotify status --full`: `web` gains `item_uri`, `is_playing`, `source` (`spotify_player` or
  `web_api`) and `stale`. When spotify_player's memory disagrees with Spotify.app, a fresh Web API
  read (1–4 s) is used instead; when even that is for another item, `web.stale` is true, repeat
  and shuffle that contradict Spotify.app are left out, and the new warning `web_state_stale`
  says so. New warnings also include `no_active_device` and `timeout`. The human output then
  takes repeat from Spotify.app and adds "web data out of date".
- Time left, `(-m:ss)`, in `spotify status` and `spotify track` is rounded to the nearest
  second, as is `playback.remaining` in trigger firings: a `--remaining 20s` firing reads 0:20,
  not 0:19 (`remaining_ms` is unchanged).
- `spotify launch --json` answers `launched: false, already_running: true` when Spotify.app was
  already running (it said `launched: true`) and does not run `open`; the human output says
  "Spotify.app was already running." or "Started Spotify.app (hidden).".

Queue:

- Quick or concurrent `spotify next`s are applied one after another and never hand off the same
  item or lose one. A `next` that arrives while the previous hand-off is still in flight skips
  that item and plays the one after it (`skipped` in the reply, and a
  `skipped <uri> (it was still being switched to)` line).
- `play <item>` (also `--liked` and radios) and `previous` wait for a `next` or queue hand-off in
  progress, and those wait for them, so changes reach Spotify in the order they were asked. A
  failed `play <item>` keeps the queue's resume point; if the managed item still plays, it stays
  the managed item.
- Episodes queued by URI get their name, show and length (a Web API lookup with
  spotify_player's cached token), and `queue add --search` passes the chosen hit's facts to the
  daemon (`known`). Items without a known name show as `episode <id>` / `track <id>`, Spotify's
  upcoming items show their artists or show, and `queue add` prints what it added
  ("Queued 1 item. Managed queue now has 3." plus a `+ name — by` line per item). Albums,
  artists, playlists and shows are refused as "is an album" (and so on), with a hint that fits.
- `spotify queue` leaves out Spotify's repeats of the item playing now (an episode played without
  a context was listed ten times) and prints a note saying so (`current_repeats_left_out`,
  `note` in `--json`).
- Resuming the interrupted list after the managed queue drains also works when its next item is
  a podcast episode (its URI was built as a track's).

Details, library and playlists:

- `spotify track` shows `liked: yes|no` for songs (`liked` in `--json`, from a Web API check made
  alongside the lookup, left out when it fails or takes over 2.5 s).
- `spotify track <playlist> --json` has the same envelope as the other kinds: `kind`, `item`,
  `playlist`, `owner`, `collaborative`, `track_count`, `duration_ms`, `duration`, `tracks`, `raw`.
- `spotify track --type` offers only `track`, `album`, `artist` and `playlist`; `show` or
  `episode` is a usage error. A bare id that is not found as a track gets a hint to pass
  `--type album|artist|playlist` or a full URI.
- Library and playlist reads (`library <section>`, `playlist list`, `playlist show`,
  `track <uri>`) are retried once by the daemon after a transient `transport` or
  `spotify_player_busy` error, and `library`, `playlist list` and `playlist show` once more by the
  CLI. Rate limits, timeouts and writes are not retried.
- A library or playlist read right after a playlist change or like/unlike waits until it can see
  the change (1.1 s), also when another identical read was in flight during the change, so
  `playlist list` right after `playlist delete` no longer lists the deleted playlist.

Daemon and its spotify_player:

- The warm spotify_player is the daemon's own and actually serves the CLI: copies left by earlier
  daemons (the pid in the new `warm-player.json`, or orphans with the daemon's exact overrides)
  are stopped before a start; it is reported `running` only once it holds the client port;
  another spotify_player holding the port (your own TUI) is left alone and reported as
  `deferred` with its pid; a copy that never gets the port is stopped and retried
  (`not_serving`). It dies with the daemon, even on SIGKILL.
- The warm copy refreshes playback every 3 s (`playback_refresh_duration_in_ms`; a smaller
  positive value in your `app.toml` is kept), about 20 small Web API requests a minute, so its
  view is rarely more than 3 s behind Spotify.app.
- `spotify daemon status` describes the warm spotify_player in words (pid and port, refresh
  interval, who holds the port, retry delays, errors). `spotify doctor`'s
  `spotify_player_warm_instance` check passes only when the daemon's own copy holds the port, and
  shows the state and the pid that holds the port when it fails.
- `spotify daemon restart` with the launchd agent loaded restarts the job in place
  (`launchctl kickstart -k`) and waits for a new pid; it used to stop the daemon and never start
  it again. It prints "spotify-daemon restarted via launchd (pid N, vX).". `daemon stop` and
  `uninstall` wait until launchd has unloaded the job; `daemon start` waits up to 15 s and keeps
  retrying while a previous instance is still exiting.
- `spotify update --check` reports the real `auto_update` setting and no longer leaves
  `daemon status` saying it is off.
- The telemetry relay drains the whole backlog (up to 2 000 events) every minute instead of 40
  events a minute.

CLI:

- Human output keeps Spotify names on one line (line breaks, tabs and other control characters
  become spaces); `--json` keeps the original strings. While a podcast episode plays, the status
  header has no dangling " — ", and `spotify track` shows `show:` instead of `album:`.
- The `spotify search` footer suggests `queue it: spotify queue add <uri>` only for tracks and
  episodes, and `look inside: spotify track <uri>` for albums, artists and playlists.
- `spotify report --attach` takes text files only: binary files, directories and a home's
  `session.json` or `testing.json` are refused with `invalid_input` before anything is sent. Of
  long files only the last 64 KiB are sent, starting at a whole character.
- Every `usage` error has `details.usage`, and `details.argument` and `details.value` when the
  parser names them; the hint always includes the usage line. `--limit -1` is
  `invalid_input` ("--limit -1 is out of range: it takes 1 to 10.") instead of the misleading
  `-- -1` tip, and a hyphenated value after an option (`--note -x`) gets the tip to write
  `--note=-x`.
- `spotify config set search_limit=5` or `config set search_limit 5` gets the JSON form as its
  hint: `spotify config set '{"search_limit": 5}'`. `config set --help` lists examples.
- `spotify doctor`, `daemon status`, `queue`, `launch`, `previous` and `next` say more in human
  output, as described above; the `--help` of `status`, `play`, `toggle`, `previous`, `seek`,
  `repeat`, `like`, `unlike`, `launch` and `track` describes the new behaviour.

Docs:

- The playback guide documents the control rule and its exceptions, the state checks, refusals,
  `previous`, `repeat`, volume, like/unlike and failed-start restores; the daemon guide the warm
  spotify_player, its states and costs, restart and library reads; the queue guide `next` under
  concurrency, ordering with explicit commands and item names. The error table gains
  `state_mismatch`, `track_mismatch` and `web_state_stale` and drops `queue_empty`, which no
  command returns (`spotify next` with an empty managed queue runs Spotify's own next).
- Developers can run control commands against Spotify.app without the daemon:
  `cargo run -p silicon-spotify-client --example control -- <command>`.

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
