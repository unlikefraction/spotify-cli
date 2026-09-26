//! The command grammar. Help text is the first layer of documentation: every command says what it
//! is for, how it is usually combined with others, and what to run next. Every command's help has
//! an `Examples:` block (a test checks it), which `spotify commands --json` and `spotify how`
//! read too. What a command needs and returns lives in `catalog.rs`.

use std::path::PathBuf;

use clap::{Args, CommandFactory as _, Parser, Subcommand, ValueEnum};

/// The grammar with its generated root help (commands grouped by goal). Use it instead of
/// `Cli::command()` wherever help can be shown.
#[must_use]
pub fn command() -> clap::Command {
    crate::catalog::with_root_listing(named_spotify(Cli::command()))
}

/// Every subcommand is shown as `spotify`, so `spotify queue -V` prints `spotify 0.1.6` (clap's
/// own would be `spotify-queue 0.1.6`). The name is used for the version line only.
fn named_spotify(command: clap::Command) -> clap::Command {
    command.mut_subcommands(|sub| named_spotify(sub.display_name("spotify")))
}

const ROOT_ABOUT: &str = "Control Spotify on this Mac from the terminal: play anything, read lyrics, manage the queue and playlists, and get Ting notifications at playback checkpoints.";

const ROOT_LONG: &str = "\
spotify-cli turns the Spotify desktop app into a command-line app for Carbons and Silicons.
Playback commands go through the Spotify Web API first (directly, or through spotify_player), are
verified against Spotify.app, and fall back to AppleScript when they fail or have no effect. That
includes every start: songs, episodes, albums, playlists, artists, shows, Liked Songs and radios.
Only exact seeks go to AppleScript first. Every result says which path worked.

