# silicon-spotify-client

Stateless Rust building blocks for [spotify-cli](https://spotify.unlikefraction.com): control the Spotify
desktop app on macOS through `spotify_player` and AppleScript (with verification and fallback), parse
Spotify ids and times, run the playback-trigger engine, and call the spotify-cli backend.

```toml
silicon-spotify-client = { git = "https://github.com/unlikefraction/spotify-cli", tag = "v0.1.0" }
```

```rust
use silicon_spotify_client::{applescript::Osascript, control::{Controller, Strategy}, player::SpotifyPlayer};

let script = Osascript::default();
let player = SpotifyPlayer::locate(None);
let controller = Controller {
    script: &script,
    player: player.as_ref().map_err(Clone::clone),
    strategy: Strategy::Auto,
    verify_timeout: std::time::Duration::from_millis(2500),
    launch_spotify: false,
};
let now = controller.status()?;
```

Docs: https://spotify.unlikefraction.com/docs · Source: https://github.com/unlikefraction/spotify-cli
