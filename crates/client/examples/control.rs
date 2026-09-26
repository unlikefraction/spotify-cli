//! Runs one playback command through [`Controller`] directly, without the daemon, to try control
//! code live: `cargo run -p silicon-spotify-client --example control -- <command> [args]
//! [--strategy auto|spotify_player|applescript] [--no-web] [--no-refocus] [--trace]`.
//!
//! Commands: `status`, `full`, `play [uri [context]]`, `liked [random]`, `pause`, `toggle`,
//! `next`, `previous`, `seek <time>`, `volume <0-100>`, `shuffle <on|off>`,
//! `repeat <off|context|track>`, `like`, `unlike`, `front` (the frontmost app), `handoff <uri>
//! [context]` (a bare AppleScript start with the focus hand-back, as the daemon's managed queue
//! makes one), `webstate` (the Web API's `GET /me/player`: item, context, device), `devices`
//! (the Spotify Connect devices and where a start would go now), `lookup <uri>` (the album or
//! show a Web API start would use, the id it lists the item under, and the release a relinked
//! song plays from; an item not playable here is `not_playable`), `websearch
//! <query>` (the Web API's search, as the daemon falls back to it), `raw <GET|PUT>
//! <path> [key=value…] [json body]` (one Web API request, to see how Spotify answers). Prints the
//! outcome (or error) as JSON and the time it took on stderr.
//!
//! `play <track|episode|show>` and `liked` start through the Spotify Web API first (Spotify.app
//! stays in the background). `--no-web` refuses the Web API's playback commands (lookups still
//! answer), to try the AppleScript fallback (a track then plays in its album) and its focus
//! hand-back; `--no-refocus` turns the hand-back off (config `keep_spotify_in_background`).
//! `--trace` prints each Web API request (method, path, status, time) on stderr.

use std::time::{Duration, Instant};

use silicon_spotify_client::Error;
use silicon_spotify_client::applescript::Osascript;
use silicon_spotify_client::control::{Controller, PlayTarget, RepeatMode, Strategy, VolumeTarget};
use silicon_spotify_client::focus::{Focus, LaunchServices};
use silicon_spotify_client::player::SpotifyPlayer;
use silicon_spotify_client::timing::SeekTarget;
use silicon_spotify_client::uri::SpotifyUri;
use silicon_spotify_client::webapi::WebApi;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut strategy = Strategy::Auto;
    if let Some(index) = args.iter().position(|a| a == "--strategy") {
        let value = args.get(index + 1).cloned().unwrap_or_default();
        strategy = value.parse().unwrap_or_else(|error: Error| exit(&error));
        args.drain(index..=index + 1);
    }
    let mut flag = |name: &str| {
        let found = args.iter().any(|a| a == name);
        args.retain(|a| a != name);
        found
    };
    let (no_web, no_refocus, trace) = (flag("--no-web"), flag("--no-refocus"), flag("--trace"));
    let started = Instant::now();
    let script = Osascript::default();
    let player = SpotifyPlayer::locate(None);
    let http = player
        .as_ref()
        .ok()
        .and_then(web_client)
        .map(|inner| -> Box<dyn WebApi> {
            if trace {
                Box::new(Traced { inner, started })
            } else {
                inner
            }
        })
        .map(|inner| -> Box<dyn WebApi> {
            if no_web {
                Box::new(NoPlayback { inner })
            } else {
                inner
            }
        });
    let web: Result<&dyn WebApi, Error> = match (&http, &player) {
        (Some(http), _) => Ok(http.as_ref()),
        (None, Err(error)) => Err(error.clone()),
        (None, Ok(_)) => Err(Error::unsupported(
            "This build has no Web API client (feature `api`).",
            "",
        )),
    };
    let desktop = LaunchServices;
    let controller = Controller {
        script: &script,
        player: player.as_ref().map_err(Clone::clone),
        web,
        focus: (!no_refocus).then_some(&desktop as &dyn Focus),
        strategy,
        verify_timeout: Duration::from_millis(2500),
        launch_spotify: false,
    };
    let arg = |index: usize| args.get(index).map(String::as_str).unwrap_or_default();
    let begun = Instant::now();
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
        "front" => to_json(desktop.frontmost()),
        // A bare AppleScript start, as the daemon's managed queue hands over: no Web API, no
        // verification, only the focus hand-back.
        "handoff" if args.len() > 1 => {
            let script = silicon_spotify_client::applescript::play_uri(
                arg(1),
                args.get(2).map(String::as_str),
            );
            let (result, refocused) =
                silicon_spotify_client::focus::keep_in_background(controller.focus, || {
                    controller
                        .script
                        .run(&script)
                        .and_then(|output| silicon_spotify_client::applescript::expect_ok(&output))
                });
            result.map(|()| serde_json::json!({"handed_over": arg(1), "refocused": refocused}))
        }
        "webstate" => controller
            .web
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|web| {
                web.send(
                    silicon_spotify_client::webapi::Method::Get,
                    "/me/player",
                    &[("additional_types", "episode")],
                    None,
                )
            })
            .map(|reply| {
                let b = &reply.body;
                serde_json::json!({
                    "status": reply.status,
                    "item": b.pointer("/item/uri"),
                    "context": b.pointer("/context/uri"),
                    "is_playing": b.get("is_playing"),
                    "progress_ms": b.get("progress_ms"),
                    "shuffle_state": b.get("shuffle_state"),
                    "repeat_state": b.get("repeat_state"),
                    "device": b.pointer("/device/name"),
                    "volume": b.pointer("/device/volume_percent"),
                })
            }),
        // One raw Web API request, to probe how Spotify answers: `raw GET /me/player/devices`,
        // `raw PUT /me/player/shuffle state=true`, `raw PUT /me/player/play '{"context_uri": …}'`.
        // Query pairs are `key=value` arguments; a JSON argument is the body.
        "raw" if args.len() > 2 => controller
            .web
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|web| {
                let method = match arg(1) {
                    "PUT" => silicon_spotify_client::webapi::Method::Put,
                    _ => silicon_spotify_client::webapi::Method::Get,
                };
                let mut query = Vec::new();
                let mut body = None;
                for extra in &args[3..] {
                    if extra.starts_with('{') {
                        body = Some(serde_json::from_str::<serde_json::Value>(extra)?);
                    } else if let Some((key, value)) = extra.split_once('=') {
                        query.push((key, value));
                    }
                }
                web.send(method, arg(2), &query, body.as_ref())
            })
            .map(|reply| serde_json::json!({"status": reply.status, "retry_after": reply.retry_after, "body": reply.body})),
        "devices" => controller
            .web
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|web| {
                let reply = web.send(
                    silicon_spotify_client::webapi::Method::Get,
                    "/me/player/devices",
                    &[],
                    None,
                )?;
                let playing = controller.status().is_ok_and(|p| {
                    p.state == silicon_spotify_client::model::PlayerState::Playing
                });
                let chosen = silicon_spotify_client::webapi::target(*web, playing);
                Ok(serde_json::json!({
                    "status": reply.status,
                    "devices": reply.body.get("devices"),
                    "start_goes_to": chosen.as_ref().ok().map(|c| serde_json::json!({
                        "name": c.device.name, "type": c.device.kind, "remote": c.remote,
                    })),
                    "error": chosen.err(),
                }))
            }),
        // The daemon's search fallback: the Web API's search, as search hits.
        "websearch" if args.len() > 1 => controller
            .web
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|web| {
                let kinds = ["track", "album", "artist", "playlist", "show", "episode"];
                let value = silicon_spotify_client::webapi::search(*web, arg(1), &kinds, 3)?;
                let mut results = serde_json::Map::new();
                for kind in kinds {
                    let hits: Vec<_> = value
                        .pointer(&format!("/{kind}s/items"))
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|hit| {
                            silicon_spotify_client::model::Item::from_web_json(kind, hit)
                        })
                        .collect();
                    results.insert(format!("{kind}s"), serde_json::to_value(hits)?);
                }
                Ok(serde_json::Value::Object(results))
            }),
        "lookup" => SpotifyUri::parse(arg(1), None).and_then(|uri| {
            let web = controller.web.as_ref().map_err(Clone::clone)?;
            let facts = silicon_spotify_client::webapi::item_facts(*web, &uri)?;
            if facts.playable() == Some(false) {
                return Err(silicon_spotify_client::webapi::not_playable(&facts));
            }
            Ok(serde_json::json!({
                "requested": facts.requested,
                "name": facts.name,
                "by": facts.by,
                "plays_as": facts.uri,
                "relinked": facts.uri != facts.requested,
                "context": facts.context,
                "listed_in_context_as": facts.listed_as,
                "position": facts.position,
            }))
        }),
        "like" => controller.like(true).and_then(to_json),
        "unlike" => controller.like(false).and_then(to_json),
        other => Err(Error::invalid(format!("unknown command `{other}`"), "")),
    };
    eprintln!("took {:.2} s", begun.elapsed().as_secs_f64());
    match result {
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        ),
        Err(error) => exit(&error),
    }
}

