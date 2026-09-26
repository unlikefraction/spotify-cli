# Configuration

Settings live in `$SILICON_HOME/.spotify/config.json` (0600). Set any number of keys with one JSON
object; `null` resets a key to its default. Unknown keys, duplicate keys, wrong types and
out-of-range values are rejected (`invalid_input`) before anything changes, and the error names the
key and what it accepts, e.g. `verify_timeout_ms must be an integer from 200 to 15000, not "fast".`
Only the keys you set are checked, so a stale value of another key never blocks a change.

```sh
spotify config set '{"telemetry": false}'
spotify config set '{"strategy": "applescript", "launch_spotify": false}'
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
| `strategy` | `auto` \| `spotify_player` \| `applescript` | `auto` | Playback path (`spotify docs playback`). |
| `launch_spotify` | bool | `true` | Launch Spotify.app hidden when a control command finds it closed. |
| `verify_timeout_ms` | 200–15000 | `2500` | How long to wait for spotify_player's effect before falling back. |
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
| `SPOTIFY_WARM_PLAYER=off` | Do not run the warm spotify_player instance. |
| `SPOTIFY_DAEMON_AUTOSTART=0` | Never start the daemon on demand (commands that need it fail with `daemon_unavailable`). |
| `SPOTIFY_DEBUG=1` | Print full error details in human mode. |
