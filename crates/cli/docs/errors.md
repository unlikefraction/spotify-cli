# Errors

Every failure has the same shape, in the CLI (`--json`: one object on stderr, empty stdout), the
daemon protocol and the backend:

```json
{"error": {"code": "spotify_auth_required", "message": "what happened and why",
           "hint": "the exact next step", "retryable": false, "details": {…}}}
```

Branch on `code`, never on `message`. `retryable: true` means repeating the same request later can
succeed. `details` carries evidence (stderr excerpts, both attempts of a verified action, request
ids).

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | success |
| 1 | the operation failed (see `code`); every code not listed below |
| 2 | usage: bad arguments or input (`invalid_input`, `usage`, `threshold_passed`) |
| 3 | not signed in (`not_authenticated`, `spotify_auth_required`, `reconsent_required`) |
| 4 | refused (`automation_permission_denied`, `permission_denied`, `forbidden`, `recipient_not_registered`) |
| 5 | unavailable (`daemon_unavailable`, `backend_unavailable`, `dependency_unavailable`, `transport`, `rate_limited`, `timeout`) |

## Bad arguments

Bad arguments are caught before anything runs, with exit 2. Parser errors are `usage` and keep
what the parser knows in `details`: `usage` (the command's usage line, also when the parser's own
text has none), `argument` (e.g. `"--limit <LIMIT>"`), `value` (the value it refused) and
`possible_values`, when they apply. The hint always includes the usage line. Values the parser
accepts but the command cannot use are `invalid_input`. Examples:

- `spotify track <id> --type show` is a `usage` error whose `details.possible_values` is
  `["track", "album", "artist", "playlist"]`: shows and episodes cannot be looked up by id.
- `spotify search x --limit -1` (or `--limit=-1`) is `invalid_input`
  `--limit -1 is out of range: it takes 1 to 10.` with `details` `{limit, min, max}`. The range
  fits the command: `podcast search` 1 to 10, `library` and `playlist list` 1 or more,
  `trigger history` 1 to 500. A negative `play --liked --limit`, `trigger add --times` or
  `daemon logs --lines` is a `usage` error.
- An option followed by a value that starts with `-` (`--note -x`) gets the tip
  `to pass '-x' as the value of --note, write --note=-x`.
- `spotify config set search_limit=5` is `invalid_input`
  (``Config must be one JSON object, not `search_limit=5`.``, with the object in `details.json`);
  `spotify config set search_limit 5` is a `usage` error. Both hints show the JSON form:
  `spotify config set '{"search_limit": 5}'`.

## Codes

### Input and sign-in