Triggers watch the playing track and notify the Silicon that set them through Ting (\"30 s left\",
\"25% left\", \"50% passed\", \"song over\").

An always-on daemon (spotify-daemon) watches Spotify, fires triggers and keeps spotify_player warm.
The CLI starts it on demand; `spotify daemon install` runs it at login.";

/// What `like` and `unlike` say after their examples.
macro_rules! like_note {
    () => {
        "spotify_player can like or unlike only the song it believes is playing, so the command waits (up to verify_timeout_ms + ~1 s) until that is the song Spotify.app plays. If it does not catch up, nothing changes and the error is track_mismatch (retryable). A relinked song (Spotify.app and the Web API name different ids for it) is liked only when the ids prove it is the same song; otherwise nothing changes and track_mismatch is not retryable (details.relinked). Podcast episodes, ads and local files are unsupported.\n\nNext: spotify library liked"
    };
}

const ROOT_AFTER: &str = "\
Not sure which command? Ask in plain words (offline):
  spotify how \"lyrics of a song that isn't playing\"
  spotify how \"notify me 30 seconds before the song ends\"

Explore:
  spotify                              What is set up, what is missing, what to try
  spotify <command> --help             Every command documents itself, with examples
  spotify commands --json              Every command: arguments, examples, output, errors, needs
  spotify docs                         Offline guides (usage, triggers, playback, errors, …)
  spotify completions --help           Tab completion for zsh, bash, fish and PowerShell

Authentication (Silicons; needed for triggers):
  spotify iam --json                   Discover the IAM app id (spotify) before minting an SLT
  cargo install silicon-iam-cli        Get `iam`: Silicon IAM's own CLI, separate from spotify-cli
  iam silicon-login --sid si:<handle>  Once per SILICON_HOME: the Silicon's own IAM sign-in
  iam silicon-login --app-id spotify --grant-org \"$SILICON_ORG\" --approve-scopes
  spotify login '<SLT>'                Exchange the short-lived token
  spotify login status --json          Verify the saved identity (live)
  spotify auth login                   Separately: sign spotify_player in to Spotify (browser, once)

Output: human text by default, with `Next:` suggestions on stderr when stdout is a terminal
(SPOTIFY_HINTS=0 turns them off); --json prints one JSON value on stdout (errors: one JSON object
on stderr, empty stdout).
Exit codes: 0 ok, 1 failed, 2 usage, 3 not signed in, 4 refused, 5 unavailable (daemon, network).

State: $SILICON_HOME/.spotify (else ~/.spotify) per Silicon; the daemon lives in ~/.silicon-spotify.
Docs: https://spotify.unlikefraction.com/docs · Bugs: spotify report --help
Source: https://github.com/unlikefraction/spotify-cli (Rust library: crates/client)";

/// spotify-cli.
#[derive(Debug, Parser)]
#[command(
    name = "spotify",
    version,
    about = ROOT_ABOUT,
    long_about = ROOT_LONG,
    after_help = ROOT_AFTER,
    after_long_help = ROOT_AFTER,
    propagate_version = true,
    // Help and version are global flags, listed with the others after a command's own.
    disable_help_flag = true,
    disable_version_flag = true,
    max_term_width = 100
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    // Without a command, `spotify` prints what is set up and what to try (orient.rs).
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The help heading of the flags every command accepts: listed after a command's own flags.
pub const GLOBAL: &str = "Global options";

/// Flags every command accepts.
#[derive(Debug, Args)]
pub struct Global {
    /// Print one JSON value on stdout (errors as one JSON object on stderr).
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub json: bool,
    /// Organization for org-scoped calls (default: SILICON_ORG, then config `org`, then the session's).
    #[arg(long, global = true, value_name = "ORG", help_heading = GLOBAL)]
    pub org: Option<String>,
    /// Backend origin (default: SPOTIFY_API_URL, then config `api_url`, then production).
    #[arg(
        long,
        global = true,
        value_name = "URL",
        hide_short_help = true,
        help_heading = GLOBAL
    )]
    pub api_url: Option<String>,
    /// Print help (-h: a summary; --help: everything).
    #[arg(short, long, action = clap::ArgAction::Help, global = true, help_heading = GLOBAL)]
    pub help: Option<bool>,
    /// Print the version.
    #[arg(short = 'V', long, action = clap::ArgAction::Version, global = true, help_heading = GLOBAL)]
    pub version: Option<bool>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    // ---------------------------------------------------------------- playback
    /// What is playing: song, artist, album, position, time left, volume, shuffle, repeat.
    #[command(
        visible_alias = "now",
        long_about = "What is playing now: song, artist, album, position, time left, volume, shuffle and repeat. Reads Spotify.app directly (AppleScript, ~50 ms). --full adds what only the Web API knows, under `web`: the playing context (playlist/album), the device and the exact repeat mode, plus item_uri, is_playing, source, stale and relinked (true when Spotify serves the same song under another id; that is not stale).\n\nThose facts come from the daemon's spotify_player when its view matches Spotify.app (`source: spotify_player`), otherwise from a fresh Web API read (`source: web_api`, 1-4 s). When even that is for another item, `web.stale` is true, repeat and shuffle that contradict Spotify.app are left out, and the warning `web_state_stale` says so. Other warnings: no_active_device (the Web API sees no playback), timeout, rate_limited. Warnings never fail the command.",
        after_help = "Examples:\n  spotify status\n  spotify now --json | jq .playback.remaining_ms\n  spotify status --full                     Also the playlist/album, device, repeat mode\n\nNext: spotify track (song details) · spotify lyrics · spotify trigger add --remaining 30s"
    )]
    Status {
        /// Include context, device and repeat mode from the Web API (via spotify_player).
        #[arg(long)]
        full: bool,
    },
    /// Play any song, album, playlist, artist, podcast, Liked Songs or radio; or resume.
    #[command(
        long_about = "Play anything, or resume. Without arguments: resume. With a target: start it. Targets are spotify:<kind>:<id> URIs, open.spotify.com links, or bare ids with --type; --search plays the first match, --liked the Liked Songs list, --radio a radio seeded from a track, album, artist or playlist.\n\nHow it plays: every start goes through the Spotify Web API first and is verified against Spotify.app: songs, episodes, shows and Liked Songs directly, albums, playlists, artists and radios through spotify_player. A song or episode is started inside a list, so Spotify.app has it loaded and next/previous keep going: --context when given, else the song's album or the episode's show, found automatically. If the Web API errors, is refused, or Spotify.app does not change in time (verify_timeout_ms, default 2.5 s; at most 1.5 s for resume), AppleScript plays it instead (a radio has no AppleScript way: it only goes through the Web API), and when that brings Spotify.app to the front the focus goes back to the app you were in (config keep_spotify_in_background, default true); `fallback.reason` says why. When a failed start leaves Spotify.app with nothing loaded, what was playing is put back (error details.restored). Every result says which path worked (`via`). Settings: spotify config keys (strategy, verify_timeout_ms, keep_spotify_in_background); details: spotify docs playback.",
        after_help = "Examples:\n  spotify play --search 'arctic monkeys 505'      Search and play the first song\n  spotify play spotify:track:0BxE4FqsDD1Ot4YuBXwAPp\n  spotify play https://open.spotify.com/album/78bpIziExqiI9qztvNFlQu --shuffle\n  spotify play --search 'lofi beats' --type playlist\n  spotify play spotify:track:<id> --context spotify:playlist:<id>   Track, then the playlist\n  spotify play --liked --random                   Liked Songs, shuffled, from a random song\n  spotify play --liked --shuffle                  The same (--shuffle is --random for Liked Songs)\n  spotify play --liked                            Liked Songs in list order\n  spotify play --radio spotify:artist:<id>\n  spotify play                                    Resume\n\nNext: spotify status · spotify queue add <uri> · spotify trigger add --end"
    )]
    Play(PlayArgs),
    /// Resume playback where it paused (same as `spotify play` with no target).
    #[command(
        after_help = "Examples:\n  spotify resume\n  spotify play                The same\n\nNext: spotify status"
    )]
    Resume,
    /// Pause playback (checked in Spotify.app).
    #[command(
        after_help = "Examples:\n  spotify pause\n  spotify toggle              Pause or resume, whichever applies\n\nNext: spotify resume"
    )]
    Pause,
    /// Pause if playing, play if paused (decided from Spotify.app's own state).
    #[command(
        after_help = "Examples:\n  spotify toggle\n  spotify toggle --json | jq -r .playback.state\n\nNext: spotify status"
    )]
    Toggle,
    /// Skip to the next track: the managed queue's next item first, else Spotify's own.
    #[command(
        after_help = "Examples:\n  spotify next\n  spotify next --json | jq -r .playback.track.name\n\nIf `spotify queue` has managed items, `next` plays the first of them; otherwise Spotify's own next track.\n\nNext: spotify status"
    )]
    Next,
    /// Go back: to the previous item, or to the start of this one (from 3 s in).
    #[command(
        visible_alias = "prev",
        after_help = "Examples:\n  spotify previous\n  spotify prev --json | jq -r .result         restarted or previous_item\n\nFrom 3 s into the item, or when there is no item before it, `previous` restarts the current one (seek to 0:00); otherwise it goes to the previous item. The result says which: `result` is `restarted` or `previous_item` in --json.\n\nNext: spotify status"
    )]
    Previous,
    /// Jump to a position (1:30, 50%) or by an offset (+30s, -10s) in the current item.
    #[command(
        after_help = "Positions: 90, 90s, 1:30, 1m30s, 1:02:03, 50%. Offsets: +15s, -10s, +10%.\n\nExamples:\n  spotify seek 1:30\n  spotify seek 50%\n  spotify seek +30s\n  spotify seek -10\n\nUnder strategy auto (the default) seeks are exact: AppleScript sets the position. Under strategy spotify_player a seek is a relative, approximate one, refused with state_mismatch when spotify_player is on another item."
    )]
    Seek {
        /// Position or offset.
        #[arg(allow_hyphen_values = true)]
        position: String,
    },
    /// Show or set Spotify.app's volume: 0-100, +N, -N, up, down.
    #[command(
        after_help = "Examples:\n  spotify volume          Show\n  spotify volume 40       Set to 40%\n  spotify volume +10      Louder by 10 points\n  spotify volume -- -10   Quieter (or: spotify volume down)"
    )]
    Volume {
        /// 0-100, +N, -N, up (+10) or down (-10). Omit to show.
        #[arg(allow_hyphen_values = true)]
        level: Option<String>,
    },
    /// Turn shuffle on or off (toggle when omitted).
    #[command(
        after_help = "Examples:\n  spotify shuffle on\n  spotify shuffle off\n  spotify shuffle             Toggle\n\nSome contexts (a single song, some radios) do not allow shuffle: not_allowed_in_context. To start Liked Songs shuffled, use spotify play --liked --random.\n\nNext: spotify status"
    )]
    Shuffle {
        /// on, off or toggle.
        #[arg(value_enum)]
        mode: Option<Switch>,
    },
    /// Repeat off, the playlist/album (context) or this song (track).
    #[command(
        after_help = "Examples:\n  spotify repeat context      The playlist or album\n  spotify repeat track        This song (needs spotify_player)\n  spotify repeat off\n\n`track` (repeat-one) needs spotify_player; AppleScript can only switch context repeat on and off (it cannot clear a repeat-one set through the Web API). Every change is checked in Spotify.app. When spotify_player refuses for now, `repeat track` returns its error (e.g. rate_limited, retryable)."
    )]
    Repeat {
        /// off, context or track.
        #[arg(value_enum)]
        mode: RepeatArg,
    },
    /// Save the song playing now to Liked Songs.
    #[command(after_help = concat!(
        "Examples:\n  spotify like\n  spotify track               Shows liked: yes|no\n\n",
        like_note!()
    ))]
    Like,
    /// Remove the song playing now from Liked Songs.
    #[command(after_help = concat!(
        "Examples:\n  spotify unlike\n  spotify track               Shows liked: yes|no\n\n",
        like_note!()
    ))]
    Unlike,
    /// Start Spotify.app in the background (hidden, no focus steal) unless it is running.
    #[command(
        after_help = "Examples:\n  spotify launch\n\nPlayback commands start Spotify.app by themselves when it is closed (config launch_spotify, default true).\n\nNext: spotify play --search 'song or artist'"
    )]
    Launch,

    // ---------------------------------------------------------------- information
    /// Details of the current song or any track, album, artist or playlist (URI, link, id).
    #[command(
        visible_alias = "song",
        after_help = "Examples:\n  spotify track                               The current song (+ artists, album, release date)\n  spotify track spotify:track:0BxE4FqsDD1Ot4YuBXwAPp  A song and its album's URI; nothing plays\n  spotify track spotify:album:78bpIziExqiI9qztvNFlQu\n  spotify track https://open.spotify.com/artist/7Ln80lUS6He07XvHI8qqHH\n  spotify track 37i9dQZF1DXcBWIGoYBM5M --type playlist\n  spotify track spotify:track:0BxE4FqsDD1Ot4YuBXwAPp --json | jq -r .item.album_uri\n\nA song shows its album with the album's URI (album: <name> · spotify:album:<id>; `item.album_uri` in --json), which `spotify track` takes too. Albums show their release date and track list; artists their top tracks, albums and related artists; playlists their tracks. Songs show `liked: yes|no` when the daemon knows whether they are in Liked Songs. Podcast shows and episodes cannot be looked up by id (use `spotify podcast search`; `spotify track` with no target describes the episode playing now).\n\nNext: spotify lyrics · spotify playlist add <playlist> <uri>"
    )]
    Track {
        /// URI, link or id (default: the current song).
        target: Option<String>,
        /// Kind for a bare id (default: track). Shows and episodes cannot be looked up by id.
        #[arg(long = "type", value_enum)]
        kind: Option<TrackKindArg>,
    },
    /// Lyrics of any song — the current one, or any track by URI, link or id; nothing has to play.
    #[command(
        after_help = "Examples:\n  spotify lyrics                                      The song playing now\n  spotify lyrics spotify:track:0BxE4FqsDD1Ot4YuBXwAPp  Any track; nothing has to play\n  spotify search 'bohemian rhapsody' --type track     Only its name? Find the track's URI\n  spotify lyrics --json | jq -r '.lines[]'\n\n`lyrics` takes a track, not search words: find the song with `spotify search '<words>' --type track`, then run `spotify lyrics <uri>` with its URI. Lyrics come from Spotify's provider; many tracks have none (error `not_found`).\n\nNext: spotify track · spotify play <uri>"
    )]
    Lyrics {
        /// Track URI (spotify:track:<id>), open.spotify.com link or id (default: the current song).
        target: Option<String>,
    },
    /// Find songs, albums, artists, playlists, shows and episodes; each hit has a URI to use.
    #[command(
        after_help = "Examples:\n  spotify search 'arctic monkeys 505'\n  spotify search 'daily news' --type show,episode\n  spotify search 'focus' --type playlist --limit 5 --json\n  spotify search 'bohemian rhapsody' --type track --play\n\nEvery hit has a `uri` you can pass to play, queue add, playlist add, track or lyrics."
    )]
    Search {
        /// What to look for.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Kinds, comma-separated: track, album, artist, playlist, show, episode (default: all).
        #[arg(long = "type", value_delimiter = ',', value_enum)]
        kinds: Vec<KindArg>,
        /// Results per kind, 1-10 (spotify_player returns at most 10; default: config search_limit, 10).
        #[arg(long, allow_negative_numbers = true)]
        limit: Option<i64>,
        /// Play the first result.
        #[arg(long)]
        play: bool,
    },
    /// Your library: Liked Songs, saved albums, followed artists, top tracks, playlists.
    #[command(
        after_help = "Examples:\n  spotify library liked --limit 20\n  spotify library top\n  spotify library albums\n  spotify library playlists --json | jq -r '.items[].uri'\n\nNext: spotify play --liked --random · spotify play <uri>"
    )]
    Library {
        /// Section.
        #[arg(value_enum)]
        section: LibrarySection,
        /// Show at most this many (1 or more; default: all).
        #[arg(long, allow_negative_numbers = true)]
        limit: Option<i64>,
    },

    // ---------------------------------------------------------------- collections
    /// The play queue: see what is next; add, remove, reorder or clear songs and episodes.
    #[command(
        long_about = "The play queue: see what is next (`spotify queue`, `queue list` or `queue show`), add songs or episodes, remove, reorder or clear them. To skip to the next track, run `spotify next`. Spotify offers no API to remove or reorder its own queue, so spotify-cli keeps a managed queue in the daemon. Managed items play next, in order: the daemon starts each one as the previous track ends (or on `spotify next`), then returns to what was playing before (the interrupted playlist/album) when the queue drains. `spotify queue` also shows Spotify's own upcoming items, read-only.",
        after_help = "Examples:\n  spotify queue                                   List (managed + Spotify's upcoming)\n  spotify queue show                              The same (also: queue list)\n  spotify queue add spotify:track:<id> spotify:track:<id>\n  spotify queue add --search 'song name' --next   Put the first hit at the front\n  spotify queue move 3 1\n  spotify queue remove 2\n  spotify queue clear"
    )]
    Queue {
        #[command(subcommand)]
        action: Option<QueueCommand>,
    },
    /// Playlists: list, show, play, create, delete, add/remove tracks, fork, import, sync.
    #[command(
        visible_alias = "playlists",
        after_help = "Examples:\n  spotify playlist list\n  spotify playlist show 37i9dQZF1DXcBWIGoYBM5M\n  spotify playlist create 'Deep focus' --description 'no vocals'\n  spotify playlist add <playlist-id> spotify:track:<id> spotify:album:<id>\n  spotify playlist remove <playlist-id> spotify:track:<id>\n  spotify playlist play <playlist-id> --shuffle\n  spotify playlist delete <playlist-id>\n\nRenaming is not possible through spotify_player or AppleScript (`spotify playlist rename` explains)."
    )]
    Playlist {
        #[command(subcommand)]
        action: PlaylistCommand,
    },
    /// Podcasts: search shows and episodes, play one, list saved shows, see what plays.
    #[command(
        visible_alias = "podcasts",
        after_help = "Examples:\n  spotify podcast search 'tim ferriss'\n  spotify podcast search 'ai news' --episodes\n  spotify podcast play spotify:episode:<id>\n  spotify podcast play spotify:show:<id>      Latest/next episode of the show\n  spotify podcast saved\n  spotify podcast now"
    )]
    Podcast {
        #[command(subcommand)]
        action: PodcastCommand,
    },
    /// Spotify Connect devices: list them, or move playback to a phone, speaker or computer.
    #[command(
        visible_alias = "device",
        after_help = "Examples:\n  spotify devices                             List\n  spotify devices connect --name 'Kitchen speaker'\n  spotify devices connect <device-id>"
    )]
    Devices {
        #[command(subcommand)]
        action: Option<DeviceCommand>,
    },

    // ---------------------------------------------------------------- triggers
    /// Get a Ting at playback checkpoints: time left, time played, song end or change.
    #[command(
        visible_alias = "triggers",
        long_about = "Get a Ting at playback checkpoints: time left, time played, song end or change. A trigger watches playback and, when its checkpoint is reached, the daemon sends a Ting (`spotify.trigger.fired`) to the Silicon that created it. The Ting carries the trigger (id, condition, note), the track and the playback position; its metadata names your ISI so your flow can route it.\n\nConditions: --remaining 30s | --remaining 25% | --elapsed 50% | --elapsed 1:30 | --at 2:00 | --end (song finished) | --change (song changed for any reason).\nScopes: current (default: only the song playing now, fires once) | every (every song) | track (every play of --track <uri>).\n\nA current-scope trigger whose song is skipped or stopped first sends `spotify.trigger.expired` instead (disable with --no-expiry-notice). Requires `spotify login` (the Ting goes to your identity) unless --local.",
        after_help = "Examples:\n  spotify trigger add --remaining 30s --note 'start wrapping up'\n  spotify trigger add --remaining 25%\n  spotify trigger add --elapsed 50% --label halfway\n  spotify trigger add --end --note 'song over: check the build'\n  spotify trigger add --end --scope every            Every song end, until removed\n  spotify trigger add --remaining 10s --scope track --track spotify:track:<id> --times 3\n  spotify trigger list\n  spotify trigger wait <id> --timeout 10m            Block until it fires (no Ting needed)\n  spotify trigger test                                Send a test Ting now\n  spotify trigger history\n\nDocs: spotify docs triggers"
    )]
    Trigger {
        #[command(subcommand)]
        action: TriggerCommand,
    },

    // ---------------------------------------------------------------- accounts
    /// IAM discovery: app id, owning org, scopes, URLs (works offline, before login).
    #[command(
        after_help = "Examples:\n  spotify iam\n  spotify iam --json | jq -r .app_id\n  cargo install silicon-iam-cli       The iam CLI itself (Silicon IAM's, separate)\n\nStemcell and agents run `spotify iam --json` to learn the app id before minting an SLT with `iam`, Silicon IAM's own CLI (separate from spotify-cli; get it with `cargo install silicon-iam-cli`):\n  iam silicon-login --app-id spotify --grant-org \"$SILICON_ORG\" --approve-scopes\n\n`iam` mints for the Silicon whose SILICON_HOME it runs with (its session: $SILICON_HOME/.silicon-iam). In a fresh SILICON_HOME it has no session and refuses to mint: sign that Silicon in to IAM once with its own credential first, `iam silicon-login --sid si:<handle>` (it asks for the Silicon's STK), then mint as above and run `spotify login '<SLT>'`. Details: spotify docs auth."
    )]
    Iam,
    /// Log in with an IAM short-lived token (SLT) so triggers can Ting you.
    #[command(
        args_conflicts_with_subcommands = true,
        subcommand_precedence_over_arg = true,
        long_about = "Exchanges an IAM short-lived token (SLT, ~2 minutes, single use) through the spotify-cli backend for an app session, saved in $SILICON_HOME/.spotify/session.json (0600), and registers you as a Ting recipient so triggers can notify you. The CLI never asks for passwords, OTPs or SID/STK: only the SLT.\n\nMint the SLT with `iam`, Silicon IAM's own CLI (a separate program, not part of spotify-cli; get it with `cargo install silicon-iam-cli`), or with the web consent screen:\n  Silicon: iam silicon-login --app-id spotify --grant-org \"$SILICON_ORG\" --approve-scopes\n  Carbon:  iam login --app-id spotify --grant-org <org>\n\n`iam` keeps its own session in $SILICON_HOME/.silicon-iam and mints for the Silicon whose SILICON_HOME it runs with. In a fresh SILICON_HOME it has no session and refuses to mint: first sign that Silicon in to IAM once with its own credential, `iam silicon-login --sid si:<handle>` (it asks for the Silicon's STK), then mint as above. Details: spotify docs auth.",
        after_help = "Examples:\n  cargo install silicon-iam-cli        Once: the iam CLI (Silicon IAM's, separate)\n  iam silicon-login --sid si:<handle>  Once per SILICON_HOME: the Silicon's own IAM sign-in\n  spotify login 'oac_…'\n  iam -o json silicon-login --app-id spotify --grant-org \"$SILICON_ORG\" --approve-scopes \\\n    | jq -r .slt | spotify login --token-file -\n  spotify login status --json\n\nThe exchange uses an idempotency key derived from the SLT, so retrying after a network error replays instead of burning the token.\n\nNext: spotify trigger test"
    )]
    Login(LoginArgs),
    /// Log out: revoke the session in IAM and delete it locally.
    #[command(
        after_help = "Examples:\n  spotify logout\n  spotify logout --json | jq .revoked\n\nLogging out when not logged in succeeds too."
    )]
    Logout,
    /// Ting recipient registration (triggers need it; login does it automatically).
    #[command(
        after_help = "Examples:\n  spotify ting status\n  spotify ting register       After login, when the registration failed"
    )]
    Ting {
        #[command(subcommand)]
        action: TingCommand,
    },
    /// Sign spotify_player in to Spotify (once, in a browser) for search, lyrics, playlists.
    #[command(
        long_about = "spotify_player needs its own Spotify sign-in (OAuth in the browser) for search, lyrics, playlists, library, devices and queue reads. `spotify auth login` starts it; a Carbon clicks Agree once on this Mac, then tokens are cached and refreshed by spotify_player. This is separate from `spotify login`, which is your IAM identity.",
        after_help = "Examples:\n  spotify auth status                   Is spotify_player signed in?\n  spotify auth status --json | jq .authenticated\n  spotify auth login                    Opens Spotify's consent page in a browser on this Mac\n\nA headless Silicon cannot click Agree: ask a Carbon at this Mac to run `spotify auth login` once (spotify auth login --help)."
    )]
    Auth {
        #[command(subcommand)]
        action: AuthCommand,
    },

    // ---------------------------------------------------------------- configuration & ops
    /// Settings: set with one JSON object, show, get one, list keys, reset.
    #[command(
        after_help = "Examples:\n  spotify config set '{\"telemetry\": false}'\n  spotify config set '{\"strategy\": \"applescript\", \"launch_spotify\": false}'\n  spotify config set '{\"notify_isi\": \"planner\"}'\n  spotify config set '{\"telemetry\": null}'          null resets a key\n  spotify config show\n  spotify config keys\n  SPOTIFY_HINTS=0 spotify status                    Turns off the `Next:` suggestions (environment)\n  SPOTIFY_HINTS=always spotify status | cat         `Next:` suggestions even when piped\n\nEnvironment variables (not settings) are listed in spotify docs config --section Environment: e.g. SPOTIFY_HINTS=0 turns off the `Next:` suggestions (printed on stderr only when stdout is a terminal; SPOTIFY_HINTS=always prints them when it is not), SPOTIFY_DAEMON_AUTOSTART=0 never starts the daemon, SPOTIFY_DEBUG=1 prints full error details."
    )]
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// Select or leave an IAM testing environment (isolated plane, same code paths).
    #[command(
        after_help = "Examples:\n  printf %s \"$TEST_APP_SECRET\" | spotify testing use --app-secret-file -\n  spotify login c:alice           In a testing plane an SLT may be a test public id\n  spotify testing status\n  spotify testing exit\n\nThe env var SPOTIFY_TEST_APP_SECRET selects a plane for one process."
    )]
    Testing {
        #[command(subcommand)]
        action: TestingCommand,
    },
    /// Check Spotify.app, spotify_player, permissions, daemon, login, and how to fix each.
    #[command(
        after_help = "Examples:\n  spotify doctor\n  spotify doctor --json | jq '.checks[] | select(.ok == false)'\n\nExits 1 (doctor_failed) while a required check fails; every failing check has a `fix`.\n\nNext: spotify setup"
    )]
    Doctor,
    /// Install what is missing, run the daemon at login, start the Spotify sign-in.
    #[command(
        after_help = "Examples:\n  spotify setup\n  spotify setup --check                 Only report\n  spotify setup --install-apps --no-auth\n\nWhat it does (idempotent):\n  1. Checks macOS, Spotify.app (installs with `brew install --cask spotify` when --install-apps)\n  2. Installs spotify_player with Homebrew when missing\n  3. Installs the launchd agent and starts spotify-daemon\n  4. If spotify_player is not signed in, starts `spotify auth login` (browser)\n\nIt never logs you in to IAM; run `spotify login '<SLT>'` for triggers."
    )]
    Setup {
        /// Only check; change nothing.
        #[arg(long)]
        check: bool,
        /// Also install Spotify.app with Homebrew when it is missing.
        #[arg(long)]
        install_apps: bool,
        /// Do not start the Spotify browser sign-in.
        #[arg(long)]
        no_auth: bool,
    },
    /// Run, install and inspect spotify-daemon, which watches Spotify and fires triggers.
    #[command(
        after_help = "Examples:\n  spotify daemon install     Start at login (launchd) and now\n  spotify daemon status      Version, uptime, Spotify, triggers, deliveries, spotify_player\n  spotify daemon logs\n  spotify daemon restart\n  spotify daemon uninstall\n\nThe CLI starts the daemon on demand, and restarts it when the CLI is newer."
    )]
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Report a bug to the maintainers (optionally with the pull request that fixes it).
    #[command(
        long_about = "spotify-cli is open source: reproduce, patch and open a pull request at https://github.com/unlikefraction/spotify-cli, then report it here with --pr. Reports go to the maintainers through the backend (your text, --pr, versions, OS, and --attach files you choose; no tokens or logs are collected automatically). If the backend is unreachable the report is saved locally with a `gh issue create` command to file it yourself.",
        after_help = "Examples:\n  spotify report 'queue add fails with 403 for episodes; repro: spotify queue add <episode-uri>'\n  spotify report 'seek drifts 2 s' --pr https://github.com/unlikefraction/spotify-cli/pull/42\n  spotify report 'daemon crash' --attach ~/.silicon-spotify/daemon.log"
    )]
    Report {
        /// What happened, what you expected, and how to reproduce it.
        message: String,
        /// Pull request that fixes it (github.com/unlikefraction/spotify-cli/pull/<n>).
        #[arg(long, value_name = "URL")]
        pr: Option<String>,
        /// Attach a text file (repeatable; at most 5, 64 KiB each).
        #[arg(long, value_name = "FILE")]
        attach: Vec<PathBuf>,
    },
    /// Offline guides bundled in the CLI: read a topic or one section, or search them all.
    #[command(
        after_help = "Examples:\n  spotify docs                  Topics\n  spotify docs triggers\n  spotify docs triggers --section 'Without Ting'\n  spotify docs --search 'expired'\n  spotify docs playback --search 'fallback'    Only in one guide\n  spotify docs --search --random                A flag's name works as the text\n  spotify docs --all --json\n\nNext: spotify how \"<what you want to do>\""
    )]
    Docs {
        /// Topic (see `spotify docs`).
        topic: Option<String>,
        /// Only this section of the topic (its heading, or part of it).
        #[arg(long, requires = "topic", value_name = "HEADING")]
        section: Option<String>,
        /// Search every guide for this text, or only the topic's guide when one is given.
        #[arg(
            long,
            value_name = "TEXT",
            allow_hyphen_values = true,
            conflicts_with = "section"
        )]
        search: Option<String>,
        /// Print every guide.
        #[arg(long)]
        all: bool,
    },
    /// Every command: arguments, examples, output fields, errors, requirements (--json).
    #[command(
        after_help = "Examples:\n  spotify commands                 By goal; ✎ marks the commands that change something\n  spotify commands --json          Every command, machine-readable (mutates, changes, needs, …)\n  spotify commands --json | jq '.commands[] | select(.path == \"lyrics\")'\n  spotify commands --json | jq -r '.commands[] | select(.mutates == false).command'  Read-only\n  spotify commands --json | jq -r '.commands[] | select(.has_read_only_form).command'\n  spotify commands --json | jq -r '.commands[] | select(.changes | index(\"library\")).command'\n  spotify commands --json | jq -r '.commands[].examples[].command'\n\n`mutates` is false for a command that only reads. For one that can change something, `read_only_when` names its form that only reads, if any (volume without a level, search without --play, setup and update with --check). `has_read_only_form` is true when either holds: select(.has_read_only_form) is the same as select(.mutates == false or .read_only_when != null). `changes` says what can change: `library` is your Liked Songs (like, unlike); playlists are `playlists`.\n\nNext: spotify how \"<what you want to do>\""
    )]
    Commands,
    /// Check for a new version and install it (updates are also automatic).
    #[command(
        after_help = "Examples:\n  spotify update --check      Only report\n  spotify update"
    )]
    Update {
        /// Only report; do not install.
        #[arg(long)]
        check: bool,
    },
    /// Ask in plain words which command does something; get ready-to-run examples.
    #[command(
        long_about = "Ask in plain words which command does something. `how` searches every command's help (summary, arguments, flags, allowed values, examples) and the bundled guides, offline, and prints the best matching commands with ready-to-run examples, the guide section to read and any error code that fits.",
        after_help = "Examples:\n  spotify how \"lyrics of a song that isn't playing\"\n  spotify how notify me 30 seconds before the song ends\n  spotify how 'play my liked songs shuffled'\n  spotify how 'what is an SLT'\n  spotify how 'why is automation denied' --json\n\nNext: spotify <command> --help · spotify docs <topic> --section '<heading>'"
    )]
    How {
        /// The question, in plain words (quotes optional).
        #[arg(required = true, num_args = 1..)]
        question: Vec<String>,
        /// How many commands to show, 1-10 (default 3).
        #[arg(long, allow_negative_numbers = true)]
        limit: Option<i64>,
    },
    /// Tab completion for zsh, bash, fish or PowerShell (install steps in --help).
    #[command(
        long_about = "Prints a tab-completion script for zsh, bash, fish or PowerShell on stdout. Save it where your shell loads completions (the lines below work as they are), then open a new shell. The script completes commands, subcommands, flags and their fixed values.",
        after_help = "Examples:\n  mkdir -p ~/.zfunc && spotify completions zsh > ~/.zfunc/_spotify\n  echo 'fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc\n  mkdir -p ~/.bash_completion.d && spotify completions bash > ~/.bash_completion.d/spotify\n  echo 'source ~/.bash_completion.d/spotify' >> ~/.bashrc\n  mkdir -p ~/.config/fish/completions\n  spotify completions fish > ~/.config/fish/completions/spotify.fish\n  spotify completions powershell >> $PROFILE\n\nInstall, then open a new shell:\n  zsh         mkdir -p ~/.zfunc && spotify completions zsh > ~/.zfunc/_spotify\n              echo 'fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc\n              (Oh My Zsh runs compinit itself: write the script to\n              ~/.oh-my-zsh/completions/_spotify instead of both lines)\n  bash        mkdir -p ~/.bash_completion.d\n              spotify completions bash > ~/.bash_completion.d/spotify\n              echo 'source ~/.bash_completion.d/spotify' >> ~/.bashrc\n              (macOS Terminal starts login shells, which read ~/.bash_profile: add the source\n              line there unless it sources ~/.bashrc; the stock bash 3.2 works too)\n  fish        mkdir -p ~/.config/fish/completions\n              spotify completions fish > ~/.config/fish/completions/spotify.fish\n              (fish loads it by itself; nothing else to add)\n  powershell  spotify completions powershell >> $PROFILE\n\nTry it in this shell only: source <(spotify completions zsh) in zsh (after compinit),\n. <(spotify completions bash) in bash 4+, spotify completions fish | source in fish.\n\nA new shell means a normal one (a new terminal tab, or exec zsh): `zsh -f` skips ~/.zshrc and\n`bash --norc` skips ~/.bashrc, so completion is not loaded there. To load it without a new\nshell: source ~/.zshrc (bash: source ~/.bashrc).\n\nRe-run the first line after `spotify update` so new commands complete too."
    )]
    Completions {
        /// The shell.
        #[arg(value_enum)]
        shell: ShellArg,
    },
}

