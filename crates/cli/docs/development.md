# Building on spotify-cli

Everything is open source: https://github.com/unlikefraction/spotify-cli. Reproduce a bug,
patch it, open a pull request, then `spotify report '<what>' --pr <url>`.

## Layout

```text
crates/client   silicon-spotify-client   stateless library: models, URI/time parsing, AppleScript,
                                         spotify_player wrapper, Web API starts, focus hand-back,
                                         verified control, trigger engine, backend API client;
                                         feature `runtime` adds the per-home store and daemon IPC
                                         used by the CLI and daemon
crates/cli      silicon-spotify-cli      the `spotify` command
crates/daemon   silicon-spotify-daemon   `spotify-daemon` (macOS: objc2 NSAppleScript + notifications)
src/            silicon-spotify          backend (`spotify-api`): IAM exchange, Ting, reports, telemetry
docs/           these guides (bundled into the CLI by scripts/sync-cli-docs.sh)
docs-site/      static site generator for spotify.unlikefraction.com (+ install.sh)
deploy/         backend deployment (systemd, Caddy, CloudFormation), Honeycomb and Ting setup
```

## Use the library

```toml
silicon-spotify-client = { git = "https://github.com/unlikefraction/spotify-cli", tag = "v0.1.7" }
# no HTTP at all:
silicon-spotify-client = { git = "https://github.com/unlikefraction/spotify-cli", tag = "v0.1.7", default-features = false }
```

```rust
use std::time::Duration;
use silicon_spotify_client::{applescript::Osascript, control::*, focus::{Focus, LaunchServices}, player::SpotifyPlayer, timing::SeekTarget, webapi::{HttpWebApi, WebApi}};

let script = Osascript::default();                    // or your own `applescript::Runner`
let player = SpotifyPlayer::locate(None);
let web = player.as_ref().map(HttpWebApi::new).map_err(Clone::clone);   // feature `api`
let spotify = Controller {
    script: &script,
    player: player.as_ref().map_err(Clone::clone),
    web: web.as_ref().map(|w| w as &dyn WebApi).map_err(Clone::clone),
    focus: Some(&LaunchServices as &dyn Focus),       // None: leave the focus where it lands
    strategy: Strategy::Auto,
    verify_timeout: Duration::from_millis(2500),
    launch_spotify: false,
};
let now = spotify.status()?;                          // Playback
let outcome = spotify.seek(SeekTarget::parse("50%")?)?;   // Outcome { via, fallback, result, playback, refocused, note }
```

`Outcome::result` says what an action did when it can do more than one thing (`previous`:
`restarted` or `previous_item`); `Outcome::refocused` says where the focus went back after an
AppleScript start brought Spotify.app forward; `Outcome::note` says when a start went to the
speaker or phone Spotify.app controls, or counted although the Web API's answer was lost (a 5xx
or no answer, and Spotify.app plays it). The rules the controller follows (every start through
the Web API first: songs, episodes, shows and Liked Songs directly, albums, playlists, artists
and radios through spotify_player; AppleScript first only for exact seeks; spotify_player's view
checked before commands that depend on it; refusals read from its log; restoring what a failed
start emptied) are in `spotify docs playback`.

`web` and `focus` are new in 0.1.6 (a `Controller` built for 0.1.5 needs them added).
`web: Err(error)` makes AppleScript start songs, episodes, shows and Liked Songs, as before, and
the controller then reports `fallback: {from: "web_api", reason: error}`; `focus: None` leaves
the focus where it lands. `webapi::WebApi` needs one method (`send`), so tests and other HTTP
stacks can supply their own. The `webapi` module has the steps without the verification:
`start` (a track or episode: item lookup, device, `PUT /me/player/play`, position retry),
`start_list` (Liked Songs or a show), `choose_device` and `target` (where a start goes:
the active speaker or phone Spotify.app controls, else Spotify.app on this Mac), `set_shuffle`,
`liked_songs`, `search`, `unanswered` (a start whose effect is unknown) and `cached_facts` (what a
lookup found, without asking again). The library remembers, for the life of the process, the
album or show of each item it looked up (up to 500), and keeps one process-wide pause after a
Web API 429 (`webapi::pause_left`). Devices are listed at every start.

