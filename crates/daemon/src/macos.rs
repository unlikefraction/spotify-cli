//! macOS bridge (FFI; this module is the only place unsafe is allowed): in-process AppleScript on the main thread and Spotify's playback notifications.
//!
//! - `NSAppleScript` must be used from the main thread. Every script is compiled once, cached,
//!   and executed on the main queue; callers on other threads block until it finishes. Scripts
//!   carry `with timeout`, so a frozen Spotify cannot wedge the main thread for long.
//! - Spotify posts `com.spotify.client.PlaybackStateChanged` (distributed notification) on
//!   play, pause and track changes (not on seeks). The observer only nudges the watcher, which
//!   then takes a fresh AppleScript reading.
//! - macOS asks the user once (per signed binary) whether spotify-daemon may control Spotify.
//!   While that dialog is open, Spotify answers no Apple Events from anyone, so scripts are only
//!   sent while the permission is not known to be missing; otherwise commands fail fast with
//!   `automation_permission_pending` / `automation_permission_denied`. macOS's own check blocks
//!   too, so only a background thread ([`request_automation`]) runs it; everyone else reads
//!   [`automation_state`]. That thread also raises the dialog at startup.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
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
        if let Some(error) = automation_state().blocker() {
            return Err(error);
        }
        let mut result: Option<Result<String>> = None;
        let slot = &mut result;
        let source = source.to_owned();
        DispatchQueue::main().exec_sync(move || {
            *slot = Some(run_on_main(&source));
        });
        result.unwrap_or_else(|| {
            Err(Error::internal(
                "the main thread did not run the AppleScript",
            ))
        })
    }
}

/// Whether this process may send Apple Events to Spotify, as macOS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Automation {
    /// Allowed.
    Granted,
    /// The user turned it off (System Settings → Privacy & Security → Automation).
    Denied,
    /// Never answered: macOS shows (or will show) the consent dialog.
    NeedsConsent,
    /// Spotify.app is not running, so macOS cannot say.
    SpotifyNotRunning,
    /// Not checked yet, or any other status.
    Unknown,
    /// A check is running.
    Checking,
    /// A check has been waiting for over [`STALL_AFTER`]: macOS is holding Apple Events to
    /// Spotify, which in practice means a consent dialog (ours or another app's) is open.
    Stalled,
}

impl Automation {
    /// Stable name used in `daemon.status` and `doctor`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::NeedsConsent => "needs_consent",
            Self::SpotifyNotRunning => "spotify_not_running",
            Self::Unknown => "unknown",
            Self::Checking => "checking",
            Self::Stalled => "stalled",
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Granted,
            2 => Self::Denied,
            3 => Self::NeedsConsent,
            4 => Self::SpotifyNotRunning,
            5 => Self::Checking,
            _ => Self::Unknown,
        }
    }

    const fn to_u8(self) -> u8 {
        match self {
            Self::Granted => 1,
            Self::Denied => 2,
            Self::NeedsConsent => 3,
            Self::SpotifyNotRunning => 4,
            Self::Checking | Self::Stalled => 5,
            Self::Unknown => 0,
        }
    }

    /// The error a command gets instead of an Apple Event macOS would hold or refuse.
    #[must_use]
    pub fn blocker(self) -> Option<Error> {
        match self {
            Self::NeedsConsent => Some(automation_pending(
                "macOS is asking whether spotify-daemon may control Spotify, and nothing can control Spotify through AppleScript until someone answers.",
            )),
            Self::Stalled => Some(automation_pending(
                "Spotify is not answering Apple Events: macOS is holding them, almost always because a dialog asking whether an app (spotify-daemon or another) may control Spotify is open.",
            )),
            Self::Denied => Some(classify("", Some(-1743))),
            _ => None,
        }
    }
}

/// How long a permission check may wait before it counts as [`Automation::Stalled`].
const STALL_AFTER: Duration = Duration::from_secs(3);

