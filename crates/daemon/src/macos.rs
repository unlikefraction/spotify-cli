//! macOS bridge (FFI; this module is the only place unsafe is allowed): in-process AppleScript on the main thread and Spotify's playback notifications.
//!
//! - `NSAppleScript` must be used from the main thread. Every script is compiled once, cached,
//!   and executed on the main queue; callers on other threads block until it finishes. Scripts
//!   carry `with timeout`, so a frozen Spotify cannot wedge the main thread for long.
//! - Spotify posts `com.spotify.client.PlaybackStateChanged` (distributed notification) on
//!   play, pause and track changes (not on seeks). The observer only nudges the watcher, which
//!   then takes a fresh AppleScript reading.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;

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
