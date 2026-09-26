//! Spotify commands: parse arguments, call the daemon, render.

use serde_json::{Value, json};
use silicon_spotify_client::player::SEARCH_MAX_PER_KIND;
use silicon_spotify_client::timing::{Amount, SeekTarget};
use silicon_spotify_client::trigger::{self, Condition};
use silicon_spotify_client::uri::{Kind, SpotifyUri};
use silicon_spotify_client::{Error, Result};

use crate::Ctx;
use crate::args::{
    Command, DeviceCommand, KindArg, LibrarySection, PlayArgs, PlaylistCommand, PodcastCommand,
    QueueCommand, QueueKindArg, RepeatArg, ScopeArg, Switch, TrackKindArg, TriggerAdd,
    TriggerCommand,
};
use crate::render;

/// Most firings `trigger history` returns.
const HISTORY_MAX: usize = 500;

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

fn track_kind(arg: TrackKindArg) -> Kind {
    match arg {
        TrackKindArg::Track => Kind::Track,
        TrackKindArg::Album => Kind::Album,
        TrackKindArg::Artist => Kind::Artist,
        TrackKindArg::Playlist => Kind::Playlist,
    }
}

fn parse_uri(value: &str, default: Option<Kind>) -> Result<SpotifyUri> {
    SpotifyUri::parse(value, default)
}

/// A playlist reference (bare id, URI or link), checked before it reaches the daemon. Returns the
/// bare id.
fn playlist_id(value: &str) -> Result<String> {
    let uri = parse_uri(value, Some(Kind::Playlist))?;
    if uri.kind != Kind::Playlist {
        return Err(Error::invalid(
            format!("{uri} is not a playlist ({}).", uri.kind),
            "Pass a playlist id, spotify:playlist:<id> or an open.spotify.com/playlist/<id> link; `spotify playlist list` shows yours.",
        ));
    }
    Ok(uri.id)
}

/// Tracks and albums to add to or remove from a playlist; other kinds are rejected up front so
/// an edit never stops half way. Episodes are `unsupported` (Spotify playlists hold them, but
/// spotify_player cannot add them, as the daemon also answers); artists, shows and playlists are
/// `invalid_input` (no playlist can hold them).
fn playlist_items(items: &[String]) -> Result<Vec<SpotifyUri>> {
    const HINT: &str =
        "Pass spotify:track:<id> or spotify:album:<id> items (links and bare track ids work too).";
    items
        .iter()
        .map(|item| {
            let uri = parse_uri(item, Some(Kind::Track))?;
            match uri.kind {
                Kind::Track | Kind::Album => Ok(uri),
                Kind::Episode => Err(Error::unsupported(
                    format!("{uri} is an episode: spotify_player can add or remove only tracks and albums."),
                    format!("Add or remove episodes in the Spotify app. {HINT}"),
                )),
                kind => Err(Error::invalid(
                    format!("{uri} cannot be added to or removed from a playlist ({kind}s are not playlist items)."),
                    HINT,
                )),
            }
        })
        .collect()
}

/// Checks `--limit` against what the command can honor. Out-of-range values (negative ones too)
/// are rejected rather than clamped, so the output always reflects the limit applied.
fn check_limit(limit: Option<i64>, max: Option<usize>, hint: &str) -> Result<Option<usize>> {
    let Some(value) = limit else {
        return Ok(None);
    };
    match usize::try_from(value) {
        Ok(ok) if ok > 0 && max.is_none_or(|max| ok <= max) => Ok(Some(ok)),
        _ => {
            let range = max.map_or_else(|| "1 or more".to_owned(), |max| format!("1 to {max}"));
            Err(Error::invalid(
                format!("--limit {value} is out of range: it takes {range}."),
                hint,
            )
            .with_details(json!({"limit": value, "min": 1, "max": max})))
        }
    }
}

/// A search's `--limit`: results per kind, at most what spotify_player returns.
fn search_limit_flag(flag: Option<i64>) -> Result<Option<usize>> {
    let max = SEARCH_MAX_PER_KIND as usize;
    check_limit(
        flag,
        Some(max),
        &format!(
            "spotify_player returns at most {max} results per kind and has no option for more; pass --limit 1 to {max}."
        ),
    )
}

