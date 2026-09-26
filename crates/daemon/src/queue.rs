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
    /// Send `play track <uri>` (the hand-off is already marked pending).
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
}

impl QueueState {
    /// Marks a hand-off of the head and returns its URI.
    pub fn begin_hand_off(&mut self, now_ms: u64) -> Option<String> {
        let head = self.items.first()?.uri.clone();
        self.pending = Some(Pending {
            uri: head.clone(),
            since_ms: now_ms,
        });
        Some(head)
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
                        self.pending = None;
                        // Remove that item (not blindly the head: `queue add --next` may have
                        // inserted another item in front while the hand-off was in flight).
                        if let Some(index) = self.items.iter().position(|h| h.uri == play.uri) {
                            self.items.remove(index);
                        }
                        self.managed_now = Some(play.uri.clone());
                        notes.push(Note::Started(play.uri.clone()));
                        want = None;
                        continue;
                    }
                    if self.pending.is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use silicon_spotify_client::trigger::Play;

    const T: &str = "spotify:track:t";
    const U: &str = "spotify:track:u";
    const X: &str = "spotify:track:x";
    const Y: &str = "spotify:track:y";

    fn item(uri: &str) -> QueueItem {
        QueueItem {
            id: format!("q_{uri}"),
            uri: uri.into(),
            name: None,
            by: None,
            duration_ms: None,
            added_at: String::new(),
            added_by: None,
            attempts: 0,
        }
    }

    fn reading(uri: &str, position_ms: u64) -> Reading {
        Reading {
            state: PlayerState::Playing,
            uri: Some(uri.into()),
            duration_ms: 200_000,
            position_ms,
        }
    }

    fn started(uri: &str) -> PlayEvent {
        PlayEvent::Started(Play {
            id: 1,
            uri: uri.into(),
            duration_ms: 200_000,
        })
    }

    fn ended(uri: &str, reason: EndReason) -> PlayEvent {
        PlayEvent::Ended {
            play: Play {
                id: 1,
                uri: uri.into(),
                duration_ms: 200_000,
            },
            reason,
            position_ms: 0,
            track: None,
        }
    }

    fn queue(items: &[&str]) -> QueueState {
        QueueState {
            items: items.iter().map(|u| item(u)).collect(),
            ..QueueState::default()
        }
    }

    #[test]
    fn captures_the_context_then_hands_off_near_the_end() {
        let mut q = queue(&[X]);
        let mut notes = Vec::new();
        assert_eq!(
            q.decide(&[], &reading(T, 190_000), 1_000, &mut notes),
            Action::CaptureResume
        );
        q.set_resume(Some("spotify:playlist:p".into()), Some(U.into()), 1_000);
        assert_eq!(
            q.decide(&[], &reading(T, 195_000), 2_000, &mut notes),
            Action::None
        );
        assert_eq!(
            q.decide(&[], &reading(T, 199_300), 3_000, &mut notes),
            Action::Play(X.into())
        );
        // Stale readings while pending never advance again or resume.
        assert_eq!(
            q.decide(&[], &reading(T, 199_600), 3_300, &mut notes),
            Action::None
        );
        assert_eq!(
            q.decide(
                &[ended(T, EndReason::Completed), started(U)],
                &reading(U, 100),
                3_500,
                &mut notes
            ),
            Action::None
        );
        assert_eq!(q.items.len(), 1, "the item stays until it is seen playing");
        assert_eq!(
            q.decide(
                &[ended(U, EndReason::Skipped), started(X)],
                &reading(X, 200),
                3_800,
                &mut notes
            ),
            Action::None
        );
        assert!(q.items.is_empty());
        assert_eq!(q.managed_now.as_deref(), Some(X));
        // The last managed item nears its end: resume the interrupted context.
        match q.decide(&[], &reading(X, 199_500), 200_000, &mut notes) {
            Action::Resume(r) => assert_eq!(r.next_uri.as_deref(), Some(U)),
            other => panic!("expected resume, got {other:?}"),
        }
        assert_eq!(q.managed_now, None);
    }

