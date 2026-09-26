//! Hourly update checker.
//!
//! - Installed by Honeycomb (`honeycomb install spotify`): Honeycomb's shared worker updates the
//!   binaries; this checker only reports.
//! - Installed by the one-line script: the daemon checks GitHub releases every hour, verifies the
//!   archive against `SHA256SUMS`, swaps the binaries atomically and restarts itself (launchd's
//!   KeepAlive brings the new version up). Opt out with `spotify config set '{"auto_update": false}'`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use silicon_spotify_client::model::now_rfc3339;
use silicon_spotify_client::{Error, REPOSITORY, Result, VERSION};

use crate::log;
use crate::service::{Daemon, Settings};

fn set(daemon: &Daemon, value: Value) {
    *daemon
        .update
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
}

/// How this copy was installed.
#[must_use]
pub fn install_method() -> (&'static str, Option<PathBuf>) {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let text = exe
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if text.contains("/.honeycomb/") || text.contains("/honeycomb/") {
        return ("honeycomb", exe);
    }
    if text.contains("/target/debug/") || text.contains("/target/release/") {
        return ("source", exe);
    }
    ("script", exe)
}

/// Runs the checker forever (first check two minutes after start, then hourly).
pub async fn run(daemon: Arc<Daemon>) {
    tokio::select! {
        () = tokio::time::sleep(Duration::from_secs(120)) => {}
        () = daemon.shutdown.notified() => return,
    }
    loop {
        let auto_update = daemon.settings().auto_update.unwrap_or(true);
        match check(&daemon, auto_update, auto_update).await {
            Ok(status) => {
                if status.get("restarting").and_then(Value::as_bool) == Some(true) {
                    log!("updated to {}; restarting", status["latest"]);
                    crate::shutdown(&daemon, 0);
                }
            }
            Err(error) => log!("update check failed: {error}"),
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(3600)) => {}
            () = daemon.shutdown.notified() => return,
        }
    }
}

/// The `auto_update` setting: the caller's, else the one the daemon last saw, else on.
#[must_use]
pub fn auto_update(caller: &Settings, remembered: &Settings) -> bool {
    caller
        .auto_update
        .or(remembered.auto_update)
        .unwrap_or(true)
}

/// Checks now; installs when `apply` and this is a script install. `auto_update` is the user's
/// setting, reported as is (a manual `spotify update --check` does not install, but that does
/// not turn automatic updates off).
///
/// # Errors
/// Network or verification failures.
pub async fn check(daemon: &Daemon, apply: bool, auto_update: bool) -> Result<Value> {
    let (method, exe) = install_method();
    let base = json!({"current": VERSION, "method": method, "checked_at": now_rfc3339(), "auto_update": auto_update});
    if method == "honeycomb" {
        let status = json!({"current": VERSION, "method": method, "checked_at": now_rfc3339(), "manager": "honeycomb",
            "note": "Honeycomb's shared worker installs updates for Honeycomb installs; run `honeycomb update spotify` to update now."});
        set(daemon, status.clone());
        return Ok(status);
    }
    let latest = latest_version().await?;
    let newer = semver::Version::parse(&latest)
        .ok()
        .zip(semver::Version::parse(VERSION).ok())
        .is_some_and(|(l, c)| l > c);
    let mut status = base;
    status["latest"] = json!(latest);
    status["update_available"] = json!(newer);
    if !newer || !apply || method != "script" {
        set(daemon, status.clone());
        return Ok(status);
    }
    let dir = exe
        .as_ref()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| Error::internal("cannot locate the installed binaries"))?;
    match install(&latest, &dir).await {
        Ok(()) => {
            status["installed"] = json!(latest);
            status["restarting"] = json!(true);
        }
        Err(error) => status["error"] = serde_json::to_value(&error)?,
    }
    set(daemon, status.clone());
    Ok(status)
}

