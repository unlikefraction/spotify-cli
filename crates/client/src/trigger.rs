//! The trigger engine: pure logic that turns playback observations into firings.
//!
//! A trigger is a checkpoint inside a track ("30 s left", "25% left", "50% passed", "song over")
//! that notifies the Silicon who created it through Ting. The daemon feeds this engine
//! [`Observation`]s (from Spotify's playback notifications and periodic AppleScript reads) and
//! delivers whatever it returns. Nothing here does I/O, so every rule below is unit-tested.
//!
//! Rules:
//! - A *play* is one continuous playing of one item. It starts when the item changes, or when the
//!   same item restarts from the top after reaching its end (repeat-one).
//! - Threshold conditions (`remaining`, `elapsed`) fire once per play, the first time an
//!   observation shows the threshold reached (also when the listener seeks past it).
//! - `end` fires when a play completes naturally (it was within 3 s of the end when the item
//!   changed or stopped). `change` fires whenever a play ends, with the reason.
//! - `current` scope binds to the play running at creation; it fires at most once and then
//!   completes. If that play ends first, the trigger *expires* (and can notify that too).
//! - A trigger never fires for a play whose threshold was already behind it at creation.

use serde::{Deserialize, Serialize};

use crate::model::{PlayerState, Track, now_rfc3339};
use crate::timing::{Amount, clock, clock_rounded};
use crate::{Error, Result};

/// How close to the end a play must get to count as completed.
pub const COMPLETE_SLACK_MS: u64 = 3_000;
/// A restart of the same item counts as a new play when it starts within this of the top.
pub const RESTART_WINDOW_MS: u64 = 5_000;
/// Completion is only inferred by extrapolating the position across gaps up to this long; after a
/// longer gap between readings (daemon down, read errors) the end reason is `unknown`.
pub const MAX_EXTRAPOLATION_MS: u64 = 12_000;

/// What a trigger waits for.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "at", rename_all = "snake_case")]
pub enum Condition {
    /// Fires when at most this much of the item is left.
    Remaining(Amount),
    /// Fires when at least this much has played.
    Elapsed(Amount),
    /// Fires when the item finishes by playing to its end.
    End,
    /// Fires when the item stops being current for any reason (completed, skipped, stopped).
    Change,
}

impl Condition {
    /// Short name: `remaining`, `elapsed`, `end`, `change`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Remaining(_) => "remaining",
            Self::Elapsed(_) => "elapsed",
            Self::End => "end",
            Self::Change => "change",
        }
    }

    /// Human description, e.g. `30 s remaining`.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Remaining(Amount::Millis(ms)) => format!("{} remaining", clock(ms)),
            Self::Remaining(Amount::Percent(p)) => format!("last {p}% remaining"),
            Self::Elapsed(Amount::Millis(ms)) => format!("{} elapsed", clock(ms)),
            Self::Elapsed(Amount::Percent(p)) => format!("{p}% mark passed"),
            Self::End => "track finished".into(),
            Self::Change => "track changed".into(),
        }
    }

    /// Position (ms) at which a threshold condition is reached for this duration.
    #[must_use]
    pub fn threshold_ms(self, duration_ms: u64) -> Option<u64> {
        match self {
            Self::Remaining(amount) => {
                Some(duration_ms.saturating_sub(amount.resolve_ms(duration_ms)))
            }
            // A time past the end is never reached ("10:00 elapsed" on a 3:00 song).
            Self::Elapsed(amount) => Some(amount.resolve_ms(duration_ms))
                .filter(|ms| duration_ms == 0 || *ms <= duration_ms),
            Self::End | Self::Change => None,
        }
    }
}

/// Which plays a trigger watches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// Only the play that was running when the trigger was created.
    Current {
        /// The item.
        uri: String,
        /// The play id at creation.
        play: u64,
    },
    /// Every play of one item.
    Track {
        /// The item.
        uri: String,
    },
    /// Every play of every item (ads excluded).
    Every,
}

