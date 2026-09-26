//! The watcher: reads Spotify.app, tracks plays, fires triggers and drives the managed queue.
//!
//! Readings happen when Spotify posts a playback notification, when a CLI command changed
//! something, and on a timer whose interval adapts: every 2 s while playing with work to do,
//! 5 s while playing otherwise, 10 s paused, 15 s when Spotify is closed (3 s in the daemon's
//! first two minutes while no Apple Event has reached it yet), and precisely at the next trigger checkpoint or queue hand-off.
//! Every reading (also the one `trigger add` takes) goes through [`observe_now`], which
//! serializes read-and-process so readings are never applied out of order.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long after start a closed Spotify is polled every 3 s (see [`process`]).
const FIRST_CONTACT_WINDOW: Duration = Duration::from_secs(120);

use serde_json::{Value, json};
use silicon_spotify_client::Result;
use silicon_spotify_client::applescript;
use silicon_spotify_client::model::{Playback, PlayerState, now_ms, now_rfc3339};
use silicon_spotify_client::trigger::{self, Observation, Status};

use crate::db::FiringRow;
use crate::log;
use crate::queue::{Action, Note, Pending, Reading, Resume};
use crate::service::{Daemon, Settings, blocking};

/// Runs forever.
pub async fn run(daemon: Arc<Daemon>) {
    loop {
        let d = Arc::clone(&daemon);
        let wait = match blocking(move || observe_now(&d).map(|(_, wait)| wait)).await {
            Ok(wait) => wait,
            Err(error) => {
                let repeated = daemon.live().watch_error.as_ref().map(|e| e.code.clone())
                    == Some(error.code.clone());
                if !repeated {
                    log!("watcher: cannot read Spotify.app: {error}");
                }
                daemon.live().watch_error = Some(error);
                Duration::from_secs(10)
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = daemon.nudge.notified() => {
                // Let Spotify settle after a change before reading.
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            () = daemon.shutdown.notified() => return,
        }
    }
}

/// Reads Spotify.app and processes the reading (triggers, queue). Serialized across callers.
///
/// # Errors
/// AppleScript or store errors.
pub fn observe_now(daemon: &Arc<Daemon>) -> Result<(Playback, Duration)> {
    let _serial = daemon
        .observe_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let playback = daemon.read()?;
    let at = now_ms();
    let wait = process(daemon, &playback, at)?;
    Ok((playback, wait))
}

fn reading_of(playback: &Playback) -> Reading {
    Reading {
        state: playback.state,
        uri: playback
            .track
            .as_ref()
            .map(|t| t.uri.clone())
            .filter(|u| !u.is_empty()),
        duration_ms: playback.track.as_ref().map_or(0, |t| t.duration_ms),
        position_ms: playback.position_ms,
    }
}

/// Processes one reading taken at `at` (unix ms); returns how long to wait before the next.
fn process(daemon: &Arc<Daemon>, playback: &Playback, at: u64) -> Result<Duration> {
    let observation = Observation {
        at_ms: at,
        state: playback.state,
        position_ms: playback.position_ms,
        track: playback.track.clone(),
    };
    let mut live = daemon.live();
    if live
        .tracker
        .last
        .as_ref()
        .is_some_and(|last| last.at_ms > at)
    {
        // A newer reading was already applied.
        return Ok(Duration::from_millis(200));
    }
    live.readings += 1;
    live.watch_error = None;
    let events = live.tracker.observe(&observation);

    // Triggers.
    let before: Vec<Status> = live.triggers.iter().map(|s| s.trigger.status).collect();
    let mut triggers: Vec<_> = live.triggers.iter().map(|s| s.trigger.clone()).collect();
    let firings = trigger::evaluate(&mut triggers, &live.tracker, &events, &observation);
    let mut changed = !firings.is_empty();
    for (stored, updated) in live.triggers.iter_mut().zip(triggers) {
        if stored.trigger != updated {
            stored.trigger = updated;
            changed = true;
        }
    }
    // Firings first, so anyone woken by a status change already finds them.
    for firing in &firings {
        let Some(stored) = daemon.db.trigger(&firing.trigger.id)? else {
            continue;
        };
        let row = firing_row(firing, &stored.delivery);
        daemon.db.insert_firing(&row)?;
        log!(
            "trigger {} {} ({}){}",
            firing.trigger.id,
            firing.outcome,
            firing.trigger.condition.describe(),
            if stored.delivery.ting {
                " → Ting"
            } else {
                " (local)"
            }
        );
        let _ = daemon.events.send(json!({"firing": row.id, "trigger": row.trigger_id, "state": row.state, "outcome": row.outcome}));
    }
    if changed {
        for (index, stored) in live.triggers.iter().enumerate() {
            daemon.db.save_trigger(stored)?;
            if before.get(index) != Some(&stored.trigger.status) {
                // Also wakes `trigger wait` for triggers that ended without a firing.
                let _ = daemon
                    .events
                    .send(json!({"trigger": stored.trigger.id, "status": stored.trigger.status}));
            }
        }
        live.triggers.retain(|s| s.trigger.status == Status::Active);
    }
    if !firings.is_empty() {
        daemon.deliver.notify_one();
    }
    daemon.db.put("tracker", &live.tracker)?;

    // Managed queue.
    let reading = reading_of(playback);
    let mut notes = Vec::new();
    let action = live.queue.decide(&events, &reading, at, &mut notes);
    for note in &notes {
        match note {
            Note::Started(uri) => log!("queue: {uri} is playing"),
            Note::Failed { uri, attempts } => {
                log!("queue: {uri} did not start (attempt {attempts}); retrying")
            }
            Note::Dropped(uri) => log!("queue: dropped {uri} after repeated failed hand-offs"),
            Note::Skipped(uri) => log!("queue: skipped {uri} before it started"),
        }
    }
    daemon.save_queue(&live);
    // The hand-off `decide` just marked pending, exactly (see `send_hand_off`).
    let claim = match &action {
        Action::Play(_) => live.queue.pending.clone(),
        _ => None,
    };

    // Next wake.
    let busy = !live.triggers.is_empty()
        || !live.queue.items.is_empty()
        || live.queue.managed_now.is_some();
    let mut wait = match playback.state {
        PlayerState::Playing if busy => 2_000,
        PlayerState::Playing => 5_000,
        PlayerState::Paused | PlayerState::Stopped => 10_000,
        // Right after the daemon starts (install, login) and until an Apple Event has reached
        // Spotify, look again soon: the first one after Spotify starts raises macOS's
        // Automation question, which someone may be waiting on.
        PlayerState::NotRunning
            if crate::macos::automation_state() == crate::macos::Automation::Unknown
                && daemon.started.elapsed() < FIRST_CONTACT_WINDOW =>
        {
            3_000
        }
        PlayerState::NotRunning => 15_000,
    };
    if live.tracker.pending_stop {
        wait = wait.min(1_000);
    }
    let active: Vec<_> = live.triggers.iter().map(|s| s.trigger.clone()).collect();
    if let Some(eta) = trigger::next_deadline_ms(&active, &live.tracker, now_ms()) {
        wait = wait.min(eta.saturating_sub(250).max(40));
    }
    if let Some(eta) = live.queue.next_deadline_ms(&reading) {
        wait = wait.min(eta.saturating_sub(250).max(40));
    }
    live.last = Some(playback.clone());
    live.last_at = Some(Instant::now());
    drop(live);

    match action {
        Action::None => {}
        Action::Play(_) => {
            if let Some(claim) = claim {
                send_hand_off(daemon, &claim, "end of track");
            }
            return Ok(Duration::from_millis(250));
        }
        Action::Resume(resume) => {
            resume_context(daemon, &resume, at);
            return Ok(Duration::from_millis(300));
        }
        Action::CaptureResume => capture_resume(daemon, &daemon.settings()),
    }
    Ok(Duration::from_millis(wait))
}

fn firing_row(firing: &trigger::Firing, delivery: &crate::db::Delivery) -> FiringRow {
    let ting_type = if firing.outcome == "expired" {
        "spotify.trigger.expired"
    } else {
        "spotify.trigger.fired"
    };
    FiringRow {
        id: format!("fir_{}", uuid::Uuid::now_v7().simple()),
        trigger_id: firing.trigger.id.clone(),
        home: delivery.home.clone(),
        outcome: firing.outcome.clone(),
        ting_type: ting_type.into(),
        key_suffix: format!(
            "{}/{}/{}",
            firing.trigger.id, firing.play_id, firing.outcome
        ),
        data: firing.data(),
        metadata: metadata(delivery),
        delivery: delivery.clone(),
        state: if delivery.ting {
            "pending".into()
        } else {
            "local".into()
        },
        attempts: 0,
        next_attempt_ms: i64::try_from(now_ms()).unwrap_or(0),
        last_error: None,
        ting: None,
        created_at: now_rfc3339(),
        sent_at: None,
    }
}

/// Ting metadata: routing hints only, never authority.
#[must_use]
pub fn metadata(delivery: &crate::db::Delivery) -> Value {
    json!({
        "isi": delivery.isi,
        "app": "spotify",
        "app_version": silicon_spotify_client::VERSION,
        "host": hostname(),
    })
}

fn hostname() -> Option<String> {
    std::process::Command::new("/bin/hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Sends `play track <uri>` for the hand-off `decide` marked pending (`claim`); a send failure
/// counts as a failed attempt. Nothing is sent when that exact claim is gone: a `spotify next`
/// took it over (and sent the item itself, with a claim of its own), or an explicit command or
/// a queue edit dropped it.
fn send_hand_off(daemon: &Arc<Daemon>, claim: &Pending, why: &str) {
    let _serial = daemon
        .hand_off
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    {
        let mut live = daemon.live();
        if !live.queue.send_claimed(claim) {
            return;
        }
        daemon.save_queue(&live);
    }
    let uri = claim.uri.as_str();
    let result = daemon
        .script
        .run(&applescript::play_uri(uri, None))
        .and_then(|o| applescript::expect_ok(&o));
    match result {
        Ok(()) => log!("queue: handing over to {uri} ({why})"),
        Err(error) => {
            let mut live = daemon.live();
            let note = live.queue.hand_off_failed();
            daemon.save_queue(&live);
            log!("queue: could not start {uri}: {error} ({note:?})");
        }
    }
    daemon.nudge.notify_one();
}

/// Longest wait for a one-shot Web API read of playback in [`capture_resume`] (usually 1–2 s).
const FRESH_READ_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether spotify_player's memory of playback (`get key playback`) names another item than the
/// Web API's queue, read just after it (`get key queue`, which spotify_player asks the Web API
/// for): the memory then describes an earlier moment, and its context may be an earlier one too.
fn memory_behind(playback: &Value, queue: &Value) -> bool {
    let Some(now) = queue
        .get("currently_playing")
        .filter(|item| !item.is_null())
        .and_then(item_uri)
    else {
        return false;
    };
    playback
        .get("item")
        .filter(|item| !item.is_null())
        .and_then(item_uri)
        != Some(now)
}

/// Remembers the playing context and what is up next in it (Web API through spotify_player).
///
/// The running spotify_player answers playback from its memory, which re-reads the Web API only
/// every refresh interval ([`crate::warm::REFRESH_MS`]) and after its own commands, so a context
/// switched in Spotify.app (or by AppleScript) moments ago is not in it yet. When that memory is
/// behind the queue ([`memory_behind`]), the context comes from a one-shot Web API read instead
/// (the memory's, when that read fails).
pub fn capture_resume(daemon: &Arc<Daemon>, settings: &Settings) {
    let Ok(player) = settings.authed_player() else {
        return;
    };
    let mut playback = player
        .json(&["get", "key", "playback"])
        .unwrap_or(Value::Null);
    let queue = player.json(&["get", "key", "queue"]).unwrap_or(Value::Null);
    if memory_behind(&playback, &queue)
        && let Ok(fresh) = player.fresh_json(&["get", "key", "playback"], FRESH_READ_TIMEOUT)
        && !fresh.is_null()
    {
        playback = fresh;
    }
    let context = playback
        .pointer("/context/uri")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let uri_of = |item: &Value| item_uri(item);
    // The upcoming list can start with the item playing now; the resume point is the one after it.
    let current = queue
        .get("currently_playing")
        .and_then(uri_of)
        .or_else(|| playback.get("item").and_then(uri_of));
    let managed: Vec<String> = daemon
        .live()
        .queue
        .items
        .iter()
        .map(|i| i.uri.clone())
        .collect();
    let next = queue
        .get("queue")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(uri_of)
        .find(|uri| Some(uri) != current.as_ref() && !managed.contains(uri));
    let mut live = daemon.live();
    live.queue.set_resume(context, next, now_ms());
    daemon.save_queue(&live);
}

/// A Web API item's URI: its `uri`, else `spotify:<type>:<id>` (episodes in the queue come
/// without a `uri`).
#[must_use]
pub fn item_uri(item: &Value) -> Option<String> {
    item.get("uri")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            let id = item.get("id").and_then(Value::as_str)?;
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("track");
            Some(format!("spotify:{kind}:{id}"))
        })
}

/// What `spotify next` did with the managed queue.
pub enum Advance {
    /// Handed over to a managed item; the reply.
    Played(Value),
    /// Nothing managed to play. `skipped` is a hand-off still in flight that this `next`
    /// skipped (it was sent, so Spotify is switching to it).
    Nothing {
        /// The skipped item's URI.
        skipped: Option<String>,
    },
}

/// `spotify next`: hands over to the managed queue's next item now. The caller holds
/// [`Daemon::hand_off`], so concurrent `next`s claim different items (see
/// [`crate::queue::QueueState::claim_next`]).
///
/// # Errors
/// AppleScript errors.
pub fn advance_queue(daemon: &Arc<Daemon>, settings: &Settings) -> Result<Advance> {
    let needs_resume = {
        let live = daemon.live();
        if live.queue.items.is_empty() {
            return Ok(Advance::Nothing { skipped: None });
        }
        live.queue.pending.is_none()
            && live.queue.managed_now.is_none()
            && live.queue.resume.is_none()
    };
    if needs_resume {
        capture_resume(daemon, settings);
    }
    let (item, notes, remaining) = {
        let mut live = daemon.live();
        let mut notes = Vec::new();
        let item = live.queue.claim_next(now_ms(), &mut notes);
        daemon.save_queue(&live);
        (item, notes, live.queue.items.len().saturating_sub(1))
    };
    let mut skipped = None;
    for note in notes {
        match note {
            Note::Skipped(uri) => {
                log!("queue: skipped {uri} before it started (next)");
                skipped = Some(uri);
            }
            Note::Failed { uri, attempts } => {
                log!("queue: {uri} did not start (attempt {attempts}); retrying");
            }
            Note::Dropped(uri) => log!("queue: dropped {uri} after repeated failed hand-offs"),
            Note::Started(_) => {}
        }
    }
    let Some(item) = item else {
        return Ok(Advance::Nothing { skipped });
    };
    if let Err(error) = daemon
        .script
        .run(&applescript::play_uri(&item.uri, None))
        .and_then(|o| applescript::expect_ok(&o))
    {
        let mut live = daemon.live();
        live.queue.hand_off_failed();
        daemon.save_queue(&live);
        return Err(error);
    }
    daemon.nudge.notify_one();
    log!("queue: playing {} (next)", item.uri);
    Ok(Advance::Played(
        json!({"action": "next", "via": "applescript", "source": "managed_queue", "playing": item, "queue_remaining": remaining, "skipped": skipped}),
    ))
}

/// Waits (up to `within`) until Spotify.app shows `uri` as the current item.
pub fn wait_until_current(daemon: &Daemon, uri: &str, within: Duration) {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        let current = daemon
            .read()
            .ok()
            .and_then(|p| p.track)
            .is_some_and(|t| t.uri == uri);
        if current {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Plays the resume point `decide` chose from the reading taken at `decided_ms`, unless an
/// explicit command (`play`, `previous`, `next`) ran since: that one is the user's and wins.
fn resume_context(daemon: &Arc<Daemon>, resume: &Resume, decided_ms: u64) {
    // Serialized with `spotify next` and hand-offs, so a resume never lands after (and over)
    // one of them.
    let _serial = daemon
        .hand_off
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // `decide` resumes only while no hold is open, so a hold that is open now began after it.
    if daemon.live().queue.hold_until_ms > decided_ms {
        log!("queue: drained, but an explicit command ran meanwhile; not resuming");
        return;
    }
    let script = match (&resume.next_uri, &resume.context_uri) {
        (Some(next), Some(context)) => applescript::play_uri(next, Some(context)),
        (Some(next), None) => applescript::play_uri(next, None),
        (None, Some(context)) => applescript::play_uri(context, None),
        (None, None) => return,
    };
    match daemon
        .script
        .run(&script)
        .and_then(|o| applescript::expect_ok(&o))
    {
        Ok(()) => log!(
            "queue: drained; resumed {:?} in {:?}",
            resume.next_uri,
            resume.context_uri
        ),
        Err(error) => log!("queue: drained but could not resume the previous context: {error}"),
    }
    let mut live = daemon.live();
    // The resume is our own change: do not treat its start as something to override.
    live.queue.hold(now_ms());
    daemon.save_queue(&live);
    daemon.nudge.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: &str) -> Value {
        json!({"id": id, "type": kind, "name": id})
    }

    #[test]
    fn the_resume_context_is_read_afresh_only_when_spotify_players_memory_is_behind() {
        let a = item("1PVeB2mHmWwdB9YHm0yeIZ", "track");
        let b = item("43Xb3G04Pgp63fNhQW4T6W", "track");
        let memory = |item: &Value| json!({"item": item, "context": {"uri": "spotify:album:37fimO5ahI9qtvEN7OqlME", "type": "album"}});
        let queue = |item: &Value| json!({"currently_playing": item, "queue": [item]});
        // The memory names what the Web API plays now: its context is used as is.
        assert!(!memory_behind(&memory(&a), &queue(&a)));
        // Spotify.app moved on (another list, a skip) since the last refresh.
        assert!(memory_behind(&memory(&a), &queue(&b)));
        assert!(memory_behind(&json!({"item": null}), &queue(&b)));
        assert!(memory_behind(&Value::Null, &queue(&b)));
        // An episode and a track with the same id are different items.
        let episode = item("1PVeB2mHmWwdB9YHm0yeIZ", "episode");
        assert!(memory_behind(&memory(&a), &queue(&episode)));
        // Without the Web API's answer there is nothing to compare with.
        assert!(!memory_behind(&memory(&a), &Value::Null));
        assert!(!memory_behind(
            &memory(&a),
            &json!({"currently_playing": null})
        ));
    }
}
