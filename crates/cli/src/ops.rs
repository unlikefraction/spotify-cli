//! Spotify commands: parse arguments, call the daemon, render.

use serde_json::{Value, json};
use silicon_spotify_client::timing::{Amount, SeekTarget};
use silicon_spotify_client::uri::{Kind, SpotifyUri};
use silicon_spotify_client::{Error, Result};

use crate::Ctx;
use crate::args::{
    Command, DeviceCommand, KindArg, LibrarySection, PlayArgs, PlaylistCommand, PodcastCommand,
    QueueCommand, RepeatArg, ScopeArg, Switch, TriggerAdd, TriggerCommand,
};
use crate::render;

fn kind(arg: KindArg) -> Kind {
    match arg {
        KindArg::Track => Kind::Track,
        KindArg::Album => Kind::Album,
        KindArg::Artist => Kind::Artist,
        KindArg::Playlist => Kind::Playlist,
        KindArg::Show => Kind::Show,
        KindArg::Episode => Kind::Episode,
    }
}

fn parse_uri(value: &str, default: Option<Kind>) -> Result<SpotifyUri> {
    SpotifyUri::parse(value, default)
}

/// First search hit of a kind.
async fn first_hit(ctx: &Ctx, query: &str, kind: Kind) -> Result<(SpotifyUri, Value)> {
    let results = ctx
        .daemon(
            "search",
            json!({"query": query, "kinds": [kind], "limit": 1}),
        )
        .await?;
    let key = format!("{}s", kind.as_str());
    let hit = results
        .pointer(&format!("/results/{key}/0"))
        .cloned()
        .ok_or_else(|| {
            Error::not_found(
                format!("No {} matches `{query}`.", kind),
                "Try other words, or another --type (track, album, artist, playlist, show, episode).",
            )
        })?;
    let uri = hit.get("uri").and_then(Value::as_str).unwrap_or_default();
    Ok((parse_uri(uri, None)?, hit))
}

async fn play(ctx: &Ctx, args: PlayArgs) -> Result<()> {
    let target = if args.liked {
        json!({"type": "liked", "limit": args.limit, "random": args.random})
    } else if let Some(seed) = &args.radio {
        json!({"type": "radio", "uri": parse_uri(seed, args.kind.map(kind))?})
    } else if let Some(query) = &args.search {
        let (uri, hit) = first_hit(ctx, query, args.kind.map_or(Kind::Track, kind)).await?;
        ctx.hint(&format!(
            "Found: {} ({})",
            hit.get("name").and_then(Value::as_str).unwrap_or("?"),
            uri
        ));
        json!({"type": "uri", "uri": uri, "context": args.context.as_deref().map(|c| parse_uri(c, Some(Kind::Playlist))).transpose()?, "shuffle": args.shuffle})
    } else if let Some(target) = &args.target {
        json!({"type": "uri", "uri": parse_uri(target, args.kind.map(kind))?, "context": args.context.as_deref().map(|c| parse_uri(c, Some(Kind::Playlist))).transpose()?, "shuffle": args.shuffle})
    } else {
        json!({"type": "resume"})
    };
    let value = ctx.daemon("player.play", json!({"target": target})).await?;
    ctx.emit(&value, render::outcome);
    Ok(())
}

async fn control(ctx: &Ctx, op: &str, args: Value) -> Result<()> {
    let value = ctx.daemon(op, args).await?;
    ctx.emit(&value, render::outcome);
    Ok(())
}

fn volume_target(level: &str) -> Result<Value> {
    let level = level.trim();
    let bad = || {
        Error::invalid(
            format!("`{level}` is not a volume."),
            "Use 0-100, +N, -N, up or down (e.g. spotify volume 40, spotify volume +10).",
        )
    };
    let target = match level {
        "up" => json!({"delta": 10}),
        "down" => json!({"delta": -10}),
        "mute" => json!({"absolute": 0}),
        other if other.starts_with('+') || other.starts_with('-') => {
            let delta: i16 = other.parse().map_err(|_| bad())?;
            json!({"delta": delta})
        }
        other => {
            let value: u8 = other.trim_end_matches('%').parse().map_err(|_| bad())?;
            if value > 100 {
                return Err(bad());
            }
            json!({"absolute": value})
        }
    };
    Ok(target)
}

