//! Offline documentation (bundled Markdown) and the machine-readable command tree.

use clap::CommandFactory as _;
use serde_json::{Value, json};
use silicon_spotify_client::{DOCS_URL, Error, REPOSITORY, Result, VERSION};

use crate::Ctx;
use crate::args::Cli;

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

/// `spotify docs`.
pub fn docs(ctx: &Ctx, topic: Option<&str>, search: Option<&str>, all: bool) -> Result<()> {
    if let Some(query) = search {
        let needle = query.to_lowercase();
        let mut results = Vec::new();
        for (name, title, content) in TOPICS {
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
        let value = json!({"query": query, "embedded": true, "results": results});
        ctx.emit(&value, |v| {
            let list = v["results"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                return format!("No guide mentions `{query}`. Topics: spotify docs");
            }
            list.iter()
                .map(|r| {
                    let lines = r["matches"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|m| {
                            format!("    {}: {}", m["line"], m["excerpt"].as_str().unwrap_or(""))
                        })
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
        return Ok(());
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
                "help": "spotify docs <topic>; spotify docs --search <text>; spotify docs --all; spotify <command> --help; spotify commands --json", "online": DOCS_URL});
            ctx.emit(&value, |_| {
                let mut out = String::from("Guides (offline, bundled with this CLI):\n");
                for (name, title, _) in TOPICS {
                    out.push_str(&format!("  spotify docs {name:<12} {title}\n"));
                }
                out.push_str(&format!(
                    "Search: spotify docs --search '<text>' · Online: {DOCS_URL}"
                ));
                out
            });
        }
        Some(name) => {
            let (name, title, content) =
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
                })?;
            let value = json!({"topic": name, "title": title, "format": "markdown", "command": format!("spotify docs {name}"), "content": content, "package_version": VERSION, "embedded": true});
            ctx.emit(&value, |_| format!("{content}{}", footer()));
        }
    }
    Ok(())
}

fn walk(command: &clap::Command, prefix: &str, out: &mut Vec<Value>) {
    let path = if prefix.is_empty() {
        command.get_name().to_owned()
    } else {
        format!("{prefix} {}", command.get_name())
    };
    let arguments: Vec<Value> = command
        .get_arguments()
        .filter(|a| !a.is_hide_set() && a.get_id() != "help" && a.get_id() != "version")
        .map(|a| {
            json!({
                "name": a.get_id().as_str(),
                "long": a.get_long(),
                "short": a.get_short().map(|c| c.to_string()),
                "positional": a.is_positional(),
                "required": a.is_required_set(),
                "global": a.is_global_set(),
                "description": a.get_help().map(ToString::to_string),
                "possible_values": a.get_possible_values().iter().map(|v| v.get_name().to_owned()).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut rendered = command.clone();
    out.push(json!({
        "command": path,
        "description": command.get_about().map(ToString::to_string),
        "usage": rendered.render_usage().to_string(),
        "aliases": command.get_visible_aliases().collect::<Vec<_>>(),
        "group": command.has_subcommands(),
        "arguments": arguments,
    }));
    for sub in command.get_subcommands().filter(|s| s.get_name() != "help") {
        walk(sub, &path, out);
    }
}

/// `spotify commands`.
pub fn commands(ctx: &Ctx) -> Result<()> {
    let mut out = Vec::new();
    let root = Cli::command();
    walk(&root, "", &mut out);
    let value = json!({"version": VERSION, "commands": out});
    ctx.emit(&value, |v| {
        let mut text = String::new();
        for c in v["commands"].as_array().into_iter().flatten().skip(1) {
            text.push_str(&format!("  {:<34} {}\n", c["command"].as_str().unwrap_or(""), c["description"].as_str().unwrap_or("")));
        }
        text.push_str("\nRun `spotify <command> --help` for arguments and examples; `spotify commands --json` for machine-readable help; `spotify docs` for guides.");
        text
    });
    Ok(())
}
