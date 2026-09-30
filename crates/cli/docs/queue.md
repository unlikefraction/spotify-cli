# The queue

Spotify's own queue can be appended to through the app, but neither the Web API nor AppleScript
can remove or reorder it, and spotify_player's CLI cannot even append. So spotify-cli keeps a
**managed queue** in the daemon and shows Spotify's upcoming list next to it, read-only.

```sh
spotify queue                                   # managed items, then Spotify's upcoming
spotify queue show                              # the same (also queue list, queue ls)
spotify queue add spotify:track:<id> spotify:episode:<id>
spotify queue add --search 'song name'          # first hit (--type episode for podcasts)
spotify queue add <uri> --next                  # put at the front
spotify queue move 3 1                          # position 3 → 1
spotify queue remove 2                          # by position, id (q_…) or uri
spotify queue clear
spotify next                                    # plays the managed head now
```

`spotify queue` with no subcommand lists the queue, as do `queue list` and its aliases
`queue show` and `queue ls`; `queue remove` is also `queue rm`, and `queue move` is `queue mv`.
Skipping is not a queue subcommand: `queue next` exits 2 with a hint that lists the
subcommands and adds "To skip to the next track: spotify next."

## How it plays

- When the current song is within 0.9 s of its end, the daemon starts the managed head through
  the same controller as `spotify play`: under `auto`, the Web API first, with AppleScript as
  fallback. Automatic starts, managed `next` and resumes all honor `strategy`. If Spotify moves
  on first (the queue was added too late, crossfade, a skip or stop in the Spotify app), the daemon
  switches to the managed head immediately. When an AppleScript start brings Spotify.app to the
  front, the focus goes back to the app that had it (`keep_spotify_in_background`,
  `spotify docs playback`); the daemon log says so.
- An item leaves the queue once the controller verifies that Spotify plays it or its relinked
  release, using `verify_timeout_ms` for each playback path. Failed hand-offs are retried; after
  three failures the item is dropped (logged in `spotify daemon logs`).
- Shortly before the first hand-off (and when you add to an idle queue) the daemon remembers what
  the queue will interrupt: the playing context (playlist/album) and the item up next in it. The
  up-next item comes from the Web API; the context comes from spotify_player's memory of playback,
  and when that memory names another item than the Web API (it can be up to about 20 s behind a
  change made in Spotify.app), from one fresh Web API read instead.
- When the managed queue drains and its last item ends (or Spotify stops), the daemon resumes
  that item in that context, so a playlist continues where you left it. Saved local files and
  older `spotify:user:…:collection` contexts need AppleScript (`auto` or `applescript`);
  `spotify_player` refuses those resume points. A failed resume keeps its saved point unless a
  newer one has been captured.
- Queue timing needs a known track length; right after a switch, while Spotify still reports 0,
  nothing is handed over.

## `next`

- With managed items, `spotify next` plays the head now; `--json` reports the actual `via`,
  playback and any fallback, note or focus hand-back. With an empty managed queue it is
  Spotify's own next track (never an error).
- Quick or concurrent `next`s are applied one after another, waiting for earlier playback
  verification to finish. If an unconfirmed hand-off remains (for example after a restart),
  `next` skips that item: it leaves the queue, counts as played, and the item after it plays.
  When nothing is left, Spotify's own next runs once the skipped item shows (up to 1.5 s), so it
  moves past that item. The reply then has
  `skipped` (the human output adds `skipped <uri> (it was still being switched to)`).
- Queueing the item that plays now and running `next` restarts it, and that counts as its turn.

## Your own commands win

- `spotify play <item>` (also `--liked` and `--radio`) and `spotify previous` wait for a `next` or
  a hand-off already in progress, and those wait for them, so changes reach Spotify one at a time
  and never interleave (commands sent at the same instant have no guaranteed order). `spotify play` with no target (resume) changes no item and is not affected.
