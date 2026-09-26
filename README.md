# spotify-cli

Spotify from the command line, for Carbons and Silicons on macOS, with playback triggers
delivered through Ting.

```sh
curl -fsSL https://spotify.unlikefraction.com/install.sh | sh
spotify doctor
spotify play --search 'arctic monkeys 505'
spotify trigger add --remaining 30s --note 'wrap up'
```

- **Every control:** play (URIs, links, search, Liked Songs, radio), pause, next/previous, seek,
  volume, shuffle, repeat, like, track details, lyrics, search, library, devices, playlists
  (create/delete/add/remove/import/fork/sync), podcasts, and a managed queue you can reorder.
- **Verified, not assumed:** spotify_player first, checked against Spotify.app, AppleScript
  fallback; every result says which path worked and why.
- **Triggers:** `--remaining 30s|25%`, `--elapsed 50%|1:30`, `--end`, `--change`, for the current
  song, every song or one track; durable, retried, idempotent Ting delivery with your note and ISI.
- **Agent-grade CLI:** `--json` everywhere, one error shape with exact fixes, a documented command
  tree (`spotify commands --json`), offline guides (`spotify docs`), `spotify report --pr`.

| Component | Crate / path | Binary |
| --- | --- | --- |
| Stateless library | `crates/client` · [`silicon-spotify-client`](https://github.com/unlikefraction/spotify-cli/tree/main/crates/client) | — |
| CLI | `crates/cli` · `silicon-spotify-cli` | `spotify` |
| Daemon | `crates/daemon` | `spotify-daemon` |
| Backend | `src/` · `silicon-spotify` | `spotify-api` |
| Website + docs | `docs-site/`, `docs/` | — |

Docs: https://spotify.unlikefraction.com/docs (or `spotify docs`). Start with
[`docs/usage.md`](docs/usage.md); architecture and decisions: [`ARCHITECTURE.md`](ARCHITECTURE.md);
going live: [`deploy/README.md`](deploy/README.md).

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
