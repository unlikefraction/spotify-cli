//! `spotify`: the spotify-cli command. A stateful client of the stateless
//! `silicon-spotify-client` library and of `spotify-daemon`.

/// A line on stderr. Unlike `eprintln!` it never panics: a reader that has gone away
/// (`spotify … 2>&1 | head`) must not turn a result into a crash with exit 101.
macro_rules! err_line {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), $($arg)*);
    }};
}

mod account;
mod args;
mod catalog;
mod daemon;
mod docs;
mod how;
mod next;
mod ops;
mod orient;
mod render;
#[cfg(test)]
mod shell;

use std::io::IsTerminal as _;
use std::time::Instant;

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
            "keep_spotify_in_background": self.config.keep_spotify_in_background,
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

    /// Prints a success value. A reader that stops early (`| head`) is not an error.
    pub fn emit(&self, value: &Value, human: impl FnOnce(&Value) -> String) {
        use std::io::Write as _;
        let text = if self.json {
            serde_json::to_string(value).unwrap_or_default()
        } else {
            human(value).trim_end().to_owned()
        };
        if self.json || !text.is_empty() {
            let _ = writeln!(std::io::stdout().lock(), "{text}");
        }
    }

    /// A next-step suggestion (stderr, human mode only).
    pub fn hint(&self, text: &str) {
        if !self.json {
            err_line!("{text}");
        }
    }

    /// `Next:` suggestions after human output (stderr; only when stdout is a terminal, unless
    /// SPOTIFY_HINTS=always; never with --json or SPOTIFY_HINTS=0).
    pub fn next(&self, steps: &[String]) {
        if self.json || !next::enabled() {
            return;
        }
        if let Some(text) = next::format(steps) {
            err_line!("{text}");
        }
    }
}

fn command_path(command: Option<&Command>) -> String {
    let Some(command) = command else {
        return "orientation".into();
    };
    let debug = format!("{command:?}");
    let head: String = debug.chars().take_while(|c| c.is_alphanumeric()).collect();
    head.to_ascii_lowercase()
}

fn print_error(json: bool, error: &Error) {
    if json {
        err_line!(
            "{}",
            serde_json::to_string(&json!({"error": error})).unwrap_or_default()
        );
        return;
    }
    err_line!("error: {}", error.message);
    if !error.hint.is_empty() {
        err_line!("hint: {}", error.hint);
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
    err_line!("({})", meta.join(", "));
    if let Some(details) = &error.details {
        if std::env::var("SPOTIFY_DEBUG").is_ok() {
            err_line!("details: {details}");
        } else if details.get("first_attempt").is_some() || details.get("stderr").is_some() {
            err_line!(
                "details: {}",
                silicon_spotify_client::model::truncate(&details.to_string(), 600)
            );
        }
    }
}

/// What clap says about a parse error, read from its rendering: `error: <what>` with indented
/// continuation lines, then blank-line separated `tip:` lines, `Usage: …` and a `--help` pointer.
#[derive(Debug, Default)]
struct ClapText {
    message: String,
    possible: Vec<String>,
    tips: Vec<String>,
    usage: Option<String>,
}

fn clap_text(rendered: &str) -> ClapText {
    let mut lines = rendered.lines();
    let first = lines
        .next()
        .unwrap_or("invalid arguments")
        .trim_start_matches("error: ")
        .trim()
        .to_owned();
    let mut text = ClapText::default();
    let mut listed = Vec::new();
    let mut in_error_block = true;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_error_block = false;
        } else if let Some(values) = trimmed
            .strip_prefix("[possible values: ")
            .and_then(|v| v.strip_suffix(']'))
        {
            text.possible = values.split(", ").map(str::to_owned).collect();
        } else if let Some(tip) = trimmed.strip_prefix("tip: ") {
            text.tips.push(tip.to_owned());
        } else if trimmed.starts_with("Usage:") {
            text.usage = Some(trimmed.to_owned());
        } else if in_error_block {
            listed.push(trimmed.to_owned());
        }
    }
    text.message = if listed.is_empty() {
        first
    } else {
        format!("{first} {}", listed.join(", "))
    };
    text
}