impl Scope {
    /// `current`, `track` or `every`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Current { .. } => "current",
            Self::Track { .. } => "track",
            Self::Every => "every",
        }
    }

    fn matches(&self, play: &Play) -> bool {
        match self {
            Self::Current { play: id, .. } => *id == play.id,
            Self::Track { uri } => *uri == play.uri,
            Self::Every => !play.uri.starts_with("spotify:ad:"),
        }
    }
}

/// Lifecycle of a trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Waiting.
    Active,
    /// Fired as many times as allowed.
    Completed,
    /// Its play ended before it could fire.
    Expired,
    /// Removed by a caller.
    Removed,
}

/// A trigger and its runtime state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    /// `trg_<uuidv7 simple>`.
    pub id: String,
    /// What it waits for.
    pub condition: Condition,
    /// Which plays it watches.
    pub scope: Scope,
    /// Fire at most this many times (`None` = until removed).
    #[serde(default)]
    pub times: Option<u32>,
    /// Free text echoed in every notification (what to remind about).
    #[serde(default)]
    pub note: Option<String>,
    /// Short caller-chosen name.
    #[serde(default)]
    pub label: Option<String>,
    /// For `current` scope: notify with `spotify.trigger.expired` if the play ends first.
    #[serde(default = "yes")]
    pub notify_expiry: bool,
    /// Lifecycle state.
    pub status: Status,
    /// Times fired so far.
    #[serde(default)]
    pub fired: u32,
    /// Plays this trigger already handled (fired, or was already past at creation). Bounded.
    #[serde(default)]
    pub handled_plays: Vec<u64>,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// Last fire time (RFC 3339).
    #[serde(default)]
    pub last_fired_at: Option<String>,
}

fn yes() -> bool {
    true
}

/// One continuous playing of one item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Play {
    /// Monotonic play counter (persisted by the daemon).
    pub id: u64,
    /// The item.
    pub uri: String,
    /// Its length.
    pub duration_ms: u64,
}

/// One reading of Spotify.app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Wall-clock milliseconds when read.
    pub at_ms: u64,
    /// Player state.
    pub state: PlayerState,
    /// Position.
    pub position_ms: u64,
    /// The loaded item, if any.
    pub track: Option<Track>,
}

impl Observation {
    fn uri(&self) -> Option<&str> {
        self.track
            .as_ref()
            .map(|t| t.uri.as_str())
            .filter(|u| !u.is_empty())
    }

    fn duration(&self) -> u64 {
        self.track.as_ref().map_or(0, |t| t.duration_ms)
    }

    /// Position extrapolated to `at_ms` assuming playback continued.
    #[must_use]
    pub fn position_at(&self, at_ms: u64) -> u64 {
        let pos = if self.state == PlayerState::Playing {
            self.position_ms + at_ms.saturating_sub(self.at_ms)
        } else {
            self.position_ms
        };
        let duration = self.duration();
        if duration == 0 {
            pos
        } else {
            pos.min(duration)
        }
    }
}

/// Why a play ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// Played to (within 3 s of) its end.
    Completed,
    /// Another item started before the end.
    Skipped,
    /// Playback stopped or Spotify quit.
    Stopped,
    /// The daemon did not see the end (a long gap between readings).
    Unknown,
}

/// Tracks plays across observations. Persist it to keep play ids across daemon restarts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tracker {
    /// Highest play id handed out.
    pub last_play_id: u64,
    /// The play in progress.
    pub current: Option<Play>,
    /// The previous observation that showed an item (or none).
    pub last: Option<Observation>,
    /// One reading without an item was seen mid-play; a second one ends the play.
    #[serde(default)]
    pub pending_stop: bool,
}

/// A play boundary detected by [`Tracker::observe`].
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)] // Short-lived values; boxing would only add allocations.
pub enum PlayEvent {
    /// A play began.
    Started(Play),
    /// A play ended.
    Ended {
        /// The play.
        play: Play,
        /// Why.
        reason: EndReason,
        /// Last known (extrapolated) position.
        position_ms: u64,
        /// The item's metadata as last seen.
        track: Option<Track>,
    },
}

