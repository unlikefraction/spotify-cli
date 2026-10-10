//! Playback control with verification and fallback.
//!
//! The rule (from the product spec): try `spotify_player` first; if it fails, use AppleScript.
//! "Fails" includes "exited 0 but nothing happened": `spotify_player` hands commands to its
//! running instance asynchronously, and some commands (starting a single track by id on the
//! desktop app) report success while leaving Spotify.app with no track. So every action is
//! verified against Spotify.app's own state before it counts as done, and every result says
//! which path worked (`via`) and, when it fell back, why.
//!
//! The running instance also decides *what* to send from its own memory of the player: whether
//! it is playing, the repeat mode, shuffle, the current track. That memory refreshes only after
//! commands the instance ran itself, so changes made in Spotify.app, through AppleScript or by a
//! track ending leave it behind. Before a command whose effect depends on it (play, pause,
//! toggle, shuffle, repeat, relative seek, like), the controller reads that memory
//! (`get key playback`, answered in milliseconds) and compares it with Spotify.app. When they
//! disagree, AppleScript acts directly and `fallback` says `state_mismatch`; `like` and `unlike`
//! refuse with `track_mismatch` instead of guessing which song they would change.
//!
//! Spotify sometimes plays a song under another id than the one Spotify.app shows (track
//! relinking: the same recording from another release). The Web API, and so spotify_player,
//! then reports the substitute's id. These checks and `status --full` count it as the same item
//! when the Web API's `linked_from` names Spotify.app's id, or when the title, a length within a
//! second and the album name agree (`web.relinked`). `like` and `unlike` still refuse it,
//! without retrying: spotify_player would save or remove the substitute's id, not the one
//! Spotify.app shows.
//!
//! Seeking goes to AppleScript first, because spotify_player's way of doing it is worse on the
//! desktop app: spotify_player can only seek by an offset, which it adds to a position it fetches
//! from the Web API when the command runs, seconds later when the Web API is rate limiting.
//!
//! A track or episode, Liked Songs and a show start through the Spotify Web API directly
//! ([`crate::webapi`]): a track or episode in the caller's context, else in its album or show;
//! Liked Songs as its own list (`spotify:user:<id>:collection`, the id spotify_player is signed
//! in as), a show as its own. The start goes to the device Spotify.app on this Mac controls: a
//! speaker or phone it is the remote for, else Spotify.app itself by device id. That keeps
//! Spotify.app in the background, where AppleScript's `play track` brings it to the front. It
//! counts once Spotify.app plays it (for a track or episode: the item asked for, or the same song
//! relinked), read through AppleScript, which does not bring it forward. A start the Web API
//! answered with a 5xx, or never answered, is looked for in Spotify.app too: when it went
//! through, it counts (with a `note`) and is not started again. Otherwise (a rate limit or the
//! pause after one, no Premium, no device to start on, no effect in time) AppleScript starts it
//! and `fallback` says why (`from: web_api`); a track then plays in the album the Web API's
//! lookup named (or the caller's context), so the album goes on after it. Whenever an
//! AppleScript start brings Spotify.app to the front, the focus goes back to the app that had it
//! ([`crate::focus`]; `refocused`). When a start still leaves Spotify.app empty, what was loaded
//! before is put back.
//!
//! spotify_player never starts Liked Songs or a single track as the fallback of a start
//! AppleScript can make: it starts them as a list of track ids, which leaves Spotify.app stopped
//! with nothing loaded. It does so only when AppleScript is not allowed (strategy
//! `spotify_player`) or when Liked Songs' uri is unknown.

use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::applescript::{self, Runner};
use crate::focus::{self, Focus, Refocus};
use crate::model::{
    Playback, PlayerState, SameSong, Track, WebItem, WebPlayback, playing_item_uri,
};
use crate::player::SpotifyPlayer;
use crate::timing::SeekTarget;
use crate::uri::{Kind, SpotifyUri};
use crate::webapi::{self, WebApi};
use crate::{Error, Result};

/// Which tool performs playback commands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// spotify_player (or, to start a track or episode, the Web API directly) first, verified;
    /// AppleScript when it fails or has no effect (default).
    #[default]
    Auto,
    /// Only spotify_player and the Web API. No AppleScript fallback.
    SpotifyPlayer,
    /// Only AppleScript (Spotify.app on this Mac). Works without spotify_player or Premium.
    Applescript,
}

impl std::str::FromStr for Strategy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "spotify_player" => Ok(Self::SpotifyPlayer),
            "applescript" => Ok(Self::Applescript),
            other => Err(Error::invalid(
                format!("`{other}` is not a strategy."),
                "Use auto, spotify_player or applescript.",
            )),
        }
    }
}

/// Which tool actually made the change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// The spotify_player CLI (Spotify Web API).
    SpotifyPlayer,
    /// AppleScript against Spotify.app.
    Applescript,
    /// The Spotify Web API directly, with spotify_player's token (starts of tracks, episodes,
    /// Liked Songs and shows).
    WebApi,
}

/// Why the primary path was abandoned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fallback {
    /// The path that was tried first.
    pub from: Via,
    /// What went wrong there.
    pub reason: Error,
}

/// The result of a verified playback action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// What was done, e.g. `pause`, `play`, `seek`.
    pub action: String,
    /// Which tool made the change.
    pub via: Via,
    /// Present when the first path failed and the other one succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Fallback>,
    /// What the action did when it can do more than one thing: `previous` reports `restarted`
    /// (back to the start of the current item) or `previous_item`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Spotify.app right after the change.
    pub playback: Playback,
    /// Spotify.app came to the front when AppleScript started the item, and the focus went back
    /// to the app that had it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refocused: Option<Refocus>,
    /// Something worth knowing about how it went: the start went to the speaker or phone
    /// Spotify.app controls, or it counted although the Web API's answer never came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Outcome {
    fn unchanged(action: &str, playback: Playback) -> Self {
        Self::new(action, Via::Applescript, playback)
    }

    /// An outcome without a fallback, result or refocus.
    #[must_use]
    pub fn new(action: &str, via: Via, playback: Playback) -> Self {
        Self {
            action: action.to_owned(),
            via,
            fallback: None,
            result: None,
            playback,
            refocused: None,
            note: None,
        }
    }
}

/// What `play` should start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlayTarget {
    /// Resume the current item.
    Resume,
    /// Start a track, episode, album, playlist, artist or show.
    Uri {
        /// What to play.
        uri: SpotifyUri,
        /// For a track or episode: the list to keep playing afterwards.
        #[serde(default)]
        context: Option<SpotifyUri>,
        /// For a context: start shuffled (spotify_player only).
        #[serde(default)]
        shuffle: bool,
    },
    /// Liked Songs.
    Liked {
        /// Maximum tracks to enqueue when spotify_player starts them; AppleScript plays the whole
        /// Liked Songs list.
        limit: u32,
        /// Shuffled, from a random song (a new shuffle order each time). Otherwise in list
        /// order from the first song: a shuffle Spotify kept for Liked Songs is turned off.
        random: bool,
    },
    /// A radio (recommendations) seeded from an item.
    Radio {
        /// The seed.
        uri: SpotifyUri,
    },
}

/// Absolute or relative volume.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeTarget {
    /// Set to this percent.
    Absolute(u8),
    /// Change by this many points.
    Delta(i16),
}

/// Repeat modes as the Spotify Web API names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    /// No repeat.
    Off,
    /// Repeat the playlist/album.
    Context,
    /// Repeat the current track.
    Track,
}

impl RepeatMode {
    fn from_web(value: Option<&str>) -> Option<Self> {
        match value? {
            "off" => Some(Self::Off),
            "context" => Some(Self::Context),
            "track" => Some(Self::Track),
            _ => None,
        }
    }

    /// The mode one `spotify_player playback repeat` moves to: off → track → context → off
    /// (spotify_player 0.25 `PlayerRequest::Repeat`, checked live). It steps from the instance's
    /// own idea of the mode and sets the next one explicitly.
    fn next(self) -> Self {
        match self {
            Self::Off => Self::Track,
            Self::Track => Self::Context,
            Self::Context => Self::Off,
        }
    }

    fn cycles_to(self, target: Self) -> usize {
        let mut mode = self;
        let mut cycles = 0;
        while mode != target {
            mode = mode.next();
            cycles += 1;
        }
        cycles
    }
}

impl std::str::FromStr for RepeatMode {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(Self::Off),
            "context" | "on" | "all" | "playlist" | "album" => Ok(Self::Context),
            "track" | "one" | "song" => Ok(Self::Track),
            other => Err(Error::invalid(
                format!("`{other}` is not a repeat mode."),
                "Use off, context (repeat the playlist/album) or track (repeat this song).",
            )),
        }
    }
}

/// What the running spotify_player instance believes, from `get key playback`.
///
/// `is_playing`, `repeat` and `shuffle` are the instance's working state, the values its next
/// command is computed from: `play` does nothing while it believes playback runs, `pause` does
/// nothing while it believes playback is paused, `play-pause` sends the opposite of what it
/// believes, `repeat` and `shuffle` step from what it believes. `item_uri`, `context_uri` and
/// `disallows` come from its last Web API read. All of them can lag behind Spotify.app.
#[derive(Clone, Debug, Default, PartialEq)]
struct PlayerView {
    item_uri: Option<String>,
    is_playing: bool,
    repeat: Option<RepeatMode>,
    shuffle: Option<bool>,
    context_uri: Option<String>,
    /// Actions the Web API disallows for its current item (`skipping_prev`, `pausing`, …).
    disallows: Vec<String>,
    /// Its item's title, length and album, to recognise a relinked song.
    item: WebItem,
}

