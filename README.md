# spotify-cli

Spotify from the command line, for Carbons and Silicons on macOS, with playback triggers
delivered through Ting. New to Carbons, Silicons, IAM, SLTs or Ting? See
[Concepts](docs/usage.md#concepts).

```sh
curl -fsSL https://spotify.unlikefraction.com/install.sh | sh
spotify doctor
spotify play --search 'arctic monkeys 505'
spotify lyrics spotify:track:0BxE4FqsDD1Ot4YuBXwAPp     # any song's lyrics; nothing has to play
spotify trigger add --remaining 30s --note 'wrap up'
```

Installer options, where state lives and how to uninstall:
[Install options](docs/usage.md#install-options).

- **Every control:** play (URIs, links, search, Liked Songs, radio), pause, next/previous, seek,
  volume, shuffle, repeat, like, track details, search, library, devices, playlists
  (create/delete/add/remove/import/fork/sync), podcasts, and a managed queue you can reorder.
- **Lyrics of any song:** `spotify lyrics <uri|link|id>` for any track, whether or not anything
  plays; `spotify lyrics` alone for the song playing now. Only know its name? Find the track with
  `spotify search '<words>' --type track`, then run `spotify lyrics <uri>`.
- **Verified, not assumed, and out of your way:** every start goes through the Spotify Web API
  first, so Spotify.app stays in the background: songs, episodes, shows and Liked Songs directly
  (a song inside its album, so the album plays on), albums, playlists, artists and radios through
  spotify_player, which also runs the other controls. A start plays where Spotify.app plays: on
  this Mac, or on the speaker or phone it controls. AppleScript is the fallback (it goes first
  only for exact seeks), and when it brings Spotify.app to the front the focus goes back to your
  app. Every effect is checked against Spotify.app, and every result says which path worked and
  why.
- **Triggers:** `--remaining 30s` or `25%`, `--elapsed 50%` or `1:30`, `--end`, `--change`, for
  the current song, every song or one track; durable, retried, idempotent Ting delivery with your
  note and ISI.
- **Agent-grade CLI:** `--json` everywhere, one error shape with exact fixes, a machine-readable
  manifest of every command, argument, output field, error and requirement
  (`spotify commands --json`), offline guides (`spotify docs`), `spotify report --pr`.

Docs: https://spotify.unlikefraction.com/docs (or `spotify docs`). Start with
[`docs/usage.md`](docs/usage.md); architecture and decisions: [`ARCHITECTURE.md`](ARCHITECTURE.md);
going live: [`deploy/README.md`](deploy/README.md).

## Find anything

```sh
spotify                                            # set up? missing? what to try
spotify --help                                     # every command by goal, with examples
spotify how "lyrics of a song that isn't playing"  # ask in plain words, offline
spotify lyrics --help                              # any command: its flags and examples
spotify commands --json                            # the whole CLI as JSON, for Silicons
```

After human output in a terminal, a `Next:` line suggests the likely next commands with the real
URIs filled in (never when output is piped or with `--json`; `SPOTIFY_HINTS=0` turns it off,
`SPOTIFY_HINTS=always` keeps it when piped). Tab completion:

```sh
# zsh (Oh My Zsh: write the script to ~/.oh-my-zsh/completions/_spotify instead of both lines)
mkdir -p ~/.zfunc && spotify completions zsh > ~/.zfunc/_spotify
echo 'fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc
# bash, also macOS's bash 3.2 (login shells read ~/.bash_profile: add the line there unless it sources ~/.bashrc)
mkdir -p ~/.bash_completion.d && spotify completions bash > ~/.bash_completion.d/spotify
echo 'source ~/.bash_completion.d/spotify' >> ~/.bashrc
# fish
mkdir -p ~/.config/fish/completions
spotify completions fish > ~/.config/fish/completions/spotify.fish
```

Then open a new shell. `spotify completions --help` shows the same lines, and PowerShell's.

More: [Find anything](docs/usage.md#2-find-anything).

## Components

| Component | Crate / path | Binary |
| --- | --- | --- |
| Stateless library | `crates/client` · [`silicon-spotify-client`](https://github.com/unlikefraction/spotify-cli/tree/main/crates/client) | — |
| CLI | `crates/cli` · `silicon-spotify-cli` | `spotify` |
| Daemon | `crates/daemon` | `spotify-daemon` |
| Backend | `src/` · `silicon-spotify` | `spotify-api` |
| Website + docs | `docs-site/`, `docs/` | — |

## Develop

```sh
cargo test --workspace
cargo run --example dev_backend --features dev -- 127.0.0.1:8787 /tmp/tings.jsonl   # fake IAM + Ting
SPOTIFY_API_URL=http://127.0.0.1:8787 cargo run -p silicon-spotify-cli -- login si:dev
SPOTIFY_API_URL=http://127.0.0.1:8787 cargo run -p silicon-spotify-cli -- trigger test
scripts/package-release.sh      # all six targets + Honeycomb package
```

Found a bug? Patch it, open a PR, then `spotify report '<what>' --pr <url>`.

License: MIT.