impl Tracker {
    /// Feeds one observation and returns play boundaries in order (end before start).
    pub fn observe(&mut self, obs: &Observation) -> Vec<PlayEvent> {
        let mut events = Vec::new();
        let new_uri = obs.uri().map(str::to_owned);
        // A single reading without an item mid-play is usually transient (Spotify between items
        // or busy); only a second consecutive one ends the play. Quitting Spotify ends it at once.
        if self.current.is_some() && new_uri.is_none() && obs.state != PlayerState::NotRunning {
            if !self.pending_stop {
                self.pending_stop = true;
                return events;
            }
        } else {
            self.pending_stop = false;
        }
        let previous = self.last.clone();
        match (&self.current, new_uri) {
            (None, Some(uri)) => {
                events.push(self.start(uri, obs.duration()));
            }
            (Some(play), new_uri) => {
                let gap = previous
                    .as_ref()
                    .map_or(u64::MAX, |last| obs.at_ms.saturating_sub(last.at_ms));
                let raw_pos = previous.as_ref().map_or(0, |last| last.position_ms);
                let last_pos = previous
                    .as_ref()
                    .map_or(0, |last| last.position_at(obs.at_ms));
                let near =
                    |pos: u64| play.duration_ms > 0 && pos + COMPLETE_SLACK_MS >= play.duration_ms;
                let near_end = near(raw_pos) || (gap <= MAX_EXTRAPOLATION_MS && near(last_pos));
                let unseen_end = !near_end && gap > MAX_EXTRAPOLATION_MS && near(last_pos);
                let same = new_uri.as_deref() == Some(play.uri.as_str());
                let restarted = same
                    && (near_end || unseen_end)
                    && obs.position_ms < RESTART_WINDOW_MS
                    && previous
                        .as_ref()
                        .is_some_and(|last| last.position_ms > obs.position_ms);
                if !same || restarted || obs.state == PlayerState::NotRunning {
                    let reason = if near_end {
                        EndReason::Completed
                    } else if unseen_end {
                        EndReason::Unknown
                    } else if new_uri.is_some() && obs.state != PlayerState::NotRunning {
                        EndReason::Skipped
                    } else {
                        EndReason::Stopped
                    };
                    let play = play.clone();
                    self.current = None;
                    events.push(PlayEvent::Ended {
                        track: previous.as_ref().and_then(|p| p.track.clone()),
                        play,
                        reason,
                        position_ms: if near_end {
                            last_pos
                        } else {
                            last_pos.min(raw_pos + MAX_EXTRAPOLATION_MS)
                        },
                    });
                    if let Some(uri) = new_uri.filter(|_| obs.state != PlayerState::NotRunning) {
                        events.push(self.start(uri, obs.duration()));
                    }
                } else if let Some(current) = self.current.as_mut() {
                    // Durations can arrive late (0) on the first read; keep the best value.
                    if current.duration_ms == 0 {
                        current.duration_ms = obs.duration();
                    }
                }
            }
            (None, None) => {}
        }
        self.last = Some(obs.clone());
        events
    }

    fn start(&mut self, uri: String, duration_ms: u64) -> PlayEvent {
        self.last_play_id += 1;
        let play = Play {
            id: self.last_play_id,
            uri,
            duration_ms,
        };
        self.current = Some(play.clone());
        PlayEvent::Started(play)
    }
}

/// What a firing reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Firing {
    /// `fired` or `expired`.
    pub outcome: String,
    /// The trigger (state after this firing).
    pub trigger: Trigger,
    /// The play it fired for.
    pub play_id: u64,
    /// Item metadata.
    pub track: Option<Track>,
    /// Position when it fired.
    pub position_ms: u64,
    /// Item length.
    pub duration_ms: u64,
    /// For `end`/`change` and expiry: why the play ended.
    pub reason: Option<EndReason>,
    /// When (RFC 3339).
    pub at: String,
}

impl Firing {
    /// Stable Ting idempotency key: unique per trigger, play and outcome.
    #[must_use]
    pub fn key(&self, recipient: &str) -> String {
        format!(
            "{recipient}/{}/{}/{}",
            self.trigger.id, self.play_id, self.outcome
        )
    }

