//! CLI ↔ daemon protocol: one JSON object per line over a Unix socket, one request per connection.
//!
//! There is one `spotify-daemon` per OS user (one Spotify.app per Mac user), shared by every
//! Silicon home. It lives in `~/.silicon-spotify/` of the *real* user home (from the password
//! database, not `$HOME`/`$SILICON_HOME`), or `$SPOTIFY_DAEMON_HOME` when set.
//!
//! Request: `{"v":1,"id":"<uuid>","op":"player.pause","home":"/abs/silicon/home","args":{…},
//! "client_version":"0.1.0","trace_id":"…","isi":"planner"}`
//! Reply: `{"v":1,"id":"<same>","ok":true,"data":{…}}` or `{"v":1,"id":"…","ok":false,"error":{…}}`.
//! Unknown ops are refused by name. The socket is 0600 inside a 0700 directory, and both sides
//! check that the peer runs as the same uid.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use crate::{Error, Result};

/// Wire protocol version.
pub const PROTOCOL: u32 = 1;
/// Largest accepted frame.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

/// A request to the daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version.
    pub v: u32,
    /// Correlation id.
    pub id: String,
    /// Operation, e.g. `player.status`, `trigger.add`.
    pub op: String,
    /// Canonical Silicon home the request acts for (triggers, sessions).
    #[serde(default)]
    pub home: Option<String>,
    /// Operation arguments.
    #[serde(default)]
    pub args: Value,
    /// Caller's version (the CLI restarts an older daemon).
    #[serde(default)]
    pub client_version: String,
    /// Trace id for telemetry.
    #[serde(default)]
    pub trace_id: Option<String>,
    /// The caller's ISI, when known.
    #[serde(default)]
    pub isi: Option<String>,
}

/// The daemon's answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    /// Protocol version.
    pub v: u32,
    /// Same as the request.
    pub id: String,
    /// Success flag.
    pub ok: bool,
    /// Result on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Error on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

impl Reply {
    /// Success.
    #[must_use]
    pub fn ok(id: String, data: Value) -> Self {
        Self {
            v: PROTOCOL,
            id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// Failure.
    #[must_use]
    pub fn err(id: String, error: Error) -> Self {
        Self {
            v: PROTOCOL,
            id,
            ok: false,
            data: None,
            error: Some(error),
        }
    }
}

/// The daemon's directory.
///
/// # Errors
/// When no home directory can be determined.
pub fn daemon_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SPOTIFY_DAEMON_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    real_home()
        .map(|home| home.join(".silicon-spotify"))
        .ok_or_else(|| {
            Error::internal("cannot determine the user's home directory from the password database")
        })
}

/// `<daemon dir>/daemon.sock`, or `/tmp/silicon-spotify-<hash>.sock` when that path would exceed
/// the 104-byte Unix socket limit (deep `SPOTIFY_DAEMON_HOME` directories).
///
/// # Errors
/// See [`daemon_dir`].
pub fn socket_path() -> Result<PathBuf> {
    let dir = daemon_dir()?;
    let path = dir.join("daemon.sock");
    if path.as_os_str().len() < 100 {
        return Ok(path);
    }
    let digest = blake3::hash(dir.to_string_lossy().as_bytes()).to_hex();
    Ok(PathBuf::from(format!(
        "/tmp/silicon-spotify-{}.sock",
        &digest.as_str()[..16]
    )))
}

/// The OS user's home from the password database (ignores `$HOME`), falling back to `$HOME`.
#[must_use]
pub fn real_home() -> Option<PathBuf> {
    passwd_home().or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}

#[cfg(not(unix))]
fn passwd_home() -> Option<PathBuf> {
    None
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn passwd_home() -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt as _;
    let mut buffer = vec![0_u8; 16 * 1024];
    // SAFETY: `passwd` is plain C data and fully written by getpwuid_r on success; `buffer`
    // outlives every pointer read from `passwd`, and `result` is checked before use.
    unsafe {
        let mut passwd: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let status = libc::getpwuid_r(
            libc::getuid(),
            &raw mut passwd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &raw mut result,
        );
        if status != 0 || result.is_null() || passwd.pw_dir.is_null() {
            return None;
        }
        let dir = CStr::from_ptr(passwd.pw_dir);
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
    }
}

/// The current effective uid.
#[cfg(unix)]
#[allow(unsafe_code)]
#[must_use]
pub fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The current effective uid (always 0 off unix; the daemon only runs on macOS).
#[cfg(not(unix))]
#[must_use]
pub fn current_uid() -> u32 {
    0
}

/// Sends one request and waits for the reply (the daemon only exists on macOS).
///
/// # Errors
/// Always `platform_unsupported` off unix.
#[cfg(not(unix))]
pub async fn call(request: &Request, _timeout: Duration) -> Result<Value> {
    Err(Error::platform_unsupported(&format!(
        "`{}` (the Spotify daemon)",
        request.op
    )))
}

/// Sends one request and waits for the reply.
///
/// # Errors
/// `daemon_unavailable` when the socket is missing or refuses; the daemon's own error otherwise.
#[cfg(unix)]
pub async fn call(request: &Request, timeout: Duration) -> Result<Value> {
    let path = socket_path()?;
    let stream = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::UnixStream::connect(&path),
    )
    .await
    .map_err(|_| {
        Error::daemon_unavailable(format!(
            "Connecting to the daemon at {} timed out.",
            path.display()
        ))
    })?
    .map_err(|error| {
        Error::daemon_unavailable(format!(
            "The Spotify daemon is not running ({}: {error}).",
            path.display()
        ))
    })?;
    if let Ok(cred) = stream.peer_cred()
        && cred.uid() != current_uid()
    {
        return Err(Error::new(
            "permission_denied",
            format!(
                "The daemon socket {} belongs to another user (uid {}).",
                path.display(),
                cred.uid()
            ),
            "Each macOS user runs their own daemon. Remove the stale socket or set SPOTIFY_DAEMON_HOME.",
        ));
    }
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    let exchange = async {
        write.write_all(&line).await?;
        write.flush().await?;
        let mut reader = BufReader::new(read).take(MAX_FRAME as u64);
        let mut response = String::new();
        reader.read_line(&mut response).await?;
        Ok::<String, std::io::Error>(response)
    };
    let response = tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| {
            Error::new(
                "timeout",
                format!("The daemon did not answer `{}` within {timeout:?}.", request.op),
                "Spotify or the network may be slow. Retry; `spotify daemon status` and ~/.silicon-spotify/daemon.log show what the daemon is doing.",
            )
            .retryable()
        })?
        .map_err(|error| Error::daemon_unavailable(format!("The daemon connection broke: {error}.")))?;
    if response.trim().is_empty() {
        return Err(Error::daemon_unavailable(
            "The daemon closed the connection without answering (it may be restarting).",
        ));
    }
    let reply: Reply = serde_json::from_str(&response).map_err(|error| {
        Error::internal(format!("The daemon sent an unreadable reply: {error}"))
    })?;
    if reply.ok {
        Ok(reply.data.unwrap_or(Value::Null))
    } else {
        Err(reply
            .error
            .unwrap_or_else(|| Error::internal("the daemon failed without an error")))
    }
}

#[cfg(unix)]
use tokio::io::AsyncReadExt as _;
