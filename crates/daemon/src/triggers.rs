//! Trigger requests and the Ting delivery outbox.
//!
//! Custody: the daemon holds no credentials of its own. To deliver, it opens the Silicon's own
//! session store (`$SILICON_HOME/.spotify/session.json`) under the same lock the CLI uses,
//! refreshes when needed, and asks the backend to send the Ting with that Silicon's token.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use silicon_spotify_client::api::{Api, TingDelivery};
use silicon_spotify_client::ipc::Request;
use silicon_spotify_client::model::{now_ms, now_rfc3339};
use silicon_spotify_client::store::{self, Home};
use silicon_spotify_client::trigger::{Condition, ScopeRequest, Status, Trigger};
use silicon_spotify_client::{Error, Result};

use crate::db::{Delivery, FiringRow, StoredTrigger};
use crate::log;
use crate::service::{Daemon, args, blocking};

/// Give up on a delivery after a day of retries (Ting keeps idempotency keys 14 days).
/// Stop retrying after an hour: a playback checkpoint notification older than that is stale.
/// (Ting keeps idempotency keys for 14 days, so retries within the window never duplicate.)
const GIVE_UP_MS: i64 = 60 * 60 * 1000;

#[derive(Deserialize)]
struct Target {
    api_url: String,
    slot: String,
    #[serde(default)]
    testing: bool,
    #[serde(default)]
    org: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ScopeArg {
    Current,
    Every,
    Track(String),
}

#[derive(Deserialize)]
struct AddArgs {
    condition: Condition,
    scope: ScopeArg,
    #[serde(default)]
    times: Option<u32>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default = "yes")]
    notify_expiry: bool,
    #[serde(default = "yes")]
    ting: bool,
    target: Target,
    #[serde(default)]
    isi: Option<String>,
}

fn yes() -> bool {
    true
}