    /// The Ting `data` object.
    #[must_use]
    pub fn data(&self) -> serde_json::Value {
        let remaining = self.duration_ms.saturating_sub(self.position_ms);
        #[allow(clippy::cast_precision_loss)]
        let progress = if self.duration_ms == 0 {
            0.0
        } else {
            ((self.position_ms as f64 / self.duration_ms as f64) * 1000.0).round() / 1000.0
        };
        let threshold = match self.trigger.condition {
            Condition::Remaining(amount) | Condition::Elapsed(amount) => Some(amount.to_string()),
            _ => None,
        };
        serde_json::json!({
            "outcome": self.outcome,
            "trigger": {
                "id": self.trigger.id,
                "label": self.trigger.label,
                "condition": self.trigger.condition.name(),
                "threshold": threshold,
                "description": self.trigger.condition.describe(),
                "scope": self.trigger.scope.name(),
                "note": self.trigger.note,
                "fired": self.trigger.fired,
                "times": self.trigger.times,
                "final": self.trigger.status != Status::Active,
            },
            "track": self.track.as_ref().map(|t| serde_json::json!({
                "uri": t.uri, "name": t.name, "artist": t.artist, "album": t.album,
                "duration_ms": t.duration_ms, "url": t.url, "artwork_url": t.artwork_url,
            })),
            "playback": {
                "position_ms": self.position_ms,
                "position": clock(self.position_ms),
                "remaining_ms": remaining,
                // Measured a poll after the threshold, so usually a few hundred ms under it:
                // rounded, a `--remaining 20s` firing reads 0:20, not 0:19.
                "remaining": clock_rounded(remaining),
                "progress": progress,
            },
            "reason": self.reason,
            "at": self.at,
        })
    }
}

/// Keeps `handled_plays` bounded.
const MAX_HANDLED: usize = 32;

/// Longest `--note`, in characters (it travels inside the Ting notification).
pub const MAX_NOTE_CHARS: usize = 1000;
/// Longest `--label`, in characters.
pub const MAX_LABEL_CHARS: usize = 80;

/// The checks of a trigger request that need no playback state: note and label length,
/// `--times`, percentage range. [`Trigger::create`] runs them first; callers run them before
/// reading Spotify so a malformed request fails as `invalid_input` whatever is playing.
///
/// # Errors
/// `invalid_input`.
pub fn validate_request(
    condition: Condition,
    times: Option<u32>,
    note: Option<&str>,
    label: Option<&str>,
) -> Result<()> {
    if let Some(note) = note
        && note.chars().count() > MAX_NOTE_CHARS
    {
        return Err(Error::invalid(
            format!("--note is longer than {MAX_NOTE_CHARS} characters."),
            "Shorten the note; it travels inside the Ting notification.",
        ));
    }
    if let Some(label) = label
        && label.chars().count() > MAX_LABEL_CHARS
    {
        return Err(Error::invalid(
            format!("--label is longer than {MAX_LABEL_CHARS} characters."),
            "Use a short label; put details in --note.",
        ));
    }
    if times == Some(0) {
        return Err(Error::invalid(
            "--times must be at least 1.",
            "Omit --times to fire until removed, or use --once.",
        ));
    }
    if let Condition::Remaining(Amount::Percent(p)) | Condition::Elapsed(Amount::Percent(p)) =
        condition
        && !(0.0..=100.0).contains(&p)
    {
        return Err(Error::invalid(
            "Percentages must be between 0% and 100%.",
            "For example --remaining 25% or --elapsed 50%.",
        ));
    }
    Ok(())
}

