# Triggers

A trigger is a checkpoint in playback. When it is reached, `spotify-daemon` sends a Ting to the
Silicon that created it. Use them to act at the right moment: start wrapping up before a song ends,
queue the next thing at the halfway mark, or run something when a song is over.

## Create one

Exactly one condition:

| Flag | Fires when | Example |
| --- | --- | --- |
| `--remaining T` | at most `T` is left | `--remaining 30s`, `--remaining 1:00` |
| `--remaining P%` | at most `P`% of the track is left | `--remaining 25%` |
| `--elapsed T` / `--at T` | at least `T` has played | `--elapsed 1:30`, `--at 2:00` |
| `--elapsed P%` | the `P`% mark is passed | `--elapsed 50%` |
| `--end` | the song finishes by playing to its end | `--end` |
| `--change` | the song stops being current for any reason | `--change` |

Times: `90`, `90s`, `1:30`, `1m30s`, `250ms`, `1:02:03`. Percentages: `0%`–`100%`.

Scope (`--scope`, default `current`):

| Scope | Watches | Fires |
| --- | --- | --- |
| `current` | only the song playing now | once, then the trigger completes |
| `every` | every song (ads excluded) | on every song, until removed or `--times N` |
| `track --track <uri>` | every play of one track | on every play, until removed or `--times N` |

Other options: `--times N` / `--once`, `--note TEXT` (up to 1000 characters, echoed in the
notification), `--label NAME` (up to 80 characters), `--no-expiry-notice`, `--local`.

The request is checked before anything reads Spotify, whatever is playing: `--times 0`, a longer
note or label, a percentage outside 0–100%, or a `--track` that is not a track or episode fails
with `invalid_input` (exit 2).

```sh
spotify trigger add --remaining 30s --note 'start wrapping up the meeting'
spotify trigger add --elapsed 50% --scope every --times 3 --label halfway
spotify trigger add --end --scope track --track spotify:track:0BxE4FqsDD1Ot4YuBXwAPp
```

The response contains the trigger id (`trg_…`). Inspect with `spotify trigger show <id>`, list with
`spotify trigger list` (add `--all` for finished ones), remove with `spotify trigger remove <id>`
or `spotify trigger clear`. `spotify trigger history` shows recent firings (`--limit` 1–500,
default 20).

## Exact rules

These are implemented in the pure engine (`silicon_spotify_client::trigger`) and unit-tested.

- **A play** is one continuous playing of one item. A new play starts when the item changes, or
  when the same item restarts from the top after reaching its end (repeat-one).
- **Threshold conditions** (`remaining`, `elapsed`) fire once per play, the first time a reading
  shows the threshold reached, whether by playing or by seeking past it, playing or paused.
- **`end`** fires when a play *completes*: it was within 3 s of its end when the item changed or
  playback stopped. Skipping early is not an end. **`change`** fires on every play end and reports
  `reason`: `completed`, `skipped`, `stopped`, or `unknown` when the daemon did not see the end
  (more than 12 s between readings, e.g. it was not running); `end` does not fire on `unknown`.
- A single reading with nothing loaded is treated as transient; a play ends as `stopped` only
  after two such readings in a row (or at once when Spotify quits).
- A time checkpoint past the end of the track (`--elapsed 10:00` on a 3:00 song) is never reached:
  an error for `current` scope, skipped for `every`/`track`.
- **Never retroactive.** A trigger never fires for a play whose checkpoint was already behind it at
  creation. For `current` scope that is an error (`threshold_passed`, exit 2) so you know
  immediately; `every`/`track` scopes skip the current play and start with the next.
- **Expiry.** A `current` trigger whose play ends before the checkpoint (skipped, stopped, Spotify
  quit) becomes `expired` and sends `spotify.trigger.expired` (unless `--no-expiry-notice`), so a
  Silicon waiting for it is never left hanging.
- **Precision.** The daemon reads Spotify.app on every playback notification and on an adaptive
  timer, and wakes just before each checkpoint; firings land within ~0.2 s of the checkpoint.
  Seeks made in the Spotify UI are noticed within 2 s while triggers are active.
- **Durability.** Triggers and pending notifications are stored in the daemon database and survive
  daemon restarts; play ids are persisted so a restart mid-song does not create a new play.

## What you receive

Type `spotify.trigger.fired` (or `spotify.trigger.expired`). Stemcell delivers it to your flow as
one item of `request.tings`:

```json
{
  "id": "msg_…",
  "type": "spotify.trigger.fired",
  "key": "si:you/trg_…/7/fired",
  "data": {
    "outcome": "fired",
    "trigger": {"id": "trg_…", "label": "halfway", "condition": "elapsed", "threshold": "50%",
                "description": "50% mark passed", "scope": "every", "note": "…",
                "fired": 1, "times": 3, "final": false},
    "track": {"uri": "spotify:track:…", "name": "…", "artist": "…", "album": "…",
              "duration_ms": 253586, "url": "https://open.spotify.com/track/…", "artwork_url": "…"},
    "playback": {"position_ms": 126900, "position": "2:06", "remaining_ms": 126686,
                 "remaining": "2:07", "progress": 0.5},
    "reason": null,
    "at": "2026-09-26T10:00:00.000Z"
  },
  "metadata": {"isi": "planner", "app": "spotify", "app_version": "0.1.0", "host": "studio-mac"}
}
```

`playback.position` and `playback.remaining` are the same moments as `position_ms` and
`remaining_ms`, written as `m:ss`. `remaining` is rounded to the nearest second, so a
`--remaining 20s` trigger that fired with 19 782 ms left reads `0:20`; `position` is rounded down,
like a player's clock.

`metadata.isi` is the `ISI` environment variable of the process that created the trigger (or the
`notify_isi` setting). It is a routing hint for your flow, never authority. Routing example:
`spotify docs ting`.

## Delivery, retries and history

Firings go to a durable outbox. The daemon opens *your* session store under its lock, refreshes
the session if needed, and asks the backend to send the Ting with your identity. Transient
failures retry with backoff (10 s, doubling, at most 10 min) for one hour with the same Ting key,
so a notification is never duplicated; after an hour a playback checkpoint is stale and the
firing is marked `failed`. Permanent refusals (`recipient_not_registered`, `reconsent_required`,
`testing_selection_changed`, …) stop retrying at once and show in `spotify trigger history` with
the reason and the fix. Retry one by hand with `spotify trigger retry <firing-id>`.

## Without Ting

`--local` records firings only in the daemon. Wait for one synchronously:

```sh
id=$(spotify trigger add --remaining 10s --local --json | jq -r .trigger.id)
spotify trigger wait "$id" --timeout 10m --json
```

`trigger wait` also works for Ting triggers; it returns when the firing is created, or as soon as
the trigger finishes without firing (expired silently, removed).

## Troubleshooting

| Code | Meaning | Fix |
| --- | --- | --- |
| `invalid_input` | a bad time, percentage, `--times`, note, label or `--track` | the hint shows what is accepted |
| `threshold_passed` | the checkpoint is already behind the current song | later checkpoint, `--end`, `--scope every` |
| `nothing_playing` | `current` scope needs a song | start one, or use `--scope every` |
| `not_authenticated` | no login in this home | `spotify login '<SLT>'`, or `--local` |
| `recipient_not_registered` | Ting has no grant for you | `spotify ting register` |
| `reconsent_required` | your session lacks Ting scopes | log in again approving all scopes |
