//! What every command is for beyond its grammar: the goal it serves, what it can do, what it
//! needs, the JSON it prints and the error codes it can return. The grammar and help text live in
//! `args.rs`; this module adds the rest and builds from both the grouped root help, the
//! `spotify commands --json` manifest and the command side of `spotify how`.
//!
//! A test checks that every command in the grammar has an entry here and every entry names a
//! command, so the two cannot drift apart.

use std::fmt::Write as _;

use clap::builder::styling::Style;
use serde_json::{Value, json};
use silicon_spotify_client::Error;

/// What a user wants to do: the groups of the root help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Goal {
    Listen,
    Find,
    Details,
    Queue,
    Playlists,
    Podcasts,
    Triggers,
    Account,
    Setup,
}

impl Goal {
    /// In root-help order.
    pub const ALL: [Self; 9] = [
        Self::Listen,
        Self::Find,
        Self::Details,
        Self::Queue,
        Self::Playlists,
        Self::Podcasts,
        Self::Triggers,
        Self::Account,
        Self::Setup,
    ];

    /// Stable id for `--json`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Listen => "listen",
            Self::Find => "find",
            Self::Details => "lyrics_and_details",
            Self::Queue => "queue",
            Self::Playlists => "playlists",
            Self::Podcasts => "podcasts",
            Self::Triggers => "triggers",
            Self::Account => "account",
            Self::Setup => "setup",
        }
    }

    /// Heading in the root help.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Listen => "Listen",
            Self::Find => "Find",
            Self::Details => "Lyrics & details",
            Self::Queue => "Queue",
            Self::Playlists => "Playlists",
            Self::Podcasts => "Podcasts",
            Self::Triggers => "Triggers (Ting)",
            Self::Account => "Account & login",
            Self::Setup => "Setup & diagnose",
        }
    }
}

/// Something a command needs before it can work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// spotify-daemon (started on demand; macOS only).
    Daemon,
    /// The Spotify desktop app on this Mac, and permission to control it.
    SpotifyApp,
    /// spotify_player installed and signed in to Spotify (Web API features).
    SpotifyPlayer,
    /// An IAM session in this home (`spotify login`).
    IamLogin,
    /// A Spotify Premium account.
    Premium,
    /// It changes what plays.
    ControlsPlayback,
    /// The internet (Spotify's Web API or the spotify-cli backend).
    Network,
}

impl Need {
    /// Every requirement, in manifest order, with its manifest key.
    const KEYS: [(Self, &'static str); 7] = [
        (Self::Daemon, "daemon"),
        (Self::SpotifyApp, "spotify_app"),
        (Self::SpotifyPlayer, "spotify_player_signed_in"),
        (Self::IamLogin, "iam_login"),
        (Self::Premium, "premium"),
        (Self::ControlsPlayback, "controls_playback"),
        (Self::Network, "network"),
    ];

    /// Error codes a command with this requirement can return because of it.
    const fn errors(self) -> &'static [&'static str] {
        match self {
            Self::Daemon => &[
                "daemon_unavailable",
                "daemon_missing",
                "daemon_outdated",
                "protocol_mismatch",
                "unknown_op",
                "platform_unsupported",
            ],
            Self::SpotifyApp => &[
                "spotify_not_running",
                "spotify_not_installed",
                "automation_permission_denied",
                "applescript_failed",
                "timeout",
            ],
            Self::SpotifyPlayer => &[
                "spotify_auth_required",
                "spotify_player_missing",
                "spotify_player_failed",
                "spotify_player_busy",
                "transport",
                "rate_limited",
                "timeout",
            ],
            Self::IamLogin => &["not_authenticated", "reconsent_required", "forbidden"],
            Self::Premium => &["premium_required"],
            Self::ControlsPlayback => &[
                "verification_failed",
                "no_effect",
                "state_mismatch",
                "no_active_device",
                "premium_required",
            ],
            Self::Network => &[
                "backend_unavailable",
                "dependency_unavailable",
                "rate_limited",
            ],
        }
    }
}

/// One command's catalog entry.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// Path without `spotify`: `trigger add`.
    pub path: &'static str,
    /// The goal it serves (subcommands without one take their top-level command's).
    pub goal: Option<Goal>,
    /// What it can do, in short phrases.
    pub capabilities: &'static [&'static str],
    /// Other words people use for it (for `spotify how`).
    pub keywords: &'static [&'static str],
    /// What it needs.
    pub needs: &'static [Need],
    /// When a requirement applies only sometimes, or what happens without it.
    pub notes: &'static [&'static str],
    /// Error codes of its own, beyond those its requirements imply.
    pub errors: &'static [&'static str],
    /// Key fields of its `--json` output: (field, meaning).
    pub output: &'static [(&'static str, &'static str)],
    /// The example the root help shows (default: the first of its help); lines that belong
    /// together (`mkdir … && …` then a line for ~/.zshrc) are separated by `\n`.
    pub featured: Option<&'static str>,
}

impl Entry {
    const EMPTY: Self = Self {
        path: "",
        goal: None,
        capabilities: &[],
        keywords: &[],
        needs: &[],
        notes: &[],
        errors: &[],
        output: &[],
        featured: None,
    };
}

use Need::{ControlsPlayback, Daemon, IamLogin, Network, Premium, SpotifyApp, SpotifyPlayer};

/// Reads of Spotify.app through the daemon.
const APP: &[Need] = &[Daemon, SpotifyApp];
/// Playback control: spotify_player first when it is signed in, AppleScript otherwise.
const CONTROL: &[Need] = &[Daemon, SpotifyApp, ControlsPlayback];
/// Web API reads through the daemon's spotify_player.
const WEB: &[Need] = &[Daemon, SpotifyPlayer, Network];
/// Web API reads that also look at what Spotify.app plays.
const WEB_APP: &[Need] = &[Daemon, SpotifyApp, SpotifyPlayer, Network];
/// Backend and IAM calls.
const IAM: &[Need] = &[IamLogin, Network];

const CONTROL_NOTE: &str = "Tries the Spotify Web API (spotify_player) first when it is signed in and falls back to AppleScript, so neither spotify_player nor Premium is required; under strategy spotify_player both are.";

const OUTCOME: &[(&str, &str)] = &[
    ("action", "what was done: play, pause, next, seek, …"),
    (
        "via",
        "the path that made the change, e.g. spotify_player or applescript",
    ),
    (
        "fallback.reason",
        "{code, message}: why the first path did not do it (only when the other one did)",
    ),
    (
        "playback",
        "Spotify.app right after the change (the shape of `status`'s playback)",
    ),
];

const PLAYBACK: &[(&str, &str)] = &[
    ("playback.state", "playing, paused, stopped or not_running"),
    (
        "playback.track.uri",
        "spotify:track:<id> or spotify:episode:<id>",
    ),
    ("playback.track.kind", "track or episode"),
    ("playback.track.name", "title"),
    ("playback.track.artist", "artists (empty for episodes)"),
    ("playback.track.album", "album, or the show of an episode"),
    ("playback.track.duration_ms", "length"),
    ("playback.position_ms", "position"),
    ("playback.remaining_ms", "time left"),
    ("playback.progress", "share played, 0-1"),
    ("playback.volume", "Spotify.app volume, 0-100"),
    ("playback.shuffling", "shuffle on"),
    ("playback.repeating", "repeat on"),
    (
        "playback.web",
        "--full only: context_uri, device, repeat_state, is_playing, source, stale, relinked",
    ),
    (
        "warnings[]",
        "{code, message}: web_state_stale, no_active_device, timeout, rate_limited",
    ),
    ("managed_queue", "items waiting in the managed queue"),
];

const ITEMS: &[(&str, &str)] = &[
    ("section", "which list"),
    ("total", "how many there are"),
    ("items[]", "{kind, name, by[], uri, duration}"),
];

const SEARCH: &[(&str, &str)] = &[
    ("query", "what was searched"),
    (
        "results.tracks[] (also albums, artists, playlists, shows, episodes)",
        "{kind, name, by[], album, uri, duration_ms, duration}",
    ),
];

const TRIGGER_SHOW: &[(&str, &str)] = &[
    ("trigger.id", "trg_…"),
    ("trigger.description", "the condition in words"),
    (
        "trigger.condition",
        "remaining, elapsed, end or change, with its amount",
    ),
    ("trigger.scope.kind", "current, every or track"),
    ("trigger.status", "active, completed, expired or removed"),
    (
        "trigger.delivery",
        "{ting, recipient}: where the firing goes",
    ),
    (
        "firings[]",
        "its recent firings (the fields of trigger history)",
    ),
];

const FIRING: &[(&str, &str)] = &[
    ("firings[].id", "fir_…"),
    ("firings[].trigger_id", "trg_…"),
    ("firings[].outcome", "fired or expired"),
    (
        "firings[].state",
        "delivery state: pending, delivered, failed or local",
    ),
    (
        "firings[].data",
        "the trigger, track and playback position sent",
    ),
    ("firings[].ting.id", "the Ting's id once delivered"),
    (
        "firings[].last_error",
        "{code, message} of the last failed delivery",
    ),
];