/// Runs a Spotify command.
///
/// # Errors
/// Per command.
pub async fn run(ctx: &Ctx, command: Command) -> Result<()> {
    match command {
        Command::Status { full } => {
            let value = ctx.daemon("player.status", json!({"full": full})).await?;
            ctx.emit(&value, render::status);
        }
        Command::Play(args) => play(ctx, args).await?,
        Command::Resume => {
            control(ctx, "player.play", json!({"target": {"type": "resume"}})).await?
        }
        Command::Pause => control(ctx, "player.pause", json!({})).await?,
        Command::Toggle => control(ctx, "player.toggle", json!({})).await?,
        Command::Next => control(ctx, "player.next", json!({})).await?,
        Command::Previous => control(ctx, "player.previous", json!({})).await?,
        Command::Seek { position } => {
            let target = SeekTarget::parse(&position)?;
            control(ctx, "player.seek", json!({"target": target})).await?;
        }
        Command::Volume { level } => match level {
            None => {
                let value = ctx.daemon("player.status", json!({})).await?;
                let volume = value
                    .pointer("/playback/volume")
                    .cloned()
                    .unwrap_or(Value::Null);
                ctx.emit(&json!({"volume": volume}), |v| {
                    format!("Volume: {}%", v["volume"])
                });
            }
            Some(level) => {
                control(
                    ctx,
                    "player.volume",
                    json!({"target": volume_target(&level)?}),
                )
                .await?
            }
        },
        Command::Shuffle { mode } => {
            let on = match mode {
                Some(Switch::On) => Some(true),
                Some(Switch::Off) => Some(false),
                Some(Switch::Toggle) | None => None,
            };
            control(ctx, "player.shuffle", json!({"on": on})).await?;
        }
        Command::Repeat { mode } => {
            let mode = match mode {
                RepeatArg::Off => "off",
                RepeatArg::Context => "context",
                RepeatArg::Track => "track",
            };
            control(ctx, "player.repeat", json!({"mode": mode})).await?;
        }
        Command::Like => control(ctx, "player.like", json!({"like": true})).await?,
        Command::Unlike => control(ctx, "player.like", json!({"like": false})).await?,
        Command::Launch => {
            let value = ctx.daemon("spotify.launch", json!({})).await?;
            ctx.emit(&value, |_| "Spotify.app is running.".into());
        }
        Command::Track { target, kind: k } => {
            let uri = target
                .as_deref()
                .map(|t| parse_uri(t, Some(k.map_or(Kind::Track, kind))))
                .transpose()?;
            let value = ctx.daemon("track.info", json!({"uri": uri})).await?;
            ctx.emit(&value, render::track);
        }
        Command::Lyrics { target } => {
            let uri = target
                .as_deref()
                .map(|t| parse_uri(t, Some(Kind::Track)))
                .transpose()?;
            let value = ctx.daemon("lyrics", json!({"uri": uri})).await?;
            ctx.emit(&value, |v| {
                format!(
                    "{}\n\n{}",
                    v["title"].as_str().unwrap_or(""),
                    v["text"].as_str().unwrap_or("")
                )
            });
        }
        Command::Search {
            query,
            kinds,
            limit,
            play,
        } => {
            let query = query.join(" ");
            let kinds: Vec<Kind> = kinds.into_iter().map(kind).collect();
            let limit = limit
                .or(ctx.config.search_limit.map(|l| l as usize))
                .unwrap_or(10)
                .clamp(1, 50);
            let value = ctx
                .daemon(
                    "search",
                    json!({"query": query, "kinds": kinds, "limit": limit}),
                )
                .await?;
            if play {
                let first = [
                    "tracks",
                    "episodes",
                    "albums",
                    "playlists",
                    "artists",
                    "shows",
                ]
                .iter()
                .find_map(|k| {
                    value
                        .pointer(&format!("/results/{k}/0/uri"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .ok_or_else(|| {
                    Error::not_found(format!("Nothing matches `{query}`."), "Try other words.")
                })?;
                let uri = parse_uri(&first, None)?;
                let played = ctx
                    .daemon(
                        "player.play",
                        json!({"target": {"type": "uri", "uri": uri, "shuffle": false}}),
                    )
                    .await?;
                ctx.emit(&json!({"search": value, "played": played}), |v| {
                    render::outcome(&v["played"])
                });
            } else {
                ctx.emit(&value, render::search);
            }
        }
        Command::Library { section, limit } => {
            let key = match section {
                LibrarySection::Liked => "liked",
                LibrarySection::Albums => "albums",
                LibrarySection::Artists => "artists",
                LibrarySection::Top => "top",
                LibrarySection::Playlists => "playlists",
            };
            let value = ctx
                .daemon("library.get", json!({"key": key, "limit": limit}))
                .await?;
            ctx.emit(&value, render::items_list);
        }
        Command::Queue { action } => queue(ctx, action.unwrap_or(QueueCommand::List)).await?,
        Command::Playlist { action } => playlist(ctx, action).await?,
        Command::Podcast { action } => podcast(ctx, action).await?,
        Command::Devices { action } => match action.unwrap_or(DeviceCommand::List) {
            DeviceCommand::List => {
                let value = ctx.daemon("devices.list", json!({})).await?;
                ctx.emit(&value, render::devices);
            }
            DeviceCommand::Connect { id, name } => {
                let value = ctx
                    .daemon("devices.connect", json!({"id": id, "name": name}))
                    .await?;
                ctx.emit(&value, |v| {
                    format!(
                        "Playback moved to {}.",
                        v["connected"].as_str().unwrap_or("?")
                    )
                });
            }
        },
        Command::Trigger { action } => trigger(ctx, action).await?,
        other => return Err(Error::internal(format!("unrouted command {other:?}"))),
    }
    Ok(())
}

async fn queue(ctx: &Ctx, action: QueueCommand) -> Result<()> {
    match action {
        QueueCommand::List => {
            let value = ctx.daemon("queue.list", json!({})).await?;
            ctx.emit(&value, render::queue);
        }
        QueueCommand::Add {
            items,
            search,
            kind: k,
            next,
        } => {
            let mut uris = Vec::new();
            for item in &items {
                uris.push(parse_uri(item, Some(Kind::Track))?);
            }
            if let Some(query) = &search {
                let (uri, hit) = first_hit(ctx, query, k.map_or(Kind::Track, kind)).await?;
                ctx.hint(&format!(
                    "Found: {} ({uri})",
                    hit.get("name").and_then(Value::as_str).unwrap_or("?")
                ));
                uris.push(uri);
            }
            if uris.is_empty() {
                return Err(Error::invalid(
                    "Nothing to queue.",
                    "spotify queue add <uri>… or spotify queue add --search 'song'",
                ));
            }
            let value = ctx
                .daemon("queue.add", json!({"uris": uris, "next": next}))
                .await?;
            ctx.emit(&value, |v| {
                let added = v["added"].as_array().map_or(0, Vec::len);
                format!(
                    "Queued {added} item(s). Managed queue now has {}.",
                    v["queue"].as_array().map_or(0, Vec::len)
                )
            });
            ctx.hint("They play when the current track ends (or run `spotify next`). See `spotify queue`.");
        }
        QueueCommand::Remove { item } => {
            let value = ctx.daemon("queue.remove", json!({"item": item})).await?;
            ctx.emit(&value, |v| {
                format!("Removed {}.", v["removed"]["uri"].as_str().unwrap_or("?"))
            });
        }
        QueueCommand::Move { item, to } => {
            let value = ctx
                .daemon("queue.move", json!({"from": item, "to": to}))
                .await?;
            ctx.emit(&value, |v| {
                format!(
                    "Moved {} to position {}.",
                    v["moved"]["uri"].as_str().unwrap_or("?"),
                    v["position"]
                )
            });
        }
        QueueCommand::Clear => {
            let value = ctx.daemon("queue.clear", json!({})).await?;
            ctx.emit(&value, |v| {
                format!(
                    "Cleared {} managed item(s). Spotify's own upcoming list is unchanged.",
                    v["cleared"]
                )
            });
        }
    }
    Ok(())
}

async fn playlist(ctx: &Ctx, action: PlaylistCommand) -> Result<()> {
    let (op, args): (&str, Value) = match action {
        PlaylistCommand::List { limit } => ("playlist.list", json!({"limit": limit})),
        PlaylistCommand::Show { playlist } => ("playlist.show", json!({"id": playlist})),
        PlaylistCommand::Create {
            name,
            description,
            public,
            collab,
        } => (
            "playlist.create",
            json!({"name": name, "description": description, "public": public, "collab": collab}),
        ),
        PlaylistCommand::Delete { playlist } => ("playlist.delete", json!({"id": playlist})),
        PlaylistCommand::Add { playlist, items } => {
            let items = items
                .iter()
                .map(|i| parse_uri(i, Some(Kind::Track)))
                .collect::<Result<Vec<_>>>()?;
            ("playlist.add", json!({"id": playlist, "items": items}))
        }
        PlaylistCommand::Remove { playlist, items } => {
            let items = items
                .iter()
                .map(|i| parse_uri(i, Some(Kind::Track)))
                .collect::<Result<Vec<_>>>()?;
            ("playlist.remove", json!({"id": playlist, "items": items}))
        }
        PlaylistCommand::Play { playlist, shuffle } => {
            let uri = parse_uri(&playlist, Some(Kind::Playlist))?;
            let value = ctx
                .daemon(
                    "player.play",
                    json!({"target": {"type": "uri", "uri": uri, "shuffle": shuffle}}),
                )
                .await?;
            ctx.emit(&value, render::outcome);
            return Ok(());
        }
        PlaylistCommand::Rename { playlist, name } => {
            ("playlist.rename", json!({"id": playlist, "name": name}))
        }
        PlaylistCommand::Import { from, to, delete } => (
            "playlist.import",
            json!({"from": from, "to": to, "delete": delete}),
        ),
        PlaylistCommand::Fork { playlist } => ("playlist.fork", json!({"id": playlist})),
        PlaylistCommand::Sync { playlist, delete } => {
            ("playlist.sync", json!({"id": playlist, "delete": delete}))
        }
    };
    let value = ctx.daemon(op, args).await?;
    ctx.emit(&value, |v| render::playlist(op, v));
    Ok(())
}

async fn podcast(ctx: &Ctx, action: PodcastCommand) -> Result<()> {
    match action {
        PodcastCommand::Search {
            query,
            episodes,
            shows,
            limit,
        } => {
            let kinds: Vec<Kind> = if episodes {
                vec![Kind::Episode]
            } else if shows {
                vec![Kind::Show]
            } else {
                vec![Kind::Show, Kind::Episode]
            };
            let value = ctx
                .daemon(
                    "search",
                    json!({"query": query.join(" "), "kinds": kinds, "limit": limit.unwrap_or(10)}),
                )
                .await?;
            ctx.emit(&value, render::search);
        }
        PodcastCommand::Play { target } => {
            let uri = parse_uri(&target, Some(Kind::Episode))?;
            if !matches!(uri.kind, Kind::Show | Kind::Episode) {
                return Err(Error::invalid(
                    format!("{uri} is not a podcast show or episode."),
                    "Use `spotify play` for music.",
                ));
            }
            let value = ctx
                .daemon(
                    "player.play",
                    json!({"target": {"type": "uri", "uri": uri, "shuffle": false}}),
                )
                .await?;
            ctx.emit(&value, render::outcome);
        }
        PodcastCommand::Saved => {
            let value = ctx.daemon("podcast.saved", json!({})).await?;
            ctx.emit(&value, |v| {
                render::items_list(&json!({"items": v["shows"], "section": "saved shows"}))
            });
        }
        PodcastCommand::Now => {
            let value = ctx.daemon("player.status", json!({})).await?;
            let track = value
                .pointer("/playback/track")
                .cloned()
                .unwrap_or(Value::Null);
            if track.get("kind").and_then(Value::as_str) != Some("episode") {
                return Err(Error::new(
                    "not_a_podcast",
                    "The current item is not a podcast episode.",
                    "Play one with `spotify podcast play spotify:episode:<id>`; `spotify status` shows what is playing.",
                )
                .with_details(json!({"now_playing": track})));
            }
            ctx.emit(&value, render::status);
        }
    }
    Ok(())
}

fn parse_duration_ms(value: &str) -> Result<u64> {
    match Amount::parse(value)? {
        Amount::Millis(ms) => Ok(ms),
        Amount::Percent(_) => Err(Error::invalid(
            "A timeout must be a duration, not a percentage.",
            "For example --timeout 10m or --timeout 90s.",
        )),
    }
}

async fn trigger(ctx: &Ctx, action: TriggerCommand) -> Result<()> {
    match action {
        TriggerCommand::Add(add) => trigger_add(ctx, add).await,
        TriggerCommand::List { all, everyone } => {
            let value = ctx
                .daemon("trigger.list", json!({"all": all, "everyone": everyone}))
                .await?;
            ctx.emit(&value, render::triggers);
            Ok(())
        }
        TriggerCommand::Show { id } => {
            let value = ctx.daemon("trigger.get", json!({"id": id})).await?;
            ctx.emit(&value, render::trigger_detail);
            Ok(())
        }
        TriggerCommand::Remove { id } => {
            let value = ctx.daemon("trigger.remove", json!({"id": id})).await?;
            ctx.emit(&value, |v| format!("Removed {}.", v["removed"]));
            Ok(())
        }
        TriggerCommand::Clear => {
            let value = ctx.daemon("trigger.clear", json!({})).await?;
            ctx.emit(&value, |v| {
                format!(
                    "Removed {} trigger(s).",
                    v["removed"].as_array().map_or(0, Vec::len)
                )
            });
            Ok(())
        }
        TriggerCommand::History {
            id,
            limit,
            everyone,
        } => {
            let value = ctx
                .daemon(
                    "trigger.history",
                    json!({"id": id, "limit": limit, "everyone": everyone}),
                )
                .await?;
            ctx.emit(&value, render::history);
            Ok(())
        }
        TriggerCommand::Test { id } => {
            let value = ctx
                .daemon(
                    "trigger.test",
                    json!({"id": id, "target": target(ctx), "isi": isi(ctx)}),
                )
                .await?;
            ctx.emit(&value, |v| {
                format!(
                    "Test Ting delivered: {}.",
                    v["firing"]["ting"]["id"].as_str().unwrap_or("accepted")
                )
            });
            Ok(())
        }
        TriggerCommand::Wait { id, timeout } => {
            let timeout_ms = timeout
                .as_deref()
                .map(parse_duration_ms)
                .transpose()?
                .unwrap_or(3_600_000);
            ctx.hint(&format!(
                "Waiting for {id} (up to {})…",
                silicon_spotify_client::timing::clock(timeout_ms)
            ));
            let value = ctx
                .daemon("trigger.wait", json!({"id": id, "timeout_ms": timeout_ms}))
                .await?;
            ctx.emit(&value, |v| {
                if v["firing"].is_null() {
                    v["note"]
                        .as_str()
                        .unwrap_or("The trigger is no longer active.")
                        .to_string()
                } else {
                    render::firing_line(&v["firing"])
                }
            });
            Ok(())
        }
        TriggerCommand::Retry { firing } => {
            let value = ctx
                .daemon("trigger.retry", json!({"firing": firing}))
                .await?;
            ctx.emit(&value, |_| "Delivery re-queued.".into());
            Ok(())
        }
    }
}

fn target(ctx: &Ctx) -> Value {
    json!({
        "api_url": ctx.api_url,
        "slot": ctx.slot(),
        "testing": ctx.testing.is_some(),
        "org": ctx.org(None),
    })
}

fn isi(ctx: &Ctx) -> Option<String> {
    ctx.isi.clone().or_else(|| ctx.config.notify_isi.clone())
}

async fn trigger_add(ctx: &Ctx, add: TriggerAdd) -> Result<()> {
    let condition = if let Some(value) = &add.remaining {
        json!({"kind": "remaining", "at": Amount::parse(value)?})
    } else if let Some(value) = add.elapsed.as_ref().or(add.at.as_ref()) {
        json!({"kind": "elapsed", "at": Amount::parse(value)?})
    } else if add.end {
        json!({"kind": "end"})
    } else {
        json!({"kind": "change"})
    };
    let scope = match add.scope {
        ScopeArg::Current => {
            if add.track.is_some() {
                return Err(Error::invalid(
                    "--track needs --scope track.",
                    "Example: spotify trigger add --end --scope track --track spotify:track:<id>",
                ));
            }
            json!("current")
        }
        ScopeArg::Every => json!("every"),
        ScopeArg::Track => {
            let track = add.track.as_deref().ok_or_else(|| {
                Error::invalid(
                    "--scope track needs --track <uri>.",
                    "Example: --scope track --track spotify:track:<id>",
                )
            })?;
            json!({"track": parse_uri(track, Some(Kind::Track))?.uri()})
        }
    };
    if ctx.testing.is_some() && !add.local && ctx.home.testing_saved()? != ctx.testing {
        return Err(Error::invalid(
            "The testing plane is selected only by SPOTIFY_TEST_APP_SECRET, but triggers are delivered later by the daemon, which reads the home's saved selection.",
            "Save it with `spotify testing use --app-secret-file -` (then unset the variable), or add --local.",
        ));
    }
    let times = if add.once { Some(1) } else { add.times };
    let value = ctx
        .daemon(
            "trigger.add",
            json!({
                "condition": condition,
                "scope": scope,
                "times": times,
                "note": add.note,
                "label": add.label,
                "notify_expiry": !add.no_expiry_notice,
                "ting": !add.local,
                "target": target(ctx),
                "isi": isi(ctx),
            }),
        )
        .await?;
    ctx.emit(&value, render::trigger_added);
    let id = value
        .pointer("/trigger/id")
        .and_then(Value::as_str)
        .unwrap_or("<id>");
    if add.local {
        ctx.hint(&format!("Local only: `spotify trigger wait {id}` blocks until it fires; `spotify trigger history {id}` shows firings."));
    } else {
        ctx.hint(&format!("You will get a `spotify.trigger.fired` Ting. Check delivery with `spotify trigger history {id}`; remove with `spotify trigger remove {id}`."));
    }
    Ok(())
}
