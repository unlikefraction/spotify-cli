//! The command grammar. Help text is the first layer of documentation: every command says what it
//! is for, how it is usually combined with others, and what to run next.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

const ROOT_ABOUT: &str = "Control Spotify on this Mac from the terminal, and get Ting notifications at playback checkpoints.";

const ROOT_LONG: &str = "\
spotify-cli turns the Spotify desktop app into a command-line app for Carbons and Silicons.
Playback commands go through spotify_player (Spotify Web API) first, are verified against
Spotify.app, and fall back to AppleScript when they fail or have no effect; every result says which
path worked. Triggers watch the playing track and notify the Silicon that set them through Ting
(\"30 s left\", \"25% left\", \"50% passed\", \"song over\").

An always-on daemon (spotify-daemon) watches Spotify, fires triggers and keeps spotify_player warm.
The CLI starts it on demand; `spotify daemon install` runs it at login.";

const ROOT_AFTER: &str = "\
Start here:
  spotify doctor                      Check Spotify.app, spotify_player, permissions, daemon, login
  spotify setup                       Install and start everything that is missing (no login)
  spotify status                      What is playing now
  spotify play --search 'song name'   Search and play
  spotify trigger add --remaining 30s --note 'wrap up'   Ting me 30 s before this song ends

Authentication (Silicons; needed for triggers):
  spotify iam --json                  Discover the IAM app id (spotify) before minting an SLT
  iam silicon-login --app-id spotify --grant-org <org> --approve-scopes
  spotify login '<SLT>'               Exchange the short-lived token
  spotify login status --json         Verify the saved identity (live)
  spotify auth login                  Separately: sign spotify_player in to Spotify (browser, once)

Explore:
  spotify <command> --help            Every command documents itself, with examples
  spotify commands --json             The whole command tree for agents
  spotify docs                        Offline guides (usage, triggers, development, api, errors)

Output: human text by default; --json prints one JSON value on stdout (errors: one JSON object on
stderr, empty stdout). Exit codes: 0 ok, 1 failed, 2 usage, 3 not signed in, 4 refused,
5 unavailable (daemon, network).

State: $SILICON_HOME/.spotify (else ~/.spotify) per Silicon; the daemon lives in ~/.silicon-spotify.
Docs: https://spotify.unlikefraction.com/docs · Source: https://github.com/unlikefraction/spotify-cli
Rust: https://github.com/unlikefraction/spotify-cli/tree/main/crates/client · Bugs: spotify report --help";

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
    subcommand_required = true,
    arg_required_else_help = true,
    max_term_width = 100
)]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub command: Command,
}

