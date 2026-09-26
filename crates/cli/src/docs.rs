//! Offline documentation (bundled Markdown) and the machine-readable command tree.

use serde_json::{Value, json};
use silicon_spotify_client::{DOCS_URL, Error, REPOSITORY, Result, VERSION};

use crate::Ctx;
use crate::args::ShellArg;
use crate::catalog::{self, Goal};

/// (topic, title, content). Canonical copies live in the repository's `docs/`; `scripts/sync-cli-docs.sh`
/// copies them here so the published crate is self-contained.
pub const TOPICS: &[(&str, &str, &str)] = &[
    (
        "usage",
        "Quick start for Carbons and Silicons",
        include_str!("../docs/usage.md"),
    ),
    (
        "triggers",
        "Triggers: playback checkpoints delivered through Ting",
        include_str!("../docs/triggers.md"),
    ),
    (
        "playback",
        "Playback control, verification and fallback",
        include_str!("../docs/playback.md"),
    ),
    (
        "queue",
        "The managed queue",
        include_str!("../docs/queue.md"),
    ),
    (
        "auth",
        "IAM login, Spotify sign-in and testing planes",
        include_str!("../docs/auth.md"),
    ),
    (
        "config",
        "Settings and bring-your-own",
        include_str!("../docs/config.md"),
    ),
    (
        "daemon",
        "The daemon: what runs, where, and why",
        include_str!("../docs/daemon.md"),
    ),
    (
        "errors",
        "Error codes, exit codes and fixes",
        include_str!("../docs/errors.md"),
    ),
    (
        "ting",
        "Ting types, payloads and flow routing",
        include_str!("../docs/ting.md"),
    ),
    (
        "development",
        "Building on spotify-cli (library, protocol, contributing)",
        include_str!("../docs/development.md"),
    ),
    ("api", "Backend HTTP API", include_str!("../docs/api.md")),
    (
        "telemetry",
        "What telemetry records and how to turn it off",
        include_str!("../docs/telemetry.md"),
    ),
    (
        "versioning",
        "Versions, compatibility and deprecation",
        include_str!("../docs/versioning.md"),
    ),
];

fn footer() -> String {
    format!(
        "\n---\nDocs: {DOCS_URL} · Source: {REPOSITORY} · Explore: spotify docs <topic>; spotify <command> --help"
    )
}

