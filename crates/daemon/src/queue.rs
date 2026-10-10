//! The managed queue as a pure state machine (no I/O, unit-tested).
//!
//! Rules:
//! - Items stay in the queue until Spotify actually shows them playing. A hand-off is *pending*
//!   from the moment the play command is sent until `Started(item)` is observed (or a grace period
//!   passes, which counts as a failed attempt; three failures drop the item).
//! - While a hand-off is pending nothing else is decided, so a stale reading can never skip an
//!   item or resume over it.
//! - The head plays when the current item is within [`ADVANCE_MS`] of its end, when the current
//!   item ends on its own (Spotify moved on first, crossfade, stop), or on `spotify next`.
//! - `spotify next` while a hand-off is still in flight skips that item (it counts as played),
//!   so two quick `next`s never hand off the same item ([`QueueState::claim_next`]). A hand-off
//!   the watcher decided on but has not sent yet is not skipped: that `next` plays the item
//!   itself, and the watcher then sends nothing ([`QueueState::send_claimed`]).
//! - Handing off the item that is already playing restarts it; Spotify reports no new play for
//!   that, so a reading that shows it again from (near) its start counts as the start.
//! - When the last managed item nears its end or ends, the interrupted context resumes.
//! - Explicit commands (`play`, `previous`, `next` without a queue) open a short hold during which
//!   item changes are the user's, not something to override.
//! - Remaining-time rules need a known duration.

use serde::{Deserialize, Serialize};
use silicon_spotify_client::model::{PlayerState, now_rfc3339};
use silicon_spotify_client::trigger::{EndReason, PlayEvent};

/// Hand over when this little of the current item is left.
pub const ADVANCE_MS: u64 = 900;
/// Give a hand-off this long to show up in Spotify.app.
pub const GRACE_MS: u64 = 5_000;
/// After an explicit command, item changes are the user's for this long.
pub const HOLD_MS: u64 = 5_000;
/// Drop an item after this many failed hand-offs.
pub const MAX_ATTEMPTS: u32 = 3;
/// A pending item seen at most this far past the time since its hand-off was sent has started
/// (from the beginning) rather than kept playing.
pub const RESTART_SLACK_MS: u64 = 1_500;

/// A managed-queue entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    /// `q_<8 hex>`.
    pub id: String,
    /// What to play.
    pub uri: String,
    /// Title, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Artists or show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// When added.
    pub added_at: String,
    /// Who added it (ISI or home).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    /// Failed hand-offs so far.
    #[serde(default)]
    pub attempts: u32,
}

/// Where to return after the managed queue drains.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resume {
    /// The interrupted context.
    pub context_uri: Option<String>,
    /// The item that was up next in it.
    pub next_uri: Option<String>,
    /// When captured (RFC 3339).
    pub saved_at: String,
    /// When captured (unix ms), to refresh stale captures.
    #[serde(default)]
    pub saved_ms: u64,
}

/// A hand-off in flight.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    /// The item sent to Spotify.
    pub uri: String,
    /// When it was sent (unix ms).
    pub since_ms: u64,
    /// Whether the play command has gone out. The watcher marks its hand-off pending when it
    /// decides on it and sends it right after, under [`crate::service::Daemon::hand_off`]; a
    /// `spotify next` that gets that lock first takes the hand-off over instead of skipping an
    /// item Spotify never saw (see [`QueueState::claim_next`]).
    #[serde(default = "sent_by_default")]
    pub sent: bool,
}

/// Hand-offs persisted by earlier versions were sent right after they were marked.
fn sent_by_default() -> bool {
    true
}

/// Persisted queue state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueState {
    /// Upcoming managed items, in order.
    #[serde(default)]
    pub items: Vec<QueueItem>,
    /// The managed item playing now.
    #[serde(default)]
    pub managed_now: Option<String>,
    /// Hand-off in flight.
    #[serde(default)]
    pub pending: Option<Pending>,
    /// Resume point.
    #[serde(default)]
    pub resume: Option<Resume>,
    /// Explicit-command hold (unix ms).
    #[serde(default)]
    pub hold_until_ms: u64,
}

/// A minimal reading for decisions.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// Player state.
    pub state: PlayerState,
    /// Current item.
    pub uri: Option<String>,
    /// Its length (0 = unknown).
    pub duration_ms: u64,
    /// Position.
    pub position_ms: u64,
}

impl Reading {
    fn remaining(&self) -> Option<u64> {
        (self.duration_ms > 0).then(|| self.duration_ms.saturating_sub(self.position_ms))
    }
}

