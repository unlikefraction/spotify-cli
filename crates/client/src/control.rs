//! Playback control with verification and fallback.
//!
//! The rule (from the product spec): try `spotify_player` first; if it fails, use AppleScript.
//! "Fails" includes "exited 0 but nothing happened": `spotify_player` hands commands to its
//! running instance asynchronously, and some commands (starting a single track by id on the
//! desktop app) report success while leaving Spotify.app with no track. So every action is
//! verified against Spotify.app's own state before it counts as done, and every result says
//! which path worked (`via`) and, when it fell back, why.

use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::applescript::{self, Runner};
use crate::model::{Playback, PlayerState, WebPlayback};
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
    /// Spotify.app right after the change.
    pub playback: Playback,
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
        /// Maximum tracks to enqueue.
        limit: u32,
        /// Random order.
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

    fn cycles_to(self, target: Self) -> usize {
        // spotify_player's `repeat` cycles off → context → track → off.
        let index = |mode: Self| match mode {
            Self::Off => 0,
            Self::Context => 1,
            Self::Track => 2,
        };
        (index(target) + 3 - index(self)) % 3
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

/// The spotify_player half of an action (absent when spotify_player cannot do it).
type Primary<'a> = Option<&'a dyn Fn(&SpotifyPlayer) -> Result<()>>;
const APPLESCRIPT_VERIFY: Duration = Duration::from_millis(2500);

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
    /// # Errors
    /// AppleScript failures.
    pub fn status_full(&self) -> Result<(Playback, Vec<Error>)> {
        let mut playback = self.status()?;
        let mut warnings = Vec::new();
        match &self.player {
            Ok(player) => match player.json(&["get", "key", "playback"]) {
                Ok(value) => playback.web = WebPlayback::from_player_json(&value),
                Err(error) => warnings.push(error),
            },
            Err(error) => warnings.push(error.clone()),
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

    /// Runs `primary` (spotify_player) and `fallback` (AppleScript) per the strategy, verifying
    /// each with `check`.
    fn attempt(
        &self,
        action: &str,
        primary: Primary<'_>,
        fallback: Option<&dyn Fn() -> Result<()>>,
        check: &dyn Fn(&Playback, Duration) -> bool,
    ) -> Result<Outcome> {
        let mut first_failure: Option<Fallback> = None;
        let try_player = matches!(self.strategy, Strategy::Auto | Strategy::SpotifyPlayer);
        let try_script = matches!(self.strategy, Strategy::Auto | Strategy::Applescript);
        if try_player {
            let reason = match (&self.player, primary) {
                (Ok(player), Some(primary)) => match (Instant::now(), primary(player)) {
                    (issued, Ok(())) => {
                        let (mut ok, mut playback) =
                            self.wait_for(self.verify_timeout, issued, check)?;
                        if !ok {
                            // A late effect must not be applied twice (a second skip, a toggle
                            // that cancels out): one more short look before falling back.
                            sleep(LATE_GRACE);
                            playback = self.status()?;
                            ok = check(&playback, issued.elapsed());
                        }
                        if ok {
                            return Ok(Outcome {
                                action: action.to_owned(),
                                via: Via::SpotifyPlayer,
                                fallback: None,
                                playback,
                            });
                        }
                        Error::new(
                            "no_effect",
                            format!(
                                "spotify_player accepted `{action}` but Spotify.app did not change within {} ms.",
                                self.verify_timeout.as_millis()
                            ),
                            "spotify_player may be controlling a different device, or the Web API ignored the command.",
                        )
                    }
                    (_, Err(error)) => error,
                },
                (Err(error), _) => error.clone(),
                (Ok(_), None) => Error::unsupported(
                    format!("spotify_player has no command for `{action}`."),
                    "AppleScript handles it.",
                ),
            };
            if !try_script || fallback.is_none() {
                return Err(reason);
            }
            first_failure = Some(Fallback {
                from: Via::SpotifyPlayer,
                reason,
            });
        }
        let Some(fallback) = fallback.filter(|_| try_script) else {
            return Err(Error::unsupported(
                format!(
                    "AppleScript cannot `{action}`; it needs spotify_player (Spotify Web API)."
                ),
                "Use `spotify config set '{\"strategy\": \"auto\"}'` and make sure `spotify auth status` reports signed in.",
            ));
        };
        let issued = Instant::now();
        fallback()?;
        let (ok, playback) = self.wait_for(APPLESCRIPT_VERIFY, issued, check)?;
        if ok {
            return Ok(Outcome {
                action: action.to_owned(),
                via: Via::Applescript,
                fallback: first_failure,
                playback,
            });
        }
        Err(Error::new(
            "verification_failed",
            format!("`{action}` was sent but Spotify.app never reflected it."),
            "Spotify may be showing an ad, a dialog, or be offline. Check Spotify.app, then retry. `spotify status` shows what it is doing now.",
        )
        .retryable()
        .with_details(serde_json::json!({
            "first_attempt": first_failure,
            "observed": playback,
        })))
    }

    /// Starts or resumes playback.
    ///
    /// # Errors
    /// Classified tool errors, `verification_failed`, or `unsupported`.
    pub fn play(&self, target: &PlayTarget) -> Result<Outcome> {
        let before = self.ensure_running()?;
        match target {
            PlayTarget::Resume => {
                if before.track.is_none() {
                    return Err(Error::new(
                        "nothing_to_resume",
                        "Spotify has nothing loaded, so there is nothing to resume.",
                        "Start something: `spotify play --search 'query'`, `spotify play <uri>` or `spotify play --liked`.",
                    ));
                }
                self.attempt(
                    "play",
                    Some(&|p: &SpotifyPlayer| p.run(&["playback", "play"]).map(drop)),
                    Some(&|| applescript::expect_ok(&self.script.run(&applescript::play())?)),
                    &|pb, _| pb.state == PlayerState::Playing,
                )
            }
            PlayTarget::Uri {
                uri,
                context,
                shuffle,
            } => {
                let target_uri = uri.uri();
                let before_uri = before.track.as_ref().map(|t| t.uri.clone());
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
                        let primary: Primary<'_> = if uri.kind == Kind::Track && context.is_none() {
                            Some(&primary)
                        } else {
                            None
                        };
                        self.attempt("play", primary, Some(&fallback), &check)
                    }
                    Kind::Album | Kind::Playlist | Kind::Artist | Kind::Show => {
                        let before_position = before.position_ms;
                        let check = move |pb: &Playback, since: Duration| {
                            // A different item, or the same one restarted from the top.
                            pb.state == PlayerState::Playing
                                && (pb.track.as_ref().map(|t| t.uri.clone()) != before_uri
                                    || (before_position >= 5000
                                        && pb.position_ms
                                            < 4000 + u64::try_from(since.as_millis()).unwrap_or(0)))
                        };
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
                        let primary: Primary<'_> = if uri.kind == Kind::Show {
                            None
                        } else {
                            Some(&primary)
                        };
                        self.attempt("play", primary, Some(&fallback), &check)
                    }
                }
            }
            PlayTarget::Liked { limit, random } => {
                let before_uri = before.track.as_ref().map(|t| t.uri.clone());
                let limit = limit.to_string();
                let random = *random;
                self.attempt(
                    "play_liked",
                    Some(&|p: &SpotifyPlayer| {
                        let mut args =
                            vec!["playback", "start", "liked", "--limit", limit.as_str()];
                        if random {
                            args.push("--random");
                        }
                        p.run(&args).map(drop)
                    }),
                    None,
                    &|pb, _| {
                        pb.state == PlayerState::Playing
                            && pb.track.as_ref().map(|t| t.uri.clone()) != before_uri
                    },
                )
            }
            PlayTarget::Radio { uri } => {
                let before_uri = before.track.as_ref().map(|t| t.uri.clone());
                let id = uri.id.clone();
                let kind = uri.kind.as_str().to_owned();
                if matches!(uri.kind, Kind::Show | Kind::Episode) {
                    return Err(Error::invalid(
                        "Radio can be seeded from a track, album, artist or playlist, not a podcast.",
                        "Pass a track, album, artist or playlist uri.",
                    ));
                }
                self.attempt(
                    "play_radio",
                    Some(&|p: &SpotifyPlayer| {
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
                    None,
                    &|pb, _| {
                        pb.state == PlayerState::Playing
                            && pb.track.as_ref().map(|t| t.uri.clone()) != before_uri
                    },
                )
            }
        }
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
        if before.state != PlayerState::Playing {
            return Ok(Outcome {
                action: "pause".into(),
                via: Via::Applescript,
                fallback: None,
                playback: before,
            });
        }
        self.attempt(
            "pause",
            Some(&|p: &SpotifyPlayer| p.run(&["playback", "pause"]).map(drop)),
            Some(&|| applescript::expect_ok(&self.script.run(&applescript::pause())?)),
            &|pb, _| pb.state != PlayerState::Playing,
        )
    }

    /// Toggles play/pause.
    ///
    /// # Errors
    /// Classified tool errors or `verification_failed`.
    pub fn toggle(&self) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let was_playing = before.state == PlayerState::Playing;
        self.attempt(
            "toggle",
            Some(&|p: &SpotifyPlayer| p.run(&["playback", "play-pause"]).map(drop)),
            Some(&|| applescript::expect_ok(&self.script.run(&applescript::toggle())?)),
            &|pb, _| (pb.state == PlayerState::Playing) != was_playing,
        )
    }

    /// Skips to the next track.
    ///
    /// # Errors
    /// Classified tool errors or `verification_failed`.
    pub fn next(&self) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let before_uri = before.track.as_ref().map(|t| t.uri.clone());
        let before_position = before.position_ms;
        self.attempt(
            "next",
            Some(&|p: &SpotifyPlayer| p.run(&["playback", "next"]).map(drop)),
            Some(&|| applescript::expect_ok(&self.script.run(&applescript::next())?)),
            &|pb, _| {
                // A new item, or the same one restarted from the top (repeat-one); judged
                // against the state before the command, never against elapsed time.
                pb.track.as_ref().map(|t| t.uri.clone()) != before_uri
                    || (before_position >= 4000 && pb.position_ms < 3000)
            },
        )
    }

    /// Goes to the previous track (or restarts the current one, as Spotify does after 3 s).
    ///
    /// # Errors
    /// Classified tool errors or `verification_failed`.
    pub fn previous(&self) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let before_uri = before.track.as_ref().map(|t| t.uri.clone());
        self.attempt(
            "previous",
            Some(&|p: &SpotifyPlayer| p.run(&["playback", "previous"]).map(drop)),
            Some(&|| applescript::expect_ok(&self.script.run(&applescript::previous())?)),
            &|pb, _| {
                // Spotify restarts the item when past 3 s, else goes to the previous item.
                pb.track.as_ref().map(|t| t.uri.clone()) != before_uri
                    || (before.position_ms >= 3000 && pb.position_ms < 3000)
            },
        )
    }

    /// Seeks within the current item.
    ///
    /// # Errors
    /// `nothing_playing`, classified tool errors or `verification_failed`.
    pub fn seek(&self, target: SeekTarget) -> Result<Outcome> {
        let before = self.ensure_running()?;
        let track = before.track.clone().ok_or_else(Error::nothing_playing)?;
        let target_ms = target.resolve_ms(before.position_ms, track.duration_ms);
        let observed_at = Instant::now();
        let playing = before.state == PlayerState::Playing;
        let offset = move || -> i64 {
            // Account for time elapsed since `before` was read while playing.
            let drift = if playing {
                i64::try_from(observed_at.elapsed().as_millis()).unwrap_or(0)
            } else {
                0
            };
            i64::try_from(target_ms).unwrap_or(0)
                - (i64::try_from(before.position_ms).unwrap_or(0) + drift)
        };
        let check = move |pb: &Playback, since: Duration| {
            let expected = target_ms
                + if playing {
                    u64::try_from(since.as_millis()).unwrap_or(0)
                } else {
                    0
                };
            pb.position_ms.abs_diff(expected) < 1500
                && pb.track.as_ref().is_some_and(|t| t.uri == track.uri)
        };
        let script = applescript::seek(target_ms);
        self.attempt(
            "seek",
            Some(&|p: &SpotifyPlayer| {
                let delta = offset().to_string();
                p.run(&["playback", "seek", "--", delta.as_str()]).map(drop)
            }),
            Some(&|| applescript::expect_ok(&self.script.run(&script)?)),
            &check,
        )
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
        self.attempt(
            "volume",
            Some(&|p: &SpotifyPlayer| p.run(&["playback", "volume", text.as_str()]).map(drop)),
            Some(&|| applescript::expect_ok(&self.script.run(&script)?)),
            &|pb, _| pb.volume.is_some_and(|v| v.abs_diff(value) <= 1),
        )
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
            return Ok(Outcome {
                action: "shuffle".into(),
                via: Via::Applescript,
                fallback: None,
                playback: before,
            });
        }
        let script = applescript::shuffle(target);
        self.attempt(
            "shuffle",
            Some(&|p: &SpotifyPlayer| p.run(&["playback", "shuffle"]).map(drop)),
            Some(&|| applescript::expect_ok(&self.script.run(&script)?)),
            &|pb, _| pb.shuffling == Some(target),
        )
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
        if use_player {
            match &self.player {
                Ok(player) => match self.repeat_with_player(player, target) {
                    Ok(playback) => {
                        return Ok(Outcome {
                            action: "repeat".into(),
                            via: Via::SpotifyPlayer,
                            fallback: None,
                            playback,
                        });
                    }
                    Err(error) => first_failure = Some(error),
                },
                Err(error) => first_failure = Some(error.clone()),
            }
            if self.strategy == Strategy::SpotifyPlayer {
                return Err(first_failure
                    .unwrap_or_else(|| Error::internal("repeat failed without a reason")));
            }
        }
        if target == RepeatMode::Track {
            return Err(Error::unsupported(
                "AppleScript cannot turn on repeat-one (track); only spotify_player can.",
                "Make sure `spotify auth status` is signed in and strategy is auto or spotify_player.",
            )
            .with_details(serde_json::json!({"spotify_player_error": first_failure})));
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
        let _ = before;
        Ok(Outcome {
            action: "repeat".into(),
            via: Via::Applescript,
            fallback: first_failure.map(|reason| Fallback {
                from: Via::SpotifyPlayer,
                reason,
            }),
            playback,
        })
    }

    fn repeat_with_player(&self, player: &SpotifyPlayer, target: RepeatMode) -> Result<Playback> {
        let read = || -> Result<Option<RepeatMode>> {
            let value = player.json(&["get", "key", "playback"])?;
            Ok(RepeatMode::from_web(
                value
                    .get("repeat_state")
                    .and_then(serde_json::Value::as_str),
            ))
        };
        let current = read()?.ok_or_else(|| {
            Error::new(
                "no_active_device",
                "The Web API reports no playback, so the repeat mode is unknown.",
                "Play something first.",
            )
        })?;
        for _ in 0..current.cycles_to(target) {
            player.run(&["playback", "repeat"])?;
            sleep(Duration::from_millis(400));
        }
        let deadline = Instant::now() + self.verify_timeout;
        loop {
            if read()? == Some(target) {
                return self.status();
            }
            if Instant::now() > deadline {
                return Err(Error::new(
                    "no_effect",
                    "spotify_player cycled repeat but the Web API did not reach the requested mode.",
                    "Retry.",
                ));
            }
            sleep(POLL);
        }
    }

    /// Likes (saves) or unlikes the current track. spotify_player only.
    ///
    /// # Errors
    /// `nothing_playing`, spotify_player errors.
    pub fn like(&self, like: bool) -> Result<Outcome> {
        let before = self.status()?;
        if before.track.is_none() {
            return Err(Error::nothing_playing());
        }
        let player = self.player.as_ref().map_err(Clone::clone)?;
        if like {
            player.run(&["like"])?;
        } else {
            player.run(&["like", "--unlike"])?;
        }
        Ok(Outcome {
            action: if like { "like".into() } else { "unlike".into() },
            via: Via::SpotifyPlayer,
            fallback: None,
            playback: before,
        })
    }
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
    use std::sync::Mutex;

    /// A fake Spotify.app: AppleScript commands mutate state; status reads it.
    struct FakeApp {
        state: Mutex<(String, u64, String)>, // (state, position_ms, uri)
        log: Mutex<Vec<String>>,
    }

    impl Runner for FakeApp {
        fn run(&self, source: &str) -> Result<String> {
            self.log.lock().expect("lock").push(
                source
                    .lines()
                    .find(|l| l.trim_start().starts_with(['p', 's', 'n']))
                    .unwrap_or("")
                    .trim()
                    .to_owned(),
            );
            let mut state = self.state.lock().expect("lock");
            if source.contains("character id 31") {
                let s = applescript::SEP;
                return Ok(format!(
                    "ok{s}{}{s}50{s}false{s}false{s}{}{s}{}{s}Song{s}Artist{s}Album{s}Artist{s}200000{s}1{s}1{s}10{s}{s}",
                    state.0, state.1, state.2
                ));
            }
            if source.contains("play track \"") {
                let uri = source
                    .split("play track \"")
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .unwrap_or_default();
                *state = ("playing".into(), 0, uri.to_owned());
            } else if source.contains("\t\tpause") {
                state.0 = "paused".into();
            } else if source.contains("\t\tplay") {
                state.0 = "playing".into();
            }
            Ok("ok".into())
        }
    }

    fn app() -> FakeApp {
        FakeApp {
            state: Mutex::new(("paused".into(), 1000, "spotify:track:a".into())),
            log: Mutex::new(Vec::new()),
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
    fn repeat_cycles() {
        assert_eq!(RepeatMode::Off.cycles_to(RepeatMode::Track), 2);
        assert_eq!(RepeatMode::Track.cycles_to(RepeatMode::Off), 1);
        assert_eq!(RepeatMode::Context.cycles_to(RepeatMode::Context), 0);
    }
}