/// The command a command line names, and the positional arguments it was given. Follows
/// subcommand names and aliases, and skips options with the values they take.
fn invoked<'a>(root: &'a clap::Command, argv: &[String]) -> (&'a clap::Command, Vec<String>) {
    let mut command = root;
    let mut positionals = Vec::new();
    let mut tokens = argv.iter().skip(1);
    while let Some(token) = tokens.next() {
        if token == "--" {
            positionals.extend(tokens.by_ref().cloned());
        } else if let Some(long) = token.strip_prefix("--") {
            let takes_value = !long.contains('=')
                && command
                    .get_arguments()
                    .find(|a| a.get_long() == Some(long))
                    .is_some_and(|a| a.get_action().takes_values());
            if takes_value {
                tokens.next();
            }
        } else if token.starts_with('-') && token.len() > 1 {
            // Short flags (-h, -V) take no values.
        } else if let Some(sub) = command
            .find_subcommand(token)
            .filter(|_| positionals.is_empty())
        {
            command = sub;
        } else {
            positionals.push(token.clone());
        }
    }
    (command, positionals)
}

/// For an unexpected `-x` right after an option that takes a value (`--note -x`): clap reads
/// `-x` as a flag, and its generic tip (`-- -x`) would make it a positional argument instead;
/// `--note=-x` is what works.
fn value_advice(unexpected: &str, argv: &[String], command: &clap::Command) -> Option<String> {
    // `--note --lable x` is a mistyped flag (clap suggests `--label`), not a value.
    if unexpected.starts_with("--") {
        return None;
    }
    let at = argv.iter().position(|a| a == unexpected)?;
    let option = argv.get(at.checked_sub(1)?)?;
    let long = option.strip_prefix("--")?;
    command
        .get_arguments()
        .find(|a| a.get_long() == Some(long))
        .filter(|a| a.get_action().takes_values())?;
    Some(format!(
        "to pass '{unexpected}' as the value of {option}, write {option}={unexpected}"
    ))
}

/// clap's generic tip for a hyphenated value: "to pass '-x' as a value, use '-- -x'".
fn is_dash_dash_tip(tip: &str) -> bool {
    tip.starts_with("to pass '") && tip.contains("' as a value, use '-- ")
}

/// Keys look like `search_limit`; `config set '{"a": 1}' extra` is something else.
fn config_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The JSON form `config set` takes for settings typed as `key value` or `key=value` words:
/// values that read as JSON scalars keep their type, anything else becomes a string.
fn config_json_form(pairs: &[(&str, &str)]) -> String {
    let pairs: Vec<String> = pairs
        .iter()
        .map(|(key, value)| {
            let value = serde_json::from_str::<Value>(value)
                .ok()
                .filter(|v| !v.is_object() && !v.is_array())
                .unwrap_or_else(|| Value::String((*value).to_owned()));
            format!("{}: {value}", Value::String((*key).to_owned()))
        })
        .collect();
    // Single-quoted for the shell: a quote inside becomes '\''.
    let object = format!("{{{}}}", pairs.join(", ")).replace('\'', r"'\''");
    format!("config set takes one JSON object: spotify config set '{object}'.")
}

/// `key=value` words (`search_limit=5 telemetry=false`) → the JSON form, when every word is one.
pub(crate) fn config_key_value_advice<'a>(
    words: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let pairs: Option<Vec<(&str, &str)>> = words
        .into_iter()
        .map(|word| word.split_once('=').filter(|(key, _)| config_key(key)))
        .collect();
    pairs
        .filter(|pairs| !pairs.is_empty())
        .map(|pairs| config_json_form(&pairs))
}

/// `config set <key> <value>` (or several `key=value` words) → the JSON form it takes.
fn config_set_advice(command: &clap::Command, positionals: &[String]) -> Option<String> {
    if command.get_bin_name() != Some("spotify config set") || positionals.len() < 2 {
        return None;
    }
    if positionals.len().is_multiple_of(2) && positionals.iter().step_by(2).all(|k| config_key(k)) {
        let pairs: Vec<(&str, &str)> = positionals
            .chunks(2)
            .map(|pair| (pair[0].as_str(), pair[1].as_str()))
            .collect();
        return Some(config_json_form(&pairs));
    }
    config_key_value_advice(positionals.iter().map(String::as_str))
}

