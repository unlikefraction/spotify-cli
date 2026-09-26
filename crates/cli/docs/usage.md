# spotify-cli: quick start

spotify-cli turns the Spotify desktop app on a Mac into a command-line app. Carbons use it from
a terminal; Silicons use it as a tool, and can ask to be **notified through Ting** at playback
checkpoints ("30 seconds left", "25% left", "halfway", "song over").

The product is spotify-cli and its command is `spotify`. New to Carbons, Silicons, IAM, SLTs or
Ting? Read [Concepts](#concepts) at the end of this guide first.

## 1. Install (once per Mac)

```sh
curl -fsSL https://spotify.unlikefraction.com/install.sh | sh
```

The installer puts `spotify` and `spotify-daemon` on your PATH, installs `spotify_player` with
Homebrew if it is missing, registers the daemon to start at login, and starts it. It never logs
you in ([Install options](#install-options), [Uninstall](#uninstall)). Then:

```sh
spotify doctor          # every dependency, with the exact fix for anything missing
spotify auth login      # sign spotify_player in to Spotify (a browser tab; a Carbon clicks Agree once)
```

`spotify doctor` also checks that the backend still supports this CLI version
(`cli_version_supported`; the fix is `spotify update`), and exits 1 with `doctor_failed` while a
required check fails.

Requirements: macOS, the Spotify desktop app (signed in), Homebrew (for `spotify_player`). While
installing, macOS asks whether `spotify-daemon` may control Spotify: click **Allow** (System
Settings → Privacy & Security → Automation if you missed it). The installer's last lines say
whether that answer is still needed. Until then commands fail with `timeout`.

## 2. Play music

```sh
spotify status                          # what is playing, position, time left
spotify play --search 'arctic monkeys 505'
spotify play spotify:playlist:37i9dQZF1DXcBWIGoYBM5M --shuffle
spotify pause
spotify resume
spotify next
spotify previous
spotify seek 1:30                       # also +15s, -10s or 50%
spotify volume 40                       # also +10 or -10
spotify track                           # details of the current song
spotify lyrics
spotify search 'lofi' --type playlist --limit 5
spotify queue add spotify:track:<id>    # appends to the managed queue; --next puts it first
spotify queue                           # lists it
spotify queue remove 1
spotify playlist list
spotify playlist create 'Focus'
spotify playlist add <playlist-id> spotify:track:<id>
spotify podcast search 'tim ferriss'
spotify podcast play spotify:episode:<id>
```

Every command prints human text by default and exactly one JSON value with `--json`. Human text
keeps each Spotify name on one line (line breaks and tabs inside names become spaces); `--json`
keeps the names as Spotify sends them. Every playback result says which path worked
(`via: spotify_player` or `via: applescript`) and, when the first path failed, why
(`fallback.reason`). Seeks, and starting a single track, episode, show or Liked Songs, go to
AppleScript first; `previous` restarts the item from 3 s in (`result: restarted`). See
`spotify docs playback`.

**Spotify references.** Anything that takes an item accepts `spotify:<kind>:<id>`, a link
`https://open.spotify.com/<kind>/<id>`, or a bare id (add `--type` where the kind is not implied).
An id is exactly 22 letters and digits. Anything else, or a reference of the wrong kind (an album
given to a playlist command), fails with `invalid_input` before anything is sent to Spotify; a
well-formed id that does not exist is `not_found`. `playlist add` and `playlist remove` take
tracks and albums; episodes are `unsupported` (edit those in the Spotify app).

**Search, details and library.** `spotify search --limit` takes 1 to 10 results per kind
(default: the `search_limit` setting, else 10), because spotify_player returns at most 10;
`spotify podcast search --limit` too (default 10). The search footer suggests what fits the
results: `queue it: spotify queue add <uri>` for tracks and episodes,
`look inside: spotify track <uri>` for albums, artists and playlists.

`spotify track` describes the song playing now (for a podcast episode: `show:` instead of
`album:`). For a song, now or by `spotify:track:` URI, it adds `liked: yes|no`: whether the song is
in Liked Songs (`liked` in `--json`, left out when that check fails or takes over 2.5 s).
`spotify track <album>` shows the release date, track count, length and track list;
`spotify track <artist>` its top tracks, albums, singles and related artists;
`spotify track <playlist>` what `spotify playlist show` shows. With a URI, `--json` has one shape
for every kind: `kind`, `item`, the per-kind fields and `raw` (for a playlist: `playlist`,
`owner`, `collaborative`, `track_count`, `duration_ms`, `duration`, `tracks`). A bare id is looked
up as a track unless `--type album|artist|playlist` says otherwise; podcast shows and episodes
cannot be looked up by id (`--type show` is a usage error).

`spotify library <section>` and `spotify playlist list` show everything unless `--limit N`
(1 or more) says otherwise. Library and playlist reads are repeated once after a network blip,
and a read right after a playlist change or like/unlike waits until it can see the change
(`spotify docs daemon`).

**Playlists.** `spotify playlist create 'Focus' --json` returns the new playlist's bare `id` and
its `uri`. `spotify playlist fork <playlist>` copies a playlist into a new one you own and returns
its `id`, `uri` and `name`; `--name 'Focus (mine)'` names the copy (default: the original's name),
and a named copy is private. Forks are imports: `spotify playlist sync` brings them up to date.
Playlists cannot be renamed (`spotify playlist rename` explains).

## 3. For Silicons: log in once, then set triggers

Triggers notify *you* (the Silicon that set them) through Ting, so they need your identity. The
SLT comes from the `iam` CLI (`cargo install silicon-iam-cli`; details in `spotify docs auth`):

```sh
spotify iam --json                                   # app_id: spotify
iam silicon-login --app-id spotify --grant-org "$SILICON_ORG" --approve-scopes   # prints an SLT
spotify login '<SLT>'                                # exchanges it; registers you with Ting
spotify login status --json                          # {"authenticated": true, ...}
```

Stemcell does these steps for you when `spotify` is in your `apps`.

```sh
spotify trigger add --remaining 30s --note 'wrap up the call'
spotify trigger add --remaining 25%
spotify trigger add --elapsed 50%
spotify trigger add --end --note 'song over: check the build'
spotify trigger add --end --scope every            # every song end, until removed
spotify trigger list
spotify trigger history
spotify trigger remove <id>
spotify trigger test                               # send yourself a test Ting now
```

When a checkpoint is reached you receive a `spotify.trigger.fired` Ting whose `data` holds the
trigger (id, condition, note), the track and the position, and whose `metadata.isi` names the ISI
that created it (from `$ISI`), so your flow can route it back. If a `current` trigger's song is
skipped first you get `spotify.trigger.expired`. Details: `spotify docs triggers`, `spotify docs ting`.

No Ting? Add `--local` and block on it instead: `spotify trigger wait <id> --timeout 10m`.

## 4. When something fails

Errors say what failed, why, and the exact next step:

```text
error: spotify_player is not signed in to Spotify, so Web API features … cannot run.
hint: Run `spotify auth login`: it opens Spotify's consent page in the browser on this Mac…
(code: spotify_auth_required)
```

With `--json` the same error is one object on stderr: `{"error":{"code","message","hint","retryable","details"}}`.
Exit codes: 0 ok, 1 failed, 2 usage, 3 not signed in, 4 refused, 5 unavailable. All codes:
`spotify docs errors`.

Found a bug? It is open source: reproduce, patch, open a pull request at
https://github.com/unlikefraction/spotify-cli, and report it:

```sh
spotify report 'what I ran, what happened, what I expected' --pr https://github.com/unlikefraction/spotify-cli/pull/<n>
spotify report 'daemon crash' --attach ~/.silicon-spotify/daemon.log
```

`--attach` takes text files only, at most 5. A binary file (a NUL byte in its first 8 KiB or last
64 KiB, or text that is not UTF-8), a directory, and any home's `.spotify/session.json` or
`.spotify/testing.json` are refused with `invalid_input` (exit 2) before anything is sent. Of a
longer file only its last 64 KiB are read and sent, starting at a whole character.

## Install options

The installer reads these environment variables (put them before `sh`):

| Variable | Effect |
| --- | --- |
| `SPOTIFY_VERSION=v0.1.1` | install this release tag (default: the latest) |
| `SPOTIFY_INSTALL_DIR=/path` | where `spotify` and `spotify-daemon` go (default: `/usr/local/bin` if writable, else `~/.local/bin`) |
| `SPOTIFY_ARCHIVE=/path/to/spotify-….tar.gz` | install from a local release archive (no download, no checksum check) |
| `SPOTIFY_INSTALL_FROM_SOURCE=1` | build both binaries with `cargo` from the repository instead |
| `SPOTIFY_SKIP_DEPS=1` | do not install Spotify.app or spotify_player (it only reports them) |
| `SPOTIFY_NO_START=1` | install only: do not start Spotify or the daemon (later: `spotify daemon install`) |

```sh
curl -fsSL https://spotify.unlikefraction.com/install.sh | SPOTIFY_INSTALL_DIR="$HOME/bin" sh
```

When the install directory is not on your PATH, the installer adds it to your shell's startup
file (`~/.zshrc`, `~/.bash_profile`, fish's `config.fish`, else `~/.profile`) under a
`# spotify-cli` line. On Linux it stops after copying the binaries (no dependencies, no daemon):
login, config, docs and report work there, Spotify control needs macOS. Installs made with Honeycomb (`honeycomb install spotify`)
are managed by Honeycomb, which also updates them.

State lives in two places:

| Where | Per | Holds |
| --- | --- | --- |
| `~/.silicon-spotify/` | macOS user | the daemon's state: its socket, database (triggers, queue), log, `install.json` (`spotify docs daemon`) |
| `$SILICON_HOME/.spotify/` (else `~/.spotify/`) | Silicon home | that home's `config.json`, IAM session and testing selection |

## Uninstall

```sh
spotify daemon uninstall                 # stop the daemon and remove its launchd agent
rm "$(command -v spotify-daemon)" "$(command -v spotify)"
rm -rf ~/.silicon-spotify                # daemon state: triggers, queue, log
rm -rf "${SILICON_HOME:-$HOME}/.spotify" # repeat for every Silicon home that used spotify-cli
```

Then delete the `# spotify-cli` line and the PATH line under it from your shell's startup file
(older installers wrote `# silicon-spotify` instead). Optionally remove spotify_player and its
Spotify sign-in too:

```sh
brew uninstall spotify_player
rm -rf ~/.cache/spotify-player           # its Spotify tokens
```

## Concepts

- **Carbon**: a person. **Silicon**: an AI agent with its own identity (a public id such as
  `si:you`), home directory (`SILICON_HOME`) and organization (`SILICON_ORG`). spotify-cli keeps
  one session per Silicon home, so every Silicon on a Mac has its own login and triggers.
- **IAM** (Silicon IAM): the identity service Carbons and Silicons sign in to. It decides which
  apps may act for you and with which scopes. Its CLI is `iam`: `cargo install silicon-iam-cli`
  (crate `silicon-iam-cli`, binary `iam`). The service runs at
  https://backend.iam.teamofsilicons.com; sign-in and consent pages are at
  https://auth.iam.teamofsilicons.com.
- **SLT** (short-lived token): what `iam silicon-login --app-id spotify` (a Silicon) or
  `iam login --app-id spotify` (a Carbon) prints. It lasts about 2 minutes and works once;
  `spotify login` exchanges it for a spotify-cli session. spotify-cli never sees passwords, SID or
  STK (`spotify docs auth`).
- **Ting**: the notification service. spotify-cli sends trigger firings as Tings; Ting delivers
  them, with retries and muting, to the machine of the Silicon that set the trigger
  (`spotify docs ting`).
- **Stemcell**: the runtime a Silicon runs in. It installs, configures and logs in the apps the
  Silicon lists in its `apps`, and hands incoming Tings to the Silicon's flow.
- **ISI**: the name of a part of a Silicon's flow that messages can be sent to, such as `planner`.
  When `ISI` is set, spotify-cli records it as `metadata.isi` on the triggers you create, so the
  flow can route the notification back (`notify_isi` in `spotify docs config` sets a default).
- **Space Station**: the telemetry store. spotify-cli's usage and diagnostics go there through
  the backend, never lyrics, titles or queries (`spotify docs telemetry`).

## Where to go next

Every guide is bundled in the CLI (`spotify docs <topic>`, `spotify docs --search <text>`) and
published at https://spotify.unlikefraction.com/docs, where `/` jumps to the search box.

| You want to… | Read |
| --- | --- |
| Understand triggers and their exact semantics | `spotify docs triggers` |
| Know how play/pause/seek decide between spotify_player and AppleScript | `spotify docs playback` |
| Queue, reorder and remove upcoming songs | `spotify docs queue` |
| Log in, Spotify sign-in, testing planes | `spotify docs auth` |
| Change settings, bring your own Spotify client id | `spotify docs config` |
| Know what runs in the background and where state lives | `spotify docs daemon` |
| Look up an error code and its exit code | `spotify docs errors` |
| Build on top (library, daemon protocol, backend API) | `spotify docs development`, `spotify docs api` |
