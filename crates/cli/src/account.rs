//! Identity, configuration and operations: iam, login, logout, ting, config, testing, report,
//! doctor, setup, update.

use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use silicon_spotify_client::api::Session;
use silicon_spotify_client::store::{self, CONFIG_KEYS, Config, Slot};
use silicon_spotify_client::{
    APP_ID, APP_NAME, CLI_PACKAGE, DOCS_URL, Error, IAM_AUTH_URL, IAM_URL, INSTALL_COMMAND,
    OWNER_ORG, REPOSITORY, RUST_PACKAGE, Result, TING_TYPES, VERSION, WEBSITE,
};

use crate::Ctx;
use crate::args::{
    AuthCommand, ConfigCommand, LoginArgs, LoginCommand, TestingCommand, TingCommand,
};

/// `spotify iam --json`: static, offline discovery.
pub fn iam(ctx: &Ctx) -> Result<()> {
    let value = json!({
        "app_id": APP_ID,
        "org_id": OWNER_ORG,
        "name": APP_NAME,
        "version": VERSION,
        "api_url": ctx.api_url,
        "iam_url": IAM_URL,
        "auth_url": IAM_AUTH_URL,
        "login_method": "short_lived_token",
        "login": format!("Mint an SLT with `iam silicon-login --app-id {APP_ID} --grant-org <org> --approve-scopes` (Silicon) or `iam login --app-id {APP_ID} --grant-org <org>` (Carbon), then run `spotify login '<SLT>'`."),
        "credential_issuer": false,
        "scopes": ["self.identity.read", "self.profile.read", "obo:ting:subscriptions.register", "obo:ting:tings.send"],
        "ting_types": TING_TYPES.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        "docs": DOCS_URL,
        "website": WEBSITE,
        "repository": REPOSITORY,
        "rust_package": RUST_PACKAGE,
        "cli_package": CLI_PACKAGE,
        "install": INSTALL_COMMAND,
        "testing": ctx.testing.is_some(),
    });
    ctx.emit(&value, |v| {
        format!(
            "{APP_NAME} {VERSION}\n  app_id: {APP_ID} (org {OWNER_ORG})\n  backend: {}\n  login: {}\n  docs: {DOCS_URL}\n  source: {REPOSITORY}\n  rust: {RUST_PACKAGE}",
            v["api_url"].as_str().unwrap_or(""),
            v["login"].as_str().unwrap_or("")
        )
    });
    Ok(())
}

fn read_token(path: &Path) -> Result<String> {
    let mut text = String::new();
    if path == Path::new("-") {
        if std::io::stdin().is_terminal() {
            return Err(Error::invalid(
                "--token-file - reads the SLT from stdin, but stdin is a terminal.",
                "Pipe it: iam -o json silicon-login --app-id spotify --grant-org <org> --approve-scopes | jq -r .slt | spotify login --token-file -",
            ));
        }
        std::io::stdin()
            .take(65_536)
            .read_to_string(&mut text)
            .map_err(|e| {
                Error::invalid(format!("Cannot read stdin: {e}."), "Pipe the SLT on stdin.")
            })?;
    } else {
        text = std::fs::read_to_string(path).map_err(|e| {
            Error::invalid(
                format!("Cannot read {}: {e}.", path.display()),
                "Check the path.",
            )
        })?;
    }
    Ok(text.trim().to_owned())
}

fn session_view(ctx: &Ctx, slot: &Slot) -> Value {
    let expires = time::OffsetDateTime::from_unix_timestamp(slot.expires_at)
        .ok()
        .map(silicon_spotify_client::model::rfc3339);
    json!({
        "authenticated": true,
        "actor": slot.session.actor,
        "org_id": slot.session.org_id,
        "org_ids": slot.session.org_ids,
        "scopes": slot.session.scope.split_whitespace().collect::<Vec<_>>(),
        "access_expires_at": expires,
        "ting": slot.session.ting,
        "testing_environment_id": slot.session.testing_environment_id,
        "api_url": slot.api_url,
        "store": ctx.home.dir,
    })
}