#[derive(Debug, Args)]
pub struct PlayArgs {
    /// URI, open.spotify.com link, or bare id (with --type). Omit to resume.
    pub target: Option<String>,
    /// Play the first search hit instead.
    #[arg(long, conflicts_with = "target")]
    pub search: Option<String>,
    /// Kind of --search hit or bare id (default for search: track).
    #[arg(long = "type", value_enum)]
    pub kind: Option<KindArg>,
    /// For a track or episode: continue in this playlist/album afterwards.
    #[arg(long)]
    pub context: Option<String>,
    /// Start a playlist/album/artist shuffled. With --liked it is the same as --random.
    #[arg(long)]
    pub shuffle: bool,
    /// Play the Liked Songs list itself (next and previous stay in it).
    #[arg(long, conflicts_with_all = ["target", "search", "radio"])]
    pub liked: bool,
    /// With --liked: start at a random song with a newly drawn shuffle order (Liked Songs'
    /// shuffle stays on; --shuffle does the same). Without either, Liked Songs plays in list
    /// order and its remembered shuffle is turned off.
    #[arg(long, requires = "liked")]
    pub random: bool,
    /// With --liked: at most this many tracks. Applies only when spotify_player starts the list
    /// (strategy spotify_player, or its Spotify username is unknown).
    #[arg(
        long,
        requires = "liked",
        default_value_t = 200,
        allow_negative_numbers = true
    )]
    pub limit: u32,
    /// Start a radio seeded from this track/album/artist/playlist.
    #[arg(long, conflicts_with_all = ["target", "search"])]
    pub radio: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Switch {
    On,
    Off,
    Toggle,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RepeatArg {
    Off,
    Context,
    Track,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Track,
    Album,
    Artist,
    Playlist,
    Show,
    Episode,
}

/// What `spotify track --type` can look up by id: podcast shows and episodes cannot be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TrackKindArg {
    Track,
    Album,
    Artist,
    Playlist,
}

