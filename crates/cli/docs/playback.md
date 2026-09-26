# Playback control

How `play`, `pause`, `seek` and the other controls reach Spotify.app, how every result is
verified, and what happens when the first path fails.

## The rule: spotify_player first, verified, AppleScript fallback

spotify-cli drives Spotify through two tools:

- **spotify_player** — a Spotify Web API client. It can do everything the Web API can: start
  albums/playlists/radios/Liked Songs, repeat-one, likes, search, lyrics, playlists, devices.
- **AppleScript** — Spotify.app's own scripting interface on this Mac: play/pause, next/previous,
  play any URI (tracks, episodes, playlists, albums, shows, Liked Songs), seek to an absolute
  position, volume, shuffle, context repeat, and complete player state in ~50 ms.

For every playback command (`strategy: auto`, the default), except the ones listed under
[AppleScript first](#applescript-first):

1. Run the spotify_player command.
2. **Verify** the effect in Spotify.app (AppleScript reads its state every 120 ms). spotify_player
   hands commands to its running instance asynchronously and exits 0 before anything happened,
   and some commands report success while doing nothing, so exit 0 is never trusted.
3. If it errored, was refused, or did not take effect in time, run the AppleScript equivalent
   and verify again.
4. Report which path worked:

```json
{"action": "play", "via": "applescript",
 "fallback": {"from": "spotify_player", "reason": {"code": "no_effect", "message": "spotify_player accepted `play` but Spotify.app did not change within 1500 ms.", …}},
 "playback": {…}}
```

If both fail you get one path's error (usually AppleScript's `verification_failed`, retryable)
with the other attempt in `details` (`first_attempt`, or `second_attempt` when spotify_player
was tried second and could not run at all).

How long spotify_player gets before the fallback:

| Commands | Wait |
| --- | --- |
| `play` (resume), `pause`, `toggle`, `volume`, `shuffle` | at most 1.5 s (less when `verify_timeout_ms` is smaller): they set a state, so a late effect only repeats the fallback's |
| `next`, `previous`, starts, `seek` | `verify_timeout_ms` (default 2500 ms), then one more look 0.4 s later, so a late skip or seek is never applied twice |
| AppleScript itself | up to 2.5 s |

Other strategies (`spotify config set '{"strategy": "…"}'`):

| Strategy | Use when |
| --- | --- |
| `auto` | default |
| `applescript` | no Premium, no spotify_player sign-in, or you want the fastest local control |
| `spotify_player` | you control another Spotify Connect device and do not want AppleScript. It cannot start episodes or shows, or a track with `--context` (`unsupported`), and its seeks are approximate |

## AppleScript first

Some actions go to AppleScript first under `auto`, because spotify_player's way of doing them is
worse on the desktop app. They normally report `via: applescript` with no `fallback`.

- **`seek`**: AppleScript sets the position itself, exactly. spotify_player can only seek by an
  offset, which it adds to a position it fetches from the Web API when the command runs (seconds
  later when the Web API is rate limiting), so it lands wherever that reading was. It is the
  fallback under `auto`, and the only path under strategy `spotify_player`: a relative,
  approximate seek, refused with `state_mismatch` when spotify_player is on another item.
- **`play <track>`, `play <episode>`, `play <show>`** (and a track with `--context`): AppleScript
  only. The Web API starts a single track as a list of ids, which leaves Spotify.app stopped with
  nothing loaded, so spotify_player is never the fallback. It starts a track (without `--context`)
  only under strategy `spotify_player`; it cannot start episodes or shows at all.
- **`play --liked`**: AppleScript plays the Liked Songs list itself
  (`spotify:user:<id>:collection`, with the Spotify user id spotify_player is signed in as, read
  from its `credentials.json`), so `next` and `previous` stay in it. spotify_player's
  `playback start liked` (up to `--limit` tracks, default 200, as a list of ids that also empties
  the desktop app) runs only when that user id is unknown or under strategy `spotify_player`.
  `--random` turns on shuffle for Liked Songs and skips to a random song; Spotify remembers
  shuffle per list, so it stays on for Liked Songs afterwards. Without `--random`, Liked Songs
  keeps its own shuffle setting.

Albums, playlists, artists, radios and resume follow the rule: spotify_player first.

## Checking spotify_player's view first

The running spotify_player instance decides *what* to send from its own memory of the player:
whether it is playing, the repeat mode, shuffle, the current track. Its `play` does nothing while
it believes playback runs, `shuffle` flips what it believes, `like` saves what it believes is
playing. That memory can lag behind Spotify.app, so before such a command the controller reads it
(`get key playback`, answered in milliseconds) and compares:

