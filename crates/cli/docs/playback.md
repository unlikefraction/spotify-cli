# Playback control

How `play`, `pause`, `seek` and the other controls reach Spotify.app, how every result is
verified, and what happens when the first path fails.

## The rule: the Web API first, verified, AppleScript fallback

spotify-cli drives Spotify through three paths:

- **The Spotify Web API, directly** — to start a song, an episode, a show or Liked Songs
  ([Web API first](#web-api-first-songs-episodes-shows-and-liked-songs)), with spotify_player's
  cached token. It tells Spotify.app on this Mac, or the speaker or phone Spotify.app controls,
  what to play, so Spotify.app stays in the background.
- **spotify_player** — a Spotify Web API client. It starts albums, playlists, artists and
  radios, runs the other controls (resume, pause, next, previous, volume, shuffle, repeat, likes)
  and serves search, lyrics, playlists and devices.
- **AppleScript** — Spotify.app's own scripting interface on this Mac: play/pause, next/previous,
  play any URI (tracks, episodes, playlists, albums, shows, Liked Songs), seek to an absolute
  position, volume, shuffle, context repeat, and complete player state in ~50 ms. Reading state
  leaves Spotify.app where it is; starting something with it brings Spotify.app to the front
  ([Focus](#keeping-spotify-app-in-the-background)).

So every start goes through the Spotify Web API first: songs, episodes, shows and Liked Songs
directly, albums, playlists, artists and radios through spotify_player. Starts fall back to
AppleScript, except a radio, which has no AppleScript way. Only exact seeks go to AppleScript
first ([AppleScript first](#applescript-first)).

For album, playlist, artist and radio starts and every other playback command but `seek`
(`strategy: auto`, the default):

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

How long the first path gets before the fallback:

| Commands | Wait |
| --- | --- |
| `play` (resume), `pause`, `toggle`, `volume`, `shuffle` | at most 1.5 s (less when `verify_timeout_ms` is smaller): they set a state, so a late effect only repeats the fallback's |
| `next`, `previous`, starts, `seek` | `verify_timeout_ms` (default 2500 ms), then one more look 0.4 s later, so a late skip or seek is never applied twice |
| A Web API start (song, episode, show, Liked Songs) | the same; a rate limit, missing Premium, a device that takes no Web API commands or no device to start on falls back at once, and a start whose answer was lost is first looked for in Spotify.app ([Lost answers](#when-the-answer-is-lost)) |
| AppleScript itself | up to 2.5 s |

Other strategies (`spotify config set '{"strategy": "…"}'`):

| Strategy | Use when |
| --- | --- |
| `auto` | default |
| `applescript` | no Premium, no spotify_player sign-in, or you want only local control. The Web API is never used: every start goes through AppleScript (Spotify.app comes forward and the focus goes back), and a radio cannot start |
| `spotify_player` | you do not want AppleScript at all. Songs, episodes, shows and Liked Songs start through the Web API; when that fails only a song without `--context` and Liked Songs have a second way (spotify_player's own start, which loads a bare list of ids), else the error is `unsupported` with the Web API's attempt in `details.first_attempt`. Its seeks are approximate |

## Web API first: songs, episodes, shows and Liked Songs

`spotify play <track>` and `spotify play <episode>` (also `play --search`, `podcast play` and
`search --play` when they pick a song or episode, and a song with `--context`),
`spotify play <show>` and `spotify play --liked` start through the Spotify Web API directly. Not
through spotify_player, which starts a song or Liked Songs as a bare list of ids that leaves
Spotify.app stopped with nothing loaded, and cannot start a show; and not through AppleScript,
whose `play track` brings Spotify.app to the front and takes the focus from the app you work in.
The Web API tells Spotify.app, as a Spotify Connect device, what to play, and it plays where it
is.

### A song or an episode

1. **The item.** `GET /v1/tracks/{id}` (or `/v1/episodes/{id}`) with `market=from_token` names
   the song's album or the episode's show, and, for a song that plays from another release in
   your market ([relinked](#relinked-songs)), that release. A song or episode Spotify lists as
   not playable in your market, with no release that plays there, fails at once
   ([Not playable](#not-playable-in-your-market)): nothing is sent and playback is not touched.
2. **The list.** `--context` when you give one, else the song's album or the episode's show. So
   Spotify.app has a whole list loaded: after the song the album plays on (the show for an
   episode), and `next` and `previous` move through it.
3. **The device.** Listed at every start ([Where a start plays](#where-a-start-plays)): the
   speaker or phone Spotify.app controls when one is active, else Spotify.app on this Mac.
4. **The start.** `PUT /v1/me/player/play?device_id=…` with
   `{"context_uri": <list>, "offset": {"uri": <item>}, "position_ms": 0}`: the item, from its
   beginning, named as the list holds it. In its own album a relinked song is named by the id it
   links from (the album holds it under that id), so one request starts it
   ([Relinked songs](#relinked-songs)).
5. **The check.** AppleScript reads Spotify.app every 120 ms (reads never bring it forward) until
   it plays the item: the id asked for, the release it plays from, or the same song relinked.
   Starting the song already loaded counts only once it went back to its start. It waits up to
   `verify_timeout_ms` (default 2500 ms), then looks once more 0.4 s later. When another track of
   the album starts instead, or Spotify refuses the item's uri in its album (400/404), the start
   is sent once more naming the track by its position in the album (counted across discs): the
   fallback when naming it by uri did not place it. A `--context` list is never retried by
   position.

```json
{"action": "play", "via": "web_api", "playback": {…}}
```

Lookups are remembered for the life of the daemon (up to 500 items, not the ones that were not
playable). The devices are not: they are listed at every start, because the active one changes
whenever you pick a speaker in Spotify.app.

### Not playable in your market

When the lookup says Spotify cannot play the song (or episode) in your country or market
(`is_playable: false`) and names no other release that plays there, the start fails at once with
`not_playable` (exit 1, not retryable):

```json
{"code": "not_playable",
 "message": "'Fall (Acoustic Version)' by Ana Rey, The Tides (spotify:track:…) is not playable in your country/market: Spotify has no release of it that plays here, so nothing was started.",
 "hint": "Find another version with `spotify search 'Fall (Acoustic Version) Ana Rey' --type track`, then `spotify play <uri>`.",
 "details": {"uri": "spotify:track:…", "name": "Fall (Acoustic Version)", "market": "from_token", "reason": "market"}}
```

`details.reason` is the lookup's `restrictions.reason` when it gives one. `market` is the case
above. Two reasons are about the account, not the country, and the message says so instead:
`explicit` ("… is explicit, and this Spotify account is set not to play explicit content"; the
hint also points to Spotify's explicit-content setting and searches for a clean version) and
`product` ("… is not available on this account's Spotify plan").

No start is sent and nothing falls back: AppleScript's `play track` of such a song leaves
Spotify.app with nothing loaded for a few seconds until what played before is put back. What
played before keeps playing, untouched; the answer comes in about a second (one lookup of
0.3–0.6 s, and Spotify.app reads). A song that is relinked to a release that plays in your
market is playable and starts as usual. Under strategy `applescript` there is no lookup, so
AppleScript tries it.

### A show

`spotify play spotify:show:<id>` sends `{"context_uri": "spotify:show:<id>"}` alone, so Spotify
starts the show where it starts it, on the device chosen as for a song. It counts once
Spotify.app plays something new (an episode, or an ad). spotify_player cannot start a show, so
when this fails AppleScript plays it (`play track <show uri>`).

### Liked Songs

`spotify play --liked` plays the Liked Songs list itself (`spotify:user:<id>:collection`, with
the Spotify user id spotify_player is signed in as, read from its `credentials.json`), so `next`
and `previous` stay in it. The result says `via: web_api`, action `play_liked`, and Spotify.app
stays in the background.

1. `GET /v1/me/tracks?limit=1` gives how many songs Liked Songs holds and its first song (the one
   liked last). An empty Liked Songs is `not_found` at once and nothing starts. An answer that
   does not say how many is `web_api_failed` (retryable), and AppleScript starts the list.
2. The start: at position 0, or, with `--random` (or `--shuffle`, which is the same with
   `--liked`), at a random position below that count. It counts once Spotify.app plays
   something new.
3. Spotify keeps a shuffle setting per list and switches to Liked Songs' own when the list
   starts, so shuffle is set after the start, never before (that would only change the list
   playing before). That switch comes a moment after the first song shows: in one trace the
   first song showed with the shuffle of the list before (off), and Spotify.app turned shuffle on
   75 ms later. So the controller first lets shuffle settle: it reads Spotify.app every 120 ms
   until neither shuffle nor the song changed for 0.45 s (at most 1.5 s), which adds about half
   a second to the start.
4. When the settled shuffle differs from what was asked (off without `--random`, on with it),
   `PUT /v1/me/player/shuffle` sets it, and AppleScript's `set shuffling` does when that does
   not show in Spotify.app within 2.5 s.
5. Without `--random`, when the list plays another song than its first (it started shuffled),
   it is started once more at position 0, so it plays in list order from the first song. When
   the first song already plays (shuffle came on only after it showed), turning shuffle off is
   enough.
6. Whenever step 4 or 5 changed something, shuffle is let settle again and the result checked:
   shuffle as asked and, without `--random`, the first song. When Spotify switched it back, steps
   4 and 5 run once more. The result is what Spotify.app keeps, e.g. `shuffling: false` on the
   first song for a plain `play --liked`.

Once Liked Songs plays, a step that does not show is `verification_failed` (retryable) saying
which one, and so is a shuffle that Spotify switched back twice ("Spotify.app's shuffle did not
stay off"); Liked Songs keeps playing and is not started a second time.

While Liked Songs plays, the Web API reports it as a playlist: `GET /v1/me/player` (and so
`spotify status --full`'s `web`) says `"context_type": "playlist"` and
`"context_uri": "spotify:playlist:37i9dQZF1F…"`, an id Spotify makes for your account
(`GET /v1/playlists/<id>` names it "Liked Songs"), not `spotify:user:<id>:collection`, the uri
that starts it. Both name the same list; to start Liked Songs use `spotify play --liked`.

When the Web API start fails, AppleScript plays the list, the focus goes back to your app, and
the same result is reached its way once the start shows:

- Shuffle is let settle first, as above (Spotify.app switches to Liked Songs' kept shuffle a
  moment after the start shows).
- Without `--random` it plays in list order from its first song. A kept shuffle, also one set in
  Spotify.app, is turned off (it stays off for Liked Songs) and the list is started again, so
  the shuffled start plays for a moment first.
- `--random` switches shuffle off and on, so Spotify draws a new order, then skips once, so every
  start is a random song. Shuffle stays on for Liked Songs until a plain `play --liked` turns it
  off.
- Each step is checked in Spotify.app, and after them shuffle is let settle once more and
  checked. When the start does not show within 2.5 s the error is `verification_failed`
  (retryable) at once, and no shuffle step is sent. When a shuffle change, the restart in order
  or the skip does not show, or shuffle did not stay as asked, the error is
  `verification_failed` (retryable) saying which; Liked Songs keeps playing.

spotify_player's `playback start liked` (up to `--limit` tracks, default 200, as a list of ids
that also empties the desktop app) runs only when the Spotify user id is unknown or under
strategy `spotify_player`.

### Where a start plays

Every Web API start lists `GET /v1/me/player/devices` first and picks:

- **Another device is active** (a speaker, a phone or a TV that Spotify.app on this Mac is the
  remote for; never spotify_player's): the start goes there, as Spotify.app's own play button
  would, so playback stays on that device. Spotify.app shows what plays there, so the check reads
  Spotify.app all the same. The outcome carries a `note`: "Started on <device>, the device
  Spotify.app on this Mac is playing on, so playback stays there." A device listed without an id
  gets no `device_id`: Spotify plays on the active one.
- **That device takes no Web API commands** (the list marks it restricted): the reason is
  `device_restricted`, and AppleScript starts the item through Spotify.app, which controls it.
- **Spotify.app plays, but nothing listed is active**: it plays on a device Spotify leaves out of
  that list (Spotify says some device models are never listed). The start names no device, so
  Spotify plays it on the active one there too, instead of moving playback to this Mac.
- **Nothing is active**: Spotify.app on this Mac, by device id: the one Computer device named
  like this Mac (`scutil --get ComputerName`); only when that name is unknown, the only Computer
  device. Never spotify_player's own device, and never another Mac or the Web Player that
  happens to be the only computer listed. This works when no device is active; with no such
  device the reason is `device_not_found`.

When Spotify answers that the device is not found or that nothing is active (`no_active_device`:
Spotify.app restarted, or the unlisted device went away), the devices are listed once more and
the start goes to this Mac, unless another device is active by then.

### When the answer is lost

A start answered with a server error (5xx), or not answered in time (a timeout or a broken
connection; a connection that never opened sent nothing), may still have gone through. So the
controller first looks for the item in Spotify.app, as for any start (the verify timeout, then
one more look). When it plays, the start counts: `via: web_api`, with a `note` such as "The
Spotify Web API gave HTTP 502 to the start, but Spotify.app plays it: the start went through, so
it was not started again." Only when it does not play does the fallback start it, once. This
covers songs, episodes, the retry by position, shows and Liked Songs (whose shuffle steps then
follow as usual).

### Fallback

When the start could not be sent or did not take effect, AppleScript plays it and verifies that.
A song plays in the album the Web API's lookup named, named as that album holds it (`play track
<uri> in context <album>`; a relinked song by the id it links from), so the album goes on after
it, and Spotify.app showing either id counts. A song that is [not playable](#not-playable-in-your-market)
never gets here. `--context` wins when given. The song plays alone when there was no
lookup (the Web API unavailable) or when the album did not place it: Spotify refused it there by
uri and by position, or another track of the album kept playing (`no_effect` with
`details.landed_elsewhere: true`). An episode plays alone, a show and Liked Songs as themselves.

The outcome says `fallback: {"from": "web_api", "reason": …}`, and if AppleScript fails too, its
error carries the Web API's attempt in `details.first_attempt`. `fallback.reason.code`:

| Code | When |
| --- | --- |
| `rate_limited` | Spotify answered 429, or the pause after an earlier 429 still runs: nothing is sent until its `Retry-After` has passed (1 s to 10 minutes, 30 s when it gives none). The daemon's own Web API lookups and searches share that one pause. Falls back at once |
| `premium_required` | the account is not Premium: Spotify's Web API playback needs it. Falls back at once |
| `device_restricted` | the active device takes no Web API commands (`details.device`: its name and type); AppleScript starts the item through Spotify.app, which controls that device |
| `device_not_found` | nothing else is active and no device in the list is clearly Spotify.app on this Mac (just launched, not yet registered, or offline); `details.devices` lists each device's name and type |
| `no_active_device` | Spotify answered that the device is not found or nothing is active, also after the devices were listed again |
| `spotify_auth_required`, `spotify_player_missing` | the token is spotify_player's: it is not signed in, or not installed |
| `web_api_failed` | any other HTTP status (`details`: `request`, `status`, `message`, `reason`, and `unanswered: true` for a 5xx that Spotify.app did not show), no album or show named for the item, a Liked Songs answer without its size, or spotify_player's cached token has expired (retryable) |
| `not_found`, `transport` | Spotify does not know the item; the network (`details`: `connect`, `timeout`, and `unanswered: true` when the start may have left) |
| `no_effect` | Spotify accepted the start but Spotify.app did not play the item in time (`details`: `context`, `device`, `offset` or `position`, `observed`, and for a song `landed_elsewhere`) |

Under strategy `applescript` the Web API is never used; under strategy `spotify_player` there is
no AppleScript fallback (see the strategies above).

Notes:
- **Premium.** Web API playback needs Premium. Without it every song, episode, show or Liked
  Songs start spends one refused request (about 0.3 s) before AppleScript plays it; strategy
  `applescript` skips that: `spotify config set '{"strategy": "applescript"}'`.
- **Another device.** A start plays on the speaker or phone Spotify.app controls
  ([above](#where-a-start-plays)), as AppleScript's `play track` does in the remote session. Move
  playback with `spotify devices connect <id>`.
- **Relinked songs** start in one request, named in their album by the id the album holds;
  Spotify plays the release that plays in your market, Spotify.app shows the id you asked for,
  and the start counts ([Relinked songs](#relinked-songs)).
- **Not playable** songs and episodes fail at once with `not_playable`, before anything is sent
  ([above](#not-playable-in-your-market)).

## Keeping Spotify.app in the background

AppleScript's starts bring Spotify.app to the front, which steals the focus from the app you work
in. So whenever AppleScript starts something (the fallback of a song, episode, show or Liked
Songs start, an album or playlist that spotify_player could not start, the restore after a
failed start, and the daemon's managed-queue hand-offs and resumes), spotify-cli notes which app
is in front, watches the front while the start runs and for 1 s after it, and as soon as
Spotify.app takes it gives the focus back to that app (at most 3 times per start). If Spotify.app
was hidden before, it is hidden again. In practice Spotify.app is in front for about half a
second.

- Giving the focus back: `lsappinfo setfront`, and when macOS ignores that (it usually does from a
  background process), `open -a <that app>`. `open -a` is used only for a regular app (one with a
  Dock icon) that still runs and is not Finder, so a launcher or the login window is never
  reopened and a quit app is never relaunched. Once macOS has ignored `setfront`, later hand-backs
  go straight to `open -a`.
- Hiding Spotify.app again: `NSRunningApplication`'s `hide`, run by `osascript -l JavaScript`.
- None of it needs a new macOS permission: these are LaunchServices and AppKit calls, not Apple
  Events.

The outcome says what happened in `refocused`, e.g. `{"app": "iTerm2", "via": "open"}` (`via`
is `setfront` or `open`), plus `hid_spotify: true` when Spotify.app was hidden again and `error`
(code `focus_not_returned`) when macOS ignored both ways and Spotify.app stays in front. On a failed
command it is `details.refocused`. It is never a command error. The daemon logs its queue's
hand-backs.

Turn it off with `spotify config set '{"keep_spotify_in_background": false}'` (default true).

## AppleScript first

Only exact seeks go to AppleScript first under `auto`, because spotify_player's way of doing them
is worse on the desktop app. They report `via: applescript` with no `fallback`.

- **`seek`**: AppleScript sets the position itself, exactly. spotify_player can only seek by an
  offset, which it adds to a position it fetches from the Web API when the command runs (seconds
  later when the Web API is rate limiting), so it lands wherever that reading was. It is the
  fallback under `auto`, and the only path under strategy `spotify_player`: a relative,
  approximate seek, refused with `state_mismatch` when spotify_player is on another item.

Every start goes to the Web API first: songs, episodes, shows and Liked Songs
[directly](#web-api-first-songs-episodes-shows-and-liked-songs), albums, playlists, artists and
radios through spotify_player (the rule). spotify_player starts a single song as a list of ids,
which leaves Spotify.app stopped with nothing loaded, so it is never the fallback of a song
start; it starts a song without `--context` only under strategy `spotify_player`, after the Web
API. When any start falls back to AppleScript, the focus goes back to your app.

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
| `like`, `unlike` | its track is the song Spotify.app plays, by id ([Like and unlike](#like-and-unlike)) |

A song Spotify plays under another id than Spotify.app shows counts as the same item, except for
`like` and `unlike` ([Relinked songs](#relinked-songs)).

When they disagree under `auto`, AppleScript makes the change at once and `fallback.reason.code`
is `state_mismatch` (`details.spotify_player` and `details.spotify_app` say what each believes).
Under strategy `spotify_player`, `state_mismatch` is the error (retryable: retry after the next
track change, or run `spotify daemon restart`). The daemon's spotify_player re-reads playback
every 20 s (by default) and right after its own commands (`spotify docs daemon`), so after a change made in
Spotify.app its view can be up to about 20 s behind; under `auto` that only means AppleScript
makes the change.

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
| `play <track>` | Web API `PUT /me/player/play` in its album | `play track <uri> in context <album>`, named as the album holds it (the track alone when the album did not place it; none when it is `not_playable`) | that track playing, relinked included (the track already loaded: restarted) |
| `play <episode>` | Web API `PUT /me/player/play` in its show | `play track <uri>` | that episode playing (already loaded: restarted) |
| `play <track> --context <list>` | Web API `PUT /me/player/play` in `<list>` | `play track <uri> in context <list>` | that track playing, relinked included |
| `play <album/playlist/artist>` | `playback start context` (`--shuffle`) | `play track <uri>` | a new item playing |
| `play <show>` | Web API `PUT /me/player/play` with the show as the list | `play track <uri>` | a new item playing |
| `play --liked` | Web API `PUT /me/player/play` in Liked Songs (its first song, or a random one), then its shuffle | AppleScript: the Liked Songs list, then its shuffle steps | a new item playing, then each step |
| `play --radio` | `playback start radio` | none (AppleScript has no radio) | a new item playing |
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
- refuse at once, without the nudge, when spotify_player plays the same song under another id
  (a [relinked song](#relinked-songs)): `track_mismatch`, not retryable, nothing changed, with
  `details.relinked: true`;
- fail with `track_mismatch` (not retryable, no `relinked`) when spotify_player's track changed
  while the command ran, because the like may have hit another song: check
  `spotify library liked`.

## When a start fails

When `play <something>` fails and leaves Spotify.app with nothing loaded although something was
loaded before, the controller puts that back through AppleScript, under any strategy: the same
track, in its playlist or album when spotify_player knew it, at the same position, paused if it
was paused (and the focus goes back to your app). The error's `details.restored` holds the
playback afterwards, or `details.restore_failed` says why it could not.

## Status and the Web API

`spotify status` reads Spotify.app only (~50 ms). `spotify status --full` adds what only the Web
API knows, under `web`: `context_uri`, `context_type`, `device`, `repeat_state` (tells repeat-one
apart), `shuffle_state`, plus `item_uri`, `is_playing`, `source`, and `stale` and `relinked`
(each present only when true).

- `source: spotify_player`: the running instance's memory, used when it agrees with Spotify.app
  (same item, relinked included, repeat and shuffle; a play state that disagrees is left out).
- `source: web_api`: otherwise a one-shot Web API read (a separate spotify_player sign-in plus a
  request, 1–4 s). An agent polling `status --full` while the memory is out of date adds that
  Web API load each time.
- `stale: true`: even that read is for another item (or failed, and the memory is shown).
  Repeat and shuffle that contradict Spotify.app are left out, and the warning `web_state_stale`
  says why (retryable: the Web API usually catches up within seconds of a change in Spotify.app;
  if it keeps reporting another item, `web` describes that item, not the one Spotify.app plays).
- `relinked: true` (present only when true): the Web API plays Spotify.app's song under another
  id, so `item_uri` differs from `track.uri`. That is current, not stale: no `web_state_stale`,
  and the memory can serve it (`source: spotify_player`) without a Web API read each time
  ([Relinked songs](#relinked-songs)).

Other warnings: `no_active_device` (the Web API sees no playback), `timeout`, `rate_limited` and
spotify_player's own errors. Warnings never fail the command; the human output prints them as
`note:` lines. When `web.stale` is true the human output takes repeat from Spotify.app's own flag
(never "repeat one" from out-of-date data) and ends the flags line with "web data out of date".
Time left, `(-m:ss)`, is rounded to the nearest second (19.6 s left shows 0:20).

## Relinked songs

When the release a song was saved or started from cannot play in your market, Spotify plays the
same recording from another release (track relinking). Spotify.app then shows the id it was asked
for, with the substitute's title, album and length, while the Web API, and so spotify_player,
reports the substitute's id. spotify-cli counts the two as one song when the Web API item's
`linked_from` names Spotify.app's id, or when the title (ignoring case and surrounding spaces),
the length (within 1 s) and the album name agree. The same recording on a release with another
album name (a single and its album, a deluxe edition) is another item.

- `status --full` treats it as the current item: `web.relinked: true`, `web.item_uri` is the
  substitute's id, and it is not `stale`.
- A [Web API start](#web-api-first-songs-episodes-shows-and-liked-songs) looks the song up in
  your market. The answer is the release that plays there (its `id`, with `linked_from` naming
  the id you asked for) but the album of the release you asked for, which holds the song under
  the id you asked for. So the start names that id in that album
  (`"offset": {"uri": <the linked_from id>}`), and Spotify plays the substitute: one request.
  Named by the substitute's id there, Spotify started the album's first track (seen 2026-09),
  and only a second start by position reached the song; that retry stays as the fallback.
  Spotify.app shows the id you asked for (and the album's other songs under their ids in that
  album), and the start counts. When AppleScript has to start it instead, it plays it the same
  way (`play track <the id you asked for> in context <album>`; by the substitute's id it too
  starts the album's first track), with `--context` the id you gave, in that list, and
  Spotify.app showing either id counts.
- A song with no release that plays in your market is not relinked: it is
  [`not_playable`](#not-playable-in-your-market).
- The checks before spotify_player's commands count it as Spotify.app's song: its relative seek
  is not refused with `state_mismatch`, `next` and `previous` heed what the Web API allows for
  it, and a failed start can put it back in its playlist or album.
- `like` and `unlike` refuse it at once: spotify_player's `like` saves or removes the id it plays,
  which is the substitute's, not the one Spotify.app shows. The error is `track_mismatch`, not
  retryable, and nothing changed. `details`: `spotify_app`, `spotify_player`, `relinked: true`,
  `matched_by` (`linked_from` or `title_length_album`). Its hint points to the heart in
  Spotify.app; other songs are not affected.

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
20 s (slow enough to leave Spotify's rate limit to commands). `spotify daemon status` shows it as
`warm spotify_player: running (…)`; details in `spotify docs daemon`.

A Web API start of a song or episode takes about 2–3 s end to end. In one traced start of 1.8 s,
the item lookup took 0.35 s (with the TLS handshake), the device list 0.14 s and the play request
0.34 s, and Spotify.app showed the song about 0.3 s later. The daemon keeps one HTTPS connection
for these requests, and a later start of the same item skips the lookup; the device list is read
at every start, so a start follows the speaker you just picked.

## Known limits

- spotify_player's device is recognised by its default name, `spotify-player`. A spotify_player
  you run yourself with streaming on and another `[device] name` in its `app.toml` looks like a
  speaker Spotify.app controls, so a start while it is the active device plays there. The
  daemon's own copy never streams.
- AppleScript cannot read which list plays, so Liked Songs and show starts are checked by
  "something new plays". A start whose answer was lost and that did nothing still counts when
  the list playing before moves on to its next song within the wait (about 3 s), and for Liked
  Songs its shuffle step then changes that list's shuffle. Rare; run the command again.
- If repeat-one was turned on in Spotify.app while spotify_player believes `context`, a
  rate-limited `repeat off` can end in an AppleScript success while repeat-one stays on. Check
  `spotify status --full` and retry in a minute.
- `status --full` from the instance's memory does not notice a new context when the same item
  plays on (the same song replayed from another list).
- `like` and `unlike` cannot change a [relinked song](#relinked-songs): use the heart in
  Spotify.app. They never change the wrong song.
- Two releases of one recording with the same album name (a clean and an explicit edition) look
  relinked while spotify_player's view is still on the other one after a switch in Spotify.app
  (up to about 20 s): a `like` then gets the relinked refusal instead of waiting to catch up, and
  `status --full` may show the other one's context. `like` still never changes the wrong id.
- A local file has no Web API id, so `status --full` marks it `web.stale` every time.
