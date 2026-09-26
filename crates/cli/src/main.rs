//! `spotify`: the spotify-cli command. A stateful client of the stateless
//! `silicon-spotify-client` library and of `spotify-daemon`.

mod account;
mod args;
mod daemon;
mod docs;
mod ops;
mod render;

use std::io::IsTerminal as _;
use std::time::Instant;

use clap::CommandFactory as _;
use serde_json::{Value, json};
use silicon_spotify_client::api::{Api, Testing};
use silicon_spotify_client::store::{Config, Home, slot_key};
use silicon_spotify_client::{Error, Result};

use crate::args::{Cli, Command};

/// Everything a command needs.
pub struct Ctx {
    /// `--json` (or config output=json).
    pub json: bool,
    /// The Silicon's home.
    pub home: Home,
    /// Saved settings.
    pub config: Config,
    /// Effective backend origin.
    pub api_url: String,
    /// Selected testing plane.
    pub testing: Option<Testing>,
    /// `--org`.
    pub org_flag: Option<String>,
    /// Effective telemetry preference.
    pub telemetry: bool,
    /// Trace id for this invocation.
    pub trace_id: String,
    /// `ISI` env (routing hint only).
    pub isi: Option<String>,
    /// The command path for telemetry (`trigger add`).
    pub command: String,
}

impl Ctx {
    /// Backend client with testing, telemetry and trace applied.
    ///
    /// # Errors
    /// Bad API URL.
    pub fn api(&self) -> Result<Api> {
        Ok(Api::new(&self.api_url, "cli")?
            .with_testing(self.testing.clone())
            .with_telemetry(self.telemetry)
            .with_trace(Some(self.trace_id.clone())))
    }

    /// This home's session slot for the current backend and plane.
    #[must_use]
    pub fn slot(&self) -> String {
        slot_key(&self.api_url, self.testing.as_ref())
    }

    /// Org precedence: --org, SILICON_ORG, config org, then the session's.
    #[must_use]
    pub fn org(&self, session_org: Option<&str>) -> Option<String> {
        self.org_flag
            .clone()
            .or_else(|| {
                std::env::var("SILICON_ORG")
                    .ok()
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
            })
            .or_else(|| self.config.org.clone())
            .or_else(|| session_org.map(str::to_owned))
    }

    /// Control settings forwarded to the daemon with every request.
    #[must_use]
    pub fn settings(&self) -> Value {
        json!({
            "strategy": self.config.strategy,
            "launch_spotify": self.config.launch_spotify,
            "verify_timeout_ms": self.config.verify_timeout_ms,
            "spotify_player_binary": self.config.spotify_player_binary,
            "spotify_player_config_dir": self.config.spotify_player_config_dir,
            "spotify_player_cache_dir": self.config.spotify_player_cache_dir,
            "telemetry": self.telemetry,
            "api_url": self.api_url,
            "auto_update": self.config.auto_update,
        })
    }

    /// Calls the daemon (starting it when needed). `args` gets `settings` merged in.
    ///
    /// # Errors
    /// Daemon or operation errors.
    pub async fn daemon(&self, op: &str, mut args: Value) -> Result<Value> {
        if let Some(object) = args.as_object_mut() {
            object.insert("settings".into(), self.settings());
        }
        daemon::call(self, op, args).await
    }

    /// Prints a success value.
    pub fn emit(&self, value: &Value, human: impl FnOnce(&Value) -> String) {
        if self.json {
            println!("{}", serde_json::to_string(value).unwrap_or_default());
        } else {
            let text = human(value);
            if !text.is_empty() {
                println!("{}", text.trim_end());
            }
        }
    }

    /// A next-step suggestion (stderr, human mode only).
    pub fn hint(&self, text: &str) {
        if !self.json {
            eprintln!("{text}");
        }
    }
}

fn command_path(command: &Command) -> String {
    let debug = format!("{command:?}");
    let head: String = debug.chars().take_while(|c| c.is_alphanumeric()).collect();
    head.to_ascii_lowercase()
}

fn print_error(json: bool, error: &Error) {
    if json {
        eprintln!(
            "{}",
            serde_json::to_string(&json!({"error": error})).unwrap_or_default()
        );
        return;
    }
    eprintln!("error: {}", error.message);
    if !error.hint.is_empty() {
        eprintln!("hint: {}", error.hint);
    }
    let mut meta = vec![format!("code: {}", error.code)];
    if error.retryable {
        meta.push("retryable".into());
    }
    if let Some(request) = error
        .details
        .as_ref()
        .and_then(|d| d.get("request_id"))
        .and_then(Value::as_str)
    {
        meta.push(format!("request: {request}"));
    }
    eprintln!("({})", meta.join(", "));
    if let Some(details) = &error.details {
        if std::env::var("SPOTIFY_DEBUG").is_ok() {
            eprintln!("details: {details}");
        } else if details.get("first_attempt").is_some() || details.get("stderr").is_some() {
            eprintln!(
                "details: {}",
                silicon_spotify_client::model::truncate(&details.to_string(), 600)
            );
        }
    }
}