/// `heading` as `--section` compares it: lowercase letters, digits and single spaces.
fn heading_key(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A section of a guide: its heading and its content, down to the next heading of the same or a
/// higher level. Matches the whole heading first, then the start of one, then any part of one.
fn find_section(content: &str, wanted: &str) -> Option<(String, String)> {
    let sections = crate::how::sections(content);
    let key = heading_key(wanted);
    let keys: Vec<String> = sections.iter().map(|(_, h, _)| heading_key(h)).collect();
    let at = keys
        .iter()
        .position(|k| *k == key)
        .or_else(|| keys.iter().position(|k| k.starts_with(&key)))
        .or_else(|| {
            keys.iter()
                .position(|k| !key.is_empty() && k.contains(&key))
        })?;
    let (level, heading, _) = &sections[at];
    let mut text = format!("{} {heading}\n", "#".repeat(*level));
    for (sub_level, sub_heading, body) in &sections[at..] {
        if !std::ptr::eq(sub_heading, heading) {
            if sub_level <= level {
                break;
            }
            text.push_str(&format!("{} {sub_heading}\n", "#".repeat(*sub_level)));
        }
        text.push_str(body);
    }
    Some((heading.replace('`', ""), text.trim_end().to_owned()))
}

/// The bundled guide called `name`.
fn guide(name: &str) -> Result<&'static (&'static str, &'static str, &'static str)> {
    TOPICS.iter().find(|(n, ..)| *n == name).ok_or_else(|| {
        Error::not_found(
            format!("There is no guide called `{name}`."),
            format!(
                "Topics: {}. Or search: spotify docs --search '{name}'",
                TOPICS
                    .iter()
                    .map(|(n, ..)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    })
}

/// A value for a shell command line, single-quoted.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `spotify docs --search`: the lines of the guides (or of one) that hold `query`.
fn search_guides(ctx: &Ctx, topic: Option<&str>, query: &str) -> Result<()> {
    if query.trim().is_empty() {
        return Err(Error::invalid(
            "--search needs some text to look for.",
            "e.g. spotify docs --search 'expired', or spotify docs <topic> --search '<text>' for one guide.",
        ));
    }
    let guides: Vec<&(&str, &str, &str)> = match topic {
        Some(name) => vec![guide(name)?],
        None => TOPICS.iter().collect(),
    };
    let needle = query.to_lowercase();
    let mut results = Vec::new();
    for (name, title, content) in guides {
        let matches: Vec<Value> = content
            .lines()
            .enumerate()
            .filter(|(_, line)| line.to_lowercase().contains(&needle))
            .take(8)
            .map(|(index, line)| json!({"line": index + 1, "excerpt": silicon_spotify_client::model::truncate(line.trim(), 240)}))
            .collect();
        if !matches.is_empty() {
            results.push(json!({"topic": name, "title": title, "command": format!("spotify docs {name}"), "matches": matches}));
        }
    }
    let value = json!({"query": query, "topic": topic, "embedded": true, "results": results});
    ctx.emit(&value, |v| {
        let list = v["results"].as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            return match topic {
                Some(name) => format!(
                    "The {name} guide does not mention `{query}`. Every guide: spotify docs --search {}",
                    quoted(query)
                ),
                None => format!("No guide mentions `{query}`. Topics: spotify docs"),
            };
        }
        list.iter()
            .map(|r| {
                let lines = r["matches"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| format!("    {}: {}", m["line"], m["excerpt"].as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "{} — {}\n{lines}",
                    r["command"].as_str().unwrap_or(""),
                    r["title"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    Ok(())
}

/// `spotify docs`.
pub fn docs(
    ctx: &Ctx,
    topic: Option<&str>,
    section: Option<&str>,
    search: Option<&str>,
    all: bool,
) -> Result<()> {
    if let Some(query) = search {
        return search_guides(ctx, topic, query);
    }
    if all {
        let value = json!({"embedded": true, "version": VERSION, "topics": TOPICS.iter().map(|(n, t, c)| json!({"topic": n, "title": t, "content": c})).collect::<Vec<_>>()});
        ctx.emit(&value, |_| {
            TOPICS
                .iter()
                .map(|(_, _, c)| (*c).to_owned())
                .collect::<Vec<_>>()
                .join("\n\n")
                + &footer()
        });
        return Ok(());
    }
    match topic {
        None => {
            let value = json!({"embedded": true, "version": VERSION, "topics": TOPICS.iter().map(|(n, t, _)| json!({"topic": n, "title": t, "command": format!("spotify docs {n}")})).collect::<Vec<_>>(),
                "help": "spotify docs <topic>; spotify docs <topic> --section <heading>; spotify docs --search <text>; spotify docs <topic> --search <text>; spotify docs --all; spotify how <question>; spotify <command> --help; spotify commands --json", "online": DOCS_URL});
            ctx.emit(&value, |_| {
                let mut out = String::from("Guides (offline, bundled with this CLI):\n");
                for (name, title, _) in TOPICS {
                    out.push_str(&format!("  spotify docs {name:<12} {title}\n"));
                }
                out.push_str(&format!(
                    "One section: spotify docs <topic> --section '<heading>' · Search: spotify docs [<topic>] --search '<text>'\nAsk: spotify how \"<question>\" · Online: {DOCS_URL}"
                ));
                out
            });
        }
        Some(name) => {
            let (name, title, content) = guide(name)?;
            if let Some(wanted) = section {
                if heading_key(wanted).is_empty() {
                    return Err(Error::invalid(
                        "--section needs a heading, or a word of one.",
                        format!(
                            "e.g. spotify docs {name} --section '<heading>'; `spotify docs {name}` shows the whole guide."
                        ),
                    ));
                }
                let (heading, text) = find_section(content, wanted).ok_or_else(|| {
                    Error::not_found(
                        format!("The {name} guide has no section `{wanted}`."),
                        format!(
                            "Sections: {}.",
                            crate::how::sections(content)
                                .iter()
                                .skip(1)
                                .map(|(_, h, _)| h.replace('`', ""))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                })?;
                let value = json!({"topic": name, "title": title, "section": heading, "format": "markdown", "command": format!("spotify docs {name}"), "content": text, "package_version": VERSION, "embedded": true});
                ctx.emit(&value, |_| {
                    format!("{text}\n\n(whole guide: spotify docs {name})")
                });
                return Ok(());
            }
            let value = json!({"topic": name, "title": title, "format": "markdown", "command": format!("spotify docs {name}"), "content": content, "package_version": VERSION, "embedded": true});
            ctx.emit(&value, |_| format!("{content}{}", footer()));
        }
    }
    Ok(())
}

/// What a manifest command changes, as `spotify commands` marks it: `✎ changes library`, with
/// the form that only reads (`✎ changes playback; reads only without a level`). `None` for a
/// command that only reads.
fn change_mark(command: &Value) -> Option<String> {
    if command["mutates"] != true {
        return None;
    }
    let changes: Vec<&str> = command["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let what = if changes.is_empty() {
        // `report`: nothing here changes, something is sent out.
        "sends a report".to_owned()
    } else {
        format!("changes {}", changes.join(", "))
    };
    Some(match command["read_only_when"].as_str() {
        Some(when) => {
            let when = when.split(" (").next().unwrap_or(when);
            format!("✎ {what}; reads only {when}")
        }
        None => format!("✎ {what}"),
    })
}

/// `spotify commands`: the whole tree, grouped by goal (`--json`: the manifest for agents).
pub fn commands(ctx: &Ctx) -> Result<()> {
    let value = catalog::manifest(&crate::args::command());
    ctx.emit(&value, |v| {
        let list = v["commands"].as_array().cloned().unwrap_or_default();
        let mut text = String::new();
        for goal in Goal::ALL {
            text.push_str(&format!("{}:\n", goal.title()));
            for c in list.iter().filter(|c| c["goal"] == goal.id()) {
                let mut line = format!(
                    "  {:<34} {}",
                    c["command"].as_str().unwrap_or(""),
                    c["description"].as_str().unwrap_or("")
                );
                if let Some(mark) = change_mark(c) {
                    line.push_str("  ");
                    line.push_str(&mark);
                }
                text.push_str(line.trim_end());
                text.push('\n');
            }
            text.push('\n');
        }
        text.push_str("✎ marks the commands that change something, and what (`changes` in `spotify commands --json`: library is your Liked Songs, playlists your playlists); the others only read.\n");
        text.push_str("Run `spotify <command> --help` for arguments and examples; `spotify commands --json` for arguments, examples, output fields, errors and requirements; `spotify how \"<question>\"` to find one; `spotify docs` for guides.");
        text
    });
    Ok(())
}

/// `spotify completions <shell>`.
pub fn completions(ctx: &Ctx, shell: ShellArg) -> Result<()> {
    let (name, generator) = match shell {
        ShellArg::Zsh => ("zsh", clap_complete::Shell::Zsh),
        ShellArg::Bash => ("bash", clap_complete::Shell::Bash),
        ShellArg::Fish => ("fish", clap_complete::Shell::Fish),
        ShellArg::Powershell => ("powershell", clap_complete::Shell::PowerShell),
    };
    let mut command = crate::args::command();
    let mut script = Vec::new();
    clap_complete::generate(generator, &mut command, "spotify", &mut script);
    let script = String::from_utf8_lossy(&script).into_owned();
    let value = json!({"shell": name, "script": script, "install": "spotify completions --help"});
    ctx.emit(&value, |v| v["script"].as_str().unwrap_or("").to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_list_marks_what_changes_something() {
        let manifest = catalog::manifest(&crate::args::command());
        let find = |path: &str| {
            manifest["commands"]
                .as_array()
                .and_then(|c| c.iter().find(|c| c["path"] == path))
                .cloned()
                .unwrap_or_else(|| panic!("no {path}"))
        };
        assert_eq!(change_mark(&find("status")), None);
        assert_eq!(change_mark(&find("library")), None);
        assert_eq!(
            change_mark(&find("like")).as_deref(),
            Some("✎ changes library")
        );
        assert_eq!(
            change_mark(&find("playlist add")).as_deref(),
            Some("✎ changes playlists")
        );
        assert_eq!(
            change_mark(&find("volume")).as_deref(),
            Some("✎ changes playback; reads only without a level")
        );
        assert_eq!(
            change_mark(&find("report")).as_deref(),
            Some("✎ sends a report")
        );
    }

    #[test]
    fn sections_are_found_by_any_part_of_their_heading() {
        let guide =
            "# Guide\nintro\n## Create one\nhow\n### Details\nmore\n## Without Ting\nlocal\n";
        let (heading, text) = find_section(guide, "create one").expect("section");
        assert_eq!(heading, "Create one");
        assert_eq!(text, "## Create one\nhow\n### Details\nmore");
        assert_eq!(
            find_section(guide, "without").map(|s| s.0).as_deref(),
            Some("Without Ting")
        );
        assert_eq!(
            find_section(guide, "ting").map(|s| s.0).as_deref(),
            Some("Without Ting")
        );
        assert!(find_section(guide, "nope").is_none());
        let (_, text) = find_section(guide, "Details").expect("section");
        assert_eq!(text, "### Details\nmore");
        // Every real heading can be asked for as `spotify how` prints it.
        for (topic, _, content) in TOPICS {
            for (_, heading, _) in crate::how::sections(content) {
                let asked = heading.replace('`', "");
                assert!(find_section(content, &asked).is_some(), "{topic}: {asked}");
            }
        }
    }
}
