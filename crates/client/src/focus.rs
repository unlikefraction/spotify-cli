//! Keeping Spotify.app in the background when AppleScript starts something.
//!
//! Spotify.app answers AppleScript's `play track` by coming to the front, which takes the focus
//! from whatever the user is working in. [`keep_in_background`] notes the frontmost app before
//! such a start and, when Spotify.app took its place, gives the focus back to it, and hides
//! Spotify.app again when it was hidden before. Reads never bring Spotify.app forward, and the
//! Web API start ([`crate::webapi`]) does not either, so this only matters for AppleScript starts.
//!
//! [`LaunchServices`] asks macOS through `lsappinfo` (a few milliseconds per call) and needs no
//! permission. Giving the focus back goes through LaunchServices too (`lsappinfo setfront`), and
//! through `open -a <the app's bundle>` when macOS's cooperative activation ignored that; hiding
//! Spotify.app again is `NSRunningApplication`'s `hide`, run by `osascript -l JavaScript` (an
//! AppKit call, not an Apple Event, so it needs no permission either).

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Spotify.app's bundle identifier.
pub const SPOTIFY_BUNDLE_ID: &str = "com.spotify.client";

/// A running application, as LaunchServices names it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    /// Display name, e.g. `iTerm2`.
    pub name: String,
    /// Bundle identifier, e.g. `com.googlecode.iterm2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// Where the app bundle is, e.g. `/Applications/iTerm.app`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_path: Option<String>,
    /// Process id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// LaunchServices' application serial number (`ASN:0x0-0x1234:`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asn: Option<String>,
    /// LaunchServices' application type: `Foreground` for an app with a Dock icon, `UIElement`
    /// for agents (launchers such as Raycast, the login window), …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl App {
    /// Whether this is Spotify.app.
    #[must_use]
    pub fn is_spotify(&self) -> bool {
        self.bundle_id.as_deref() == Some(SPOTIFY_BUNDLE_ID)
    }

    /// The same app (by process when both know it, else by bundle or name).
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        match (self.pid, other.pid) {
            (Some(a), Some(b)) => a == b,
            _ => match (&self.bundle_id, &other.bundle_id) {
                (Some(a), Some(b)) => a == b,
                _ => self.name == other.name,
            },
        }
    }
}

/// What macOS says about the frontmost app, and how to change it.
pub trait Focus: Send + Sync {
    /// The app in front now (`None` when macOS does not say).
    fn frontmost(&self) -> Option<App>;
    /// Whether Spotify.app is hidden (`None`: not running, or unknown).
    fn spotify_hidden(&self) -> Option<bool>;
    /// Brings `app` to the front and names the way that worked.
    ///
    /// # Errors
    /// When macOS refused or ignored it.
    fn activate(&self, app: &App) -> Result<&'static str>;
    /// Hides Spotify.app.
    ///
    /// # Errors
    /// When macOS refused or ignored it.
    fn hide_spotify(&self) -> Result<()>;
}

/// What [`keep_in_background`] did after Spotify.app came to the front.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Refocus {
    /// The app that had the focus before, which got it back.
    pub app: String,
    /// How: `setfront` (LaunchServices) or `open` (`open -a`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Spotify.app was hidden before and is hidden again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hid_spotify: bool,
    /// Why the focus could not be given back (Spotify.app stays in front).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

/// How long Spotify.app can take to come forward after the start returned. `play track` returns
/// before Spotify.app activates (about 0.1 s later, seen on macOS 27), so a start that only runs
/// the script (the daemon's queue hand-offs) returns while the old app still has the front.
const ACTIVATION_GRACE: Duration = Duration::from_millis(1000);
const FOCUS_POLL: Duration = Duration::from_millis(40);
/// A start with several AppleScript steps (Liked Songs in order) can bring Spotify.app forward
/// more than once; the focus goes back at most this often.
const MAX_HAND_BACKS: usize = 3;

/// Runs `start` (something that may bring Spotify.app to the front) and, when Spotify.app takes
/// the front from another app, gives the focus back to that app and hides Spotify.app again when
/// it was hidden before. Without `focus`, or when Spotify.app was already in front, only runs
/// `start`.
///
/// The front is watched from another thread while `start` runs, so a start that waits until
/// Spotify.app shows its effect does not keep Spotify.app in front meanwhile, and for
/// `ACTIVATION_GRACE` (1 s) after it returned.
pub fn keep_in_background<T>(
    focus: Option<&dyn Focus>,
    start: impl FnOnce() -> T,
) -> (T, Option<Refocus>) {
    let Some(focus) = focus else {
        return (start(), None);
    };
    let before = focus.frontmost().filter(|app| !app.is_spotify());
    let Some(before) = before else {
        return (start(), None);
    };
    let hidden = focus.spotify_hidden();
    let returned = Returned(std::sync::Mutex::new(None));
    std::thread::scope(|scope| {
        let watcher = scope.spawn(|| watch(focus, &before, hidden, &returned));
        let result = {
            // Set on return, also when `start` panics, so the watcher always ends.
            let _mark = Mark(&returned);
            start()
        };
        let refocus = watcher.join().unwrap_or(None);
        (result, refocus)
    })
}