/// A clap parse error as a `usage` error, keeping everything clap says: the missing arguments,
/// the possible values, its tips and the usage line.
///
/// clap renders `error: <what>` with indented continuation lines, then blank-line separated
/// `tip:` lines, `Usage: …` and a `--help` pointer.
fn usage_error(rendered: &str) -> Error {
    let mut lines = rendered.lines();
    let first = lines
        .next()
        .unwrap_or("invalid arguments")
        .trim_start_matches("error: ")
        .trim()
        .to_owned();
    let mut listed = Vec::new();
    let mut possible: Vec<String> = Vec::new();
    let mut tips = Vec::new();
    let mut usage = None;
    let mut in_error_block = true;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_error_block = false;
        } else if let Some(values) = trimmed
            .strip_prefix("[possible values: ")
            .and_then(|v| v.strip_suffix(']'))
        {
            possible = values.split(", ").map(str::to_owned).collect();
        } else if let Some(tip) = trimmed.strip_prefix("tip: ") {
            tips.push(tip.to_owned());
        } else if trimmed.starts_with("Usage:") {
            usage = Some(trimmed.to_owned());
        } else if in_error_block {
            listed.push(trimmed.to_owned());
        }
    }
    let message = if listed.is_empty() {
        first
    } else {
        format!("{first} {}", listed.join(", "))
    };
    let mut hint = Vec::new();
    if !possible.is_empty() {
        hint.push(format!("Possible values: {}.", possible.join(", ")));
    }
    for tip in &tips {
        let mut chars = tip.chars();
        let tip: String = chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
        hint.push(format!("{}.", tip.trim_end_matches('.')));
    }
    if let Some(usage) = &usage {
        // `<QUERY>...` already ends in dots.
        hint.push(if usage.ends_with('.') {
            usage.clone()
        } else {
            format!("{usage}.")
        });
    }
    hint.push("Run the command with --help for arguments and examples.".into());
    let mut details = serde_json::Map::new();
    if !possible.is_empty() {
        details.insert("possible_values".into(), json!(possible));
    }
    if let Some(usage) = usage {
        details.insert(
            "usage".into(),
            json!(usage.trim_start_matches("Usage:").trim()),
        );
    }
    let error = Error::new("usage", message, hint.join(" "));
    if details.is_empty() {
        error
    } else {
        error.with_details(Value::Object(details))
    }
}

fn build_ctx(cli: &Cli) -> Result<Ctx> {
    let home = Home::resolve()?;
    let config = home.config()?;
    let api_url = match &cli.global.api_url {
        Some(url) => {
            silicon_spotify_client::api::validate_base(url)?;
            url.clone()
        }
        None => config.api_url(),
    };
    let testing = home.testing()?;
    let json = cli.global.json || config.output.as_deref() == Some("json");
    if let Some(org) = &cli.global.org
        && !silicon_spotify_client::store::valid_handle(org, 3, 50)
    {
        return Err(Error::invalid(
            format!("`{org}` is not an organization handle."),
            "Pass the bare handle, e.g. --org unlikefraction.",
        ));
    }
    if testing.is_some() && !json && std::io::stderr().is_terminal() {
        eprintln!("[testing plane selected · leave with `spotify testing exit`]");
    }
    Ok(Ctx {
        json,
        telemetry: silicon_spotify_client::telemetry::enabled(config.telemetry),
        home,
        config,
        api_url,
        testing,
        org_flag: cli.global.org.clone(),
        trace_id: uuid::Uuid::now_v7().to_string(),
        isi: std::env::var("ISI").ok().filter(|v| !v.trim().is_empty()),
        command: command_path(&cli.command),
    })
}

async fn run(cli: Cli, ctx: &Ctx) -> Result<()> {
    match cli.command {
        Command::Iam => account::iam(ctx),
        Command::Login(args) => account::login(ctx, args).await,
        Command::Logout => account::logout(ctx).await,
        Command::Ting { action } => account::ting(ctx, action).await,
        Command::Config { action } => account::config(ctx, action).await,
        Command::Testing { action } => account::testing(ctx, action),
        Command::Report {
            message,
            pr,
            attach,
        } => account::report(ctx, &message, pr.as_deref(), &attach).await,
        Command::Docs { topic, search, all } => {
            docs::docs(ctx, topic.as_deref(), search.as_deref(), all)
        }
        Command::Commands => docs::commands(ctx),
        Command::Daemon { action } => daemon::command(ctx, action).await,
        Command::Doctor => account::doctor(ctx).await,
        Command::Setup {
            check,
            install_apps,
            no_auth,
        } => account::setup(ctx, check, install_apps, no_auth).await,
        Command::Update { check } => account::update(ctx, check).await,
        Command::Auth { action } => account::auth(ctx, action).await,
        other => ops::run(ctx, other).await,
    }
}