fn home_of(request: &Request) -> Result<Home> {
    let home = request.home.clone().ok_or_else(|| {
        Error::invalid(
            "This op needs the caller's home.",
            "Update the CLI and daemon to the same version.",
        )
    })?;
    let path = PathBuf::from(&home);
    if !path.is_absolute() || !path.is_dir() {
        return Err(Error::invalid(
            format!("Home {home} does not exist."),
            "Check SILICON_HOME.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let owner = std::fs::metadata(&path)
            .map(|m| m.uid())
            .unwrap_or(u32::MAX);
        if owner != silicon_spotify_client::ipc::current_uid() {
            return Err(Error::new(
                "permission_denied",
                format!("{home} is owned by another user."),
                "Each OS user runs their own daemon.",
            ));
        }
    }
    Ok(Home::from_root(path))
}

/// Dispatches `trigger.*` ops.
pub async fn handle(daemon: &Arc<Daemon>, request: &Request) -> Option<Result<Value>> {
    let result = match request.op.as_str() {
        "trigger.add" => add(daemon, request).await,
        "trigger.list" => list(daemon, request),
        "trigger.get" => get(daemon, request),
        "trigger.remove" => remove(daemon, request, false),
        "trigger.clear" => remove(daemon, request, true),
        "trigger.history" => history(daemon, request),
        "trigger.test" => test(daemon, request).await,
        "trigger.wait" => wait(daemon, request).await,
        "trigger.retry" => retry(daemon, request),
        _ => return None,
    };
    Some(result)
}

async fn add(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    let a: AddArgs = args(request)?;
    // A bad spec is reported as such before anything is read, even when Spotify cannot be.
    let scope = checked_scope(&a)?;
    let home = home_of(request)?;
    let home_key = home.key();
    // Who will be notified, and can they be?
    let (recipient, org, ting_state) = {
        let sessions = home.sessions()?;
        match sessions.slots.get(&a.target.slot) {
            Some(slot) => (
                Some(slot.session.actor.public_id.clone()),
                a.target
                    .org
                    .clone()
                    .or_else(|| Some(slot.session.org_id.clone())),
                slot.session.ting.clone(),
            ),
            None if a.ting => {
                return Err(Error::not_authenticated(format!(
                    "Triggers notify you through Ting, which needs a spotify-cli login, and {} has none for {}.",
                    home.dir.display(),
                    a.target.api_url
                ))
                .with_details(json!({"alternative": "Add --local to record firings only locally (see `spotify trigger history` / `spotify trigger wait`)."})));
            }
            None => (None, a.target.org.clone(), None),
        }
    };
    if a.ting && ting_state.as_ref().is_some_and(|t| !t.subscribed) {
        let reason = ting_state.and_then(|t| t.error);
        return Err(Error::new(
            "recipient_not_registered",
            "This Silicon is not registered as a Ting recipient for spotify-cli, so trigger notifications would be refused.",
            "Run `spotify ting register` (re-registers with your current session). If it keeps failing, log in again: `spotify login '<SLT>'`.",
        )
        .with_details(json!({"registration_error": reason})));
    }
    // Fresh reading so `current` binds to what is playing right now.
    // It goes through the watcher's own path, so play boundaries since the last reading (the old
    // track ending, a new one starting) are evaluated for existing triggers and the queue too.
    let d = Arc::clone(daemon);
    blocking(move || crate::watcher::observe_now(&d).map(drop)).await?;
    let id = format!("trg_{}", &uuid::Uuid::now_v7().simple().to_string()[16..]);
    let trigger = {
        let live = daemon.live();
        #[allow(clippy::too_many_arguments)]
        Trigger::create(
            id,
            a.condition,
            scope,
            a.times,
            a.note,
            a.label,
            a.notify_expiry,
            &live.tracker,
        )?
    };
    let delivery = Delivery {
        home: home_key,
        api_url: a.target.api_url,
        slot: a.target.slot,
        testing: a.target.testing,
        ting: a.ting,
        recipient,
        org,
        isi: a.isi.or_else(|| request.isi.clone()),
    };
    let stored = StoredTrigger {
        trigger: trigger.clone(),
        delivery,
    };
    daemon.db.save_trigger(&stored)?;
    let current = {
        let mut live = daemon.live();
        live.triggers.push(stored.clone());
        live.last.clone()
    };
    daemon.nudge.notify_one();
    log!(
        "trigger {} added: {} ({})",
        trigger.id,
        trigger.condition.describe(),
        trigger.scope.name()
    );
    Ok(json!({"trigger": describe(&stored), "now_playing": current.and_then(|p| p.track)}))
}

/// The requested scope, after every check that needs no playback (times, note, label,
/// percentages, the track URI). Binding `current` to the playing item comes after the reading.
fn checked_scope(a: &AddArgs) -> Result<ScopeRequest> {
    silicon_spotify_client::trigger::validate_request(
        a.condition,
        a.times,
        a.note.as_deref(),
        a.label.as_deref(),
    )?;
    Ok(match &a.scope {
        ScopeArg::Current => ScopeRequest::Current,
        ScopeArg::Every => ScopeRequest::Every,
        ScopeArg::Track(uri) => {
            use silicon_spotify_client::uri::{Kind, SpotifyUri};
            let uri = SpotifyUri::parse(uri, Some(Kind::Track))?;
            // Only a playing item's own URI can match: an album or playlist never would.
            if !matches!(uri.kind, Kind::Track | Kind::Episode) {
                return Err(Error::invalid(
                    format!(
                        "--track needs a track or episode; {} is not one.",
                        uri.uri()
                    ),
                    "Example: spotify trigger add --end --scope track --track spotify:track:<id>",
                ));
            }
            ScopeRequest::Track(uri.uri())
        }
    })
}

fn describe(stored: &StoredTrigger) -> Value {
    let t = &stored.trigger;
    json!({
        "id": t.id,
        "label": t.label,
        "condition": t.condition.name(),
        "description": t.condition.describe(),
        "spec": t.condition,
        "scope": t.scope,
        "times": t.times,
        "fired": t.fired,
        "status": t.status,
        "note": t.note,
        "notify_expiry": t.notify_expiry,
        "delivery": {
            "ting": stored.delivery.ting,
            "recipient": stored.delivery.recipient,
            "org": stored.delivery.org,
            "isi": stored.delivery.isi,
            "testing": stored.delivery.testing,
        },
        "created_at": t.created_at,
        "last_fired_at": t.last_fired_at,
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListArgs {
    all: bool,
    everyone: bool,
    id: Option<String>,
    limit: Option<i64>,
    timeout_ms: Option<u64>,
    firing: Option<String>,
    target: Option<Value>,
}

fn list(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    let a: ListArgs = args(request)?;
    let home = if a.everyone {
        None
    } else {
        Some(home_of(request)?.key())
    };
    let triggers = daemon.db.triggers(!a.all, home.as_deref())?;
    Ok(json!({"triggers": triggers.iter().map(describe).collect::<Vec<_>>()}))
}

fn get(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    let a: ListArgs = args(request)?;
    let id = a.id.ok_or_else(|| {
        Error::invalid(
            "Give a trigger id.",
            "List them with `spotify trigger list`.",
        )
    })?;
    let stored = daemon.db.trigger(&id)?.ok_or_else(|| not_found(&id))?;
    let firings = daemon.db.history(None, Some(&id), 20)?;
    Ok(
        json!({"trigger": describe(&stored), "firings": firings.iter().map(firing_view).collect::<Vec<_>>()}),
    )
}

fn not_found(id: &str) -> Error {
    Error::not_found(
        format!("No trigger `{id}`."),
        "List triggers with `spotify trigger list --all`.",
    )
}

fn remove(daemon: &Arc<Daemon>, request: &Request, all: bool) -> Result<Value> {
    let a: ListArgs = args(request)?;
    let home = home_of(request)?.key();
    let mut removed = Vec::new();
    let targets: Vec<StoredTrigger> = if all {
        daemon.db.triggers(true, Some(&home))?
    } else {
        let id = a.id.ok_or_else(|| {
            Error::invalid(
                "Give a trigger id.",
                "List them with `spotify trigger list`.",
            )
        })?;
        let stored = daemon.db.trigger(&id)?.ok_or_else(|| not_found(&id))?;
        if stored.delivery.home != home {
            return Err(Error::new(
                "permission_denied",
                format!("Trigger {id} belongs to another Silicon home."),
                "Only the home that created a trigger can remove it.",
            ));
        }
        vec![stored]
    };
    {
        // Under the live lock, so a concurrent watcher pass cannot re-save it as active.
        let mut live = daemon.live();
        for stored in targets {
            let mut current = live
                .triggers
                .iter()
                .find(|s| s.trigger.id == stored.trigger.id)
                .cloned()
                .unwrap_or(stored);
            if current.trigger.status == Status::Active {
                current.trigger.status = Status::Removed;
                daemon.db.save_trigger(&current)?;
            }
            removed.push(current.trigger.id.clone());
        }
        live.triggers.retain(|s| !removed.contains(&s.trigger.id));
    }
    for id in &removed {
        let _ = daemon
            .events
            .send(json!({"trigger": id, "status": "removed"}));
    }
    Ok(json!({"removed": removed}))
}

/// A firing without internal fields.
#[must_use]
pub fn firing_view(row: &FiringRow) -> Value {
    json!({
        "id": row.id,
        "trigger_id": row.trigger_id,
        "outcome": row.outcome,
        "type": row.ting_type,
        "state": row.state,
        "attempts": row.attempts,
        "created_at": row.created_at,
        "sent_at": row.sent_at,
        "ting": row.ting,
        "last_error": row.last_error,
        "data": row.data,
    })
}

fn history(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    let a: ListArgs = args(request)?;
    let home = if a.everyone {
        None
    } else {
        Some(home_of(request)?.key())
    };
    let rows = daemon.db.history(
        home.as_deref(),
        a.id.as_deref(),
        match a.limit.unwrap_or(20) {
            limit @ 1..=500 => limit,
            other => {
                return Err(Error::invalid(
                    format!("limit must be from 1 to 500, not {other}."),
                    "Example: spotify trigger history --limit 50",
                ));
            }
        },
    )?;
    Ok(json!({"firings": rows.iter().map(firing_view).collect::<Vec<_>>()}))
}

fn retry(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    let a: ListArgs = args(request)?;
    let id = a.firing.ok_or_else(|| {
        Error::invalid(
            "Give a firing id.",
            "See ids with `spotify trigger history`.",
        )
    })?;
    let mut row = daemon.db.firing(&id)?.ok_or_else(|| {
        Error::not_found(
            format!("No firing `{id}`."),
            "See `spotify trigger history`.",
        )
    })?;
    if row.state == "sent" {
        return Ok(json!({"firing": firing_view(&row), "note": "Already delivered."}));
    }
    row.state = "pending".into();
    row.next_attempt_ms = i64::try_from(now_ms()).unwrap_or(0);
    daemon.db.update_firing(&row)?;
    daemon.deliver.notify_one();
    Ok(json!({"firing": firing_view(&row), "queued": true}))
}

async fn test(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    #[derive(Deserialize)]
    struct A {
        #[serde(default)]
        id: Option<String>,
        target: Option<Target>,
        #[serde(default)]
        isi: Option<String>,
    }
    let a: A = args(request)?;
    let home = home_of(request)?;
    let (delivery, trigger_id) = match &a.id {
        Some(id) => {
            let stored = daemon.db.trigger(id)?.ok_or_else(|| not_found(id))?;
            (
                Delivery {
                    ting: true,
                    ..stored.delivery
                },
                id.clone(),
            )
        }
        None => {
            let target = a
                .target
                .ok_or_else(|| Error::invalid("Missing delivery target.", "Update the CLI."))?;
            let sessions = home.sessions()?;
            let slot = sessions.slots.get(&target.slot).ok_or_else(|| {
                Error::not_authenticated(format!(
                    "{} has no spotify-cli login for {}.",
                    home.dir.display(),
                    target.api_url
                ))
            })?;
            (
                Delivery {
                    home: home.key(),
                    api_url: target.api_url,
                    slot: target.slot,
                    testing: target.testing,
                    ting: true,
                    recipient: Some(slot.session.actor.public_id.clone()),
                    org: target.org.or_else(|| Some(slot.session.org_id.clone())),
                    isi: a.isi.or_else(|| request.isi.clone()),
                },
                "trg_test".to_owned(),
            )
        }
    };
    let d = Arc::clone(daemon);
    let now_playing = blocking(move || d.read()).await.ok().and_then(|p| p.track);
    let firing_id = format!("fir_{}", uuid::Uuid::now_v7().simple());
    let row = FiringRow {
        id: firing_id.clone(),
        trigger_id: trigger_id.clone(),
        home: delivery.home.clone(),
        outcome: "test".into(),
        ting_type: "spotify.trigger.fired".into(),
        key_suffix: format!("{trigger_id}/test/{firing_id}"),
        data: json!({
            "outcome": "test",
            "test": true,
            "trigger": {"id": trigger_id, "description": "test notification from `spotify trigger test`"},
            "track": now_playing,
            "at": now_rfc3339(),
        }),
        metadata: crate::watcher::metadata(&delivery),
        delivery,
        state: "pending".into(),
        attempts: 0,
        next_attempt_ms: 0,
        last_error: None,
        ting: None,
        created_at: now_rfc3339(),
        sent_at: None,
    };
    let mut events = daemon.events.subscribe();
    daemon.db.insert_firing(&row)?;
    daemon.deliver.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let current = daemon
            .db
            .firing(&firing_id)?
            .ok_or_else(|| Error::internal("test firing vanished"))?;
        if current.state != "pending" || (current.attempts > 0 && current.last_error.is_some()) {
            let delivered = current.state == "sent";
            let view = firing_view(&current);
            if delivered {
                return Ok(json!({"delivered": true, "firing": view}));
            }
            let error: Option<Error> = current
                .last_error
                .clone()
                .and_then(|e| serde_json::from_value(e).ok());
            return Err(error
                .unwrap_or_else(|| Error::internal("delivery failed without a reason"))
                .with_details(json!({"firing": view})));
        }
        tokio::select! {
            _ = events.recv() => {}
            () = tokio::time::sleep_until(deadline) => {
                return Err(Error::new("timeout", "The test notification was not confirmed within 30 s; it stays queued.", "Check `spotify trigger history` in a minute.").retryable());
            }
        }
    }
}

async fn wait(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    let a: ListArgs = args(request)?;
    let id = a.id.ok_or_else(|| {
        Error::invalid(
            "Give a trigger id.",
            "spotify trigger wait <id> [--timeout 10m]",
        )
    })?;
    let initial = daemon.db.trigger(&id)?.ok_or_else(|| not_found(&id))?;
    let since = now_rfc3339();
    let was_active = initial.trigger.status == Status::Active;
    let mut events = daemon.events.subscribe();
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(a.timeout_ms.unwrap_or(3_600_000));
    loop {
        // A firing since we started waiting; for a trigger that had already finished, its last one.
        let rows = daemon.db.history(None, Some(&id), 5)?;
        if let Some(row) = rows.iter().find(|r| r.created_at >= since || !was_active) {
            return Ok(json!({"firing": firing_view(row)}));
        }
        // Re-read the status every pass: it may have expired silently or been removed.
        let current = daemon.db.trigger(&id)?.ok_or_else(|| not_found(&id))?;
        if current.trigger.status != Status::Active {
            if let Some(row) = daemon
                .db
                .history(None, Some(&id), 5)?
                .into_iter()
                .find(|r| r.created_at >= since)
            {
                return Ok(json!({"firing": firing_view(&row)}));
            }
            let status = serde_json::to_value(current.trigger.status)?;
            return Ok(json!({
                "firing": null,
                "trigger": describe(&current),
                "note": format!("The trigger is {} and did not fire.", status.as_str().unwrap_or("finished")),
            }));
        }
        tokio::select! {
            _ = events.recv() => {}
            () = tokio::time::sleep_until(deadline) => {
                return Err(Error::new("timeout", format!("Trigger {id} did not fire before the timeout."), "It is still active; wait again or check `spotify trigger show <id>`.").retryable());
            }
        }
    }
}

/// Delivers pending firings forever.
pub async fn deliver_forever(daemon: Arc<Daemon>) {
    loop {
        let now = i64::try_from(now_ms()).unwrap_or(0);
        match daemon.db.due_firings(now) {
            Ok(rows) => {
                for row in rows {
                    deliver_one(&daemon, row).await;
                }
            }
            Err(error) => log!("delivery: cannot read the outbox: {error}"),
        }
        let next = daemon.db.next_due().ok().flatten();
        let wait_ms = next.map_or(60_000, |at| {
            (at - i64::try_from(now_ms()).unwrap_or(0)).clamp(50, 60_000)
        });
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(u64::try_from(wait_ms).unwrap_or(60_000))) => {}
            () = daemon.deliver.notified() => {}
            () = daemon.shutdown.notified() => return,
        }
    }
}

async fn deliver_one(daemon: &Arc<Daemon>, mut row: FiringRow) {
    row.attempts += 1;
    let result = send(&row).await;
    let now = i64::try_from(now_ms()).unwrap_or(0);
    let created = time::OffsetDateTime::parse(
        &row.created_at,
        &time::format_description::well_known::Rfc3339,
    )
    .map(|t| i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap_or(now))
    .unwrap_or(now);
    match result {
        Ok(ting) => {
            row.state = "sent".into();
            row.ting = Some(ting);
            row.sent_at = Some(now_rfc3339());
            row.last_error = None;
            log!("delivery: {} sent to Ting", row.id);
        }
        Err(error) => {
            let permanent = !error.retryable
                && matches!(
                    error.code.as_str(),
                    "recipient_not_registered"
                        | "reconsent_required"
                        | "test_context_mismatch"
                        | "recipient_changed"
                        | "testing_selection_missing"
                        | "testing_selection_changed"
                        | "forbidden"
                        | "permission_denied"
                        | "invalid_input"
                        | "not_found"
                        | "idempotency_conflict"
                        | "payload_too_large"
                );
            let expired = now - created > GIVE_UP_MS;
            row.last_error = serde_json::to_value(&error).ok();
            if permanent || expired {
                row.state = "failed".into();
                log!("delivery: {} failed permanently: {error}", row.id);
            } else {
                // 10 s after the first failure, doubling, at most 10 minutes.
                let exponent = u32::try_from((row.attempts - 1).clamp(0, 8)).unwrap_or(8);
                let backoff = (10_000_i64 * 2_i64.pow(exponent)).min(10 * 60 * 1000);
                row.next_attempt_ms = now + backoff;
                log!(
                    "delivery: {} will retry in {} s: {error}",
                    row.id,
                    backoff / 1000
                );
            }
        }
    }
    if let Err(error) = daemon.db.update_firing(&row) {
        log!("delivery: cannot record the result for {}: {error}", row.id);
    }
    let _ = daemon
        .events
        .send(json!({"firing": row.id, "state": row.state}));
}

async fn send(row: &FiringRow) -> Result<Value> {
    let home = Home::from_root(PathBuf::from(&row.delivery.home));
    // Only the home's saved selection counts: the daemon's own environment must never pick a plane.
    let testing = if row.delivery.testing {
        home.testing_saved()?
    } else {
        None
    };
    if row.delivery.testing && testing.is_none() {
        return Err(Error::new(
            "testing_selection_missing",
            "This trigger was created in a testing environment, but the home no longer selects one.",
            "Re-select it with `spotify testing use --app-secret-file -`, or remove the trigger.",
        ));
    }
    if store::slot_key(&row.delivery.api_url, testing.as_ref()) != row.delivery.slot {
        return Err(Error::new(
            "testing_selection_changed",
            "The home now selects a different testing plane (or production) than when this trigger was created, so its session is not the one to use.",
            "Select the original plane again with `spotify testing use`, or remove the trigger and create it again.",
        ));
    }
    let api = Api::new(&row.delivery.api_url, "daemon")?.with_testing(testing);
    let mut slot = store::fresh_session(&home, &api, &row.delivery.slot, false).await?;
    let recipient = slot.session.actor.public_id.clone();
    if row
        .delivery
        .recipient
        .as_ref()
        .is_some_and(|r| *r != recipient)
    {
        return Err(Error::new(
            "recipient_changed",
            format!(
                "The home now holds a session for {recipient}, not {} who created the trigger.",
                row.delivery.recipient.clone().unwrap_or_default()
            ),
            "Remove the trigger and create it again from the right Silicon.",
        ));
    }
    let org = row
        .delivery
        .org
        .clone()
        .unwrap_or_else(|| slot.session.org_id.clone());
    let delivery = TingDelivery {
        event_type: row.ting_type.clone(),
        key: format!("{recipient}/{}", row.key_suffix),
        data: row.data.clone(),
        metadata: row.metadata.clone(),
    };
    match api
        .send_ting(&slot.session.access_token, &org, &delivery)
        .await
    {
        Err(error) if error.code == "not_authenticated" => {
            slot = store::fresh_session(&home, &api, &row.delivery.slot, true).await?;
            api.send_ting(&slot.session.access_token, &org, &delivery)
                .await
        }
        other => other,
    }
}

/// Relays queued telemetry to the backend every minute.
pub async fn relay_telemetry(daemon: Arc<Daemon>) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(60)) => {}
            () = daemon.shutdown.notified() => return,
        }
        let settings = daemon.settings();
        if !silicon_spotify_client::telemetry::enabled(settings.telemetry) {
            let _ = daemon.db.clear_telemetry();
            continue;
        }
        let Ok(batch) = daemon.db.peek_telemetry() else {
            continue;
        };
        if batch.is_empty() {
            continue;
        }
        let api_url = settings
            .api_url
            .unwrap_or_else(|| silicon_spotify_client::DEFAULT_API_URL.to_owned());
        let Ok(api) = Api::new(&api_url, "daemon") else {
            continue;
        };
        for (events, ids) in telemetry_chunks(batch) {
            if events.is_empty() {
                // Unreadable or oversized rows: nothing to send, just forget them.
                let _ = daemon.db.drop_telemetry(&ids);
                continue;
            }
            match api.telemetry(&events).await {
                Ok(()) => {
                    let _ = daemon.db.drop_telemetry(&ids);
                }
                Err(error) if !error.retryable => {
                    // The backend rejected the batch; drop it rather than retry forever.
                    let _ = daemon.db.drop_telemetry(&ids);
                }
                // Keep the rest for the next round.
                Err(_) => break,
            }
        }
    }
}

