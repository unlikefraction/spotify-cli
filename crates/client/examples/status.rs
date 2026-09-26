//! Prints Spotify.app's state through AppleScript: `cargo run -p silicon-spotify-client --example status`.

use silicon_spotify_client::applescript::{Osascript, Runner as _, parse_status, status};

fn main() {
    let started = std::time::Instant::now();
    match Osascript::default()
        .run(&status())
        .and_then(|out| parse_status(&out))
    {
        Ok(playback) => println!(
            "{}",
            serde_json::to_string_pretty(&playback).unwrap_or_default()
        ),
        Err(error) => eprintln!(
            "{}",
            serde_json::to_string_pretty(&error).unwrap_or_default()
        ),
    }
    eprintln!("took {:?}", started.elapsed());
}