/// Flags every command accepts.
#[derive(Debug, Args)]
pub struct Global {
    /// Print one JSON value on stdout (errors as one JSON object on stderr).
    #[arg(long, global = true)]
    pub json: bool,
    /// Organization for org-scoped calls (default: SILICON_ORG, then config `org`, then the session's).
    #[arg(long, global = true, value_name = "ORG")]
    pub org: Option<String>,
    /// Backend origin (default: SPOTIFY_API_URL, then config `api_url`, then production).
    #[arg(long, global = true, value_name = "URL", hide_short_help = true)]
    pub api_url: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    // ---------------------------------------------------------------- playback
    /// Show what Spotify is playing (track, position, remaining, volume, shuffle, repeat).
    #[command(
        visible_alias = "now",
        long_about = "Reads Spotify.app directly (AppleScript, ~50 ms). --full adds what only the Web API knows: the playing context (playlist/album), the device and the exact repeat mode.",
        after_help = "Examples:\n  spotify status\n  spotify now --json | jq .playback.remaining_ms\n  spotify status --full\n\nNext: spotify track (song details) · spotify lyrics · spotify trigger add --remaining 30s"
    )]
    Status {
        /// Include context, device and repeat mode from the Web API (via spotify_player).
        #[arg(long)]
        full: bool,
    },
    /// Play something (a URI, a link, a search, Liked Songs, a radio) or resume.
    #[command(
        long_about = "Without arguments: resume. With a target: start it. Targets are spotify:<kind>:<id> URIs, open.spotify.com links, or bare ids with --type. --search plays the first match.\n\nHow it plays: spotify_player (Web API) first; if it errors or Spotify.app does not change within verify_timeout_ms (default 2.5 s), AppleScript plays it. Starting a single track by id through spotify_player is known to fail on the desktop app, so tracks usually report `via: applescript` with the reason in `fallback`.",
        after_help = "Examples:\n  spotify play                                   Resume\n  spotify play spotify:track:0BxE4FqsDD1Ot4YuBXwAPp\n  spotify play https://open.spotify.com/album/78bpIziExqiI9qztvNFlQu --shuffle\n  spotify play --search 'arctic monkeys 505'\n  spotify play --search 'lofi beats' --type playlist\n  spotify play spotify:track:<id> --context spotify:playlist:<id>   Track, then the playlist\n  spotify play --liked --random\n  spotify play --radio spotify:artist:<id>\n\nNext: spotify status · spotify queue add <uri> · spotify trigger add --end"
    )]
    Play(PlayArgs),
    /// Resume playback (same as `spotify play` with no target).
    Resume,
    /// Pause playback.
    Pause,
    /// Toggle between play and pause.
    Toggle,
    /// Skip to the next track (plays the managed queue first when it has items).
    #[command(
        after_help = "If `spotify queue` has managed items, `next` plays the first of them; otherwise Spotify's own next track.\n\nNext: spotify status"
    )]
    Next,
    /// Go to the previous track (Spotify restarts the current one when past 3 s).
    #[command(visible_alias = "prev")]
    Previous,
    /// Jump within the current track.
    #[command(
        after_help = "Positions: 90, 90s, 1:30, 1m30s, 1:02:03, 50%. Offsets: +15s, -10s, +10%.\n\nExamples:\n  spotify seek 1:30\n  spotify seek 50%\n  spotify seek +30s\n  spotify seek -10"
    )]
    Seek {
        /// Position or offset.
        #[arg(allow_hyphen_values = true)]
        position: String,
    },
    /// Show or set the Spotify.app volume.
    #[command(
        after_help = "Examples:\n  spotify volume          Show\n  spotify volume 40       Set to 40%\n  spotify volume +10      Louder by 10 points\n  spotify volume -- -10   Quieter (or: spotify volume down)"
    )]
    Volume {
        /// 0-100, +N, -N, up (+10) or down (-10). Omit to show.
        #[arg(allow_hyphen_values = true)]
        level: Option<String>,
    },
    /// Turn shuffle on or off (toggle when omitted).
    Shuffle {
        /// on, off or toggle.
        #[arg(value_enum)]
        mode: Option<Switch>,
    },
    /// Set repeat: off, context (the playlist/album) or track (this song).
    #[command(
        after_help = "`track` (repeat-one) needs spotify_player; AppleScript can only switch context repeat on and off."
    )]
    Repeat {
        /// off, context or track.
        #[arg(value_enum)]
        mode: RepeatArg,
    },
    /// Save the current track to Liked Songs.
    Like,
    /// Remove the current track from Liked Songs.
    Unlike,
    /// Start Spotify.app in the background (hidden, no focus steal).
    Launch,

    // ---------------------------------------------------------------- information
    /// Details about the current song, or about any track, album, artist or playlist.
    #[command(
        visible_alias = "song",
        after_help = "Examples:\n  spotify track                               The current song (+ artists, album, release date)\n  spotify track spotify:album:78bpIziExqiI9qztvNFlQu\n  spotify track https://open.spotify.com/artist/7Ln80lUS6He07XvHI8qqHH\n  spotify track 37i9dQZF1DXcBWIGoYBM5M --type playlist\n\nAlbums show their release date and track list; artists their top tracks, albums and related artists; playlists their tracks. Podcast shows and episodes cannot be looked up by id (use `spotify podcast search`).\n\nNext: spotify lyrics · spotify playlist add <playlist> <uri>"
    )]
    Track {
        /// URI, link or id (default: the current song).
        target: Option<String>,
        /// Kind for a bare id.
        #[arg(long = "type", value_enum)]
        kind: Option<KindArg>,
    },
    /// Lyrics of the current song (or of a given track).
    #[command(
        after_help = "Examples:\n  spotify lyrics\n  spotify lyrics spotify:track:0BxE4FqsDD1Ot4YuBXwAPp\n  spotify lyrics --json | jq -r '.lines[]'\n\nLyrics come from Spotify's provider; many tracks have none (error `not_found`)."
    )]
    Lyrics {
        /// Track URI, link or id (default: the current song).
        target: Option<String>,
    },
    /// Search tracks, albums, artists, playlists, shows and episodes.
    #[command(
        after_help = "Examples:\n  spotify search 'arctic monkeys 505'\n  spotify search 'daily news' --type show,episode\n  spotify search 'focus' --type playlist --limit 5 --json\n  spotify search 'bohemian rhapsody' --type track --play\n\nEvery hit has a `uri` you can pass to play, queue add, playlist add or track."
    )]
    Search {
        /// What to look for.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Kinds, comma-separated: track, album, artist, playlist, show, episode (default: all).
        #[arg(long = "type", value_delimiter = ',', value_enum)]
        kinds: Vec<KindArg>,
        /// Results per kind, 1-10 (spotify_player returns at most 10; default: config search_limit, 10).
        #[arg(long)]
        limit: Option<usize>,
        /// Play the first result.
        #[arg(long)]
        play: bool,
    },
    /// Your library: liked songs, saved albums, followed artists, top tracks, playlists.
    #[command(
        after_help = "Examples:\n  spotify library liked --limit 20\n  spotify library top\n  spotify library albums --json"
    )]
    Library {
        /// Section.
        #[arg(value_enum)]
        section: LibrarySection,
        /// Show at most this many (1 or more; default: all).
        #[arg(long)]
        limit: Option<usize>,
    },

    // ---------------------------------------------------------------- collections
    /// The play queue: add, list, remove, reorder and clear upcoming tracks.
    #[command(
        long_about = "Spotify offers no API to remove or reorder its own queue, so spotify-cli keeps a managed queue in the daemon. Managed items play next, in order: the daemon starts each one as the previous track ends (or on `spotify next`), then returns to what was playing before (the interrupted playlist/album) when the queue drains. `spotify queue` also shows Spotify's own upcoming items, read-only.",
        after_help = "Examples:\n  spotify queue                                   List (managed + Spotify's upcoming)\n  spotify queue add spotify:track:<id> spotify:track:<id>\n  spotify queue add --search 'song name' --next   Put the first hit at the front\n  spotify queue move 3 1\n  spotify queue remove 2\n  spotify queue clear"
    )]
    Queue {
        #[command(subcommand)]
        action: Option<QueueCommand>,
    },
    /// Playlists: list, show, create, delete, add/remove tracks, play, import, fork, sync.
    #[command(
        visible_alias = "playlists",
        after_help = "Examples:\n  spotify playlist list\n  spotify playlist show 37i9dQZF1DXcBWIGoYBM5M\n  spotify playlist create 'Deep focus' --description 'no vocals'\n  spotify playlist add <playlist-id> spotify:track:<id> spotify:album:<id>\n  spotify playlist remove <playlist-id> spotify:track:<id>\n  spotify playlist play <playlist-id> --shuffle\n  spotify playlist delete <playlist-id>\n\nRenaming is not possible through spotify_player or AppleScript (`spotify playlist rename` explains)."
    )]
    Playlist {
        #[command(subcommand)]
        action: PlaylistCommand,
    },
    /// Podcasts: search shows and episodes, play, list saved shows.
    #[command(
        visible_alias = "podcasts",
        after_help = "Examples:\n  spotify podcast search 'tim ferriss'\n  spotify podcast search 'ai news' --episodes\n  spotify podcast play spotify:episode:<id>\n  spotify podcast play spotify:show:<id>      Latest/next episode of the show\n  spotify podcast saved\n  spotify podcast now"
    )]
    Podcast {
        #[command(subcommand)]
        action: PodcastCommand,
    },
    /// Spotify Connect devices: list them or move playback to one.
    #[command(visible_alias = "device")]
    Devices {
        #[command(subcommand)]
        action: Option<DeviceCommand>,
    },

    // ---------------------------------------------------------------- triggers
    /// Playback checkpoints that notify you through Ting (time left, time elapsed, song end).
    #[command(
        visible_alias = "triggers",
        long_about = "A trigger watches playback and, when its checkpoint is reached, the daemon sends a Ting (`spotify.trigger.fired`) to the Silicon that created it. The Ting carries the trigger (id, condition, note), the track and the playback position; its metadata names your ISI so your flow can route it.\n\nConditions: --remaining 30s | --remaining 25% | --elapsed 50% | --elapsed 1:30 | --at 2:00 | --end (song finished) | --change (song changed for any reason).\nScopes: current (default: only the song playing now, fires once) | every (every song) | track (every play of --track <uri>).\n\nA current-scope trigger whose song is skipped or stopped first sends `spotify.trigger.expired` instead (disable with --no-expiry-notice). Requires `spotify login` (the Ting goes to your identity) unless --local.",
        after_help = "Examples:\n  spotify trigger add --remaining 30s --note 'start wrapping up'\n  spotify trigger add --remaining 25%\n  spotify trigger add --elapsed 50% --label halfway\n  spotify trigger add --end --note 'song over: check the build'\n  spotify trigger add --end --scope every            Every song end, until removed\n  spotify trigger add --remaining 10s --scope track --track spotify:track:<id> --times 3\n  spotify trigger list\n  spotify trigger wait <id> --timeout 10m            Block until it fires (no Ting needed)\n  spotify trigger test                                Send a test Ting now\n  spotify trigger history\n\nDocs: spotify docs triggers"
    )]
    Trigger {
        #[command(subcommand)]
        action: TriggerCommand,
    },

    // ---------------------------------------------------------------- accounts
    /// Print IAM discovery: app id, owning org, URLs (works offline, before login).
    #[command(
        after_help = "Stemcell and agents run `spotify iam --json` to learn the app id before minting an SLT:\n  iam silicon-login --app-id spotify --grant-org <org> --approve-scopes"
    )]
    Iam,
    /// Log in with an IAM short-lived token (SLT), or check the login with `login status`.
    #[command(
        args_conflicts_with_subcommands = true,
        subcommand_precedence_over_arg = true,
        long_about = "Exchanges an IAM short-lived token (SLT, ~2 minutes, single use) through the spotify-cli backend for an app session, saved in $SILICON_HOME/.spotify/session.json (0600), and registers you as a Ting recipient so triggers can notify you. The CLI never asks for passwords, OTPs or SID/STK: only the SLT.\n\nMint the SLT with the official iam CLI (or the web consent screen):\n  Silicon: iam silicon-login --app-id spotify --grant-org <org> --approve-scopes\n  Carbon:  iam login --app-id spotify --grant-org <org>",
        after_help = "Examples:\n  spotify login 'oac_…'\n  iam -o json silicon-login --app-id spotify --grant-org <org> --approve-scopes \\\n    | jq -r .slt | spotify login --token-file -\n  spotify login status --json\n\nThe exchange uses an idempotency key derived from the SLT, so retrying after a network error replays instead of burning the token."
    )]
    Login(LoginArgs),
    /// Log out: revoke the session in IAM and delete it locally.
    Logout,
    /// Ting recipient registration (triggers need it; login does it automatically).
    Ting {
        #[command(subcommand)]
        action: TingCommand,
    },
    /// The Spotify account used by spotify_player (Web API features): status or browser login.
    #[command(
        long_about = "spotify_player needs its own Spotify sign-in (OAuth in the browser) for search, lyrics, playlists, library, devices and queue reads. `spotify auth login` starts it; a Carbon clicks Agree once on this Mac, then tokens are cached and refreshed by spotify_player. This is separate from `spotify login`, which is your IAM identity.",
        after_help = "Examples:\n  spotify auth status\n  spotify auth login"
    )]
    Auth {
        #[command(subcommand)]
        action: AuthCommand,
    },

    // ---------------------------------------------------------------- configuration & ops
    /// Settings: `config set '<json>'`, show, get, keys, reset.
    #[command(
        after_help = "Examples:\n  spotify config set '{\"telemetry\": false}'\n  spotify config set '{\"strategy\": \"applescript\", \"launch_spotify\": false}'\n  spotify config set '{\"notify_isi\": \"planner\"}'\n  spotify config set '{\"telemetry\": null}'          null resets a key\n  spotify config show\n  spotify config keys"
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
    /// Check every dependency and say exactly how to fix what is missing.
    Doctor,
    /// Install missing dependencies, register the daemon at login and start it (no IAM login).
    #[command(
        after_help = "What it does (idempotent):\n  1. Checks macOS, Spotify.app (installs with `brew install --cask spotify` when --install-apps)\n  2. Installs spotify_player with Homebrew when missing\n  3. Installs the launchd agent and starts spotify-daemon\n  4. If spotify_player is not signed in, starts `spotify auth login` (browser)\n\nIt never logs you in to IAM; run `spotify login '<SLT>'` for triggers."
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
    /// Run, install and inspect spotify-daemon.
    #[command(
        after_help = "Workflow:\n  spotify daemon install     Start at login (launchd) and now\n  spotify daemon status      Version, uptime, Spotify, triggers, deliveries, spotify_player\n  spotify daemon logs\n  spotify daemon restart\n  spotify daemon uninstall\n\nThe CLI starts the daemon on demand, and restarts it when the CLI is newer."
    )]
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Report a bug (optionally with the pull request that fixes it).
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
    /// Offline documentation: guides bundled in the CLI.
    #[command(
        after_help = "Examples:\n  spotify docs                  Topics\n  spotify docs triggers\n  spotify docs --search 'expired'\n  spotify docs --all --json"
    )]
    Docs {
        /// Topic (see `spotify docs`).
        topic: Option<String>,
        /// Search every guide.
        #[arg(long)]
        search: Option<String>,
        /// Print every guide.
        #[arg(long)]
        all: bool,
    },
    /// The full command tree with arguments (use --json for agents).
    Commands,
    /// Check for a new version (updates are automatic; see config auto_update).
    Update {
        /// Only report; do not install.
        #[arg(long)]
        check: bool,
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
    /// Start a playlist/album/artist shuffled.
    #[arg(long)]
    pub shuffle: bool,
    /// Play Liked Songs.
    #[arg(long, conflicts_with_all = ["target", "search", "radio"])]
    pub liked: bool,
    /// With --liked: random order.
    #[arg(long, requires = "liked")]
    pub random: bool,
    /// With --liked: at most this many tracks.
    #[arg(long, requires = "liked", default_value_t = 200)]
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

#[derive(Debug, Subcommand)]
pub enum QueueCommand {
    /// List managed items and Spotify's upcoming items.
    List,
    /// Add tracks or episodes (URIs, links, or --search).
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
    /// Remove a managed item by position (1 = next), id or URI.
    #[command(visible_alias = "rm")]
    Remove {
        /// Position, id (q_…) or URI.
        item: String,
    },
    /// Move a managed item to a new position.
    #[command(visible_alias = "mv")]
    Move {
        /// Position, id or URI.
        item: String,
        /// New position (1 = next).
        to: usize,
    },
    /// Empty the managed queue.
    Clear,
}

#[derive(Debug, Subcommand)]
pub enum PlaylistCommand {
    /// Your playlists (id and name).
    #[command(visible_alias = "ls")]
    List {
        /// Show at most this many (1 or more; default: all).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Tracks of a playlist.
    Show {
        /// Playlist id, URI or link.
        playlist: String,
    },
    /// Create a playlist.
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
    Delete {
        /// Playlist id, URI or link.
        playlist: String,
    },
    /// Add tracks or whole albums.
    Add {
        /// Playlist id, URI or link.
        playlist: String,
        /// spotify:track:… or spotify:album:… (links work too).
        #[arg(required = true)]
        items: Vec<String>,
    },
    /// Remove tracks or whole albums.
    #[command(visible_alias = "rm")]
    Remove {
        /// Playlist id, URI or link.
        playlist: String,
        /// spotify:track:… or spotify:album:….
        #[arg(required = true)]
        items: Vec<String>,
    },
    /// Play a playlist.
    Play {
        /// Playlist id, URI or link.
        playlist: String,
        /// Shuffled.
        #[arg(long)]
        shuffle: bool,
    },
    /// Rename or re-describe (not supported by the underlying tools; explains the alternative).
    Rename {
        /// Playlist id.
        playlist: String,
        /// New name.
        name: Option<String>,
    },
    /// Copy every track of one playlist into another (spotify_player import).
    Import {
        /// Source playlist.
        from: String,
        /// Destination playlist.
        to: String,
        /// Also delete tracks removed from the source since the last import.
        #[arg(long)]
        delete: bool,
    },
    /// Copy a playlist into a new one you own.
    #[command(
        after_help = "Examples:\n  spotify playlist fork 37i9dQZF1DXcBWIGoYBM5M\n  spotify playlist fork spotify:playlist:<id> --name 'Focus (mine)' --json | jq -r .uri"
    )]
    Fork {
        /// Playlist id, URI or link.
        playlist: String,
        /// Name of the new playlist (default: the original's name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Re-run imports for one playlist or all.
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
    /// Search shows and episodes.
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
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Play an episode or show.
    Play {
        /// spotify:episode:…, spotify:show:… or a link.
        target: String,
    },
    /// Your saved shows (from spotify_player's library cache).
    Saved,
    /// The episode playing now.
    Now,
}

#[derive(Debug, Subcommand)]
pub enum DeviceCommand {
    /// List Spotify Connect devices.
    List,
    /// Move playback to a device.
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
    /// Create a trigger.
    #[command(
        after_help = "Exactly one condition: --remaining, --elapsed/--at, --end or --change.\nTimes: 30s, 1:30, 1m30s, 250ms; percentages: 25%.\n\nExamples:\n  spotify trigger add --remaining 30s --note 'wrap up the call'\n  spotify trigger add --remaining 25%\n  spotify trigger add --elapsed 50%\n  spotify trigger add --end --scope every --label song-over\n  spotify trigger add --change --once --scope every     Next track change, whatever the reason\n  spotify trigger add --remaining 5s --local            Local only: no Ting (see `trigger wait`)"
    )]
    Add(TriggerAdd),
    /// List active triggers (--all includes finished ones).
    #[command(visible_alias = "ls")]
    List {
        /// Include completed, expired and removed triggers.
        #[arg(long)]
        all: bool,
        /// Every home on this machine, not just this Silicon's.
        #[arg(long)]
        everyone: bool,
    },
    /// One trigger with its recent firings.
    Show {
        /// Trigger id.
        id: String,
    },
    /// Remove a trigger.
    #[command(visible_alias = "rm")]
    Remove {
        /// Trigger id.
        id: String,
    },
    /// Remove all of this Silicon's active triggers.
    Clear,
    /// Recent firings and their Ting delivery state.
    History {
        /// Only this trigger.
        id: Option<String>,
        /// How many, 1-500 (default 20).
        #[arg(long)]
        limit: Option<usize>,
        /// Every home on this machine.
        #[arg(long)]
        everyone: bool,
    },
    /// Send a test Ting now (for a trigger's delivery target, or your own).
    Test {
        /// Trigger id (default: a generic test to you).
        id: Option<String>,
    },
    /// Block until a trigger fires (or expires), then print the firing.
    Wait {
        /// Trigger id.
        id: String,
        /// Give up after this long (e.g. 90s, 10m; default 1h).
        #[arg(long)]
        timeout: Option<String>,
    },
    /// Retry a failed delivery now.
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
    #[arg(long, conflicts_with = "once")]
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
    Status,
}

