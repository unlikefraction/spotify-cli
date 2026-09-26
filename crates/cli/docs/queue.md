# The queue

Spotify's own queue can be appended to through the app, but neither the Web API nor AppleScript
can remove or reorder it, and spotify_player's CLI cannot even append. So spotify-cli keeps a
**managed queue** in the daemon and shows Spotify's upcoming list next to it, read-only.

```sh
spotify queue                                   # managed items, then Spotify's upcoming
spotify queue add spotify:track:<id> spotify:episode:<id>
spotify queue add --search 'song name'          # first hit (--type episode for podcasts)
spotify queue add <uri> --next                  # put at the front
spotify queue move 3 1                          # position 3 → 1
spotify queue remove 2                          # by position, id (q_…) or uri
spotify queue clear
spotify next                                    # plays the managed head now
```

## How it plays

- When the current song is within 0.9 s of its end, the daemon starts the managed head through
  AppleScript. If Spotify moves on first (the queue was added too late, crossfade, a skip or stop
  in the Spotify app), the daemon switches to the managed head immediately.
- An item leaves the queue only once Spotify actually shows it playing. Until then the hand-off
  is pending and nothing else is decided; if it does not show up within 5 s it is retried, and
  after three failed hand-offs the item is dropped (logged in `spotify daemon logs`).
- Shortly before the first hand-off (and when you add to an idle queue) the daemon remembers what
  the queue will interrupt: the playing context (playlist/album) and the item up next in it.
- When the managed queue drains and its last item ends (or Spotify stops), the daemon resumes
  that item in that context, so a playlist continues where you left it.
- Queue timing needs a known track length; right after a switch, while Spotify still reports 0,
  nothing is handed over.

## `next`

- With managed items, `spotify next` plays the head now. With an empty managed queue it is
  Spotify's own next track (never an error).
- Quick or concurrent `next`s are applied one after another. A `next` that arrives while the
  previous hand-off is still in flight (sent, not yet showing) skips that item: it leaves the
  queue, counts as played, and the item after it plays. When nothing is left, Spotify's own next
  runs once the skipped item shows (up to 1.5 s), so it moves past that item. The reply then has
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
  playing now there again (ten times for an episode played without a context, and for a track on
  repeat-one); those leading repeats are left out, and `spotify_upcoming.current_repeats_left_out`
  and `spotify_upcoming.note` in `--json` say so. The human output prints the note.

Names, artists (the show, for episodes) and lengths are recorded when an item is queued: from the
search hit `queue add --search` picked (sent to the daemon as `known`, so no lookup is needed),
from spotify_player for a track URI, and from the Web API for an episode URI (spotify_player
cannot look up episodes; the daemon uses spotify_player's cached token, spending at most 8 s per
`queue add`). A failed lookup leaves them out, and the item shows by kind and id.

Only tracks and episodes can be queued: anything else is `invalid_input`
(`spotify:album:… is an album; the queue holds tracks and episodes.`), with a hint that fits the
kind, such as playing the whole album with `spotify play`. The queue belongs to the Mac, not to
one Silicon: every Silicon on this user account sees and edits the same queue, and each item
records who added it (`added_by`).