/// `spotify login …`.
pub async fn login(ctx: &Ctx, args: LoginArgs) -> Result<()> {
    if matches!(args.command, Some(LoginCommand::Status)) {
        return status(ctx).await;
    }
    let slt = match (&args.slt, &args.token_file) {
        (Some(_), Some(_)) => {
            return Err(Error::invalid(
                "Give the SLT either as an argument or with --token-file, not both.",
                "spotify login '<SLT>'",
            ));
        }
        (Some(slt), None) => slt.trim().to_owned(),
        (None, Some(path)) => read_token(path)?,
        (None, None) => {
            return Err(Error::invalid(
                "`spotify login` needs an IAM short-lived token (SLT). The CLI never asks for passwords or codes.",
                "Silicon: iam silicon-login --app-id spotify --grant-org <org> --approve-scopes, then spotify login '<SLT>'. Carbon: iam login --app-id spotify --grant-org <org>.",
            ));
        }
    };
    if slt.is_empty() || slt.len() > 8192 || !slt.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::invalid(
            "The SLT is empty or contains spaces/control characters.",
            "Pass the token exactly as iam printed it (usually oac_…).",
        ));
    }
    let api = ctx.api()?;
    let session: Session = api.login(&slt, &store::login_key(&slt)).await.map_err(|error| {
        if error.code == "not_authenticated" {
            Error::new(
                "slt_rejected",
                "IAM rejected the short-lived token: it expired (they last ~2 minutes), was already used, or was minted for another app.",
                "Mint a fresh one for app `spotify` and retry immediately: iam silicon-login --app-id spotify --grant-org <org> --approve-scopes",
            )
            .with_details(json!({"iam": error}))
        } else {
            error
        }
    })?;
    let slot = store::save_login(&ctx.home, &ctx.slot(), &ctx.api_url, session)?;
    let view = session_view(ctx, &slot);
    ctx.emit(&view, |v| {
        let ting = if v.pointer("/ting/subscribed") == Some(&Value::Bool(true)) {
            "registered as a Ting recipient ✓".to_owned()
        } else {
            format!("Ting registration failed: {} (triggers need it; retry with `spotify ting register`)", v.pointer("/ting/error/message").and_then(Value::as_str).unwrap_or("unknown reason"))
        };
        format!("Logged in as {} (org {}).\n  {ting}", v["actor"]["public_id"].as_str().unwrap_or("?"), v["org_id"].as_str().unwrap_or("?"))
    });
    ctx.hint(
        "Next: spotify trigger add --remaining 30s --note 'what to do'   (spotify trigger --help)",
    );
    Ok(())
}

