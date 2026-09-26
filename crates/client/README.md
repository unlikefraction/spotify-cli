# silicon-spotify-client

Stateless Rust building blocks for [spotify-cli](https://spotify.unlikefraction.com): control the Spotify
desktop app on macOS through `spotify_player`, the Spotify Web API and AppleScript (with verification
and fallback), start songs, episodes, shows and Liked Songs without bringing Spotify.app to the front
(on this Mac, or on the speaker or phone it controls), parse Spotify ids and times, run the
playback-trigger engine, and call the spotify-cli backend.

```toml
silicon-spotify-client = { git = "https://github.com/unlikefraction/spotify-cli", tag = "v0.1.6" }
```

```rust
use silicon_spotify_client::{applescript::Osascript, control::{Controller, Strategy}, focus::{Focus, LaunchServices}, player::SpotifyPlayer, webapi::{HttpWebApi, WebApi}};

let script = Osascript::default();
let player = SpotifyPlayer::locate(None);
let web = player.as_ref().map(HttpWebApi::new).map_err(Clone::clone);
let controller = Controller {
    script: &script,
    player: player.as_ref().map_err(Clone::clone),
    // Starts songs, episodes, shows and Liked Songs through the Web API with spotify_player's token.
    web: web.as_ref().map(|w| w as &dyn WebApi).map_err(Clone::clone),
    // Gives the focus back when an AppleScript start brings Spotify.app forward.
    focus: Some(&LaunchServices as &dyn Focus),
    strategy: Strategy::Auto,
    verify_timeout: std::time::Duration::from_millis(2500),
    launch_spotify: false,
};
let now = controller.status()?;
```

`web` and `focus` are new in 0.1.6: code that builds a `Controller` from 0.1.5 adds them. To keep
the 0.1.5 behaviour (AppleScript starts songs, episodes, shows and Liked Songs; the focus stays
where it lands), pass `web: Err(silicon_spotify_client::Error::unsupported("No Web API.", ""))` and
`focus: None`. Without default features there is no `HttpWebApi`; implement `webapi::WebApi`
yourself or pass an `Err`. `Outcome` gains `refocused` and `note` (a start that went to the speaker
Spotify.app controls, or counted although the Web API's answer was lost).

Try it against the real Spotify.app without the daemon:
`cargo run -p silicon-spotify-client --example control -- status` (also `play <uri> [context]`,
`liked [random]`, `seek 1:30`, `previous`, `repeat track`, `like`, `front`, `lookup <uri>`,
`webstate`, `devices`, `websearch <query>`, `raw <GET|PUT> <path>`, …; `play` takes `--no-web`,
`--no-refocus` and `--trace`).

Docs: https://spotify.unlikefraction.com/docs · Source: https://github.com/unlikefraction/spotify-cli