/// Results per kind for `spotify search`: `--limit`, else config `search_limit`, else 10.
fn search_limit(ctx: &Ctx, flag: Option<i64>) -> Result<usize> {
    let max = SEARCH_MAX_PER_KIND as usize;
    if let Some(limit) = search_limit_flag(flag)? {
        return Ok(limit);
    }
    Ok(match ctx.config.search_limit.map(|l| l as usize) {
        // Saved by a release that allowed up to 50: apply what spotify_player can return.
        Some(saved) if saved > max => {
            ctx.hint(&format!(
                "note: config search_limit is {saved}, but spotify_player returns at most {max} per kind; using {max}. Fix: spotify config set '{{\"search_limit\": {max}}}'"
            ));
            max
        }
        Some(saved) if saved > 0 => saved,
        _ => max,
    })
}

/// First search hit of a kind. `kinds` lists what the command's --type accepts, for the hint.
async fn first_hit(ctx: &Ctx, query: &str, kind: Kind, kinds: &str) -> Result<(SpotifyUri, Value)> {
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
                format!("Try other words, or another --type ({kinds})."),
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
        let (uri, hit) = first_hit(
            ctx,
            query,
            args.kind.map_or(Kind::Track, kind),
            "track, album, artist, playlist, show, episode",
        )
        .await?;
        ctx.hint(&format!(
            "Found: {} ({})",
            render::one_line(hit.get("name").and_then(Value::as_str).unwrap_or("?")),
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

/// A read-only daemon call, repeated once after a transient failure: one network blip while
/// paging through a long listing should not fail the whole command.
async fn read(ctx: &Ctx, op: &str, args: Value) -> Result<Value> {
    match ctx.daemon(op, args.clone()).await {
        Err(error) if transient(&error) => {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            ctx.daemon(op, args).await
        }
        other => other,
    }
}

/// A failure worth one more try: a retryable network or busy-instance error.
fn transient(error: &Error) -> bool {
    error.retryable && matches!(error.code.as_str(), "transport" | "spotify_player_busy")
}

/// What `queue add` already knows about a search hit it queues, so the daemon need not look
/// it up: name, artists (or, for an episode, its show) and length.
fn known_item(uri: &SpotifyUri, hit: &Value) -> Value {
    let artists = hit
        .get("by")
        .and_then(Value::as_array)
        .map(|by| {
            by.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|by| !by.is_empty());
    let by = match uri.kind {
        Kind::Episode => hit
            .get("album")
            .and_then(Value::as_str)
            .filter(|show| !show.is_empty())
            .map(str::to_owned)
            .or(artists),
        _ => artists,
    };
    json!({
        "uri": uri.uri(),
        "name": hit.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()),
        "by": by,
        "duration_ms": hit.get("duration_ms").and_then(Value::as_u64),
    })
}

/// `not_found` for a bare id looked up as a track: it may name another kind.
fn bare_id_not_found(mut error: Error, id: &str) -> Error {
    error.hint = format!(
        "Bare ids are looked up as tracks: if {id} is an album, artist or playlist, pass --type album|artist|playlist (or use its spotify:<kind>:<id> URI). {}",
        error.hint
    );
    error
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
            ctx.emit(&value, render::launched);
        }
        Command::Track { target, kind: k } => {
            let uri = target
                .as_deref()
                .map(|t| parse_uri(t, Some(k.map_or(Kind::Track, track_kind))))
                .transpose()?;
            // A bare id without --type is looked up as a track.
            let bare = k.is_none()
                && target
                    .as_deref()
                    .is_some_and(|t| parse_uri(t, None).is_err());
            let mut value = match ctx.daemon("track.info", json!({"uri": uri})).await {
                Err(error) if bare && error.code == "not_found" => {
                    return Err(bare_id_not_found(error, target.as_deref().unwrap_or("")));
                }
                other => other?,
            };
            // Normalize spotify_player's album/artist/track output (`raw`) so the album's tracks
            // or the artist's top tracks and albums are structured fields, not only raw data.
            if let (Some(uri), Some(raw)) = (&uri, value.get("raw").filter(|r| !r.is_null())) {
                let view = silicon_spotify_client::model::item_view(uri.kind.as_str(), raw);
                if let (Some(object), Value::Object(view)) = (value.as_object_mut(), view) {
                    object.extend(view);
                }
            }
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
            let limit = search_limit(ctx, limit)?;
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
            let limit = check_limit(
                limit,
                None,
                "Pass --limit 1 or more, or omit it to show the whole section.",
            )?;
            let value = read(ctx, "library.get", json!({"key": key, "limit": limit})).await?;
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
            let mut known = Vec::new();
            for item in &items {
                uris.push(parse_uri(item, Some(Kind::Track))?);
            }
            if let Some(query) = &search {
                let k = match k {
                    Some(QueueKindArg::Episode) => Kind::Episode,
                    Some(QueueKindArg::Track) | None => Kind::Track,
                };
                let (uri, hit) = first_hit(ctx, query, k, "track, episode").await?;
                ctx.hint(&format!(
                    "Found: {} ({uri})",
                    render::one_line(hit.get("name").and_then(Value::as_str).unwrap_or("?"))
                ));
                known.push(known_item(&uri, &hit));
                uris.push(uri);
            }
            if uris.is_empty() {
                return Err(Error::invalid(
                    "Nothing to queue.",
                    "spotify queue add <uri>… or spotify queue add --search 'song'",
                ));
            }
            let value = ctx
                .daemon(
                    "queue.add",
                    json!({"uris": uris, "next": next, "known": known}),
                )
                .await?;
            ctx.emit(&value, render::queue_added);
            ctx.hint("They play when the current track ends (or run `spotify next`). See `spotify queue`.");
        }
        QueueCommand::Remove { item } => {
            let value = ctx.daemon("queue.remove", json!({"item": item})).await?;
            ctx.emit(&value, |v| {
                format!("Removed {}.", render::queue_item_label(&v["removed"]))
            });
        }
        QueueCommand::Move { item, to } => {
            let value = ctx
                .daemon("queue.move", json!({"from": item, "to": to}))
                .await?;
            ctx.emit(&value, |v| {
                format!(
                    "Moved {} to position {}.",
                    render::queue_item_label(&v["moved"]),
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
    let mut fork_name = None;
    let (op, args): (&str, Value) = match action {
        PlaylistCommand::List { limit } => {
            let limit = check_limit(
                limit,
                None,
                "Pass --limit 1 or more, or omit it to list every playlist.",
            )?;
            let value = read(ctx, "playlist.list", json!({"limit": limit})).await?;
            ctx.emit(&value, |v| render::playlist("playlist.list", v));
            return Ok(());
        }
        PlaylistCommand::Show { playlist } => {
            ("playlist.show", json!({"id": playlist_id(&playlist)?}))
        }
        PlaylistCommand::Create {
            name,
            description,
            public,
            collab,
        } => (
            "playlist.create",
            json!({"name": name, "description": description, "public": public, "collab": collab}),
        ),
        PlaylistCommand::Delete { playlist } => {
            ("playlist.delete", json!({"id": playlist_id(&playlist)?}))
        }
        PlaylistCommand::Add { playlist, items } => {
            let id = playlist_id(&playlist)?;
            let items = playlist_items(&items)?;
            ("playlist.add", json!({"id": id, "items": items}))
        }
        PlaylistCommand::Remove { playlist, items } => {
            let id = playlist_id(&playlist)?;
            let items = playlist_items(&items)?;
            ("playlist.remove", json!({"id": id, "items": items}))
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
            json!({"from": playlist_id(&from)?, "to": playlist_id(&to)?, "delete": delete}),
        ),
        PlaylistCommand::Fork { playlist, name } => {
            let name = name.map(|n| n.trim().to_owned());
            if name.as_deref() == Some("") {
                return Err(Error::invalid(
                    "--name is empty.",
                    "Pass a name for the new playlist, or omit --name to keep the original's.",
                ));
            }
            fork_name.clone_from(&name);
            (
                "playlist.fork",
                json!({"id": playlist_id(&playlist)?, "name": name}),
            )
        }
        PlaylistCommand::Sync { playlist, delete } => {
            let id = playlist.as_deref().map(playlist_id).transpose()?;
            ("playlist.sync", json!({"id": id, "delete": delete}))
        }
    };
    let value = if op == "playlist.show" {
        read(ctx, op, args).await?
    } else {
        ctx.daemon(op, args).await?
    };
    ctx.emit(&value, |v| render::playlist(op, v));
    if let Some(wanted) = fork_name
        && value.get("name").and_then(Value::as_str) != Some(wanted.as_str())
    {
        ctx.hint("note: the fork kept the original's name (this daemon could not apply --name); rename it in the Spotify app.");
    }
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
            let limit = search_limit_flag(limit)?.unwrap_or(SEARCH_MAX_PER_KIND as usize);
            let value = ctx
                .daemon(
                    "search",
                    json!({"query": query.join(" "), "kinds": kinds, "limit": limit}),
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
            let limit = check_limit(
                limit,
                Some(HISTORY_MAX),
                &format!("Pass --limit 1 to {HISTORY_MAX} (default 20)."),
            )?;
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
        Condition::Remaining(Amount::parse(value)?)
    } else if let Some(value) = add.elapsed.as_ref().or(add.at.as_ref()) {
        Condition::Elapsed(Amount::parse(value)?)
    } else if add.end {
        Condition::End
    } else {
        Condition::Change
    };
    let times = if add.once { Some(1) } else { add.times };
    // The same request checks the daemon runs, before it reads Spotify: a bad --times or --note
    // is invalid_input whatever is playing.
    trigger::validate_request(condition, times, add.note.as_deref(), add.label.as_deref())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_checked_whatever_their_sign() {
        assert_eq!(check_limit(None, Some(10), "").ok(), Some(None));
        assert_eq!(check_limit(Some(3), Some(10), "").ok(), Some(Some(3)));
        assert_eq!(check_limit(Some(500), None, "").ok(), Some(Some(500)));
        for bad in [0, -1, 11] {
            let error = check_limit(Some(bad), Some(10), "hint").expect_err("out of range");
            assert_eq!(error.code, "invalid_input");
            assert_eq!(
                error.message,
                format!("--limit {bad} is out of range: it takes 1 to 10.")
            );
            assert_eq!(
                error.details,
                Some(json!({"limit": bad, "min": 1, "max": 10}))
            );
        }
    }

    #[test]
    fn queued_search_hits_carry_what_is_known() {
        let track = parse_uri("spotify:track:0BxE4FqsDD1Ot4YuBXwAPp", None).expect("uri");
        let hit = json!({"kind": "track", "name": "505", "by": ["Arctic Monkeys", "X"],
            "album": "Favourite Worst Nightmare", "duration_ms": 253_000});
        assert_eq!(
            known_item(&track, &hit),
            json!({"uri": "spotify:track:0BxE4FqsDD1Ot4YuBXwAPp", "name": "505",
                "by": "Arctic Monkeys, X", "duration_ms": 253_000})
        );
        // An episode's `by` is its show.
        let episode = parse_uri("spotify:episode:4IzpgR6RCEkRqMHbJF38Wp", None).expect("uri");
        let hit = json!({"kind": "episode", "name": "How to Speak Clearly", "album": "Huberman Lab",
            "duration_ms": 7_140_000});
        assert_eq!(
            known_item(&episode, &hit),
            json!({"uri": "spotify:episode:4IzpgR6RCEkRqMHbJF38Wp", "name": "How to Speak Clearly",
                "by": "Huberman Lab", "duration_ms": 7_140_000})
        );
        // Nothing known: nulls, which the daemon fills in itself.
        assert_eq!(
            known_item(&episode, &json!({"name": ""})),
            json!({"uri": "spotify:episode:4IzpgR6RCEkRqMHbJF38Wp", "name": null, "by": null, "duration_ms": null})
        );
        // An empty show is not known either.
        assert_eq!(
            known_item(&episode, &json!({"name": "Ep", "album": ""}))["by"],
            Value::Null
        );
    }

    #[test]
    fn only_transient_read_failures_are_retried() {
        assert!(transient(
            &Error::new("transport", "connection reset", "retry").retryable()
        ));
        assert!(transient(
            &Error::new("spotify_player_busy", "busy", "retry").retryable()
        ));
        // Not retryable, or not a network blip: fail at once.
        assert!(!transient(&Error::new("transport", "bad url", "fix it")));
        assert!(!transient(
            &Error::new("rate_limited", "429", "wait").retryable()
        ));
        assert!(!transient(&Error::not_found("gone", "search")));
    }

    #[test]
    fn bare_track_ids_that_are_not_found_suggest_a_kind() {
        let error = bare_id_not_found(
            Error::not_found("Spotify could not find that item.", "Search for it."),
            "78bpIziExqiI9qztvNFlQu",
        );
        assert_eq!(error.code, "not_found");
        assert!(
            error.hint.starts_with("Bare ids are looked up as tracks: if 78bpIziExqiI9qztvNFlQu is an album, artist or playlist, pass --type album|artist|playlist"),
            "{}",
            error.hint
        );
        assert!(error.hint.ends_with("Search for it."), "{}", error.hint);
    }
}
