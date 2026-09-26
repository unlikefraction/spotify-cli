# Changelog

## 0.1.0 — 2026-09-26

First release.

- `spotify` CLI: playback (verified spotify_player → AppleScript fallback), track details, lyrics,
  search, library, devices, playlists, podcasts, managed queue.
- Triggers (`--remaining`, `--elapsed`, `--end`, `--change`; current/every/track scopes) delivered
  as `spotify.trigger.fired` / `spotify.trigger.expired` Tings, durable and idempotent.
- IAM SLT login, live `login status --json`, logout, Ting recipient registration, testing planes.
- `spotify-daemon`: in-process AppleScript, Spotify notifications, warm spotify_player, outbox,
  telemetry relay, hourly updates.
- Backend `spotify-api`: SLT exchange, refresh, logout, me, Ting registration and sends with OBO
  proofs, bug reports, telemetry gateway, IAM webhooks.
- One-line installer, website and docs, Honeycomb package for six targets.