/// What the watcher must do.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Nothing.
    None,
    /// Start this item (the hand-off is already marked pending).
    Play(String),
    /// Play the resume point.
    Resume(Resume),
    /// Remember the context being played (Web API) before a hand-off.
    CaptureResume,
}

/// Events worth logging.
#[derive(Clone, Debug, PartialEq)]
pub enum Note {
    /// The pending hand-off showed up.
    Started(String),
    /// A hand-off did not show up in time.
    Failed { uri: String, attempts: u32 },
    /// An item was dropped after repeated failures.
    Dropped(String),
    /// `spotify next` skipped an item whose hand-off was still in flight.
    Skipped(String),
}

impl QueueState {
    /// Marks a hand-off of the head (not sent yet: the watcher sends it next) and returns its
    /// URI.
    pub fn begin_hand_off(&mut self, now_ms: u64) -> Option<String> {
        let head = self.items.first()?.uri.clone();
        self.pending = Some(Pending {
            uri: head.clone(),
            since_ms: now_ms,
            sent: false,
        });
        Some(head)
    }

    /// `spotify next` with managed items: claims the item to hand off now and marks it pending
    /// (the caller sends it before releasing [`crate::service::Daemon::hand_off`]).
    ///
    /// A hand-off still in flight (sent, not yet seen playing) is what the previous `next` (or
    /// the end of the track) moved to, so this `next` skips it: it leaves the queue and the one
    /// after it is claimed. `None` when nothing is left, in which case the caller moves on the
    /// way Spotify would. A pending hand-off older than [`GRACE_MS`] is counted as failed first.
    /// One the watcher decided on but has not sent yet is taken over: its item is claimed again
    /// (the watcher then finds its claim gone and sends nothing).
    pub fn claim_next(&mut self, now_ms: u64, notes: &mut Vec<Note>) -> Option<QueueItem> {
        if let Some(pending) = self.pending.take() {
            if !pending.sent {
                // Spotify never got it: nothing to skip.
            } else if now_ms.saturating_sub(pending.since_ms) <= GRACE_MS {
                if let Some(index) = self.items.iter().position(|h| h.uri == pending.uri) {
                    self.items.remove(index);
                }
                notes.push(Note::Skipped(pending.uri));
            } else if let Some(note) = self.fail(&pending.uri) {
                notes.push(note);
            }
        }
        let head = self.items.first()?.clone();
        self.pending = Some(Pending {
            uri: head.uri.clone(),
            since_ms: now_ms,
            sent: true,
        });
        Some(head)
    }

    /// The watcher is about to send the hand-off it decided on (`claim`, as [`Self::decide`]
    /// left it): marks it sent and returns true, or false when a `spotify next`, an explicit
    /// command or a queue edit replaced or dropped it meanwhile (then nothing may be sent).
    pub fn send_claimed(&mut self, claim: &Pending) -> bool {
        match self.pending.as_mut() {
            Some(pending) if pending == claim && !pending.sent => {
                pending.sent = true;
                true
            }
            _ => false,
        }
    }

    /// A pending item was verified: remove the requested URI and remember the URI Spotify
    /// actually plays (it can be a relinked release). A canceled claim stays canceled.
    pub fn hand_off_started(&mut self, uri: &str, playing: &str) -> Option<Note> {
        if !self.pending.as_ref().is_some_and(|p| p.uri == uri) {
            return None;
        }
        self.pending = None;
        // Remove that item (not blindly the head: `queue add --next` may have inserted another
        // item in front while the hand-off was in flight).
        if let Some(index) = self.items.iter().position(|h| h.uri == uri) {
            self.items.remove(index);
        }
        self.managed_now = Some(playing.to_owned());
        Some(Note::Started(playing.to_owned()))
    }

    /// Records a hand-off that failed to send.
    pub fn hand_off_failed(&mut self) -> Option<Note> {
        let pending = self.pending.take()?;
        self.fail(&pending.uri)
    }

    fn fail(&mut self, uri: &str) -> Option<Note> {
        let index = self.items.iter().position(|h| h.uri == uri)?;
        let item = &mut self.items[index];
        item.attempts += 1;
        if item.attempts >= MAX_ATTEMPTS {
            let dropped = self.items.remove(index);
            return Some(Note::Dropped(dropped.uri));
        }
        Some(Note::Failed {
            uri: uri.to_owned(),
            attempts: item.attempts,
        })
    }

    /// An explicit user command just ran.
    pub fn hold(&mut self, now_ms: u64) {
        self.hold_until_ms = now_ms + HOLD_MS;
        self.managed_now = None;
        self.pending = None;
    }

