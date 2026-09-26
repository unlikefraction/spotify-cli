# Building on spotify-cli

Everything is open source: https://github.com/unlikefraction/spotify-cli. Reproduce a bug,
patch it, open a pull request, then `spotify report '<what>' --pr <url>`.

## Layout

```text
crates/client   silicon-spotify-client   stateless library: models, URI/time parsing, AppleScript,
                                         spotify_player wrapper, verified control, trigger engine,
                                         backend API client; feature `runtime` adds the per-home
                                         store and daemon IPC used by the CLI and daemon
crates/cli      silicon-spotify-cli      the `spotify` command
crates/daemon   silicon-spotify-daemon   `spotify-daemon` (macOS: objc2 NSAppleScript + notifications)
src/            silicon-spotify          backend (`spotify-api`): IAM exchange, Ting, reports, telemetry
docs/           these guides (bundled into the CLI by scripts/sync-cli-docs.sh)
docs-site/      static site generator for spotify.unlikefraction.com (+ install.sh)
deploy/         backend deployment (systemd, Caddy, CloudFormation), Honeycomb and Ting setup
```

## Use the library

```toml
silicon-spotify-client = { git = "https://github.com/unlikefraction/spotify-cli", tag = "v0.1.2" }
# no HTTP at all:
silicon-spotify-client = { git = "https://github.com/unlikefraction/spotify-cli", tag = "v0.1.2", default-features = false }
```

```rust
use std::time::Duration;
use silicon_spotify_client::{applescript::Osascript, control::*, player::SpotifyPlayer, timing::SeekTarget};

let script = Osascript::default();                    // or your own `applescript::Runner`
let player = SpotifyPlayer::locate(None);
let spotify = Controller {
    script: &script,
    player: player.as_ref().map_err(Clone::clone),
    strategy: Strategy::Auto,
    verify_timeout: Duration::from_millis(2500),
    launch_spotify: false,
};
let now = spotify.status()?;                          // Playback
let outcome = spotify.seek(SeekTarget::parse("50%")?)?;   // Outcome { via, fallback, playback }
```

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
The socket accepts only the same OS user.

## Build and test

```sh
cargo test --workspace                        # unit + backend contract tests (fake IAM and Ting)
cargo run --example dev_backend --features dev -- 127.0.0.1:8787 /tmp/tings.jsonl
SPOTIFY_API_URL=http://127.0.0.1:8787 cargo run -p silicon-spotify-cli -- login si:dev-silicon
SPOTIFY_API_URL=http://127.0.0.1:8787 cargo run -p silicon-spotify-cli -- trigger test
```

The dev backend serves the real router with in-process IAM and Ting fakes and appends every
"sent" Ting to the JSONL file, so the whole path (CLI → daemon → session → backend → Ting) can be
exercised on a Mac without an IAM application secret. Release artifacts: `scripts/package-release.sh`.

## Conventions

- Library: stateless; never caches credentials, never retries or updates behind the caller.
- Errors: `{code, message, hint, retryable, details}` everywhere; add new codes to `docs/errors.md`.
- Every playback command verifies its effect and reports `via`/`fallback`.
- Never send lyrics, titles, queries, notes or tokens to telemetry or logs.
- Keep `crates/*` versions equal; the CLI restarts an older daemon automatically.