/// What `queue add --search` can queue: Spotify's queue holds single items only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum QueueKindArg {
    Track,
    Episode,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LibrarySection {
    Liked,
    Albums,
    Artists,
    Top,
    Playlists,
}

/// Shells `spotify completions` writes scripts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ShellArg {
    Zsh,
    Bash,
    Fish,
    Powershell,
}

#[derive(Debug, Subcommand)]
pub enum QueueCommand {
    /// List the managed queue (plays next, editable) and Spotify's own upcoming items (read-only).
    #[command(
        visible_aliases = ["show", "ls"],
        after_help = "Examples:\n  spotify queue list\n  spotify queue show              The same\n  spotify queue                   The same\n  spotify queue --json | jq -r '.managed[].uri'\n\nNext: spotify queue add --search 'song name'"
    )]
    List,
    /// Add tracks or episodes by URI, link or search; --next puts them first.
    #[command(
        after_help = "Examples:\n  spotify queue add spotify:track:0BxE4FqsDD1Ot4YuBXwAPp\n  spotify queue add --search 'bohemian rhapsody'\n  spotify queue add --search 'huberman sleep' --type episode\n  spotify queue add spotify:track:<id> --next      Right after the song playing now\n\nManaged items play in order as each song ends (or on `spotify next`); then what was playing continues.\n\nNext: spotify queue · spotify next"
    )]
    Add {
        /// URIs or links.
        items: Vec<String>,
        /// Add the first search hit instead.
        #[arg(long)]
        search: Option<String>,
        /// Kind for --search (default track).
        #[arg(long = "type", value_enum)]
        kind: Option<QueueKindArg>,
        /// Put at the front instead of the end.
        #[arg(long)]
        next: bool,
    },
    /// Remove a queued item by position (1 = next), id or URI.
    #[command(
        visible_alias = "rm",
        after_help = "Examples:\n  spotify queue remove 1                  The next item\n  spotify queue rm spotify:track:<id>\n  spotify queue remove q_<id>             By id (see `spotify queue`)"
    )]
    Remove {
        /// Position, id (q_…) or URI.
        item: String,
    },
    /// Move a queued item to a new position (1 = next).
    #[command(
        visible_alias = "mv",
        after_help = "Examples:\n  spotify queue move 3 1                  The third item plays next\n  spotify queue mv spotify:track:<id> 2"
    )]
    Move {
        /// Position, id or URI.
        item: String,
        /// New position (1 = next).
        to: usize,
    },
    /// Empty the managed queue (Spotify's own upcoming list stays; it cannot be edited).
    #[command(after_help = "Examples:\n  spotify queue clear")]
    Clear,
}