    /// Stores a captured resume point.
    pub fn set_resume(
        &mut self,
        context_uri: Option<String>,
        next_uri: Option<String>,
        now_ms: u64,
    ) {
        if context_uri.is_some() || next_uri.is_some() {
            self.resume = Some(Resume {
                context_uri,
                next_uri,
                saved_at: now_rfc3339(),
                saved_ms: now_ms,
            });
        }
    }

    /// Decides the next action from play boundaries and the latest reading.
    pub fn decide(
        &mut self,
        events: &[PlayEvent],
        reading: &Reading,
        now_ms: u64,
        notes: &mut Vec<Note>,
    ) -> Action {
        let held = now_ms < self.hold_until_ms;
        let mut want: Option<Action> = None;
        for event in events {
            match event {
                PlayEvent::Started(play) => {
                    if self.pending.as_ref().is_some_and(|p| p.uri == play.uri) {
                        notes.extend(self.hand_off_started(&play.uri, &play.uri));
                        want = None;
                        continue;
                    }
                    if self.pending.is_some() {
                        continue;
                    }
                    if self.managed_now.as_deref() == Some(play.uri.as_str()) {
                        // The managed item playing now started over: it did not end.
                        continue;
                    }
                    if held {
                        self.managed_now = None;
                        continue;
                    }
                    if !self.items.is_empty() {
                        // Spotify moved on by itself (end, crossfade, skip in the app): the queue
                        // plays first.
                        want = Some(Action::Play(String::new()));
                    } else if self.managed_now.is_some() {
                        // The last managed item ended and Spotify moved on: back to the context.
                        self.managed_now = None;
                        want = self.resume.take().map(Action::Resume);
                    }
                }
                PlayEvent::Ended { play, reason, .. } => {
                    let stopped = matches!(reason, EndReason::Stopped) && reading.uri.is_none();
                    if stopped
                        && self.pending.is_none()
                        && !held
                        && reading.state != PlayerState::NotRunning
                    {
                        if !self.items.is_empty() {
                            want = Some(Action::Play(String::new()));
                        } else if self.managed_now.as_deref() == Some(play.uri.as_str()) {
                            self.managed_now = None;
                            want = self.resume.take().map(Action::Resume);
                        }
                    }
                }
            }
        }
        // Handing off the item that was already playing restarts it without a new play: seeing
        // it from (near) its start after the hand-off was sent is its start.
        if let Some(pending) = self.pending.clone()
            && reading.uri.as_deref() == Some(pending.uri.as_str())
            && reading.state == PlayerState::Playing
            && now_ms >= pending.since_ms
            && reading.position_ms <= now_ms - pending.since_ms + RESTART_SLACK_MS
        {
            notes.extend(self.hand_off_started(&pending.uri, &pending.uri));
            return Action::None;
        }
        // A hand-off still in flight: wait for it, or count it failed after the grace period.
        if let Some(pending) = self.pending.clone() {
            if now_ms.saturating_sub(pending.since_ms) <= GRACE_MS {
                return Action::None;
            }
            self.pending = None;
            if let Some(note) = self.fail(&pending.uri) {
                notes.push(note);
            }
        }
        if let Some(action) = want {
            return match action {
                Action::Play(_) => self
                    .begin_hand_off(now_ms)
                    .map_or(Action::None, Action::Play),
                other => other,
            };
        }
        if held || reading.state != PlayerState::Playing {
            return Action::None;
        }
        let Some(remaining) = reading.remaining() else {
            return Action::None;
        };
        if remaining <= ADVANCE_MS {
            if !self.items.is_empty() {
                return self
                    .begin_hand_off(now_ms)
                    .map_or(Action::None, Action::Play);
            }
            if self.managed_now.is_some() && self.managed_now == reading.uri {
                self.managed_now = None;
                return self.resume.take().map_or(Action::None, Action::Resume);
            }
            return Action::None;
        }
        // Capture what is being interrupted, well before the hand-off window.
        let stale = self
            .resume
            .as_ref()
            .is_none_or(|r| now_ms.saturating_sub(r.saved_ms) > 60_000);
        if !self.items.is_empty()
            && self.managed_now.is_none()
            && stale
            && (2_000..=15_000).contains(&remaining)
        {
            return Action::CaptureResume;
        }
        Action::None
    }

    /// Milliseconds until the queue next needs a reading, if it does.
    #[must_use]
    pub fn next_deadline_ms(&self, reading: &Reading) -> Option<u64> {
        if self.pending.is_some() {
            return Some(250);
        }
        if reading.state != PlayerState::Playing
            || (self.items.is_empty() && self.managed_now.is_none())
        {
            return None;
        }
        let remaining = reading.remaining()?;
        Some(remaining.saturating_sub(ADVANCE_MS))
    }
}
