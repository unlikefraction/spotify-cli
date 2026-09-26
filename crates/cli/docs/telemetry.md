# Telemetry

spotify-cli records usage and diagnostics in Space Station so failures can be traced and
fixed. It is **on by default** and easy to turn off.

## Turn it off

Any one of these wins:

```sh
spotify config set '{"telemetry": false}'      # this Silicon home (also clears the daemon's backlog)
export SPOTIFY_TELEMETRY=0                     # or SPACE_STATION_TELEMETRY=0 / SILICON_TELEMETRY=0
```

Requests then carry `X-Spotify-Telemetry: off`, and the backend records nothing for them. Stemcell
sets `SPACE_STATION_TELEMETRY=0` for every tool when its own telemetry is off.

On the website, the footer toggle ("Anonymous usage stats") stores the choice in your browser
(`localStorage` key `spotify-cli.telemetry`). The site sends nothing when your browser sends
Global Privacy Control or Do Not Track; the toggle then shows off and cannot be switched on.

## What is recorded

Self-contained events: event name, step (command or op), outcome (`ok`/`error`), error code,
duration, which path worked (`via`) and why a fallback happened, versions, OS and architecture,
a per-command trace id, and `ISI` when set.

**Never recorded:** track, album or artist names, lyrics, search queries, `spotify how` questions,
trigger notes, playlist names, URIs you play, tokens, SLTs, file contents. The offline commands
(`spotify` alone, `how`, `docs`, `commands`, `completions`, `iam`) record nothing at all.

The website records only a `page_view` event per page and an `install_command_copied` event when
you copy the install command. Each carries the page path, the referrer's host (not its full
address) and the window size, and nothing else. The site sends them straight to the backend's
gateway, sets no cookies, and needs JavaScript to send anything.

## Where it goes

The CLI hands events to the daemon, which keeps up to 2 000 of them and relays them to the
backend's gateway every minute. Each wake drains the whole backlog, in requests of at most 40
events and 64 KiB (what the gateway accepts); when the backend cannot be reached, the rest waits
for the next minute. Only the backend holds Space Station keys.
Tables (org `unlikefraction`):

| Table | Written by |
| --- | --- |
| `spotifybackend` | the backend itself (requests, logins, Ting sends, reports) |
| `spotifyclidaemon` | CLI and daemon events (relayed) |
| `spotifyfrontendanalytics` | website page analytics (relayed) |
| `spotifyfrontendevents` | website events (relayed) |
