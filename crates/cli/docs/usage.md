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

## 2. Find anything

You never have to remember a command. These answer offline, in milliseconds:

```sh
spotify                                            # set up? missing? what to try
spotify --help                                     # every command by goal, with examples
spotify how "lyrics of a song that isn't playing"  # ask in plain words
spotify lyrics --help                              # any command: its flags and examples
spotify docs playback --section 'Web API first'    # one section of a guide
spotify commands --json                            # the whole CLI as JSON (below)
```

- **`spotify`** with no arguments checks what is set up (macOS, Spotify.app, spotify_player and
  its Spotify sign-in, spotify-daemon, the permission to control Spotify, your IAM login), marks
  each ✓, · (optional) or ✗ with its fix, shows what is playing and up to five things to try,
  fixes first. It reads only local state: the daemon is asked over its socket only when it
  already runs, never started. It exits 0; `spotify doctor` does the full live check. `--json`
  gives `{version, ready, now_playing, checks[{check, ok, detail, fix, optional}],
  try[{command, description}], help}`.
- **`spotify --help`** lists the commands by goal: Listen, Find, Lyrics & details, Queue,
  Playlists, Podcasts, Triggers (Ting), Account & login, Setup & diagnose. Every command's
  `--help` lists its own flags under `Options:`, then under `Global options:` the ones every
  command takes (`--json`, `--org`, `--api-url`, `-h`/`--help`, `-V`/`--version`), and ends with
  examples you can run as they are.