impl PlayerView {
    fn from_json(value: &Value) -> Option<Self> {
        if value.is_null() {
            return None;
        }
        Some(Self {
            item: WebItem::from_player_json(value),
            item_uri: playing_item_uri(value),
            is_playing: value
                .get("is_playing")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            repeat: RepeatMode::from_web(value.get("repeat_state").and_then(Value::as_str)),
            shuffle: value.get("shuffle_state").and_then(Value::as_bool),
            context_uri: value
                .get("context")
                .and_then(|c| c.get("uri"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            disallows: value
                .pointer("/actions/disallows")
                .and_then(Value::as_object)
                .map(|all| {
                    all.iter()
                        .filter(|(_, v)| v.as_bool() == Some(true))
                        .map(|(k, _)| k.clone())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    fn disallows(&self, action: &str) -> bool {
        self.disallows.iter().any(|a| a == action)
    }

    /// Whether its item is the one Spotify.app has loaded (by id, or relinked).
    fn is_on(&self, playback: &Playback) -> bool {
        playback
            .track
            .as_ref()
            .is_some_and(|t| self.item.same_song(t).is_some())
    }
}

/// Stateless controller over an AppleScript runner, an optional spotify_player and the Web API.
pub struct Controller<'a> {
    /// Runs AppleScript against Spotify.app.
    pub script: &'a dyn Runner,
    /// `Err` holds why spotify_player cannot be used (missing binary, not signed in).
    pub player: std::result::Result<&'a SpotifyPlayer, Error>,
    /// Starts tracks and episodes without bringing Spotify.app forward (normally
    /// [`webapi::HttpWebApi`] over spotify_player's token). `Err` holds why it cannot be used.
    pub web: std::result::Result<&'a dyn WebApi, Error>,
    /// Gives the focus back when an AppleScript start brings Spotify.app to the front (normally
    /// [`focus::LaunchServices`]); `None` leaves the focus as it is.
    pub focus: Option<&'a dyn Focus>,
    /// Which tool to use.
    pub strategy: Strategy,
    /// How long to wait for spotify_player's effect to show up in Spotify.app.
    pub verify_timeout: Duration,
    /// Start Spotify.app in the background when a control command finds it closed.
    pub launch_spotify: bool,
}

const POLL: Duration = Duration::from_millis(120);
const LATE_GRACE: Duration = Duration::from_millis(400);
const APPLESCRIPT_VERIFY: Duration = Duration::from_millis(2500);
/// spotify_player's play, pause and volume show up in Spotify.app within about a second; for
/// these idempotent actions a later AppleScript fallback cannot be undone by a late effect.
const IDEMPOTENT_VERIFY: Duration = Duration::from_millis(1500);
/// Spotify.app's `previous` restarts the item past this point (the Web API never does).
const RESTART_AFTER_MS: u64 = 3000;
/// Longest wait for a one-shot Web API read in `status --full` (usually 1–2 s).
const FRESH_READ_TIMEOUT: Duration = Duration::from_secs(4);
/// The same when a command only uses it to skip work it would otherwise do.
const FRESH_CHECK_TIMEOUT: Duration = Duration::from_millis(2500);
/// The instance re-reads the Web API 1 s and 3 s after each command it runs.
const CATCH_UP_EXTRA: Duration = Duration::from_millis(1100);
/// Longest wait for one `playback repeat` step to be handled.
const REPEAT_STEP: Duration = Duration::from_millis(2000);
const VIEW_POLL: Duration = Duration::from_millis(60);
/// How long a start whose follow-up call was refused gets to show up in Spotify.app.
const START_AFTER_REFUSAL: Duration = Duration::from_millis(1000);
/// How long Spotify.app's shuffle must hold still, once a list (Liked Songs) plays, before it
/// counts as the one Spotify keeps for that list ([`Controller::settle_shuffle`]).
const SHUFFLE_QUIET: Duration = Duration::from_millis(450);
/// The longest wait for that, from the first read that shows the list playing.
const SHUFFLE_SETTLE: Duration = Duration::from_millis(1500);

/// The spotify_player half of an action (absent when spotify_player cannot do it).
type Primary<'a> = Option<&'a dyn Fn(&SpotifyPlayer) -> Result<()>>;

/// Which tool goes first under [`Strategy::Auto`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// spotify_player, then AppleScript: the default rule.
    PlayerFirst,
    /// AppleScript, then spotify_player: for actions spotify_player does worse on the desktop app.
    ScriptFirst,
}

/// How long to wait for spotify_player's effect before falling back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Patience {
    /// The whole verify timeout plus a last look: a skip or relative seek that lands after the
    /// fallback's would act twice.
    Full,
    /// Actions that set an absolute state (play, pause, a volume level): a late effect repeats
    /// the fallback's harmlessly, so fall back sooner.
    Idempotent,
}

/// One verified action: what to send through each tool and how to tell it worked.
struct Plan<'a> {
    action: &'a str,
    /// The name the running spotify_player instance logs the primary's request under
    /// (`request=Playback(<name>…)`), to notice its refusals.
    request: &'a str,
    route: Route,
    patience: Patience,
    primary: Primary<'a>,
    script: Option<&'a dyn Fn() -> Result<()>>,
    check: &'a dyn Fn(&Playback, Duration) -> bool,
}

/// How one path of a [`Plan`] went: the outcome, or why it did not work.
type Tried = std::result::Result<Outcome, Error>;

impl Controller<'_> {
    /// Reads Spotify.app's state. Never launches Spotify.
    ///
    /// # Errors
    /// AppleScript failures (permission, timeout).
    pub fn status(&self) -> Result<Playback> {
        let output = self.script.run(&applescript::status())?;
        applescript::parse_status(&output)
    }

    /// [`Self::status`] plus Web API facts (context, device, repeat mode) when spotify_player
    /// is usable. Web API errors are reported in `warnings` instead of failing the read.
    ///
    /// The facts come from the running spotify_player instance's memory when that agrees with
    /// Spotify.app (same item, repeat and shuffle; `web.source` is `spotify_player`; a play state
    /// that disagrees is left out).
    /// Otherwise they come from a one-shot Web API read (`web_api`, 1–4 s). When even that does
    /// not match Spotify.app, `web.stale` is true and a warning says why. A song the Web API
    /// plays under another id (relinked) is the same item; `web.relinked` says so.
    ///
    /// # Errors
    /// AppleScript failures.
    pub fn status_full(&self) -> Result<(Playback, Vec<Error>)> {
        let mut playback = self.status()?;
        let mut warnings = Vec::new();
        let player = match &self.player {
            Ok(player) => *player,
            Err(error) => {
                warnings.push(error.clone());
                return Ok((playback, warnings));
            }
        };
        let cached = match player.json(&["get", "key", "playback"]) {
            Ok(value) => web_facts(&value, "spotify_player", &playback),
            Err(error) => {
                warnings.push(error);
                return Ok((playback, warnings));
            }
        };
        if web_agrees(cached.as_ref(), &playback) {
            playback.web = cached.map(|(mut web, _)| {
                // Its play state is its own working state, which Spotify.app's pause or play
                // does not update; Spotify.app's `state` is the answer, so leave it out.
                if web.is_playing != Some(playback.state == PlayerState::Playing) {
                    web.is_playing = None;
                }
                web
            });
            return Ok((playback, warnings));
        }
        // The instance's memory describes an earlier moment: ask the Web API itself.
        match player.fresh_json(&["get", "key", "playback"], FRESH_READ_TIMEOUT) {
            Ok(value) => match web_facts(&value, "web_api", &playback) {
                Some((mut fresh, on_item)) => {
                    if !on_item {
                        warnings.push(web_behind(&fresh, &playback));
                        mark_stale(&mut fresh, &playback);
                    }
                    playback.web = Some(fresh);
                }
                None => warnings.push(Error::new(
                    "no_active_device",
                    "The Spotify Web API reports no active playback (Spotify.app has been paused for a while, or plays on a device the Web API does not see), so context, device and repeat mode are unknown.",
                    "Resume playback for a moment, then retry `spotify status --full`.",
                )),
            },
            Err(error) => {
                if let Some((mut stale, _)) = cached {
                    warnings.push(web_behind(&stale, &playback));
                    mark_stale(&mut stale, &playback);
                    playback.web = Some(stale);
                }
                warnings.push(error);
            }
        }
        Ok((playback, warnings))
    }

    /// Makes sure Spotify.app runs, launching it hidden when allowed.
    ///
    /// # Errors
    /// `spotify_not_running` (launch disabled), `spotify_not_installed`, or a launch timeout.
    pub fn ensure_running(&self) -> Result<Playback> {
        let playback = self.status()?;
        if playback.state != PlayerState::NotRunning {
            return Ok(playback);
        }
        if !self.launch_spotify {
            return Err(applescript::not_running());
        }
        launch_spotify()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            sleep(Duration::from_millis(400));
            match self.status() {
                Ok(playback) if playback.state != PlayerState::NotRunning => return Ok(playback),
                // Waiting cannot fix a refusal.
                Err(error) if error.code == "automation_permission_denied" => return Err(error),
                _ if Instant::now() > deadline => {
                    return Err(Error::new(
                        "spotify_not_running",
                        "Spotify.app was launched but did not become scriptable within 20 s.",
                        "Open Spotify manually, make sure you are signed in, then retry.",
                    )
                    .retryable());
                }
                _ => {}
            }
        }
    }

    fn wait_for(
        &self,
        timeout: Duration,
        issued: Instant,
        check: &dyn Fn(&Playback, Duration) -> bool,
    ) -> Result<(bool, Playback)> {
        let deadline = Instant::now() + timeout;
        loop {
            let playback = self.status()?;
            if check(&playback, issued.elapsed()) {
                return Ok((true, playback));
            }
            if Instant::now() >= deadline {
                return Ok((false, playback));
            }
            sleep(POLL);
        }
    }

    /// What the running spotify_player instance believes (`None`: it has no playback).
    fn view(&self, player: &SpotifyPlayer) -> Result<Option<PlayerView>> {
        Ok(PlayerView::from_json(
            &player.json(&["get", "key", "playback"])?,
        ))
    }

    /// Runs a plan's paths in route order per the strategy, verifying each with its check.
    fn attempt(&self, plan: &Plan<'_>) -> Result<Outcome> {
        let try_player = matches!(self.strategy, Strategy::Auto | Strategy::SpotifyPlayer);
        let try_script = matches!(self.strategy, Strategy::Auto | Strategy::Applescript);
        let order = match plan.route {
            Route::PlayerFirst => [Via::SpotifyPlayer, Via::Applescript],
            Route::ScriptFirst => [Via::Applescript, Via::SpotifyPlayer],
        };
        let mut failures: Vec<Fallback> = Vec::new();
        for via in order {
            let tried = match via {
                // After AppleScript, spotify_player only when it has a command for this. When
                // AppleScript was not tried (strategy spotify_player), spotify_player's "no
                // command" is the answer.
                Via::SpotifyPlayer
                    if plan.route == Route::ScriptFirst && plan.primary.is_none() && try_script =>
                {
                    continue;
                }
                Via::SpotifyPlayer if try_player => self.try_player(plan)?,
                // Without a script this path does not exist; the other one decides.
                Via::Applescript if try_script && plan.script.is_some() => self.try_script(plan)?,
                _ => continue,
            };
            match tried {
                Ok(mut outcome) => {
                    outcome.fallback = failures.into_iter().next();
                    return Ok(outcome);
                }
                Err(reason) => failures.push(Fallback { from: via, reason }),
            }
        }
        let mut failures = failures.into_iter();
        match (failures.next(), failures.next()) {
            (Some(only), None) => Err(only.reason),
            // A spotify_player that cannot run at all says less than the AppleScript that ran.
            (Some(first), Some(last))
                if last.from == Via::SpotifyPlayer && self.player.is_err() =>
            {
                Err(with_detail(
                    first.reason,
                    "second_attempt",
                    serde_json::to_value(&last).unwrap_or(Value::Null),
                ))
            }
            (Some(first), Some(last)) => Err(with_detail(
                last.reason,
                "first_attempt",
                serde_json::to_value(&first).unwrap_or(Value::Null),
            )),
            _ => Err(Error::unsupported(
                format!(
                    "AppleScript cannot `{}`; it needs spotify_player (Spotify Web API).",
                    plan.action
                ),
                "Use `spotify config set '{\"strategy\": \"auto\"}'` and make sure `spotify auth status` reports signed in.",
            )),
        }
    }

    fn try_player(&self, plan: &Plan<'_>) -> Result<Tried> {
        let (player, primary) = match (&self.player, plan.primary) {
            (Ok(player), Some(primary)) => (*player, primary),
            (Err(error), _) => return Ok(Err(error.clone())),
            (Ok(_), None) => {
                return Ok(Err(Error::unsupported(
                    format!("spotify_player has no command for `{}`.", plan.action),
                    "AppleScript handles it under strategy auto: `spotify config set '{\"strategy\": \"auto\"}'`.",
                )));
            }
        };
        let log = player.log_cursor();
        let issued = Instant::now();
        if let Err(error) = primary(player) {
            return Ok(Err(error));
        }
        let wait = match plan.patience {
            Patience::Full => self.verify_timeout,
            Patience::Idempotent => self.verify_timeout.min(IDEMPOTENT_VERIFY),
        };
        let deadline = Instant::now() + wait;
        let (mut ok, mut playback) = loop {
            let playback = self.status()?;
            if (plan.check)(&playback, issued.elapsed()) {
                break (true, playback);
            }
            // The instance logs a refused Web API call; no effect will follow.
            if let Some(refusal) = crate::player::logged_failure(&log, plan.request) {
                // Except for a start: it is two calls (play, then shuffle), and a refused second
                // one leaves the start made. Let that show before starting again elsewhere.
                if plan.request.starts_with("Start") {
                    let (started, playback) =
                        self.wait_for(START_AFTER_REFUSAL, issued, plan.check)?;
                    if started {
                        break (true, playback);
                    }
                }
                return Ok(Err(refusal));
            }
            if Instant::now() >= deadline {
                break (false, playback);
            }
            sleep(POLL);
        };
        if !ok && plan.patience == Patience::Full {
            // A late effect must not be applied twice (a second skip, a seek that moves on
            // again): one more short look before falling back.
            sleep(LATE_GRACE);
            playback = self.status()?;
            ok = (plan.check)(&playback, issued.elapsed());
        }
        if ok {
            return Ok(Ok(Outcome::new(plan.action, Via::SpotifyPlayer, playback)));
        }
        Ok(Err(Error::new(
            "no_effect",
            format!(
                "spotify_player accepted `{}` but Spotify.app did not change within {} ms.",
                plan.action,
                wait.as_millis()
            ),
            "spotify_player may be controlling a different device, or the Web API ignored the command (it rate limits or refuses some actions).",
        )))
    }

    fn try_script(&self, plan: &Plan<'_>) -> Result<Tried> {
        let Some(script) = plan.script else {
            return Ok(Err(Error::internal("no AppleScript for this action")));
        };
        // Spotify.app comes to the front when AppleScript starts something.
        let focus = if plan.request.starts_with("Start") {
            self.focus
        } else {
            None
        };
        let (tried, refocused) = focus::keep_in_background(focus, || -> Result<Tried> {
            let issued = Instant::now();
            script()?;
            let (ok, playback) = self.wait_for(APPLESCRIPT_VERIFY, issued, plan.check)?;
            if ok {
                return Ok(Ok(Outcome::new(plan.action, Via::Applescript, playback)));
            }
            Ok(Err(never_reflected(plan.action, &playback)))
        });
        Ok(match (tried?, refocused) {
            (Ok(mut outcome), refocused) => {
                outcome.refocused = refocused;
                Ok(outcome)
            }
            (Err(reason), Some(refocused)) => Err(with_detail(
                reason,
                "refocused",
                serde_json::to_value(refocused).unwrap_or(Value::Null),
            )),
            (Err(reason), None) => Err(reason),
        })
    }

    /// Checks that spotify_player believes playback is `playing`, as Spotify.app says: its
    /// `play` only acts when it believes playback is paused, and `pause` only when it believes
    /// it runs.
    fn expect_play_state(&self, player: &SpotifyPlayer, action: &str, playing: bool) -> Result<()> {
        let word = |playing: bool| if playing { "playing" } else { "paused" };
        match self.view(player)? {
            Some(view) if view.is_playing == playing => Ok(()),
            Some(view) => Err(state_mismatch(action, word(view.is_playing), word(playing))),
            None => Err(state_mismatch(action, "nothing is loaded", word(playing))),
        }
    }

    /// Starts or resumes playback.
    ///
    /// # Errors
    /// Classified tool errors, `verification_failed`, or `unsupported`. When a start fails and
    /// leaves Spotify.app with nothing loaded, what was loaded before is put back and the
    /// error's details say so (`restored`).
    pub fn play(&self, target: &PlayTarget) -> Result<Outcome> {
        let before = self.ensure_running()?;
        if matches!(target, PlayTarget::Resume) {
            return self.resume(&before);
        }
        // Where the current item plays from, to put it back if a start empties Spotify.app.
        let context = match (&self.player, before.track.is_some()) {
            (Ok(player), true) if self.strategy != Strategy::Applescript => self
                .view(player)
                .ok()
                .flatten()
                .filter(|view| view.is_on(&before))
                .and_then(|view| view.context_uri),
            _ => None,
        };
        self.start(&before, target)
            .map_err(|error| self.restore_if_emptied(error, &before, context.as_deref()))
    }

    fn resume(&self, before: &Playback) -> Result<Outcome> {
        if before.track.is_none() {
            return Err(Error::new(
                "nothing_to_resume",
                "Spotify has nothing loaded, so there is nothing to resume.",
                "Start something: `spotify play --search 'query'`, `spotify play <uri>` or `spotify play --liked`.",
            ));
        }
        if before.state == PlayerState::Playing {
            return Ok(Outcome::unchanged("play", before.clone()));
        }
        self.attempt(&Plan {
            action: "play",
            request: "Play",
            route: Route::PlayerFirst,
            patience: Patience::Idempotent,
            primary: Some(&|p: &SpotifyPlayer| {
                self.expect_play_state(p, "play", false)?;
                p.run(&["playback", "play"]).map(drop)
            }),
            script: Some(&|| applescript::expect_ok(&self.script.run(&applescript::play())?)),
            check: &|pb, _| pb.state == PlayerState::Playing,
        })
    }

    fn start(&self, before: &Playback, target: &PlayTarget) -> Result<Outcome> {
        let before_uri = before.track.as_ref().map(|t| t.uri.clone());
        let before_position = before.position_ms;
        let was_playing = before.state == PlayerState::Playing;
        // A different item, or the same one restarted from the top (a list whose first item is
        // the one loaded): clearly behind where it would be had nothing happened.
        let started = |pb: &Playback, since: Duration| {
            let since = u64::try_from(since.as_millis()).unwrap_or(0);
            let unchanged = before_position + if was_playing { since } else { 0 };
            pb.state == PlayerState::Playing
                && pb.track.is_some()
                && (pb.track.as_ref().map(|t| t.uri.clone()) != before_uri
                    || (before_position >= 5000 && pb.position_ms < 4000 + since)
                    || pb.position_ms + 1500 < unchanged)
        };
        match target {
            PlayTarget::Resume => self.resume(before),
            PlayTarget::Uri {
                uri,
                context,
                shuffle,
            } => {
                let target_uri = uri.uri();
                let context_uri = context.as_ref().map(SpotifyUri::uri);
                match uri.kind {
                    Kind::Track | Kind::Episode => {
                        // The Web API first: it leaves Spotify.app in the background.
                        let web_failure = if self.strategy == Strategy::Applescript {
                            None
                        } else {
                            match self.start_with_web(before, uri, context.as_ref())? {
                                Ok(outcome) => return Ok(outcome),
                                // No release of it plays here: AppleScript's start would only
                                // empty Spotify.app. Said at once, playback untouched.
                                Err(reason) if reason.code == "not_playable" => {
                                    return Err(reason);
                                }
                                Err(reason) => Some(Fallback {
                                    from: Via::WebApi,
                                    reason,
                                }),
                            }
                        };
                        // What the Web API's lookup found: the facts to recognise the track by
                        // (Spotify.app may show the release it plays from), and the album the
                        // AppleScript start plays it in (so the album goes on after it), named as
                        // that album lists it (a relinked song by the id it links from: by the
                        // id that plays, Spotify.app starts the album's first track instead).
                        // Not when the Web API's start showed the album does not place the track
                        // (refused, or another of its tracks played): the track alone then. The
                        // caller's context always wins.
                        let looked_up = (uri.kind == Kind::Track)
                            .then(|| webapi::cached_facts(uri))
                            .flatten();
                        let in_album = looked_up.as_ref().filter(|facts| {
                            context.is_none()
                                && !web_failure
                                    .as_ref()
                                    .is_some_and(|failure| album_refused(&failure.reason))
                                && facts
                                    .context
                                    .as_deref()
                                    .is_some_and(|c| c.starts_with("spotify:album:"))
                        });
                        let script = match (&context_uri, in_album) {
                            (Some(context), _) => applescript::play_uri(&target_uri, Some(context)),
                            (None, Some(facts)) => {
                                applescript::play_uri(&facts.listed_as, facts.context.as_deref())
                            }
                            (None, None) => applescript::play_uri(&target_uri, None),
                        };
                        let fallback = || applescript::expect_ok(&self.script.run(&script)?);
                        let expected = target_uri.clone();
                        let is_item = move |t: &Track| {
                            t.uri == expected || looked_up.as_ref().is_some_and(|facts| facts.is(t))
                        };
                        let replay = before.track.as_ref().is_some_and(&is_item);
                        // Replaying the item already loaded only counts once it restarted.
                        let check = move |pb: &Playback, since: Duration| {
                            pb.state == PlayerState::Playing
                                && pb.track.as_ref().is_some_and(&is_item)
                                && (!replay
                                    || pb.position_ms
                                        < 4000 + u64::try_from(since.as_millis()).unwrap_or(0))
                        };
                        let id = uri.id.clone();
                        let primary = move |p: &SpotifyPlayer| {
                            p.run(&["playback", "start", "track", "--id", &id])
                                .map(drop)
                        };
                        // Then AppleScript. spotify_player only when AppleScript is not allowed: it
                        // starts a track as a list of ids, which leaves the desktop app stopped
                        // with nothing loaded, so it is no fallback for a start AppleScript could
                        // not verify (a slow first start after launching Spotify.app would be
                        // emptied).
                        let primary: Primary<'_> = if uri.kind == Kind::Track
                            && context.is_none()
                            && self.strategy == Strategy::SpotifyPlayer
                        {
                            Some(&primary)
                        } else {
                            None
                        };
                        after_web(
                            self.attempt(&Plan {
                                action: "play",
                                request: "StartTrack",
                                route: Route::ScriptFirst,
                                patience: Patience::Full,
                                primary,
                                script: Some(&fallback),
                                check: &check,
                            }),
                            web_failure,
                        )
                    }
                    Kind::Show => {
                        // The Web API first, as for a track: AppleScript would bring Spotify.app
                        // forward. spotify_player cannot start a show.
                        let web_failure = if self.strategy == Strategy::Applescript {
                            None
                        } else {
                            match self.list_with_web(
                                "play",
                                &target_uri,
                                None,
                                was_playing,
                                &started,
                            )? {
                                Ok((outcome, _)) => return Ok(outcome),
                                Err(reason) => Some(Fallback {
                                    from: Via::WebApi,
                                    reason,
                                }),
                            }
                        };
                        let script = applescript::play_uri(&target_uri, None);
                        let fallback = || applescript::expect_ok(&self.script.run(&script)?);
                        after_web(
                            self.attempt(&Plan {
                                action: "play",
                                request: "StartContext",
                                route: Route::ScriptFirst,
                                patience: Patience::Full,
                                primary: None,
                                script: Some(&fallback),
                                check: &started,
                            }),
                            web_failure,
                        )
                    }
                    Kind::Album | Kind::Playlist | Kind::Artist => {
                        let script = applescript::play_uri(&target_uri, context_uri.as_deref());
                        let fallback = || applescript::expect_ok(&self.script.run(&script)?);
                        let id = uri.id.clone();
                        let kind = uri.kind.as_str().to_owned();
                        let shuffle = *shuffle;
                        let primary = move |p: &SpotifyPlayer| {
                            let mut args = vec![
                                "playback",
                                "start",
                                "context",
                                "--id",
                                id.as_str(),
                                kind.as_str(),
                            ];
                            if shuffle {
                                args.push("--shuffle");
                            }
                            p.run(&args).map(drop)
                        };
                        self.attempt(&Plan {
                            action: "play",
                            request: "StartContext",
                            route: Route::PlayerFirst,
                            patience: Patience::Full,
                            primary: Some(&primary),
                            script: Some(&fallback),
                            check: &started,
                        })
                    }
                }
            }
            PlayTarget::Liked { limit, random } => {
                self.play_liked(*limit, *random, was_playing, &started)
            }
            PlayTarget::Radio { uri } => {
                let id = uri.id.clone();
                let kind = uri.kind.as_str().to_owned();
                if matches!(uri.kind, Kind::Show | Kind::Episode) {
                    return Err(Error::invalid(
                        "Radio can be seeded from a track, album, artist or playlist, not a podcast.",
                        "Pass a track, album, artist or playlist uri.",
                    ));
                }
                self.attempt(&Plan {
                    action: "play_radio",
                    request: "StartRadio",
                    route: Route::PlayerFirst,
                    patience: Patience::Full,
                    primary: Some(&|p: &SpotifyPlayer| {
                        p.run(&[
                            "playback",
                            "start",
                            "radio",
                            "--id",
                            id.as_str(),
                            kind.as_str(),
                        ])
                        .map(drop)
                    }),
                    script: None,
                    check: &started,
                })
            }
        }
    }

    /// Starts a track or episode through the Web API ([`webapi::start`]) and waits (up to the
    /// verify timeout) until Spotify.app plays it: the item asked for, or the same song under
    /// another id (relinked), from its start. A start that lands on another item of the list is
    /// sent again once by the track's position in its album; if that does not help either, the
    /// caller falls back. A start whose answer was a 5xx or never came is looked for all the
    /// same, so the fallback never starts it a second time.
    fn start_with_web(
        &self,
        before: &Playback,
        uri: &SpotifyUri,
        context: Option<&SpotifyUri>,
    ) -> Result<Tried> {
        let web = match &self.web {
            Ok(web) => *web,
            Err(error) => return Ok(Err(error.clone())),
        };
        let mut issued = Instant::now();
        // Starting the item already loaded only counts once it went back to its start.
        let replay = |item: &webapi::ItemFacts| before.track.as_ref().is_some_and(|t| item.is(t));
        let landed_on = |item: &webapi::ItemFacts| {
            let replay = replay(item);
            let item = item.clone();
            move |pb: &Playback, since: Duration| {
                pb.state == PlayerState::Playing
                    && pb.track.as_ref().is_some_and(|t| item.is(t))
                    && (!replay
                        || pb.position_ms < 4000 + u64::try_from(since.as_millis()).unwrap_or(0))
            }
        };
        let app_playing = before.state == PlayerState::Playing;
        let mut started = match webapi::start(web, uri, context, app_playing) {
            Ok(started) => started,
            Err(error) if webapi::unanswered(&error) => {
                return match webapi::cached_facts(uri) {
                    Some(item) => self.confirm_unanswered("play", error, issued, &landed_on(&item)),
                    None => Ok(Err(error)),
                };
            }
            Err(error) => return Ok(Err(error)),
        };
        let before_uri = before.track.as_ref().map(|t| t.uri.clone());
        loop {
            let landed = landed_on(&started.item);
            let item = &started.item;
            // Another item of the list plays: the start named the wrong one.
            let elsewhere = |pb: &Playback| {
                pb.state == PlayerState::Playing
                    && pb
                        .track
                        .as_ref()
                        .is_some_and(|t| !item.is(t) && Some(&t.uri) != before_uri.as_ref())
            };
            let deadline = Instant::now() + self.verify_timeout;
            let mut playback = loop {
                let playback = self.status()?;
                if landed(&playback, issued.elapsed())
                    || elsewhere(&playback)
                    || Instant::now() >= deadline
                {
                    break playback;
                }
                sleep(POLL);
            };
            if !landed(&playback, issued.elapsed()) && !elsewhere(&playback) {
                // A late start would land after AppleScript's: one more look first.
                sleep(LATE_GRACE);
                playback = self.status()?;
            }
            if landed(&playback, issued.elapsed()) {
                let mut outcome = Outcome::new("play", Via::WebApi, playback);
                outcome.note = remote_note(started.remote, &started.device);
                return Ok(Ok(outcome));
            }
            let observed = playback.track.as_ref().map(|t| t.uri.clone());
            if elsewhere(&playback) {
                let again_at = Instant::now();
                match started.by_position(web) {
                    Ok(Some(again)) => {
                        started = again;
                        issued = again_at;
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) if webapi::unanswered(&error) => {
                        return self.confirm_unanswered("play", error, again_at, &landed);
                    }
                    Err(error) => return Ok(Err(error)),
                }
            }
            let ms = self.verify_timeout.as_millis();
            let message = match (&observed, elsewhere(&playback)) {
                (Some(other), true) => format!(
                    "The Spotify Web API started {} in {}, but Spotify.app plays {other} instead.",
                    started.item.requested, started.context
                ),
                _ => format!(
                    "The Spotify Web API accepted the start of {} in {}, but Spotify.app did not play it within {ms} ms.",
                    started.item.requested, started.context
                ),
            };
            return Ok(Err(Error::new(
                "no_effect",
                message,
                "Under strategy auto AppleScript starts it instead; nothing to do.",
            )
            .with_details(json!({
                "context": started.context,
                "device": started.device.name,
                "offset": match &started.offset {
                    webapi::Offset::Uri(uri) => json!({"uri": uri}),
                    webapi::Offset::Position(position) => json!({"position": position}),
                },
                "observed": observed,
                // The album did not place the track: the fallback does not start it there.
                "landed_elsewhere": elsewhere(&playback),
            }))));
        }
    }

    /// After a start whose answer was a 5xx or never came ([`webapi::unanswered`]): looks for it
    /// in Spotify.app (the verify timeout, then one more look). When `landed` shows, the start
    /// went through: that is the outcome, through the Web API with a note, and nothing starts it
    /// again. Otherwise `error`, and the caller falls back.
    fn confirm_unanswered(
        &self,
        action: &str,
        error: Error,
        issued: Instant,
        landed: &dyn Fn(&Playback, Duration) -> bool,
    ) -> Result<Tried> {
        let (mut ok, mut playback) = self.wait_for(self.verify_timeout, issued, landed)?;
        if !ok {
            sleep(LATE_GRACE);
            playback = self.status()?;
            ok = landed(&playback, issued.elapsed());
        }
        if !ok {
            return Ok(Err(error));
        }
        let answer = error
            .details
            .as_ref()
            .and_then(|d| d.get("status"))
            .and_then(Value::as_u64)
            .map_or_else(
                || "no answer in time".to_owned(),
                |status| format!("HTTP {status}"),
            );
        let mut outcome = Outcome::new(action, Via::WebApi, playback);
        outcome.note = Some(format!(
            "The Spotify Web API gave {answer} to the start, but Spotify.app plays it: the start went through, so it was not started again."
        ));
        Ok(Ok(outcome))
    }

    /// Starts the list `context` (Liked Songs, a show) through the Web API
    /// ([`webapi::start_list`]; `app_playing`: whether Spotify.app played before) and waits (up
    /// to the verify timeout, then one more look) until Spotify.app shows `check`. The outcome and
    /// where the start went (`None` when its answer never came and it showed all the same), or
    /// why it did not work.
    fn list_with_web(
        &self,
        action: &str,
        context: &str,
        position: Option<u32>,
        app_playing: bool,
        check: &dyn Fn(&Playback, Duration) -> bool,
    ) -> Result<std::result::Result<(Outcome, Option<webapi::ListStarted>), Error>> {
        let web = match &self.web {
            Ok(web) => *web,
            Err(error) => return Ok(Err(error.clone())),
        };
        let issued = Instant::now();
        let started = match webapi::start_list(web, context, position, app_playing) {
            Ok(started) => started,
            Err(error) if webapi::unanswered(&error) => {
                return Ok(self
                    .confirm_unanswered(action, error, issued, check)?
                    .map(|outcome| (outcome, None)));
            }
            Err(error) => return Ok(Err(error)),
        };
        let (mut ok, mut playback) = self.wait_for(self.verify_timeout, issued, check)?;
        if !ok {
            // A late start would land after AppleScript's: one more look first.
            sleep(LATE_GRACE);
            playback = self.status()?;
            ok = check(&playback, issued.elapsed());
        }
        if !ok {
            return Ok(Err(Error::new(
                "no_effect",
                format!(
                    "The Spotify Web API accepted the start of {context}, but Spotify.app did not play it within {} ms.",
                    self.verify_timeout.as_millis()
                ),
                "Under strategy auto AppleScript starts it instead; nothing to do.",
            )
            .with_details(json!({
                "context": context,
                "device": started.device.name,
                "position": position,
                "observed": playback.track.as_ref().map(|t| t.uri.clone()),
            }))));
        }
        let mut outcome = Outcome::new(action, Via::WebApi, playback);
        outcome.note = remote_note(started.remote, &started.device);
        Ok(Ok((outcome, Some(started))))
    }

    /// Liked Songs, the Web API first, then AppleScript: see [`Self::liked_with_web`] and
    /// [`Self::liked_with_script`].
    fn play_liked(
        &self,
        limit: u32,
        random: bool,
        app_playing: bool,
        check: &dyn Fn(&Playback, Duration) -> bool,
    ) -> Result<Outcome> {
        let collection = self
            .player
            .as_ref()
            .ok()
            .and_then(|p| p.username())
            .map(|user| format!("spotify:user:{user}:collection"));
        // The Web API first: it leaves Spotify.app in the background.
        let web_failure = match &collection {
            Some(collection) if self.strategy != Strategy::Applescript => {
                match self.liked_with_web(collection, random, app_playing, check)? {
                    Ok(outcome) => return Ok(outcome),
                    Err(reason) => Some(Fallback {
                        from: Via::WebApi,
                        reason,
                    }),
                }
            }
            _ => None,
        };
        after_web(
            self.liked_with_script(collection.as_deref(), limit, random, check),
            web_failure,
        )
    }

    /// Liked Songs through the Web API: the list started at its first song (in order) or at a
    /// random position (`random`; its size from `GET /v1/me/tracks`), then its shuffle set.
    /// Spotify switches to the shuffle it keeps for Liked Songs when the list starts (a shuffle
    /// set before would only change the list playing before; see [`webapi::start_list`]), and
    /// a moment after its first song shows. So shuffle is checked in Spotify.app once it plays
    /// and has settled, and set when it differs: on for `random` (a random song, then shuffled),
    /// off in order, where a list that began elsewhere (it started shuffled) is started again
    /// from its first song; the result is checked again ([`Self::liked_settle`]). Once Liked Songs plays, those
    /// follow-up steps' failures are errors, not fallbacks: a fallback would start the list a
    /// second time.
    fn liked_with_web(
        &self,
        collection: &str,
        random: bool,
        app_playing: bool,
        check: &dyn Fn(&Playback, Duration) -> bool,
    ) -> Result<Tried> {
        let web = match &self.web {
            Ok(web) => *web,
            Err(error) => return Ok(Err(error.clone())),
        };
        let liked = match webapi::liked_songs(web) {
            Ok(liked) => liked,
            Err(error) => return Ok(Err(error)),
        };
        if liked.total == 0 {
            return Err(Error::not_found(
                "Liked Songs is empty, so there is nothing to play.",
                "Like songs first (`spotify like` while one plays), or play something else: `spotify play --search '<query>'`.",
            ));
        }
        let position = if random { random_below(liked.total) } else { 0 };
        let (mut outcome, started) = match self.list_with_web(
            "play_liked",
            collection,
            Some(position),
            app_playing,
            check,
        )? {
            Ok(done) => done,
            Err(error) => return Ok(Err(error)),
        };
        // Without a device (the start's answer never came), the one playing now.
        let device = started.map(|s| s.device).unwrap_or_default();
        outcome.playback = self.liked_settle(
            web,
            &device,
            collection,
            random,
            liked.first.as_ref(),
            outcome.playback,
        )?;
        Ok(Ok(outcome))
    }

    /// Liked Songs, just started through the Web API and playing as `now`: once its shuffle has
    /// settled ([`Self::settle_shuffle`]: Spotify switches to the shuffle it keeps for Liked Songs
    /// a moment after the first song shows), sets it to `random` when it differs, and in order
    /// (`!random`) starts the list again from `first` when it began elsewhere (it started
    /// shuffled). Whenever it changed something it lets the shuffle settle again and checks the
    /// result, up to twice (a late switch can undo the first round), so what it returns is what
    /// Spotify.app keeps: shuffle as asked and, in order, the first song.
    ///
    /// # Errors
    /// `verification_failed` (retryable) when a step does not show or the result does not hold.
    fn liked_settle(
        &self,
        web: &dyn WebApi,
        device: &crate::model::Device,
        collection: &str,
        random: bool,
        first: Option<&webapi::ItemFacts>,
        now: Playback,
    ) -> Result<Playback> {
        let at_first = |pb: &Playback| {
            random || first.is_none_or(|first| pb.track.as_ref().is_some_and(|t| first.is(t)))
        };
        let in_place = |pb: &Playback| pb.shuffling == Some(random) && at_first(pb);
        let mut now = self.settle_shuffle(now)?;
        for _ in 0..2 {
            if in_place(&now) {
                return Ok(now);
            }
            if now.shuffling != Some(random) {
                now = self.set_list_shuffle(web, device, random)?;
            }
            if let Some(first) = first.filter(|_| !at_first(&now)) {
                // It began where the shuffle it kept starts: from the first song, now in order.
                now = self.liked_again_from_first(web, collection, first, now)?;
            }
            now = self.settle_shuffle(now)?;
        }
        if in_place(&now) {
            return Ok(now);
        }
        let word = if random { "on" } else { "off" };
        Err(Error::new(
            "verification_failed",
            if now.shuffling == Some(random) {
                "Liked Songs plays with shuffle off, but Spotify.app did not stay on its first song.".to_owned()
            } else {
                format!(
                    "Liked Songs plays, but Spotify.app's shuffle did not stay {word}: Spotify switched it back after it was set."
                )
            },
            format!(
                "Check Spotify.app, then retry `spotify play --liked{}` or run `spotify shuffle {word}`.",
                if random { " --random" } else { "" }
            ),
        )
        .retryable()
        .with_details(json!({"observed": now, "first": first.map(|f| f.requested.clone())})))
    }

    /// Starts Liked Songs (playing, just set in order) again at its first song through the Web
    /// API and waits until Spotify.app plays `first`. Returns Spotify.app then.
    fn liked_again_from_first(
        &self,
        web: &dyn WebApi,
        collection: &str,
        first: &webapi::ItemFacts,
        now: Playback,
    ) -> Result<Playback> {
        let issued = Instant::now();
        // Liked Songs plays now, on the device it was started on.
        let (ok, again) = match webapi::start_list(web, collection, Some(0), true) {
            // A lost answer may still have started it.
            Err(error) if !webapi::unanswered(&error) => (false, now),
            _ => self.wait_for(APPLESCRIPT_VERIFY, issued, &|pb, _| {
                pb.state == PlayerState::Playing && pb.track.as_ref().is_some_and(|t| first.is(t))
            })?,
        };
        if ok {
            return Ok(again);
        }
        Err(Error::new(
            "verification_failed",
            "Liked Songs plays in order, but Spotify.app did not start it again from its first song.",
            "Check Spotify.app, then retry `spotify play --liked`.",
        )
        .retryable()
        .with_details(json!({"observed": again, "first": first.requested})))
    }

    /// Waits until Spotify.app's shuffle holds still after a list started (Liked Songs), from
    /// `now`, the first read that showed it: Spotify switches to the shuffle it keeps for the
    /// list a moment after its first song shows (seen 2026-09: 75 ms later, so that first read
    /// can still carry the shuffle of the list before). Settled once neither shuffle nor the item
    /// changed for [`SHUFFLE_QUIET`] (several reads in a row), at most [`SHUFFLE_SETTLE`] after
    /// `now`. Returns the last read.
    fn settle_shuffle(&self, now: Playback) -> Result<Playback> {
        let begun = Instant::now();
        let mut still_since = begun;
        let mut last = now;
        while still_since.elapsed() < SHUFFLE_QUIET && begun.elapsed() < SHUFFLE_SETTLE {
            sleep(POLL);
            let read = self.status()?;
            let item = |pb: &Playback| pb.track.as_ref().map(|t| t.uri.clone());
            if read.shuffling != last.shuffling || item(&read) != item(&last) {
                still_since = Instant::now();
            }
            last = read;
        }
        Ok(last)
    }

    /// Sets shuffle for the list Spotify.app plays (just started through the Web API) through the
    /// Web API and waits until Spotify.app shows it; AppleScript sets it when that does not show
    /// (except under strategy spotify_player). Returns Spotify.app after that.
    fn set_list_shuffle(
        &self,
        web: &dyn WebApi,
        device: &crate::model::Device,
        on: bool,
    ) -> Result<Playback> {
        let sent = webapi::set_shuffle(web, device, on);
        if sent.is_ok() {
            let (ok, now) = self.wait_for(APPLESCRIPT_VERIFY, Instant::now(), &|pb, _| {
                pb.shuffling == Some(on)
            })?;
            if ok {
                return Ok(now);
            }
        }
        if self.strategy == Strategy::SpotifyPlayer {
            return Err(sent.err().unwrap_or_else(|| {
                Error::new(
                    "verification_failed",
                    format!(
                        "Liked Songs is playing, but Spotify.app did not turn its shuffle {}.",
                        if on { "on" } else { "off" }
                    ),
                    "Check Spotify.app, then retry.",
                )
                .retryable()
            }));
        }
        self.liked_shuffle(on)?;
        self.status()
    }

    /// Liked Songs through AppleScript, which plays the Liked Songs list itself
    /// (`spotify:user:<id>:collection`, the id spotify_player is signed in as), so it keeps going
    /// and `next` works; spotify_player can only start a list of track ids, which the desktop app
    /// answers by stopping.
    ///
    /// Spotify keeps a shuffle setting per list and switches to Liked Songs' own when it starts.
    /// Shuffled, the list starts at the song its kept order starts with, the same one every time;
    /// in order, at its first song. So without `random` a kept shuffle is turned off and the list
    /// started again, and with `random` shuffle is switched off and on (Spotify then draws a new
    /// order) before one skip. Each step is checked in Spotify.app, and none is taken before
    /// Liked Songs plays and its shuffle has settled ([`Self::settle_shuffle`]): shuffle and the
    /// skip would otherwise land on what was playing, or be undone by the shuffle Spotify switches
    /// to a moment after the start.
    fn liked_with_script(
        &self,
        collection: Option<&str>,
        limit: u32,
        random: bool,
        check: &dyn Fn(&Playback, Duration) -> bool,
    ) -> Result<Outcome> {
        let play_list = || -> Result<()> {
            let Some(collection) = collection else {
                return Err(Error::internal("Liked Songs has no list uri"));
            };
            let start = applescript::play_uri(collection, None);
            let issued = Instant::now();
            applescript::expect_ok(&self.script.run(&start)?)?;
            let (started, now) = self.wait_for(APPLESCRIPT_VERIFY, issued, check)?;
            if !started {
                // Said now: the check after this would wait as long again, and a start that
                // shows only then would count without its shuffle step.
                return Err(never_reflected("play_liked", &now));
            }
            // Spotify switches to the shuffle it keeps for Liked Songs a moment later.
            let now = self.settle_shuffle(now)?;
            match now.shuffling {
                Some(_) if random => self.liked_at_random(&now)?,
                Some(true) => self.liked_in_order(collection, &now)?,
                _ => return Ok(()),
            }
            // What it keeps now, after the steps: shuffle as asked.
            let end = self.settle_shuffle(self.status()?)?;
            if end.shuffling == Some(random) {
                return Ok(());
            }
            Err(Error::new(
                "verification_failed",
                format!(
                    "Liked Songs plays, but Spotify.app's shuffle did not stay {}: Spotify switched it back after it was set.",
                    if random { "on" } else { "off" }
                ),
                "Check Spotify.app, then retry.",
            )
            .retryable()
            .with_details(json!({"observed": end})))
        };
        let limit = limit.to_string();
        let primary = |p: &SpotifyPlayer| {
            let mut args = vec!["playback", "start", "liked", "--limit", limit.as_str()];
            if random {
                args.push("--random");
            }
            p.run(&args).map(drop)
        };
        // spotify_player's start empties the desktop app, so it never runs as the fallback of a
        // list start that AppleScript can make: only without the list's uri, or when it is the
        // only tool allowed.
        let primary: Primary<'_> =
            if collection.is_none() || self.strategy == Strategy::SpotifyPlayer {
                Some(&primary)
            } else {
                None
            };
        self.attempt(&Plan {
            action: "play_liked",
            request: "StartLikedTracks",
            route: Route::ScriptFirst,
            patience: Patience::Full,
            primary,
            script: if collection.is_some() {
                Some(&play_list)
            } else {
                None
            },
            check,
        })
    }

    /// Liked Songs started shuffled, from a shuffle Spotify kept for it: turns that off and
    /// starts the list again, which then begins at its first song.
    fn liked_in_order(&self, collection: &str, started: &Playback) -> Result<()> {
        let shuffled = started.track.as_ref().map(|t| t.uri.clone());
        self.liked_shuffle(false)?;
        applescript::expect_ok(&self.script.run(&applescript::play_uri(collection, None))?)?;
        // The first song replaces the shuffled one, unless they are the same song (then nothing
        // shows, and in order is what counts).
        let (_, now) = self.wait_for(APPLESCRIPT_VERIFY, Instant::now(), &|pb, _| {
            pb.state == PlayerState::Playing
                && pb.track.is_some()
                && pb.track.as_ref().map(|t| t.uri.clone()) != shuffled
        })?;
        if now.state == PlayerState::Playing && now.shuffling == Some(false) {
            return Ok(());
        }
        Err(Error::new(
            "verification_failed",
            "Liked Songs' shuffle was turned off, but Spotify.app did not start the list again from its first song.",
            "Check Spotify.app, then retry `spotify play --liked`.",
        )
        .retryable()
        .with_details(json!({"observed": now})))
    }

    /// Moves Liked Songs, just started, to a random song: switching shuffle off and on makes
    /// Spotify draw a new order (a shuffled list otherwise starts where its kept order does), and
    /// one skip leaves the song that started.
    fn liked_at_random(&self, started: &Playback) -> Result<()> {
        let first = started.track.as_ref().map(|t| t.uri.clone());
        self.liked_shuffle(false)?;
        self.liked_shuffle(true)?;
        applescript::expect_ok(&self.script.run(&applescript::next())?)?;
        let (moved, now) = self.wait_for(APPLESCRIPT_VERIFY, Instant::now(), &|pb, _| {
            pb.track.is_some() && pb.track.as_ref().map(|t| t.uri.clone()) != first
        })?;
        if moved {
            return Ok(());
        }
        Err(Error::new(
            "verification_failed",
            "Liked Songs is playing shuffled, but the skip to a random song did not show in Spotify.app.",
            "Run `spotify next`, or retry `spotify play --liked --random`.",
        )
        .retryable()
        .with_details(json!({"observed": now})))
    }

    /// Sets shuffle for the list Spotify.app plays (Liked Songs, just started) and waits until
    /// Spotify.app shows it.
    fn liked_shuffle(&self, on: bool) -> Result<()> {
        applescript::expect_ok(&self.script.run(&applescript::shuffle(on))?)?;
        let (ok, now) = self.wait_for(APPLESCRIPT_VERIFY, Instant::now(), &|pb, _| {
            pb.shuffling == Some(on)
        })?;
        if ok {
            return Ok(());
        }
        Err(Error::new(
            "verification_failed",
            format!(
                "Liked Songs is playing, but Spotify.app did not turn its shuffle {}.",
                if on { "on" } else { "off" }
            ),
            "Check Spotify.app, then retry.",
        )
        .retryable()
        .with_details(json!({"observed": now})))
    }

    /// When `error` left Spotify.app with nothing loaded although `before` had an item, plays
    /// that item again (in `context` when known) at its position and pause state, through
    /// AppleScript whatever the strategy, and records the attempt in the error's details.
    fn restore_if_emptied(&self, error: Error, before: &Playback, context: Option<&str>) -> Error {
        let Some(track) = before.track.as_ref() else {
            return error;
        };
        match self.status() {
            Ok(now) if now.track.is_none() && now.state != PlayerState::NotRunning => {}
            _ => return error,
        }
        let (restored, _) =
            focus::keep_in_background(self.focus, || self.restore(before, track, context));
        match restored {
            Ok(playback) => with_detail(
                error,
                "restored",
                serde_json::to_value(playback).unwrap_or(Value::Null),
            ),
            Err(failure) => with_detail(
                error,
                "restore_failed",
                serde_json::to_value(failure).unwrap_or(Value::Null),
            ),
        }
    }

    fn restore(&self, before: &Playback, track: &Track, context: Option<&str>) -> Result<Playback> {
        applescript::expect_ok(
            &self
                .script
                .run(&applescript::play_uri(&track.uri, context))?,
        )?;
        let uri = track.uri.clone();
        let (loaded, _) = self.wait_for(APPLESCRIPT_VERIFY, Instant::now(), &|pb, _| {
            pb.track.as_ref().is_some_and(|t| t.uri == uri)
        })?;
        if !loaded {
            return Err(Error::new(
                "verification_failed",
                format!("Spotify.app did not load {} again.", track.uri),
                format!("Start it yourself: `spotify play {}`.", track.uri),
            ));
        }
        if before.position_ms >= 1000 {
            applescript::expect_ok(&self.script.run(&applescript::seek(before.position_ms))?)?;
        }
        if before.state != PlayerState::Playing {
            applescript::expect_ok(&self.script.run(&applescript::pause())?)?;
        }
        self.status()
    }

    /// Pauses.
    ///
    /// # Errors
    /// Classified tool errors or `verification_failed`.
    pub fn pause(&self) -> Result<Outcome> {
        let before = self.status()?;
        if before.state == PlayerState::NotRunning {
            return Err(applescript::not_running());
        }
        self.pause_from(&before)
    }

    fn pause_from(&self, before: &Playback) -> Result<Outcome> {
        if before.state != PlayerState::Playing {
            return Ok(Outcome::unchanged("pause", before.clone()));
        }
        self.attempt(&Plan {
            action: "pause",
            request: "Pause",
            route: Route::PlayerFirst,
            patience: Patience::Idempotent,
            primary: Some(&|p: &SpotifyPlayer| {
                self.expect_play_state(p, "pause", true)?;
                p.run(&["playback", "pause"]).map(drop)
            }),
            script: Some(&|| applescript::expect_ok(&self.script.run(&applescript::pause())?)),
            check: &|pb, _| pb.state != PlayerState::Playing,
        })
    }

    /// Toggles play/pause: pauses when Spotify.app plays, else resumes.
    ///
    /// Decided from Spotify.app's state and sent as an explicit pause or play, never as
    /// spotify_player's `play-pause` (which flips its own, possibly out-of-date, idea of it).
    ///
    /// # Errors
    /// Classified tool errors, `nothing_to_resume` or `verification_failed`.
    pub fn toggle(&self) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let mut outcome = if before.state == PlayerState::Playing {
            self.pause_from(&before)?
        } else {
            self.resume(&before)?
        };
        outcome.action = "toggle".into();
        Ok(outcome)
    }

