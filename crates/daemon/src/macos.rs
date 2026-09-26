//! macOS bridge (FFI; this module is the only place unsafe is allowed): in-process AppleScript on the main thread and Spotify's playback notifications.
//!
//! - `NSAppleScript` must be used from the main thread. Every script is compiled once, cached,
//!   and executed on the main queue; callers on other threads block until it finishes. Scripts
//!   carry `with timeout`, so a frozen Spotify cannot wedge the main thread for long.
//! - Spotify posts `com.spotify.client.PlaybackStateChanged` (distributed notification) on
//!   play, pause and track changes (not on seeks). The observer only nudges the watcher, which
//!   then takes a fresh AppleScript reading.
//! - macOS asks the user once (per signed binary) whether spotify-daemon may control Spotify.
//!   The watcher's first reading raises that dialog. While it is open macOS holds every Apple
//!   Event to Spotify, from any app, so scripts time out. The permission state is derived from
//!   what Apple Events actually do ([`automation_state`]): `AEDeterminePermissionToAutomateTarget`
//!   is not used because it never returns for Spotify on current macOS. After a timeout, scripts
//!   fail fast for [`BACKOFF`] instead of stacking up behind the main thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread as _, msg_send};
use objc2_foundation::{
    NSAppleEventDescriptor, NSAppleScript, NSAppleScriptErrorMessage, NSAppleScriptErrorNumber,
    NSDate, NSDefaultRunLoopMode, NSDictionary, NSDistributedNotificationCenter, NSNotification,
    NSNumber, NSRunLoop, NSString,
};
use silicon_spotify_client::applescript::{Runner, classify};
use silicon_spotify_client::{Error, Result};

thread_local! {
    static COMPILED: RefCell<HashMap<String, Retained<NSAppleScript>>> = RefCell::new(HashMap::new());
}

/// Runs AppleScript in-process on the main thread.
pub struct MainThreadScript;

impl Runner for MainThreadScript {
    fn run(&self, source: &str) -> Result<String> {
        if let Some(error) = backing_off() {
            return Err(error);
        }
        let mut result: Option<Result<String>> = None;
        let slot = &mut result;
        let source = source.to_owned();
        DispatchQueue::main().exec_sync(move || {
            // Callers that queued behind a script which then timed out fail fast too, and
            // outcomes are recorded in the order the main thread ran them.
            *slot = Some(backing_off().map_or_else(
                || {
                    let result = run_on_main(&source);
                    record(&result);
                    result
                },
                Err,
            ));
        });
        let result = result.unwrap_or_else(|| {
            Err(Error::internal(
                "the main thread did not run the AppleScript",
            ))
        });
        result.map_err(explain_timeout)
    }
}

/// Whether this process may send Apple Events to Spotify, as the last one showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Automation {
    /// The last Apple Event to Spotify succeeded.
    Granted,
    /// macOS refused it (System Settings → Privacy & Security → Automation).
    Denied,
    /// Spotify did not answer: usually macOS's consent dialog is open, or Spotify is busy.
    NotAnswering,
    /// No answer is known: no Apple Event reached Spotify yet (not running, or nothing asked),
    /// or the Spotify that stopped answering has quit.
    Unknown,
}

impl Automation {
    /// Stable name used in `daemon.status` and `doctor`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::NotAnswering => "not_answering",
            Self::Unknown => "unknown",
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Granted,
            2 => Self::Denied,
            3 => Self::NotAnswering,
            _ => Self::Unknown,
        }
    }

    const fn to_u8(self) -> u8 {
        match self {
            Self::Granted => 1,
            Self::Denied => 2,
            Self::NotAnswering => 3,
            Self::Unknown => 0,
        }
    }
}

/// After a timeout, scripts fail fast for this long before Spotify is tried again.
pub const BACKOFF: Duration = Duration::from_secs(3);

static STATE: AtomicU8 = AtomicU8::new(0);
static LAST_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    // +1 so that 0 keeps meaning "never".
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX - 1) + 1
}

