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
sets `SPACE_STATION_TELEMETRY=0` for every tool when its own telemetry is off. On the website,
the footer toggle stores the choice in your browser.

## What is recorded

Self-contained events: event name, step (command or op), outcome (`ok`/`error`), error code,
duration, which path worked (`via`) and why a fallback happened, versions, OS and architecture,
a per-command trace id, and `ISI` when set.

**Never recorded:** track, album or artist names, lyrics, search queries, trigger notes, playlist
names, URIs you play, tokens, SLTs, file contents.

## Where it goes

The CLI hands events to the daemon, which relays them in batches to the backend's gateway; only
the backend holds Space Station keys. Tables (org `unlikefraction`):

| Table | Written by |
| --- | --- |
| `spotifybackend` | the backend itself (requests, logins, Ting sends, reports) |
| `spotifyclidaemon` | CLI and daemon events (relayed) |
| `spotifyfrontendanalytics` | website page analytics (relayed) |
| `spotifyfrontendevents` | website events (relayed) |