    /// Skips to the next track.
    ///
    /// # Errors
    /// Classified tool errors or `verification_failed`.
    pub fn next(&self) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let before_uri = before.track.as_ref().map(|t| t.uri.clone());
        let before_position = before.position_ms;
        self.attempt(&Plan {
            action: "next",
            request: "Next",
            route: Route::PlayerFirst,
            patience: Patience::Full,
            primary: Some(&|p: &SpotifyPlayer| {
                match self.view(p)? {
                    None => return Err(state_mismatch("next", "nothing is loaded", "loaded")),
                    Some(view) if view.is_on(&before) && view.disallows("skipping_next") => {
                        return Err(Error::new(
                            "not_allowed_in_context",
                            "The Web API does not allow skipping forward from this item.",
                            "AppleScript skips instead (Spotify.app may move on to autoplay).",
                        ));
                    }
                    Some(_) => {}
                }
                p.run(&["playback", "next"]).map(drop)
            }),
            script: Some(&|| applescript::expect_ok(&self.script.run(&applescript::next())?)),
            check: &|pb, _| {
                // A new item, or the same one restarted from the top (repeat-one); judged
                // against the state before the command, never against elapsed time.
                pb.track.as_ref().map(|t| t.uri.clone()) != before_uri
                    || (before_position >= 4000 && pb.position_ms < 3000)
            },
        })
    }

    /// Goes to the previous item, or restarts the current one when it has played 3 s or more
    /// (as Spotify.app's own button does) or when there is no item before it. `result` says
    /// which: `previous_item` or `restarted`.
    ///
    /// # Errors
    /// `nothing_playing`, classified tool errors or `verification_failed`.
    pub fn previous(&self) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let track = before.track.clone().ok_or_else(Error::nothing_playing)?;
        let view = match (&self.player, self.strategy) {
            (Ok(player), Strategy::Auto | Strategy::SpotifyPlayer) => {
                self.view(player).ok().flatten()
            }
            _ => None,
        };
        let first_item = view
            .as_ref()
            .is_some_and(|v| v.is_on(&before) && v.disallows("skipping_prev"));
        if before.position_ms >= RESTART_AFTER_MS || first_item {
            // The Web API's previous always goes back an item, so a restart is a seek to 0 on
            // every path.
            let mut outcome = self.seek_to(&before, &track, 0)?;
            outcome.action = "previous".into();
            outcome.result = Some("restarted".into());
            return Ok(outcome);
        }
        let before_uri = track.uri.clone();
        let before_position = before.position_ms;
        let has_view = view.is_some();
        let mut outcome = self.attempt(&Plan {
            action: "previous",
            request: "Previous",
            route: Route::PlayerFirst,
            patience: Patience::Full,
            primary: Some(&|p: &SpotifyPlayer| {
                if !has_view {
                    return Err(state_mismatch("previous", "nothing is loaded", "loaded"));
                }
                p.run(&["playback", "previous"]).map(drop)
            }),
            script: Some(&|| applescript::expect_ok(&self.script.run(&applescript::previous())?)),
            check: &|pb, _| match &pb.track {
                Some(t) if t.uri != before_uri => true,
                // Nothing before this item: Spotify.app went back to its start.
                Some(_) => pb.position_ms + 300 <= before_position,
                None => false,
            },
        })?;
        let moved = outcome
            .playback
            .track
            .as_ref()
            .is_some_and(|t| t.uri != track.uri);
        outcome.result = Some(if moved { "previous_item" } else { "restarted" }.into());
        Ok(outcome)
    }

    /// Seeks within the current item.
    ///
    /// # Errors
    /// `nothing_playing`, classified tool errors or `verification_failed`.
    pub fn seek(&self, target: SeekTarget) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let track = before.track.clone().ok_or_else(Error::nothing_playing)?;
        let target_ms = target.resolve_ms(before.position_ms, track.duration_ms);
        self.seek_to(&before, &track, target_ms)
    }

    /// Moves `track` (loaded in `before`) to `target_ms`.
    ///
    /// AppleScript goes first: it sets the position itself, locally. spotify_player can only
    /// seek by an offset, which it adds to a position it fetches from the Web API when the
    /// command runs (seconds later when rate limited), so it lands wherever that reading was;
    /// it is used when AppleScript is not (strategy `spotify_player`), with the offset worked out
    /// from Spotify.app's position at the moment it is sent.
    fn seek_to(&self, before: &Playback, track: &Track, target_ms: u64) -> Result<Outcome> {
        let observed_at = Instant::now();
        let playing = before.state == PlayerState::Playing;
        let before_position = before.position_ms;
        let offset = move || -> i64 {
            // Account for time elapsed since `before` was read while playing.
            let drift = if playing {
                i64::try_from(observed_at.elapsed().as_millis()).unwrap_or(0)
            } else {
                0
            };
            i64::try_from(target_ms).unwrap_or(0)
                - (i64::try_from(before_position).unwrap_or(0) + drift)
        };
        let uri = track.uri.clone();
        let check = move |pb: &Playback, since: Duration| {
            let expected = target_ms
                + if playing {
                    u64::try_from(since.as_millis()).unwrap_or(0)
                } else {
                    0
                };
            pb.position_ms.abs_diff(expected) < 1500
                && pb.track.as_ref().is_some_and(|t| t.uri == uri)
        };
        let script = applescript::seek(target_ms);
        self.attempt(&Plan {
            action: "seek",
            request: "Seek",
            route: Route::ScriptFirst,
            patience: Patience::Full,
            primary: Some(&|p: &SpotifyPlayer| {
                // An offset only lands right on the item Spotify.app plays.
                match self.view(p)? {
                    Some(view) if view.is_on(before) => {}
                    view => {
                        let believes = view
                            .and_then(|v| v.item_uri)
                            .unwrap_or_else(|| "nothing is loaded".into());
                        return Err(state_mismatch("seek", &believes, &track.uri));
                    }
                }
                let delta = offset().to_string();
                p.run(&["playback", "seek", "--", delta.as_str()]).map(drop)
            }),
            script: Some(&|| applescript::expect_ok(&self.script.run(&script)?)),
            check: &check,
        })
    }

    /// Sets the Spotify.app volume.
    ///
    /// # Errors
    /// Classified tool errors or `verification_failed`.
    pub fn volume(&self, target: VolumeTarget) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let current = i16::from(before.volume.unwrap_or(50));
        let value = match target {
            VolumeTarget::Absolute(v) => i16::from(v.min(100)),
            VolumeTarget::Delta(d) => (current + d).clamp(0, 100),
        };
        let value = u8::try_from(value).unwrap_or(100);
        let text = value.to_string();
        let script = applescript::volume(value);
        self.attempt(&Plan {
            action: "volume",
            request: "Volume",
            route: Route::PlayerFirst,
            patience: Patience::Idempotent,
            primary: Some(&|p: &SpotifyPlayer| {
                p.run(&["playback", "volume", text.as_str()]).map(drop)
            }),
            script: Some(&|| applescript::expect_ok(&self.script.run(&script)?)),
            check: &|pb, _| {
                pb.volume
                    .is_some_and(|read| applescript::volume_reached(value, read))
            },
        })
    }

    /// Turns shuffle on, off, or toggles it (`None`).
    ///
    /// # Errors
    /// Classified tool errors or `verification_failed`.
    pub fn shuffle(&self, on: Option<bool>) -> Result<Outcome> {
        let before = self.ensure_running()?;
        if before.shuffle_allowed == Some(false) {
            return Err(not_allowed("shuffle"));
        }
        let current = before.shuffling.unwrap_or(false);
        let target = on.unwrap_or(!current);
        if current == target {
            return Ok(Outcome::unchanged("shuffle", before));
        }
        let word = |on: bool| if on { "shuffle on" } else { "shuffle off" };
        let script = applescript::shuffle(target);
        self.attempt(&Plan {
            action: "shuffle",
            request: "Shuffle",
            route: Route::PlayerFirst,
            // Checked below to flip from the same state Spotify.app has, so it sets `target`.
            patience: Patience::Idempotent,
            primary: Some(&|p: &SpotifyPlayer| {
                // spotify_player's `shuffle` flips its own idea of the setting.
                match self.view(p)?.and_then(|v| v.shuffle) {
                    Some(believed) if believed == current => {}
                    Some(believed) => {
                        return Err(state_mismatch("shuffle", word(believed), word(current)));
                    }
                    None => {
                        return Err(state_mismatch(
                            "shuffle",
                            "nothing is loaded",
                            word(current),
                        ));
                    }
                }
                p.run(&["playback", "shuffle"]).map(drop)
            }),
            script: Some(&|| applescript::expect_ok(&self.script.run(&script)?)),
            check: &|pb, _| pb.shuffling == Some(target),
        })
    }

    /// Sets repeat. AppleScript can only switch context repeat on/off; `track` needs
    /// spotify_player.
    ///
    /// # Errors
    /// Classified tool errors, `unsupported` or `verification_failed`.
    pub fn repeat(&self, target: RepeatMode) -> Result<Outcome> {
        let before = self.ensure_running()?;
        if before.repeat_allowed == Some(false) {
            return Err(not_allowed("repeat"));
        }
        let use_player = matches!(self.strategy, Strategy::Auto | Strategy::SpotifyPlayer);
        let mut first_failure = None;
        // The mode spotify_player left the Web API in when it failed (steps it confirmed count).
        let mut left_at = None;
        if use_player {
            match &self.player {
                Ok(player) => match self.repeat_with_player(player, target, &before) {
                    Ok(playback) => {
                        return Ok(Outcome::new("repeat", Via::SpotifyPlayer, playback));
                    }
                    Err(error) => {
                        first_failure = Some(error);
                        left_at = self.view(player).ok().flatten().and_then(|v| v.repeat);
                    }
                },
                Err(error) => first_failure = Some(error.clone()),
            }
            if self.strategy == Strategy::SpotifyPlayer {
                return Err(first_failure
                    .unwrap_or_else(|| Error::internal("repeat failed without a reason")));
            }
        }
        if target == RepeatMode::Track {
            return Err(match first_failure {
                // Rate limited or refused for now: retrying is the way, not another tool.
                Some(error) if error.retryable => error,
                other => Error::unsupported(
                    "AppleScript cannot turn on repeat-one (track); only spotify_player can.",
                    "Make sure `spotify auth status` is signed in and strategy is auto or spotify_player.",
                )
                .with_details(json!({"spotify_player_error": other})),
            });
        }
        if left_at == Some(RepeatMode::Track) {
            // Repeat-one is its own flag in Spotify.app: `set repeating` neither clears it nor
            // reads it once the context flag is off, so AppleScript would report a change that
            // leaves the song repeating.
            return Err(repeat_one_stuck(target, first_failure));
        }
        let on = target == RepeatMode::Context;
        let script = applescript::repeat(on);
        applescript::expect_ok(&self.script.run(&script)?)?;
        let (ok, playback) = self.wait_for(APPLESCRIPT_VERIFY, Instant::now(), &|pb, _| {
            pb.repeating == Some(on)
        })?;
        if !ok {
            return Err(Error::new(
                "verification_failed",
                "Spotify.app did not reflect the repeat change.",
                "Retry.",
            )
            .retryable());
        }
        Ok(Outcome {
            fallback: first_failure.map(|reason| Fallback {
                from: Via::SpotifyPlayer,
                reason,
            }),
            ..Outcome::new("repeat", Via::Applescript, playback)
        })
    }

    /// spotify_player's `playback repeat` steps from the mode *it* believes (off → track →
    /// context) and sets the next one explicitly. So the number of steps comes from its view,
    /// each step is confirmed in that view before the next one is sent (a step sent earlier
    /// would start from the same mode), and the end result is checked in Spotify.app.
    fn repeat_with_player(
        &self,
        player: &SpotifyPlayer,
        target: RepeatMode,
        before: &Playback,
    ) -> Result<Playback> {
        let believed = |view: Option<PlayerView>| view.and_then(|v| v.repeat);
        let mut mode = believed(self.view(player)?).ok_or_else(|| {
            Error::new(
                "no_active_device",
                "spotify_player has no playback, so it cannot step the repeat mode.",
                "Play something first.",
            )
        })?;
        let mut steps = mode.cycles_to(target);
        if steps == 0 {
            // It already believes `target`, but that belief goes stale when repeat is changed in
            // Spotify.app or through AppleScript. Spotify.app shows context and track alike, so
            // when it agrees, the Web API itself says whether anything needs sending.
            if before.repeating == Some(target != RepeatMode::Off) {
                let fresh = player
                    .fresh_json(&["get", "key", "playback"], FRESH_CHECK_TIMEOUT)
                    .map(|value| {
                        RepeatMode::from_web(value.get("repeat_state").and_then(Value::as_str))
                    });
                match fresh {
                    Ok(Some(mode)) if mode == target => return Ok(before.clone()),
                    // Unconfirmed (the Web API is rate limiting or slow) while spotify_player and
                    // Spotify.app both say off: going round would start with repeat-one, which a
                    // refused later step would leave on and AppleScript cannot undo.
                    Err(_) if target == RepeatMode::Off => return Ok(before.clone()),
                    _ => {}
                }
            }
            // Once around the cycle ends on `target` whatever Spotify had.
            steps = 3;
        }
        for _ in 0..steps {
            let next = mode.next();
            let log = player.log_cursor();
            player.run(&["playback", "repeat"])?;
            let deadline = Instant::now() + REPEAT_STEP.min(self.verify_timeout.max(POLL));
            while believed(self.view(player)?) != Some(next) {
                if let Some(refusal) = crate::player::logged_failure(&log, "Repeat") {
                    return Err(refusal);
                }
                if Instant::now() > deadline {
                    return Err(Error::new(
                        "no_effect",
                        format!(
                            "spotify_player did not get the repeat mode from {} to {} (the Web API refused the change or is rate limiting).",
                            repeat_name(mode),
                            repeat_name(next)
                        ),
                        "Retry in a minute.",
                    )
                    .retryable());
                }
                sleep(VIEW_POLL);
            }
            mode = next;
        }
        // Spotify.app reports repeat on for both context and track.
        let on = target != RepeatMode::Off;
        let (ok, playback) = self.wait_for(self.verify_timeout, Instant::now(), &|pb, _| {
            pb.repeating == Some(on)
        })?;
        if ok {
            return Ok(playback);
        }
        Err(Error::new(
            "no_effect",
            format!(
                "The Web API accepted repeat {} but Spotify.app does not show it.",
                repeat_name(target)
            ),
            "spotify_player may be controlling a different device (`spotify devices`).",
        ))
    }

    /// Likes (saves) or unlikes the current track. spotify_player only.
    ///
    /// spotify_player's `like` has no id option: it saves or removes the track its instance
    /// believes is playing. So it runs only when that is provably the song Spotify.app plays, by
    /// the same id; otherwise nothing changes and the error is `track_mismatch`. A relinked song
    /// (the same recording under another id) is refused too, and not as retryable: spotify_player
    /// would save or remove its other id, not the one Spotify.app shows.
    ///
    /// # Errors
    /// `nothing_playing`, `unsupported` (not a song), `track_mismatch`, spotify_player errors.
    pub fn like(&self, like: bool) -> Result<Outcome> {
        let action = if like { "like" } else { "unlike" };
        let before = self.status()?;
        let track = before.track.clone().ok_or_else(Error::nothing_playing)?;
        if track.kind != "track" {
            return Err(Error::unsupported(
                format!(
                    "Only songs can be liked; Spotify.app is playing {} ({}).",
                    match track.kind.as_str() {
                        "episode" => "a podcast episode",
                        "ad" => "an ad",
                        "local" => "a local file",
                        _ => "something that is not a Spotify song",
                    },
                    track.uri
                ),
                "Play a song, then retry.",
            ));
        }
        let player = self.player.as_ref().map_err(Clone::clone)?;
        self.confirm_track(player, action, &before, &track)?;
        if like {
            player.run(&["like"])?;
        } else {
            player.run(&["like", "--unlike"])?;
        }
        // The instance's track only changes when it re-reads the Web API; if that happened
        // around the command, say so rather than claim which song changed.
        let after = self.view(player).ok().flatten().and_then(|v| v.item_uri);
        if after.as_deref() != Some(track.uri.as_str()) {
            let message = match &after {
                Some(other) => format!(
                    "spotify_player's current track changed while running `{action}` (to {other}), so it may have {action}d that song instead of {}.",
                    track.uri
                ),
                None => format!(
                    "spotify_player lost its current track while running `{action}`, so it may not have {action}d {}.",
                    track.uri
                ),
            };
            return Err(Error::new(
                "track_mismatch",
                message,
                "Check with `spotify library liked --json` and fix it with `spotify like`/`spotify unlike` while the song plays.",
            )
            .with_details(json!({"spotify_app": track.uri, "spotify_player": after})));
        }
        Ok(Outcome::new(action, Via::SpotifyPlayer, before))
    }

    /// Makes sure spotify_player's current track is `track` by id, letting it catch up once:
    /// the instance re-reads the Web API 1 s and 3 s after each command it runs, and setting the
    /// volume to its current level is a command that changes nothing audible. On the same song
    /// under another id (relinked), it refuses at once: catching up cannot change the id.
    fn confirm_track(
        &self,
        player: &SpotifyPlayer,
        action: &str,
        before: &Playback,
        track: &Track,
    ) -> Result<()> {
        // `Ok(true)`: on `track` by id. `Err`: on it under another id.
        let on_track = |view: Option<&PlayerView>| -> Result<bool> {
            match view.and_then(|v| v.item.same_song(track).map(|how| (v, how))) {
                Some((_, SameSong::Id)) => Ok(true),
                Some((view, how)) => Err(relinked_refusal(
                    action,
                    track,
                    view.item_uri.as_deref(),
                    how,
                )),
                None => Ok(false),
            }
        };
        let view = self.view(player)?;
        if on_track(view.as_ref())? {
            return Ok(());
        }
        let has_playback = view.is_some();
        let mut seen = view.and_then(|v| v.item_uri);
        // Why catching up did not happen, when known.
        let mut stuck: Option<Error> = None;
        let log = player.log_cursor();
        if has_playback && let Some(volume) = before.volume {
            match player.run(&["playback", "volume", &volume.to_string()]) {
                Ok(_) => {
                    let deadline = Instant::now() + self.verify_timeout + CATCH_UP_EXTRA;
                    while Instant::now() < deadline {
                        sleep(VIEW_POLL * 2);
                        let view = self.view(player)?;
                        if on_track(view.as_ref())? {
                            return Ok(());
                        }
                        seen = view.and_then(|v| v.item_uri);
                        // A refused command (rate limited) triggers no re-read.
                        stuck = crate::player::logged_failure(&log, "Volume");
                        if stuck.is_some() {
                            break;
                        }
                    }
                }
                Err(error) => stuck = Some(error),
            }
        }
        Err(Error::new(
            "track_mismatch",
            format!(
                "Refused to {action}: spotify_player's current track ({}) is not the song Spotify.app is playing ({}), and spotify_player can only {action} its own current track. Nothing was changed.",
                seen.as_deref().unwrap_or("none"),
                track.uri
            ),
            "Retry in a few seconds (spotify_player catches up after its next Web API read). If it keeps failing, let the song play for a moment or run `spotify daemon restart`, then retry.",
        )
        .retryable()
        .with_details(json!({"spotify_app": track.uri, "spotify_player": seen, "catch_up_failed": stuck})))
    }
}