| Command | spotify_player runs only when |
| --- | --- |
| `play` (resume) | it believes playback is paused |
| `pause` | it believes playback runs |
| `toggle` | as `play` or `pause`: toggle sends an explicit pause or play chosen from Spotify.app's state, never `play-pause` |
| `shuffle` | its shuffle is Spotify.app's |
| `next`, `previous` | it has playback loaded (`next` also: the Web API allows skipping from this item, else `not_allowed_in_context` and AppleScript skips) |
| `seek` | it is on the item Spotify.app plays |
| `repeat` | always; it steps from the mode it believes and each step is confirmed ([Repeat](#repeat)) |
| `like`, `unlike` | its track is the song Spotify.app plays ([Like and unlike](#like-and-unlike)) |

When they disagree under `auto`, AppleScript makes the change at once and `fallback.reason.code`
is `state_mismatch` (`details.spotify_player` and `details.spotify_app` say what each believes).
Under strategy `spotify_player`, `state_mismatch` is the error (retryable: retry after the next
track change, or run `spotify daemon restart`). The daemon's spotify_player refreshes its view
every 3 s (`spotify docs daemon`), so this is rare.

## Refusals are noticed at once

spotify_player exits 0 as soon as its running instance has the command; a Web API refusal (429,
403) then shows up only in that instance's log (`spotify-player-<date>.log` in its cache folder).
The controller reads the lines logged after its own request, so a refusal ends the wait right
away instead of after the verify timeout. The fallback reason is then:

| Code | When |
| --- | --- |
| `rate_limited` | 429 Too Many Requests |
| `no_active_device` | spotify_player has no playback to send it to |
| `no_effect` | any other refusal |

with `details.request` (the request, e.g. `Next`) and `details.spotify_player` (the logged cause).
Only a failure logged right after this command's own request counts, never another command's. A
start whose second call (shuffle) was refused gets 1 s to show before anything else is tried. If
spotify_player's `log_folder` is set outside its cache folder, refusals are not seen and commands
wait for the timeout as before.

## What each command does

| Command | First | Then | Verified by |
| --- | --- | --- | --- |
| `play` (resume) | `playback play` | `play` | state is playing |
| `play <track>` / `<episode>` | AppleScript `play track <uri>` | — | that item playing (the item already loaded: restarted) |
| `play <track> --context <list>` | AppleScript `play track <uri> in context <list>` | — | that track playing |
| `play <album/playlist/artist>` | `playback start context` (`--shuffle`) | `play track <uri>` | a new item playing |
| `play <show>` | AppleScript `play track <uri>` | — | a new item playing |
| `play --liked` | AppleScript: the Liked Songs list | — | a new item playing |
| `play --radio` | `playback start radio` | none | a new item playing |
| `pause` | `playback pause` | `pause` | state is not playing |
| `toggle` | `playback pause` or `playback play` | `pause` or `play` | state flipped |
| `next` | `playback next` | `next track` | item changed (or restarted by repeat-one) |
| `previous` | `playback previous`, or a restart ([Previous](#previous)) | `previous track` | previous item, or back at the start |
| `seek` | AppleScript `set player position` (absolute) | `playback seek <offset>` (relative) | position within 1.5 s |
| `volume` | `playback volume` | `set sound volume` | the exact level ([Volume](#volume)) |
| `shuffle` | `playback shuffle` (flips) | `set shuffling` | shuffling matches |
| `repeat off/context` | `playback repeat` (steps) | `set repeating` | Spotify.app's repeat flag |
| `repeat track` | `playback repeat` (steps) | none (AppleScript cannot) | Spotify.app's repeat flag |
| `like` / `unlike` | `like [--unlike]` | none | its track before and after |

Notes:
- `next` plays the **managed queue** first when it has items (`spotify docs queue`).
- Shuffle and repeat are refused by Spotify for singles and some contexts. `spotify status --json`
  shows `shuffle_allowed` / `repeat_allowed`; commands fail fast with `not_allowed_in_context`.

### Previous

From 3 s into the item, or when there is no item before it, `previous` restarts the current item
(a seek to 0:00, the same way as `seek`) on every path, as Spotify.app's own button does; the Web
API's previous would always go back an item. Otherwise it goes to the previous item. The outcome
says which: `result` is `restarted` or `previous_item`, and the human output starts with
"Back to the start of this item." or "Back to the previous item.".

### Repeat

Spotify.app shows one repeat flag for both `context` and `track`, and AppleScript can only switch
context repeat: `set repeating false` does not clear a repeat-one set through the Web API. So:

- spotify_player's `playback repeat` moves through off → track → context → off from the mode it
  believes. Each step is sent after the previous one shows in its view (up to 2 s each), and the
  end result is checked in Spotify.app.
- Asking for the mode it already believes: when Spotify.app agrees, a fresh Web API read (up to
  2.5 s) confirms it and nothing is sent. A `repeat off` that cannot be confirmed (rate limited)
  sends nothing either, since going round would pass through repeat-one. Otherwise it goes once
  around the cycle, which ends on the requested mode whatever Spotify had.
- When spotify_player fails, `repeat track` returns its error when that is retryable (for
  example `rate_limited`), else `unsupported`. `repeat off` and `repeat context` fall back to
  AppleScript, except when spotify_player left repeat-one on, which AppleScript cannot turn off:
  then its error, retryable.

### Volume

Spotify.app keeps the volume at 16-bit precision and reads most levels back one lower, so the
AppleScript fallback sets one more when needed. It lands exactly on the requested level, except
19, 39, 59, 79 and 99, which land one above. Verification is exact (one above for those five).

### Like and unlike

spotify_player's `like` has no id option: it saves or removes the track its instance believes is
playing. So `like` and `unlike`:

- refuse podcast episodes, ads and local files with `unsupported`;
- when spotify_player's track is not the song Spotify.app plays, send one inaudible
  `playback volume <current>` so it re-reads the Web API, and wait up to `verify_timeout_ms` +
  1.1 s for it to catch up;
- refuse with `track_mismatch` (retryable, nothing changed; `details`: `spotify_app`,
  `spotify_player`, `catch_up_failed`) when it does not;
- fail with `track_mismatch` (not retryable) when spotify_player's track changed while the
  command ran, because the like may have hit another song: check `spotify library liked`.

## When a start fails

When `play <something>` fails and leaves Spotify.app with nothing loaded although something was
loaded before, the controller puts that back through AppleScript, under any strategy: the same
track, in its playlist or album when spotify_player knew it, at the same position, paused if it
was paused. The error's `details.restored` holds the playback afterwards, or
`details.restore_failed` says why it could not.

## Status and the Web API

`spotify status` reads Spotify.app only (~50 ms). `spotify status --full` adds what only the Web
API knows, under `web`: `context_uri`, `context_type`, `device`, `repeat_state` (tells repeat-one
apart), `shuffle_state`, plus `item_uri`, `is_playing`, `source` and `stale` (present only when
true).

- `source: spotify_player`: the running instance's memory, used when it agrees with Spotify.app
  (same item, repeat and shuffle; a play state that disagrees is left out).
- `source: web_api`: otherwise a one-shot Web API read (a separate spotify_player sign-in plus a
  request, 1–4 s). An agent polling `status --full` while the memory is out of date adds that
  Web API load each time.
- `stale: true`: even that read is for another item (or failed, and the memory is shown).
  Repeat and shuffle that contradict Spotify.app are left out, and the warning `web_state_stale`
  says why.

Other warnings: `no_active_device` (the Web API sees no playback), `timeout`, `rate_limited` and
spotify_player's own errors. Warnings never fail the command; the human output prints them as
`note:` lines. When `web.stale` is true the human output takes repeat from Spotify.app's own flag
(never "repeat one" from out-of-date data) and ends the flags line with "web data out of date".
Time left, `(-m:ss)`, is rounded to the nearest second (19.6 s left shows 0:20).

## Spotify.app not running

Reading state never launches Spotify. Control commands launch it hidden (`open -g -j -a Spotify`)
when `launch_spotify` is true (default); set it false to get `spotify_not_running` instead.
`pause` never launches it (`spotify_not_running`), and `like`/`unlike` need a song playing
(`nothing_playing`).
`spotify launch` starts it explicitly: "Started Spotify.app (hidden)." (`launched: true`), or
"Spotify.app was already running." (`launched: false`, `already_running: true`, nothing opened).

## Speed

The daemon keeps one headless spotify_player instance running on a pseudo-terminal it owns
(streaming, media keys and notifications off, so it never becomes a playback device). With it,
spotify_player commands take ~20 ms instead of ~1.5 s, and its view of playback is refreshed every
3 s. `spotify daemon status` shows it as `warm spotify_player: running (…)`; details in
`spotify docs daemon`.

## Known limits

- If repeat-one was turned on in Spotify.app while spotify_player believes `context`, a
  rate-limited `repeat off` can end in an AppleScript success while repeat-one stays on. Check
  `spotify status --full` and retry in a minute.
- `status --full` from the instance's memory does not notice a new context when the same item
  plays on (the same song replayed from another list).
- If Spotify.app and the Web API report different ids for one song (relinked tracks), `like` and
  `unlike` refuse with `track_mismatch` every time; they never change the wrong song.