/// When the start returned (`None` while it runs).
struct Returned(std::sync::Mutex<Option<Instant>>);

impl Returned {
    fn at(&self) -> Option<Instant> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Marks the start as returned when dropped.
struct Mark<'a>(&'a Returned);

impl Drop for Mark<'_> {
    fn drop(&mut self) {
        *self
            .0
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    }
}

/// Watches the front while the start runs and for [`ACTIVATION_GRACE`] after it returned; gives
/// it back to `before` whenever Spotify.app took it. Stops when another app came forward (the user
/// switched apps), or when macOS kept Spotify.app in front.
fn watch(
    focus: &dyn Focus,
    before: &App,
    hidden: Option<bool>,
    returned: &Returned,
) -> Option<Refocus> {
    let mut refocus: Option<Refocus> = None;
    let mut hand_backs = 0;
    loop {
        // Read first: a start that returned before this look had its chance to show.
        let ended = returned.at();
        match focus.frontmost() {
            Some(now) if now.is_spotify() && hand_backs < MAX_HAND_BACKS => {
                hand_backs += 1;
                let mut this = Refocus {
                    app: before.name.clone(),
                    via: None,
                    hid_spotify: false,
                    error: None,
                };
                match focus.activate(before) {
                    Ok(via) => this.via = Some(via.to_owned()),
                    Err(error) => {
                        this.error = Some(error);
                        return Some(this);
                    }
                }
                if hidden == Some(true) {
                    this.hid_spotify = focus.hide_spotify().is_ok();
                }
                refocus = Some(this);
                continue;
            }
            Some(now) if !now.is_spotify() && !now.same_as(before) => return refocus,
            _ => {}
        }
        match ended {
            Some(at) if refocus.is_some() || at.elapsed() >= ACTIVATION_GRACE => return refocus,
            _ => std::thread::sleep(FOCUS_POLL),
        }
    }
}

/// [`Focus`] through LaunchServices: `lsappinfo` reads and sets the front app; `open -a` is the
/// second way to bring one forward; `NSRunningApplication` (through JavaScript for Automation)
/// hides Spotify.app.
#[derive(Clone, Copy, Debug, Default)]
pub struct LaunchServices;

const LSAPPINFO: &str = "/usr/bin/lsappinfo";
/// How long `lsappinfo setfront` gets to show. macOS's cooperative activation usually ignores it
/// from a process in the background (seen on macOS 27), so this is kept short.
const SETFRONT_WAIT: Duration = Duration::from_millis(150);
/// How long `open -a` gets to show.
const OPEN_WAIT: Duration = Duration::from_millis(800);
/// macOS ignored a `setfront` of this process's.
static SETFRONT_IGNORED: AtomicBool = AtomicBool::new(false);

impl LaunchServices {
    fn run(args: &[&str]) -> Option<String> {
        let output = Command::new(LSAPPINFO)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn spotify_asn() -> Option<String> {
        parse_asn(&Self::run(&[
            "find",
            &format!("bundleid={SPOTIFY_BUNDLE_ID}"),
        ])?)
    }

    /// Whether `app` still runs (known by its serial number; assumed when it has none).
    fn running(app: &App) -> bool {
        app.asn.as_deref().is_none_or(|asn| {
            Self::run(&["info", "-only", "pid", asn]).is_some_and(|info| info.contains("pid = "))
        })
    }

    /// Waits (up to `within`) until `app` is in front.
    fn wait_front(&self, app: &App, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if self.frontmost().is_some_and(|now| now.same_as(app)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(FOCUS_POLL);
        }
    }
}

impl Focus for LaunchServices {
    fn frontmost(&self) -> Option<App> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let asn = parse_asn(&Self::run(&["front"])?)?;
        let info = Self::run(&[
            "info",
            "-only",
            "name",
            "-only",
            "bundleid",
            "-only",
            "bundlepath",
            "-only",
            "pid",
            "-only",
            "applicationtype",
            &asn,
        ])?;
        let mut app = parse_info(&info)?;
        app.asn = Some(asn);
        Some(app)
    }