/// `track_mismatch` for a song spotify_player knows under another id than Spotify.app (track
/// relinking). Not retryable: that stays so while the song plays.
fn relinked_refusal(action: &str, track: &Track, seen: Option<&str>, how: SameSong) -> Error {
    let seen = seen.unwrap_or("another id");
    let app = track.uri.as_str();
    let why = match how {
        SameSong::LinkedFrom => format!("the Web API names {app} as the song it links from"),
        _ => "same title, length and album".to_owned(),
    };
    Error::new(
        "track_mismatch",
        format!(
            "Refused to {action}: Spotify.app shows this song as {app} but Spotify plays it as {seen} (track relinking: the same recording from another release; {why}). spotify_player can only {action} the id it plays, so it would change {seen}, not {app}. Nothing was changed."
        ),
        format!(
            "Retrying will not help while this song plays (it is relinked). Use the heart in Spotify.app for \"{}\"; other songs are not affected.",
            track.name
        ),
    )
    .with_details(json!({
        "spotify_app": track.uri,
        "spotify_player": seen,
        "relinked": true,
        "matched_by": how.as_str(),
    }))
}

/// `verification_failed`: AppleScript sent `action` but Spotify.app never showed its effect.
fn never_reflected(action: &str, observed: &Playback) -> Error {
    Error::new(
        "verification_failed",
        format!("`{action}` was sent but Spotify.app never reflected it."),
        "Spotify may be showing an ad, a dialog, or be offline. Check Spotify.app, then retry. `spotify status` shows what it is doing now.",
    )
    .retryable()
    .with_details(json!({"observed": observed}))
}