#[derive(Debug, Subcommand)]
pub enum TingCommand {
    /// Register (or re-activate) this Silicon as a Ting recipient for spotify-cli.
    Register,
    /// Show the saved registration.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Is spotify_player installed and signed in to Spotify?
    Status,
    /// Start spotify_player's browser sign-in (a Carbon clicks Agree once).
    Login,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set keys from one JSON object; null resets a key. Unknown keys are rejected.
    Set {
        /// JSON object, e.g. '{"telemetry": false}'.
        #[arg(value_name = "JSON")]
        settings: String,
    },
    /// Effective settings (defaults filled in) and where they come from.
    Show,
    /// One key's effective value.
    Get {
        /// Key name (see `spotify config keys`).
        key: String,
    },
    /// Every key with type, default and meaning.
    Keys,
    /// Delete all settings (back to defaults).
    Reset,
}

#[derive(Debug, Subcommand)]
pub enum TestingCommand {
    /// Select a testing plane by its spotify-cli test app secret.
    Use {
        /// File with the `ask_…` test secret, or `-` for stdin.
        #[arg(long, value_name = "PATH|-")]
        app_secret_file: PathBuf,
    },
    /// Show the selected plane.
    Status,
    /// Return to production.
    Exit,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start in the background (launchd when installed).
    Start,
    /// Stop it.
    Stop,
    /// Stop and start (picks up a new binary).
    Restart,
    /// Version, uptime, Spotify state, triggers, deliveries, warm spotify_player, updates.
    Status,
    /// Run in the foreground (for other supervisors).
    Run,
    /// Start at login through a launchd agent (and now).
    Install,
    /// Remove the launchd agent and stop the daemon.
    Uninstall,
    /// Print the end of the daemon log.
    Logs {
        /// Lines (default 60).
        #[arg(long, default_value_t = 60)]
        lines: usize,
    },
}