Try control code live, without the daemon:

```sh
cargo run -p silicon-spotify-client --example control -- status
cargo run -p silicon-spotify-client --example control -- seek 1:30 --strategy spotify_player
cargo run -p silicon-spotify-client --example control -- play spotify:track:0BxE4FqsDD1Ot4YuBXwAPp --trace
cargo run -p silicon-spotify-client --example control -- play spotify:track:0BxE4FqsDD1Ot4YuBXwAPp --no-web
cargo run -p silicon-spotify-client --example control -- devices
```

It takes `status`, `full`, `play [uri [context]]`, `liked [random]`, `pause`, `toggle`, `next`,
`previous`, `seek <time>`, `volume <0-100>`, `shuffle <on|off>`, `repeat <off|context|track>`,
`like`, `unlike`, `front` (the frontmost app), `handoff <uri> [context]` (a bare AppleScript start
with the focus hand-back), `lookup <uri>` (the album or show a Web API start would use, the
release a relinked song plays from, its position),
`webstate` (a summary of the Web API's `GET /me/player`), `devices` (the Spotify Connect devices
and where a start would go now), `websearch <query>` (the Web API's search, as the daemon falls
back to it) and `raw <GET|PUT> <path> [key=value…] [json body]` (one Web API request, to see how
Spotify answers). Flags: `--strategy auto|spotify_player|applescript`, `--no-web` (the Web API's
playback commands, its PUTs, fail with `web_api_disabled` while lookups still answer, to try the
AppleScript fallback: a track then plays in its album), `--no-refocus` (no focus hand-back) and
`--trace` (method, path, status and time of each Web API request on stderr; never tokens). It
prints the outcome or error as JSON and the time it took on stderr, and it controls the real
Spotify.app.

The trigger engine is pure: feed `trigger::Tracker::observe` readings, pass the events to
`trigger::evaluate`, deliver the returned `Firing`s however you like (`Firing::data()` is the Ting
payload, `Firing::key()` the idempotency key). `trigger::validate_request` checks a request's
note, label, `times` and percentage without any playback state, so run it before reading Spotify.

`uri::SpotifyUri::parse` accepts only ids of exactly 22 base62 characters (`uri::ACCEPTED_FORMS`
lists the forms), so malformed references fail as `invalid_input` before they reach Spotify.
`player::parse_json` reads spotify_player's JSON even when strings hold raw control characters,
and `model::item_view` turns its `get item` output for albums and artists into `kind`, `item` and
per-kind lists. spotify_player returns at most `player::SEARCH_MAX_PER_KIND` (10) search results
per kind.

## Talk to the daemon

Newline-delimited JSON over `~/.silicon-spotify/daemon.sock`, one request per connection:

```json
{"v":1,"id":"<uuid>","op":"player.status","home":"/abs/SILICON_HOME","args":{"full":true},"client_version":"0.1.0"}
{"v":1,"id":"<uuid>","ok":true,"data":{…}}
{"v":1,"id":"<uuid>","ok":false,"error":{"code":"…","message":"…","hint":"…","retryable":false}}
```

Ops: `daemon.status`, `daemon.shutdown`, `doctor`, `player.{status,play,pause,toggle,next,previous,seek,volume,shuffle,repeat,like}`,
`spotify.{launch,auth.status,auth.login}`, `track.info`, `lyrics`, `search`, `library.get`,
`devices.{list,connect}`, `queue.{list,add,remove,move,clear}`, `playlist.{list,show,create,delete,add,remove,rename,import,fork,sync}`,
`podcast.saved`, `trigger.{add,list,get,remove,clear,history,test,wait,retry}`,
`telemetry.{record,clear}`, `update.{check,apply}`. Argument shapes: `crates/cli/src/ops.rs` (the
CLI is the reference client) and `crates/daemon/src/*.rs`. Unknown ops fail with `unknown_op`.
The socket accepts only the same OS user. Some fields a client may rely on:

- `queue.add` takes `{"uris": […], "next": bool, "known": [{"uri", "name", "by", "duration_ms"}]}`;
  `known` (optional) holds facts the caller already has, such as the search hit it picked, so the
  daemon need not look them up; it looks up only the fields `known` lacks (empty strings count as
  missing). Older daemons ignore it. The answer `{"added", "queue"}` may carry
  `metadata_pending` (the `q_…` ids of added episodes whose facts the daemon still looks up in
  the background) and `note`.
- `track.info` adds `liked: true|false` for songs when the Liked Songs check answers in 2.5 s.
- `spotify.launch` answers `{"launched", "already_running", "playback"}`.
- `player.next` may carry `skipped` (a managed item whose hand-off was still in flight). Managed
  starts use the same controller as `player.play` and report its actual `via`, `playback`,
  `fallback`, `note` and `refocused` fields when present.
- `queue.list`'s `spotify_upcoming` may carry `current_repeats_left_out` and `note`. Its
  top-level `warnings` (on every reply, empty unless something is worth knowing) holds
  `no_active_device` when Spotify reports nothing playing on any device and Spotify.app does not
  play either, so the upcoming list is empty for that reason.
- `search` replies carry `via` (`spotify_player`, or `web_api` when spotify_player's search failed
  and the Web API's answered) and then `fallback: {from: "spotify_player", reason}`. When both
  fail, the error is the Web API's with spotify_player's in `details.first_attempt`.
- `player.play`'s outcome may say `via: "web_api"` (a song, episode, show or Liked Songs started
  through the Web API) or `fallback.from: "web_api"`, carries `note` when the start went to the
  speaker or phone Spotify.app controls or counted although the Web API's answer was lost, and
  carries `refocused` (`{app, via, hid_spotify, error}`, the last two only when set) when an
  AppleScript start brought Spotify.app forward. The settings the CLI merges into `args` include
  `keep_spotify_in_background` (default true); older daemons ignore it.
- `player.status` with `{"full": true}` may set `web.relinked: true` (the Web API plays
  Spotify.app's song under another id); `player.like` then refuses with a non-retryable
  `track_mismatch` whose `details` carry `relinked: true` and `matched_by`.

## Build and test

```sh
cargo test --workspace                        # unit + backend contract tests (fake IAM and Ting)
cargo run --example dev_backend --features dev -- 127.0.0.1:8787 /tmp/tings.jsonl
SPOTIFY_API_URL=http://127.0.0.1:8787 cargo run -p silicon-spotify-cli -- login si:dev-silicon
SPOTIFY_API_URL=http://127.0.0.1:8787 cargo run -p silicon-spotify-cli -- trigger test
```

The dev backend serves the real router with in-process IAM and Ting fakes and appends every
"sent" Ting to the JSONL file, so the whole path (CLI → daemon → session → backend → Ting) can be
exercised on a Mac without an IAM application secret.

Release artifacts: `scripts/package-release.sh` (all six targets). macOS binaries are ad-hoc
signed unless `SPOTIFY_CODESIGN_IDENTITY` is set; releases set it and the notary profile:

```sh
SPOTIFY_CODESIGN_IDENTITY="Developer ID Application: …" \
SPOTIFY_NOTARY_PROFILE=spotify-cli \
  scripts/package-release.sh     # Developer ID + hardened runtime, then Apple notarization
```

`scripts/macos-sign.sh <dir>` does the signing for one architecture; the release steps are in
`deploy/README.md`. On macOS, `crates/daemon/build.rs` embeds `crates/daemon/Info.plist` in
`spotify-daemon`; keep its `CFBundleIdentifier` equal to the codesign identifier.

## Conventions

- Library: stateless apart from the process-lifetime memory above (looked-up items, the Web
  API's rate-limit pause); never caches credentials, never updates behind the caller.
- Errors: `{code, message, hint, retryable, details}` everywhere; add new codes to `docs/errors.md`.
- Every playback command verifies its effect and reports `via`/`fallback`.
- Never send lyrics, titles, queries, notes or tokens to telemetry or logs.
- Keep `crates/*` versions equal; the CLI restarts an older daemon automatically.