#[derive(Debug, Subcommand)]
pub enum PlaylistCommand {
    /// Your playlists, with the ids the other playlist commands take.
    #[command(
        visible_alias = "ls",
        after_help = "Examples:\n  spotify playlist list\n  spotify playlist ls --limit 10 --json | jq -r '.items[].uri'"
    )]
    List {
        /// Show at most this many (1 or more; default: all).
        #[arg(long, allow_negative_numbers = true)]
        limit: Option<i64>,
    },
    /// The tracks of any playlist (id, URI or link), with lengths and URIs.
    #[command(
        after_help = "Examples:\n  spotify playlist show 37i9dQZF1DXcBWIGoYBM5M\n  spotify playlist show https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M --json"
    )]
    Show {
        /// Playlist id, URI or link.
        playlist: String,
    },
    /// Create a playlist (private unless --public); prints its id.
    #[command(
        after_help = "Examples:\n  spotify playlist create 'Deep focus' --description 'no vocals'\n  spotify playlist create 'Road trip' --collab --json | jq -r .id"
    )]
    Create {
        /// Name.
        name: String,
        /// Description.
        #[arg(long)]
        description: Option<String>,
        /// Public (default private).
        #[arg(long)]
        public: bool,
        /// Collaborative.
        #[arg(long)]
        collab: bool,
    },
    /// Delete (unfollow) a playlist.
    #[command(after_help = "Examples:\n  spotify playlist delete <playlist-id>")]
    Delete {
        /// Playlist id, URI or link.
        playlist: String,
    },
    /// Add tracks or whole albums to a playlist you own or collaborate on.
    #[command(
        after_help = "Examples:\n  spotify playlist add <playlist-id> spotify:track:0BxE4FqsDD1Ot4YuBXwAPp\n  spotify playlist add <playlist-id> spotify:album:78bpIziExqiI9qztvNFlQu   Every track of it"
    )]
    Add {
        /// Playlist id, URI or link.
        playlist: String,
        /// spotify:track:… or spotify:album:… (links work too).
        #[arg(required = true)]
        items: Vec<String>,
    },
    /// Remove tracks or whole albums from a playlist.
    #[command(
        visible_alias = "rm",
        after_help = "Examples:\n  spotify playlist remove <playlist-id> spotify:track:0BxE4FqsDD1Ot4YuBXwAPp\n  spotify playlist rm <playlist-id> spotify:album:78bpIziExqiI9qztvNFlQu"
    )]
    Remove {
        /// Playlist id, URI or link.
        playlist: String,
        /// spotify:track:… or spotify:album:….
        #[arg(required = true)]
        items: Vec<String>,
    },
    /// Play a playlist, optionally shuffled.
    #[command(
        after_help = "Examples:\n  spotify playlist play 37i9dQZF1DXcBWIGoYBM5M --shuffle\n  spotify playlist play spotify:playlist:<id>"
    )]
    Play {
        /// Playlist id, URI or link.
        playlist: String,
        /// Shuffled.
        #[arg(long)]
        shuffle: bool,
    },
    /// Rename or re-describe a playlist: not possible with the underlying tools; explains the alternative.
    #[command(after_help = "Examples:\n  spotify playlist rename <playlist-id> 'New name'")]
    Rename {
        /// Playlist id.
        playlist: String,
        /// New name.
        name: Option<String>,
    },
    /// Copy every track of one playlist into another (spotify_player import).
    #[command(
        after_help = "Examples:\n  spotify playlist import <from-playlist-id> <to-playlist-id>\n  spotify playlist import <from-playlist-id> <to-playlist-id> --delete   Mirror removals too\n\nNext: spotify playlist sync"
    )]
    Import {
        /// Source playlist.
        from: String,
        /// Destination playlist.
        to: String,
        /// Also delete tracks removed from the source since the last import.
        #[arg(long)]
        delete: bool,
    },
    /// Copy any playlist into a new one you own; `playlist sync` keeps the copy caught up.
    #[command(
        after_help = "Examples:\n  spotify playlist fork 37i9dQZF1DXcBWIGoYBM5M\n  spotify playlist fork spotify:playlist:<id> --name 'Focus (mine)' --json | jq -r .uri\n  spotify playlist sync                        Later: catch up with the source\n\nVisibility: without --name, spotify_player makes the copy with the source's name, description, visibility and collaborative setting, so a fork of a public playlist (such as one of Spotify's) is public too. With --name the copy is private and not collaborative, with the source's description. Change it in the Spotify app.\n\nEither way the copy is an import of the source: `spotify playlist sync` adds the tracks the source gained since (with --delete it also removes those the source dropped).\n\nNext: spotify playlist sync · spotify playlist show <id>"
    )]
    Fork {
        /// Playlist id, URI or link.
        playlist: String,
        /// Name of the new playlist (default: the original's name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Re-run imports and forks so the copies catch up with their sources.
    #[command(
        after_help = "Examples:\n  spotify playlist sync\n  spotify playlist sync <playlist-id> --delete"
    )]
    Sync {
        /// Playlist (default: all with imports).
        playlist: Option<String>,
        /// Also delete tracks removed from sources.
        #[arg(long)]
        delete: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum PodcastCommand {
    /// Search podcast shows and episodes.
    #[command(
        after_help = "Examples:\n  spotify podcast search 'tim ferriss'\n  spotify podcast search 'ai news' --episodes --limit 5\n\nNext: spotify podcast play <uri>"
    )]
    Search {
        /// What to look for.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Only episodes.
        #[arg(long, conflicts_with = "shows")]
        episodes: bool,
        /// Only shows.
        #[arg(long)]
        shows: bool,
        /// Results per kind, 1-10 (spotify_player returns at most 10; default 10).
        #[arg(long, allow_negative_numbers = true)]
        limit: Option<i64>,
    },
    /// Play an episode, or a show's latest/next episode.
    #[command(
        after_help = "Examples:\n  spotify podcast play spotify:episode:4rOoJ6Egrf8K2IrywzwOMk\n  spotify podcast play spotify:show:<id>      The show's latest/next episode"
    )]
    Play {
        /// spotify:episode:…, spotify:show:… or a link.
        target: String,
    },
    /// Your saved shows (from spotify_player's library cache).
    #[command(after_help = "Examples:\n  spotify podcast saved\n  spotify podcast saved --json")]
    Saved,
    /// The podcast episode playing now, with its position and time left.
    #[command(
        after_help = "Examples:\n  spotify podcast now\n  spotify podcast now --json | jq .playback.remaining_ms"
    )]
    Now,
}

