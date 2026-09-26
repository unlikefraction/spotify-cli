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
| 1 | the operation failed (see `code`) |
| 2 | usage: bad arguments or input (`invalid_input`, `usage`, `threshold_passed`) |
| 3 | not signed in (`not_authenticated`, `spotify_auth_required`, `reconsent_required`) |
| 4 | refused (`automation_permission_denied`, `permission_denied`, `forbidden`, `recipient_not_registered`) |
| 5 | unavailable: daemon, network, rate limit, timeout |

## Codes

| Code | Meaning | Fix |
| --- | --- | --- |
| `invalid_input` / `usage` | arguments or values are wrong | the hint shows accepted forms; `<command> --help` |
| `not_authenticated` | no IAM session in this home, or it was revoked | `spotify login '<SLT>'` |
| `slt_rejected` | the SLT expired (~2 min), was used, or is for another app | mint a fresh one for `spotify` |
| `reconsent_required` | the session lacks Ting scopes | log in again approving all scopes |
| `recipient_not_registered` | Ting has no grant for this app to notify you | `spotify ting register` |
| `spotify_auth_required` | spotify_player is not signed in to Spotify | `spotify auth login` |
| `spotify_player_missing` | spotify_player is not installed | `brew install spotify_player` |
| `spotify_player_failed` | spotify_player failed in an unexpected way | retry; `spotify doctor`; report |
| `no_active_device` | the Web API sees no active device | play anything in Spotify.app once |
| `premium_required` | Web API playback needs Premium | `spotify config set '{"strategy": "applescript"}'` |
| `spotify_not_running` | Spotify.app is closed | `spotify launch` |
| `spotify_not_installed` | macOS cannot find Spotify.app | install Spotify |
| `automation_permission_denied` | macOS blocks Apple Events to Spotify | enable it in Privacy & Security → Automation |
| `nothing_playing` | Spotify has no current item | start something |
| `nothing_to_resume` | resume with nothing loaded | `spotify play <something>` |
| `not_allowed_in_context` | Spotify disallows shuffle/repeat here | play a playlist or album |
| `no_effect` | spotify_player accepted a command that did not happen (appears in `fallback.reason`) | none; AppleScript handled it |
| `verification_failed` | neither path produced the effect | check Spotify.app (ads, dialogs, offline) |
| `not_found` | no such item, lyrics, trigger, firing or queue entry | the hint names the lookup command |
| `unsupported` | the underlying tools cannot do this (rename playlists, repeat-one via AppleScript) | the hint names the alternative |
| `threshold_passed` | a `current` trigger's checkpoint is already behind | later checkpoint or `--scope every` |
| `queue_empty` | `next` from an empty managed queue | `spotify queue add <uri>` |
| `daemon_unavailable` | the daemon is down or unreachable | `spotify daemon start`; see its log |
| `daemon_missing` | spotify-daemon is not installed next to spotify | reinstall |
| `daemon_outdated` | an older daemon is running and could not be replaced | reinstall both from one release; `spotify daemon restart` |
| `testing_selection_changed` | a trigger's home now selects another plane | re-select it (`spotify testing use`) or recreate the trigger |
| `backend_unavailable` | the backend could not be reached | network; `spotify config get api_url` |
| `rate_limited` | Spotify or the backend is limiting | wait and retry |
| `timeout` | Spotify, spotify_player or the daemon did not answer in time | retry; if macOS shows "spotify-daemon wants access to control Spotify", click Allow first |
| `platform_unsupported` | this needs macOS | run it on the Mac that plays the music |
| `webhook_unverified` | backend only: an `/webhook/` delivery failed IAM signature, timestamp or header checks | none for users; the backend logs the reason |
| `internal` | a bug | `spotify report` |