- **`spotify how "<question>"`** searches every command's help (summary, flags, allowed values,
  examples), the bundled guides, every setting and the error codes, and prints the best commands
  with ready-to-run examples, the guide section to read
  (`spotify docs <topic> --section '<heading>'`) and any error code that fits. The examples are
  made for the question. A song, album or artist name in it (quoted, capitalized, or the words no
  guide knows) becomes `spotify search '<name>' --type track` then `spotify lyrics <uri>` for
  lyrics, or `--search '<name>'` for play, queue add and search; a name never outweighs what you
  want to do. A setting becomes `spotify config set '{"<key>": <value>}'` with the value you asked
  for; one that is already that way by default becomes `spotify config get <key>` to check it,
  then the `config set` line that changes it, only if you want that ("keep Spotify from jumping
  to the front" is `keep_spotify_in_background`, on by default). "Which commands change my
  library" gets the manifest filter for that change and the commands themselves (`like`,
  `unlike`); "which commands don't change anything" gets the filter for the commands that can
  run without changing anything. A command named as it is typed ("spotify play returns exit
  code 1") comes first, and an error code named as it is written (`track_mismatch`) always gets
  its row. A shell gets both of its completion install lines, a flag gets
  `spotify docs --search '<flag>'`, and a question that only asks to see something ("what's in
  the queue") puts read-only commands and examples first. `--limit` takes 1 to 10 (default 3);
  `--json` gives `{question, name, terms, commands, guides, errors, more}` (`name`: the name it
  found, or null).
- **`spotify docs`** lists the guides; `spotify docs <topic> --section '<heading>'` prints one
  section, `spotify docs --search <text>` searches them all and
  `spotify docs <topic> --search <text>` only that guide (its `--json` adds `topic`). `--search`
  takes text that starts with a hyphen (`spotify docs --search --random`), cannot be combined
  with `--section`, and an empty text is `invalid_input`.
- **Mistakes are explained.** A mistyped command gets the closest one first (`spotify lyircs`:
  "did you mean 'lyrics'?"). A group's unknown subcommand exits 2 with the real ones and their
  aliases: `queue next` answers "`spotify queue` has no subcommand `next`; its
  subcommands are list (show, ls), add, remove (rm), move (mv), clear. To skip to the next track:
  spotify next." Search words given to `spotify lyrics` get the search that finds the track.
- **`Next:`** After human output, a `Next:` line on stderr suggests what usually follows, with the
  real URIs and ids just printed: after a search, the first hit's play, lyrics, queue add and
  track commands; after `status`, lyrics, details and a trigger for the time left. It appears
  only when stdout is a terminal, so it never lands ahead of what a pipe prints
  (`spotify search x | head` shows no hint). `--json` never gets it, `SPOTIFY_HINTS=0` turns it
  off, and `SPOTIFY_HINTS=always` prints it even when stdout is not a terminal.
- **Tab completion.** `spotify completions <zsh|bash|fish|powershell>` prints the script. Install
  it once with the lines for your shell, as they are, then open a new shell (after
  `spotify update`, run the line that writes the script again so new commands complete):

```sh
# zsh
mkdir -p ~/.zfunc && spotify completions zsh > ~/.zfunc/_spotify
echo 'fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc
# bash (no bash-completion package needed; macOS's own bash 3.2 works)
mkdir -p ~/.bash_completion.d && spotify completions bash > ~/.bash_completion.d/spotify
echo 'source ~/.bash_completion.d/spotify' >> ~/.bashrc
# fish (it loads the file by itself)
mkdir -p ~/.config/fish/completions
spotify completions fish > ~/.config/fish/completions/spotify.fish
# PowerShell
spotify completions powershell >> $PROFILE
```

With Oh My Zsh, which runs `compinit` itself, write the zsh script to
`~/.oh-my-zsh/completions/_spotify` instead of both zsh lines. macOS Terminal starts bash as a
login shell, which reads `~/.bash_profile`: add the `source` line there unless that file sources
`~/.bashrc`. To try completion in the current shell only: `source <(spotify completions zsh)`
after `compinit` in zsh, `. <(spotify completions bash)` in bash 4 or later, or
`spotify completions fish | source` in fish. A new shell means a normal one (a new terminal tab,
or `exec zsh`): `zsh -f` skips `~/.zshrc` and `bash --norc` skips `~/.bashrc`, so completion is
not loaded there; load it without a new shell with `source ~/.zshrc` (or `source ~/.bashrc`). `spotify completions --help` shows the same lines,
and `--json` gives `{shell, script, install}`.

## 3. Play music

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
spotify lyrics                          # lyrics of the song playing now
spotify lyrics spotify:track:0BxE4FqsDD1Ot4YuBXwAPp   # of any song; nothing has to play
spotify search 'bohemian rhapsody' --type track       # only its name? its URI, for lyrics <uri>
spotify search 'lofi' --type playlist --limit 5
spotify queue add spotify:track:<id>    # appends to the managed queue; --next puts it first
spotify queue                           # lists it (also queue list, queue show)
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
(`via: web_api`, `spotify_player` or `applescript`) and, when the first path failed, why
(`fallback.reason`). Every start goes through the Spotify Web API first, so Spotify.app stays in
the background: songs, episodes, shows and Liked Songs directly, albums, playlists, artists and
radios through spotify_player. A song or episode starts inside its album or show (or
`--context`), so the rest of the album plays on afterwards. A start plays where Spotify.app
plays: on this Mac, or on the speaker or phone Spotify.app controls (then `--json` says so in
`note`). When the Web API cannot do it (no Premium, a rate limit, no device it can reach)
AppleScript starts it, a song still inside its album, and if Spotify.app comes to the front the
focus goes back to the app you were in; a radio has no AppleScript way. `play --liked` plays Liked
Songs in list order from its first song, `--random` (or `--shuffle`) shuffled from a random song.
Only exact seeks go to AppleScript first; `previous` restarts the item from 3 s in
(`result: restarted`). See `spotify docs playback`.

**Lyrics of any song.** `spotify lyrics` shows the song playing now; give it a track URI, an
`https://open.spotify.com/track/…` link or a bare id and it shows that song's lyrics, whether or
not anything plays, and without Spotify.app running (it needs the daemon and spotify_player
signed in). It takes a track, not search words: to find a song by name, run
`spotify search '<words>' --type track`, then `spotify lyrics <uri>` with the URI it shows (the
search's `Next:` line suggests that). Words instead of a track get that advice: a single word
that is no id is `invalid_input` ("`<word>` is not a track URI, link or id.", with the search to
run in `details.search`), and several words are a usage error (exit 2); both hints name
`spotify search '<words>' --type track`. `--json` gives `{track, title, lines, text, synced}`
(`synced` is false: the lines carry no times). Many songs have no lyrics (`not_found`), and
podcast episodes have none.

**Spotify references.** Anything that takes an item accepts `spotify:<kind>:<id>`, a link
`https://open.spotify.com/<kind>/<id>`, or a bare id (add `--type` where the kind is not implied).
An id is exactly 22 letters and digits. Anything else, or a reference of the wrong kind (an album
given to a playlist command), fails with `invalid_input` before anything is sent to Spotify; a
well-formed id that does not exist is `not_found`. `playlist add` and `playlist remove` take
tracks and albums; episodes are `unsupported` (edit those in the Spotify app).

**Search, details and library.** `spotify search --limit` takes 1 to 10 results per kind
(default: the `search_limit` setting, else 10), because spotify_player returns at most 10;
`spotify podcast search --limit` too (default 10). Every hit has a `uri` for play, queue add,
playlist add, track and lyrics. The `Next:` line on stderr fills in the first hit: play, lyrics,
queue add and track for a song; play and track for an album; `playlist play` and
`playlist show` for a playlist; play, track and `play --radio` for an artist; `podcast play` for
a show or episode. When spotify_player's search fails (it cannot read some of Spotify's answers),
the daemon asks the Web API's own search instead; the `--json` result says which answered
(`via: spotify_player` or `web_api`, plus `fallback` with spotify_player's error).

`spotify track` describes the song playing now (for a podcast episode: `show:` instead of
`album:`). A song's album line carries the album's URI (`album: AM · spotify:album:<id>`), so
`spotify track spotify:track:<id>` shows which album any song is on without playing anything, and
`spotify track <album-uri>` then lists that album. In `--json` a looked-up song has
`item.album` and `item.album_uri`; the song playing now has `album` (`{id, name, …}`). For a
song, now or by `spotify:track:` URI, it adds `liked: yes|no`: whether the song is
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
its `id`, `uri` and `name`. Without `--name` the copy keeps the source's name, description,
visibility and collaborative setting, so a fork of a public playlist (such as one of Spotify's)
is public too. With `--name 'Focus (mine)'` it is private and not collaborative, with the
source's description. Forks are imports: `spotify playlist sync` adds the tracks the source
gained since, and with `--delete` also removes those it dropped. Playlists cannot be renamed
(`spotify playlist rename` explains).

## 4. For Silicons: log in once, then set triggers

Triggers notify *you* (the Silicon that set them) through Ting, so they need your identity. The
SLT comes from `iam`, Silicon IAM's own CLI, separate from spotify-cli (details in
`spotify docs auth`):

```sh
cargo install silicon-iam-cli                        # once: the iam CLI
spotify iam --json                                   # app_id: spotify
iam silicon-login --app-id spotify --grant-org "$SILICON_ORG" --approve-scopes   # prints an SLT
spotify login '<SLT>'                                # one account and organization
spotify ting authorize                              # review separate notification consent in IAM
spotify ting complete REQUEST_ID --code-file /secure/consent-code
spotify login status --json                          # {"authenticated": true, ...}
```

`iam` mints for the Silicon whose `SILICON_HOME` it runs with. In a fresh `SILICON_HOME` it has no
session yet and refuses to mint: sign that Silicon in to IAM once with its own credential first,
`iam silicon-login --sid si:<handle>` (it asks for the Silicon's STK), then mint as above
(`spotify docs auth`). Stemcell does these steps for you when `spotify` is in your `apps`.

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

Everything else a Silicon needs to plan a call is in `spotify commands --json`
([The command manifest](#the-command-manifest)), and `spotify how '<task>' --json` finds the
command for a task.

## 5. When something fails

Errors say what failed, why, and the exact next step:

```text
error: spotify_player is not signed in to Spotify, so Web API features … cannot run.
hint: Run `spotify auth login`: it opens Spotify's consent page in the browser on this Mac…
(code: spotify_auth_required)
```

With `--json` the same error is one object on stderr: `{"error":{"code","message","hint","retryable","details"}}`.
Exit codes: 0 ok, 1 failed, 2 usage, 3 not signed in, 4 refused, 5 unavailable. All codes:
`spotify docs errors`; ask about one in plain words with `spotify how 'premium required'`, which
answers with the code's meaning and fix.

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

## The command manifest

`spotify commands --json` describes the whole CLI in one JSON value, offline and before any
login, so a Silicon can pick a command, build its arguments and know what can go wrong without
reading help text. `spotify commands` (without `--json`) prints the same tree for people,
grouped by goal, with `✎ changes <what>` after every command that changes something.

At the top level:

| Field | Holds |
| --- | --- |
| `version` | the CLI's version |
| `commands[]` | every command and subcommand, including the root (`path: ""`) |
| `goals[]` | `{id, title, commands}`: the goal groups of `spotify --help` |
| `errors[]` | `{code, exit, meaning, fix, section}` for every code in `spotify docs errors` |
| `exit_codes[]` | `{exit, meaning}` |
| `requirements` | what each requirement flag below means |
| `changes[]` | `{change, description}`: what a command can change (`playback`, `library`, `playlists`, `config`, `triggers`, `daemon`, `session`). `library` is your Liked Songs, changed only by `like` and `unlike`; playlists are `playlists`; saved albums, followed artists and shows are only read |
| `mutates` | a note on how to read `mutates`, `changes`, `read_only_when` and `has_read_only_form` |
| `argument_types[]` | `{type, description}`: the forms each argument type accepts (`spotify_ref`, `time_or_percent`, `duration`, `json_object`, …) |
| `output_contract` | the shape of `--json` output and of errors |

Each command has:

| Field | Holds |
| --- | --- |
| `path`, `command` | `"queue add"` and `"spotify queue add"` |
| `summary`, `description`, `details` | what it does in one line, then the long help (`details`, or null) |
| `usage` | e.g. `Usage: spotify queue add [OPTIONS] [ITEMS]...` |
| `goal`, `goal_title` | its goal group (`lyrics_and_details`, "Lyrics & details") |
| `capabilities`, `keywords` | what it can do, and the words people use for it (what `spotify how` matches) |
| `aliases`, `subcommands`, `group` | other names, its subcommands, and whether it has any |
| `arguments[]` | per argument: `name`, `flag` (`--limit`), `positional`, `required`, `takes_value`, `value_name`, `multiple`, `type` (one of `argument_types`), `allowed_values`, `default`, `min`, `max`, `conflicts_with`, `requires`, `description` (and, as before, `long`, `short`, `global`, `possible_values`) |
| `arg_groups[]` | `{id, args, rule, required}`: e.g. `trigger add` takes exactly one of `--remaining`, `--elapsed`, `--at`, `--end`, `--change` |
| `examples[]` | `{command, description}`: the runnable lines of its help |
| `output[]` | `{field, description}`: the fields of its `--json` output |
| `errors[]` | `{code, exit}`: the codes it can return, including those its requirements imply |
| `requirements` | true or false for `macos`, `daemon`, `spotify_app`, `spotify_player_signed_in`, `iam_login`, `premium`, `controls_playback`, `network` |
| `requirement_notes[]` | exceptions, e.g. for `lyrics`: with a target, Spotify.app is not needed |
| `mutates` | true when running it can change something or send something out (`report`); false for commands that only read |
| `changes[]` | what it can change, from the top-level `changes` (`["playback"]` for `play`, `[]` for `status`) |
| `read_only_when` | the form that only reads, or null: `volume` "without a level", `search` "without --play", `setup` and `update` "with --check" |
| `has_read_only_form` | true when it can run without changing anything: `mutates` is false or `read_only_when` is set |
| `help` | the command that prints its help |

```sh
spotify commands --json | jq '.commands[] | select(.path == "lyrics")'
spotify commands --json | jq -r '.commands[] | select(.requirements.iam_login) | .command'
spotify commands --json | jq '.errors[] | select(.code == "premium_required")'
spotify commands --json | jq -r '.commands[] | select(.mutates == false).command'   # only read
spotify commands --json | jq -r '.commands[] | select(.mutates == false or .read_only_when != null).command'
spotify commands --json | jq -r '.commands[] | select(.has_read_only_form).command'   # the same
spotify commands --json | jq -r '.commands[] | select(.changes | index("library")).command'
```

The second and third list every command that can run without changing anything: those that only
read, and those with a form that only reads (`volume` without a level, `search` without
`--play`, `setup` and `update` with `--check`). The last one prints `spotify like` and
`spotify unlike`. `spotify commands` marks the same in its list: a trailing `✎ changes library`
(with `; reads only …` where a form only reads) on every command that changes something.

Fields are only added within a major version (`spotify docs versioning`).

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
  apps may act for you and with which scopes. Its own CLI, `iam`, is a separate program from
  spotify-cli: `cargo install silicon-iam-cli` (crate `silicon-iam-cli`, binary `iam`;
  `spotify iam --json` names it under `iam_cli`). The service runs at
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

Every guide is bundled in the CLI (`spotify docs <topic>`, one section with
`spotify docs <topic> --section '<heading>'`, all of them with `spotify docs --search <text>`)
and published at https://spotify.unlikefraction.com/docs, where `/` jumps to the search box.

| You want to… | Read |
| --- | --- |
| Find the command for something | `spotify how "<what you want to do>"` |
| Understand triggers and their exact semantics | `spotify docs triggers` |
| Know how play/pause/seek decide between the Web API, spotify_player and AppleScript | `spotify docs playback` |
| Queue, reorder and remove upcoming songs | `spotify docs queue` |
| Log in, Spotify sign-in, testing planes | `spotify docs auth` |
| Change settings, bring your own Spotify client id | `spotify docs config` |
| Know what runs in the background and where state lives | `spotify docs daemon` |
| Look up an error code and its exit code | `spotify docs errors` |
| Build on top (library, daemon protocol, backend API) | `spotify docs development`, `spotify docs api` |
| Read every command, argument, output field and error as JSON | `spotify commands --json` ([The command manifest](#the-command-manifest)) |