/// `spotify lyrics arctic monkeys 505`: words, not a track. Find the track first.
fn lyrics_advice(command: &clap::Command, positionals: &[String]) -> Option<String> {
    (command.get_bin_name() == Some("spotify lyrics") && positionals.len() > 1)
        .then(|| crate::ops::lyrics_search_advice(&positionals.join(" ")))
}

/// `spotify lyrics --search '<words>'` (a flag `lyrics` no longer has): clap's tip would make
/// `--search` the track (`-- --search`); the way that works is to find the track first.
fn lyrics_search_flag_advice(
    unexpected: &str,
    argv: &[String],
    command: &clap::Command,
    positionals: &[String],
) -> Option<String> {
    if command.get_bin_name() != Some("spotify lyrics")
        || !(unexpected == "--search" || unexpected.starts_with("--search="))
    {
        return None;
    }
    let inline = argv
        .iter()
        .find_map(|a| a.strip_prefix("--search="))
        .map(str::to_owned);
    let words = inline
        .into_iter()
        .chain(positionals.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let words = if words.trim().is_empty() {
        "<words>".to_owned()
    } else {
        words
    };
    Some(format!(
        "`spotify lyrics` has no --search; it takes a track: {}",
        crate::ops::find_the_track(&words)
    ))
}

/// `spotify queue next`: a subcommand the group does not have. Lists the ones it has, and says
/// where a word that is a command of its own (`next`) lives.
fn subcommand_advice(
    error: &clap::Error,
    root: &clap::Command,
    command: &clap::Command,
    positionals: &[String],
) -> Option<String> {
    if error.kind() != clap::error::ErrorKind::InvalidSubcommand {
        return None;
    }
    let word = positionals.first()?;
    // The root has too many to list; clap names a similar one.
    let path = command.get_bin_name().filter(|p| *p != "spotify")?;
    let names: Vec<String> = command
        .get_subcommands()
        .filter(|s| s.get_name() != "help" && !s.is_hide_set())
        .map(|s| {
            let aliases: Vec<&str> = s.get_visible_aliases().collect();
            if aliases.is_empty() {
                s.get_name().to_owned()
            } else {
                format!("{} ({})", s.get_name(), aliases.join(", "))
            }
        })
        .collect();
    if names.is_empty() {
        return None;
    }
    let mut advice = format!(
        "`{path}` has no subcommand `{word}`; its subcommands are {}.",
        names.join(", ")
    );
    let top = root
        .get_subcommands()
        .find(|s| s.get_name() == word || s.get_visible_aliases().any(|a| a == word));
    if path == "spotify queue" && matches!(word.as_str(), "next" | "skip") {
        advice.push_str(" To skip to the next track: spotify next.");
    } else if let Some(top) = top {
        advice.push_str(&format!(
            " `spotify {}` is a command of its own: spotify {} --help.",
            top.get_name(),
            top.get_name()
        ));
    }
    Some(advice)
}

/// The Damerau-Levenshtein distance of two words (optimal string alignment): insertions,
/// deletions, substitutions and swaps of two neighbours each count 1, so `lyircs` is 1 from
/// `lyrics`.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // Rows i-1 and i of the table while row i+1 is computed.
    let mut before: Vec<usize> = Vec::new();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, &ca) in a.iter().enumerate() {
        let mut row = vec![i + 1; b.len() + 1];
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let mut best = (previous[j + 1] + 1)
                .min(row[j] + 1)
                .min(previous[j] + cost);
            if i > 0 && j > 0 && ca == b[j - 1] && a[i - 1] == cb {
                best = best.min(before[j - 1] + 1);
            }
            row[j + 1] = best;
        }
        before = std::mem::replace(&mut previous, row);
    }
    previous[b.len()]
}

/// The subcommands of `command` whose name or visible alias is a likely typo of `word`,
/// closest first (a name before an alias as close, then by name), each subcommand once, as it
/// is closest. Up to one edit for words of 2-4 letters, two for 5-8, three for longer ones.
fn similar_subcommands(command: &clap::Command, word: &str) -> Vec<String> {
    let word = word.to_lowercase();
    let most = match word.chars().count() {
        0..=1 => return Vec::new(),
        2..=4 => 1,
        5..=8 => 2,
        _ => 3,
    };
    let mut found: Vec<(usize, bool, String)> = command
        .get_subcommands()
        .filter(|s| s.get_name() != "help" && !s.is_hide_set())
        .filter_map(|sub| {
            std::iter::once((sub.get_name(), false))
                .chain(sub.get_visible_aliases().map(|a| (a, true)))
                .map(|(name, alias)| (edit_distance(&word, name), alias, name.to_owned()))
                .min()
        })
        .filter(|(distance, ..)| *distance <= most)
        .collect();
    found.sort();
    found.into_iter().map(|(.., name)| name).collect()
}

