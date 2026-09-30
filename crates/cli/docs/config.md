# Configuration

Settings live in `$SILICON_HOME/.spotify/config.json` (0600). Set any number of keys with one JSON
object; `null` resets a key to its default. Unknown keys, duplicate keys, wrong types and
out-of-range values are rejected (`invalid_input`) before anything changes, and the error names the
key and what it accepts, e.g. `verify_timeout_ms must be an integer from 200 to 15000, not "fast".`
Only the keys you set are checked, so a stale value of another key never blocks a change.
`config set` takes one JSON object, quoted for the shell: `key=value` or `key value` words are
refused (exit 2) with the JSON form they meant as the hint, e.g.
`spotify config set search_limit=5` suggests `spotify config set '{"search_limit": 5}'`.

```sh
spotify config set '{"telemetry": false}'
spotify config set '{"strategy": "applescript", "launch_spotify": false}'
spotify config set '{"keep_spotify_in_background": false}'   # only if Spotify.app may stay in front
spotify config set '{"notify_isi": "planner", "search_limit": 5}'
spotify config set '{"strategy": null}'
spotify config show          # effective values, defaults filled in, env overrides
spotify config get strategy
spotify config keys          # every key with type, default and meaning
spotify config reset
```

Stemcell applies `silicon.app_configs.spotify` with exactly this command
(`spotify config set '<compact JSON>'`).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `api_url` | https origin | `https://backend.spotify.unlikefraction.com` | Backend. `SPOTIFY_API_URL` overrides. |
| `telemetry` | bool | `true` | Usage and diagnostics to Space Station (`spotify docs telemetry`). |
| `org` | org handle | `SILICON_ORG`, then the session's | Organization for Ting delivery. |
| `strategy` | `auto` \| `spotify_player` \| `applescript` | `auto` | Playback path (`spotify docs playback`). `auto`: every start goes through the Web API first (songs, episodes, shows and Liked Songs directly, the rest through spotify_player), AppleScript is the fallback. `applescript` never uses the Web API, so every start goes through AppleScript (a radio cannot start). `spotify_player` never uses AppleScript. |
| `launch_spotify` | bool | `true` | Launch Spotify.app hidden when a control command finds it closed. |
| `verify_timeout_ms` | 200–15000 | `2500` | How long to wait for spotify_player's effect, or for a Web API start (a song, episode, show or Liked Songs) to show in Spotify.app, before falling back to AppleScript; a start whose Web API answer was lost is looked for as long before anything starts it again (`play`, `pause`, `toggle`, `volume` and `shuffle` wait at most 1.5 s); `like` waits up to this plus 1.1 s for spotify_player to catch up (`spotify docs playback`). |
| `keep_spotify_in_background` | bool | `true` | When an AppleScript start brings Spotify.app to the front (the fallback when a Web API start was not possible or did not take effect, an album or playlist spotify_player could not start, the restore after a failed start, managed-queue starts that use AppleScript), give the focus back to the app that had it, and hide Spotify.app again if it was hidden. On by default, so keeping Spotify.app from jumping to the front needs nothing: check it with `spotify config get keep_spotify_in_background`, and turn it off (`false`) only if you want Spotify.app to stay in front. Needs no macOS permission (`spotify docs playback`). |
| `spotify_player_binary` | absolute path | auto-detect | spotify_player executable. |
| `spotify_player_config_dir` | absolute path | `~/.config/spotify-player` | spotify_player `-c`. |
| `spotify_player_cache_dir` | absolute path | `~/.cache/spotify-player` | spotify_player `-C` (its Spotify tokens). |
| `search_limit` | 1–10 | `10` | Results per kind for `spotify search` (spotify_player returns at most 10). A larger value saved by an earlier release (which allowed up to 50) is applied as 10. |
| `notify_isi` | string | unset | ISI named in Ting metadata when `ISI` is not set. |
| `auto_update` | bool | `true` | Daemon installs new releases hourly (script installs). |
| `output` | `human` \| `json` | `human` | Default output format. |

## Bring your own (BYO)

- **Spotify app/client id.** spotify_player talks to Spotify with a client id. Register your own
  at https://developer.spotify.com/dashboard (redirect URI `http://127.0.0.1:8989/login`), put
  `client_id = "…"` in a spotify_player config folder, and point spotify-cli at it:
  `spotify config set '{"spotify_player_config_dir": "/path/to/folder"}'`, then `spotify auth login`.
- **Backend.** Run your own backend (open source, `spotify docs development`) with your own IAM
  app and point `api_url` at it.

## Environment

| Variable | Effect |
| --- | --- |
| `SILICON_HOME` | Home whose `.spotify/` holds config and sessions (else `$HOME`). |
| `SILICON_ORG` | Default organization. |
| `ISI` | Recorded as `metadata.isi` on notifications from triggers you create. |
| `SPOTIFY_API_URL` | Backend origin for this process. |
| `SPOTIFY_TEST_APP_SECRET` | Select a testing plane for this process. |
| `SPOTIFY_TELEMETRY`, `SPACE_STATION_TELEMETRY`, `SILICON_TELEMETRY` | `0`, `false`, `off` or `no` (any case) disables telemetry. |
| `SPOTIFY_DAEMON_HOME` | Daemon state directory (else `~/.silicon-spotify`). |
| `SPOTIFY_WARM_PLAYER=off` | Do not run the warm spotify_player instance (for the daemon; copies left by earlier daemons are still stopped). |
| `SPOTIFY_DAEMON_AUTOSTART=0` | Never start the daemon on demand (commands that need it fail with `daemon_unavailable`). |
| `SPOTIFY_DEBUG=1` | Print full error details in human mode. |
| `SPOTIFY_HINTS` | The `Next:` suggestions after human output go to stderr, and only when stdout is a terminal: never when stdout is a pipe or a file, where they would show up before the output the pipe's reader prints (`--json` never prints them). `0`, `false`, `off` or `no` (any case) turns them off; `always` prints them even when stdout is not a terminal. |