    #[test]
    fn spotify_moving_on_first_still_plays_the_queue() {
        let mut q = queue(&[X, Y]);
        let mut notes = Vec::new();
        assert_eq!(
            q.decide(
                &[ended(T, EndReason::Completed), started(U)],
                &reading(U, 300),
                10,
                &mut notes
            ),
            Action::Play(X.into())
        );
        q.decide(
            &[ended(U, EndReason::Skipped), started(X)],
            &reading(X, 100),
            400,
            &mut notes,
        );
        assert_eq!(q.items.len(), 1);
        // X ends on its own (crossfade into autoplay V): Y plays next.
        assert_eq!(
            q.decide(
                &[ended(X, EndReason::Completed), started("spotify:track:v")],
                &reading("spotify:track:v", 500),
                300_000,
                &mut notes
            ),
            Action::Play(Y.into())
        );
    }

    #[test]
    fn failed_hand_offs_retry_then_drop() {
        let mut q = queue(&[X]);
        let mut notes = Vec::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let now = u64::from(attempt) * 10_000;
            assert_eq!(
                q.decide(&[], &reading(T, 199_500), now, &mut notes),
                Action::Play(X.into())
            );
            // Nothing happens for longer than the grace period.
            assert_eq!(
                q.decide(&[], &reading(T, 199_600), now + GRACE_MS + 1, &mut notes),
                if attempt < MAX_ATTEMPTS {
                    Action::Play(X.into())
                } else {
                    Action::None
                }
            );
            if attempt < MAX_ATTEMPTS {
                q.pending = None;
            }
        }
        assert!(
            notes
                .iter()
                .any(|n| matches!(n, Note::Dropped(u) if u == X))
        );
        assert!(q.items.is_empty());
    }

    #[test]
    fn explicit_commands_are_not_overridden() {
        let mut q = queue(&[X]);
        let mut notes = Vec::new();
        q.hold(1_000);
        assert_eq!(
            q.decide(
                &[ended(T, EndReason::Skipped), started(Y)],
                &reading(Y, 100),
                2_000,
                &mut notes
            ),
            Action::None
        );
        assert_eq!(q.items.len(), 1, "queue kept for after the explicit item");
        assert_eq!(
            q.decide(&[], &reading(Y, 199_500), 200_000, &mut notes),
            Action::Play(X.into())
        );
    }

    #[test]
    fn an_item_inserted_during_a_hand_off_is_kept() {
        let mut q = queue(&[X]);
        let mut notes = Vec::new();
        assert_eq!(
            q.decide(&[], &reading(T, 199_500), 1_000, &mut notes),
            Action::Play(X.into())
        );
        q.items.insert(0, item(Y)); // `queue add Y --next` while X is being handed over
        q.decide(&[started(X)], &reading(X, 100), 1_300, &mut notes);
        assert_eq!(
            q.items.iter().map(|i| i.uri.as_str()).collect::<Vec<_>>(),
            vec![Y]
        );
    }

    #[test]
    fn unknown_duration_never_triggers_a_hand_off() {
        let mut q = queue(&[X]);
        let mut notes = Vec::new();
        let r = Reading {
            state: PlayerState::Playing,
            uri: Some(T.into()),
            duration_ms: 0,
            position_ms: 0,
        };
        assert_eq!(q.decide(&[], &r, 1, &mut notes), Action::None);
        assert_eq!(q.next_deadline_ms(&r), None);
    }

    #[test]
    fn stopping_after_the_last_item_resumes() {
        let mut q = QueueState {
            managed_now: Some(X.into()),
            resume: Some(Resume {
                context_uri: Some("spotify:album:a".into()),
                next_uri: None,
                saved_at: String::new(),
                saved_ms: 0,
            }),
            ..QueueState::default()
        };
        let mut notes = Vec::new();
        let stopped = Reading {
            state: PlayerState::Stopped,
            uri: None,
            duration_ms: 0,
            position_ms: 0,
        };
        assert!(matches!(
            q.decide(&[ended(X, EndReason::Stopped)], &stopped, 5, &mut notes),
            Action::Resume(_)
        ));
    }
}