/// `spotify login status`: live check.
async fn status(ctx: &Ctx) -> Result<()> {
    let key = ctx.slot();
    let unauthenticated = |reason: &str| json!({"authenticated": false, "api_url": ctx.api_url, "store": ctx.home.dir, "reason": reason, "testing": ctx.testing.is_some()});
    if !ctx.home.sessions()?.slots.contains_key(&key) {
        let value = unauthenticated("no session saved");
        ctx.emit(&value, |_| {
            format!(
                "Not logged in (no session in {}). Run `spotify login '<SLT>'`.",
                ctx.home.dir.display()
            )
        });
        return Ok(());
    }
    let api = ctx.api()?;
    let mut slot = match store::fresh_session(&ctx.home, &api, &key, false).await {
        Ok(slot) => slot,
        Err(error) if error.code == "not_authenticated" => {
            let value = unauthenticated("session revoked or expired");
            ctx.emit(&value, |_| "Not logged in: the saved session was revoked or expired. Run `spotify login '<SLT>'`.".into());
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let org = slot.session.org_id.clone();
    let me = match api.me(&slot.session.access_token, &org).await {
        Err(error) if error.code == "not_authenticated" => {
            match store::fresh_session(&ctx.home, &api, &key, true).await {
                Ok(fresh) => {
                    slot = fresh;
                    api.me(&slot.session.access_token, &org).await
                }
                Err(error) if error.code == "not_authenticated" => {
                    let value = unauthenticated("session revoked");
                    ctx.emit(&value, |_| {
                        "Not logged in: IAM revoked the session. Run `spotify login '<SLT>'`."
                            .into()
                    });
                    return Ok(());
                }
                Err(error) => Err(error),
            }
        }
        other => other,
    };
    let me = match me {
        Ok(me) => me,
        Err(error) if error.code == "not_authenticated" => {
            store::remove_slot(&ctx.home, &key)?;
            let value = unauthenticated("session rejected by the backend");
            ctx.emit(&value, |_| {
                "Not logged in: the backend rejected the session. Run `spotify login '<SLT>'`."
                    .into()
            });
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut view = session_view(ctx, &slot);
    view["identity"] = me;
    ctx.emit(&view, |v| {
        format!(
            "Logged in as {} (org {}) at {}.\n  Ting recipient: {}",
            v["actor"]["public_id"].as_str().unwrap_or("?"),
            v["org_id"].as_str().unwrap_or("?"),
            v["api_url"].as_str().unwrap_or("?"),
            if v.pointer("/ting/subscribed") == Some(&Value::Bool(true)) {
                "registered"
            } else {
                "NOT registered (spotify ting register)"
            }
        )
    });
    Ok(())
}

/// `spotify logout`: idempotent.
pub async fn logout(ctx: &Ctx) -> Result<()> {
    let key = ctx.slot();
    let Some(slot) = ctx.home.sessions()?.slots.get(&key).cloned() else {
        let value = json!({"authenticated": false, "revoked": false, "note": "was not logged in"});
        ctx.emit(&value, |_| "Not logged in; nothing to do.".into());
        return Ok(());
    };
    let key_for_logout = format!(
        "spotify-logout-{}",
        &store::refresh_key(&slot.session.refresh_token)["spotify-refresh-".len()..]
    );
    let revoked = match ctx
        .api()?
        .logout(&slot.session.refresh_token, &key_for_logout)
        .await
    {
        Ok(()) => true,
        Err(error) => {
            ctx.hint(&format!(
                "Could not revoke the session in IAM ({}: {}); it was removed locally anyway.",
                error.code, error.message
            ));
            false
        }
    };
    store::remove_slot(&ctx.home, &key)?;
    let value = json!({"authenticated": false, "revoked": revoked});
    ctx.emit(&value, |_| "Logged out.".into());
    Ok(())
}

/// `spotify ting …`.
pub async fn ting(ctx: &Ctx, action: TingCommand) -> Result<()> {
    let key = ctx.slot();
    match action {
        TingCommand::Status => {
            let sessions = ctx.home.sessions()?;
            let slot = sessions.slots.get(&key).ok_or_else(|| {
                Error::not_authenticated("Not logged in, so there is no Ting registration.")
            })?;
            let value = json!({"actor": slot.session.actor, "org_id": slot.session.org_id, "ting": slot.session.ting});
            ctx.emit(&value, |v| {
                format!(
                    "{}: {}",
                    v["actor"]["public_id"].as_str().unwrap_or("?"),
                    if v.pointer("/ting/subscribed") == Some(&Value::Bool(true)) {
                        "registered"
                    } else {
                        "not registered"
                    }
                )
            });
        }
        TingCommand::Register => {
            let api = ctx.api()?;
            let slot = store::fresh_session(&ctx.home, &api, &key, false).await?;
            let org = ctx
                .org(Some(&slot.session.org_id))
                .unwrap_or_else(|| slot.session.org_id.clone());
            let attempt = format!("spotify-ting-register-{}", uuid::Uuid::now_v7().simple());
            let result = api
                .register_ting(&slot.session.access_token, &org, &attempt)
                .await;
            let registration = match &result {
                Ok(value) => silicon_spotify_client::api::TingRegistration {
                    subscribed: true,
                    subscription_id: value.get("id").and_then(Value::as_str).map(str::to_owned),
                    error: None,
                },
                Err(error) => silicon_spotify_client::api::TingRegistration {
                    subscribed: false,
                    subscription_id: None,
                    error: Some(error.clone()),
                },
            };
            {
                let lock = ctx.home.lock()?;
                let mut sessions = ctx.home.sessions()?;
                if let Some(saved) = sessions.slots.get_mut(&key) {
                    saved.session.ting = Some(registration.clone());
                }
                ctx.home.save_sessions(&sessions, &lock)?;
            }
            let value = result?;
            ctx.emit(
                &json!({"ting": registration, "subscription": value}),
                |_| "Registered as a Ting recipient for spotify-cli.".into(),
            );
        }
    }
    Ok(())
}

/// `spotify config …`.
pub async fn config(ctx: &Ctx, action: ConfigCommand) -> Result<()> {
    match action {
        ConfigCommand::Set { settings: text } => {
            let (config, changed) = match ctx.home.config()?.apply_json(&text) {
                // `search_limit=5` (one word, so clap took it): show the JSON form it meant.
                Err(mut error) if error.code == "invalid_input" => {
                    if let Some(advice) = crate::config_key_value_advice(text.split_whitespace()) {
                        error.hint = advice;
                    }
                    return Err(error);
                }
                other => other?,
            };
            ctx.home.save_config(&config)?;
            let value = json!({"updated": store::describe_changes(&changed, &config), "path": ctx.home.config_path()});
            if changed.iter().any(|k| k == "telemetry") && config.telemetry == Some(false) {
                // Clear a running daemon's backlog; never start one just for this.
                let _ = crate::daemon::call_if_running(ctx, "telemetry.clear", json!({})).await;
            }
            ctx.emit(&value, |v| {
                format!(
                    "Saved {} to {}.",
                    v["updated"],
                    v["path"].as_str().unwrap_or("")
                )
            });
        }
        ConfigCommand::Show => {
            let config = ctx.home.config()?;
            let mut value = config.effective();
            value["path"] = json!(ctx.home.config_path());
            value["env_overrides"] = json!({
                "SPOTIFY_API_URL": std::env::var("SPOTIFY_API_URL").ok(),
                "SILICON_ORG": std::env::var("SILICON_ORG").ok(),
                "telemetry_kill_switch": silicon_spotify_client::telemetry::KILL_SWITCHES.iter().find(|k| std::env::var(k).is_ok_and(|v| silicon_spotify_client::telemetry::is_off(&v))),
            });
            ctx.emit(&value, |v| {
                serde_json::to_string_pretty(v).unwrap_or_default()
            });
        }
        ConfigCommand::Get { key } => {
            if !CONFIG_KEYS.iter().any(|(k, ..)| *k == key) {
                return Err(Error::invalid(
                    format!("`{key}` is not a setting."),
                    "See `spotify config keys`.",
                ));
            }
            let value = ctx
                .home
                .config()?
                .effective()
                .get(&key)
                .cloned()
                .unwrap_or(Value::Null);
            ctx.emit(&json!({"key": key, "value": value}), |v| {
                v["value"].to_string()
            });
        }
        ConfigCommand::Keys => {
            let keys: Vec<Value> = CONFIG_KEYS
                .iter()
                .map(|(k, t, d, m)| json!({"key": k, "type": t, "default": d, "description": m}))
                .collect();
            ctx.emit(&json!({"keys": keys}), |v| {
                v["keys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|k| {
                        format!(
                            "{}  ({}, default {})\n    {}",
                            k["key"].as_str().unwrap_or(""),
                            k["type"].as_str().unwrap_or(""),
                            k["default"].as_str().unwrap_or(""),
                            k["description"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        ConfigCommand::Reset => {
            ctx.home.save_config(&Config::default())?;
            ctx.emit(&json!({"reset": true}), |_| {
                "Settings reset to defaults.".into()
            });
        }
    }
    Ok(())
}

/// `spotify testing …`.
pub fn testing(ctx: &Ctx, action: TestingCommand) -> Result<()> {
    match action {
        TestingCommand::Use { app_secret_file } => {
            let secret = read_token(&app_secret_file)?;
            ctx.home.save_testing(Some(&secret))?;
            let value = json!({"testing": true, "slot": store::slot_key(&ctx.api_url, Some(&silicon_spotify_client::api::Testing { app_secret: secret }))});
            ctx.emit(&value, |_| "Testing plane selected. Log in inside it with `spotify login <SLT-or-test-public-id>`.".into());
        }
        TestingCommand::Status => {
            let value = json!({"testing": ctx.testing.is_some(), "source": if std::env::var("SPOTIFY_TEST_APP_SECRET").is_ok() { "env" } else if ctx.testing.is_some() { "testing.json" } else { "none" }});
            ctx.emit(&value, |v| {
                if v["testing"] == Value::Bool(true) {
                    "A testing plane is selected.".into()
                } else {
                    "Production (no testing plane selected).".into()
                }
            });
        }
        TestingCommand::Exit => {
            ctx.home.save_testing(None)?;
            ctx.emit(&json!({"testing": false}), |_| "Back to production.".into());
        }
    }
    Ok(())
}

const PR_PREFIX: &str = "https://github.com/unlikefraction/spotify-cli/pull/";

/// `spotify report`.
pub async fn report(ctx: &Ctx, message: &str, pr: Option<&str>, attach: &[PathBuf]) -> Result<()> {
    let message = message.trim();
    if message.len() < 10 {
        return Err(Error::invalid(
            "The report is too short to act on.",
            "Say what you ran, what happened, what you expected, and how to reproduce it.",
        ));
    }
    if message.len() > 20_000 {
        return Err(Error::invalid(
            "The report is longer than 20 000 characters.",
            "Summarize it and attach long output with --attach.",
        ));
    }
    if let Some(pr) = pr {
        let number = pr.strip_prefix(PR_PREFIX).unwrap_or("");
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::invalid(
                format!("`{pr}` is not a spotify-cli pull request URL."),
                format!(
                    "Use {PR_PREFIX}<number> (open the PR against unlikefraction/spotify-cli first)."
                ),
            ));
        }
    }
    if attach.len() > 5 {
        return Err(Error::invalid(
            "At most 5 attachments.",
            "Combine files first.",
        ));
    }
    let mut attachments = Vec::new();
    for path in attach {
        attachments.push(attachment(ctx, path)?);
    }
    let daemon_version = silicon_spotify_client::ipc::socket_path()
        .ok()
        .filter(|p| p.exists())
        .map(|_| "running");
    let body = json!({
        "message": message,
        "pr": pr,
        "attachments": attachments,
        "context": {"cli_version": VERSION, "os": std::env::consts::OS, "arch": std::env::consts::ARCH, "isi": ctx.isi, "daemon": daemon_version, "testing": ctx.testing.is_some()},
    });
    let bearer = ctx
        .home
        .sessions()
        .ok()
        .and_then(|s| s.slots.get(&ctx.slot()).cloned());
    let key = format!("spotify-report-{}", uuid::Uuid::now_v7().simple());
    let api = ctx.api()?;
    let sent = api
        .report(
            bearer
                .as_ref()
                .map(|s| (s.session.access_token.as_str(), s.session.org_id.as_str())),
            &body,
            &key,
        )
        .await;
    match sent {
        Ok(value) => {
            ctx.emit(&value, |v| {
                format!(
                    "Report filed ({}). {}",
                    v["id"].as_str().unwrap_or("accepted"),
                    v.get("issue_url").and_then(Value::as_str).unwrap_or("")
                )
            });
            if pr.is_none() {
                ctx.hint(&format!("You can also submit a fix: {REPOSITORY} (then `spotify report '<msg>' --pr {PR_PREFIX}<n>`)."));
            }
            Ok(())
        }
        Err(error) if error.retryable || error.code == "backend_unavailable" => {
            let dir = ctx.home.dir.join("reports");
            std::fs::create_dir_all(&dir).map_err(|e| Error::internal(e.to_string()))?;
            let path = dir.join(format!("{key}.json"));
            store::write_json(&path, &body)?;
            let value = json!({"filed": false, "saved": path, "backend_error": error,
                "file_it_yourself": format!("gh issue create --repo unlikefraction/spotify-cli --title '{}' --body-file <(jq -r .message {})", silicon_spotify_client::model::truncate(message.lines().next().unwrap_or(message), 80).replace('\'', ""), path.display())});
            ctx.emit(&value, |v| format!("The backend is unreachable, so the report was saved to {}.\nFile it yourself: {}", v["saved"].as_str().unwrap_or(""), v["file_it_yourself"].as_str().unwrap_or("")));
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Most of a file `spotify report --attach` sends: its end, where the latest log lines are.
const ATTACHMENT_MAX: usize = 64 * 1024;

/// One `--attach` file, checked before anything is sent: a text file (UTF-8, no NUL bytes) that
/// is not one of this home's credential files. Longer files keep their last 64 KiB.
fn attachment(ctx: &Ctx, path: &Path) -> Result<Value> {
    let unreadable = |e: std::io::Error| {
        Error::invalid(
            format!("Cannot read {}: {e}.", path.display()),
            "Check the path.",
        )
    };
    let canonical = std::fs::canonicalize(path).map_err(unreadable)?;
    // This home's session and testing files, and those of any other home (`<home>/.spotify/`).
    let secret = [ctx.home.session_path(), ctx.home.testing_path()]
        .iter()
        .any(|p| std::fs::canonicalize(p).is_ok_and(|p| p == canonical))
        || [path, canonical.as_path()].iter().any(|p| {
            p.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new(".spotify"))
                && p.file_name()
                    .is_some_and(|name| name == "session.json" || name == "testing.json")
        });
    if secret {
        return Err(Error::invalid(
            format!(
                "{} holds spotify-cli credentials and is never attached.",
                path.display()
            ),
            "Attach logs or command output instead; reports never need tokens.",
        ));
    }
    if !canonical.is_file() {
        return Err(Error::invalid(
            format!("{} is not a file.", path.display()),
            "Attach a text file, such as ~/.silicon-spotify/daemon.log or saved command output.",
        ));
    }
    let (head, tail, size) = file_ends(&canonical).map_err(unreadable)?;
    let cut = size > ATTACHMENT_MAX as u64;
    let content = attachment_text(&head, &tail, cut).ok_or_else(|| {
        Error::invalid(
            format!("{} is not a text file.", path.display()),
            "Attach text (logs, command output as UTF-8); describe binary files in the message instead.",
        )
        .with_details(json!({"path": path, "bytes": size}))
    })?;
    Ok(json!({
        "name": path.file_name().map(|n| n.to_string_lossy().into_owned()),
        "content": content,
        "truncated": cut,
    }))
}

/// A file's first 8 KiB, its last [`ATTACHMENT_MAX`] bytes and its size, without reading the rest.
fn file_ends(path: &Path) -> std::io::Result<(Vec<u8>, Vec<u8>, u64)> {
    use std::io::{Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut head = Vec::new();
    file.by_ref().take(8 * 1024).read_to_end(&mut head)?;
    file.seek(SeekFrom::Start(size.saturating_sub(ATTACHMENT_MAX as u64)))?;
    let mut tail = Vec::new();
    file.take(ATTACHMENT_MAX as u64).read_to_end(&mut tail)?;
    Ok((head, tail, size))
}

/// The text of a file's end, starting at a whole character when the limit `cut` one; `None` for
/// binary content (a NUL byte at its start or end, or invalid UTF-8).
fn attachment_text<'a>(head: &[u8], tail: &'a [u8], cut: bool) -> Option<&'a str> {
    if head.contains(&0) || tail.contains(&0) {
        return None;
    }
    // Skip the continuation bytes (at most 3) of a character cut by the limit.
    let start = if cut {
        tail.iter()
            .take(3)
            .take_while(|b| *b & 0xC0 == 0x80)
            .count()
    } else {
        0
    };
    let text = &tail[start..];
    match std::str::from_utf8(text) {
        Ok(text) => Some(text),
        // Only the last character is incomplete (a log being written as it is read): drop it.
        Err(error) if error.error_len().is_none() && text.len() - error.valid_up_to() < 4 => {
            std::str::from_utf8(&text[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        })
        .or_else(|| {
            ["/opt/homebrew/bin", "/usr/local/bin"]
                .iter()
                .map(|d| Path::new(d).join(name))
                .find(|p| p.is_file())
        })
}

/// `spotify doctor`.
pub async fn doctor(ctx: &Ctx) -> Result<()> {
    let mut checks: Vec<Value> = Vec::new();
    let mut push = |name: &str, ok: bool, detail: Value, fix: &str| {
        checks.push(json!({"check": name, "ok": ok, "detail": detail, "fix": if ok { Value::Null } else { json!(fix) }}));
    };
    push(
        "macos",
        cfg!(target_os = "macos"),
        json!(std::env::consts::OS),
        "Spotify control needs macOS; login, config and docs work anywhere.",
    );
    push(
        "store",
        ctx.home.ensure().is_ok(),
        json!(ctx.home.dir),
        "Make $SILICON_HOME (or $HOME) writable.",
    );
    match crate::daemon::daemon_binary() {
        Ok(path) => push("daemon_binary", true, json!(path), ""),
        Err(error) => push(
            "daemon_binary",
            false,
            serde_json::to_value(&error)?,
            &error.hint,
        ),
    }
    let daemon = if cfg!(target_os = "macos") {
        crate::daemon::call(ctx, "doctor", json!({"settings": ctx.settings()})).await
    } else {
        Err(Error::platform_unsupported("The daemon"))
    };
    match daemon {
        Ok(value) => {
            for check in value["checks"].as_array().cloned().unwrap_or_default() {
                checks.push(check);
            }
        }
        Err(error) => push("daemon", false, serde_json::to_value(&error)?, &error.hint),
    }
    let agent = silicon_spotify_client::ipc::real_home().is_some_and(|h| {
        h.join("Library/LaunchAgents")
            .join(format!("{}.plist", crate::daemon::LABEL))
            .is_file()
    });
    checks.push(json!({"check": "daemon_starts_at_login", "ok": agent, "detail": agent, "fix": if agent { Value::Null } else { json!("spotify daemon install") }, "optional": true}));
    // Backend reachability, whether it still supports this CLI (`min_cli`), and login. Both
    // requests run together; offline, the version check is simply left out.
    let api = ctx.api()?;
    match tokio::join!(api.iam(), api.version()) {
        (Ok(value), version) => {
            let version = version.unwrap_or_else(|error| json!({"error": error}));
            checks.push(json!({"check": "backend_reachable", "ok": value.get("app_id").and_then(Value::as_str) == Some(APP_ID), "detail": {"api_url": ctx.api_url, "iam": value, "version": version}, "fix": Value::Null}));
            if let Some(check) = cli_version_check(&version, VERSION) {
                checks.push(check);
            }
        }
        (Err(error), _) => checks.push(json!({"check": "backend_reachable", "ok": false, "detail": {"api_url": ctx.api_url, "error": error}, "fix": "Check the network and `spotify config get api_url`."})),
    }
    let logged_in = ctx.home.sessions()?.slots.get(&ctx.slot()).cloned();
    let ting_ok = logged_in
        .as_ref()
        .and_then(|s| s.session.ting.as_ref())
        .is_some_and(|t| t.subscribed);
    checks.push(json!({"check": "iam_login", "ok": logged_in.is_some(), "detail": logged_in.as_ref().map(|s| &s.session.actor), "fix": if logged_in.is_some() { Value::Null } else { json!("Needed for Ting triggers: spotify login '<SLT>' (spotify login --help)") }, "optional": true}));
    checks.push(json!({"check": "ting_recipient", "ok": ting_ok, "detail": logged_in.as_ref().and_then(|s| s.session.ting.clone()), "fix": if ting_ok { Value::Null } else { json!("spotify ting register (after spotify login)") }, "optional": true}));
    let required_ok = checks.iter().all(|c| {
        c["ok"] == Value::Bool(true)
            || c.get("optional") == Some(&Value::Bool(true))
            || c["check"] == json!("spotify_player_warm_instance")
    });
    let value = json!({"ok": required_ok, "checks": checks, "version": VERSION});
    ctx.emit(&value, |v| {
        let mut out = String::new();
        for c in v["checks"].as_array().into_iter().flatten() {
            let mark = if c["ok"] == Value::Bool(true) {
                "✓"
            } else if c.get("optional") == Some(&Value::Bool(true))
                || c["check"] == json!("spotify_player_warm_instance")
            {
                "·"
            } else {
                "✗"
            };
            out.push_str(&format!("{mark} {}", c["check"].as_str().unwrap_or("")));
            if c["check"] == json!("spotify_player_warm_instance") && c["ok"] != Value::Bool(true) {
                out.push_str(&format!(
                    "\n    state: {}",
                    warm_check_state(&c["detail"]).replace('\n', "\n    ")
                ));
            }
            if let Some(fix) = c.get("fix").and_then(Value::as_str) {
                out.push_str(&format!("\n    fix: {fix}"));
            }
            out.push('\n');
        }
        out.push_str(if v["ok"] == Value::Bool(true) {
            "Ready."
        } else {
            "Some required checks failed; run the fixes above (or `spotify setup`)."
        });
        out
    });
    if value["ok"] == Value::Bool(false) {
        return Err(Error::new("doctor_failed", "One or more required checks failed.", "Run the listed fixes, or `spotify setup`.").with_details(json!({"failed": value["checks"].as_array().map(|c| c.iter().filter(|x| x["ok"] == Value::Bool(false) && x.get("optional") != Some(&Value::Bool(true))).map(|x| x["check"].clone()).collect::<Vec<_>>())})));
    }
    Ok(())
}

/// The failing `spotify_player_warm_instance` check's detail (the daemon's warm state) in words,
/// plus who holds the client port now when that is not the daemon's copy.
fn warm_check_state(detail: &Value) -> String {
    let mut state = crate::render::warm_player(detail);
    // The daemon runs a copy, but another process answers spotify_player commands.
    let own = detail.get("pid").and_then(Value::as_u64);
    if let Some(owner) = detail
        .get("port_owner_pid_now")
        .and_then(Value::as_u64)
        .filter(|owner| own.is_some_and(|own| own != *owner))
    {
        let at = state.find('\n').unwrap_or(state.len());
        state.insert_str(at, &format!("; the port is held by pid {owner} now"));
    }
    state
}

/// `cli_version_supported` from the backend's `GET /api/v1/version`: fails when this CLI is older
/// than its `min_cli`. `None` when the answer has no usable `min_cli`.
fn cli_version_check(version: &Value, cli: &str) -> Option<Value> {
    let min_cli = version.get("min_cli").and_then(Value::as_str)?;
    let minimum = semver::Version::parse(min_cli).ok()?;
    let current = semver::Version::parse(cli).ok()?;
    let ok = current >= minimum;
    Some(json!({
        "check": "cli_version_supported",
        "ok": ok,
        "detail": {"cli": cli, "min_cli": min_cli, "backend": version.get("version")},
        "fix": if ok { Value::Null } else { json!(format!("spotify update (the backend supports CLI {min_cli} and newer; this is {cli})")) },
    }))
}

fn brew(args: &[&str]) -> Result<()> {
    let brew = which("brew").ok_or_else(|| Error::new("homebrew_missing", "Homebrew is not installed, so dependencies cannot be installed automatically.", "Install Homebrew from https://brew.sh, or install spotify_player with `cargo install spotify_player`."))?;
    let status = Command::new(brew)
        .args(args)
        .status()
        .map_err(|e| Error::internal(e.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(
            "install_failed",
            format!("`brew {}` failed.", args.join(" ")),
            "Run it yourself to see the error.",
        ))
    }
}

/// `spotify setup`.
pub async fn setup(ctx: &Ctx, check: bool, install_apps: bool, no_auth: bool) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(Error::platform_unsupported("Setup"));
    }
    let mut steps = Vec::new();
    // 1. Spotify.app
    let app = Path::new("/Applications/Spotify.app").exists()
        || silicon_spotify_client::ipc::real_home()
            .is_some_and(|h| h.join("Applications/Spotify.app").exists());
    if app {
        steps.push(json!({"step": "spotify_app", "status": "present"}));
    } else if check || !install_apps {
        steps.push(json!({"step": "spotify_app", "status": "missing", "fix": "brew install --cask spotify (or rerun with --install-apps)"}));
    } else {
        brew(&["install", "--cask", "spotify"])?;
        steps.push(json!({"step": "spotify_app", "status": "installed"}));
    }
    // 2. spotify_player
    let player = which("spotify_player").is_some()
        || Path::new(&format!(
            "{}/.cargo/bin/spotify_player",
            std::env::var("HOME").unwrap_or_default()
        ))
        .exists();
    if player {
        steps.push(json!({"step": "spotify_player", "status": "present"}));
    } else if check {
        steps.push(json!({"step": "spotify_player", "status": "missing", "fix": "brew install spotify_player"}));
    } else {
        brew(&["install", "spotify_player"])?;
        steps.push(json!({"step": "spotify_player", "status": "installed"}));
    }
    // 3. daemon at login + running
    if check {
        steps.push(json!({"step": "daemon", "status": if crate::daemon::start().await.is_ok() { "running" } else { "not running" }}));
    } else {
        crate::daemon::command(
            &Ctx {
                json: true,
                ..clone_ctx(ctx)
            },
            crate::args::DaemonCommand::Install,
        )
        .await
        .or_else(|e| {
            steps.push(json!({"step": "launch_agent", "status": "failed", "error": e}));
            Ok::<(), Error>(())
        })?;
        steps.push(json!({"step": "daemon", "status": "installed_and_running"}));
    }
    // 4. spotify_player sign-in
    let auth = crate::daemon::call(
        ctx,
        "spotify.auth.status",
        json!({"settings": ctx.settings()}),
    )
    .await
    .unwrap_or(Value::Null);
    if auth["authenticated"] == Value::Bool(true) {
        steps.push(json!({"step": "spotify_player_auth", "status": "signed_in"}));
    } else if check || no_auth {
        steps.push(json!({"step": "spotify_player_auth", "status": "not_signed_in", "fix": "spotify auth login"}));
    } else {
        let started = crate::daemon::call(
            ctx,
            "spotify.auth.login",
            json!({"settings": ctx.settings()}),
        )
        .await?;
        steps.push(
            json!({"step": "spotify_player_auth", "status": "browser_opened", "detail": started}),
        );
    }
    let value =
        json!({"steps": steps, "next": "spotify doctor · spotify login '<SLT>' (for triggers)"});
    ctx.emit(&value, |v| {
        let mut out = String::new();
        for step in v["steps"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "{}: {}",
                step["step"].as_str().unwrap_or(""),
                step["status"].as_str().unwrap_or("")
            ));
            if let Some(fix) = step.get("fix").and_then(Value::as_str) {
                out.push_str(&format!("  → {fix}"));
            }
            out.push('\n');
        }
        out.push_str("Next: spotify doctor, then spotify login '<SLT>' for triggers.");
        out
    });
    Ok(())
}

fn clone_ctx(ctx: &Ctx) -> Ctx {
    Ctx {
        json: ctx.json,
        home: ctx.home.clone(),
        config: ctx.config.clone(),
        api_url: ctx.api_url.clone(),
        testing: ctx.testing.clone(),
        org_flag: ctx.org_flag.clone(),
        telemetry: ctx.telemetry,
        trace_id: ctx.trace_id.clone(),
        isi: ctx.isi.clone(),
        command: ctx.command.clone(),
    }
}

/// `spotify auth …` (spotify_player's Spotify account).
pub async fn auth(ctx: &Ctx, action: AuthCommand) -> Result<()> {
    match action {
        AuthCommand::Status => {
            let value = crate::daemon::call(
                ctx,
                "spotify.auth.status",
                json!({"settings": ctx.settings()}),
            )
            .await?;
            ctx.emit(&value, |v| {
                if v["authenticated"] == Value::Bool(true) {
                    format!(
                        "spotify_player {} is signed in to Spotify.",
                        v["version"].as_str().unwrap_or("")
                    )
                } else if v["installed"] == Value::Bool(false) {
                    "spotify_player is not installed: brew install spotify_player".into()
                } else {
                    format!(
                        "spotify_player is not signed in: {}",
                        v.pointer("/error/hint")
                            .and_then(Value::as_str)
                            .unwrap_or("run `spotify auth login`")
                    )
                }
            });
        }
        AuthCommand::Login => {
            let value = crate::daemon::call(
                ctx,
                "spotify.auth.login",
                json!({"settings": ctx.settings()}),
            )
            .await?;
            ctx.emit(&value, |v| v["next"].as_str().unwrap_or("").to_owned());
        }
    }
    Ok(())
}

/// `spotify update`.
pub async fn update(ctx: &Ctx, check: bool) -> Result<()> {
    let op = if check {
        "update.check"
    } else {
        "update.apply"
    };
    let value = crate::daemon::call(ctx, op, json!({"settings": ctx.settings()})).await?;
    ctx.emit(&value, |v| match v.get("manager").and_then(Value::as_str) {
        Some("honeycomb") => format!(
            "Installed by Honeycomb: {}",
            v["note"].as_str().unwrap_or("")
        ),
        _ if v["update_available"] == Value::Bool(true) && v["restarting"] == Value::Bool(true) => {
            format!(
                "Updated {} → {}; the daemon restarted.",
                VERSION,
                v["latest"].as_str().unwrap_or("?")
            )
        }
        _ if v["update_available"] == Value::Bool(true) => format!(
            "Version {} is available (you have {VERSION}). Run `spotify update`.",
            v["latest"].as_str().unwrap_or("?")
        ),
        _ => format!("Up to date ({VERSION})."),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_says_who_holds_the_warm_port() {
        let detail = json!({"state": "running", "pid": 812, "port": 8080, "refresh_ms": 3000,
            "serves_cli": true, "port_owner_pid_now": 999});
        assert_eq!(
            warm_check_state(&detail),
            "running (pid 812, 127.0.0.1:8080, playback refresh every 3 s, serves spotify-cli); the port is held by pid 999 now"
        );
        // Before the note line, which stays on its own line.
        let detail = json!({"state": "running", "pid": 812, "port": 8080, "serves_cli": null,
            "note": "lsof could not confirm that this copy holds the client port.", "port_owner_pid_now": 7});
        assert!(
            warm_check_state(&detail).starts_with(
                "running (pid 812, 127.0.0.1:8080); the port is held by pid 7 now\n    lsof"
            ),
            "{}",
            warm_check_state(&detail)
        );
        // Its own copy, or no copy of its own (deferred): nothing to add.
        let detail =
            json!({"state": "starting", "pid": 812, "port": 8080, "port_owner_pid_now": 812});
        assert!(!warm_check_state(&detail).contains("held by pid"));
        let detail = json!({"state": "deferred", "port": 8080, "port_owner": {"pid": 5, "kind": "your_spotify_player"},
            "port_owner_pid_now": 5});
        assert!(
            !warm_check_state(&detail).contains("now"),
            "{}",
            warm_check_state(&detail)
        );
    }

    #[test]
    fn doctor_flags_a_cli_older_than_min_cli() {
        let version = json!({"version": "0.2.0", "api_versions": ["v1"], "min_cli": "0.1.0"});
        let check = cli_version_check(&version, "0.1.1").expect("check");
        assert_eq!(check["check"], "cli_version_supported");
        assert_eq!(check["ok"], true);
        assert!(check["fix"].is_null());
        let check = cli_version_check(&json!({"min_cli": "0.3.0"}), "0.2.9").expect("check");
        assert_eq!(check["ok"], false);
        assert!(
            check["fix"]
                .as_str()
                .expect("fix")
                .starts_with("spotify update")
        );
        // An older backend without min_cli (or an unreachable one) adds no check.
        assert!(cli_version_check(&json!({"error": {"code": "not_found"}}), "0.1.1").is_none());
        assert!(cli_version_check(&json!({"min_cli": "soon"}), "0.1.1").is_none());
    }

    #[test]
    fn attachments_are_text_only() {
        assert_eq!(
            attachment_text(b"line 1\nline 2\n", b"line 1\nline 2\n", false),
            Some("line 1\nline 2\n")
        );
        // Binary: a NUL byte at the start (Mach-O, sqlite) or the end, or invalid UTF-8.
        assert_eq!(
            attachment_text(b"\xcf\xfa\xed\xfe\0\0", b"text", true),
            None
        );
        assert_eq!(attachment_text(b"text", b"te\0xt", true), None);
        assert_eq!(attachment_text(b"\xff\xfe", b"\xff\xfe", false), None);
        // A character cut by the 64 KiB limit is dropped, not turned into U+FFFD or a refusal.
        let tail = "é log line".as_bytes();
        assert_eq!(
            attachment_text(b"head", &tail[1..], true),
            Some(" log line")
        );
        assert_eq!(attachment_text(b"head", &tail[1..], false), None);
        // A last character still being written is dropped too; invalid bytes are not.
        let text = "log line é".as_bytes();
        assert_eq!(
            attachment_text(b"log", &text[..text.len() - 1], false),
            Some("log line ")
        );
        assert_eq!(attachment_text(b"log", b"log \xe9 line", false), None);
    }
}