/// clap's own "similar subcommand" tip, which [`similar_subcommands`] replaces.
fn is_similar_tip(tip: &str) -> bool {
    tip.starts_with("a similar subcommand exists") || tip.starts_with("some similar subcommands")
}

/// The tip for the subcommands `similar_subcommands` found, as clap words its tips.
fn similar_tip(names: &[String]) -> Option<String> {
    let (best, rest) = names.split_first()?;
    let mut tip = format!("did you mean '{best}'?");
    if !rest.is_empty() {
        let rest: Vec<String> = rest.iter().map(|n| format!("'{n}'")).collect();
        tip.push_str(&format!(" (also close: {})", rest.join(", ")));
    }
    Some(tip)
}

/// What to say instead of or beyond clap for this command line.
#[derive(Debug, Default)]
struct Advice {
    /// A tip that replaces clap's generic `-- -x` tip, or is added when clap has none.
    value_tip: Option<String>,
    /// Said before everything else in the hint.
    first: Option<String>,
    /// For an unknown subcommand: the likely typos of it (`lyrics` for `lyircs`), closest first,
    /// said ahead of clap's own suggestions, which they replace.
    similar: Vec<String>,
    /// The command the unknown subcommand was given to (`spotify`, `spotify queue`).
    parent: String,
    /// The usage line of the command, for errors whose rendering has none.
    usage: String,
}