/// `state_mismatch`: spotify_player's working state disagrees with Spotify.app, so the command it
/// would send does nothing or the wrong thing.
fn state_mismatch(action: &str, believes: &str, actual: &str) -> Error {
    Error::new(
        "state_mismatch",
        format!(
            "spotify_player's view of the player is out of date (it believes {believes}; Spotify.app is {actual}), so its `{action}` would do nothing or the wrong thing."
        ),
        "Under strategy auto AppleScript makes the change instead; nothing to do. Under strategy spotify_player, retry after the next track change or run `spotify daemon restart` (its spotify_player then starts from Spotify's current state).",
    )
    .retryable()
    .with_details(json!({"spotify_player": believes, "spotify_app": actual}))
}

/// spotify_player could not finish a repeat change and left repeat-one on, which AppleScript
/// cannot turn off: its failure, saying so.
fn repeat_one_stuck(target: RepeatMode, failure: Option<Error>) -> Error {
    let failure = failure.unwrap_or_else(|| Error::internal("repeat failed without a reason"));
    Error {
        code: failure.code.clone(),
        message: format!(
            "Repeat-one (track) is on and only spotify_player can change it, but its change to {} failed: {}",
            repeat_name(target),
            failure.message
        ),
        hint: format!(
            "Retry `spotify repeat {}` in a minute. AppleScript was not used: it cannot turn repeat-one off.",
            repeat_name(target)
        ),
        retryable: true,
        details: Some(json!({"repeat_state": "track", "spotify_player_error": failure})),
    }
}