/// The permission state as the last Apple Event to Spotify showed it.
#[must_use]
pub fn automation_state() -> Automation {
    Automation::from_u8(STATE.load(Ordering::Relaxed))
}

fn record(result: &Result<String>) {
    let state = match result {
        // The script found Spotify closed and sent it nothing, which says nothing about the
        // permission. Silence from a Spotify that has since quit no longer means anything either.
        Ok(output) if output == "not_running" => {
            let _ = STATE.compare_exchange(
                Automation::NotAnswering.to_u8(),
                Automation::Unknown.to_u8(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            return;
        }
        Ok(_) => Automation::Granted,
        Err(e) if e.code == "automation_permission_denied" => Automation::Denied,
        Err(e) if e.code == "timeout" => {
            LAST_TIMEOUT_MS.store(now_ms(), Ordering::Relaxed);
            Automation::NotAnswering
        }
        Err(_) => return,
    };
    STATE.store(state.to_u8(), Ordering::Relaxed);
}

/// How much longer scripts fail fast after Spotify's last timeout (`None` when they do not).
#[must_use]
pub fn backoff_remaining() -> Option<Duration> {
    if automation_state() != Automation::NotAnswering {
        return None;
    }
    backoff_left(LAST_TIMEOUT_MS.load(Ordering::Relaxed), now_ms())
}

fn backoff_left(last_timeout_ms: u64, now_ms: u64) -> Option<Duration> {
    if last_timeout_ms == 0 {
        return None;
    }
    let since = Duration::from_millis(now_ms.saturating_sub(last_timeout_ms));
    BACKOFF.checked_sub(since).filter(|left| !left.is_zero())
}

/// Spotify timed out moments ago: fail fast rather than block the main thread again.
fn backing_off() -> Option<Error> {
    backoff_remaining().map(|_| explain_timeout(classify("", Some(-1712))))
}

/// A timeout from Spotify usually means macOS is waiting for someone to answer its Automation
/// dialog; say so.
fn explain_timeout(error: Error) -> Error {
    if error.code != "timeout" {
        return error;
    }
    Error {
        message: "Spotify.app did not answer the Apple Event in time.".into(),
        hint: "If macOS shows \"spotify-daemon\" wants access to control \"Spotify\", click Allow: until someone answers, macOS holds every Apple Event to Spotify. Otherwise Spotify is busy; retry in a moment. `spotify doctor` re-checks.".into(),
        ..error
    }
}

fn error_from(info: Option<&NSDictionary<NSString, AnyObject>>) -> Error {
    let Some(info) = info else {
        return Error::new(
            "applescript_failed",
            "AppleScript failed without error information.",
            "Retry.",
        );
    };
    // SAFETY: the keys are Foundation-provided constant strings.
    let (number_key, message_key) =
        unsafe { (NSAppleScriptErrorNumber, NSAppleScriptErrorMessage) };
    let number = info
        .objectForKey(number_key)
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .map(|n| i64::from(n.intValue()));
    let message = info
        .objectForKey(message_key)
        .and_then(|value| value.downcast::<NSString>().ok())
        .map(|s| s.to_string())
        .unwrap_or_default();
    classify(&message, number)
}

fn run_on_main(source: &str) -> Result<String> {
    COMPILED.with(|cache| {
        let cached = cache.borrow().get(source).cloned();
        let script = if let Some(script) = cached {
            script
        } else {
            let text = NSString::from_str(source);
            let script = NSAppleScript::initWithSource(NSAppleScript::alloc(), &text)
                .ok_or_else(|| Error::internal("NSAppleScript refused the script source"))?;
            let mut info: Option<Retained<NSDictionary<NSString, AnyObject>>> = None;
            // SAFETY: the out-parameter has the documented dictionary type.
            let compiled = unsafe { script.compileAndReturnError(Some(&mut info)) };
            if !compiled {
                return Err(error_from(info.as_deref()));
            }
            let mut cache = cache.borrow_mut();
            if cache.len() > 256 {
                cache.clear();
            }
            cache.insert(source.to_owned(), script.clone());
            script
        };
        let mut info: Option<Retained<NSDictionary<NSString, AnyObject>>> = None;
        // SAFETY: `executeAndReturnError:` returns nil on failure (despite its nonnull
        // annotation), so it is called with an optional return type; the out-parameter has the
        // documented dictionary type.
        let descriptor: Option<Retained<NSAppleEventDescriptor>> =
            unsafe { msg_send![&*script, executeAndReturnError: Some(&mut info)] };
        match descriptor {
            Some(descriptor) if info.is_none() => Ok(descriptor
                .stringValue()
                .map(|s| s.to_string())
                .unwrap_or_default()),
            _ => Err(error_from(info.as_deref())),
        }
    })
}

/// Subscribes to Spotify's playback notifications; `nudge` is called on each one.
/// Must be called on the main thread before [`run_main_loop`].
pub fn observe_spotify(nudge: impl Fn() + 'static) {
    let center = NSDistributedNotificationCenter::defaultCenter();
    let name = NSString::from_str("com.spotify.client.PlaybackStateChanged");
    let block = RcBlock::new(move |_notification: NonNull<NSNotification>| nudge());
    // SAFETY: the block is 'static and only calls the nudge closure; no queue means it runs on
    // the posting (main) thread's run loop.
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(Some(&name), None, None, &block)
    };
    // The observer lives for the whole process.
    std::mem::forget(token);
}

/// Runs the main run loop forever (services distributed notifications and main-queue work).
pub fn run_main_loop() -> ! {
    let run_loop = NSRunLoop::mainRunLoop();
    loop {
        let date = NSDate::dateWithTimeIntervalSinceNow(60.0);
        // SAFETY: NSDefaultRunLoopMode is a Foundation constant.
        let ran = unsafe { run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &date) };
        if !ran {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_name_the_consent_dialog_and_keep_their_code() {
        let explained = explain_timeout(classify("", Some(-1712)));
        assert_eq!(explained.code, "timeout");
        assert!(explained.retryable);
        assert!(explained.hint.contains("click Allow"));
        let denied = explain_timeout(classify("", Some(-1743)));
        assert_eq!(denied.code, "automation_permission_denied");
    }

    #[test]
    fn outcomes_set_the_permission_state() {
        record(&Ok(String::new()));
        assert_eq!(automation_state(), Automation::Granted);
        assert!(backing_off().is_none());
        record(&Err(classify("", Some(-1712))));
        assert_eq!(automation_state(), Automation::NotAnswering);
        assert_eq!(backing_off().map(|e| e.code), Some("timeout".to_owned()));
        assert!(backoff_remaining().is_some_and(|left| left <= BACKOFF));
        record(&Err(classify("", Some(-1743))));
        assert_eq!(automation_state(), Automation::Denied);
        assert!(backing_off().is_none());
        // Spotify closed: no Apple Event was sent, so a known answer stands...
        record(&Ok("not_running".into()));
        assert_eq!(automation_state(), Automation::Denied);
        record(&Ok(String::new()));
        record(&Ok("not_running".into()));
        assert_eq!(automation_state(), Automation::Granted);
        // ...and silence from the Spotify that quit is forgotten, lifting the backoff.
        record(&Err(classify("", Some(-1712))));
        record(&Ok("not_running".into()));
        assert_eq!(automation_state(), Automation::Unknown);
        assert!(backing_off().is_none());
        // Errors that say nothing about the permission leave it alone.
        record(&Err(classify("", Some(-600))));
        assert_eq!(automation_state(), Automation::Unknown);
    }

    #[test]
    fn the_backoff_lifts() {
        assert_eq!(backoff_left(0, 5_000), None);
        assert_eq!(backoff_left(5_000, 5_000), Some(BACKOFF));
        assert_eq!(
            backoff_left(5_000, 6_000),
            Some(BACKOFF - Duration::from_secs(1))
        );
        let backoff_ms = u64::try_from(BACKOFF.as_millis()).expect("ms");
        assert_eq!(backoff_left(5_000, 5_000 + backoff_ms), None);
        assert_eq!(backoff_left(5_000, 60_000), None);
    }
}
