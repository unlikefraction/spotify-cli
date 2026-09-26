//! Help and guides cannot drift apart: every `spotify …` command line the guides show must name
//! commands and flags the CLI has, every `spotify docs <topic>` a topic it bundles, and every
//! command must have examples. The CLI describes itself through `spotify commands --json`, so
//! this also checks that the manifest is complete.

#[path = "../src/shell.rs"]
mod shell;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn manifest() -> Value {
    let home = tempfile::tempdir().expect("home");
    let out = Command::new(env!("CARGO_BIN_EXE_spotify"))
        .args(["commands", "--json"])
        .env("SILICON_HOME", home.path())
        .env("SPOTIFY_TELEMETRY", "0")
        .env("SPOTIFY_DAEMON_AUTOSTART", "0")
        .env("SPOTIFY_API_URL", "http://127.0.0.1:9")
        .output()
        .expect("run spotify");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("manifest")
}

/// One command of the manifest, as far as a command line needs it.
#[derive(Debug, Default)]
struct Grammar {
    /// Subcommand names and aliases → their path.
    subcommands: HashMap<String, String>,
    /// Long flags → whether they take a value.
    flags: HashMap<String, bool>,
    /// Short flags → whether they take a value.
    shorts: HashMap<String, bool>,
    /// It takes positional arguments.
    positionals: bool,
    /// Long flags → the values they allow (only those with a fixed list).
    values: HashMap<String, Vec<String>>,
    /// Positional arguments in order: (allowed values, empty when any; takes several).
    positional_values: Vec<(Vec<String>, bool)>,
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|v| {
            v.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn grammar(manifest: &Value) -> HashMap<String, Grammar> {
    let commands = manifest["commands"].as_array().expect("commands");
    let mut out: HashMap<String, Grammar> = HashMap::new();
    for command in commands {
        let path = command["path"].as_str().expect("path").to_owned();
        let entry = out.entry(path.clone()).or_default();
        for arg in command["arguments"].as_array().expect("arguments") {
            let takes = arg["takes_value"].as_bool().expect("takes_value");
            let allowed = strings(&arg["allowed_values"]);
            if let Some(long) = arg["long"].as_str() {
                entry.flags.insert(long.to_owned(), takes);
                if takes && !allowed.is_empty() {
                    entry.values.insert(long.to_owned(), allowed.clone());
                }
            }
            if let Some(short) = arg["short"].as_str() {
                entry.shorts.insert(short.to_owned(), takes);
            }
            if arg["positional"] == true {
                entry.positionals = true;
                entry
                    .positional_values
                    .push((allowed, arg["multiple"] == true));
            }
        }
        if !path.is_empty() {
            let (parent, name) = path.rsplit_once(' ').unwrap_or(("", path.as_str()));
            let parent = out.entry(parent.to_owned()).or_default();
            parent.subcommands.insert(name.to_owned(), path.clone());
            for alias in command["aliases"].as_array().expect("aliases") {
                let alias = alias.as_str().expect("alias").to_owned();
                parent.subcommands.insert(alias, path.clone());
            }
        }
    }
    out
}

/// A token that stands for something (`<id>`, `…`, `$var`), not a literal word.
fn placeholder(word: &str) -> bool {
    word.contains('<')
        || word.contains('…')
        || word.contains("...")
        || word.contains('*')
        || word.contains('$')
}

/// What is wrong with a value given to an argument that allows only some, if anything.
/// `--type show,episode` lists several. `who` is the command, or the command and its flag.
fn bad_value(who: &str, value: &str, allowed: &[String]) -> Option<String> {
    if allowed.is_empty() || placeholder(value) {
        return None;
    }
    value
        .split(',')
        .find(|v| !allowed.iter().any(|a| a == v))
        .map(|v| format!("`{who}` takes {}, not `{v}`", allowed.join("|")))
}

/// The keys of a `spotify config set` JSON object that are not settings.
fn bad_config_keys(json: &str, keys: &[String]) -> Option<String> {
    if placeholder(json) || keys.is_empty() {
        return None;
    }
    let object: serde_json::Map<String, Value> = serde_json::from_str(json).ok()?;
    let unknown: Vec<&String> = object.keys().filter(|k| !keys.contains(k)).collect();
    (!unknown.is_empty()).then(|| {
        format!(
            "`spotify config set` has no setting {}",
            unknown
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// What is wrong with one `spotify …` command line, if anything.
fn check(
    words: &[String],
    grammar: &HashMap<String, Grammar>,
    topics: &[String],
) -> Option<String> {
    let root = grammar.get("")?;
    let config_keys = grammar
        .get("config get")
        .and_then(|g| g.positional_values.first())
        .map(|(allowed, _)| allowed.clone())
        .unwrap_or_default();
    let mut path = String::new();
    let mut positional_seen = false;
    // Which positional argument the next plain word fills.
    let mut position = 0;
    let mut rest = words.iter().skip(1);
    while let Some(word) = rest.next() {
        let current = grammar.get(&path)?;
        let at = if path.is_empty() {
            "spotify".to_owned()
        } else {
            format!("spotify {path}")
        };
        if word == "--" {
            return None;
        }
        if let Some(flag) = word.strip_prefix("--") {
            let (name, inline) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (flag, None),
            };
            if matches!(name, "help" | "version") {
                return None;
            }
            let Some(takes) = current.flags.get(name).or_else(|| root.flags.get(name)) else {
                return Some(format!("`{at}` has no flag --{name}"));
            };
            let value = match inline {
                Some(value) => Some(value),
                None if *takes => rest.next().cloned(),
                None => None,
            };
            if let (Some(value), Some(allowed)) = (value, current.values.get(name))
                && let Some(problem) = bad_value(&format!("{at} --{name}"), &value, allowed)
            {
                return Some(problem);
            }
            continue;
        }
        if let Some(short) = word
            .strip_prefix('-')
            .filter(|s| s.len() == 1 && s.chars().all(char::is_alphabetic))
        {
            if matches!(short, "h" | "V") {
                return None;
            }
            match current.shorts.get(short) {
                Some(true) => {
                    rest.next();
                }
                Some(false) => {}
                None => return Some(format!("`spotify {path}` has no flag -{short}")),
            }
            continue;
        }
        if word.starts_with('-') {
            // A negative number or `-` (stdin) as a value.
            continue;
        }
        if path == "docs" && !positional_seen && !placeholder(word) {
            if !topics.iter().any(|t| t == word) {
                return Some(format!("`spotify docs {word}`: there is no guide `{word}`"));
            }
            positional_seen = true;
            continue;
        }
        let is_subcommand = !positional_seen && current.subcommands.contains_key(word.as_str());
        if is_subcommand {
            if let Some(sub) = current.subcommands.get(word.as_str()) {
                path.clone_from(sub);
                position = 0;
            }
            continue;
        }
        if !positional_seen && !current.subcommands.is_empty() && placeholder(word) {
            // `spotify <command> --help`: which command is not known.
            return None;
        }
        if !positional_seen && !current.subcommands.is_empty() && !current.positionals {
            return Some(format!("`{at}` has no subcommand `{word}`"));
        }
        // A positional argument: check it against the values it allows.
        positional_seen = true;
        if let Some((allowed, multiple)) = current.positional_values.get(position) {
            if let Some(problem) = bad_value(&at, word, allowed) {
                return Some(problem);
            }
            if path == "config set"
                && let Some(problem) = bad_config_keys(word, &config_keys)
            {
                return Some(problem);
            }
            if !multiple {
                position += 1;
            }
        }
    }
    None
}

/// The code a Markdown file shows: its inline code spans and the lines of its code blocks.
fn code(markdown: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (index, line) in markdown.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            let line = line.trim_start().trim_start_matches("$ ");
            out.push((index + 1, line.to_owned()));
        } else {
            // Odd pieces between backticks are code (double backticks for spans holding one).
            let spans = if line.contains("``") {
                line.split("``")
                    .skip(1)
                    .step_by(2)
                    .map(|s| s.trim().to_owned())
                    .collect::<Vec<_>>()
            } else {
                line.split('`')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            };
            out.extend(spans.into_iter().map(|span| (index + 1, span)));
        }
    }
    out
}

/// The guides to check: the repository's canonical `docs/`, else the copies the CLI bundles.
fn guide_files() -> Vec<PathBuf> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let canonical = crate_dir.join("../../docs");
    let dir = if canonical.is_dir() {
        canonical
    } else {
        crate_dir.join("docs")
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("docs dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no guides in {}", dir.display());
    // The READMEs show commands too.
    for readme in [
        crate_dir.join("README.md"),
        crate_dir.join("../../README.md"),
    ] {
        if readme.is_file() {
            files.push(readme);
        }
    }
    files
}

/// Command lines the guides show on purpose as usage errors (`spotify docs errors`, "Bad
/// arguments"), as their words joined by spaces.
const INVALID_ON_PURPOSE: &[&str] = &["spotify track <id> --type show", "spotify lyircs"];

#[test]
fn guides_mention_only_commands_and_flags_that_exist() {
    let manifest = manifest();
    let grammar = grammar(&manifest);
    let topics: Vec<String> = manifest["commands"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["path"] == "docs"))
        .and_then(|d| d["arguments"].as_array())
        .and_then(|a| a.iter().find(|a| a["name"] == "topic"))
        .and_then(|t| t["allowed_values"].as_array())
        .map(|v| {
            v.iter()
                .filter_map(|t| t.as_str().map(str::to_owned))
                .collect()
        })
        .expect("docs topics");
    let mut problems = Vec::new();
    let mut checked = 0;
    for file in guide_files() {
        let text = std::fs::read_to_string(&file).expect("read guide");
        for (line, snippet) in code(&text) {
            for words in shell::spotify_commands(&snippet) {
                checked += 1;
                if INVALID_ON_PURPOSE.contains(&words.join(" ").as_str()) {
                    continue;
                }
                if let Some(problem) = check(&words, &grammar, &topics) {
                    problems.push(format!(
                        "{}:{line}: {problem} (in `{}`)",
                        file.display(),
                        words.join(" ")
                    ));
                }
            }
        }
    }
    assert!(
        checked > 100,
        "only {checked} command lines found: is the extraction broken?"
    );
    assert!(
        problems.is_empty(),
        "guides mention what the CLI does not have (a usage error shown on purpose goes in \
         INVALID_ON_PURPOSE):\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_drift_check_catches_what_does_not_exist() {
    let manifest = manifest();
    let grammar = grammar(&manifest);
    let topics = vec!["triggers".to_owned()];
    let run = |line: &str| {
        shell::spotify_commands(line)
            .first()
            .and_then(|words| check(words, &grammar, &topics))
    };
    assert_eq!(run("spotify trigger add --end --scope every"), None);
    assert_eq!(run("spotify --json trigger ls --all"), None);
    assert_eq!(run("spotify now --json | jq .playback"), None);
    assert_eq!(run("spotify search x --limit -1"), None);
    assert_eq!(run("spotify volume -- -10"), None);
    assert_eq!(run("spotify <command> --help"), None);
    assert_eq!(run("spotify login 'oac_…'"), None);
    assert_eq!(run("spotify docs triggers --section 'Create one'"), None);
    assert_eq!(
        run("spotify trigger add --in 30s").as_deref(),
        Some("`spotify trigger add` has no flag --in")
    );
    assert_eq!(
        run("spotify trigger snooze <id>").as_deref(),
        Some("`spotify trigger` has no subcommand `snooze`")
    );
    assert_eq!(
        run("spotify frobnicate").as_deref(),
        Some("`spotify` has no subcommand `frobnicate`")
    );
    assert_eq!(
        run("spotify docs nonsense").as_deref(),
        Some("`spotify docs nonsense`: there is no guide `nonsense`")
    );
    // Values of arguments that allow only some.
    assert_eq!(run("spotify library liked --limit 20"), None);
    assert_eq!(run("spotify search 'daily news' --type show,episode"), None);
    assert_eq!(run("spotify repeat track"), None);
    assert_eq!(run("spotify config get <key>"), None);
    assert_eq!(run("spotify config set '{\"telemetry\": false}'"), None);
    assert_eq!(
        run("spotify library saved").as_deref(),
        Some("`spotify library` takes liked|albums|artists|top|playlists, not `saved`")
    );
    assert_eq!(
        run("spotify repeat one").as_deref(),
        Some("`spotify repeat` takes off|context|track, not `one`")
    );
    assert_eq!(
        run("spotify search x --type show,podcast").as_deref(),
        Some(
            "`spotify search --type` takes track|album|artist|playlist|show|episode, not `podcast`"
        )
    );
    assert_eq!(
        run("spotify trigger add --end --scope=always").as_deref(),
        Some("`spotify trigger add --scope` takes current|every|track, not `always`")
    );
    assert_eq!(
        run("spotify config set '{\"telemetry\": false, \"colour\": \"on\"}'").as_deref(),
        Some("`spotify config set` has no setting `colour`")
    );
}

#[test]
fn every_command_has_examples_in_the_manifest() {
    let manifest = manifest();
    let missing: Vec<&str> = manifest["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .filter(|c| c["path"] != "" && c["examples"].as_array().is_none_or(Vec::is_empty))
        .filter_map(|c| c["command"].as_str())
        .collect();
    assert!(missing.is_empty(), "commands without examples: {missing:?}");
}
