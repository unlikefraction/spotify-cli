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
//! Three actions go to AppleScript first, because spotify_player's way of doing them is worse on
//! the desktop app: seeking (spotify_player can only seek by an offset, which it adds to a
//! position it fetches from the Web API when the command runs, seconds later when the Web API is
//! rate limiting), starting a single track and starting Liked Songs (the Web API starts both as a
//! list of track ids, which leaves Spotify.app stopped with nothing loaded; AppleScript plays the
//! track, or the Liked Songs list itself). Those two starts never fall back to spotify_player:
//! it starts them only when AppleScript is not allowed (strategy `spotify_player`) or, for Liked
//! Songs, when the list's uri is unknown. When a start still leaves Spotify.app empty, what was
//! loaded before is put back.

use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::applescript::{self, Runner};
use crate::model::{
    Playback, PlayerState, SameSong, Track, WebItem, WebPlayback, playing_item_uri,
};
use crate::player::SpotifyPlayer;
use crate::timing::SeekTarget;
use crate::uri::{Kind, SpotifyUri};
use crate::{Error, Result};

/// Which tool performs playback commands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// spotify_player first, verified; AppleScript when it fails or has no effect (default).
    #[default]
    Auto,
    /// Only spotify_player (Spotify Web API). No fallback.
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
}

