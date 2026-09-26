# The daemon

`spotify-daemon` is the always-on half of spotify-cli. There is one per macOS user (one
Spotify.app per user), shared by every Silicon home on the machine.

What it does:

| Job | How |
| --- | --- |
| Watch Spotify.app | Spotify's `com.spotify.client.PlaybackStateChanged` notification (play, pause, track change) plus AppleScript readings on an adaptive timer: 2 s while playing with triggers or a queue, 5 s playing, 10 s paused, 15 s closed, and precisely at the next checkpoint |
| Run AppleScript | in-process `NSAppleScript` on the main thread, each script compiled once (~50 ms per read, no process spawn) |
| Fire triggers | the pure trigger engine; firings go to a durable outbox |
| Deliver Tings | opens the creating Silicon's session under its lock, refreshes if needed, sends through the backend; retries with backoff for an hour |
| Managed queue | hands over to the next queued item at the end of each song, resumes the interrupted context |
| Warm spotify_player | keeps one headless instance on a pseudo-terminal so Web API calls take ~20 ms |
| Telemetry relay | batches CLI and daemon events to the backend every minute (when enabled) |
| Updates | checks GitHub releases hourly and installs verified updates (script installs; Honeycomb updates its own installs) |

## Commands

```sh
spotify daemon status       # version, uptime, Spotify state, triggers, deliveries, warm player, updates
spotify daemon start        # launchd when installed, else a detached process
spotify daemon install      # launchd agent com.unlikefraction.spotify.daemon (start at login, KeepAlive)
spotify daemon restart
spotify daemon logs --lines 100
spotify daemon stop
spotify daemon uninstall
```

The CLI starts the daemon on demand, and when the CLI is newer than the running daemon it
restarts it, so an updated binary takes over immediately.

## Files

```text
~/.silicon-spotify/            (SPOTIFY_DAEMON_HOME overrides; directory 0700)
  daemon.sock                  Unix socket (0600); JSON lines, one request per connection
  daemon.lock                  single-instance lock (kept forever)
  daemon.sqlite                triggers, firings/outbox, managed queue, tracker, settings (0600)
  daemon.log                   log (launchd and the CLI launcher append here)
  spotify-auth.log             output of `spotify auth login`
~/Library/LaunchAgents/com.unlikefraction.spotify.daemon.plist
```

Per Silicon home (`$SILICON_HOME/.spotify/`): `config.json`, `session.json`, `session.lock`,
`testing.json`. The daemon reads a home's session only to deliver that home's triggers.

## Permissions

macOS asks once whether `spotify-daemon` may control Spotify (Automation). The daemon raises the
question as soon as it starts (the installer waits for it), because while that dialog is open
macOS holds **every** Apple Event to Spotify, from any app. Until someone clicks Allow, commands
fail fast with `automation_permission_pending` instead of hanging, and `spotify daemon status`
shows `automation: waiting for Allow`.

If you clicked Don't Allow: System Settings → Privacy & Security → Automation → spotify-daemon →
enable Spotify (`automation_permission_denied`). If no prompt ever appears,
`tccutil reset AppleEvents` and retry. `spotify doctor` reports `automation_permission` either way.

Release binaries are ad-hoc signed, so macOS treats each new version as a new program and asks
again after an update. A Developer ID signature (`SPOTIFY_CODESIGN_IDENTITY` when packaging)
keeps the answer across updates.

## Security

The socket is owner-only inside an owner-only directory, and both sides check the peer runs as
the same user. The daemon holds no credentials of its own; Silicon sessions stay in each home.
It never sends lyrics, titles, search queries or notes to telemetry.