#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// List Spotify Connect devices (this Mac, phones, speakers) with their ids.
    #[command(
        after_help = "Examples:\n  spotify devices list\n  spotify devices --json | jq '.devices[] | {id, name}'"
    )]
    List,
    /// Move playback to a device, by id or --name.
    #[command(
        after_help = "Examples:\n  spotify devices connect --name 'Kitchen speaker'\n  spotify devices connect <device-id>"
    )]
    Connect {
        /// Device id (see `spotify devices`).
        id: Option<String>,
        /// Device name instead of id.
        #[arg(long, conflicts_with = "id")]
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum TriggerCommand {
    /// Set a checkpoint (--remaining 30s, --remaining 25%, --elapsed 50%, --at 1:30, --end, --change) that Tings you.
    #[command(
        after_help = "Exactly one condition: --remaining, --elapsed/--at, --end or --change.\nTimes: 30s, 1:30, 1m30s, 250ms; percentages: 25%.\n\nExamples:\n  spotify trigger add --remaining 30s --note 'wrap up the call'\n  spotify trigger add --remaining 25%\n  spotify trigger add --elapsed 50%\n  spotify trigger add --end --scope every --label song-over\n  spotify trigger add --change --once --scope every     Next track change, whatever the reason\n  spotify trigger add --remaining 5s --local            Local only: no Ting (see `trigger wait`)"
    )]
    Add(TriggerAdd),
    /// Active triggers (--all adds finished ones, --everyone every home on this Mac).
    #[command(
        visible_alias = "ls",
        after_help = "Examples:\n  spotify trigger list\n  spotify trigger ls --all\n  spotify trigger list --everyone --json"
    )]
    List {
        /// Include completed, expired and removed triggers.
        #[arg(long)]
        all: bool,
        /// Every home on this machine, not just this Silicon's.
        #[arg(long)]
        everyone: bool,
    },
    /// One trigger with its recent firings and their delivery.
    #[command(
        after_help = "Examples:\n  spotify trigger show <id>\n  spotify trigger show <id> --json"
    )]
    Show {
        /// Trigger id.
        id: String,
    },
    /// Remove a trigger.
    #[command(
        visible_alias = "rm",
        after_help = "Examples:\n  spotify trigger remove <id>\n  spotify trigger rm <id>"
    )]
    Remove {
        /// Trigger id.
        id: String,
    },
    /// Remove all of this Silicon's active triggers.
    #[command(after_help = "Examples:\n  spotify trigger clear")]
    Clear,
    /// Recent firings and whether each Ting was delivered.
    #[command(
        after_help = "Examples:\n  spotify trigger history\n  spotify trigger history <id> --limit 5\n  spotify trigger history --everyone --json"
    )]
    History {
        /// Only this trigger.
        id: Option<String>,
        /// How many, 1-500 (default 20).
        #[arg(long, allow_negative_numbers = true)]
        limit: Option<i64>,
        /// Every home on this machine.
        #[arg(long)]
        everyone: bool,
    },
    /// Send a test Ting now (to a trigger's recipient, or to you) to check delivery end to end.
    #[command(
        after_help = "Examples:\n  spotify trigger test                  A test Ting to you\n  spotify trigger test <id>             To that trigger's recipient"
    )]
    Test {
        /// Trigger id (default: a generic test to you).
        id: Option<String>,
    },
    /// Block until a trigger fires (or expires), then print the firing (no Ting needed).
    #[command(
        after_help = "Examples:\n  spotify trigger add --remaining 5s --local --json | jq -r .trigger.id\n  spotify trigger wait <id> --timeout 10m"
    )]
    Wait {
        /// Trigger id.
        id: String,
        /// Give up after this long (e.g. 90s, 10m; default 1h).
        #[arg(long)]
        timeout: Option<String>,
    },
    /// Retry a failed Ting delivery now.
    #[command(
        after_help = "Examples:\n  spotify trigger history              Find the failed firing (fir_…)\n  spotify trigger retry fir_<id>"
    )]
    Retry {
        /// Firing id (fir_…) from `trigger history`.
        firing: String,
    },
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("condition").required(true).args(["remaining", "elapsed", "at", "end", "change"])))]
pub struct TriggerAdd {
    /// Fire when this much is left (30s, 1:00, 25%).
    #[arg(long, value_name = "TIME|%")]
    pub remaining: Option<String>,
    /// Fire when this much has played (50%, 1:30).
    #[arg(long, value_name = "TIME|%")]
    pub elapsed: Option<String>,
    /// Same as --elapsed: fire at this position.
    #[arg(long, value_name = "TIME|%")]
    pub at: Option<String>,
    /// Fire when the song finishes playing to its end.
    #[arg(long)]
    pub end: bool,
    /// Fire when the song stops being current for any reason (completed, skipped, stopped).
    #[arg(long)]
    pub change: bool,
    /// current (only the song playing now), every (every song) or track (every play of --track).
    #[arg(long, value_enum, default_value = "current")]
    pub scope: ScopeArg,
    /// With --scope track: the track to watch (URI, link or id).
    #[arg(long)]
    pub track: Option<String>,
    /// Fire at most N times (current scope always fires once).
    #[arg(long, conflicts_with = "once", allow_negative_numbers = true)]
    pub times: Option<u32>,
    /// Fire once, then finish (same as --times 1).
    #[arg(long)]
    pub once: bool,
    /// Text echoed in the notification (what to remind you about). Up to 1000 characters.
    #[arg(long)]
    pub note: Option<String>,
    /// Short name shown in lists and notifications.
    #[arg(long)]
    pub label: Option<String>,
    /// Do not send `spotify.trigger.expired` when a current-scope trigger's song ends first.
    #[arg(long)]
    pub no_expiry_notice: bool,
    /// Record firings locally only (no Ting, no login needed).
    #[arg(long)]
    pub local: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ScopeArg {
    Current,
    Every,
    Track,
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// The IAM short-lived token.
    #[arg(value_name = "SLT")]
    pub slt: Option<String>,
    /// Read the SLT from a file, or `-` for stdin (keeps it out of shell history).
    #[arg(long, value_name = "PATH|-")]
    pub token_file: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<LoginCommand>,
}

#[derive(Debug, Subcommand)]
pub enum LoginCommand {
    /// Verify the saved session live and print the identity ({"authenticated": true|false, …}).
    #[command(
        after_help = "Examples:\n  spotify login status\n  spotify login status --json | jq .authenticated"
    )]
    Status,
}