fn context(error: &clap::Error, kind: clap::error::ContextKind) -> Option<String> {
    use clap::error::ContextValue;
    match error.get(kind) {
        Some(ContextValue::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(ContextValue::Strings(values)) if !values.is_empty() => Some(values.join(", ")),
        _ => None,
    }
}

fn advice(error: &clap::Error, argv: &[String]) -> Advice {
    let mut root = crate::args::command();
    root.build();
    let (command, positionals) = invoked(&root, argv);
    let unexpected = (error.kind() == clap::error::ErrorKind::UnknownArgument)
        .then(|| context(error, clap::error::ContextKind::InvalidArg))
        .flatten();
    let lyrics_flag = unexpected
        .as_deref()
        .and_then(|u| lyrics_search_flag_advice(u, argv, command, &positionals));
    let value_tip = lyrics_flag.clone().or_else(|| {
        unexpected
            .as_deref()
            .and_then(|u| value_advice(u, argv, command))
    });
    let similar = if error.kind() == clap::error::ErrorKind::InvalidSubcommand {
        positionals
            .first()
            .map(|word| similar_subcommands(command, word))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Advice {
        value_tip,
        similar,
        parent: command.get_bin_name().unwrap_or("spotify").to_owned(),
        first: config_set_advice(command, &positionals)
            .or_else(|| {
                lyrics_flag
                    .is_none()
                    .then(|| lyrics_advice(command, &positionals))
                    .flatten()
            })
            .or_else(|| subcommand_advice(error, &root, command, &positionals)),
        usage: command.clone().render_usage().to_string().trim().to_owned(),
    }
}

/// A clap parse error as a `usage` error, keeping everything clap says (the missing arguments,
/// the possible values, its tips and the usage line), with the argument it is about and the
/// command's usage in `details` even when clap's text has no usage line.
fn usage_error(error: &clap::Error, argv: &[String]) -> Error {
    use clap::error::ContextKind;
    let advice = advice(error, argv);
    let mut text = clap_text(&error.render().to_string());
    if let Some(better) = &advice.value_tip {
        text.tips.retain(|tip| !is_dash_dash_tip(tip));
        text.tips.insert(0, better.clone());
    }
    if !advice.similar.is_empty() {
        text.tips.retain(|tip| !is_similar_tip(tip));
    }
    if text.usage.is_none() && advice.usage.starts_with("Usage:") {
        text.usage = Some(advice.usage.clone());
    }
    let mut hint = Vec::new();
    if let Some((best, rest)) = advice.similar.split_first() {
        let mut meant = format!("Did you mean `{} {best}`?", advice.parent);
        if !rest.is_empty() {
            let rest: Vec<String> = rest
                .iter()
                .map(|n| format!("`{} {n}`", advice.parent))
                .collect();
            meant.push_str(&format!(" Also close: {}.", rest.join(", ")));
        }
        hint.push(meant);
    }
    hint.extend(advice.first.clone());
    if !text.possible.is_empty() {
        hint.push(format!("Possible values: {}.", text.possible.join(", ")));
    }
    for tip in &text.tips {
        let mut chars = tip.chars();
        let tip: String = chars
            .next()
            .map(|c| c.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
        hint.push(format!("{}.", tip.trim_end_matches('.')));
    }
    if let Some(usage) = &text.usage {
        // `<QUERY>...` already ends in dots.
        hint.push(if usage.ends_with('.') {
            usage.clone()
        } else {
            format!("{usage}.")
        });
    }
    hint.push("Run the command with --help for arguments and examples.".into());
    let mut details = serde_json::Map::new();
    if let Some(argument) = context(error, ContextKind::InvalidArg) {
        details.insert("argument".into(), json!(argument));
    }
    if let Some(value) = context(error, ContextKind::InvalidValue) {
        details.insert("value".into(), json!(value));
    }
    if !text.possible.is_empty() {
        details.insert("possible_values".into(), json!(text.possible));
    }
    if !advice.similar.is_empty() {
        let meant: Vec<String> = advice
            .similar
            .iter()
            .map(|n| format!("{} {n}", advice.parent))
            .collect();
        details.insert("suggestions".into(), json!(meant));
    }
    if let Some(usage) = &text.usage {
        details.insert(
            "usage".into(),
            json!(usage.trim_start_matches("Usage:").trim()),
        );
    }
    let error = Error::new("usage", text.message, hint.join(" "));
    if details.is_empty() {
        error
    } else {
        error.with_details(Value::Object(details))
    }
}

/// clap's own rendering of a parse error, corrected where `advice` knows better; `None` when
/// clap's text stands as it is.
fn human_usage_error(error: &clap::Error, argv: &[String]) -> Option<String> {
    let advice = advice(error, argv);
    if advice.value_tip.is_none() && advice.first.is_none() && advice.similar.is_empty() {
        return None;
    }
    let mut text = error.render().to_string();
    if let Some(tip) = similar_tip(&advice.similar) {
        // Ours instead of clap's (`playlists`, `playlist`, `lyrics` for `lyircs`), right under
        // the error line.
        let mut lines: Vec<String> = text
            .lines()
            .filter(|l| !l.trim().strip_prefix("tip: ").is_some_and(is_similar_tip))
            .map(str::to_owned)
            .collect();
        lines.dedup_by(|a, b| a.trim().is_empty() && b.trim().is_empty());
        let at = lines.len().min(1);
        lines.splice(at..at, [String::new(), format!("  tip: {tip}")]);
        text = lines.join("\n");
        text.push('\n');
    }
    if let Some(better) = &advice.value_tip {
        let tips: Vec<String> = clap_text(&text)
            .tips
            .into_iter()
            .filter(|tip| is_dash_dash_tip(tip))
            .collect();
        if tips.is_empty() {
            text = format!("{}\n\nhint: {better}", text.trim_end());
        }
        for tip in tips {
            text = text.replace(&tip, better);
        }
    }
    if let Some(first) = &advice.first {
        text = format!("{}\n\nhint: {first}", text.trim_end());
    }
    Some(text)
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
        err_line!("[testing plane selected · leave with `spotify testing exit`]");
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
        command: command_path(cli.command.as_ref()),
    })
}

async fn run(cli: Cli, ctx: &Ctx) -> Result<()> {
    let Some(command) = cli.command else {
        return orient::orient(ctx).await;
    };
    match command {
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
        Command::Docs {
            topic,
            section,
            search,
            all,
        } => docs::docs(
            ctx,
            topic.as_deref(),
            section.as_deref(),
            search.as_deref(),
            all,
        ),
        Command::Commands => docs::commands(ctx),
        Command::How { question, limit } => how::how(ctx, &question, limit),
        Command::Completions { shell } => docs::completions(ctx, shell),
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
    if !ctx.telemetry
        || matches!(
            ctx.command.as_str(),
            "docs" | "commands" | "iam" | "how" | "completions" | "orientation"
        )
    {
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
    let matches = crate::args::command().try_get_matches();
    let cli = match matches.and_then(|m| <Cli as clap::FromArgMatches>::from_arg_matches(&m)) {
        Ok(cli) => cli,
        Err(error) => {
            // `std::env::args` panics on an argument that is not UTF-8 (clap reports those as
            // usage errors, which must reach the user as such).
            let argv: Vec<String> = std::env::args_os()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let wants_json = argv.iter().any(|a| a == "--json");
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
                    print_error(true, &usage_error(&error, &argv));
                    std::process::exit(2);
                }
                _ => {
                    match human_usage_error(&error, &argv) {
                        Some(text) => err_line!("{}", text.trim_end()),
                        None => {
                            let _ = error.print();
                        }
                    }
                    err_line!(
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
    use serde_json::json;

    #[test]
    fn grammar_is_consistent() {
        // Catches duplicate argument ids, bad defaults and conflicting names at test time.
        crate::args::command().debug_assert();
    }

    /// `-V` is a global option listed in every command's help, so every command answers it the
    /// same way.
    #[test]
    fn every_command_prints_the_version_as_spotify() {
        for args in [
            &["spotify", "-V"][..],
            &["spotify", "queue", "-V"],
            &["spotify", "trigger", "add", "--version"],
        ] {
            let error = crate::args::command()
                .try_get_matches_from(args)
                .expect_err("version");
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
            assert_eq!(
                error.render().to_string(),
                format!("spotify {}\n", env!("CARGO_PKG_VERSION")),
                "{args:?}"
            );
        }
    }

    fn parse_error(args: &[&str]) -> silicon_spotify_client::Error {
        let error = crate::args::command()
            .try_get_matches_from(args)
            .expect_err("usage error");
        let argv: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        super::usage_error(&error, &argv)
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

    #[test]
    fn value_errors_carry_the_argument_and_usage() {
        for args in [
            &["spotify", "search", "queen", "--limit", "abc", "--json"][..],
            &["spotify", "search", "queen", "--limit", "--play", "--json"],
            &["spotify", "trigger", "add", "--end", "--times", "x"],
        ] {
            let error = parse_error(args);
            let details = error.details.clone().unwrap_or_default();
            assert!(
                details["argument"]
                    .as_str()
                    .is_some_and(|a| a.starts_with("--")),
                "{args:?}: {details}"
            );
            assert!(
                details["usage"]
                    .as_str()
                    .is_some_and(|u| u.starts_with(&format!("spotify {} ", args[1]))),
                "{args:?}: {details}"
            );
            assert!(error.hint.contains("Usage: spotify "), "{}", error.hint);
        }
        let error = parse_error(&["spotify", "search", "queen", "--limit", "abc"]);
        assert_eq!(
            error.details.map(|d| d["value"].clone()),
            Some(json!("abc"))
        );
    }

    #[test]
    fn negative_limits_are_values_not_flags() {
        use clap::Parser as _;
        for args in [
            &["spotify", "search", "queen", "--limit", "-1"][..],
            &["spotify", "search", "queen", "--limit=-1"],
            &["spotify", "library", "liked", "--limit", "-1"],
        ] {
            // Parsed, so the range check (invalid_input, 1 to 10) answers instead of clap's tip
            // to write `-- -1`, which would add -1 to the query.
            crate::args::Cli::try_parse_from(args).expect("parses");
        }
    }

    #[test]
    fn hyphenated_option_values_get_the_equals_form() {
        let error = parse_error(&["spotify", "trigger", "add", "--end", "--note", "-x"]);
        assert!(
            error
                .hint
                .starts_with("To pass '-x' as the value of --note, write --note=-x."),
            "{}",
            error.hint
        );
        assert!(!error.hint.contains("'-- "), "{}", error.hint);
        // A positional keeps clap's tip, which is right there.
        let error = parse_error(&["spotify", "queue", "move", "3", "-1"]);
        assert!(error.hint.contains("use '-- -1'"), "{}", error.hint);
        // A mistyped flag after an option keeps clap's suggestion, without the value advice.
        let error = parse_error(&["spotify", "trigger", "add", "--end", "--note", "--lable"]);
        assert!(!error.hint.contains("--note=--lable"), "{}", error.hint);
        assert!(error.hint.contains("'--label'"), "{}", error.hint);
    }

    #[test]
    fn config_set_key_value_points_to_the_json_form() {
        let error = parse_error(&["spotify", "config", "set", "search_limit", "5"]);
        assert!(
            error.hint.starts_with(
                r#"config set takes one JSON object: spotify config set '{"search_limit": 5}'."#
            ),
            "{}",
            error.hint
        );
        let error = parse_error(&["spotify", "config", "set", "a=1", "strategy=applescript"]);
        assert!(
            error
                .hint
                .contains(r#"'{"a": 1, "strategy": "applescript"}'"#),
            "{}",
            error.hint
        );
        assert_eq!(
            super::config_key_value_advice(["search_limit=5"]).as_deref(),
            Some(r#"config set takes one JSON object: spotify config set '{"search_limit": 5}'."#)
        );
        assert_eq!(
            super::config_key_value_advice([r#"{"search_limit": 5}"#]),
            None
        );
        assert_eq!(
            super::config_key_value_advice(["search_limit=5", "oops"]),
            None
        );
        let error = parse_error(&["spotify", "config", "set", "strategy", "applescript"]);
        assert!(
            error.hint.contains(r#"'{"strategy": "applescript"}'"#),
            "{}",
            error.hint
        );
        // Only `key value` pairs get the advice; a JSON object plus a stray word does not.
        let error = parse_error(&["spotify", "config", "set", r#"{"a": 1}"#, "extra"]);
        assert!(
            !error.hint.contains("takes one JSON object"),
            "{}",
            error.hint
        );
        // A quote in a value stays copyable in the single-quoted shell form.
        let error = parse_error(&["spotify", "config", "set", "label", "it's"]);
        assert!(
            error.hint.contains(r#"'{"label": "it'\''s"}'"#),
            "{}",
            error.hint
        );
        let argv: Vec<String> = ["spotify", "config", "set", "search_limit", "5"]
            .iter()
            .map(|a| (*a).to_owned())
            .collect();
        let error = crate::args::command()
            .try_get_matches_from(&argv)
            .expect_err("usage error");
        let text = super::human_usage_error(&error, &argv).expect("advice");
        assert!(
            text.ends_with(
                "hint: config set takes one JSON object: spotify config set '{\"search_limit\": 5}'."
            ),
            "{text}"
        );
    }

    #[test]
    fn lyrics_of_search_words_points_to_search() {
        let error = parse_error(&["spotify", "lyrics", "arctic", "monkeys", "505", "--json"]);
        assert!(
            error.hint.starts_with(
                "`spotify lyrics` takes a track, not search words: find it with `spotify search 'arctic monkeys 505' --type track`, then run `spotify lyrics <uri>`"
            ),
            "{}",
            error.hint
        );
    }

    /// `lyrics --search` is gone. clap's own tip (`-- --search`) would pass the flag as the track.
    #[test]
    fn lyrics_search_flag_points_to_search() {
        for (args, words) in [
            (&["spotify", "lyrics", "--search", "505"][..], "505"),
            (
                &["spotify", "lyrics", "--search", "bohemian rhapsody"],
                "'bohemian rhapsody'",
            ),
            (&["spotify", "lyrics", "--search=queen", "--json"], "queen"),
            (&["spotify", "lyrics", "--search"], "'<words>'"),
        ] {
            let error = parse_error(args);
            assert_eq!(error.code, "usage");
            assert!(
                error.hint.starts_with(&format!(
                    "`spotify lyrics` has no --search; it takes a track: find it with `spotify search {words} --type track`, then run `spotify lyrics <uri>`"
                )),
                "{args:?}: {}",
                error.hint
            );
            assert!(!error.hint.contains("-- --search"), "{}", error.hint);
            let argv: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            let clap_error = crate::args::command()
                .try_get_matches_from(&argv)
                .expect_err("usage error");
            let text = super::human_usage_error(&clap_error, &argv).expect("advice");
            assert!(!text.contains("-- --search"), "{text}");
            assert!(text.contains("has no --search"), "{text}");
        }
    }

    #[test]
    fn unknown_subcommands_list_the_real_ones() {
        let error = parse_error(&["spotify", "queue", "next"]);
        assert_eq!(error.code, "usage");
        assert!(
            error.hint.starts_with(
                "`spotify queue` has no subcommand `next`; its subcommands are list (show, ls), add, remove (rm), move (mv), clear. To skip to the next track: spotify next."
            ),
            "{}",
            error.hint
        );
        let error = parse_error(&["spotify", "trigger", "status"]);
        assert!(
            error
                .hint
                .contains("`spotify status` is a command of its own: spotify status --help."),
            "{}",
            error.hint
        );
        let error = parse_error(&["spotify", "frobnicate"]);
        assert!(
            !error.hint.contains("its subcommands are"),
            "{}",
            error.hint
        );
        // `queue show` is `queue list`.
        use clap::Parser as _;
        let cli = crate::args::Cli::try_parse_from(["spotify", "queue", "show"]).expect("parses");
        assert!(matches!(
            cli.command,
            Some(crate::args::Command::Queue {
                action: Some(crate::args::QueueCommand::List)
            })
        ));
    }

    #[test]
    fn typos_of_a_command_suggest_the_closest_one_first() {
        use super::edit_distance;
        assert_eq!(edit_distance("lyircs", "lyrics"), 1, "a swap is one edit");
        assert_eq!(edit_distance("stauts", "status"), 1);
        assert_eq!(edit_distance("playlst", "playlist"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("lyrics", "lyrics"), 0);
        assert_eq!(edit_distance("ca", "abc"), 3, "optimal string alignment");
        // clap's own tip named `playlists` and `playlist` before `lyrics`.
        let error = parse_error(&["spotify", "lyircs", "--json"]);
        assert!(
            error.hint.starts_with("Did you mean `spotify lyrics`?"),
            "{}",
            error.hint
        );
        assert!(!error.hint.contains("playlist"), "{}", error.hint);
        assert_eq!(
            error.details.map(|d| d["suggestions"].clone()),
            Some(json!(["spotify lyrics"]))
        );
        let argv: Vec<String> = ["spotify", "lyircs"]
            .iter()
            .map(|a| (*a).to_owned())
            .collect();
        let clap_error = crate::args::command()
            .try_get_matches_from(&argv)
            .expect_err("usage error");
        let text = super::human_usage_error(&clap_error, &argv).expect("advice");
        assert!(
            text.starts_with(
                "error: unrecognized subcommand 'lyircs'\n\n  tip: did you mean 'lyrics'?\n"
            ),
            "{text}"
        );
        assert!(!text.contains("similar subcommand"), "{text}");
        assert!(text.contains("\nUsage: spotify "), "{text}");
        // A name and an alias of the same command count once; aliases are suggested too.
        for (args, meant) in [
            (&["spotify", "stauts"][..], "spotify status"),
            (&["spotify", "playlst"], "spotify playlist"),
            (&["spotify", "nxt"], "spotify next"),
            (&["spotify", "queue", "ad"], "spotify queue add"),
            (&["spotify", "trigger", "lst"], "spotify trigger list"),
        ] {
            let error = parse_error(args);
            assert!(
                error.hint.starts_with(&format!("Did you mean `{meant}`?")),
                "{args:?}: {}",
                error.hint
            );
        }
        // A group keeps its list of subcommands after the suggestion.
        let error = parse_error(&["spotify", "queue", "ad"]);
        assert!(
            error
                .hint
                .contains("`spotify queue` has no subcommand `ad`; its subcommands are"),
            "{}",
            error.hint
        );
        // Nothing close: no suggestion of ours.
        for args in [&["spotify", "xyzzy"][..], &["spotify", "frobnicate"]] {
            let error = parse_error(args);
            assert!(
                !error.hint.contains("Did you mean"),
                "{args:?}: {}",
                error.hint
            );
        }
    }

    /// Help is wrapped at 100 columns (`max_term_width`), so an indented example line longer
    /// than that would be broken in two and no longer copyable.
    #[test]
    fn help_examples_fit_on_one_line() {
        fn walk(command: &clap::Command, path: &str, long: &mut Vec<String>) {
            let texts = [
                command.get_about(),
                command.get_long_about(),
                command.get_before_help(),
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
        walk(&crate::args::command(), "spotify", &mut long);
        assert!(
            long.is_empty(),
            "example lines over 100 columns:\n{}",
            long.join("\n")
        );
    }
}