| Code | Exit | Meaning | Fix |
| --- | --- | --- | --- |
| `invalid_input` / `usage` | 2 | arguments or values are wrong: a malformed Spotify id or link, a reference of the wrong kind, a limit out of range, a config value of the wrong type, a `config set` given `key=value` instead of JSON, a `report --attach` file that is not text, or Spotify answering 400 Bad Request | the hint shows the accepted forms; `<command> --help` |
| `not_authenticated` | 3 | no IAM session in this home, or it was revoked or expired (the backend's `unauthenticated`) | `spotify login '<SLT>'` |
| `slt_rejected` | 1 | the SLT expired (~2 min), was used, or is for another app | mint a fresh one for `spotify` and log in right away (`spotify docs auth`) |
| `reconsent_required` | 3 | the session lacks Ting scopes | log in again approving all scopes |
| `forbidden` | 4 | IAM refused the request (scopes, consent, organization) | log in again; check the organization (`--org`) |
| `permission_denied` | 4 | Spotify refused (403: a playlist you neither own nor collaborate on), or a daemon socket, home or trigger belongs to another user or home | edit only your own playlists; use your own OS user and home |

### Spotify and playback

| Code | Exit | Meaning | Fix |
| --- | --- | --- | --- |
| `spotify_auth_required` | 3 | spotify_player is not signed in to Spotify | `spotify auth login` |
| `spotify_player_missing` | 1 | spotify_player is not installed | `brew install spotify_player` |
| `spotify_player_failed` | 1 | spotify_player failed in an unexpected way, printed malformed JSON, or printed text instead of JSON (`details`: command, stdout, parse_error) | retry; `spotify doctor`; report |
| `spotify_player_busy` | 1 | another spotify_player instance was starting (retryable) | retry in a moment |
| `transport` | 5 | spotify_player could not reach the Spotify Web API | check the network and retry |
| `no_active_device` | 1 | the Web API sees no active device (also a `fallback.reason`, and a `status --full` warning) | play anything in Spotify.app once |
| `premium_required` | 1 | Web API playback needs Premium | `spotify config set '{"strategy": "applescript"}'` |
| `spotify_not_running` | 1 | Spotify.app is closed | `spotify launch` |
| `spotify_not_installed` | 1 | macOS cannot find Spotify.app | install Spotify |
| `automation_permission_denied` | 4 | macOS blocks Apple Events to Spotify | enable it in Privacy & Security → Automation (`spotify docs daemon`) |
| `applescript_failed` | 1 | osascript could not run, or AppleScript failed without saying why | retry; `spotify doctor` |
| `nothing_playing` | 1 | Spotify has no current item | start something |
| `nothing_to_resume` | 1 | resume with nothing loaded | `spotify play <something>` |
| `not_a_podcast` | 1 | `spotify podcast now` while the current item is not an episode | `spotify podcast play spotify:episode:<id>` |
| `not_allowed_in_context` | 1 | Spotify disallows shuffle/repeat here | play a playlist or album |
| `no_effect` | 1 | spotify_player accepted a command that did not happen, or its instance logged a refusal. Usually only a `fallback.reason` (AppleScript made the change); it is the error under strategy `spotify_player`, and for `repeat` when a spotify_player step was not confirmed (retryable; `spotify docs playback`: Repeat) | under `auto`, none; else retry in a minute |
| `state_mismatch` | 1 | spotify_player's working state (play state, shuffle, current item) disagrees with Spotify.app, so its command would do nothing or the wrong thing. Under strategy `auto` it is normally only a `fallback.reason` and AppleScript made the change; under strategy `spotify_player` it is the error (retryable). `details`: `spotify_player`, `spotify_app` | under `spotify_player`: retry after the next track change, or `spotify daemon restart` |
| `track_mismatch` | 1 | `like`/`unlike` refused: spotify_player can only change its own current track, and that is not the song Spotify.app plays; it did not catch up after one nudge. Nothing changed (retryable; `details`: `spotify_app`, `spotify_player`, `catch_up_failed`). Not retryable with `details.relinked: true`: spotify_player plays the same song under another id (track relinking; `details.matched_by`: `linked_from` or `title_length_album`), so it would change that id; nothing changed. Not retryable without `relinked`: spotify_player's track changed during the command, so the like may have hit another song | retry in a few seconds; with `relinked`, use the heart in Spotify.app; otherwise, if not retryable, check `spotify library liked` |
| `web_state_stale` | — | a `spotify status --full` warning: the Web API's playback is for another item than Spotify.app's (or could not be refreshed), so `web.stale` is true and repeat or shuffle that contradict Spotify.app are left out (retryable). A song it plays under another id (relinked) is the same item: `web.relinked`, no warning | retry in a few seconds; if it keeps naming another item, `web` describes that item |
| `verification_failed` | 1 | neither path produced the effect. For `play --liked` also: Spotify.app did not show a shuffle change, the restart in list order or the skip to a random song (Liked Songs keeps playing; `spotify docs playback`) | check Spotify.app (ads, dialogs, offline) |
| `not_found` | 1 | no such item, lyrics, trigger, firing or queue entry (a well-formed id that does not exist) | the hint names the lookup command |
| `unsupported` | 1 | the underlying tools cannot do this (rename playlists, add episodes to a playlist, repeat-one via AppleScript, like an episode, ad or local file, start an episode, a show or a track with `--context` under strategy `spotify_player`) | the hint names the alternative |
| `rate_limited` | 5 | Spotify or the backend is limiting (also a `fallback.reason` when spotify_player's instance logged a 429, with `details.request` and `details.spotify_player`) | wait and retry |
| `timeout` | 5 | Spotify, spotify_player or the daemon did not answer in time | retry; if macOS shows "spotify-daemon wants access to control Spotify", click Allow first |
| `platform_unsupported` | 1 | this needs macOS | run it on the Mac that plays the music |

### Triggers and Ting

| Code | Exit | Meaning | Fix |
| --- | --- | --- | --- |
| `threshold_passed` | 2 | a `current` trigger's checkpoint is already behind | later checkpoint or `--scope every` |
| `recipient_not_registered` | 4 | Ting has no grant for this app to notify you | `spotify ting register` |
| `recipient_changed` | 1 | the trigger's home now holds another Silicon's session (in `spotify trigger history`) | remove the trigger and create it again as the right Silicon |
| `testing_selection_changed` | 1 | a trigger's home now selects another plane | re-select it (`spotify testing use`) or recreate the trigger |
| `testing_selection_missing` | 1 | a trigger made in a testing plane, but the home no longer selects one | `spotify testing use --app-secret-file -`, or remove the trigger |

The managed queue has no codes of its own: items that cannot be queued are `invalid_input`, an
unknown position, id or URI is `not_found`, and `spotify next` with an empty managed queue is not
an error (it runs Spotify's own next).

### Daemon, setup and updates

| Code | Exit | Meaning | Fix |
| --- | --- | --- | --- |
| `daemon_unavailable` | 5 | the daemon is down or unreachable | `spotify daemon start`; see its log |
| `daemon_missing` | 1 | spotify-daemon is not installed next to spotify | reinstall |
| `daemon_outdated` | 1 | an older daemon is running and could not be replaced | reinstall both from one release; `spotify daemon restart` |
| `protocol_mismatch` | 1 | the daemon speaks another protocol version (the CLI replaces an older daemon by itself) | `spotify daemon restart` |
| `unknown_op` | 1 | the daemon is older than the CLI and does not know the request | `spotify daemon restart` |
| `daemon_stuck` | 1 | spotify-daemon did not stop within 10 s | `pgrep -fl spotify-daemon`, then kill it |
| `daemon_running` | 1 | another spotify-daemon already runs for this user (in the daemon log) | `spotify daemon status`; `spotify daemon restart` replaces it |
| `launchd_failed` | 1 | `spotify daemon install` could not register the launchd agent (it needs a logged-in GUI session) | log in to the Mac's desktop and retry; `spotify daemon start` works without launchd |
| `daemon_dir` / `daemon_bind_failed` / `daemon_store` | 1 | the daemon cannot create `~/.silicon-spotify`, listen on its socket, or use its database | check the permissions and free space of `~/.silicon-spotify`; for a corrupt database, stop the daemon and move `daemon.sqlite` aside (triggers are lost) |
| `doctor_failed` | 1 | `spotify doctor` found a required check failing (`details.failed` names them) | run the listed fixes, or `spotify setup` |
| `homebrew_missing` | 1 | `spotify setup` needs Homebrew to install Spotify.app or spotify_player | install Homebrew (https://brew.sh), or `cargo install spotify_player` |
| `install_failed` | 1 | a `brew install` run by `spotify setup` failed | run the `brew` command yourself to see why |
| `no_release` / `download_failed` / `checksum_missing` / `checksum_mismatch` / `unpack_failed` | 1 | `spotify update` (or the hourly update) found no usable release, or the download did not verify; nothing was installed | retry later; report it if it persists |
| `update_not_writable` | 1 | the update cannot replace the binaries in their directory | re-run the installer (`spotify docs usage`), or install with Honeycomb |

### Files, backend and bugs

| Code | Exit | Meaning | Fix |
| --- | --- | --- | --- |
| `store_io` / `corrupt_store` / `unsafe_store` | 1 | a file under `$SILICON_HOME/.spotify` cannot be read or written, is not valid JSON, or the directory is a symlink | fix or delete the named file (deleting `session.json` logs you out); use a real directory owned by you |
| `backend_unavailable` | 5 | the backend could not be reached | network; `spotify config get api_url` |
| `dependency_unavailable` | 5 | the backend could not reach IAM or Ting (retryable) | retry later |
| `backend_unexpected_response` / `backend_error` | 1 | the backend answered in a shape this CLI does not know | `spotify update`, then retry |
| `internal` | 1 | a bug | `spotify report` |

## Backend codes

The backend API (`spotify docs api`) answers every error, including framework errors such as a
wrong method or an oversized body, with the same envelope and an `X-Request-Id` header. Its codes
by HTTP status:

| HTTP | Code | Meaning |
| --- | --- | --- |
| 400 | `invalid_input` | the request is malformed (missing header, bad JSON, bad field); also a request IAM rejected as malformed |
| 401 | `unauthenticated` | the bearer or refresh token is missing, malformed, expired or revoked. The CLI shows it as `not_authenticated` |
| 401 | `slt_rejected` | `POST /api/v1/auth/login`: IAM refused the SLT (expired, used, or minted for another app) |
| 401 | `invalid_testing_secret` | `X-Testing-Environment-Key` is not this app's 47-character `ask_…` test secret; select a plane with `spotify testing use --app-secret-file -` |
| 401 | `webhook_unverified` | an `/webhook/` delivery failed IAM signature, timestamp or header checks (nothing for users to do; the backend logs the reason) |
| 403 | `forbidden`, `reconsent_required`, `recipient_not_registered` | IAM or Ting refused; see the tables above |
| 404 | `not_found` | no such route, or Ting does not know the type |
| 405 | `method_not_allowed` | the route exists but not for this method; the `Allow` header lists the methods it accepts |
| 409 | `conflict` | an `Idempotency-Key` was reused with a different request; retry the original request with its original key |
| 413 | `payload_too_large` | the body is over 512 KiB, or a telemetry batch is over 64 KiB; send less or split it |
| 429 | `rate_limited` | slow down; `retryable: true` |
| 500 | `internal` | a bug; report it with the `X-Request-Id` |
| 503 | `dependency_unavailable` | IAM or Ting is unreachable or refused the backend's own credentials; `retryable: true` |

Through the CLI, every 401 is reported as `not_authenticated` (exit 3), except that
`spotify login` reports a refused SLT as `slt_rejected` (exit 1). `conflict`, `payload_too_large`
and `method_not_allowed` exit 1, and `dependency_unavailable` exits 5. Ting's own refusals pass
through with Ting's code.