#[derive(Debug, Subcommand)]
pub enum TingCommand {
    /// Register (or re-activate) this Silicon as a Ting recipient for spotify-cli.
    #[command(after_help = "Examples:\n  spotify ting register")]
    Register,
    /// Show the saved registration.
    #[command(after_help = "Examples:\n  spotify ting status\n  spotify ting status --json")]
    Status,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Is spotify_player installed and signed in to Spotify?
    #[command(
        after_help = "Examples:\n  spotify auth status                   spotify_player's version, or what is missing\n  spotify auth status --json | jq .authenticated\n  spotify auth status --json | jq .installed\n\nNot signed in: spotify auth login (a Carbon clicks Agree once, in a browser on this Mac).\n\nNext: spotify auth login"
    )]
    Status,
    /// Start spotify_player's browser sign-in (a Carbon clicks Agree once).
    #[command(
        long_about = "Signs spotify_player in to Spotify, once per macOS user. The daemon starts `spotify_player authenticate`, which opens Spotify's consent page in the default browser on this Mac. A Carbon clicks Agree once; the page then returns to a local address on this Mac (127.0.0.1:8989), and spotify_player caches the tokens (~/.cache/spotify-player) and refreshes them from then on. Every Silicon home of this macOS user shares that sign-in. It is separate from `spotify login` (your IAM identity).\n\nA headless Silicon cannot click Agree: ask a Carbon at this Mac to run `spotify auth login` (or run it yourself and ask them to click Agree in the tab it opens), once. The consent completes only in a browser on this Mac, not from another machine. Until then playback still works through AppleScript (Spotify.app comes forward for a moment at each start), and search, lyrics, playlists, library, devices and queue reads fail with spotify_auth_required.",
        after_help = "Examples:\n  spotify auth login                    Opens Spotify's consent page in a browser on this Mac\n  spotify auth status                   Then check that the sign-in took\n  spotify auth status --json | jq .authenticated\n\nspotify_player's output goes to ~/.silicon-spotify/spotify-auth.log.\n\nNext: spotify auth status"
    )]
    Login,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set keys from one JSON object; null resets a key. Unknown keys are rejected.
    #[command(
        after_help = "Examples:\n  spotify config set '{\"search_limit\": 5}'\n  spotify config set '{\"telemetry\": false, \"strategy\": \"applescript\"}'\n  spotify config set '{\"search_limit\": null}'     null resets a key\n\nKeys, types and defaults: spotify config keys"
    )]
    Set {
        /// JSON object, e.g. '{"telemetry": false}'.
        #[arg(value_name = "JSON")]
        settings: String,
    },
    /// Effective settings (defaults filled in) and where they come from.
    #[command(
        after_help = "Examples:\n  spotify config show\n  spotify config show --json | jq .strategy"
    )]
    Show,
    /// One key's effective value.
    #[command(
        after_help = "Examples:\n  spotify config get strategy\n  spotify config get search_limit --json"
    )]
    Get {
        /// Key name (see `spotify config keys`).
        key: String,
    },
    /// Every key with its type, default and meaning.
    #[command(after_help = "Examples:\n  spotify config keys\n  spotify config keys --json")]
    Keys,
    /// Delete all settings (back to defaults).
    #[command(after_help = "Examples:\n  spotify config reset")]
    Reset,
}