fn record(ctx: &Ctx, started: Instant, result: &Result<()>) {
    if !ctx.telemetry || matches!(ctx.command.as_str(), "docs" | "commands" | "iam") {
        return;
    }
    let mut builder = silicon_spotify_client::telemetry::Builder::new(
        "cli",
        if ctx.testing.is_some() {
            "testing"
        } else {
            "production"
        },
    );
    builder.trace_id.clone_from(&ctx.trace_id);
    let mut context = serde_json::Map::new();
    context.insert("command".into(), json!(ctx.command));
    context.insert("json".into(), json!(ctx.json));
    let event = builder.event(
        "cli.command.completed",
        &ctx.command,
        if result.is_ok() { "ok" } else { "error" },
        result.as_ref().err().map(|e| e.code.as_str()),
        u64::try_from(started.elapsed().as_millis()).ok(),
        context,
    );
    daemon::record_telemetry(ctx, event);
}

fn main() {
    let matches = Cli::command().try_get_matches();
    let cli = match matches.and_then(|m| <Cli as clap::FromArgMatches>::from_arg_matches(&m)) {
        Ok(cli) => cli,
        Err(error) => {
            let wants_json = std::env::args().any(|a| a == "--json");
            match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    let _ = error.print();
                    std::process::exit(0);
                }
                clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    let _ = error.print();
                    std::process::exit(2);
                }
                _ if wants_json => {
                    print_error(true, &usage_error(&error.render().to_string()));
                    std::process::exit(2);
                }
                _ => {
                    let _ = error.print();
                    eprintln!(
                        "\nRun `spotify <command> --help` for arguments and examples, or `spotify commands` to explore."
                    );
                    std::process::exit(2);
                }
            }
        }
    };
    let wants_json = cli.global.json;
    let ctx = match build_ctx(&cli) {
        Ok(ctx) => ctx,
        Err(error) => {
            print_error(wants_json, &error);
            std::process::exit(error.exit_code());
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            print_error(
                ctx.json,
                &Error::internal(format!("cannot start the async runtime: {error}")),
            );
            std::process::exit(1);
        }
    };
    let started = Instant::now();
    let result = runtime.block_on(run(cli, &ctx));
    record(&ctx, started, &result);
    if let Err(error) = result {
        print_error(ctx.json, &error);
        drop(runtime);
        std::process::exit(error.exit_code());
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;
    use serde_json::json;

    #[test]
    fn grammar_is_consistent() {
        // Catches duplicate argument ids, bad defaults and conflicting names at test time.
        crate::args::Cli::command().debug_assert();
    }

    fn parse_error(args: &[&str]) -> silicon_spotify_client::Error {
        let error = crate::args::Cli::command()
            .try_get_matches_from(args)
            .expect_err("usage error");
        super::usage_error(&error.render().to_string())
    }

    #[test]
    fn usage_errors_keep_what_clap_says() {
        let error = parse_error(&["spotify", "config", "set", "--json"]);
        assert_eq!(error.code, "usage");
        assert_eq!(
            error.message,
            "the following required arguments were not provided: <JSON>"
        );
        assert!(
            error
                .hint
                .contains("Usage: spotify config set --json <JSON>."),
            "{}",
            error.hint
        );
        let error = parse_error(&["spotify", "search", "x", "--type", "bogus", "--json"]);
        assert_eq!(error.message, "invalid value 'bogus' for '--type <KINDS>'");
        assert!(
            error
                .hint
                .starts_with("Possible values: track, album, artist, playlist, show, episode."),
            "{}",
            error.hint
        );
        assert_eq!(
            error
                .details
                .as_ref()
                .map(|d| d["possible_values"][0].clone()),
            Some(json!("track"))
        );
        let error = parse_error(&["spotify", "search", "x", "--limt", "3"]);
        assert!(
            error.hint.contains("A similar argument exists: '--limit'."),
            "{}",
            error.hint
        );
        assert!(
            error.hint.contains("<QUERY>... Run the command"),
            "no extra period after `...`: {}",
            error.hint
        );
        let error = parse_error(&["spotify", "trigger", "add"]);
        assert!(error.message.contains("--remaining"), "{}", error.message);
        assert!(!error.hint.starts_with('.'), "{}", error.hint);
    }

    /// Help is wrapped at 100 columns (`max_term_width`), so an indented example line longer
    /// than that would be broken in two and no longer copyable.
    #[test]
    fn help_examples_fit_on_one_line() {
        fn walk(command: &clap::Command, path: &str, long: &mut Vec<String>) {
            let texts = [
                command.get_about(),
                command.get_long_about(),
                command.get_after_help(),
                command.get_after_long_help(),
            ];
            for text in texts.into_iter().flatten() {
                for line in text.to_string().lines() {
                    if line.starts_with("  ") && line.chars().count() > 100 {
                        long.push(format!("{path}: {line}"));
                    }
                }
            }
            for sub in command.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), long);
            }
        }
        let mut long = Vec::new();
        walk(&crate::args::Cli::command(), "spotify", &mut long);
        assert!(
            long.is_empty(),
            "example lines over 100 columns:\n{}",
            long.join("\n")
        );
    }
}