impl Trigger {
    /// Validates and prepares a new trigger against the current playback.
    #[allow(clippy::too_many_arguments)] // Mirrors the CLI flags one to one.
    ///
    /// # Errors
    /// `nothing_playing` for `current` scope with nothing loaded; `threshold_passed` when a
    /// `current` trigger's checkpoint is already behind; `invalid_input` for bad limits (see
    /// [`validate_request`]).
    pub fn create(
        id: String,
        condition: Condition,
        scope_request: ScopeRequest,
        times: Option<u32>,
        note: Option<String>,
        label: Option<String>,
        notify_expiry: bool,
        tracker: &Tracker,
    ) -> Result<Self> {
        validate_request(condition, times, note.as_deref(), label.as_deref())?;
        let current_play = tracker.current.clone();
        let scope = match scope_request {
            ScopeRequest::Current => {
                let play = current_play.clone().ok_or_else(|| {
                    Error::nothing_playing().with_details(serde_json::json!({
                        "why": "A --scope current trigger binds to the item playing now.",
                        "alternatives": ["--scope every", "--scope track --track <uri>"],
                    }))
                })?;
                Scope::Current {
                    uri: play.uri,
                    play: play.id,
                }
            }
            ScopeRequest::Track(uri) => Scope::Track { uri },
            ScopeRequest::Every => Scope::Every,
        };
        let times = match scope {
            Scope::Current { .. } => Some(1),
            _ => times,
        };
        let mut trigger = Self {
            id,
            condition,
            scope,
            times,
            note,
            label,
            notify_expiry,
            status: Status::Active,
            fired: 0,
            handled_plays: Vec::new(),
            created_at: now_rfc3339(),
            last_fired_at: None,
        };
        // A checkpoint past the end of the current item can never be reached.
        if let (Scope::Current { .. }, Some(play), Condition::Elapsed(amount)) =
            (&trigger.scope, &current_play, condition)
        {
            let duration = if play.duration_ms > 0 {
                play.duration_ms
            } else {
                tracker.last.as_ref().map_or(0, Observation::duration)
            };
            if duration > 0 && condition.threshold_ms(duration).is_none() {
                return Err(Error::invalid(
                    format!(
                        "The track is only {} long, so `{amount}` elapsed is never reached.",
                        clock(duration)
                    ),
                    "Use a shorter time, a percentage (e.g. --elapsed 90%), --end, or --scope every for longer tracks.",
                ));
            }
        }
        // Never fire for a play whose checkpoint is already behind.
        if let (Some(play), Some(last)) = (&current_play, &tracker.last)
            && trigger.scope.matches(play)
        {
            let duration = if play.duration_ms > 0 {
                play.duration_ms
            } else {
                last.duration()
            };
            if let Some(threshold) = condition.threshold_ms(duration) {
                let position = last.position_at(crate::model::now_ms());
                if position >= threshold {
                    if matches!(trigger.scope, Scope::Current { .. }) {
                        return Err(Error::new(
                                "threshold_passed",
                                format!(
                                    "The checkpoint ({} = {}) is already behind: the track is at {} of {}.",
                                    condition.describe(), clock(threshold), clock(position), clock(duration)
                                ),
                                "Pick a later checkpoint, use --end, seek back with `spotify seek`, or add --scope every to catch it on the next track.",
                            )
                            .with_details(serde_json::json!({"position_ms": position, "threshold_ms": threshold, "duration_ms": duration})));
                    }
                    trigger.handled_plays.push(play.id);
                }
            }
        }
        Ok(trigger)
    }

    fn handled(&self, play: u64) -> bool {
        self.handled_plays.contains(&play)
    }

    fn mark(&mut self, play: u64) {
        self.handled_plays.push(play);
        if self.handled_plays.len() > MAX_HANDLED {
            let excess = self.handled_plays.len() - MAX_HANDLED;
            self.handled_plays.drain(..excess);
        }
    }

    fn fire(&mut self, play: u64) {
        self.mark(play);
        self.fired += 1;
        self.last_fired_at = Some(now_rfc3339());
        if self.times.is_some_and(|times| self.fired >= times) {
            self.status = Status::Completed;
        }
    }
}

/// How the caller asked to scope a trigger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeRequest {
    /// The play running now.
    Current,
    /// Every play of this URI.
    Track(String),
    /// Every item.
    Every,
}

