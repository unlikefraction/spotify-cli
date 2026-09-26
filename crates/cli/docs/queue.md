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
- Explicit commands (`spotify play <uri>`, `previous`, `next` with an empty queue) are yours: for
  5 s the daemon does not override item changes. `spotify play <uri>` also discards the resume
  point; the managed queue itself stays and plays after that item.
- Queue timing needs a known track length; right after a switch, while Spotify still reports 0,
  nothing is handed over.

Only tracks and episodes can be queued (`invalid_input` otherwise; play whole albums with
`spotify play`). The queue belongs to the Mac, not to one Silicon: every Silicon on this user
account sees and edits the same queue, and each item records who added it (`added_by`).