/// The gateway takes at most 64 KiB per request; stay under it with room for the envelope.
const TELEMETRY_BATCH_BYTES: usize = 60 * 1024;

/// Splits queued telemetry rows into requests under [`TELEMETRY_BATCH_BYTES`], each with the
/// row ids it covers. Rows that do not parse, or that alone exceed the limit, come back as a
/// chunk with no events so the caller drops them.
fn telemetry_chunks(
    batch: Vec<(String, Value)>,
) -> Vec<(Vec<silicon_spotify_client::telemetry::Event>, Vec<String>)> {
    let mut chunks = Vec::new();
    let mut unusable = Vec::new();
    let (mut events, mut ids, mut size) = (Vec::new(), Vec::new(), 0usize);
    for (id, value) in batch {
        let Ok(event) = serde_json::from_value::<silicon_spotify_client::telemetry::Event>(value)
        else {
            unusable.push(id);
            continue;
        };
        let len = serde_json::to_vec(&event).map_or(usize::MAX, |bytes| bytes.len() + 1);
        if len > TELEMETRY_BATCH_BYTES {
            unusable.push(id);
            continue;
        }
        if size + len > TELEMETRY_BATCH_BYTES && !events.is_empty() {
            chunks.push((std::mem::take(&mut events), std::mem::take(&mut ids)));
            size = 0;
        }
        events.push(event);
        ids.push(id);
        size += len;
    }
    if !events.is_empty() {
        chunks.push((events, ids));
    }
    if !unusable.is_empty() {
        chunks.push((Vec::new(), unusable));
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_args(spec: Value) -> AddArgs {
        let mut value = json!({
            "condition": {"kind": "end"},
            "scope": "every",
            "target": {"api_url": "https://api.test", "slot": "default"},
        });
        for (key, v) in spec.as_object().into_iter().flatten() {
            value[key] = v.clone();
        }
        serde_json::from_value(value).expect("add args")
    }

    fn check(spec: Value) -> Result<ScopeRequest> {
        checked_scope(&add_args(spec))
    }

    #[test]
    fn bad_specs_fail_without_playback() {
        for spec in [
            json!({"times": 0}),
            json!({"scope": "current", "times": 0}),
            json!({"note": "n".repeat(1001)}),
            json!({"label": "l".repeat(81)}),
            json!({"condition": {"kind": "elapsed", "at": {"unit": "percent", "value": 101.0}}}),
            json!({"scope": {"track": "not a track"}}),
            json!({"scope": {"track": "spotify:album:4uLU6hMCjMI75M1A2tKUQC"}}),
        ] {
            let code = check(spec.clone()).err().map(|e| e.code);
            assert_eq!(code.as_deref(), Some("invalid_input"), "{spec}");
        }
    }

    #[test]
    fn good_specs_pass_without_playback() {
        for spec in [
            json!({}),
            // Binding to the playing item is checked after the reading, not here.
            json!({"scope": "current"}),
            json!({"times": 3, "note": "n".repeat(1000), "label": "l".repeat(80)}),
            json!({"scope": {"track": "spotify:track:4uLU6hMCjMI75M1A2tKUQC"}}),
            json!({"scope": {"track": "spotify:episode:4uLU6hMCjMI75M1A2tKUQC"}}),
            json!({"condition": {"kind": "remaining", "at": {"unit": "millis", "value": 30000}}}),
        ] {
            let result = check(spec.clone());
            assert!(result.is_ok(), "{spec}: {result:?}");
        }
    }
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;

    fn event(id: &str, bytes: usize) -> (String, Value) {
        (
            id.to_owned(),
            json!({"id": id, "type": "cli.command.completed", "data": {"pad": "x".repeat(bytes)}, "metadata": {}}),
        )
    }

    #[test]
    fn telemetry_batches_stay_under_the_gateway_limit() {
        let batch: Vec<_> = (0..40).map(|i| event(&format!("e{i}"), 4_000)).collect();
        let chunks = telemetry_chunks(batch);
        assert!(chunks.len() > 1);
        let mut seen = 0;
        for (events, ids) in &chunks {
            assert_eq!(events.len(), ids.len());
            assert!(
                serde_json::to_vec(events)
                    .map(|b| b.len())
                    .unwrap_or(usize::MAX)
                    <= 64 * 1024
            );
            seen += ids.len();
        }
        assert_eq!(seen, 40);
    }

    #[test]
    fn unusable_rows_are_returned_for_dropping() {
        let batch = vec![
            event("ok", 10),
            ("bad".to_owned(), json!({"nope": true})),
            event("huge", 70_000),
        ];
        let chunks = telemetry_chunks(batch);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].1, vec!["ok".to_owned()]);
        assert!(chunks[1].0.is_empty());
        assert_eq!(chunks[1].1, vec!["bad".to_owned(), "huge".to_owned()]);
    }
}