/// Evaluates triggers against one observation (after [`Tracker::observe`]).
///
/// Mutates the triggers' state and returns what must be delivered.
pub fn evaluate(
    triggers: &mut [Trigger],
    tracker: &Tracker,
    events: &[PlayEvent],
    obs: &Observation,
) -> Vec<Firing> {
    let mut firings = Vec::new();
    let now = now_rfc3339();
    // 1. Play boundaries: end/change conditions, and expiry of current-scope triggers.
    for event in events {
        let PlayEvent::Ended {
            play,
            reason,
            position_ms,
            track,
        } = event
        else {
            continue;
        };
        for trigger in triggers.iter_mut().filter(|t| t.status == Status::Active) {
            if !trigger.scope.matches(play) || trigger.handled(play.id) {
                continue;
            }
            let fires = match trigger.condition {
                Condition::End => *reason == EndReason::Completed,
                Condition::Change => true,
                // A threshold reached exactly at the end (e.g. `--remaining 0s`) counts on completion.
                Condition::Remaining(_) | Condition::Elapsed(_) => {
                    *reason == EndReason::Completed
                        && trigger
                            .condition
                            .threshold_ms(play.duration_ms)
                            .is_some_and(|t| t <= play.duration_ms)
                }
            };
            if fires {
                trigger.fire(play.id);
                firings.push(Firing {
                    outcome: "fired".into(),
                    trigger: trigger.clone(),
                    play_id: play.id,
                    track: track.clone(),
                    position_ms: *position_ms,
                    duration_ms: play.duration_ms,
                    reason: Some(*reason),
                    at: now.clone(),
                });
            } else if matches!(trigger.scope, Scope::Current { .. }) {
                trigger.mark(play.id);
                trigger.status = Status::Expired;
                if trigger.notify_expiry {
                    firings.push(Firing {
                        outcome: "expired".into(),
                        trigger: trigger.clone(),
                        play_id: play.id,
                        track: track.clone(),
                        position_ms: *position_ms,
                        duration_ms: play.duration_ms,
                        reason: Some(*reason),
                        at: now.clone(),
                    });
                }
            }
        }
    }
    // 2. Thresholds within the current play.
    let Some(play) = &tracker.current else {
        return firings;
    };
    if obs.state == PlayerState::NotRunning {
        return firings;
    }
    let duration = if play.duration_ms > 0 {
        play.duration_ms
    } else {
        obs.duration()
    };
    if duration == 0 {
        return firings;
    }
    for trigger in triggers.iter_mut().filter(|t| t.status == Status::Active) {
        if !trigger.scope.matches(play) || trigger.handled(play.id) {
            continue;
        }
        let Some(threshold) = trigger.condition.threshold_ms(duration) else {
            continue;
        };
        if obs.position_ms >= threshold && !(threshold >= duration && obs.position_ms < duration) {
            trigger.fire(play.id);
            firings.push(Firing {
                outcome: "fired".into(),
                trigger: trigger.clone(),
                play_id: play.id,
                track: obs.track.clone(),
                position_ms: obs.position_ms,
                duration_ms: duration,
                reason: None,
                at: now.clone(),
            });
        }
    }
    firings
}

/// Milliseconds until the next checkpoint in the current play, assuming playback continues.
/// The daemon wakes slightly before this to take a fresh reading.
#[must_use]
pub fn next_deadline_ms(triggers: &[Trigger], tracker: &Tracker, now_ms: u64) -> Option<u64> {
    let play = tracker.current.as_ref()?;
    let last = tracker.last.as_ref()?;
    if last.state != PlayerState::Playing {
        return None;
    }
    let duration = if play.duration_ms > 0 {
        play.duration_ms
    } else {
        last.duration()
    };
    if duration == 0 {
        // Unknown length: no checkpoint can be scheduled yet.
        return None;
    }
    let position = last.position_at(now_ms);
    triggers
        .iter()
        .filter(|t| t.status == Status::Active && t.scope.matches(play) && !t.handled(play.id))
        .filter_map(|t| match t.condition {
            Condition::Remaining(_) | Condition::Elapsed(_) => t.condition.threshold_ms(duration),
            // Track ends are detected from the change notification; wake near the end to confirm.
            Condition::End | Condition::Change => Some(duration),
        })
        .map(|threshold| threshold.saturating_sub(position))
        .min()
}