impl Outcome {
    fn unchanged(action: &str, playback: Playback) -> Self {
        Self {
            action: action.to_owned(),
            via: Via::Applescript,
            fallback: None,
            result: None,
            playback,
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

/// Stateless controller over an AppleScript runner and an optional spotify_player.
pub struct Controller<'a> {
    /// Runs AppleScript against Spotify.app.
    pub script: &'a dyn Runner,
    /// `Err` holds why spotify_player cannot be used (missing binary, not signed in).
    pub player: std::result::Result<&'a SpotifyPlayer, Error>,
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
            return Ok(Ok(Outcome {
                action: plan.action.to_owned(),
                via: Via::SpotifyPlayer,
                fallback: None,
                result: None,
                playback,
            }));
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
        let issued = Instant::now();
        script()?;
        let (ok, playback) = self.wait_for(APPLESCRIPT_VERIFY, issued, plan.check)?;
        if ok {
            return Ok(Ok(Outcome {
                action: plan.action.to_owned(),
                via: Via::Applescript,
                fallback: None,
                result: None,
                playback,
            }));
        }
        Ok(Err(never_reflected(plan.action, &playback)))
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
                let script = applescript::play_uri(&target_uri, context_uri.as_deref());
                let fallback = || applescript::expect_ok(&self.script.run(&script)?);
                match uri.kind {
                    Kind::Track | Kind::Episode => {
                        let expected = target_uri.clone();
                        let replay = before_uri.as_deref() == Some(target_uri.as_str());
                        // Replaying the item already loaded only counts once it restarted.
                        let check = move |pb: &Playback, since: Duration| {
                            pb.state == PlayerState::Playing
                                && pb.track.as_ref().is_some_and(|t| t.uri == expected)
                                && (!replay
                                    || pb.position_ms
                                        < 4000 + u64::try_from(since.as_millis()).unwrap_or(0))
                        };
                        let id = uri.id.clone();
                        let primary = move |p: &SpotifyPlayer| {
                            p.run(&["playback", "start", "track", "--id", &id])
                                .map(drop)
                        };
                        // AppleScript only, unless it is not allowed: the Web API starts a track
                        // as a list of ids, which leaves the desktop app stopped with nothing
                        // loaded, so it is no fallback for a start AppleScript could not verify
                        // (a slow first start after launching Spotify.app would be emptied).
                        let primary: Primary<'_> = if uri.kind == Kind::Track
                            && context.is_none()
                            && self.strategy == Strategy::SpotifyPlayer
                        {
                            Some(&primary)
                        } else {
                            None
                        };
                        self.attempt(&Plan {
                            action: "play",
                            request: "StartTrack",
                            route: Route::ScriptFirst,
                            patience: Patience::Full,
                            primary,
                            script: Some(&fallback),
                            check: &check,
                        })
                    }
                    Kind::Album | Kind::Playlist | Kind::Artist | Kind::Show => {
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
                        let (primary, route): (Primary<'_>, _) = if uri.kind == Kind::Show {
                            // spotify_player cannot start a show.
                            (None, Route::ScriptFirst)
                        } else {
                            (Some(&primary), Route::PlayerFirst)
                        };
                        self.attempt(&Plan {
                            action: "play",
                            request: "StartContext",
                            route,
                            patience: Patience::Full,
                            primary,
                            script: Some(&fallback),
                            check: &started,
                        })
                    }
                }
            }
            PlayTarget::Liked { limit, random } => self.play_liked(*limit, *random, &started),
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

    /// Liked Songs. AppleScript plays the Liked Songs list itself (`spotify:user:<id>:collection`,
    /// the id spotify_player is signed in as), so it keeps going and `next` works; spotify_player
    /// can only start a list of track ids, which the desktop app answers by stopping.
    ///
    /// Spotify keeps a shuffle setting per list and switches to Liked Songs' own when it starts.
    /// Shuffled, the list starts at the song its kept order starts with, the same one every time;
    /// in order, at its first song. So without `random` a kept shuffle is turned off and the list
    /// started again, and with `random` shuffle is switched off and on (Spotify then draws a new
    /// order) before one skip. Each step is checked in Spotify.app, and none is taken before
    /// Liked Songs plays: shuffle and the skip would otherwise land on what was playing.
    fn play_liked(
        &self,
        limit: u32,
        random: bool,
        check: &dyn Fn(&Playback, Duration) -> bool,
    ) -> Result<Outcome> {
        let collection = self
            .player
            .as_ref()
            .ok()
            .and_then(|p| p.username())
            .map(|user| format!("spotify:user:{user}:collection"));
        let play_list = || -> Result<()> {
            let Some(collection) = collection.as_deref() else {
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
            match now.shuffling {
                Some(_) if random => self.liked_at_random(&now),
                Some(true) => self.liked_in_order(collection, &now),
                _ => Ok(()),
            }
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
        match self.restore(before, track, context) {
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
                        return Ok(Outcome {
                            action: "repeat".into(),
                            via: Via::SpotifyPlayer,
                            fallback: None,
                            result: None,
                            playback,
                        });
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
            action: "repeat".into(),
            via: Via::Applescript,
            fallback: first_failure.map(|reason| Fallback {
                from: Via::SpotifyPlayer,
                reason,
            }),
            result: None,
            playback,
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
        Ok(Outcome {
            action: action.into(),
            via: Via::SpotifyPlayer,
            fallback: None,
            result: None,
            playback: before,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Spotify.app as the fake sees it.
    struct App {
        state: String,
        position: u64,
        uri: Option<String>,
        volume: u8,
        shuffling: bool,
        repeating: bool,
    }

    /// A fake Spotify.app: AppleScript commands mutate state; status reads it. With `shared`, it
    /// also applies what the fake spotify_player did (lines in `shared/app`).
    struct FakeApp {
        app: Mutex<App>,
        log: Mutex<Vec<String>>,
        shared: Option<PathBuf>,
        /// Takes commands without acting on them (a dialog or an offline app).
        frozen: bool,
        /// Commands (by prefix) it takes without acting on them.
        ignores: &'static [&'static str],
    }

    /// Spotify.app keeps 16-bit volume and reads it back rounded down.
    fn stored_volume(set: u8) -> u8 {
        if set.is_multiple_of(20) { set } else { set - 1 }
    }

    impl FakeApp {
        fn apply_player_effects(&self, app: &mut App) {
            let Some(dir) = &self.shared else { return };
            let Ok(effects) = std::fs::read_to_string(dir.join("app")) else {
                return;
            };
            let _ = std::fs::remove_file(dir.join("app"));
            let mut later = Vec::new();
            for line in effects.lines() {
                let (what, value) = line.split_once(' ').unwrap_or((line, ""));
                match what {
                    // Shows from the next read on.
                    "later" => later.push(value.to_owned()),
                    "uri" => {
                        app.uri = Some(value.to_owned());
                        app.state = "playing".into();
                        app.position = 0;
                    }
                    "state" => app.state = value.to_owned(),
                    "repeat" => app.repeating = value != "off",
                    "shuffle" => app.shuffling = value == "true",
                    "volume" => app.volume = value.parse().unwrap_or(app.volume),
                    "stopped" => {
                        app.state = "stopped".into();
                        app.uri = None;
                        app.position = 0;
                    }
                    _ => {}
                }
            }
            if !later.is_empty() {
                let _ = std::fs::write(dir.join("app"), later.join("\n") + "\n");
            }
        }

        fn commands(&self) -> Vec<String> {
            self.log.lock().expect("lock").clone()
        }
    }

    impl Runner for FakeApp {
        fn run(&self, source: &str) -> Result<String> {
            let mut app = self.app.lock().expect("lock");
            self.apply_player_effects(&mut app);
            let s = applescript::SEP;
            if source.contains("character id 31") {
                let head = format!(
                    "{}{s}{}{s}{}{s}{}",
                    app.state, app.volume, app.shuffling, app.repeating
                );
                return Ok(match &app.uri {
                    None => format!("no_track{s}{head}"),
                    Some(uri) => format!(
                        "ok{s}{head}{s}{}{s}{uri}{s}Song{s}Artist{s}Album{s}Artist{s}200000{s}1{s}1{s}10{s}{s}",
                        app.position
                    ),
                });
            }
            for line in source.lines().map(str::trim) {
                let known = [
                    "play",
                    "pause",
                    "playpause",
                    "next track",
                    "previous track",
                    "set sound volume",
                    "set player",
                    "set shuffling",
                    "set repeating",
                    "if sound volume < ",
                ];
                if !known.iter().any(|k| line.starts_with(k)) {
                    continue;
                }
                self.log.lock().expect("lock").push(line.to_owned());
                if self.frozen || self.ignores.iter().any(|i| line.starts_with(i)) {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("play track \"") {
                    let uri = rest.split('"').next().unwrap_or_default();
                    // Liked Songs starts at the song its kept shuffle order starts with (the same
                    // one every time), or in order at its first song.
                    let uri = match (uri.ends_with(":collection"), app.shuffling) {
                        (true, true) => "spotify:track:kept-shuffle-start",
                        (true, false) => "spotify:track:first-liked",
                        (false, _) => uri,
                    };
                    app.uri = Some(uri.to_owned());
                    app.state = "playing".into();
                    app.position = 0;
                } else if line == "pause" {
                    app.state = "paused".into();
                } else if line == "play" {
                    app.state = "playing".into();
                } else if line == "previous track" {
                    app.position = 0;
                } else if line == "next track" {
                    app.uri = Some("spotify:track:next".into());
                    app.position = 0;
                } else if let Some(seconds) = line.strip_prefix("set player position to ") {
                    let seconds: f64 = seconds.parse().unwrap_or(0.0);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    {
                        app.position = (seconds * 1000.0).round() as u64;
                    }
                } else if let Some(level) = line.strip_prefix("set sound volume to ") {
                    app.volume = stored_volume(level.parse().unwrap_or(0));
                } else if let Some(rest) = line.strip_prefix("if sound volume < ") {
                    let mut numbers = rest
                        .split(|c: char| !c.is_ascii_digit())
                        .filter(|n| !n.is_empty())
                        .map(|n| n.parse::<u8>().unwrap_or(0));
                    let (below, set) = (numbers.next(), numbers.next());
                    if let (Some(below), Some(set)) = (below, set)
                        && app.volume < below
                    {
                        app.volume = stored_volume(set);
                    }
                } else if let Some(on) = line.strip_prefix("set shuffling to ") {
                    app.shuffling = on == "true";
                } else if let Some(on) = line.strip_prefix("set repeating to ") {
                    app.repeating = on == "true";
                }
            }
            Ok("ok".into())
        }
    }

    fn app() -> FakeApp {
        FakeApp {
            app: Mutex::new(App {
                state: "paused".into(),
                position: 1000,
                uri: Some("spotify:track:a".into()),
                volume: 50,
                shuffling: false,
                repeating: false,
            }),
            log: Mutex::new(Vec::new()),
            shared: None,
            frozen: false,
            ignores: &[],
        }
    }

    fn controller<'a>(runner: &'a FakeApp) -> Controller<'a> {
        Controller {
            script: runner,
            player: Err(crate::player::missing()),
            strategy: Strategy::Auto,
            verify_timeout: Duration::from_millis(200),
            launch_spotify: false,
        }
    }

    /// A fake `spotify_player` (a shell script) whose running instance "believes" what the files
    /// in `dir/state` say. Commands are logged to `state/log`; effects on Spotify.app go to
    /// `state/app` for [`FakeApp`].
    struct FakePlayer {
        dir: tempfile::TempDir,
        player: SpotifyPlayer,
    }

    const FAKE_PLAYER: &str = r#"#!/bin/sh
d="$(dirname "$0")/state"
fresh=""
while [ "${1#-}" != "$1" ]; do [ "$1" = "-o" ] && fresh=1; shift 2; done
echo "$*" >> "$d/log"
if [ -n "$fresh" ] && [ -f "$d/fresh_fail" ]; then echo "http error: status code 429 Too Many Requests" >&2; exit 1; fi
case "$*" in
  "get key playback")
    if [ -n "$fresh" ] && [ -f "$d/fresh" ]; then cat "$d/fresh"; exit 0; fi
    if [ -z "$fresh" ] && [ -f "$d/view" ]; then cat "$d/view"; exit 0; fi
    item=$(cat "$d/item")
    if [ -z "$item" ]; then echo null; exit 0; fi
    rest=${item#spotify:}; kind=${rest%%:*}; id=${item##*:}
    printf '{"is_playing":%s,"repeat_state":"%s","shuffle_state":%s,"item":{"id":"%s","type":"%s"},"context":{"uri":"spotify:album:ctx","type":"album"},"actions":{"disallows":{%s}}}\n' \
      "$(cat "$d/playing")" "$(cat "$d/repeat")" "$(cat "$d/shuffle")" "$id" "$kind" "$(cat "$d/disallows" 2>/dev/null)" ;;
  "playback repeat")
    if [ -f "$d/refuse" ]; then
      printf 'x INFO socket_request{request=Playback(Repeat) dest_addr=127.0.0.1:1}: handled\nx WARN spotify_player::cli::client: Failed to handle a player request for playback CLI command: http error: status code 429 Too Many Requests\n' >> "$(dirname "$0")/cache/spotify-player-26-09-26-16-29.log"
      exit 0
    fi
    case $(cat "$d/repeat") in off) m=track;; track) m=context;; *) m=off;; esac
    echo "$m" > "$d/repeat"; echo "repeat $m" >> "$d/app" ;;
  "playback shuffle")
    if [ "$(cat "$d/shuffle")" = true ]; then m=false; else m=true; fi
    echo "$m" > "$d/shuffle"; echo "shuffle $m" >> "$d/app" ;;
  "playback volume "*)
    echo "volume ${3}" >> "$d/app"
    if [ -f "$d/refresh_item" ]; then cp "$d/refresh_item" "$d/item"; fi ;;
  "playback play")
    if [ "$(cat "$d/playing")" = false ]; then echo true > "$d/playing"; echo "state playing" >> "$d/app"; fi ;;
  "playback pause")
    if [ "$(cat "$d/playing")" = true ]; then echo false > "$d/playing"; echo "state paused" >> "$d/app"; fi ;;
  "playback start liked"*|"playback start track"*)
    echo stopped >> "$d/app" ;;
  "playback start context"*)
    # The start goes through; the shuffle call after it is rate limited.
    echo "later uri spotify:track:first" >> "$d/app"
    printf 'x INFO socket_request{request=Playback(StartContext { context_type: Album, id_or_name: Id("x"), shuffle: false }) dest_addr=127.0.0.1:1}: handled\nx WARN spotify_player::cli::client: Failed to handle a player request for playback CLI command: http error: status code 429 Too Many Requests\n' >> "$(dirname "$0")/cache/spotify-player-26-09-26-16-29.log" ;;
  "playback next")
    # Rate limited: the instance logs the refusal after the CLI exited 0.
    if [ -f "$d/refuse" ]; then
      printf 'x INFO socket_request{request=Playback(Next) dest_addr=127.0.0.1:1}: handled\nx WARN spotify_player::cli::client: Failed to handle a player request for playback CLI command: http error: status code 429 Too Many Requests\n' >> "$(dirname "$0")/cache/spotify-player-26-09-26-16-29.log"
    fi ;;