/// Refuses the Web API's playback commands (`PUT`s) and passes lookups on (`--no-web`).
struct NoPlayback {
    inner: Box<dyn WebApi>,
}

impl WebApi for NoPlayback {
    fn send(
        &self,
        method: silicon_spotify_client::webapi::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&serde_json::Value>,
    ) -> silicon_spotify_client::Result<silicon_spotify_client::webapi::Reply> {
        if method == silicon_spotify_client::webapi::Method::Put {
            return Err(Error::new(
                "web_api_disabled",
                "The Web API's playback commands are turned off (--no-web).",
                "",
            ));
        }
        self.inner.send(method, path, query, body)
    }
}

/// Prints each request's method, path, status and time (never the token).
struct Traced {
    inner: Box<dyn WebApi>,
    started: Instant,
}

impl WebApi for Traced {
    fn send(
        &self,
        method: silicon_spotify_client::webapi::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&serde_json::Value>,
    ) -> silicon_spotify_client::Result<silicon_spotify_client::webapi::Reply> {
        let sent = Instant::now();
        let reply = self.inner.send(method, path, query, body);
        eprintln!(
            "[{:>5} ms] {} {path} -> {} in {} ms",
            (sent - self.started).as_millis(),
            method.as_str(),
            reply
                .as_ref()
                .map_or_else(|error| error.code.clone(), |r| r.status.to_string()),
            sent.elapsed().as_millis()
        );
        reply
    }
}

#[cfg(feature = "api")]
fn web_client(player: &SpotifyPlayer) -> Option<Box<dyn WebApi>> {
    Some(Box::new(silicon_spotify_client::webapi::HttpWebApi::new(
        player,
    )))
}

#[cfg(not(feature = "api"))]
fn web_client(_: &SpotifyPlayer) -> Option<Box<dyn WebApi>> {
    None
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