/// Every command. Order does not matter; the grammar's order is used for output.
pub const ENTRIES: &[Entry] = &[
    // ------------------------------------------------------------------------ root
    Entry {
        path: "",
        goal: None,
        capabilities: &["without a command: what is set up, what is missing, what to try"],
        keywords: &["start", "overview", "orientation", "setup", "missing"],
        needs: &[],
        output: &[
            ("ready", "whether playback works as set up now"),
            ("checks[]", "{check, ok, detail, fix, optional}"),
            ("try[]", "{command, description}: what to run next"),
        ],
        ..Entry::EMPTY
    },
    // ------------------------------------------------------------------------ listen
    Entry {
        path: "status",
        goal: Some(Goal::Listen),
        capabilities: &[
            "what is playing now, its position and time left",
            "volume, shuffle and repeat",
            "--full: the playing playlist or album, the device and repeat-one",
        ],
        keywords: &[
            "now playing",
            "current song",
            "what is playing",
            "position",
            "time left",
            "remaining",
            "how long is left",
            "song name",
        ],
        needs: APP,
        notes: &[
            "--full also reads the Web API (spotify_player); its problems are warnings, never errors.",
        ],
        output: PLAYBACK,
        featured: Some("spotify status --full"),
        ..Entry::EMPTY
    },
    Entry {
        path: "play",
        goal: Some(Goal::Listen),
        capabilities: &[
            "play a song, album, playlist, artist, episode or show by URI, link, id or search",
            "play Liked Songs in order or shuffled from a random song",
            "start a radio from a track, album, artist or playlist",
            "resume",
        ],
        keywords: &[
            "start",
            "listen",
            "put on",
            "shuffled",
            "liked songs",
            "favorites",
            "radio",
            "resume",
        ],
        needs: CONTROL,
        notes: &[
            CONTROL_NOTE,
            "--search, --radio and --liked --limit read the Web API through spotify_player.",
        ],
        errors: &[
            "nothing_to_resume",
            "not_found",
            "not_playable",
            "unsupported",
            "invalid_input",
        ],
        output: OUTCOME,
        featured: Some("spotify play --search 'arctic monkeys 505'"),
    },
    Entry {
        path: "resume",
        goal: Some(Goal::Listen),
        keywords: &["continue", "unpause", "play again"],
        needs: CONTROL,
        notes: &[CONTROL_NOTE],
        errors: &["nothing_to_resume"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "pause",
        goal: Some(Goal::Listen),
        keywords: &["stop", "hold", "silence", "stop the music", "stop playing"],
        needs: CONTROL,
        notes: &[CONTROL_NOTE],
        errors: &["nothing_playing"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "toggle",
        goal: Some(Goal::Listen),
        keywords: &["play pause", "playpause"],
        needs: CONTROL,
        notes: &[CONTROL_NOTE],
        errors: &["nothing_playing", "nothing_to_resume"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "next",
        goal: Some(Goal::Listen),
        keywords: &["skip", "forward", "next song"],
        needs: CONTROL,
        notes: &[CONTROL_NOTE],
        errors: &["nothing_playing", "not_allowed_in_context"],
        output: &[
            ("action", "next"),
            ("via", "the path that made the change"),
            ("source", "managed_queue when a managed item started"),
            ("playing", "the managed item that started"),
            ("queue_remaining", "managed items left"),
            ("playback", "Spotify.app right after the skip"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "previous",
        goal: Some(Goal::Listen),
        keywords: &["back", "rewind", "restart", "last song", "again"],
        needs: CONTROL,
        notes: &[CONTROL_NOTE],
        errors: &["nothing_playing"],
        output: &[
            ("result", "restarted (back to 0:00) or previous_item"),
            ("via", "the path that made the change"),
            ("playback", "Spotify.app right after"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "seek",
        goal: Some(Goal::Listen),
        keywords: &[
            "jump",
            "position",
            "skip ahead",
            "fast forward",
            "rewind",
            "go to",
        ],
        needs: CONTROL,
        notes: &[
            "Exact through AppleScript under strategy auto; approximate under strategy spotify_player.",
        ],
        errors: &["nothing_playing", "invalid_input"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "volume",
        goal: Some(Goal::Listen),
        keywords: &[
            "louder",
            "quieter",
            "sound",
            "loud",
            "quiet",
            "mute",
            "turn up",
            "turn down",
        ],
        needs: APP,
        notes: &["Setting it controls playback: controls_playback when a level is given."],
        errors: &["invalid_input"],
        output: &[
            ("volume", "without a level: the volume, 0-100"),
            ("playback.volume", "with a level: the new volume"),
            ("via", "with a level: the path that made the change"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "shuffle",
        goal: Some(Goal::Listen),
        keywords: &["random", "mix", "shuffled"],
        needs: CONTROL,
        notes: &[CONTROL_NOTE],
        errors: &["not_allowed_in_context", "nothing_playing"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "repeat",
        goal: Some(Goal::Listen),
        keywords: &["loop", "again", "repeat one", "on repeat"],
        needs: CONTROL,
        notes: &[
            "`repeat track` needs spotify_player (the Web API, which needs Premium); off and context work through AppleScript.",
        ],
        errors: &["not_allowed_in_context", "unsupported", "nothing_playing"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "like",
        goal: Some(Goal::Listen),
        keywords: &[
            "heart",
            "favorite",
            "favourite",
            "save",
            "love",
            "liked songs",
            "save this song",
            "save track",
            "add to liked songs",
            "like this song",
            "like the current song",
        ],
        needs: &[Daemon, SpotifyApp, SpotifyPlayer, Network],
        errors: &["track_mismatch", "unsupported", "nothing_playing"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "unlike",
        goal: Some(Goal::Listen),
        keywords: &[
            "unheart",
            "unfavorite",
            "unsave",
            "undo like",
            "remove from liked songs",
        ],
        needs: &[Daemon, SpotifyApp, SpotifyPlayer, Network],
        errors: &["track_mismatch", "unsupported", "nothing_playing"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "launch",
        goal: Some(Goal::Listen),
        keywords: &["open spotify", "start spotify", "background", "hidden"],
        needs: &[Daemon, SpotifyApp],
        output: &[
            ("launched", "whether this command started it"),
            ("already_running", "it was running before"),
            ("playback", "Spotify.app once it answers"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "devices",
        goal: Some(Goal::Listen),
        keywords: &[
            "speaker", "phone", "cast", "connect", "transfer", "output", "airplay",
        ],
        needs: WEB,
        output: &[("devices[]", "{id, name, type, is_active, volume_percent}")],
        ..Entry::EMPTY
    },
    Entry {
        path: "devices list",
        keywords: &[
            "speakers",
            "phones",
            "outputs",
            "available devices",
            "which devices",
        ],
        needs: WEB,
        output: &[("devices[]", "{id, name, type, is_active, volume_percent}")],
        ..Entry::EMPTY
    },
    Entry {
        path: "devices connect",
        keywords: &[
            "transfer playback",
            "cast",
            "move to speaker",
            "play on phone",
            "switch device",
            "another device",
            "change device",
        ],
        needs: &[Daemon, SpotifyPlayer, Network, Premium, ControlsPlayback],
        notes: &["Moving playback is a Web API playback command, which needs Premium."],
        errors: &["not_found", "invalid_input", "no_active_device"],
        output: &[
            ("connected", "the device id or name"),
            ("message", "spotify_player's answer"),
        ],
        ..Entry::EMPTY
    },
    // ------------------------------------------------------------------------ find
    Entry {
        path: "search",
        goal: Some(Goal::Find),
        capabilities: &[
            "search songs, albums, artists, playlists, podcast shows and episodes",
            "every hit has a URI for play, queue add, playlist add, track and lyrics",
            "--play plays the first hit",
        ],
        keywords: &["find", "look up", "lookup", "uri", "id", "which song"],
        needs: WEB,
        notes: &["--play also controls playback."],
        errors: &["invalid_input", "not_found", "not_playable"],
        output: SEARCH,
        ..Entry::EMPTY
    },
    Entry {
        path: "library",
        goal: Some(Goal::Find),
        capabilities: &["Liked Songs, saved albums, followed artists, top tracks, playlists"],
        keywords: &[
            "liked songs",
            "saved",
            "favorites",
            "my music",
            "collection",
            "top tracks",
        ],
        needs: WEB,
        errors: &["invalid_input"],
        output: ITEMS,
        ..Entry::EMPTY
    },
    // ------------------------------------------------------------------------ details
    Entry {
        path: "track",
        goal: Some(Goal::Details),
        featured: Some("spotify track spotify:album:78bpIziExqiI9qztvNFlQu"),
        capabilities: &[
            "the current song: artists, album, release date, length, popularity, liked",
            "any track, album, artist or playlist by URI, link or id",
            "an album's track list, an artist's top tracks, albums and related artists",
        ],
        keywords: &[
            "song info",
            "details",
            "info",
            "about",
            "credits",
            "who sings",
            "release date",
            "album tracks",
            "artist",
        ],
        needs: WEB_APP,
        notes: &["Without a target it reads Spotify.app; with one, only the Web API."],
        errors: &["not_found", "invalid_input", "nothing_playing"],
        output: &[
            (
                "track",
                "no target: the current song (uri, name, artist, album, duration)",
            ),
            ("liked", "the song is in Liked Songs (absent when unknown)"),
            ("kind", "with a target: track, album, artist or playlist"),
            ("item", "with a target: {name, by[], uri, …}"),
            (
                "item.album, item.album_uri",
                "a song's album: its name and spotify:album: URI",
            ),
            (
                "album",
                "no target: the current song's album ({id, name, …})",
            ),
            ("tracks[]", "an album's or playlist's tracks"),
            ("top_tracks[], albums[], related_artists[]", "an artist's"),
            ("release_date", "an album's"),
        ],
    },
    Entry {
        path: "lyrics",
        goal: Some(Goal::Details),
        featured: Some("spotify lyrics spotify:track:0BxE4FqsDD1Ot4YuBXwAPp"),
        capabilities: &[
            "lyrics of the song playing now",
            "lyrics of any track by URI, link or id; nothing has to play (find the URI with spotify search)",
        ],
        keywords: &[
            "words",
            "text",
            "sing along",
            "karaoke",
            "song text",
            "not playing",
            "without playing",
        ],
        needs: WEB_APP,
        notes: &[
            "With a target, Spotify.app is not needed.",
            "It takes a track, not search words: find the track with `spotify search '<words>' --type track`, then pass its URI.",
        ],
        errors: &["not_found", "invalid_input", "nothing_playing"],
        output: &[
            ("track", "spotify:track:<id>"),
            ("title", "the song"),
            ("lines[]", "the lyrics, line by line"),
            ("text", "the lyrics as one text"),
            ("synced", "whether lines carry times (false)"),
        ],
    },
    // ------------------------------------------------------------------------ queue
    Entry {
        path: "queue",
        goal: Some(Goal::Queue),
        capabilities: &[
            "see what plays next: the managed queue and Spotify's own upcoming items",
            "add, remove, reorder and clear queued songs and episodes",
        ],
        keywords: &["up next", "upcoming", "later", "coming up"],
        needs: WEB_APP,
        output: &[
            ("managed[]", "{id, uri, name, by}: plays next, editable"),
            (
                "spotify_upcoming.items[]",
                "Spotify's own upcoming items, read-only",
            ),
            (
                "spotify_upcoming.error",
                "why Spotify's list could not be read",
            ),
            (
                "warnings[]",
                "{code, message}: e.g. no_active_device (no device plays, so Spotify lists nothing upcoming)",
            ),
        ],
        featured: Some("spotify queue add --search 'song name' --next"),
        ..Entry::EMPTY
    },
    Entry {
        path: "queue list",
        keywords: &[
            "show queue",
            "see the queue",
            "up next",
            "next up",
            "coming up next",
            "what plays next",
            "in my queue",
        ],
        needs: WEB_APP,
        output: &[
            ("managed[]", "{id, uri, name, by}"),
            ("spotify_upcoming.items[]", "Spotify's own upcoming items"),
            ("warnings[]", "{code, message}: e.g. no_active_device"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "queue add",
        keywords: &[
            "enqueue",
            "play next",
            "play later",
            "add to queue",
            "up next",
            "queue up",
        ],
        needs: &[Daemon, SpotifyApp],
        notes: &[
            "--search, and items without known names, read the Web API through spotify_player.",
        ],
        errors: &["invalid_input", "not_found", "spotify_auth_required"],
        output: &[
            ("added[]", "{id, uri, name, by}"),
            ("queue[]", "the whole managed queue now"),
            ("note", "how and when they play"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "queue remove",
        keywords: &[
            "delete from queue",
            "remove from the queue",
            "unqueue",
            "drop",
        ],
        needs: &[Daemon],
        errors: &["not_found"],
        output: &[("removed", "{id, uri, name, by}")],
        ..Entry::EMPTY
    },
    Entry {
        path: "queue move",
        keywords: &["reorder", "move up", "priority"],
        needs: &[Daemon],
        errors: &["not_found", "invalid_input"],
        output: &[
            ("moved", "{id, uri, name, by}"),
            ("position", "its new position (1 = next)"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "queue clear",
        keywords: &["empty queue", "reset queue"],
        needs: &[Daemon],
        output: &[("cleared", "how many managed items were removed")],
        ..Entry::EMPTY
    },
    // ------------------------------------------------------------------------ playlists
    Entry {
        path: "playlist",
        goal: Some(Goal::Playlists),
        capabilities: &[
            "list, show and play playlists",
            "create and delete playlists; add or remove tracks and whole albums",
            "fork any playlist into your own; import one into another and keep it in sync",
        ],
        keywords: &["mix", "list", "collection"],
        needs: WEB,
        featured: Some("spotify playlist play <playlist-id> --shuffle"),
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist list",
        keywords: &["my playlists", "all playlists"],
        needs: WEB,
        errors: &["invalid_input"],
        output: ITEMS,
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist show",
        keywords: &["tracks in playlist", "open playlist", "contents"],
        needs: WEB,
        errors: &["not_found", "invalid_input"],
        output: &[
            ("playlist", "{name, uri, owner}"),
            ("tracks[]", "{name, by[], uri, duration}"),
            ("track_count", "how many tracks"),
            ("duration", "total length"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist create",
        keywords: &["new playlist", "make playlist"],
        needs: WEB,
        errors: &["invalid_input"],
        output: &[
            ("id", "the new playlist's bare id"),
            ("uri", "spotify:playlist:<id>"),
            ("name", "its name"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist delete",
        keywords: &["remove playlist", "unfollow"],
        needs: WEB,
        errors: &["not_found", "invalid_input"],
        output: &[("message", "what happened"), ("note", "how to restore it")],
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist add",
        keywords: &["add song to playlist", "save to playlist", "add album"],
        needs: WEB,
        errors: &[
            "invalid_input",
            "unsupported",
            "permission_denied",
            "not_found",
        ],
        output: &[("results[]", "{message} per item")],
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist remove",
        keywords: &["delete song from playlist", "take out"],
        needs: WEB,
        errors: &[
            "invalid_input",
            "unsupported",
            "permission_denied",
            "not_found",
        ],
        output: &[("results[]", "{message} per item")],
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist play",
        keywords: &["start playlist", "shuffle playlist"],
        needs: CONTROL,
        notes: &[CONTROL_NOTE],
        errors: &["invalid_input", "not_found"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist rename",
        keywords: &["change name", "edit description"],
        needs: &[Daemon],
        errors: &["unsupported"],
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist import",
        keywords: &[
            "copy tracks",
            "merge playlists",
            "mirror",
            "into another playlist",
        ],
        needs: WEB,
        errors: &["invalid_input", "not_found", "permission_denied"],
        output: &[("message", "what was copied")],
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist fork",
        capabilities: &[
            "copy any playlist into a new one you own, kept in sync by playlist sync",
            "create a new playlist from another, existing playlist",
        ],
        keywords: &[
            "copy playlist",
            "duplicate",
            "duplicate playlist",
            "clone",
            "clone playlist",
            "make my own",
            "from another playlist",
            "from an existing playlist",
            "existing playlist",
            "copy of a playlist",
        ],
        needs: WEB,
        errors: &["invalid_input", "not_found"],
        output: &[
            ("forked", "true"),
            ("from", "the source playlist's id"),
            ("id", "the new playlist's id"),
            ("uri", "spotify:playlist:<id>"),
            ("name", "its name"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "playlist sync",
        capabilities: &["keep forks and imports up to date with their sources"],
        keywords: &[
            "update copies",
            "refresh import",
            "keep a copy up to date",
            "catch up",
        ],
        needs: WEB,
        errors: &["invalid_input", "not_found"],
        output: &[("message", "what was synced")],
        ..Entry::EMPTY
    },
    // ------------------------------------------------------------------------ podcasts
    Entry {
        path: "podcast",
        goal: Some(Goal::Podcasts),
        capabilities: &[
            "search podcast shows and episodes",
            "play an episode or a show's latest/next episode",
            "list saved shows; see the episode playing",
        ],
        keywords: &["show", "episode", "talk", "pod"],
        needs: WEB,
        featured: Some("spotify podcast search 'tim ferriss'"),
        ..Entry::EMPTY
    },
    Entry {
        path: "podcast search",
        keywords: &["find podcast", "find episode"],
        needs: WEB,
        errors: &["invalid_input"],
        output: &[(
            "results.shows[], results.episodes[]",
            "{kind, name, by[], album, uri, duration}",
        )],
        ..Entry::EMPTY
    },
    Entry {
        path: "podcast play",
        keywords: &["listen to podcast", "play episode"],
        needs: CONTROL,
        notes: &[CONTROL_NOTE],
        errors: &["invalid_input", "not_found", "unsupported", "not_playable"],
        output: OUTCOME,
        ..Entry::EMPTY
    },
    Entry {
        path: "podcast saved",
        keywords: &["followed shows", "my podcasts", "subscriptions"],
        needs: WEB,
        output: &[("shows[]", "{name, by[], uri}")],
        ..Entry::EMPTY
    },
    Entry {
        path: "podcast now",
        keywords: &["current episode", "episode playing"],
        needs: APP,
        errors: &["not_a_podcast"],
        output: PLAYBACK,
        ..Entry::EMPTY
    },
    // ------------------------------------------------------------------------ triggers
    Entry {
        path: "trigger",
        goal: Some(Goal::Triggers),
        capabilities: &[
            "a Ting when a song has 30 s or 25% left, is halfway, at 1:30, over, or changes",
            "for the song playing now, every song, or every play of one track",
            "local triggers without Ting that `trigger wait` blocks on",
        ],
        keywords: &[
            "notify",
            "notification",
            "remind",
            "reminder",
            "alert",
            "ping",
            "tell me",
            "before the song ends",
            "when the song ends",
            "checkpoint",
            "webhook",
        ],
        needs: &[Daemon, SpotifyApp, IamLogin, Network],
        notes: &["iam_login and network: not with --local."],
        featured: Some("spotify trigger add --remaining 30s --note 'wrap up'"),
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger add",
        capabilities: &[
            "--remaining 30s or 25%: before the song ends",
            "--elapsed 50% or --at 1:30: once that much has played",
            "--end: when the song finishes; --change: when it stops being current",
            "--scope every or track, --times, --note, --label, --local",
        ],
        keywords: &[
            "notify",
            "remind",
            "alert",
            "ping",
            "ting me",
            "before the song ends",
            "when the song ends",
            "halfway",
            "seconds left",
            "time left",
        ],
        needs: &[Daemon, SpotifyApp, IamLogin, Network],
        notes: &[
            "iam_login and network: not with --local (firings are then only recorded; see trigger wait).",
        ],
        errors: &[
            "threshold_passed",
            "invalid_input",
            "nothing_playing",
            "recipient_not_registered",
        ],
        output: &[
            ("trigger.id", "trg_…"),
            ("trigger.description", "the condition in words"),
            ("trigger.scope.kind", "current, every or track"),
            ("trigger.status", "active"),
            ("trigger.delivery", "{ting, recipient}"),
            ("now_playing", "the song a current-scope trigger watches"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger list",
        keywords: &["my triggers", "active triggers", "reminders"],
        needs: &[Daemon],
        output: &[(
            "triggers[]",
            "trigger objects: id, description, scope, status, fired, note, label",
        )],
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger show",
        keywords: &["trigger details"],
        needs: &[Daemon],
        errors: &["not_found"],
        output: TRIGGER_SHOW,
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger remove",
        keywords: &[
            "delete trigger",
            "cancel trigger",
            "cancel reminder",
            "stop notifications",
            "stop reminders",
        ],
        needs: &[Daemon],
        errors: &["not_found", "permission_denied"],
        output: &[("removed[]", "the removed trigger id")],
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger clear",
        keywords: &[
            "remove all triggers",
            "cancel all",
            "stop all notifications",
        ],
        needs: &[Daemon],
        output: &[("removed[]", "the removed trigger ids")],
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger history",
        keywords: &["firings", "delivered", "did it fire", "log"],
        needs: &[Daemon],
        errors: &["invalid_input", "not_found"],
        output: FIRING,
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger test",
        keywords: &["test notification", "check ting", "delivery test"],
        needs: &[Daemon, IamLogin, Network],
        errors: &["not_found", "recipient_not_registered", "recipient_changed"],
        output: &[
            ("delivered", "true"),
            ("firing.ting.id", "the delivered Ting's id"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger wait",
        keywords: &[
            "block",
            "wait until",
            "sleep until",
            "without ting",
            "local",
        ],
        needs: &[Daemon],
        errors: &["not_found", "timeout", "invalid_input"],
        output: &[
            (
                "firing",
                "the firing (null when the trigger ended without one)",
            ),
            ("note", "why it returned without a firing"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "trigger retry",
        keywords: &["resend", "redeliver", "failed delivery"],
        needs: &[Daemon, IamLogin, Network],
        errors: &["not_found"],
        output: &[
            ("firing", "the firing"),
            ("queued", "true: queued for delivery again"),
            ("note", "when it was already delivered"),
        ],
        ..Entry::EMPTY
    },
    // ------------------------------------------------------------------------ account
    Entry {
        path: "iam",
        goal: Some(Goal::Account),
        capabilities: &["the IAM app id, owning org, scopes and URLs, offline"],
        keywords: &[
            "app id",
            "discovery",
            "slt",
            "short-lived token",
            "scopes",
            "iam cli",
            "install iam",
        ],
        output: &[
            ("app_id", "spotify"),
            ("org_id", "the owning organization"),
            ("scopes[]", "what a login grants"),
            ("login", "how to mint an SLT and log in"),
            (
                "iam_session",
                "which `iam` session mints it, and the first sign-in a fresh SILICON_HOME needs",
            ),
            ("api_url, iam_url, auth_url", "service URLs"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "login",
        goal: Some(Goal::Account),
        // Not the first example (installing the iam CLI): the listing shows what `login` does.
        featured: Some("spotify login 'oac_…'"),
        capabilities: &[
            "exchange an IAM short-lived token (SLT) for a session",
            "register as a Ting recipient so triggers can notify you",
        ],
        keywords: &[
            "sign in",
            "log in",
            "authenticate",
            "slt",
            "short-lived token",
            "token",
            "identity",
            "silicon",
        ],
        needs: &[Network],
        errors: &[
            "slt_rejected",
            "invalid_input",
            "backend_unavailable",
            "dependency_unavailable",
        ],
        output: &[
            ("authenticated", "true"),
            ("actor.public_id", "who you are, e.g. si:you"),
            ("org_id", "your organization"),
            ("scopes[]", "granted scopes"),
            ("ting.subscribed", "registered as a Ting recipient"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "login status",
        keywords: &["am i logged in", "who am i", "whoami", "session"],
        needs: &[Network],
        notes: &["Without a saved session it answers offline: authenticated false."],
        output: &[
            (
                "authenticated",
                "true or false (never an error for a missing session)",
            ),
            ("reason", "why not"),
            ("actor.public_id", "who you are"),
            ("identity", "what the backend knows about you"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "logout",
        goal: Some(Goal::Account),
        keywords: &["sign out", "log out", "forget session"],
        needs: &[Network],
        notes: &["Offline, the session is removed locally anyway."],
        output: &[
            ("authenticated", "false"),
            ("revoked", "whether IAM revoked it"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "ting",
        goal: Some(Goal::Account),
        keywords: &["notification recipient", "register"],
        needs: IAM,
        ..Entry::EMPTY
    },
    Entry {
        path: "ting register",
        keywords: &["register recipient", "enable notifications"],
        needs: IAM,
        errors: &["recipient_not_registered"],
        output: &[
            ("ting.subscribed", "true"),
            ("subscription", "Ting's answer"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "ting status",
        keywords: &["recipient status"],
        needs: &[IamLogin],
        output: &[("actor", "who you are"), ("ting.subscribed", "registered")],
        ..Entry::EMPTY
    },
    Entry {
        path: "auth",
        goal: Some(Goal::Account),
        capabilities: &["sign spotify_player in to Spotify once, in a browser; check it"],
        keywords: &[
            "spotify account",
            "spotify sign in",
            "oauth",
            "consent",
            "agree",
            "web api",
        ],
        needs: &[Daemon],
        ..Entry::EMPTY
    },
    Entry {
        path: "auth status",
        keywords: &["signed in to spotify", "spotify_player auth"],
        needs: &[Daemon],
        output: &[
            ("authenticated", "spotify_player has Spotify tokens"),
            ("installed", "spotify_player is installed"),
            ("version", "spotify_player's version"),
            ("error", "{code, message, hint} when not"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "auth login",
        keywords: &[
            "sign in to spotify",
            "browser login",
            "authorize",
            "consent page",
            "click agree",
            "headless",
        ],
        needs: &[Daemon],
        notes: &[
            "Opens Spotify's consent page in a browser on this Mac; a Carbon clicks Agree once. A headless Silicon asks a Carbon at the Mac to run it.",
        ],
        errors: &["spotify_player_missing"],
        output: &[
            ("started", "the browser sign-in started"),
            ("pid", "its process"),
            ("log", "its log"),
            ("next", "what to do"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "testing",
        goal: Some(Goal::Account),
        keywords: &["test plane", "sandbox", "staging"],
        ..Entry::EMPTY
    },
    Entry {
        path: "testing use",
        keywords: &["select test plane", "test app secret"],
        errors: &["invalid_input"],
        output: &[
            ("testing", "true"),
            ("slot", "the session slot for this plane"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "testing status",
        output: &[
            ("testing", "a plane is selected"),
            ("source", "env, testing.json or none"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "testing exit",
        keywords: &["production"],
        output: &[("testing", "false")],
        ..Entry::EMPTY
    },
    // ------------------------------------------------------------------------ setup
    Entry {
        path: "doctor",
        goal: Some(Goal::Setup),
        capabilities: &[
            "checks Spotify.app, spotify_player and its sign-in, the automation permission, the daemon, the backend and login",
            "every failing check comes with the exact fix",
        ],
        keywords: &[
            "diagnose",
            "broken",
            "not working",
            "fix",
            "check",
            "health",
            "permission",
            "automation",
            "denied",
            "allow",
            "why",
        ],
        needs: &[Daemon, Network],
        notes: &["Offline, the backend check fails and the others still run."],
        errors: &["doctor_failed"],
        output: &[
            ("ok", "every required check passed"),
            ("checks[]", "{check, ok, detail, fix, optional}"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "setup",
        goal: Some(Goal::Setup),
        capabilities: &[
            "installs spotify_player (and Spotify.app with --install-apps) with Homebrew",
            "installs and starts the daemon at login; starts the Spotify sign-in",
        ],
        keywords: &[
            "install",
            "get started",
            "first time",
            "dependencies",
            "homebrew",
        ],
        needs: &[Daemon, Network],
        errors: &["homebrew_missing", "install_failed", "launchd_failed"],
        output: &[
            ("steps[]", "{step, status, fix}"),
            ("next", "what to run next"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon",
        goal: Some(Goal::Setup),
        capabilities: &[
            "start, stop, restart, install at login, status and logs of spotify-daemon",
        ],
        keywords: &["background", "service", "launchd", "agent", "logs"],
        needs: &[Daemon],
        featured: Some("spotify daemon status"),
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon start",
        keywords: &["run daemon"],
        needs: &[Daemon],
        output: &[
            ("started", "this command started it"),
            ("already_running", "it was running"),
            ("status", "the daemon's status"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon stop",
        needs: &[Daemon],
        errors: &["daemon_stuck"],
        output: &[("stopped", "true"), ("was_running", "it was running")],
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon restart",
        keywords: &["reload", "restart the daemon", "restart spotify-daemon"],
        needs: &[Daemon],
        errors: &["daemon_stuck"],
        output: &[
            ("restarted", "true"),
            ("via", "launchd or spawn"),
            ("previous_pid", "the old process"),
            ("status", "the new daemon's status"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon status",
        keywords: &[
            "daemon version",
            "daemon running",
            "is it running",
            "alive",
            "uptime",
            "automation",
            "permission",
            "warm spotify_player",
        ],
        needs: &[Daemon],
        notes: &["Never starts the daemon: `running: false` when it is down."],
        output: &[
            ("running", "whether it answers"),
            ("version, pid, uptime_s", "the process"),
            ("spotify", "{state, track} as it last saw Spotify.app"),
            ("counts", "active triggers, pending and failed deliveries"),
            (
                "warm_spotify_player.state",
                "running, starting, deferred, waiting_for_spotify_auth, …",
            ),
            ("automation", "granted, denied or not_answering"),
            ("launch_agent_installed", "it starts at login"),
            ("log", "the log file"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon run",
        keywords: &["foreground", "supervisor"],
        needs: &[Daemon],
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon install",
        keywords: &["start at login", "launch agent", "autostart"],
        needs: &[Daemon],
        errors: &["launchd_failed", "daemon_stuck"],
        output: &[
            ("installed", "true"),
            ("plist", "the launchd agent"),
            ("binary", "the daemon binary"),
            ("log", "the log file"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon uninstall",
        keywords: &["remove launch agent"],
        needs: &[Daemon],
        output: &[
            ("uninstalled", "true"),
            ("agent_removed", "the launchd agent was removed"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "daemon logs",
        keywords: &["log file", "debug"],
        output: &[("log", "the log file"), ("lines[]", "its last lines")],
        ..Entry::EMPTY
    },
    Entry {
        path: "config",
        goal: Some(Goal::Setup),
        capabilities: &[
            "set settings with one JSON object, show them, get one, list every key, reset",
        ],
        keywords: &[
            "settings",
            "preferences",
            "options",
            "strategy",
            "telemetry",
            "default",
            "environment",
            "hints",
            "next suggestions",
        ],
        featured: Some("spotify config keys"),
        ..Entry::EMPTY
    },
    Entry {
        path: "config set",
        capabilities: &[
            "change any setting: telemetry, the playback strategy (auto, spotify_player only or applescript only), launch_spotify, search_limit, keep_spotify_in_background",
            "change a default: search results, output format, verify timeout, auto updates",
        ],
        keywords: &[
            "change setting",
            "turn off telemetry",
            "set strategy",
            "change the default",
            "set the default",
            "by default",
        ],
        errors: &["invalid_input"],
        output: &[
            ("updated", "the keys set, with their new values"),
            ("path", "config.json"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "config show",
        keywords: &["current settings"],
        output: &[
            ("<key>", "every key's effective value"),
            ("path", "config.json"),
            ("env_overrides", "environment variables in effect"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "config get",
        keywords: &["read setting"],
        errors: &["invalid_input"],
        output: &[("key", "the key"), ("value", "its effective value")],
        ..Entry::EMPTY
    },
    Entry {
        path: "config keys",
        keywords: &["available settings", "list settings"],
        output: &[("keys[]", "{key, type, default, description}")],
        ..Entry::EMPTY
    },
    Entry {
        path: "config reset",
        keywords: &["defaults", "factory reset"],
        output: &[("reset", "true")],
        ..Entry::EMPTY
    },
    Entry {
        path: "update",
        goal: Some(Goal::Setup),
        keywords: &["upgrade", "new version", "latest", "version"],
        needs: &[Daemon, Network],
        errors: &[
            "no_release",
            "download_failed",
            "checksum_missing",
            "checksum_mismatch",
            "unpack_failed",
            "update_not_writable",
        ],
        output: &[
            ("update_available", "a newer release exists"),
            ("latest", "its version"),
            ("restarting", "it was installed and the daemon restarts"),
            ("manager", "honeycomb when Honeycomb manages the install"),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "report",
        goal: Some(Goal::Setup),
        featured: Some("spotify report 'what I ran, what happened, what I expected'"),
        keywords: &[
            "bug",
            "issue",
            "crash",
            "feedback",
            "pull request",
            "problem",
        ],
        needs: &[Network],
        notes: &[
            "Offline, the report is saved locally with a `gh issue create` command to file it.",
        ],
        errors: &["invalid_input"],
        output: &[
            ("id", "the report's id"),
            ("issue_url", "the GitHub issue, when one was opened"),
            ("filed", "false when it was saved locally instead"),
            (
                "saved, file_it_yourself",
                "where it was saved and how to file it",
            ),
        ],
        ..Entry::EMPTY
    },
    Entry {
        path: "docs",
        goal: Some(Goal::Setup),
        capabilities: &[
            "offline guides: read a topic, one section, search them all, or print everything",
        ],
        keywords: &[
            "guide",
            "manual",
            "documentation",
            "read",
            "explain",
            "concepts",
            "search the docs",
            "search the guides",
        ],
        errors: &["not_found"],
        output: &[
            ("topics[]", "no topic: {topic, title, command}"),
            ("topic, title, content", "a topic (markdown)"),
            ("section", "with --section: the section's heading"),
            (
                "results[]",
                "with --search: {topic, title, command, matches[]}",
            ),
        ],
        featured: Some("spotify docs triggers"),
        ..Entry::EMPTY
    },
    Entry {
        path: "how",
        goal: Some(Goal::Setup),
        capabilities: &[
            "answers questions in plain words with the commands to run and the guide section to read",
        ],
        keywords: &["help", "question", "which command", "ask", "plain words"],
        errors: &["invalid_input"],
        output: &[
            ("commands[]", "{command, summary, examples[], help, score}"),
            ("guides[]", "{topic, section, command, excerpt, score}"),
            ("errors[]", "{code, exit, meaning, fix, score}"),
        ],
        featured: Some("spotify how \"play my liked songs shuffled\""),
        ..Entry::EMPTY
    },
    Entry {
        path: "commands",
        goal: Some(Goal::Setup),
        capabilities: &[
            "every command's arguments, examples, output fields, error codes and requirements",
        ],
        keywords: &[
            "manifest",
            "schema",
            "machine readable",
            "agents",
            "tree",
            "all commands",
            "list every command",
            "every command as json",
            "read only commands",
            "safe commands",
        ],
        output: &[
            (
                "commands[]",
                "one object per command (see the fields of this one)",
            ),
            ("goals[]", "{id, title, commands[]}"),
            ("errors[]", "{code, exit, meaning, fix}"),
            ("exit_codes[]", "{exit, meaning}"),
        ],
        featured: Some("spotify commands --json"),
        ..Entry::EMPTY
    },
    Entry {
        path: "completions",
        goal: Some(Goal::Setup),
        keywords: &[
            "tab completion",
            "autocomplete",
            "shell",
            "zsh",
            "bash",
            "fish",
            "powershell",
        ],
        output: &[("shell", "the shell"), ("script", "the completion script")],
        featured: Some(
            "mkdir -p ~/.zfunc && spotify completions zsh > ~/.zfunc/_spotify\necho 'fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc",
        ),
        ..Entry::EMPTY
    },
];

/// What running a command can change, for `spotify commands --json` (`mutates`, `changes`), so
/// a Silicon can pick the commands that only read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Playback,
    Library,
    Playlists,
    Config,
    Triggers,
    Daemon,
    Session,
}

impl Change {
    /// Every kind of change with its manifest id and meaning.
    pub const ALL: [(Self, &'static str, &'static str); 7] = [
        (
            Self::Playback,
            "playback",
            "what Spotify.app plays and how: start, pause, skip, seek, volume, shuffle, repeat, the device, and the managed queue",
        ),
        (
            Self::Library,
            "library",
            "your Liked Songs: which songs are saved in it (like, unlike). Not playlists (see playlists); saved albums, followed artists and shows are only read",
        ),
        (
            Self::Playlists,
            "playlists",
            "your playlists: create, delete, their tracks, imports, forks and syncs",
        ),
        (
            Self::Config,
            "config",
            "spotify-cli's settings (config.json)",
        ),
        (
            Self::Triggers,
            "triggers",
            "triggers and their firings and Ting deliveries",
        ),
        (
            Self::Daemon,
            "daemon",
            "spotify-daemon: its process, its launchd agent, or the installed version",
        ),
        (
            Self::Session,
            "session",
            "sign-ins: the IAM login session, the Ting registration, the testing plane, or spotify_player's Spotify sign-in",
        ),
    ];

    /// The manifest id.
    #[must_use]
    pub fn id(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(c, ..)| *c == self)
            .map_or("", |(_, id, _)| *id)
    }
}

use Change as C;

/// Every command with what it can change and, when some forms of it only read, which. A test
/// checks that every command of the grammar is here, so a new one cannot go unclassified.
/// Commands of the daemon start it when it is not running; that alone is no change.
const EFFECTS: &[(&str, &[Change], Option<&str>)] = &[
    ("", &[], None),
    ("status", &[], None),
    ("play", &[C::Playback], None),
    ("resume", &[C::Playback], None),
    ("pause", &[C::Playback], None),
    ("toggle", &[C::Playback], None),
    ("next", &[C::Playback], None),
    ("previous", &[C::Playback], None),
    ("seek", &[C::Playback], None),
    (
        "volume",
        &[C::Playback],
        Some("without a level (it shows the volume)"),
    ),
    ("shuffle", &[C::Playback], None),
    ("repeat", &[C::Playback], None),
    ("like", &[C::Library], None),
    ("unlike", &[C::Library], None),
    ("launch", &[C::Playback], None),
    ("track", &[], None),
    ("lyrics", &[], None),
    ("search", &[C::Playback], Some("without --play")),
    ("library", &[], None),
    ("queue", &[], None),
    ("queue list", &[], None),
    ("queue add", &[C::Playback], None),
    ("queue remove", &[C::Playback], None),
    ("queue move", &[C::Playback], None),
    ("queue clear", &[C::Playback], None),
    ("playlist", &[], None),
    ("playlist list", &[], None),
    ("playlist show", &[], None),
    ("playlist create", &[C::Playlists], None),
    ("playlist delete", &[C::Playlists], None),
    ("playlist add", &[C::Playlists], None),
    ("playlist remove", &[C::Playlists], None),
    ("playlist play", &[C::Playback], None),
    // Always refused: renaming is not possible with the underlying tools.
    ("playlist rename", &[], None),
    ("playlist import", &[C::Playlists], None),
    ("playlist fork", &[C::Playlists], None),
    ("playlist sync", &[C::Playlists], None),
    ("podcast", &[], None),
    ("podcast search", &[], None),
    ("podcast play", &[C::Playback], None),
    ("podcast saved", &[], None),
    ("podcast now", &[], None),
    ("devices", &[], None),
    ("devices list", &[], None),
    ("devices connect", &[C::Playback], None),
    ("trigger", &[], None),
    ("trigger add", &[C::Triggers], None),
    ("trigger list", &[], None),
    ("trigger show", &[], None),
    ("trigger remove", &[C::Triggers], None),
    ("trigger clear", &[C::Triggers], None),
    ("trigger history", &[], None),
    // Sends a test Ting and records its firing.
    ("trigger test", &[C::Triggers], None),
    ("trigger wait", &[], None),
    ("trigger retry", &[C::Triggers], None),
    ("iam", &[], None),
    ("login", &[C::Session], None),
    ("login status", &[], None),
    ("logout", &[C::Session], None),
    ("ting", &[], None),
    ("ting register", &[C::Session], None),
    ("ting status", &[], None),
    ("auth", &[], None),
    ("auth status", &[], None),
    ("auth login", &[C::Session], None),
    ("config", &[], None),
    ("config set", &[C::Config], None),
    ("config show", &[], None),
    ("config get", &[], None),
    ("config keys", &[], None),
    ("config reset", &[C::Config], None),
    ("testing", &[], None),
    ("testing use", &[C::Session], None),
    ("testing status", &[], None),
    ("testing exit", &[C::Session], None),
    ("doctor", &[], None),
    (
        "setup",
        &[C::Daemon, C::Session],
        Some("with --check (it only reports)"),
    ),
    ("daemon", &[], None),
    ("daemon start", &[C::Daemon], None),
    ("daemon stop", &[C::Daemon], None),
    ("daemon restart", &[C::Daemon], None),
    ("daemon status", &[], None),
    ("daemon run", &[C::Daemon], None),
    ("daemon install", &[C::Daemon], None),
    ("daemon uninstall", &[C::Daemon], None),
    ("daemon logs", &[], None),
    (
        "update",
        &[C::Daemon],
        Some("with --check (it only reports)"),
    ),
    // Sends a bug report to the maintainers: nothing on this Mac changes, but it is no read.
    ("report", &[], None),
    ("docs", &[], None),
    ("commands", &[], None),
    ("how", &[], None),
    ("completions", &[], None),
];

/// Commands that change nothing here but send something out (a bug report): not read-only.
const SENDS: &[&str] = &["report"];

/// What a command can change, and when it changes nothing.
#[must_use]
pub fn effects(path: &str) -> (&'static [Change], Option<&'static str>) {
    EFFECTS
        .iter()
        .find(|(p, ..)| *p == path)
        .map_or((&[], None), |(_, changes, read_only)| {
            (*changes, *read_only)
        })
}

/// The commands that can change `id` (a [`Change`] id such as `library`), in grammar order.
#[must_use]
pub fn changed_by(id: &str) -> Vec<&'static str> {
    EFFECTS
        .iter()
        .filter(|(_, changes, _)| changes.iter().any(|c| c.id() == id))
        .map(|(path, ..)| *path)
        .collect()
}

/// Whether a command can change anything (or send something out).
#[must_use]
pub fn mutates(path: &str) -> bool {
    !effects(path).0.is_empty() || SENDS.contains(&path)
}

/// Whether a command can run without changing anything: it only reads, or has a form that only
/// reads (`read_only_when`).
#[must_use]
pub fn has_read_only_form(path: &str) -> bool {
    !mutates(path) || effects(path).1.is_some()
}

/// The catalog entry for a command path (`trigger add`; `` for the root).
#[must_use]
pub fn entry(path: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|e| e.path == path)
}

/// The goal of a command: its own, else its top-level command's.
#[must_use]
pub fn goal(path: &str) -> Option<Goal> {
    entry(path).and_then(|e| e.goal).or_else(|| {
        let top = path.split(' ').next().unwrap_or("");
        (top != path).then(|| goal(top)).flatten()
    })
}

/// Semantic type, range and dynamic default of an argument, where clap's own type says less:
/// (command path, argument id, type, min, max, default).
type ArgSpec = (
    &'static str,
    &'static str,
    &'static str,
    Option<i64>,
    Option<i64>,
    Option<&'static str>,
);

const ARG_SPECS: &[ArgSpec] = &[
    ("play", "target", "spotify_ref", None, None, Some("resume")),
    ("play", "search", "search_query", None, None, None),
    (
        "play",
        "context",
        "spotify_ref",
        None,
        None,
        Some("the song's album or the episode's show"),
    ),
    ("play", "radio", "spotify_ref", None, None, None),
    ("play", "limit", "integer", Some(1), None, None),
    ("seek", "position", "position", None, None, None),
    (
        "volume",
        "level",
        "volume",
        Some(0),
        Some(100),
        Some("show the volume"),
    ),
    (
        "track",
        "target",
        "spotify_ref",
        None,
        None,
        Some("the song playing now"),
    ),
    (
        "lyrics",
        "target",
        "spotify_ref",
        None,
        None,
        Some("the song playing now"),
    ),
    ("search", "query", "search_query", None, None, None),
    (
        "search",
        "limit",
        "integer",
        Some(1),
        Some(10),
        Some("config search_limit, else 10"),
    ),
    ("library", "limit", "integer", Some(1), None, Some("all")),
    ("queue add", "items", "spotify_ref", None, None, None),
    ("queue add", "search", "search_query", None, None, None),
    ("queue remove", "item", "queue_item", None, None, None),
    ("queue move", "item", "queue_item", None, None, None),
    ("queue move", "to", "integer", Some(1), None, None),
    (
        "playlist list",
        "limit",
        "integer",
        Some(1),
        None,
        Some("all"),
    ),
    (
        "playlist show",
        "playlist",
        "playlist_ref",
        None,
        None,
        None,
    ),
    (
        "playlist delete",
        "playlist",
        "playlist_ref",
        None,
        None,
        None,
    ),
    ("playlist add", "playlist", "playlist_ref", None, None, None),
    ("playlist add", "items", "spotify_ref", None, None, None),
    (
        "playlist remove",
        "playlist",
        "playlist_ref",
        None,
        None,
        None,
    ),
    ("playlist remove", "items", "spotify_ref", None, None, None),
    (
        "playlist play",
        "playlist",
        "playlist_ref",
        None,
        None,
        None,
    ),
    (
        "playlist rename",
        "playlist",
        "playlist_ref",
        None,
        None,
        None,
    ),
    ("playlist import", "from", "playlist_ref", None, None, None),
    ("playlist import", "to", "playlist_ref", None, None, None),
    (
        "playlist fork",
        "playlist",
        "playlist_ref",
        None,
        None,
        None,
    ),
    (
        "playlist fork",
        "name",
        "string",
        None,
        None,
        Some("the original's name"),
    ),
    (
        "playlist sync",
        "playlist",
        "playlist_ref",
        None,
        None,
        Some("every playlist with imports"),
    ),
    ("podcast search", "query", "search_query", None, None, None),
    (
        "podcast search",
        "limit",
        "integer",
        Some(1),
        Some(10),
        Some("10"),
    ),
    ("podcast play", "target", "spotify_ref", None, None, None),
    (
        "trigger add",
        "remaining",
        "time_or_percent",
        None,
        None,
        None,
    ),
    (
        "trigger add",
        "elapsed",
        "time_or_percent",
        None,
        None,
        None,
    ),
    ("trigger add", "at", "time_or_percent", None, None, None),
    ("trigger add", "track", "spotify_ref", None, None, None),
    (
        "trigger add",
        "times",
        "integer",
        Some(1),
        None,
        Some("until removed (current scope: once)"),
    ),
    ("trigger add", "note", "string", None, Some(1000), None),
    ("trigger add", "label", "string", None, Some(80), None),
    ("trigger show", "id", "trigger_id", None, None, None),
    ("trigger remove", "id", "trigger_id", None, None, None),
    (
        "trigger history",
        "id",
        "trigger_id",
        None,
        None,
        Some("every trigger"),
    ),
    (
        "trigger history",
        "limit",
        "integer",
        Some(1),
        Some(500),
        Some("20"),
    ),
    (
        "trigger test",
        "id",
        "trigger_id",
        None,
        None,
        Some("a generic test to you"),
    ),
    ("trigger wait", "id", "trigger_id", None, None, None),
    (
        "trigger wait",
        "timeout",
        "duration",
        None,
        None,
        Some("1h"),
    ),
    ("trigger retry", "firing", "firing_id", None, None, None),
    ("login", "slt", "slt", None, None, None),
    ("login", "token_file", "path_or_stdin", None, None, None),
    ("config set", "settings", "json_object", None, None, None),
    ("config get", "key", "config_key", None, None, None),
    (
        "testing use",
        "app_secret_file",
        "path_or_stdin",
        None,
        None,
        None,
    ),
    ("report", "message", "string", Some(10), Some(20_000), None),
    ("report", "pr", "url", None, None, None),
    ("report", "attach", "path", None, Some(5), None),
    (
        "docs",
        "topic",
        "enum",
        None,
        None,
        Some("the list of topics"),
    ),
    ("docs", "section", "string", None, None, None),
    ("docs", "search", "string", None, None, None),
    ("how", "question", "string", None, None, None),
    ("how", "limit", "integer", Some(1), Some(10), Some("3")),
    ("daemon logs", "lines", "integer", Some(0), None, None),
];

/// What `type`, `min` and `max` mean for the semantic types above.
pub const ARG_TYPES: &[(&str, &str)] = &[
    (
        "spotify_ref",
        "spotify:<kind>:<id>, an open.spotify.com link, or a bare 22-character id",
    ),
    (
        "playlist_ref",
        "a playlist id, spotify:playlist:<id> or its link",
    ),
    ("search_query", "words to search Spotify for"),
    (
        "position",
        "90, 90s, 1:30, 1m30s, 50% (absolute) or +15s, -10s, +10% (offset)",
    ),
    ("volume", "0-100, +N, -N, up, down"),
    ("time_or_percent", "30s, 1:30, 1m30s, 250ms or 25% (0-100)"),
    ("duration", "90s, 10m, 1h, 1:30"),
    ("queue_item", "a position (1 = next), an id (q_…) or a URI"),
    ("trigger_id", "trg_…"),
    ("firing_id", "fir_…"),
    ("slt", "an IAM short-lived token (oac_…)"),
    ("path_or_stdin", "a file path, or - for stdin"),
    (
        "json_object",
        "one JSON object, e.g. '{\"telemetry\": false}'",
    ),
    ("config_key", "a key from `spotify config keys`"),
    (
        "integer",
        "a whole number; min and max bound it where known (for strings: their length or count)",
    ),
];

fn arg_spec(path: &str, id: &str) -> Option<&'static ArgSpec> {
    ARG_SPECS.iter().find(|s| s.0 == path && s.1 == id)
}

// ---------------------------------------------------------------------------- examples

/// One example from a command's help.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Example {
    /// What to run.
    pub command: String,
    /// What it does, when the help says.
    pub description: Option<String>,
}

/// Where an example's comment starts: the first run of two or more spaces outside quotes.
fn comment_at(line: &str) -> Option<usize> {
    let mut quote = None;
    let bytes = line.as_bytes();
    for (at, c) in line.char_indices() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, ' ') if bytes.get(at + 1) == Some(&b' ') => return Some(at),
            _ => {}
        }
    }
    None
}

/// The `Examples:` blocks of a help text: indented lines after an `Examples:` line, up to a
/// blank line. A line ending in ` \` continues on the next one; two or more spaces start a comment.
#[must_use]
pub fn parse_examples(text: &str) -> Vec<Example> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut pending = String::new();
    for line in text.lines() {
        if line.trim_end() == "Examples:" {
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        if line.trim().is_empty() || !line.starts_with("  ") {
            in_block = false;
            pending.clear();
            continue;
        }
        let line = line.trim();
        if let Some(head) = line.strip_suffix('\\') {
            pending.push_str(head.trim_end());
            pending.push(' ');
            continue;
        }
        let full = format!("{pending}{line}");
        pending.clear();
        let (command, description) = match comment_at(&full) {
            Some(at) => (
                full[..at].trim().to_owned(),
                Some(full[at..].trim().to_owned()).filter(|d| !d.is_empty()),
            ),
            None => (full.trim().to_owned(), None),
        };
        out.push(Example {
            command,
            description,
        });
    }
    out
}

/// A command's examples: those of its long help, else its short help.
#[must_use]
pub fn examples(command: &clap::Command) -> Vec<Example> {
    let mut out = Vec::new();
    for text in [command.get_after_long_help(), command.get_after_help()]
        .into_iter()
        .flatten()
    {
        for example in parse_examples(&text.to_string()) {
            if !out.contains(&example) {
                out.push(example);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------- the tree

/// Every command of the tree with its path (`trigger add`; `` for the root), depth first in
/// grammar order, without clap's `help` subcommands.
#[must_use]
pub fn walk(root: &clap::Command) -> Vec<(String, &clap::Command)> {
    fn go<'a>(command: &'a clap::Command, path: &str, out: &mut Vec<(String, &'a clap::Command)>) {
        out.push((path.to_owned(), command));
        for sub in command.get_subcommands().filter(|s| s.get_name() != "help") {
            let child = if path.is_empty() {
                sub.get_name().to_owned()
            } else {
                format!("{path} {}", sub.get_name())
            };
            go(sub, &child, out);
        }
    }
    let mut out = Vec::new();
    go(root, "", &mut out);
    out
}

// ---------------------------------------------------------------------------- root help

/// Where summaries start in the root listing.
const LISTING_INDENT: usize = 15;
/// Help is wrapped at 100 columns.
const WIDTH: usize = 100;

/// `text` wrapped at [`WIDTH`] when its first line starts at column `first`; later lines are
/// indented by `indent`.
fn wrap(text: &str, first: usize, indent: usize) -> String {
    let mut out = String::new();
    let mut column = first;
    let mut line_empty = true;
    for word in text.split_whitespace() {
        let len = word.chars().count();
        if !line_empty && column + 1 + len > WIDTH {
            out.push('\n');
            out.push_str(&" ".repeat(indent));
            column = indent;
            line_empty = true;
        }
        if !line_empty {
            out.push(' ');
            column += 1;
        }
        out.push_str(word);
        column += len;
        line_empty = false;
    }
    out
}

/// The grouped command listing of the root help: per goal, each top-level command with its
/// summary and one example.
#[must_use]
pub fn root_listing(root: &clap::Command) -> String {
    let header = Style::new().bold().underline();
    let literal = Style::new().bold();
    let mut out = String::from(
        "Start here: spotify doctor · spotify status · spotify how \"<what you want to do>\"\n",
    );
    for goal in Goal::ALL {
        let _ = write!(out, "\n{header}{}:{header:#}\n", goal.title());
        for sub in root
            .get_subcommands()
            .filter(|s| s.get_name() != "help" && !s.is_hide_set())
            .filter(|s| entry(s.get_name()).and_then(|e| e.goal) == Some(goal))
        {
            let name = sub.get_name();
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            let pad = LISTING_INDENT.saturating_sub(2 + name.len());
            let _ = writeln!(
                out,
                "  {literal}{name}{literal:#}{}{}",
                " ".repeat(pad.max(1)),
                wrap(&about, 2 + name.len() + pad.max(1), LISTING_INDENT)
            );
            // The featured example (its lines), else the first one that fits on the line.
            let fits = |e: &str| {
                e.lines()
                    .all(|l| LISTING_INDENT + 2 + l.chars().count() <= WIDTH)
            };
            let example = entry(name)
                .and_then(|e| e.featured)
                .map(str::to_owned)
                .into_iter()
                .chain(examples(sub).into_iter().map(|e| e.command))
                .find(|e| fits(e));
            for line in example.iter().flat_map(|e| e.lines()) {
                let _ = writeln!(out, "{}  {line}", " ".repeat(LISTING_INDENT));
            }
        }
    }
    out.trim_end().to_owned()
}

/// Root help: the about, usage, the grouped listing, options, then the rest.
const ROOT_TEMPLATE: &str = "\
{about-with-newline}
{usage-heading} {usage}

{before-help}Global options (every command takes them):
{options}{after-help}";

/// The grammar with its generated help: the root help lists commands by goal.
#[must_use]
pub fn with_root_listing(root: clap::Command) -> clap::Command {
    let listing = root_listing(&root);
    root.before_help(listing).help_template(ROOT_TEMPLATE)
}

// ---------------------------------------------------------------------------- manifest

/// `invalid_input` and friends by code, with their exit code.
fn error_value(code: &str) -> Value {
    json!({"code": code, "exit": Error::new(code, "", "").exit_code()})
}

/// Every error code a command can return: its own, its requirements', and the usage errors
/// every command can have.
#[must_use]
pub fn error_codes(entry: &Entry) -> Vec<&'static str> {
    let mut codes: Vec<&'static str> = vec!["usage"];
    for code in entry
        .errors
        .iter()
        .chain(entry.needs.iter().flat_map(|n| n.errors()))
        .chain(["internal"].iter())
    {
        if !codes.contains(code) {
            codes.push(code);
        }
    }
    codes
}

fn requirements(entry: &Entry) -> Value {
    let mut map = serde_json::Map::new();
    let macos = entry
        .needs
        .iter()
        .any(|n| matches!(n, Need::Daemon | Need::SpotifyApp));
    map.insert("macos".into(), json!(macos));
    for (need, key) in Need::KEYS {
        map.insert(key.into(), json!(entry.needs.contains(&need)));
    }
    Value::Object(map)
}

/// Clap's own type of an argument.
fn clap_type(arg: &clap::Arg) -> &'static str {
    use std::any::TypeId;
    if !arg.get_action().takes_values() {
        return "boolean";
    }
    if !arg.get_possible_values().is_empty() {
        return "enum";
    }
    let id = arg.get_value_parser().type_id();
    if [
        TypeId::of::<i64>(),
        TypeId::of::<u32>(),
        TypeId::of::<u64>(),
        TypeId::of::<usize>(),
        TypeId::of::<u8>(),
        TypeId::of::<i32>(),
    ]
    .iter()
    .any(|t| id == *t)
    {
        "integer"
    } else if id == TypeId::of::<std::path::PathBuf>() {
        "path"
    } else {
        "string"
    }
}

fn argument(path: &str, command: &clap::Command, arg: &clap::Arg) -> Value {
    let id = arg.get_id().as_str();
    let spec = arg_spec(path, id);
    let takes_value = arg.get_action().takes_values();
    let mut allowed: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|_| takes_value)
        .map(|v| v.get_name().to_owned())
        .collect();
    if path == "docs" && id == "topic" {
        allowed = crate::docs::TOPICS
            .iter()
            .map(|(n, ..)| (*n).to_owned())
            .collect();
    }
    if path == "config get" && id == "key" {
        allowed = silicon_spotify_client::store::CONFIG_KEYS
            .iter()
            .map(|(k, ..)| (*k).to_owned())
            .collect();
    }
    let default = spec.and_then(|s| s.5).map(str::to_owned).or_else(|| {
        let defaults: Vec<String> = arg
            .get_default_values()
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect();
        (takes_value && !defaults.is_empty()).then(|| defaults.join(","))
    });
    let multiple = matches!(arg.get_action(), clap::ArgAction::Append)
        || arg.get_num_args().is_some_and(|n| n.max_values() > 1);
    let conflicts: Vec<String> = command
        .get_arg_conflicts_with(arg)
        .iter()
        .filter_map(|a| {
            a.get_long()
                .map(|l| format!("--{l}"))
                .or_else(|| Some(a.get_id().to_string()))
        })
        .collect();
    let requires: Vec<String> = arg
        .get_long()
        .and_then(|_| {
            // clap keeps `requires` private; the few this grammar uses are documented here.
            match (path, id) {
                ("play", "random" | "limit") => Some(vec!["--liked".to_owned()]),
                ("docs", "section") => Some(vec!["topic".to_owned()]),
                _ => None,
            }
        })
        .unwrap_or_default();
    // The first manifest read an unbuilt grammar, where an on/off flag lists `true` and `false`;
    // the built one lists nothing. Keep the old value (`allowed_values` is the accurate list).
    let possible: Vec<String> = if matches!(
        arg.get_action(),
        clap::ArgAction::SetTrue | clap::ArgAction::SetFalse
    ) {
        vec!["true".into(), "false".into()]
    } else {
        arg.get_possible_values()
            .iter()
            .map(|v| v.get_name().to_owned())
            .collect()
    };
    json!({
        // Fields of the first manifest version, unchanged:
        "name": id,
        "long": arg.get_long(),
        "short": arg.get_short().map(|c| c.to_string()),
        "positional": arg.is_positional(),
        "required": arg.is_required_set(),
        "global": arg.is_global_set(),
        "description": arg.get_help().map(ToString::to_string),
        "possible_values": possible,
        // Added:
        "flag": arg.get_long().map(|l| format!("--{l}")),
        "value_name": arg.get_value_names().and_then(|v| v.first()).map(ToString::to_string),
        "takes_value": takes_value,
        "multiple": multiple,
        "type": spec.map_or_else(|| clap_type(arg), |s| s.2),
        "allowed_values": allowed,
        "default": default,
        "min": spec.and_then(|s| s.3),
        "max": spec.and_then(|s| s.4),
        "conflicts_with": conflicts,
        "requires": requires,
    })
}

/// One command's manifest object.
fn manifest_command(path: &str, command: &clap::Command) -> Value {
    let entry = entry(path).copied().unwrap_or(Entry::EMPTY);
    let full = if path.is_empty() {
        "spotify".to_owned()
    } else {
        format!("spotify {path}")
    };
    let arguments: Vec<Value> = command
        .get_arguments()
        .filter(|a| !a.is_hide_set() && a.get_id() != "help" && a.get_id() != "version")
        // Globals are listed once, on the root.
        .filter(|a| path.is_empty() || !a.is_global_set())
        .map(|a| argument(path, command, a))
        .collect();
    let groups: Vec<Value> = command
        .get_groups()
        // clap names a group after each `Args` struct, holding all its arguments; only the
        // grammar's own groups (one of these arguments) mean something.
        .filter(|g| !(*g).clone().is_multiple())
        .map(|g| {
            json!({
                "id": g.get_id().as_str(),
                "required": g.is_required_set(),
                "args": g.get_args().map(|a| command.get_arguments().find(|x| x.get_id() == a).and_then(clap::Arg::get_long).map_or_else(|| a.to_string(), |l| format!("--{l}"))).collect::<Vec<_>>(),
                "rule": if g.is_required_set() { "exactly one" } else { "at most one" },
            })
        })
        .collect();
    let mut rendered = command.clone();
    let examples: Vec<Value> = examples(command)
        .into_iter()
        .map(|e| json!({"command": e.command, "description": e.description}))
        .collect();
    let goal = goal(path);
    let (changes, read_only_when) = effects(path);
    json!({
        // Fields of the first manifest version, unchanged:
        "command": full,
        "description": command.get_about().map(ToString::to_string),
        "usage": rendered.render_usage().to_string(),
        "aliases": command.get_visible_aliases().collect::<Vec<_>>(),
        "group": command.has_subcommands(),
        "arguments": arguments,
        // Added:
        "path": path,
        "summary": command.get_about().map(ToString::to_string),
        "details": command.get_long_about().map(ToString::to_string),
        "goal": goal.map(Goal::id),
        "goal_title": goal.map(Goal::title),
        "capabilities": entry.capabilities,
        "keywords": entry.keywords,
        "subcommands": command.get_subcommands().filter(|s| s.get_name() != "help").map(|s| format!("{full} {}", s.get_name())).collect::<Vec<_>>(),
        "arg_groups": groups,
        "examples": examples,
        "output": entry.output.iter().map(|(field, meaning)| json!({"field": field, "description": meaning})).collect::<Vec<_>>(),
        "errors": error_codes(&entry).into_iter().map(error_value).collect::<Vec<_>>(),
        "requirements": requirements(&entry),
        "requirement_notes": entry.notes,
        "help": format!("{full} --help"),
        "mutates": mutates(path),
        "changes": changes.iter().map(|c| c.id()).collect::<Vec<_>>(),
        "read_only_when": read_only_when,
        "has_read_only_form": has_read_only_form(path),
    })
}

/// A row of the bundled error guide's code tables.
#[derive(Clone, Debug)]
pub struct ErrorRow {
    /// Error codes the row covers.
    pub codes: Vec<String>,
    /// The exit code column (`—` for warnings).
    pub exit: String,
    /// What it means.
    pub meaning: String,
    /// How to fix it.
    pub fix: String,
    /// The guide section it is in.
    pub section: String,
}

/// The code tables of `spotify docs errors` (`| \`code\` | exit | meaning | fix |`).
#[must_use]
pub fn error_rows() -> Vec<ErrorRow> {
    let guide = crate::docs::TOPICS
        .iter()
        .find(|(name, ..)| *name == "errors")
        .map_or("", |(.., content)| *content);
    let mut section = String::new();
    let mut rows = Vec::new();
    for line in guide.lines() {
        if let Some(heading) = line
            .strip_prefix("### ")
            .or_else(|| line.strip_prefix("## "))
        {
            section = heading.trim().to_owned();
            continue;
        }
        if !line.starts_with("| `") {
            continue;
        }
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if cells.len() != 4 {
            continue;
        }
        let codes: Vec<String> = cells[0]
            .split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect();
        if codes.is_empty() {
            continue;
        }
        rows.push(ErrorRow {
            codes,
            exit: cells[1].to_owned(),
            meaning: cells[2].replace('`', ""),
            fix: cells[3].replace('`', ""),
            section: section.clone(),
        });
    }
    rows
}

/// `spotify commands --json`: every command with everything a Silicon needs to use it.
#[must_use]
pub fn manifest(root: &clap::Command) -> Value {
    let mut root = root.clone();
    root.build();
    let tree = walk(&root);
    let commands: Vec<Value> = tree
        .iter()
        .map(|(path, command)| manifest_command(path, command))
        .collect();
    let goals: Vec<Value> = Goal::ALL
        .iter()
        .map(|g| {
            json!({
                "id": g.id(),
                "title": g.title(),
                "commands": tree.iter().filter(|(p, _)| !p.is_empty() && goal(p) == Some(*g)).map(|(p, _)| format!("spotify {p}")).collect::<Vec<_>>(),
            })
        })
        .collect();
    let errors: Vec<Value> = error_rows()
        .into_iter()
        .flat_map(|row| {
            row.codes.clone().into_iter().map(move |code| {
                json!({"code": code, "exit": Error::new(&code, "", "").exit_code(), "meaning": row.meaning, "fix": row.fix, "section": row.section})
            })
        })
        .collect();
    json!({
        "version": silicon_spotify_client::VERSION,
        "commands": commands,
        "goals": goals,
        "errors": errors,
        "exit_codes": [
            {"exit": 0, "meaning": "ok"},
            {"exit": 1, "meaning": "the operation failed (see error.code)"},
            {"exit": 2, "meaning": "usage: bad arguments or input"},
            {"exit": 3, "meaning": "not signed in"},
            {"exit": 4, "meaning": "refused"},
            {"exit": 5, "meaning": "unavailable (daemon, network)"},
        ],
        "requirements": {
            "macos": "runs on macOS only (Spotify control)",
            "daemon": "talks to spotify-daemon, which the CLI starts on demand",
            "spotify_app": "the Spotify desktop app on this Mac, and permission to control it",
            "spotify_player_signed_in": "spotify_player installed and signed in to Spotify (spotify auth login)",
            "iam_login": "an IAM session in this home (spotify login)",
            "premium": "a Spotify Premium account",
            "controls_playback": "changes what plays on Spotify.app",
            "network": "needs the internet (Spotify's Web API or the spotify-cli backend)",
        },
        "argument_types": ARG_TYPES.iter().map(|(t, m)| json!({"type": t, "description": m})).collect::<Vec<_>>(),
        "changes": Change::ALL.iter().map(|(_, id, meaning)| json!({"change": id, "description": meaning})).collect::<Vec<_>>(),
        "mutates": "Per command: `mutates` is true when running it can change something (listed in `changes`) or send something out (report); false for commands that only read. `read_only_when` names the form of a changing command that only reads (volume without a level, search without --play, setup and update with --check), else null. `has_read_only_form` is true when either holds. Commands that talk to spotify-daemon start it when it is not running (SPOTIFY_DAEMON_AUTOSTART=0 prevents that); that alone is no change. Commands that only read: .commands[] | select(.mutates == false). Commands that can run without changing anything: .commands[] | select(.mutates == false or .read_only_when != null), the same as select(.has_read_only_form). What changes Liked Songs: .commands[] | select(.changes | index(\"library\"))",
        "output_contract": "--json: exactly one JSON value on stdout; errors are one {\"error\": {code, message, hint, retryable, details}} object on stderr with empty stdout.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> clap::Command {
        let mut root = crate::args::command();
        root.build();
        root
    }

    #[test]
    fn every_command_has_an_entry_and_every_entry_a_command() {
        let root = tree();
        let paths: Vec<String> = walk(&root).into_iter().map(|(p, _)| p).collect();
        for path in &paths {
            assert!(entry(path).is_some(), "no catalog entry for `{path}`");
            assert!(
                goal(path).is_some() || path.is_empty(),
                "`{path}` has no goal"
            );
        }
        for entry in ENTRIES {
            assert!(
                paths.iter().any(|p| p == entry.path),
                "catalog entry `{}` names no command",
                entry.path
            );
        }
        let mut seen = std::collections::HashSet::new();
        for entry in ENTRIES {
            assert!(seen.insert(entry.path), "duplicate entry `{}`", entry.path);
        }
    }

    #[test]
    fn every_command_says_what_it_changes() {
        let root = tree();
        let paths: Vec<String> = walk(&root).into_iter().map(|(p, _)| p).collect();
        for path in &paths {
            assert!(
                EFFECTS.iter().any(|(p, ..)| p == path),
                "`{path}` is not in EFFECTS: say what it changes"
            );
        }
        for (path, ..) in EFFECTS {
            assert!(
                paths.iter().any(|p| p == path),
                "EFFECTS names no command `{path}`"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for (path, ..) in EFFECTS {
            assert!(seen.insert(path), "duplicate EFFECTS row `{path}`");
        }
        // Those that control playback say so both ways.
        for entry in ENTRIES {
            if entry.needs.contains(&Need::ControlsPlayback) {
                assert!(
                    effects(entry.path).0.contains(&Change::Playback),
                    "`{}` controls playback",
                    entry.path
                );
            }
        }
        for read in [
            "status",
            "lyrics",
            "queue",
            "queue list",
            "search",
            "commands",
            "how",
        ] {
            assert!(
                !mutates(read) || effects(read).1.is_some(),
                "`{read}` reads (or has a form that only reads)"
            );
        }
        for write in [
            "play",
            "queue add",
            "playlist fork",
            "config set",
            "trigger add",
            "login",
            "report",
        ] {
            assert!(mutates(write), "`{write}` changes something");
        }
    }

    #[test]
    fn only_top_level_commands_set_a_goal() {
        for entry in ENTRIES {
            if entry.path.contains(' ') {
                assert!(entry.goal.is_none(), "`{}` inherits its goal", entry.path);
            }
        }
    }

    #[test]
    fn arg_specs_name_real_arguments() {
        let root = tree();
        let tree = walk(&root);
        for (path, id, kind, ..) in ARG_SPECS {
            let (_, command) = tree
                .iter()
                .find(|(p, _)| p == path)
                .unwrap_or_else(|| panic!("no command `{path}`"));
            assert!(
                command.get_arguments().any(|a| a.get_id() == *id),
                "`{path}` has no argument `{id}`"
            );
            assert!(
                ARG_TYPES.iter().any(|(t, _)| t == kind)
                    || ["string", "enum", "url", "path"].contains(kind),
                "type `{kind}` of {path} {id} is not described"
            );
        }
    }

    #[test]
    fn examples_are_parsed_with_their_comments() {
        let text = "Intro.\n\nExamples:\n  spotify play                                   Resume\n  spotify config set '{\"a\": null}'   null resets  it\n  iam -o json silicon-login --app-id spotify \\\n    | jq -r .slt | spotify login --token-file -\n  spotify now --json | jq .playback\n\nNext: spotify status\n  spotify not an example";
        let examples = parse_examples(text);
        assert_eq!(
            examples,
            vec![
                Example { command: "spotify play".into(), description: Some("Resume".into()) },
                Example {
                    command: "spotify config set '{\"a\": null}'".into(),
                    description: Some("null resets  it".into())
                },
                Example {
                    command: "iam -o json silicon-login --app-id spotify | jq -r .slt | spotify login --token-file -".into(),
                    description: None
                },
                Example { command: "spotify now --json | jq .playback".into(), description: None },
            ]
        );
    }

    /// A comment one space after its command would be read as part of the command (`iam
    /// silicon-login --sid si:<handle> Once per SILICON_HOME: …`), and `spotify how` and the
    /// manifest would hand that out as a line to run.
    #[test]
    fn every_example_comment_is_two_spaces_away() {
        let root = tree();
        for (path, command) in walk(&root) {
            for text in [command.get_after_long_help(), command.get_after_help()]
                .into_iter()
                .flatten()
                .map(ToString::to_string)
            {
                let mut block: Vec<&str> = Vec::new();
                let mut in_block = false;
                for line in text.lines().chain(std::iter::once("")) {
                    if line.trim_end() == "Examples:" {
                        in_block = true;
                        continue;
                    }
                    if in_block && line.starts_with("  ") && !line.trim().is_empty() {
                        block.push(line);
                        continue;
                    }
                    in_block = false;
                    // The columns comments start at in this block.
                    let columns: Vec<usize> = block
                        .iter()
                        .filter_map(|l| {
                            let at = comment_at(l.trim_start())? + (l.len() - l.trim_start().len());
                            Some(at + l[at..].len() - l[at..].trim_start().len())
                        })
                        .collect();
                    let Some(&first) = columns.iter().min() else {
                        block.clear();
                        continue;
                    };
                    for l in &block {
                        if comment_at(l.trim_start()).is_some() || l.trim_end().ends_with('\\') {
                            continue;
                        }
                        // An uppercase word one space after the command, about where the
                        // comments start, outside quotes.
                        let mut quote = None;
                        let mut previous = ' ';
                        for (at, c) in l.char_indices() {
                            match (quote, c) {
                                (None, '\'' | '"') => quote = Some(c),
                                (Some(q), c) if c == q => quote = None,
                                _ => {}
                            }
                            let glued = quote.is_none()
                                && at + 3 >= first
                                && previous == ' '
                                && c.is_uppercase()
                                && l[..at].trim_end().len() + 1 == at;
                            assert!(
                                !glued,
                                "`spotify {path} --help`: the comment of `{l}` needs two spaces before it"
                            );
                            previous = c;
                        }
                    }
                    block.clear();
                }
            }
        }
    }

    #[test]
    fn root_listing_groups_every_top_level_command_by_goal() {
        let root = tree();
        let listing = root_listing(&root);
        let plain = anstream_strip(&listing);
        let mut last = 0;
        for goal in Goal::ALL {
            let at = plain
                .find(&format!("\n{}:\n", goal.title()))
                .unwrap_or_else(|| panic!("no heading {}:\n{plain}", goal.title()));
            assert!(at > last, "goals in order");
            last = at;
        }
        for sub in root.get_subcommands().filter(|s| s.get_name() != "help") {
            assert!(
                plain.contains(&format!("\n  {} ", sub.get_name())),
                "`{}` missing from the root listing",
                sub.get_name()
            );
        }
        for line in plain.lines() {
            assert!(line.chars().count() <= WIDTH, "too wide: {line}");
        }
        assert!(
            plain.contains("  lyrics       Lyrics of any song"),
            "{plain}"
        );
    }

    /// The listing without ANSI styling.
    fn anstream_strip(text: &str) -> String {
        clap::builder::StyledStr::from(text.to_owned()).to_string()
    }

    #[test]
    fn wrapping_keeps_a_hanging_indent() {
        let text = wrap(&"word ".repeat(40), 15, 15);
        for (index, line) in text.lines().enumerate() {
            assert!(line.chars().count() + if index == 0 { 15 } else { 0 } <= WIDTH);
            if index > 0 {
                assert!(line.starts_with(&" ".repeat(15)) && !line.starts_with(&" ".repeat(16)));
            }
        }
    }

    #[test]
    fn manifest_describes_arguments_errors_and_requirements() {
        let manifest = manifest(&tree());
        let find = |path: &str| {
            manifest["commands"]
                .as_array()
                .and_then(|c| c.iter().find(|c| c["path"] == path))
                .cloned()
                .unwrap_or_else(|| panic!("no {path}"))
        };
        let search = find("search");
        assert_eq!(search["command"], "spotify search");
        assert_eq!(
            search["usage"],
            "Usage: spotify search [OPTIONS] <QUERY>..."
        );
        let limit = search["arguments"]
            .as_array()
            .and_then(|a| a.iter().find(|a| a["name"] == "limit"))
            .cloned()
            .expect("limit");
        assert_eq!(limit["type"], "integer");
        assert_eq!(
            (limit["min"].clone(), limit["max"].clone()),
            (json!(1), json!(10))
        );
        assert_eq!(search["requirements"]["spotify_player_signed_in"], true);
        assert_eq!(search["requirements"]["iam_login"], false);
        assert!(!search["examples"].as_array().expect("examples").is_empty());
        let add = find("trigger add");
        assert_eq!(add["goal"], "triggers");
        assert_eq!(add["arg_groups"][0]["rule"], "exactly one");
        assert!(
            add["errors"]
                .as_array()
                .expect("errors")
                .contains(&json!({"code": "threshold_passed", "exit": 2}))
        );
        let scope = add["arguments"]
            .as_array()
            .and_then(|a| a.iter().find(|a| a["name"] == "scope"))
            .cloned()
            .expect("scope");
        assert_eq!(scope["default"], "current");
        assert_eq!(
            scope["allowed_values"],
            json!(["current", "every", "track"])
        );
        // Globals only on the root.
        assert!(
            find("status")["arguments"]
                .as_array()
                .expect("args")
                .iter()
                .all(|a| a["global"] == false)
        );
        assert!(
            find("")["arguments"]
                .as_array()
                .expect("args")
                .iter()
                .any(|a| a["long"] == "json")
        );
        let lyrics = find("lyrics");
        assert_eq!(lyrics["goal_title"], "Lyrics & details");
        assert!(
            manifest["errors"]
                .as_array()
                .expect("errors")
                .iter()
                .any(|e| e["code"] == "automation_permission_denied" && e["exit"] == 4)
        );
        assert_eq!(find("docs")["arguments"][0]["allowed_values"][0], "usage");
        // What each command changes: read-only ones can be picked out.
        assert_eq!(find("status")["mutates"], false);
        assert_eq!(find("status")["changes"], json!([]));
        assert_eq!(find("play")["mutates"], true);
        assert_eq!(find("play")["changes"], json!(["playback"]));
        assert_eq!(find("playlist fork")["changes"], json!(["playlists"]));
        assert_eq!(find("login")["changes"], json!(["session"]));
        assert_eq!(
            find("volume")["read_only_when"],
            "without a level (it shows the volume)"
        );
        assert_eq!(find("report")["mutates"], true);
        // Commands that can run without changing anything: read-only ones and those with a
        // read-only form.
        for (path, can) in [
            ("status", true),
            ("volume", true),
            ("search", true),
            ("update", true),
            ("play", false),
            ("like", false),
            ("report", false),
        ] {
            assert_eq!(find(path)["has_read_only_form"], can, "{path}");
            assert_eq!(
                find(path)["mutates"] == false || !find(path)["read_only_when"].is_null(),
                can,
                "{path}: has_read_only_form is the documented filter"
            );
        }
        // `library` is Liked Songs, and says it is not playlists.
        let library = manifest["changes"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["change"] == "library"))
            .and_then(|c| c["description"].as_str())
            .unwrap_or("");
        assert!(
            library.contains("Liked Songs") && library.contains("Not playlists"),
            "{library}"
        );
        let changers: Vec<String> = manifest["commands"]
            .as_array()
            .expect("commands")
            .iter()
            .filter(|c| {
                c["changes"]
                    .as_array()
                    .is_some_and(|a| a.contains(&json!("library")))
            })
            .filter_map(|c| c["path"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(changers, ["like", "unlike"]);
        assert!(
            manifest["mutates"]
                .as_str()
                .is_some_and(|m| m.contains("select(.mutates == false or .read_only_when != null)")),
            "{}",
            manifest["mutates"]
        );
        let ids: Vec<&str> = manifest["changes"]
            .as_array()
            .expect("changes")
            .iter()
            .filter_map(|c| c["change"].as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "playback",
                "library",
                "playlists",
                "config",
                "triggers",
                "daemon",
                "session"
            ]
        );
    }

    /// The command path a parsed command line names (`trigger add`).
    fn parsed_path(matches: &clap::ArgMatches) -> String {
        let mut path = Vec::new();
        let mut current = matches;
        while let Some((name, sub)) = current.subcommand() {
            path.push(name.to_owned());
            current = sub;
        }
        path.join(" ")
    }

    /// Every example in every command's help parses with the real grammar, and every command
    /// has examples that run it (or, for a group, one of its subcommands). So help cannot show
    /// a command line that does not work.
    #[test]
    fn every_command_has_examples_that_parse() {
        let root = tree();
        let mut problems = Vec::new();
        for (path, command) in walk(&root) {
            if path.is_empty() {
                continue;
            }
            let examples = examples(command);
            if examples.is_empty() {
                problems.push(format!("`spotify {path}` has no Examples: in its help"));
            }
            let mut runs_itself = false;
            for example in &examples {
                for words in crate::shell::spotify_commands(&example.command) {
                    match crate::args::command().try_get_matches_from(&words) {
                        Ok(matches) => {
                            let named = parsed_path(&matches);
                            runs_itself |= named == path || named.starts_with(&format!("{path} "));
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                clap::error::ErrorKind::DisplayHelp
                                    | clap::error::ErrorKind::DisplayVersion
                            ) => {}
                        Err(error) => problems.push(format!(
                            "`spotify {path}` example `{}` does not parse: {}",
                            example.command,
                            error.render().to_string().lines().next().unwrap_or("")
                        )),
                    }
                }
            }
            if !examples.is_empty() && !runs_itself {
                problems.push(format!("no example of `spotify {path}` runs it"));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn documented_exit_codes_are_the_real_ones() {
        for row in error_rows() {
            let Ok(documented) = row.exit.parse::<i32>() else {
                assert_eq!(row.exit, "—", "{:?}", row.codes);
                continue;
            };
            for code in &row.codes {
                assert_eq!(
                    Error::new(code, "", "").exit_code(),
                    documented,
                    "docs/errors.md says `{code}` exits {documented}"
                );
            }
        }
    }

    #[test]
    fn error_rows_come_from_the_bundled_guide() {
        let rows = error_rows();
        let automation = rows
            .iter()
            .find(|r| r.codes.iter().any(|c| c == "automation_permission_denied"))
            .expect("row");
        assert_eq!(automation.exit, "4");
        assert!(automation.fix.contains("Automation"), "{}", automation.fix);
        assert!(rows.iter().any(|r| r.codes == ["invalid_input", "usage"]));
        // Every code a command can return is documented.
        for entry in ENTRIES {
            for code in error_codes(entry) {
                assert!(
                    rows.iter().any(|r| r.codes.iter().any(|c| c == code)),
                    "`{code}` ({}) is not in docs/errors.md",
                    entry.path
                );
            }
        }
    }
}
