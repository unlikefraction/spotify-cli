//! The watcher: reads Spotify.app, tracks plays, fires triggers and drives the managed queue.
//!
//! Readings happen when Spotify posts a playback notification, when a CLI command changed
//! something, and on a timer whose interval adapts: every 2 s while playing with work to do,
//! 5 s while playing otherwise, 10 s paused, 15 s when Spotify is closed, and precisely at the
//! next trigger checkpoint or queue hand-off. Every reading (also the one `trigger add` takes) goes
//! through [`observe_now`], which serializes read-and-process so readings are never applied out
//! of order.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use silicon_spotify_client::applescript;
use silicon_spotify_client::model::{Playback, PlayerState, now_ms, now_rfc3339};
use silicon_spotify_client::trigger::{self, Observation, Status};
use silicon_spotify_client::{Error, Result};

use crate::db::FiringRow;
use crate::log;
use crate::queue::{Action, Note, Reading, Resume};
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
        }
    }
    daemon.save_queue(&live);

    // Next wake.
    let busy = !live.triggers.is_empty()
        || !live.queue.items.is_empty()
        || live.queue.managed_now.is_some();
    let mut wait = match playback.state {
        PlayerState::Playing if busy => 2_000,
        PlayerState::Playing => 5_000,
        PlayerState::Paused | PlayerState::Stopped => 10_000,
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
        Action::Play(uri) => {
            send_hand_off(daemon, &uri, "end of track");
            return Ok(Duration::from_millis(250));
        }
        Action::Resume(resume) => {
            resume_context(daemon, &resume);
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

/// Sends `play track <uri>` for a pending hand-off; a send failure counts as a failed attempt.
fn send_hand_off(daemon: &Arc<Daemon>, uri: &str, why: &str) {
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

/// Remembers the playing context and what is up next in it (Web API through spotify_player).
pub fn capture_resume(daemon: &Arc<Daemon>, settings: &Settings) {
    let Ok(player) = settings.authed_player() else {
        return;
    };
    let playback = player
        .json(&["get", "key", "playback"])
        .unwrap_or(Value::Null);
    let queue = player.json(&["get", "key", "queue"]).unwrap_or(Value::Null);
    let context = playback
        .pointer("/context/uri")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let uri_of = |item: &Value| {
        item.get("uri")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                item.get("id")
                    .and_then(Value::as_str)
                    .map(|id| format!("spotify:track:{id}"))
            })
    };
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

/// `spotify next` with managed items: hand over to the head now.
///
/// # Errors
/// `queue_empty` or AppleScript errors.
pub fn advance_queue(daemon: &Arc<Daemon>, settings: &Settings) -> Result<Value> {
    let needs_resume = {
        let live = daemon.live();
        if live.queue.items.is_empty() {
            return Err(Error::new(
                "queue_empty",
                "The managed queue is empty.",
                "Add items with `spotify queue add <uri>`.",
            ));
        }
        live.queue.managed_now.is_none() && live.queue.resume.is_none()
    };
    if needs_resume {
        capture_resume(daemon, settings);
    }
    let (uri, item) = {
        let mut live = daemon.live();
        let item = live.queue.items.first().cloned();
        let uri = live.queue.begin_hand_off(now_ms());
        daemon.save_queue(&live);
        (uri, item)
    };
    let uri = uri.ok_or_else(|| {
        Error::new(
            "queue_empty",
            "The managed queue is empty.",
            "Add items with `spotify queue add <uri>`.",
        )
    })?;
    if let Err(error) = daemon
        .script
        .run(&applescript::play_uri(&uri, None))
        .and_then(|o| applescript::expect_ok(&o))
    {
        let mut live = daemon.live();
        live.queue.hand_off_failed();
        daemon.save_queue(&live);
        return Err(error);
    }
    daemon.nudge.notify_one();
    log!("queue: playing {uri} (next)");
    let remaining = daemon.live().queue.items.len().saturating_sub(1);
    Ok(
        json!({"action": "next", "via": "applescript", "source": "managed_queue", "playing": item, "queue_remaining": remaining}),
    )
}

fn resume_context(daemon: &Arc<Daemon>, resume: &Resume) {
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