    fn spotify_hidden(&self) -> Option<bool> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let asn = Self::spotify_asn()?;
        let visible = Self::run(&["visibleProcessList"])?;
        Some(!lists_asn(&visible, &asn))
    }

    fn activate(&self, app: &App) -> Result<&'static str> {
        // `open -a` sends a running app "reopen": Finder answers it with a new window when it has
        // none, and an agent (a launcher like Raycast, the login window of a locked screen) shows
        // its own UI or is no app to return to. Only an ordinary app gets it.
        let reopens = app.bundle_id.as_deref() == Some("com.apple.finder")
            || app.kind.as_deref().is_some_and(|kind| kind != "Foreground");
        // `open -a` launches an app that is not running: never relaunch one that quit meanwhile.
        let open_path = app
            .bundle_path
            .as_deref()
            .filter(|_| !reopens && Self::running(app));
        // Once macOS ignored `setfront` (cooperative activation), it goes straight to `open` for
        // the rest of the process: each ignored try keeps Spotify.app in front a while longer.
        let skip_setfront = open_path.is_some() && SETFRONT_IGNORED.load(Ordering::Relaxed);
        if !skip_setfront
            && let Some(asn) = &app.asn
            && Self::run(&["setfront", asn]).is_some()
        {
            if self.wait_front(app, SETFRONT_WAIT) {
                return Ok("setfront");
            }
            SETFRONT_IGNORED.store(true, Ordering::Relaxed);
        }
        if let Some(path) = open_path {
            let opened = Command::new("/usr/bin/open")
                .args(["-a", path])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if opened && self.wait_front(app, OPEN_WAIT) {
                return Ok("open");
            }
        }
        Err(Error::new(
            "focus_not_returned",
            format!(
                "Spotify.app came to the front when AppleScript started playback, and macOS did not give the focus back to {}.",
                app.name
            ),
            "Switch back yourself. Playback started through the Web API does not bring Spotify.app forward; `spotify status --full` shows whether the Web API can reach Spotify.app.",
        ))
    }

    fn hide_spotify(&self) -> Result<()> {
        // NSRunningApplication's hide, from a JavaScript for Automation process: an AppKit call,
        // not an Apple Event to Spotify, so it needs no permission. LaunchServices' `setinfo`
        // cannot hide another app.
        const HIDE: &str = "ObjC.import('AppKit'); var a = $.NSRunningApplication.runningApplicationsWithBundleIdentifier('com.spotify.client'); if (a.count > 0) { a.objectAtIndex(0).hide; } 'ok'";
        let ran = Command::new("/usr/bin/osascript")
            .args(["-l", "JavaScript", "-e", HIDE])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if ran && self.spotify_hidden() == Some(true) {
            Ok(())
        } else {
            Err(Error::internal("macOS did not hide Spotify.app"))
        }
    }
}

/// The first `ASN:0x…-0x…:` in `lsappinfo` output (`front`, `find`).
fn parse_asn(output: &str) -> Option<String> {
    let start = output.find("ASN:")?;
    let rest = &output[start + 4..];
    let end = rest
        .find(|c: char| !(c.is_ascii_hexdigit() || c == 'x' || c == '-'))
        .unwrap_or(rest.len());
    let id = rest[..end].trim_end_matches('-');
    (!id.is_empty()).then(|| format!("ASN:{id}:"))
}

/// Whether `lsappinfo visibleProcessList` output (`ASN:0x0-0x55055-"Spotify": ASN:…`) names
/// `asn` (`ASN:0x0-0x55055:`) exactly: `ASN:0x0-0x550551-…` is another app.
fn lists_asn(output: &str, asn: &str) -> bool {
    let id = asn.trim_end_matches(':');
    output.split_whitespace().any(|entry| {
        entry
            .strip_prefix(id)
            .is_some_and(|rest| rest.starts_with(['-', ':']))
    })
}

