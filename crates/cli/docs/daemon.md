# The daemon

`spotify-daemon` is the always-on half of spotify-cli. There is one per macOS user (one
Spotify.app per user), shared by every Silicon home on the machine.

What it does:

| Job | How |
| --- | --- |
| Watch Spotify.app | Spotify's `com.spotify.client.PlaybackStateChanged` notification (play, pause, track change) plus AppleScript readings on an adaptive timer: 2 s while playing with triggers or a queue, 5 s playing, 10 s paused, 15 s closed (3 s in the daemon's first two minutes until an Apple Event has reached Spotify, so macOS's permission question comes soon after Spotify starts during an install), and precisely at the next checkpoint |
| Run AppleScript | in-process `NSAppleScript` on the main thread, each script compiled once (~50 ms per read, no process spawn) |
| Fire triggers | the pure trigger engine; firings go to a durable outbox |
| Deliver Tings | opens the creating Silicon's session under its lock, refreshes if needed, sends through the backend; retries with backoff for an hour |
| Managed queue | hands over to the next queued item at the end of each song, resumes the interrupted context; `next`, `play <item>`, `previous` and hand-offs reach Spotify one at a time, never interleaved |
| Warm spotify_player | keeps one headless instance on a pseudo-terminal so Web API calls take ~20 ms, with its view of playback refreshed every 20 s ([below](#the-warm-spotify-player)) |
| Library reads | retries a library or playlist read once after a network blip, and makes reads right after a change wait until they can see it ([below](#library-and-playlist-reads)) |
| Telemetry relay | relays CLI and daemon events to the backend every minute, draining the whole backlog (when enabled) |
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

- `start` waits up to 15 s for the daemon to answer. While a previous instance is still exiting,
  launchd cannot load the agent again yet, so `start` keeps retrying until it can.
- `restart` with the launchd agent loaded restarts the job in place
  (`launchctl kickstart -k`), so the agent stays loaded, and waits up to 15 s for a daemon with a
  new pid. A daemon started outside launchd is shut down cleanly first. It prints
  `spotify-daemon restarted via launchd (pid N, vX).` (`via spawn` without the agent); `--json`
  gives
  `{"restarted": true, "started": true, "via": "launchd" | "spawn", "previous_pid", "status"}`.
- `stop` and `uninstall` return once launchd has unloaded the job, not just once the socket is
  gone, so a `start` right after them works. They wait up to 10 s; after that `stop` fails with
  `daemon_stuck`, and `uninstall` removes the agent anyway.

## Files

`~/.silicon-spotify/` is the daemon's state directory: one per macOS user, whatever
`SILICON_HOME` says.

```text
~/.silicon-spotify/            (SPOTIFY_DAEMON_HOME overrides; directory 0700)
  daemon.sock                  Unix socket (0600); JSON lines, one request per connection
  daemon.lock                  single-instance lock (kept forever)
  daemon.sqlite                triggers, firings/outbox, managed queue, tracker, settings (0600)
  daemon.log                   log (launchd and the CLI launcher append here)
  spotify-auth.log             output of `spotify auth login`
  install.json                 written by the installer or Honeycomb: method, directory, version
  warm-player.json             pid and start time of the warm spotify_player this daemon started
~/Library/LaunchAgents/com.unlikefraction.spotify.daemon.plist
```

Per Silicon home (`$SILICON_HOME/.spotify/`, else `~/.spotify/`): `config.json`, `session.json`,
`session.lock`, `testing.json`. The daemon reads a home's session only to deliver that home's
triggers. Removing all of it: `spotify docs usage` (Uninstall).

## The warm spotify_player

spotify_player's CLI hands each command over UDP to the one running instance that holds its client
port (`127.0.0.1:8080` unless `client_port` in its `app.toml` says otherwise), or starts a
throwaway client (~1.5 s) when none answers. The daemon runs one copy of its own, headless, on a
pseudo-terminal it owns, with streaming, media keys and notifications off, so it never becomes a
playback device and only answers Web API commands.

- **Fresh view.** The copy runs with `-o playback_refresh_duration_in_ms=20000`: it re-reads
  playback (`GET /v1/me/player`) every 20 s, and 1 s and 3 s after each command it runs. A
  positive `playback_refresh_duration_in_ms` in your own `app.toml` is kept when it is slower and
  raised to 10000 when it is faster; 0 or none gives 20000. `spotify daemon status` shows the
  interval it runs with ("playback refresh every 20 s", `refresh_ms` in `--json`).
- **Why not faster.** Spotify rate-limits a client ID over a rolling 30-second window and does
  not publish the limit. The poll shares that quota with spotify_player's commands, its re-reads
  after them and the daemon's own lookups. With a development-mode client ID, a 3 s poll (10 GETs
  per window) drew a 429 about every 30 s even while idle, and each `Retry-After` (6–15 s) froze
  the view and stalled commands. 20 s is 1–2 GETs per window (the 10 s floor at most 3), which
  leaves most of the quota to commands.
- **How far behind.** After a change made in Spotify.app (or by AppleScript), spotify_player's
  idea of what plays, the repeat mode and shuffle can be up to about 20 s behind. Commands that
  depend on it compare it with Spotify.app first, and when they disagree AppleScript acts, the
  view is nudged, or the Web API is read once (`spotify docs playback`: *Checking
  spotify_player's view first*). The poll keeps its interval while Spotify.app is paused or
  closed: spotify_player reads the setting only when it starts, and changing it would mean
  restarting the copy (a new sign-in, and a gap in which commands fall back to one-off clients).
- **One copy.** Before it starts one, the daemon stops copies left by earlier daemons: the pid
  recorded in `warm-player.json` (while its start time matches) and orphaned processes of your
  user that carry the daemon's `-o` overrides (streaming, media keys and notifications off;
  SIGTERM, then SIGKILL after 2 s). A spotify_player you run yourself is never touched.
- **Serving.** It counts as `running` only once its copy holds the client port (checked with
  `lsof`, and again every 5 minutes; when `lsof` cannot tell, it is `running` with
  `serves_cli: null` and a note). A copy that does not get the port within 30 s is stopped and
  started again after a delay that grows from 4 s to at most 128 s (`not_serving`). If another
  process holds the port (usually a spotify_player you run yourself, such as its TUI), the daemon
  starts none and reports `deferred` with that process's pid: that instance then answers
  spotify-cli's commands from its own state, refreshed only as often as its own settings say. The
  daemon looks again every 30 s.
- **Lifetime.** The copy dies with the daemon, even on SIGKILL: the daemon's exit hangs up the
  pseudo-terminal and the kernel sends the copy SIGHUP. `spotify daemon stop` and `restart` also
  send SIGTERM to its process group, then SIGKILL after 1 s. When the copy exits by itself, the
  daemon starts a new one after 5 s if it had run for over 2 minutes; after quicker exits the
  delay starts at 10 s and doubles each time, up to 320 s.
- `SPOTIFY_WARM_PLAYER=off` turns it off (copies left by earlier daemons are still stopped).

`spotify daemon status` (`warm_spotify_player` in `--json`) says what it is doing, in words:

| State | Meaning (`--json` fields) |
| --- | --- |
| `running` | the copy holds the client port and serves spotify-cli (`pid`, `port`, `refresh_ms`, `port_owner_pid`, `serves_cli`) |
| `starting` | started, waiting for it to take the port (`pid`, `port`) |
| `not_serving` | it never got the port; stopped, retried later (`port_owner`, `retry_in_s`) |
| `deferred` | another process, usually your own spotify_player, holds the port (`port_owner`: `pid`, `parent_pid`, `kind`, `command`; `since`) |
| `restarting` | it exited; a new copy starts soon (`last_exit`, `retry_in_s`) |
| `failed`, `unavailable` | it cannot start (`error`) |
| `waiting_for_spotify_auth` | spotify_player is not signed in: `spotify auth login` |
| `disabled` | `SPOTIFY_WARM_PLAYER=off` (`reason`) |

`spotify doctor`'s `spotify_player_warm_instance` check passes only when the daemon's own copy
holds the client port; its detail adds `port_owner_pid_now`. It is not required (shown with `·`):
commands work without it, only slower. When it fails the human output adds
`state: <the state in words>`, and when the daemon's copy runs but another process holds the
port, that line ends with `the port is held by pid N now`.

## Library and playlist reads

- spotify_player hands every identical Web API GET that arrives within 1 s of a response the same
  response, even after a change. So library and playlist reads (`library <section>`,
  `playlist list`, `playlist show`, `track <playlist>`) made within 1.1 s after a playlist change
  or a like/unlike wait out the rest of that time, and so do reads within 1.1 s after a read that
  was in flight when such a change finished. They always show the change.
- The daemon repeats such a read once, after 500 ms, when spotify_player reports `transport` or
  `spotify_player_busy` (also `track <uri>` for tracks, albums and artists). `rate_limited` and
  `timeout` are not repeated, and writes never are. The CLI repeats `library`, `playlist list`
  and `playlist show` once more the same way, so one network blip can mean up to four attempts.

## Permissions

macOS asks once whether `spotify-daemon` may control Spotify (Automation). The daemon's first
reading of a running Spotify raises the question, and the installer waits for the answer, because
while that dialog is open macOS holds **every** Apple Event to Spotify, from any app. Until
someone clicks Allow, commands fail with `timeout`, whose hint names the dialog. After one timeout
the daemon fails fast for 3 s instead of queueing, and commands already waiting behind it fail
fast too.

`spotify daemon status` shows what the Apple Events so far say (`automation` in `--json`):

| State | Meaning |
| --- | --- |
| `granted` | an Apple Event reached Spotify |
| `denied` | macOS refused (`automation_permission_denied`) |
| `not_answering` | Spotify did not answer: usually the dialog is open (it may be behind other windows) |
| `unknown` | no Apple Event has reached a running Spotify yet (Spotify is closed), or the Spotify that stopped answering has quit |

A reading that finds Spotify closed sends no Apple Event, so it never counts as `granted`.
`spotify doctor` reports `automation_permission`: failing for `denied` and `not_answering`, and
optional (·) for `unknown`, with the fix: start Spotify (`spotify launch`), then run
`spotify doctor` again. The doctor waits out any remaining 3 s fail-fast window first, so it
really re-checks.

If you clicked Don't Allow: System Settings → Privacy & Security → Automation → spotify-daemon →
enable Spotify. If no prompt ever appears, try resetting only the daemon's Automation answers,
then retry:

```sh
tccutil reset AppleEvents com.unlikefraction.spotify-daemon
```

`com.unlikefraction.spotify-daemon` is the daemon's code-signing identifier; check it with
`codesign -dv "$(command -v spotify-daemon)"` (the `Identifier=` line). spotify-daemon is a plain
executable, not an app bundle, so `tccutil` may answer `No such bundle identifier`. The only
other reset is `tccutil reset AppleEvents` with no identifier, which forgets the Automation
answers of every app on the Mac, so each of them asks again.

Release binaries are ad-hoc signed, so macOS may ask again after an update: click Allow. A
Developer ID signature (`SPOTIFY_CODESIGN_IDENTITY` when packaging) keeps the answer across
updates.

## Security

The socket is owner-only inside an owner-only directory, and both sides check the peer runs as
the same user. The daemon holds no credentials of its own; Silicon sessions stay in each home.
It reads spotify_player's cached Spotify access tokens only for two Web API lookups spotify_player
has no command for (`spotify docs auth`), and sends them only to `api.spotify.com`. It never
sends lyrics, titles, search queries or notes to telemetry.

Telemetry (`spotify docs telemetry`) waits in `daemon.sqlite` (at most 2 000 events). Every minute
the daemon relays all of it, in requests of at most 40 events and 64 KiB, and keeps the rest for
the next minute when the backend cannot be reached.
