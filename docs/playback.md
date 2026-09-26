# Playback control

## The rule: spotify_player first, verified, AppleScript fallback

spotify-cli drives Spotify through two tools:

- **spotify_player** — a Spotify Web API client. It can do everything the Web API can: start
  albums/playlists/radios/Liked Songs, repeat-one, likes, search, lyrics, playlists, devices.
- **AppleScript** — Spotify.app's own scripting interface on this Mac: play/pause, next/previous,
  play any URI (tracks, episodes, playlists, albums, shows), seek to an absolute position, volume,
  shuffle, context repeat, and complete player state in ~50 ms.

For every playback command (`strategy: auto`, the default):

1. Run the spotify_player command.
2. **Verify** the effect in Spotify.app (AppleScript reads its state every 120 ms for up to
   `verify_timeout_ms`, default 2500 ms). spotify_player hands commands to its running instance
   asynchronously and exits 0 before anything happened, and some commands report success while
   doing nothing (starting a single track by id leaves the desktop app with *no* track), so exit 0
   is never trusted.
3. If it errored or did not take effect, run the AppleScript equivalent and verify again.
4. Report which path worked:

```json
{"action": "play", "via": "applescript",
 "fallback": {"from": "spotify_player", "reason": {"code": "no_effect", "message": "spotify_player accepted `play` but Spotify.app did not change within 2500 ms.", …}},
 "playback": {…}}
```

If both fail you get `verification_failed` (retryable) with both attempts in `details`.

Other strategies (`spotify config set '{"strategy": "…"}'`):

| Strategy | Use when |
| --- | --- |
| `auto` | default |
| `applescript` | no Premium, no spotify_player sign-in, or you want the fastest local control |
| `spotify_player` | you control another Spotify Connect device and do not want AppleScript |

## What each command does

| Command | spotify_player | AppleScript fallback | Verified by |
| --- | --- | --- | --- |
| `play` (resume) | `playback play` | `play` | state is playing |
| `play <track>` | `playback start track` (often no effect) | `play track <uri>` | that track playing |
| `play <track> --context <list>` | — | `play track <uri> in context <list>` | that track playing |
| `play <album/playlist/artist>` | `playback start context` (`--shuffle`) | `play track <uri>` | new item playing |
| `play <show/episode>` | — | `play track <uri>` | that item playing |
| `play --liked`, `play --radio` | `playback start liked/radio` | none | new item playing |
| `pause` / `toggle` | `playback pause` / `play-pause` | `pause` / `playpause` | state flipped |
| `next` / `previous` | `playback next` / `previous` | `next track` / `previous track` | item changed (or restarted) |
| `seek` | `playback seek <offset>` (relative) | `set player position` (absolute) | position within 1.5 s |
| `volume` | `playback volume` | `set sound volume` | volume within 1 |
| `shuffle` | `playback shuffle` (toggle) | `set shuffling` | shuffling matches |
| `repeat off/context` | `playback repeat` (cycles) | `set repeating` | mode matches |
| `repeat track` | `playback repeat` (cycles) | none (AppleScript cannot) | mode matches |
| `like` / `unlike` | `like [--unlike]` | none | — |

Notes:
- `next` plays the **managed queue** first when it has items (`spotify docs queue`).
- Relative seeks through the Web API use the Web API's idea of the position, which can lag right
  after another seek; verification catches it and AppleScript seeks absolutely.
- Shuffle and repeat are refused by Spotify for singles and some contexts. `spotify status --json`
  shows `shuffle_allowed` / `repeat_allowed`; commands fail fast with `not_allowed_in_context`.

## Spotify.app not running

Reading state never launches Spotify. Control commands launch it hidden (`open -g -j -a Spotify`)
when `launch_spotify` is true (default); set it false to get `spotify_not_running` instead.
`spotify launch` starts it explicitly.

## Speed

The daemon keeps one headless spotify_player instance running on a pseudo-terminal it owns
(streaming, media keys and notifications off, so it never becomes a playback device). With it,
spotify_player commands take ~20 ms instead of ~1.5 s. `spotify daemon status` shows it as
`warm spotify_player: running`.