esac
exit 0
"#;

    impl FakePlayer {
        fn new(item: &str, playing: bool, repeat: &str) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = tempfile::tempdir().expect("tempdir");
            let binary = dir.path().join("spotify_player");
            std::fs::write(&binary, FAKE_PLAYER).expect("script");
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            let cache = dir.path().join("cache");
            std::fs::create_dir_all(dir.path().join("state")).expect("state");
            std::fs::create_dir_all(&cache).expect("cache");
            std::fs::write(cache.join("fake_token.json"), "{}").expect("token");
            let fake = Self {
                player: SpotifyPlayer {
                    binary,
                    config_dir: None,
                    cache_dir: Some(cache),
                    timeout: Duration::from_secs(10),
                },
                dir,
            };
            fake.set("item", item);
            fake.set("playing", if playing { "true" } else { "false" });
            fake.set("repeat", repeat);
            fake.set("shuffle", "false");
            fake.set("log", "");
            fake
        }

        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn set(&self, name: &str, value: &str) {
            std::fs::write(self.state().join(name), format!("{value}\n")).expect("write");
        }

        fn get(&self, name: &str) -> String {
            std::fs::read_to_string(self.state().join(name))
                .unwrap_or_default()
                .trim()
                .to_owned()
        }

        fn commands(&self) -> Vec<String> {
            self.get("log")
                .lines()
                .filter(|l| !l.is_empty() && *l != "get key playback")
                .map(str::to_owned)
                .collect()
        }

        fn app(&self, state: &str, uri: &str) -> FakeApp {
            let fake = app();
            {
                let mut app = fake.app.lock().expect("lock");
                app.state = state.into();
                app.uri = Some(uri.into());
            }
            FakeApp {
                shared: Some(self.state()),
                ..fake
            }
        }
    }

    fn with_player<'a>(runner: &'a FakeApp, player: &'a SpotifyPlayer) -> Controller<'a> {
        Controller {
            player: Ok(player),
            ..controller(runner)
        }
    }

    fn cache_dir(player: &SpotifyPlayer) -> &Path {
        player.cache_dir.as_deref().expect("cache dir")
    }

    #[test]
    fn falls_back_to_applescript_and_reports_why() {
        let fake = app();
        let outcome = controller(&fake)
            .play(&PlayTarget::Resume)
            .expect("resumes");
        assert_eq!(outcome.via, Via::Applescript);
        assert_eq!(
            outcome.fallback.as_ref().map(|f| f.reason.code.as_str()),
            Some("spotify_player_missing")
        );
        assert_eq!(outcome.playback.state, PlayerState::Playing);
    }

    #[test]
    fn plays_a_track_by_uri_through_applescript() {
        let fake = app();
        let uri = SpotifyUri::parse("spotify:track:0BxE4FqsDD1Ot4YuBXwAPp", None).expect("uri");
        let outcome = controller(&fake)
            .play(&PlayTarget::Uri {
                uri,
                context: None,
                shuffle: false,
            })
            .expect("plays");
        assert_eq!(
            outcome.playback.track.map(|t| t.id),
            Some("0BxE4FqsDD1Ot4YuBXwAPp".to_owned())
        );
        assert!(
            outcome.fallback.is_none(),
            "AppleScript goes first for tracks"
        );
    }

    #[test]
    fn applescript_only_strategy_skips_spotify_player() {
        let fake = app();
        let mut c = controller(&fake);
        c.strategy = Strategy::Applescript;
        let outcome = c.play(&PlayTarget::Resume).expect("resumes");
        assert!(outcome.fallback.is_none());
    }

    #[test]
    fn repeat_cycles_in_spotify_players_order() {
        // spotify_player 0.25: off → track → context → off.
        assert_eq!(RepeatMode::Off.cycles_to(RepeatMode::Track), 1);
        assert_eq!(RepeatMode::Off.cycles_to(RepeatMode::Context), 2);
        assert_eq!(RepeatMode::Track.cycles_to(RepeatMode::Off), 2);
        assert_eq!(RepeatMode::Context.cycles_to(RepeatMode::Off), 1);
        assert_eq!(RepeatMode::Context.cycles_to(RepeatMode::Context), 0);
    }

    #[test]
    fn like_refuses_when_spotify_player_is_on_another_track() {
        // The QA case: Spotify.app plays Dance Ka Bhoot, spotify_player still believes Deva Deva.
        let fake = FakePlayer::new("spotify:track:deva", true, "off");
        let app = fake.app("playing", "spotify:track:dance");
        let error = with_player(&app, &fake.player)
            .like(true)
            .expect_err("refuses");
        assert_eq!(error.code, "track_mismatch");
        assert!(error.retryable);
        assert!(
            !fake.commands().iter().any(|c| c.starts_with("like")),
            "{:?}",
            fake.commands()
        );
        let error = with_player(&app, &fake.player)
            .like(false)
            .expect_err("refuses unlike too");
        assert_eq!(error.code, "track_mismatch");
        assert!(!fake.commands().iter().any(|c| c.starts_with("like")));
    }

    #[test]
    fn like_waits_for_spotify_player_to_catch_up() {
        let fake = FakePlayer::new("spotify:track:deva", true, "off");
        fake.set("refresh_item", "spotify:track:dance");
        let app = fake.app("playing", "spotify:track:dance");
        let outcome = with_player(&app, &fake.player)
            .like(true)
            .expect("likes once it agrees");
        assert_eq!(outcome.action, "like");
        assert_eq!(
            fake.commands(),
            vec!["playback volume 50".to_owned(), "like".to_owned()],
            "a volume nudge at the current level, then like"
        );
        assert_eq!(app.app.lock().expect("lock").volume, 50);
    }

    #[test]
    fn like_acts_at_once_when_both_agree_and_refuses_episodes() {
        let fake = FakePlayer::new("spotify:track:dance", true, "off");
        let app = fake.app("playing", "spotify:track:dance");
        with_player(&app, &fake.player)
            .like(false)
            .expect("unlikes");
        assert_eq!(fake.commands(), vec!["like --unlike".to_owned()]);
        let episode = fake.app("playing", "spotify:episode:e1");
        let error = with_player(&episode, &fake.player)
            .like(true)
            .expect_err("not a song");
        assert_eq!(error.code, "unsupported");
    }

    /// `get key playback` for the song [`FakeApp`] shows (Song — Artist, Album, 200 000 ms)
    /// under the id `substitute`, with `extra` merged into the item.
    fn relinked_view(extra: &Value) -> String {
        let mut view = json!({"is_playing":true,"repeat_state":"off","shuffle_state":false,
            "context":{"uri":"spotify:album:substitute-album","type":"album"},
            "item":{"id":"substitute","type":"track","name":"Song","duration_ms":200_400,
                "album":{"id":"substitute-album","name":"Album"},"artists":[{"name":"Artist"}]}});
        if let (Some(item), Some(extra)) = (view["item"].as_object_mut(), extra.as_object()) {
            item.extend(extra.clone());
        }
        view.to_string()
    }

    #[test]
    fn like_refuses_a_relinked_song_at_once_and_says_why() {
        // The QA case: Spotify.app shows the liked id, the Web API plays a substitute's id.
        // spotify_player would like or unlike the substitute, so it must not run.
        let fake = FakePlayer::new("spotify:track:substitute", true, "off");
        fake.set("view", &relinked_view(&json!({})));
        let app = fake.app("playing", "spotify:track:library");
        let started = Instant::now();
        for like in [true, false] {
            let error = with_player(&app, &fake.player)
                .like(like)
                .expect_err("refuses");
            assert_eq!(error.code, "track_mismatch");
            assert!(!error.retryable, "retrying cannot change the id");
            assert!(error.message.contains("relinking"), "{}", error.message);
            assert!(error.hint.contains("relinked"), "{}", error.hint);
            assert!(!error.hint.contains("few seconds"), "{}", error.hint);
            let details = error.details.expect("details");
            assert_eq!(details["relinked"], true);
            assert_eq!(details["spotify_app"], "spotify:track:library");
            assert_eq!(details["spotify_player"], "spotify:track:substitute");
            assert_eq!(details["matched_by"], "title_length_album");
        }
        // No catch-up nudge and no like.
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
        assert!(started.elapsed() < Duration::from_secs(2));
        // Named by linked_from: the same refusal.
        fake.set(
            "view",
            &relinked_view(
                &json!({"name":"Other title","linked_from":{"id":"library","type":"track"}}),
            ),
        );
        let error = with_player(&app, &fake.player)
            .like(false)
            .expect_err("refuses");
        assert!(!error.retryable);
        assert_eq!(error.details.expect("details")["matched_by"], "linked_from");
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
    }

    #[test]
    fn like_still_waits_when_the_view_is_on_another_song() {
        // Same length and album, another title: not the same song, so the usual catch-up.
        let fake = FakePlayer::new("spotify:track:substitute", true, "off");
        fake.set("view", &relinked_view(&json!({"name":"Another Song"})));
        let app = fake.app("playing", "spotify:track:library");
        let error = with_player(&app, &fake.player)
            .like(true)
            .expect_err("refuses");
        assert_eq!(error.code, "track_mismatch");
        assert!(error.retryable);
        assert_eq!(fake.commands(), vec!["playback volume 50".to_owned()]);
        // The same recording on another release (the view is behind after a switch between a
        // single and its album): not relinked, so it waits for the view to catch up.
        let fake = FakePlayer::new("spotify:track:substitute", true, "off");
        fake.set(
            "view",
            &relinked_view(&json!({"album":{"id":"single","name":"Song (Single)"}})),
        );
        let app = fake.app("playing", "spotify:track:library");
        let error = with_player(&app, &fake.player)
            .like(true)
            .expect_err("refuses");
        assert!(error.retryable, "{error:?}");
        assert!(error.details.expect("details").get("relinked").is_none());
        assert_eq!(fake.commands(), vec!["playback volume 50".to_owned()]);
    }

    #[test]
    fn status_full_counts_a_relinked_song_as_current() {
        // From the instance's memory: no Web API read, not stale.
        let fake = FakePlayer::new("spotify:track:substitute", true, "off");
        fake.set("view", &relinked_view(&json!({})));
        let app = fake.app("playing", "spotify:track:library");
        let (playback, warnings) = with_player(&app, &fake.player)
            .status_full()
            .expect("status");
        assert!(warnings.is_empty(), "{warnings:?}");
        let web = playback.web.expect("web");
        assert_eq!(web.source.as_deref(), Some("spotify_player"));
        assert!(web.relinked && !web.stale);
        assert_eq!(web.item_uri.as_deref(), Some("spotify:track:substitute"));
        assert_eq!(
            web.context_uri.as_deref(),
            Some("spotify:album:substitute-album")
        );
        assert_eq!(fake.get("log").lines().count(), 1, "one read, from memory");
        // From a fresh read, when the memory is on an earlier song.
        std::fs::remove_file(fake.state().join("view")).expect("view");
        fake.set("item", "spotify:track:earlier");
        std::fs::write(fake.state().join("fresh"), relinked_view(&json!({}))).expect("fresh");
        let (playback, warnings) = with_player(&app, &fake.player)
            .status_full()
            .expect("status");
        assert!(warnings.is_empty(), "{warnings:?}");
        let web = playback.web.expect("web");
        assert_eq!(web.source.as_deref(), Some("web_api"));
        assert!(web.relinked && !web.stale);
        let json = serde_json::to_value(&web).expect("json");
        assert_eq!(json["relinked"], true);
    }

    #[test]
    fn a_relinked_view_counts_as_on_spotify_apps_song() {
        // Seek offsets, skip permissions and the context to restore apply to it.
        let fake = app();
        let playback = controller(&fake).status().expect("status");
        let relinked: Value = serde_json::from_str(&relinked_view(&json!({}))).expect("json");
        let view = PlayerView::from_json(&relinked).expect("view");
        assert!(view.is_on(&playback));
        let mut other = relinked;
        other["item"]["duration_ms"] = json!(215_000);
        assert!(
            !PlayerView::from_json(&other)
                .expect("view")
                .is_on(&playback)
        );
    }

    #[test]
    fn repeat_steps_from_spotify_players_mode_and_verifies() {
        let fake = FakePlayer::new("spotify:track:a", false, "context");
        let app = fake.app("paused", "spotify:track:a");
        app.app.lock().expect("lock").repeating = true;
        let outcome = with_player(&app, &fake.player)
            .repeat(RepeatMode::Off)
            .expect("repeat off");
        assert_eq!(outcome.via, Via::SpotifyPlayer);
        assert_eq!(outcome.playback.repeating, Some(false));
        assert_eq!(fake.commands(), vec!["playback repeat".to_owned()]);
        assert_eq!(fake.get("repeat"), "off");
    }

    #[test]
    fn repeat_reasserts_when_spotify_player_already_believes_the_target() {
        // spotify_player believes context, but repeat was switched off in Spotify.app: the old
        // code sent nothing and reported success.
        let fake = FakePlayer::new("spotify:track:a", false, "context");
        let app = fake.app("paused", "spotify:track:a");
        let outcome = with_player(&app, &fake.player)
            .repeat(RepeatMode::Context)
            .expect("repeat context");
        assert_eq!(outcome.playback.repeating, Some(true));
        assert_eq!(fake.commands().len(), 3, "once around the cycle");
        assert_eq!(fake.get("repeat"), "context");
    }

    #[test]
    fn repeat_already_set_sends_nothing_once_the_web_api_confirms_it() {
        let fake = FakePlayer::new("spotify:track:a", false, "context");
        std::fs::write(
            fake.state().join("fresh"),
            r#"{"is_playing":false,"repeat_state":"context","shuffle_state":false,"item":{"id":"a","type":"track"}}"#,
        )
        .expect("fresh");
        let app = fake.app("paused", "spotify:track:a");
        app.app.lock().expect("lock").repeating = true;
        let outcome = with_player(&app, &fake.player)
            .repeat(RepeatMode::Context)
            .expect("repeat context");
        assert_eq!(outcome.via, Via::SpotifyPlayer);
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
        // The Web API says repeat-one although Spotify.app shows repeat: go round to context.
        std::fs::write(
            fake.state().join("fresh"),
            r#"{"is_playing":false,"repeat_state":"track","shuffle_state":false,"item":{"id":"a","type":"track"}}"#,
        )
        .expect("fresh");
        with_player(&app, &fake.player)
            .repeat(RepeatMode::Context)
            .expect("repeat context");
        assert_eq!(fake.commands().len(), 3, "{:?}", fake.commands());
        assert_eq!(fake.get("repeat"), "context");
    }

    #[test]
    fn toggle_uses_applescript_when_spotify_player_believes_otherwise() {
        // Spotify.app plays; spotify_player believes it is paused, so its pause would do nothing
        // and its play-pause would send play.
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        let app = fake.app("playing", "spotify:track:a");
        let outcome = with_player(&app, &fake.player).toggle().expect("toggles");
        assert_eq!(outcome.action, "toggle");
        assert_eq!(outcome.playback.state, PlayerState::Paused);
        assert_eq!(outcome.via, Via::Applescript);
        assert_eq!(
            outcome.fallback.map(|f| f.reason.code),
            Some("state_mismatch".to_owned())
        );
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
        assert!(app.commands().contains(&"pause".to_owned()));
    }

    #[test]
    fn spotify_player_only_strategy_refuses_instead_of_sending_a_wrong_command() {
        let fake = FakePlayer::new("spotify:track:a", true, "off");
        let app = fake.app("paused", "spotify:track:a");
        let mut c = with_player(&app, &fake.player);
        c.strategy = Strategy::SpotifyPlayer;
        let error = c.toggle().expect_err("refuses");
        assert_eq!(error.code, "state_mismatch");
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
        assert_eq!(app.app.lock().expect("lock").state, "paused");
    }

    #[test]
    fn toggle_sends_an_explicit_command_when_both_agree() {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        let app = fake.app("paused", "spotify:track:a");
        let outcome = with_player(&app, &fake.player).toggle().expect("toggles");
        assert_eq!(outcome.via, Via::SpotifyPlayer);
        assert_eq!(outcome.playback.state, PlayerState::Playing);
        assert_eq!(fake.commands(), vec!["playback play".to_owned()]);
    }

    #[test]
    fn a_refusal_in_spotify_players_log_falls_back_at_once() {
        let fake = FakePlayer::new("spotify:track:a", true, "off");
        fake.set("refuse", "1");
        let app = fake.app("playing", "spotify:track:a");
        let mut c = with_player(&app, &fake.player);
        c.verify_timeout = Duration::from_secs(5);
        let started = Instant::now();
        let outcome = c.next().expect("next");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "did not wait out the verify timeout: {:?}",
            started.elapsed()
        );
        assert_eq!(outcome.via, Via::Applescript);
        assert_eq!(
            outcome.fallback.map(|f| f.reason.code),
            Some("rate_limited".to_owned())
        );
        assert_eq!(
            outcome.playback.track.map(|t| t.uri),
            Some("spotify:track:next".to_owned())
        );
    }

    #[test]
    fn previous_restarts_past_three_seconds_on_every_path() {
        let fake = app();
        fake.app.lock().expect("lock").position = 11_000;
        let outcome = controller(&fake).previous().expect("previous");
        assert_eq!(outcome.action, "previous");
        assert_eq!(outcome.result.as_deref(), Some("restarted"));
        assert_eq!(outcome.playback.position_ms, 0);
        assert!(
            fake.commands()
                .iter()
                .any(|c| c == "set player position to 0.000"),
            "{:?}",
            fake.commands()
        );
    }

    #[test]
    fn previous_on_the_first_item_counts_its_restart() {
        // The QA case: a single-track album at 1 s. Spotify.app restarts it; that is success.
        let fake = app();
        let outcome = controller(&fake).previous().expect("previous");
        assert_eq!(outcome.result.as_deref(), Some("restarted"));
        assert_eq!(outcome.playback.position_ms, 0);
        // With spotify_player reporting that nothing comes before it, no Web API call is made.
        let player = FakePlayer::new("spotify:track:a", false, "off");
        player.set("disallows", "\"skipping_prev\":true");
        let app = player.app("paused", "spotify:track:a");
        app.app.lock().expect("lock").position = 1000;
        let outcome = with_player(&app, &player.player)
            .previous()
            .expect("previous");
        assert_eq!(outcome.result.as_deref(), Some("restarted"));
        assert!(player.commands().is_empty(), "{:?}", player.commands());
    }

    #[test]
    fn seek_goes_to_applescript_first() {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        let app = fake.app("paused", "spotify:track:a");
        let outcome = with_player(&app, &fake.player)
            .seek(SeekTarget::parse("0:30").expect("target"))
            .expect("seeks");
        assert_eq!(outcome.via, Via::Applescript);
        assert!(outcome.fallback.is_none());
        assert_eq!(outcome.playback.position_ms, 30_000);
        assert!(fake.commands().is_empty(), "no relative seek sent");
    }

    #[test]
    fn applescript_volume_lands_on_the_requested_level() {
        let fake = app();
        let mut c = controller(&fake);
        c.strategy = Strategy::Applescript;
        for level in [64, 66, 1, 60, 100, 0] {
            let outcome = c.volume(VolumeTarget::Absolute(level)).expect("volume");
            assert_eq!(outcome.playback.volume, Some(level), "requested {level}");
        }
        let outcome = c.volume(VolumeTarget::Absolute(59)).expect("volume");
        assert_eq!(outcome.playback.volume, Some(60), "59 is unreachable");
    }

    #[test]
    fn liked_songs_play_as_a_list_through_applescript() {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        std::fs::write(
            cache_dir(&fake.player).join("credentials.json"),
            r#"{"username":"user1","auth_type":1,"auth_data":"c2VjcmV0"}"#,
        )
        .expect("credentials");
        let app = fake.app("paused", "spotify:track:a");
        let outcome = with_player(&app, &fake.player)
            .play(&PlayTarget::Liked {
                limit: 50,
                random: false,
            })
            .expect("plays liked");
        assert_eq!(outcome.via, Via::Applescript);
        assert_eq!(
            outcome.playback.track.map(|t| t.uri),
            Some("spotify:track:first-liked".to_owned())
        );
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
        // Liked Songs' shuffle is off: nothing else is sent.
        assert_eq!(
            app.commands(),
            vec!["play track \"spotify:user:user1:collection\"".to_owned()]
        );
    }

    /// A fake spotify_player signed in as `user1`, and Spotify.app paused on another song with
    /// Liked Songs' kept shuffle on or off.
    fn liked_setup(shuffled: bool) -> (FakePlayer, FakeApp) {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        std::fs::write(
            cache_dir(&fake.player).join("credentials.json"),
            r#"{"username":"user1"}"#,
        )
        .expect("credentials");
        let app = fake.app("paused", "spotify:track:a");
        app.app.lock().expect("lock").shuffling = shuffled;
        (fake, app)
    }

    fn liked(random: bool) -> PlayTarget {
        PlayTarget::Liked { limit: 50, random }
    }

    #[test]
    fn plain_liked_songs_turn_a_kept_shuffle_off_and_start_in_order() {
        // The QA case: after an earlier --random, Liked Songs kept its shuffle, and a plain
        // --liked started at the same shuffled song every time.
        let (fake, app) = liked_setup(true);
        let outcome = with_player(&app, &fake.player)
            .play(&liked(false))
            .expect("plays liked");
        assert_eq!(outcome.via, Via::Applescript);
        assert_eq!(outcome.playback.shuffling, Some(false));
        assert_eq!(
            outcome.playback.track.map(|t| t.uri),
            Some("spotify:track:first-liked".to_owned())
        );
        assert_eq!(
            app.commands(),
            vec![
                "play track \"spotify:user:user1:collection\"".to_owned(),
                "set shuffling to false".to_owned(),
                "play track \"spotify:user:user1:collection\"".to_owned(),
            ]
        );
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
    }

    #[test]
    fn random_liked_songs_draw_a_new_order_and_skip_whatever_shuffle_was_kept() {
        for kept in [false, true] {
            let (fake, app) = liked_setup(kept);
            let outcome = with_player(&app, &fake.player)
                .play(&liked(true))
                .expect("plays liked");
            assert_eq!(outcome.playback.shuffling, Some(true), "kept {kept}");
            // Neither the in-order first song nor the kept shuffle's start.
            assert_eq!(
                outcome.playback.track.map(|t| t.uri),
                Some("spotify:track:next".to_owned()),
                "kept {kept}"
            );
            assert_eq!(
                app.commands(),
                vec![
                    "play track \"spotify:user:user1:collection\"".to_owned(),
                    "set shuffling to false".to_owned(),
                    "set shuffling to true".to_owned(),
                    "next track".to_owned(),
                ],
                "kept {kept}"
            );
            assert!(fake.commands().is_empty(), "{:?}", fake.commands());
        }
    }

    #[test]
    fn liked_songs_steps_spotify_app_ignored_are_reported_not_claimed() {
        // The skip never shows: Liked Songs plays, but not from a random song.
        let (fake, app) = liked_setup(true);
        let app = FakeApp {
            ignores: &["next track"],
            ..app
        };
        let error = with_player(&app, &fake.player)
            .play(&liked(true))
            .expect_err("the skip did not show");
        assert_eq!(error.code, "verification_failed");
        assert!(error.message.contains("random song"), "{}", error.message);
        assert!(error.retryable);
        // Shuffle cannot be turned off: plain --liked says so instead of playing shuffled.
        let (fake, app) = liked_setup(true);
        let app = FakeApp {
            ignores: &["set shuffling"],
            ..app
        };
        let error = with_player(&app, &fake.player)
            .play(&liked(false))
            .expect_err("shuffle stayed on");
        assert_eq!(error.code, "verification_failed");
        assert!(error.message.contains("shuffle off"), "{}", error.message);
        assert_eq!(
            app.commands(),
            vec![
                "play track \"spotify:user:user1:collection\"".to_owned(),
                "set shuffling to false".to_owned(),
            ]
        );
        // Spotify.app keeps playing Liked Songs either way; nothing went to spotify_player.
        assert_eq!(
            app.app.lock().expect("lock").uri.as_deref(),
            Some("spotify:track:kept-shuffle-start")
        );
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
    }

    #[test]
    fn a_start_that_empties_spotify_is_undone() {
        // spotify_player's `start liked` leaves the desktop app stopped with nothing loaded.
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        let app = fake.app("paused", "spotify:track:a");
        app.app.lock().expect("lock").position = 191_000;
        let mut c = with_player(&app, &fake.player);
        c.strategy = Strategy::SpotifyPlayer;
        let error = c
            .play(&PlayTarget::Liked {
                limit: 50,
                random: false,
            })
            .expect_err("fails");
        assert_eq!(error.code, "no_effect");
        let restored = error
            .details
            .as_ref()
            .and_then(|d| d.get("restored"))
            .expect("restored");
        assert_eq!(restored["track"]["uri"], "spotify:track:a");
        assert_eq!(restored["state"], "paused");
        assert_eq!(restored["position_ms"], 191_000);
        assert!(
            app.commands().contains(
                &"play track \"spotify:track:a\" in context \"spotify:album:ctx\"".to_owned()
            ),
            "{:?}",
            app.commands()
        );
    }

    #[test]
    fn status_full_prefers_a_fresh_read_over_stale_memory() {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        let app = fake.app("paused", "spotify:track:a");
        let (playback, warnings) = with_player(&app, &fake.player)
            .status_full()
            .expect("status");
        let web = playback.web.expect("web");
        assert_eq!(web.source.as_deref(), Some("spotify_player"));
        assert!(!web.stale && warnings.is_empty());
        // Spotify.app moved on; the instance still describes the old track.
        let app = fake.app("playing", "spotify:track:b");
        std::fs::write(
            fake.state().join("fresh"),
            r#"{"is_playing":true,"repeat_state":"off","shuffle_state":false,"item":{"id":"b","type":"track"},"context":{"uri":"spotify:album:new","type":"album"}}"#,
        )
        .expect("fresh");
        let (playback, warnings) = with_player(&app, &fake.player)
            .status_full()
            .expect("status");
        let web = playback.web.expect("web");
        assert_eq!(web.source.as_deref(), Some("web_api"));
        assert_eq!(web.context_uri.as_deref(), Some("spotify:album:new"));
        assert!(!web.stale && warnings.is_empty(), "{warnings:?}");
        // Even the Web API lags: marked stale, contradicting repeat left out.
        std::fs::write(
            fake.state().join("fresh"),
            r#"{"is_playing":true,"repeat_state":"context","shuffle_state":false,"item":{"id":"a","type":"track"},"context":{"uri":"spotify:album:old","type":"album"}}"#,
        )
        .expect("fresh");
        let (playback, warnings) = with_player(&app, &fake.player)
            .status_full()
            .expect("status");
        let web = playback.web.expect("web");
        assert!(web.stale);
        assert_eq!(web.repeat_state, None);
        assert_eq!(warnings[0].code, "web_state_stale");
    }

    #[test]
    fn repeat_off_never_reports_success_while_repeat_one_stays_on() {
        // spotify_player holds repeat-one and the Web API refuses its change (429). AppleScript's
        // `set repeating to false` would read back false and leave the song repeating.
        let fake = FakePlayer::new("spotify:track:a", false, "track");
        fake.set("refuse", "1");
        let app = fake.app("paused", "spotify:track:a");
        app.app.lock().expect("lock").repeating = true;
        let error = with_player(&app, &fake.player)
            .repeat(RepeatMode::Off)
            .expect_err("refuses");
        assert_eq!(error.code, "rate_limited");
        assert!(error.retryable);
        assert!(error.message.contains("Repeat-one"), "{}", error.message);
        assert_eq!(
            error.details.as_ref().map(|d| d["repeat_state"].clone()),
            Some(json!("track"))
        );
        assert!(
            !app.commands()
                .iter()
                .any(|c| c.starts_with("set repeating")),
            "{:?}",
            app.commands()
        );
        assert_eq!(fake.get("repeat"), "track");
        // Without repeat-one in the way, AppleScript still covers a refusal.
        fake.set("repeat", "context");
        let outcome = with_player(&app, &fake.player)
            .repeat(RepeatMode::Off)
            .expect("falls back");
        assert_eq!(outcome.via, Via::Applescript);
        assert_eq!(
            outcome.fallback.map(|f| f.reason.code),
            Some("rate_limited".to_owned())
        );
        assert_eq!(outcome.playback.repeating, Some(false));
    }

    #[test]
    fn repeat_off_does_not_go_round_through_repeat_one_when_unconfirmed() {
        // Both say off and the Web API cannot be asked: nothing is sent, so a refused later step
        // cannot leave repeat-one on.
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        fake.set("fresh_fail", "1");
        let app = fake.app("paused", "spotify:track:a");
        let outcome = with_player(&app, &fake.player)
            .repeat(RepeatMode::Off)
            .expect("repeat off");
        assert_eq!(outcome.playback.repeating, Some(false));
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
    }

    #[test]
    fn liked_songs_never_fall_back_to_a_start_that_empties_spotify() {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        std::fs::write(
            cache_dir(&fake.player).join("credentials.json"),
            r#"{"username":"user1"}"#,
        )
        .expect("credentials");
        let app = FakeApp {
            frozen: true,
            ..fake.app("paused", "spotify:track:a")
        };
        let started = Instant::now();
        let error = with_player(&app, &fake.player)
            .play(&PlayTarget::Liked {
                limit: 50,
                random: true,
            })
            .expect_err("Spotify.app did not start it");
        assert_eq!(error.code, "verification_failed");
        // Said after one wait, not two.
        assert!(started.elapsed() < Duration::from_millis(4000));
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
        // Nor shuffle and skip what was playing while Liked Songs never started.
        assert_eq!(
            app.commands(),
            vec!["play track \"spotify:user:user1:collection\"".to_owned()]
        );
        assert_eq!(
            app.app.lock().expect("lock").uri.as_deref(),
            Some("spotify:track:a")
        );
    }

    /// Spotify.app that takes commands at once but shows their effect only `delay` after the
    /// first one: status reads until then return what it showed before it.
    struct LateApp {
        inner: FakeApp,
        delay: Duration,
        before: Mutex<Option<String>>,
        commanded: Mutex<Option<Instant>>,
    }

    impl Runner for LateApp {
        fn run(&self, source: &str) -> Result<String> {
            if !source.contains("character id 31") {
                self.commanded
                    .lock()
                    .expect("lock")
                    .get_or_insert_with(Instant::now);
                return self.inner.run(source);
            }
            let shows = self
                .commanded
                .lock()
                .expect("lock")
                .is_some_and(|at| at.elapsed() >= self.delay);
            let mut before = self.before.lock().expect("lock");
            if shows {
                return self.inner.run(source);
            }
            if before.is_none() {
                *before = Some(self.inner.run(source)?);
            }
            Ok(before.clone().unwrap_or_default())
        }
    }

    #[test]
    fn a_liked_songs_start_that_shows_late_is_not_claimed_without_its_shuffle_step() {
        // Liked Songs keeps a shuffle, and the start shows only after the start check gave up
        // (2.5 s). Counting it then would report plain --liked as done while it plays shuffled.
        let (fake, app) = liked_setup(true);
        let late = LateApp {
            inner: app,
            delay: Duration::from_millis(3200),
            before: Mutex::new(None),
            commanded: Mutex::new(None),
        };
        let c = Controller {
            script: &late,
            ..with_player(&late.inner, &fake.player)
        };
        let error = c.play(&liked(false)).expect_err("not seen in time");
        assert_eq!(error.code, "verification_failed");
        assert!(error.retryable);
        let commanded = late.commanded.lock().expect("lock").expect("sent");
        assert!(commanded.elapsed() < late.delay, "said after one wait");
        // Nothing was sent after the start it could not see.
        assert_eq!(
            late.inner.commands(),
            vec!["play track \"spotify:user:user1:collection\"".to_owned()]
        );
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
    }

    #[test]
    fn liked_songs_count_a_restart_of_the_song_already_loaded() {
        // Liked Songs starts with the song that is loaded (paused at 3 s): it restarted.
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        std::fs::write(
            cache_dir(&fake.player).join("credentials.json"),
            r#"{"username":"user1"}"#,
        )
        .expect("credentials");
        let app = fake.app("paused", "spotify:track:first-liked");
        app.app.lock().expect("lock").position = 3000;
        let outcome = with_player(&app, &fake.player)
            .play(&PlayTarget::Liked {
                limit: 50,
                random: false,
            })
            .expect("plays liked");
        assert_eq!(outcome.via, Via::Applescript);
        assert_eq!(outcome.playback.position_ms, 0);
    }

    #[test]
    fn a_start_whose_shuffle_call_was_refused_is_not_started_again() {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        let app = fake.app("paused", "spotify:track:a");
        let mut c = with_player(&app, &fake.player);
        c.verify_timeout = Duration::from_secs(5);
        let uri = SpotifyUri::parse("spotify:album:4kIPlpwEZBK9JaI9pZHe79", None).expect("uri");
        let outcome = c
            .play(&PlayTarget::Uri {
                uri,
                context: None,
                shuffle: false,
            })
            .expect("plays");
        assert_eq!(outcome.via, Via::SpotifyPlayer);
        assert!(outcome.fallback.is_none());
        assert!(
            !app.commands().iter().any(|c| c.starts_with("play track")),
            "{:?}",
            app.commands()
        );
    }

    #[test]
    fn a_track_start_spotify_app_did_not_show_is_not_retried_through_the_web_api() {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        let app = FakeApp {
            frozen: true,
            ..fake.app("paused", "spotify:track:a")
        };
        let uri = SpotifyUri::parse("spotify:track:0BxE4FqsDD1Ot4YuBXwAPp", None).expect("uri");
        let error = with_player(&app, &fake.player)
            .play(&PlayTarget::Uri {
                uri,
                context: None,
                shuffle: false,
            })
            .expect_err("Spotify.app did not start it");
        assert_eq!(error.code, "verification_failed");
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
    }

    #[test]
    fn spotify_player_only_strategy_names_what_spotify_player_cannot_start() {
        let fake = FakePlayer::new("spotify:track:a", false, "off");
        let app = fake.app("paused", "spotify:track:a");
        let mut c = with_player(&app, &fake.player);
        c.strategy = Strategy::SpotifyPlayer;
        let uri = SpotifyUri::parse("spotify:episode:4IzpgR6RCEkRqMHbJF38Wp", None).expect("uri");
        let error = c
            .play(&PlayTarget::Uri {
                uri,
                context: None,
                shuffle: false,
            })
            .expect_err("spotify_player cannot start an episode");
        assert_eq!(error.code, "unsupported");
        assert!(
            error.message.contains("spotify_player has no command"),
            "{}",
            error.message
        );
    }
}