/// Last answer (an [`Automation`] as u8) and, while checking, when the check started.
static STATE: AtomicU8 = AtomicU8::new(0);
static CHECK_STARTED_MS: AtomicU64 = AtomicU64::new(0);
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The permission state as last seen by the background checker. Never blocks: macOS's own
/// check waits for Spotify, and Spotify answers nothing while a consent dialog is open.
#[must_use]
pub fn automation_state() -> Automation {
    match Automation::from_u8(STATE.load(Ordering::Relaxed)) {
        Automation::Checking => {
            let waited = now_ms().saturating_sub(CHECK_STARTED_MS.load(Ordering::Relaxed));
            if u128::from(waited) >= STALL_AFTER.as_millis() {
                Automation::Stalled
            } else {
                Automation::Checking
            }
        }
        state => state,
    }
}

/// Carbon `AEDesc` (declared under `#pragma pack(2)`).
#[repr(C, packed(2))]
struct AeDesc {
    descriptor_type: u32,
    data_handle: *mut c_void,
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn AECreateDesc(type_code: u32, data: *const c_void, size: isize, result: *mut AeDesc) -> i16;
    fn AEDisposeDesc(desc: *mut AeDesc) -> i16;
    fn AEDeterminePermissionToAutomateTarget(
        target: *const AeDesc,
        event_class: u32,
        event_id: u32,
        ask_user_if_needed: u8,
    ) -> i32;
}

const TYPE_APPLICATION_BUNDLE_ID: u32 = u32::from_be_bytes(*b"bund");
const TYPE_WILD_CARD: u32 = u32::from_be_bytes(*b"****");
const SPOTIFY_BUNDLE_ID: &str = "com.spotify.client";

/// Asks macOS whether this process may control Spotify and records the answer. Blocks while
/// Spotify's Apple Events are held; with `ask`, macOS shows its consent dialog when nobody
/// answered yet and this blocks until someone does. Only [`request_automation`]'s thread
/// calls it.
fn automation(ask: bool) -> Automation {
    CHECK_STARTED_MS.store(now_ms(), Ordering::Relaxed);
    if !ask {
        STATE.store(Automation::Checking.to_u8(), Ordering::Relaxed);
    }
    let mut desc = AeDesc {
        descriptor_type: 0,
        data_handle: std::ptr::null_mut(),
    };
    // SAFETY: the bundle id bytes outlive the call (AECreateDesc copies them) and `desc` is a
    // valid out-parameter; it is disposed below.
    let created = unsafe {
        AECreateDesc(
            TYPE_APPLICATION_BUNDLE_ID,
            SPOTIFY_BUNDLE_ID.as_ptr().cast(),
            SPOTIFY_BUNDLE_ID.len().cast_signed(),
            &mut desc,
        )
    };
    let state = if created == 0 {
        // SAFETY: `desc` is a valid address descriptor created above.
        let status = unsafe {
            AEDeterminePermissionToAutomateTarget(
                &desc,
                TYPE_WILD_CARD,
                TYPE_WILD_CARD,
                u8::from(ask),
            )
        };
        // SAFETY: disposes the descriptor created above exactly once.
        unsafe { AEDisposeDesc(&mut desc) };
        match status {
            0 => Automation::Granted,
            -1743 => Automation::Denied,
            -1744 => Automation::NeedsConsent,
            -600 => Automation::SpotifyNotRunning,
            _ => Automation::Unknown,
        }
    } else {
        Automation::Unknown
    };
    STATE.store(state.to_u8(), Ordering::Relaxed);
    state
}

/// Checks the permission on a background thread and, when nobody answered yet, raises
/// macOS's consent dialog right away (while someone is installing) instead of on the first
/// command. Re-checks every 15 s until the answer is final. `report` gets every new state.
pub fn request_automation(report: impl Fn(Automation) + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("automation".into())
        .spawn(move || {
            let mut last = None;
            loop {
                let mut state = automation(false);
                if state == Automation::NeedsConsent {
                    report(state);
                    last = Some(state);
                    state = automation(true);
                }
                if last != Some(state) {
                    report(state);
                }
                last = Some(state);
                if matches!(state, Automation::Granted | Automation::Denied) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(15));
            }
        });
}

fn automation_pending(message: &str) -> Error {
    Error::new(
        "automation_permission_pending",
        message,
        "Click Allow in the macOS dialog \"spotify-daemon\" wants access to control \"Spotify\" (it may be behind other windows). `spotify doctor` re-checks.",
    )
    .retryable()
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