fn repeat_name(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::Context => "context",
        RepeatMode::Track => "track",
    }
}

/// The Web API facts in a `get key playback` answer, marked with their `source`, and whether
/// their item is the one Spotify.app has loaded (by id, or relinked: then `relinked` is set).
fn web_facts(value: &Value, source: &str, playback: &Playback) -> Option<(WebPlayback, bool)> {
    let mut web = WebPlayback::from_player_json(value)?;
    web.source = Some(source.to_owned());
    let on_item = match &playback.track {
        None => web.item_uri.is_none(),
        Some(track) => match WebItem::from_player_json(value).same_song(track) {
            Some(SameSong::Id) => true,
            Some(SameSong::LinkedFrom | SameSong::Metadata) => {
                web.relinked = true;
                true
            }
            None => false,
        },
    };
    Some((web, on_item))
}

/// Whether the instance's cached Web API facts describe what Spotify.app shows now.
fn web_agrees(web: Option<&(WebPlayback, bool)>, playback: &Playback) -> bool {
    let Some((web, on_item)) = web else {
        return playback.track.is_none();
    };
    *on_item && repeat_agrees(web, playback) && shuffle_agrees(web, playback)
}

/// Spotify.app's `repeating` is on for both `context` and `track`.
fn repeat_agrees(web: &WebPlayback, playback: &Playback) -> bool {
    match (web.repeat_state.as_deref(), playback.repeating) {
        (Some(state), Some(on)) => (state != "off") == on,
        _ => true,
    }
}

