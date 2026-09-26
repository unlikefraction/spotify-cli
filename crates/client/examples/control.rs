//! Runs one playback command through [`Controller`] directly, without the daemon, to try control
//! code live: `cargo run -p silicon-spotify-client --example control -- <command> [args]
//! [--strategy auto|spotify_player|applescript]`.
//!
//! Commands: `status`, `full`, `play [uri [context]]`, `liked [random]`, `pause`, `toggle`,
//! `next`, `previous`, `seek <time>`, `volume <0-100>`, `shuffle <on|off>`,
//! `repeat <off|context|track>`, `like`, `unlike`. Prints the outcome (or error) as JSON and the
//! time it took on stderr.

use std::time::{Duration, Instant};

use silicon_spotify_client::Error;
use silicon_spotify_client::applescript::Osascript;
use silicon_spotify_client::control::{Controller, PlayTarget, RepeatMode, Strategy, VolumeTarget};
use silicon_spotify_client::player::SpotifyPlayer;
use silicon_spotify_client::timing::SeekTarget;
use silicon_spotify_client::uri::SpotifyUri;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut strategy = Strategy::Auto;
    if let Some(index) = args.iter().position(|a| a == "--strategy") {
        let value = args.get(index + 1).cloned().unwrap_or_default();
        strategy = value.parse().unwrap_or_else(|error: Error| exit(&error));
        args.drain(index..=index + 1);
    }
    let script = Osascript::default();
    let player = SpotifyPlayer::locate(None);
    let controller = Controller {
        script: &script,
        player: player.as_ref().map_err(Clone::clone),
        strategy,
        verify_timeout: Duration::from_millis(2500),
        launch_spotify: false,
    };
    let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
    let started = Instant::now();
    let result = match arg(0) {
        "status" => controller.status().and_then(to_json),
        "full" => controller.status_full().map(
            |(playback, warnings)| serde_json::json!({"playback": playback, "warnings": warnings}),
        ),
        "play" if args.len() > 1 => SpotifyUri::parse(arg(1), None)
            .and_then(|uri| {
                let context = match args.get(2) {
                    Some(context) => Some(SpotifyUri::parse(context, None)?),
                    None => None,
                };
                controller.play(&PlayTarget::Uri {
                    uri,
                    context,
                    shuffle: false,
                })
            })
            .and_then(to_json),
        "play" => controller.play(&PlayTarget::Resume).and_then(to_json),
        "liked" => controller
            .play(&PlayTarget::Liked {
                limit: 50,
                random: arg(1) == "random",
            })
            .and_then(to_json),
        "pause" => controller.pause().and_then(to_json),
        "toggle" => controller.toggle().and_then(to_json),
        "next" => controller.next().and_then(to_json),
        "previous" => controller.previous().and_then(to_json),
        "seek" => SeekTarget::parse(arg(1))
            .and_then(|target| controller.seek(target))
            .and_then(to_json),
        "volume" => arg(1)
            .parse::<u8>()
            .map_err(|_| Error::invalid("volume needs 0-100", ""))
            .and_then(|level| controller.volume(VolumeTarget::Absolute(level)))
            .and_then(to_json),
        "shuffle" => controller.shuffle(Some(arg(1) == "on")).and_then(to_json),
        "repeat" => arg(1)
            .parse::<RepeatMode>()
            .and_then(|mode| controller.repeat(mode))
            .and_then(to_json),
        "like" => controller.like(true).and_then(to_json),
        "unlike" => controller.like(false).and_then(to_json),
        other => Err(Error::invalid(format!("unknown command `{other}`"), "")),
    };
    eprintln!("took {:.2} s", started.elapsed().as_secs_f64());
    match result {
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        ),
        Err(error) => exit(&error),
    }
}

fn to_json<T: serde::Serialize>(value: T) -> silicon_spotify_client::Result<serde_json::Value> {
    Ok(serde_json::to_value(value)?)
}

fn exit(error: &Error) -> ! {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"error": error})).unwrap_or_default()
    );
    std::process::exit(1)
}