async fn latest_version() -> Result<String> {
    silicon_spotify_client::api::ensure_crypto();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .user_agent(concat!(
            "silicon-spotify-daemon/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(|e| Error::internal(e.to_string()))?;
    let response = client
        .get(format!("{REPOSITORY}/releases/latest"))
        .send()
        .await
        .map_err(|e| Error::backend_unavailable(format!("GitHub releases unreachable: {e}")))?;
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            Error::new(
                "no_release",
                "GitHub has no published release for spotify-cli yet.",
                "Nothing to update.",
            )
        })?;
    let tag = location.rsplit('/').next().unwrap_or_default();
    let version = tag.trim_start_matches('v');
    if version.is_empty()
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(Error::new(
            "no_release",
            format!("Unexpected release tag `{tag}`."),
            "Report it with `spotify report`.",
        ));
    }
    Ok(version.to_owned())
}

async fn install(version: &str, bin_dir: &Path) -> Result<()> {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let archive = format!("spotify-v{version}-{arch}-apple-darwin.tar.gz");
    let base = format!("{REPOSITORY}/releases/download/v{version}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .user_agent(concat!(
            "silicon-spotify-daemon/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(|e| Error::internal(e.to_string()))?;
    let fetch = |url: String| {
        let client = client.clone();
        async move {
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|e| Error::backend_unavailable(format!("download {url}: {e}")))?;
            if !response.status().is_success() {
                return Err(Error::new(
                    "download_failed",
                    format!("{url} answered {}.", response.status()),
                    "Retry later.",
                ));
            }
            response
                .bytes()
                .await
                .map_err(|e| Error::backend_unavailable(format!("download {url}: {e}")))
        }
    };
    let sums = fetch(format!("{base}/SHA256SUMS")).await?;
    let bytes = fetch(format!("{base}/{archive}")).await?;
    let expected = String::from_utf8_lossy(&sums)
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let (hash, name) = (parts.next()?, parts.next()?);
            (name.trim_start_matches('*') == archive).then(|| hash.to_owned())
        })
        .ok_or_else(|| {
            Error::new(
                "checksum_missing",
                format!("SHA256SUMS lists no {archive}."),
                "The release is incomplete; wait for the next one.",
            )
        })?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != expected {
        return Err(Error::new(
            "checksum_mismatch",
            "The downloaded release does not match SHA256SUMS; nothing was installed.",
            "Retry later; report it if it persists.",
        ));
    }
    let staging = std::env::temp_dir().join(format!(
        "silicon-spotify-update-{}",
        uuid::Uuid::now_v7().simple()
    ));
    std::fs::create_dir_all(&staging).map_err(|e| Error::internal(e.to_string()))?;
    let tarball = staging.join(&archive);
    std::fs::write(&tarball, &bytes).map_err(|e| Error::internal(e.to_string()))?;
    let status = std::process::Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&staging)
        .status()
        .map_err(|e| Error::internal(e.to_string()))?;
    if !status.success() {
        return Err(Error::new(
            "unpack_failed",
            "The release archive could not be unpacked.",
            "Retry later.",
        ));
    }
    for name in ["spotify", "spotify-daemon"] {
        let source = staging.join(name);
        if !source.is_file() {
            return Err(Error::new(
                "unpack_failed",
                format!("The release archive has no {name}."),
                "Report it with `spotify report`.",
            ));
        }
        let target = bin_dir.join(name);
        let temp = bin_dir.join(format!(".{name}.new"));
        std::fs::copy(&source, &temp).map_err(|e| {
            Error::new("update_not_writable", format!("Cannot write {}: {e}.", bin_dir.display()), "Re-run the installer with permission to that directory, or install with Honeycomb.")
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755));
        }
        std::fs::rename(&temp, &target).map_err(|e| {
            Error::new(
                "update_not_writable",
                format!("Cannot replace {}: {e}.", target.display()),
                "Re-run the installer.",
            )
        })?;
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_check_reports_the_setting_not_whether_it_installs() {
        let unset = Settings::default();
        let on = Settings {
            auto_update: Some(true),
            ..Settings::default()
        };
        let off = Settings {
            auto_update: Some(false),
            ..Settings::default()
        };
        // `spotify update --check` from a CLI whose config says auto_update true (or nothing).
        assert!(auto_update(&on, &unset));
        assert!(auto_update(&unset, &unset));
        // The caller's own setting wins over what the daemon saw last.
        assert!(!auto_update(&off, &on));
        assert!(auto_update(&on, &off));
        // Callers that send no settings get the daemon's.
        assert!(!auto_update(&unset, &off));
    }
}