fn shuffle_agrees(web: &WebPlayback, playback: &Playback) -> bool {
    match (web.shuffle_state, playback.shuffling) {
        (Some(web), Some(app)) => web == app,
        _ => true,
    }
}

/// Marks Web API facts out of date and drops repeat and shuffle that contradict Spotify.app.
fn mark_stale(web: &mut WebPlayback, playback: &Playback) {
    web.stale = true;
    if !repeat_agrees(web, playback) {
        web.repeat_state = None;
    }
    if !shuffle_agrees(web, playback) {
        web.shuffle_state = None;
    }
}

fn web_behind(web: &WebPlayback, playback: &Playback) -> Error {
    let current = playback.track.as_ref().map(|t| t.uri.as_str());
    let flag = |value: Option<bool>| value.map_or("unknown", |on| if on { "on" } else { "off" });
    let (what, hint) = if web.relinked || web.item_uri.as_deref() == current {
        // Same item: its repeat or shuffle is what disagrees.
        (
            format!(
                "The Spotify Web API's repeat ({}) or shuffle ({}) for {} contradicts Spotify.app (repeat {}, shuffle {})",
                web.repeat_state.as_deref().unwrap_or("unknown"),
                flag(web.shuffle_state),
                current.unwrap_or("nothing"),
                flag(playback.repeating),
                flag(playback.shuffling)
            ),
            "Retry in a few seconds; the Web API catches up with Spotify.app.",
        )
    } else {
        (
            format!(
                "The Spotify Web API's playback ({}) does not match Spotify.app ({})",
                web.item_uri.as_deref().unwrap_or("nothing"),
                current.unwrap_or("nothing")
            ),
            // A song it plays under another id (relinked) is recognised before this, so what is
            // left is almost always the Web API lagging behind a change.
            "Retry in a few seconds; the Web API usually catches up within seconds of a change in Spotify.app. If it keeps reporting another item, `web` describes that item, not the one Spotify.app plays.",
        )
    };
    Error::new(
        "web_state_stale",
        format!("{what}, so context, device and repeat mode may be out of date (`web.stale`)."),
        hint,
    )
    .retryable()
}