#[derive(Debug, Subcommand)]
pub enum TestingCommand {
    /// Select a testing plane by its spotify-cli test app secret.
    #[command(
        after_help = "Examples:\n  printf %s \"$TEST_APP_SECRET\" | spotify testing use --app-secret-file -\n  spotify testing use --app-secret-file ./test-app-secret.txt"
    )]
    Use {
        /// File with the `ask_…` test secret, or `-` for stdin.
        #[arg(long, value_name = "PATH|-")]
        app_secret_file: PathBuf,
    },
    /// Show the selected plane.
    #[command(after_help = "Examples:\n  spotify testing status\n  spotify testing status --json")]
    Status,
    /// Return to production.
    #[command(after_help = "Examples:\n  spotify testing exit")]
    Exit,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start in the background (launchd when installed).
    #[command(after_help = "Examples:\n  spotify daemon start")]
    Start,
    /// Stop it.
    #[command(after_help = "Examples:\n  spotify daemon stop")]
    Stop,
    /// Stop and start (picks up a new binary).
    #[command(after_help = "Examples:\n  spotify daemon restart")]
    Restart,
    /// Version, uptime, Spotify state, triggers, deliveries, warm spotify_player, updates.
    #[command(
        after_help = "Examples:\n  spotify daemon status\n  spotify daemon status --json | jq .automation"
    )]
    Status,
    /// Run in the foreground (for other supervisors).
    #[command(after_help = "Examples:\n  spotify daemon run")]
    Run,
    /// Start at login through a launchd agent (and now).
    #[command(after_help = "Examples:\n  spotify daemon install")]
    Install,
    /// Remove the launchd agent and stop the daemon.
    #[command(after_help = "Examples:\n  spotify daemon uninstall")]
    Uninstall,
    /// Print the end of the daemon log.
    #[command(after_help = "Examples:\n  spotify daemon logs\n  spotify daemon logs --lines 200")]
    Logs {
        /// Lines (default 60).
        #[arg(long, default_value_t = 60, allow_negative_numbers = true)]
        lines: usize,
    },
}