- For 5 s after an explicit change (`play <item>`, `previous`, `next` with an empty queue) the
  daemon does not override item changes. A slow change (Spotify.app launching, a long
  verification) starts those 5 s again once it has landed.
- `spotify play <item>` discards the resume point once the item has started; the managed queue
  itself stays and plays after that item. A play that fails keeps the resume point, and when
  Spotify.app still plays the managed item afterwards (or `previous` restarted it), that item
  stays the managed one, so the queue still moves on or resumes after it.

## What `spotify queue` shows

- Managed items as `name — artists` (episodes: `name — show`) with their id (`q_…`), or
  `track <id>` / `episode <id>` when no name is known. `queue add` prints
  `Queued N item(s). Managed queue now has M.` and one `  + name — artists` line per item, and
  `queue remove` / `queue move` name the item the same way.
- Then up to 10 of Spotify's upcoming items, read-only, named the same way. Spotify lists the item
  playing now there again when nothing else follows it: with repeat-one, when it was played
  without a context, or at the end of its album or playlist with repeat off. Those leading repeats
  are left out, and `spotify_upcoming.current_repeats_left_out` and `spotify_upcoming.note` in
  `--json` say so. The human output prints the note.
- The note names the kind ("Spotify listed the track playing now 10 more time(s) as upcoming;
  those are left out.") and adds a cause, such as "(repeat-one is on)", "(the episode was played
  without a context)" or "(nothing else is up next in its album)", only when spotify_player's view
  of that same item shows it and Spotify.app's repeat flag does not contradict it. That view can be
  up to about 20 s behind, so otherwise the note names no cause.
- When Spotify reports nothing playing on any device (no current item and nothing upcoming, or
  spotify_player's `no_active_device`) while Spotify.app does not play either, Spotify's upcoming
  list is empty for that reason. The reply's top-level `warnings` then holds `no_active_device`
  ("Spotify has no active device, so its upcoming list is empty; play something in Spotify.app
  once", retryable). `warnings` is on every reply and empty otherwise; when Spotify.app plays
  (an ad, a private session) the list is just empty. The human output prints each warning as
  `note: <message> (<code>)`, with its hint on the next line. The managed queue is not affected.

Names, artists (the show, for episodes) and lengths are recorded when an item is queued: from the
search hit `queue add --search` picked (sent to the daemon as `known`), from spotify_player for a
track URI, and from the Web API for an episode URI (spotify_player cannot look up episodes; the
daemon uses spotify_player's cached token, spending at most 8 s per `queue add`). What the caller
knows is kept and the rest is looked up, field by field (an empty name or artist counts as
missing): an episode lacking any of its name, show or length is looked up, so one picked with
`--search`, whose hit names no show, gets its show.

An item without a name shows by kind and id. A track's facts are looked up once, when it is
queued. For an episode that still lacks any of them, the daemon keeps asking:

- After a rate limit (a 429), it asks again in the background once Spotify's `Retry-After` has
  passed, for up to 10 minutes; a rate limit's refusal does not count as a try. Other failures
  get 3 tries per item. One background lookup runs at a time, and what it finds is saved into the
  queued item.
- `spotify queue` also looks missing facts up itself, for at most 2.5 s, when no rate limit is
  being waited out and no background lookup runs, then leaves the rest to the background. That
  also covers items queued before a daemon restart.
- When `queue add` left such a lookup to the background, its reply (`--json`) carries
  `metadata_pending` (the ids, `q_…`, of those queued items) and a `note` saying the daemon asks
  again; the note mentions a rate limit only when one refused a lookup.

Only tracks and episodes can be queued: anything else is `invalid_input`
(`spotify:album:… is an album; the queue holds tracks and episodes.`), with a hint that fits the
kind, such as playing the whole album with `spotify play`. The queue belongs to the Mac, not to
one Silicon: every Silicon on this user account sees and edits the same queue, and each item
records who added it (`added_by`).