/// Whether a Web API track start's failure shows that its album does not place the track: the
/// start by uri (or by position) was refused (400, 404), or another track of the album played.
/// Its AppleScript fallback then plays the track alone, where `play track … in context` would
/// meet the same album.
fn album_refused(reason: &Error) -> bool {
    let detail = |key: &str| reason.details.as_ref().and_then(|d| d.get(key));
    match reason.code.as_str() {
        "no_effect" => detail("landed_elsewhere").and_then(Value::as_bool) == Some(true),
        "not_found" | "web_api_failed" => {
            detail("request").and_then(Value::as_str) == Some("PUT /me/player/play")
                && matches!(detail("status").and_then(Value::as_u64), Some(400 | 404))
        }
        _ => false,
    }
}

/// The note on a start that went to another device Spotify.app controls (a speaker, a phone).
fn remote_note(remote: bool, device: &crate::model::Device) -> Option<String> {
    remote.then(|| {
        let name = if device.name.is_empty() {
            "the active device"
        } else {
            device.name.as_str()
        };
        format!(
            "Started on {name}, the device Spotify.app on this Mac is playing on, so playback stays there."
        )
    })
}

/// A random number below `n` (0 when `n` is 0), from the standard library's randomly seeded
/// hasher.
fn random_below(n: u64) -> u32 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    u32::try_from(hasher.finish() % n.max(1)).unwrap_or(0)
}

/// The result of the path tried after the Web API: its outcome names the Web API's failure as the
/// `fallback` (unless it had one of its own), its error names it as `first_attempt`.
fn after_web(result: Result<Outcome>, web_failure: Option<Fallback>) -> Result<Outcome> {
    let Some(web_failure) = web_failure else {
        return result;
    };
    match result {
        Ok(mut outcome) => {
            outcome.fallback.get_or_insert(web_failure);
            Ok(outcome)
        }
        Err(error) => Err(with_detail(
            error,
            "first_attempt",
            serde_json::to_value(&web_failure).unwrap_or(Value::Null),
        )),
    }
}

/// Adds `key: value` to an error's details, keeping what is there.
fn with_detail(mut error: Error, key: &str, value: Value) -> Error {
    let mut map = match error.details.take() {
        Some(Value::Object(map)) => map,
        Some(other) => {
            let mut map = serde_json::Map::new();
            map.insert("details".into(), other);
            map
        }
        None => serde_json::Map::new(),
    };
    map.insert(key.to_owned(), value);
    error.details = Some(Value::Object(map));
    error
}

fn not_allowed(what: &str) -> Error {
    Error::new(
        "not_allowed_in_context",
        format!(
            "Spotify does not allow {what} for what is playing now (single-track releases, some podcasts and some radio contexts disable it)."
        ),
        "Play a playlist or album first (`spotify play spotify:playlist:<id>`), then retry. `spotify status --json` shows shuffle_allowed / repeat_allowed.",
    )
}

/// Launches Spotify.app hidden without stealing focus (`open -g -j -a Spotify`).
///
/// # Errors
/// `spotify_not_installed` when macOS cannot find it.
pub fn launch_spotify() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(Error::platform_unsupported("Launching Spotify.app"));
    }
    let status = std::process::Command::new("/usr/bin/open")
        .args(["-g", "-j", "-a", "Spotify"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|error| Error::internal(format!("/usr/bin/open failed to start: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(
            "spotify_not_installed",
            "macOS could not open Spotify.app.",
            "Install Spotify (https://www.spotify.com/download/mac/ or `brew install --cask spotify`), open it once and sign in.",
        ))
    }
}