/// Name, bundle id, bundle path and pid from `lsappinfo info -only …` output:
///
/// ```text
/// "iTerm2" ASN:0x0-0x180f80e: (in front)
///     bundleID="com.googlecode.iterm2"
///     bundle path="/Applications/iTerm.app"
///     pid = 28473 …
/// ```
fn parse_info(output: &str) -> Option<App> {
    let quoted = |line: &str, key: &str| -> Option<String> {
        let value = line.trim().strip_prefix(key)?.strip_prefix('=')?;
        let value = value.strip_prefix('"')?;
        Some(value[..value.rfind('"')?].to_owned())
    };
    let mut app = App::default();
    for line in output.lines() {
        let trimmed = line.trim();
        if app.name.is_empty()
            && let Some(rest) = trimmed.strip_prefix('"')
            && let Some(end) = rest.find('"')
        {
            app.name = rest[..end].to_owned();
        } else if let Some(id) = quoted(trimmed, "bundleID") {
            app.bundle_id = Some(id);
        } else if let Some(path) = quoted(trimmed, "bundle path") {
            app.bundle_path = Some(path);
        }
        if let Some(rest) = trimmed.split("pid = ").nth(1) {
            app.pid = rest
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok());
        }
        if let Some(rest) = trimmed.split(" type=\"").nth(1)
            && let Some(end) = rest.find('"')
        {
            app.kind = Some(rest[..end].to_owned());
        }
    }
    (!app.name.is_empty() || app.bundle_id.is_some()).then_some(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn reads_lsappinfo_output() {
        assert_eq!(
            parse_asn("ASN:0x0-0x180f80e:\n").as_deref(),
            Some("ASN:0x0-0x180f80e:")
        );
        assert_eq!(
            parse_asn("ASN:0x0-0x55055-\"Spotify\":\n").as_deref(),
            Some("ASN:0x0-0x55055:")
        );
        assert_eq!(parse_asn("[ NULL ]"), None);
        let info = "\"iTerm2\" ASN:0x0-0x180f80e: (in front) \n    bundleID=\"com.googlecode.iterm2\"\n    bundle path=\"/Applications/iTerm.app\"\n    executable path=[ NULL ] \n    pid = 28473 !cgsConnection !signalled type=[ NULL ]\n";
        assert_eq!(
            parse_info(info),
            Some(App {
                name: "iTerm2".into(),
                bundle_id: Some("com.googlecode.iterm2".into()),
                bundle_path: Some("/Applications/iTerm.app".into()),
                pid: Some(28473),
                asn: None,
                kind: None,
            })
        );
        assert_eq!(
            parse_info("[ NULL ]  [ NULL ]\n    bundleID=[ NULL ]\n"),
            None
        );
        // With `-only applicationtype`: an agent such as a launcher is no app to reopen.
        let agent = "\"Raycast\" ASN:0x0-0x2209207: \n    bundleID=\"com.raycast.macos\"\n    bundle path=\"/Applications/Raycast.app\"\n    pid = 812 !cgsConnection !signalled type=\"UIElement\" flavor=[ NULL ]\n";
        assert_eq!(
            parse_info(agent).and_then(|app| app.kind).as_deref(),
            Some("UIElement")
        );
        // Visible only when its own entry is listed, not another app's with a longer number.
        let visible = "ASN:0x0-0x21af1ad-\"Claude\": ASN:0x0-0x550551-\"Notes\": \n";
        assert!(!lists_asn(visible, "ASN:0x0-0x55055:"));
        assert!(lists_asn(visible, "ASN:0x0-0x550551:"));
        assert!(lists_asn(
            "ASN:0x0-0x55055-\"Spotify\": ASN:0x0-0x44044-\"Finder\":",
            "ASN:0x0-0x55055:"
        ));
    }

    /// A desktop: who is in front, whether Spotify.app is hidden, and what was asked.
    struct Desk {
        front: Mutex<App>,
        spotify_hidden: Mutex<bool>,
        /// What happens when the start runs.
        start_brings_spotify: bool,
        refuse: bool,
        calls: Mutex<Vec<String>>,
    }

    fn app(name: &str, bundle: &str, pid: u32) -> App {
        App {
            name: name.into(),
            bundle_id: Some(bundle.into()),
            bundle_path: Some(format!("/Applications/{name}.app")),
            pid: Some(pid),
            asn: None,
            kind: Some("Foreground".into()),
        }
    }

    fn spotify() -> App {
        app("Spotify", SPOTIFY_BUNDLE_ID, 1)
    }

    impl Desk {
        fn new(front: App, hidden: bool, brings: bool) -> Self {
            Self {
                front: Mutex::new(front),
                spotify_hidden: Mutex::new(hidden),
                start_brings_spotify: brings,
                refuse: false,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn start(&self) -> &'static str {
            if self.start_brings_spotify {
                *self.front.lock().expect("lock") = spotify();
                *self.spotify_hidden.lock().expect("lock") = false;
            }
            "started"
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl Focus for Desk {
        fn frontmost(&self) -> Option<App> {
            Some(self.front.lock().expect("lock").clone())
        }

        fn spotify_hidden(&self) -> Option<bool> {
            Some(*self.spotify_hidden.lock().expect("lock"))
        }

        fn activate(&self, app: &App) -> Result<&'static str> {
            self.calls
                .lock()
                .expect("lock")
                .push(format!("activate {}", app.name));
            if self.refuse {
                return Err(Error::new("focus_not_returned", "no", ""));
            }
            *self.front.lock().expect("lock") = app.clone();
            Ok("setfront")
        }

        fn hide_spotify(&self) -> Result<()> {
            self.calls.lock().expect("lock").push("hide".into());
            *self.spotify_hidden.lock().expect("lock") = true;
            Ok(())
        }
    }

    #[test]
    fn gives_the_focus_back_when_spotify_took_it() {
        let desk = Desk::new(app("iTerm2", "com.googlecode.iterm2", 7), false, true);
        let (result, refocus) = keep_in_background(Some(&desk), || desk.start());
        assert_eq!(result, "started");
        let refocus = refocus.expect("refocused");
        assert_eq!(refocus.app, "iTerm2");
        assert_eq!(refocus.via.as_deref(), Some("setfront"));
        assert!(!refocus.hid_spotify);
        assert_eq!(desk.calls(), ["activate iTerm2"]);
        assert_eq!(desk.frontmost().map(|a| a.name).as_deref(), Some("iTerm2"));
    }

    #[test]
    fn hides_spotify_again_only_when_it_was_hidden() {
        let desk = Desk::new(app("Notes", "com.apple.Notes", 9), true, true);
        let (_, refocus) = keep_in_background(Some(&desk), || desk.start());
        assert!(refocus.expect("refocused").hid_spotify);
        assert_eq!(desk.calls(), ["activate Notes", "hide"]);
        assert_eq!(desk.spotify_hidden(), Some(true));
    }

    #[test]
    fn leaves_the_focus_alone_when_nothing_took_it() {
        // Spotify.app did not come forward.
        let desk = Desk::new(app("iTerm2", "com.googlecode.iterm2", 7), false, false);
        let (_, refocus) = keep_in_background(Some(&desk), || desk.start());
        assert_eq!(refocus, None);
        assert!(desk.calls().is_empty());
        // Spotify.app was in front already.
        let desk = Desk::new(spotify(), false, true);
        let (_, refocus) = keep_in_background(Some(&desk), || desk.start());
        assert_eq!(refocus, None);
        assert!(desk.calls().is_empty());
        // Turned off.
        let desk = Desk::new(app("iTerm2", "com.googlecode.iterm2", 7), false, true);
        let (_, refocus) = keep_in_background(None, || desk.start());
        assert_eq!(refocus, None);
        assert!(desk.frontmost().is_some_and(|a| a.is_spotify()));
    }

    #[test]
    fn gives_the_focus_back_while_a_start_verifies_and_after_a_bare_one_returned() {
        // A verified start: Spotify.app comes forward at once, and the start waits on until it
        // sees its effect. The focus goes back meanwhile, not after.
        let desk = Desk::new(app("iTerm2", "com.googlecode.iterm2", 7), false, true);
        let (given_back_while_running, refocus) = keep_in_background(Some(&desk), || {
            desk.start();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if desk.frontmost().is_some_and(|a| !a.is_spotify()) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        });
        assert!(given_back_while_running);
        assert_eq!(refocus.map(|r| r.app).as_deref(), Some("iTerm2"));
        // A bare script (the daemon's queue hand-off): it returns, and Spotify.app comes forward
        // a little later.
        let desk = Desk::new(app("iTerm2", "com.googlecode.iterm2", 7), false, false);
        std::thread::scope(|scope| {
            let (_, refocus) = keep_in_background(Some(&desk), || {
                scope.spawn(|| {
                    std::thread::sleep(Duration::from_millis(300));
                    *desk.front.lock().expect("lock") = spotify();
                });
            });
            assert_eq!(refocus.map(|r| r.app).as_deref(), Some("iTerm2"));
        });
        assert_eq!(desk.calls(), ["activate iTerm2"]);
    }

    #[test]
    fn says_when_macos_kept_spotify_in_front() {
        let mut desk = Desk::new(app("iTerm2", "com.googlecode.iterm2", 7), true, true);
        desk.refuse = true;
        let (_, refocus) = keep_in_background(Some(&desk), || desk.start());
        let refocus = refocus.expect("reported");
        assert_eq!(
            refocus.error.map(|e| e.code).as_deref(),
            Some("focus_not_returned")
        );
        // Not hidden while it holds the front.
        assert_eq!(desk.calls(), ["activate iTerm2"]);
    }
}
