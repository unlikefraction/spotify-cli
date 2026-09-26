//! Contract tests for the `spotify` binary: what Stemcell and agents rely on.

use std::process::{Command, Output};

use serde_json::Value;

struct Env {
    home: tempfile::TempDir,
    daemon: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home"),
            daemon: tempfile::tempdir().expect("daemon"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run spotify")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_spotify"));
        command
            .args(args)
            .env("SILICON_HOME", self.home.path())
            .env("SPOTIFY_DAEMON_HOME", self.daemon.path())
            .env("SPOTIFY_API_URL", "http://127.0.0.1:9")
            .env("SPOTIFY_TELEMETRY", "0")
            // Contract tests must never leave a daemon behind.
            .env("SPOTIFY_DAEMON_AUTOSTART", "0")
            .env_remove("SPOTIFY_TEST_APP_SECRET")
            .env_remove("SILICON_ORG")
            // Hints depend on it (and on stdout, which is a pipe here).
            .env_remove("SPOTIFY_HINTS")
            // clap wraps help at COLUMNS when set; without it (and no terminal), at 100.
            .env_remove("COLUMNS");
        command
    }
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn stderr_error(output: &Output) -> Value {
    let value: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr is not JSON ({e}): {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    value["error"].clone()
}

#[test]
fn iam_discovery_works_offline_and_is_bare() {
    let env = Env::new();
    let out = env.run(&["iam", "--json"]);
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "JSON commands keep stderr clean: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = stdout_json(&out);
    assert_eq!(value["app_id"], "spotify");
    assert_eq!(value["org_id"], "unlikefraction");
    assert!(
        value["repository"]
            .as_str()
            .expect("repo")
            .starts_with("https://github.com/")
    );
}

#[test]
fn login_status_without_a_session_is_false_and_exit_zero() {
    let env = Env::new();
    let out = env.run(&["login", "status", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout_json(&out)["authenticated"], false);
}

#[test]
fn logout_is_probeable_and_idempotent() {
    let env = Env::new();
    assert!(env.run(&["logout", "--help"]).status.success());
    let out = env.run(&["logout", "--json"]);
    assert!(out.status.success());
    assert_eq!(stdout_json(&out)["authenticated"], false);
    assert_eq!(
        stdout_json(&env.run(&["login", "status", "--json"]))["authenticated"],
        false
    );
}

#[test]
fn login_requires_an_slt_and_never_prompts() {
    let env = Env::new();
    let out = env.run(&["login", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let error = stderr_error(&out);
    assert_eq!(error["code"], "invalid_input");
    assert!(
        error["hint"]
            .as_str()
            .expect("hint")
            .contains("iam silicon-login")
    );
}

#[test]
fn a_short_word_is_never_sent_as_an_slt() {
    let env = Env::new();
    let out = env.run(&["login", "stauts", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let error = stderr_error(&out);
    assert_eq!(error["code"], "invalid_input");
    assert!(
        error["hint"]
            .as_str()
            .expect("hint")
            .contains("spotify login status")
    );
}

#[test]
fn login_against_an_unreachable_backend_is_a_transport_error() {
    let env = Env::new();
    let out = env.run(&["login", "oac_example", "--json"]);
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(stderr_error(&out)["code"], "backend_unavailable");
}

#[test]
fn config_set_is_strict_json() {
    let env = Env::new();
    let out = env.run(&[
        "config",
        "set",
        r#"{"telemetry": false, "strategy": "applescript"}"#,
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout_json(&out)["updated"]["strategy"], "applescript");
    let out = env.run(&["config", "get", "strategy", "--json"]);
    assert_eq!(stdout_json(&out)["value"], "applescript");
    let out = env.run(&["config", "set", r#"{"nope": 1}"#, "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let error = stderr_error(&out);
    assert_eq!(error["code"], "invalid_input");
    assert!(
        error["hint"].as_str().expect("hint").contains("telemetry"),
        "lists the valid keys"
    );
    let out = env.run(&[
        "config",
        "set",
        r#"{"telemetry": false, "telemetry": true}"#,
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "duplicate keys are rejected");
}

#[test]
fn usage_errors_are_structured_with_json() {
    let env = Env::new();
    let out = env.run(&["definitely-not-a-command", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(stderr_error(&out)["code"], "usage");
    let out = env.run(&["seek", "soon", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr_error(&out)["hint"]
            .as_str()
            .expect("hint")
            .contains("1:30")
    );
    let out = env.run(&[
        "trigger",
        "add",
        "--remaining",
        "30s",
        "--track",
        "spotify:track:x",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "--track needs --scope track");
}

#[test]
fn command_tree_and_docs_are_offline() {
    let env = Env::new();
    let out = env.run(&["commands", "--json"]);
    assert!(out.status.success());
    let tree = stdout_json(&out);
    let commands: Vec<&str> = tree["commands"]
        .as_array()
        .expect("list")
        .iter()
        .filter_map(|c| c["command"].as_str())
        .collect();
    for expected in [
        "spotify trigger add",
        "spotify login status",
        "spotify config set",
        "spotify queue remove",
        "spotify report",
    ] {
        assert!(commands.contains(&expected), "missing {expected}");
    }
    let out = env.run(&["docs", "triggers", "--json"]);
    assert!(out.status.success());
    assert!(
        stdout_json(&out)["content"]
            .as_str()
            .expect("content")
            .contains("# Triggers")
    );
    let out = env.run(&["docs", "nope", "--json"]);
    assert_eq!(stderr_error(&out)["code"], "not_found");
    for sub in [
        vec!["--help"],
        vec!["trigger", "add", "--help"],
        vec!["queue", "--help"],
        vec!["playlist", "--help"],
    ] {
        let out = env.run(&sub);
        assert!(out.status.success(), "{sub:?} --help exits 0");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("Examples") || sub == vec!["--help"],
            "{sub:?} shows examples"
        );
    }
}

#[test]
fn reports_validate_before_sending() {
    let env = Env::new();
    let out = env.run(&["report", "short", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let out = env.run(&[
        "report",
        "a long enough description of a bug",
        "--pr",
        "https://example.com/pull/1",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2));
    // With the backend down the report is saved locally with a gh command.
    let out = env.run(&["report", "a long enough description of a bug", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = stdout_json(&out);
    assert_eq!(value["filed"], false);
    assert!(
        value["file_it_yourself"]
            .as_str()
            .expect("gh")
            .starts_with("gh issue create")
    );
    // Attachments are text files: binaries, directories and this home's credentials are refused
    // before anything is sent.
    let binary = env.home.path().join("blob.bin");
    std::fs::write(&binary, b"\xcf\xfa\xed\xfe\0\0\0\x01text").expect("write");
    let session = env.home.path().join(".spotify/session.json");
    std::fs::create_dir_all(session.parent().expect("dir")).expect("mkdir");
    std::fs::write(&session, "{}").expect("write");
    let directory = env.home.path().to_path_buf();
    for (path, says) in [
        (&binary, "is not a text file"),
        (&session, "credentials"),
        (&directory, "is not a file"),
    ] {
        let path = path.to_string_lossy();
        let error = invalid(
            &env,
            &[
                "report",
                "a long enough description of a bug",
                "--attach",
                &path,
                "--json",
            ],
        );
        assert!(
            error["message"].as_str().expect("message").contains(says),
            "{error}"
        );
    }
    let log = env.home.path().join("daemon.log");
    std::fs::write(&log, "2026-09-26T10:42:57Z stopped\n").expect("write");
    let out = env.run(&[
        "report",
        "a long enough description of a bug",
        "--attach",
        &log.to_string_lossy(),
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Runs a command that must fail with `invalid_input` (exit 2) before reaching the daemon; with
/// autostart off, reaching it would be `daemon_unavailable` (exit 5) instead.
fn invalid(env: &Env, args: &[&str]) -> Value {
    let out = env.run(args);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
    let error = stderr_error(&out);
    assert_eq!(error["code"], "invalid_input", "{args:?}: {error}");
    error
}

#[test]
fn malformed_ids_are_rejected_up_front() {
    let env = Env::new();
    for args in [
        vec!["track", "garbage", "--json"],
        vec!["lyrics", "garbage", "--json"],
        vec!["playlist", "show", "garbage", "--json"],
        vec![
            "playlist",
            "add",
            "0000000000000000000000",
            "notatrack",
            "--json",
        ],
        vec!["queue", "add", "spotify:track:abc", "--json"],
        vec!["play", "spotify:album:tooshort", "--json"],
    ] {
        let error = invalid(&env, &args);
        assert!(
            error["hint"]
                .as_str()
                .expect("hint")
                .contains("spotify:<kind>:<id>"),
            "{args:?} lists the accepted forms: {error}"
        );
    }
    // Well-formed references of the wrong kind are caught too.
    invalid(
        &env,
        &[
            "playlist",
            "show",
            "spotify:album:78bpIziExqiI9qztvNFlQu",
            "--json",
        ],
    );
    invalid(
        &env,
        &[
            "playlist",
            "add",
            "37i9dQZF1DXcBWIGoYBM5M",
            "spotify:artist:7Ln80lUS6He07XvHI8qqHH",
            "--json",
        ],
    );
    // Episodes can be in playlists, but spotify_player cannot add them: `unsupported`, as the
    // daemon answers, still before any edit.
    let out = env.run(&[
        "playlist",
        "add",
        "37i9dQZF1DXcBWIGoYBM5M",
        "spotify:track:0BxE4FqsDD1Ot4YuBXwAPp",
        "spotify:episode:4rOoJ6Egrf8K2IrywzwOMk",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stderr_error(&out)["code"], "unsupported");
}

#[test]
fn limits_outside_what_can_be_honored_are_rejected() {
    let env = Env::new();
    for limit in ["0", "11", "50", "-1"] {
        let error = invalid(&env, &["search", "queen", "--limit", limit, "--json"]);
        assert!(
            error["hint"].as_str().expect("hint").contains("1 to 10"),
            "{error}"
        );
    }
    // `--limit -1` is the limit's value, not a flag (clap's `-- -1` tip made it a query word).
    let error = invalid(&env, &["search", "queen", "--limit=-1", "--json"]);
    assert_eq!(error["details"]["limit"], -1);
    invalid(&env, &["library", "liked", "--limit", "-5", "--json"]);
    invalid(
        &env,
        &["podcast", "search", "news", "--limit", "20", "--json"],
    );
    invalid(&env, &["library", "liked", "--limit", "0", "--json"]);
    invalid(&env, &["playlist", "list", "--limit", "0", "--json"]);
    invalid(&env, &["trigger", "history", "--limit", "501", "--json"]);
    // search_limit takes the same range.
    let error = invalid(
        &env,
        &["config", "set", r#"{"search_limit": 20}"#, "--json"],
    );
    assert!(
        error["message"]
            .as_str()
            .expect("message")
            .contains("from 1 to 10"),
        "{error}"
    );
}

#[test]
fn trigger_requests_are_checked_before_reading_spotify() {
    let env = Env::new();
    invalid(
        &env,
        &[
            "trigger", "add", "--end", "--scope", "every", "--times", "0", "--json",
        ],
    );
    let note = "n".repeat(1001);
    invalid(
        &env,
        &[
            "trigger", "add", "--end", "--local", "--note", &note, "--json",
        ],
    );
}

#[test]
fn config_type_errors_name_the_key() {
    let env = Env::new();
    let error = invalid(
        &env,
        &[
            "config",
            "set",
            r#"{"verify_timeout_ms": "fast"}"#,
            "--json",
        ],
    );
    assert_eq!(
        error["message"],
        r#"verify_timeout_ms must be an integer from 200 to 15000, not "fast"."#
    );
    let error = invalid(
        &env,
        &["config", "set", r#"{"strategy": "magic"}"#, "--json"],
    );
    assert!(
        error["message"].as_str().expect("message").contains("auto"),
        "lists the allowed values: {error}"
    );
}

#[test]
fn usage_errors_keep_the_details_in_json() {
    let env = Env::new();
    let out = env.run(&["config", "set", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let error = stderr_error(&out);
    assert_eq!(error["code"], "usage");
    assert!(
        error["message"]
            .as_str()
            .expect("message")
            .ends_with("<JSON>"),
        "{error}"
    );
    let out = env.run(&["search", "x", "--type", "bogus", "--json"]);
    let error = stderr_error(&out);
    assert!(
        error["hint"]
            .as_str()
            .expect("hint")
            .starts_with("Possible values: track, album"),
        "{error}"
    );
    // Value errors say which argument and show the command's usage, like every usage error.
    let out = env.run(&["search", "queen", "--limit", "abc", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let error = stderr_error(&out);
    assert_eq!(error["details"]["argument"], "--limit <LIMIT>");
    assert_eq!(
        error["details"]["usage"],
        "spotify search [OPTIONS] <QUERY>..."
    );
    // `config set key value` points to the JSON form.
    let out = env.run(&["config", "set", "search_limit", "5", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr_error(&out)["hint"]
            .as_str()
            .expect("hint")
            .contains(r#"spotify config set '{"search_limit": 5}'"#)
    );
    // queue add --type offers only what the queue accepts.
    let out = env.run(&["queue", "add", "--search", "x", "--type", "album", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr_error(&out)["details"]["possible_values"],
        serde_json::json!(["track", "episode"])
    );
    // track --type offers only what can be looked up by id.
    let out = env.run(&[
        "track",
        "4IzpgR6RCEkRqMHbJF38Wp",
        "--type",
        "episode",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr_error(&out)["details"]["possible_values"],
        serde_json::json!(["track", "album", "artist", "playlist"])
    );
}

#[test]
fn config_set_key_equals_value_points_to_the_json_form() {
    let env = Env::new();
    let out = env.run(&["config", "set", "search_limit=5", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let error = stderr_error(&out);
    assert_eq!(error["code"], "invalid_input");
    assert!(
        error["hint"]
            .as_str()
            .expect("hint")
            .contains(r#"spotify config set '{"search_limit": 5}'"#),
        "{error}"
    );
    // Nothing was saved.
    let out = env.run(&["config", "get", "search_limit", "--json"]);
    assert!(out.status.success());
    assert_ne!(stdout_json(&out)["value"], 5);
}

#[test]
fn help_examples_stay_copyable() {
    let env = Env::new();
    let out = env.run(&["login", "--help"]);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("--approve-scopes \\\n    | jq -r .slt | spotify login --token-file -"),
        "{help}"
    );
    let out = env.run(&["report", "--help"]);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("--pr https://github.com/unlikefraction/spotify-cli/pull/42\n"),
        "{help}"
    );
    let out = env.run(&["playlist", "fork", "--help"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("--name <NAME>"));
}

#[test]
fn no_command_orients_without_starting_anything() {
    let env = Env::new();
    let out = env.run(&[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("spotify-cli "), "{text}");
    assert!(text.contains("\nTry:\n  spotify "), "{text}");
    assert!(
        text.contains("not logged in"),
        "a fresh home has no session: {text}"
    );
    assert!(text.contains("spotify-daemon not running"), "{text}");
    let out = env.run(&["--json"]);
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    let value = stdout_json(&out);
    let checks: Vec<&str> = value["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .filter_map(|c| c["check"].as_str())
        .collect();
    for expected in [
        "spotify_app",
        "spotify_player",
        "spotify_signed_in",
        "daemon",
        "iam_login",
    ] {
        assert!(checks.contains(&expected), "{expected}: {value}");
    }
    let tries = value["try"].as_array().expect("try");
    assert!((1..=5).contains(&tries.len()), "{value}");
    assert!(tries.iter().all(|t| {
        t["command"]
            .as_str()
            .is_some_and(|c| c.starts_with("spotify "))
    }));
    // It never started a daemon.
    assert!(
        std::fs::read_dir(env.daemon.path())
            .expect("dir")
            .next()
            .is_none(),
        "the daemon home stays empty"
    );
}

#[test]
fn root_help_groups_commands_by_goal() {
    let env = Env::new();
    for flag in ["-h", "--help"] {
        let out = env.run(&[flag]);
        assert!(out.status.success());
        let help = String::from_utf8_lossy(&out.stdout);
        let mut at = 0;
        for goal in [
            "Listen:",
            "Find:",
            "Lyrics & details:",
            "Queue:",
            "Playlists:",
            "Podcasts:",
            "Triggers (Ting):",
            "Account & login:",
            "Setup & diagnose:",
        ] {
            let found = help[at..].find(&format!("\n{goal}\n")).map(|i| i + at);
            assert!(
                found.is_some(),
                "{flag}: {goal} missing or out of order:\n{help}"
            );
            at = found.unwrap_or(at);
        }
        assert!(
            help.contains("\n  lyrics       Lyrics of any song"),
            "{help}"
        );
        assert!(
            help.contains("\n                 spotify lyrics spotify:track:"),
            "{help}"
        );
        assert!(
            !help.contains("\nCommands:"),
            "the grouped list replaces clap's: {help}"
        );
    }
}

#[test]
fn how_answers_in_plain_words() {
    let env = Env::new();
    for (question, command, example) in [
        (
            "notify me 30 seconds before the song ends",
            "spotify trigger add",
            "--remaining 30s",
        ),
        (
            "lyrics of a song that isn't playing",
            "spotify lyrics",
            "spotify lyrics ",
        ),
        (
            "play my liked songs shuffled",
            "spotify play",
            "--liked --random",
        ),
        // The second newcomer's questions.
        (
            "keep Spotify from jumping to the front when playing",
            "spotify config get",
            "spotify config get keep_spotify_in_background",
        ),
        (
            "which commands change my library",
            "spotify commands",
            r#"select(.changes | index("library"))"#,
        ),
        (
            "see the album a track belongs to without playing it",
            "spotify track",
            "spotify track spotify:track:",
        ),
    ] {
        let out = env.run(&["how", question, "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stderr.is_empty(), "no hints with --json");
        let value = stdout_json(&out);
        assert_eq!(
            value["commands"][0]["command"], command,
            "{question}: {value}"
        );
        assert!(
            value["commands"][0]["examples"][0]["command"]
                .as_str()
                .is_some_and(|e| e.contains(example)),
            "{question}: {value}"
        );
    }
    let value = stdout_json(&env.run(&["how", "why", "is", "automation", "denied", "--json"]));
    assert_eq!(value["errors"][0]["code"], "automation_permission_denied");
    let value = stdout_json(&env.run(&["how", "what is an SLT", "--json"]));
    assert!(
        value["guides"][0]["command"]
            .as_str()
            .is_some_and(|c| c.starts_with("spotify docs ")),
        "{value}"
    );
    // Human answers end with a `Next:` hint on stderr when stdout is a terminal. Here stdout is
    // a pipe, so there is none (it would print before the pipe's reader shows the answer)...
    let out = env.run(&["how", "skip this song"]);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("spotify next — "));
    assert!(
        out.stderr.is_empty(),
        "no hints when stdout is piped: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // ...unless SPOTIFY_HINTS=always asks for them anyway.
    let out = env
        .command(&["how", "skip this song"])
        .env("SPOTIFY_HINTS", "always")
        .output()
        .expect("run");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Next: spotify next --help"
    );
    let out = Command::new(env!("CARGO_BIN_EXE_spotify"))
        .args(["how", "skip this song"])
        .env("SILICON_HOME", env.home.path())
        .env("SPOTIFY_DAEMON_HOME", env.daemon.path())
        .env("SPOTIFY_TELEMETRY", "0")
        .env("SPOTIFY_DAEMON_AUTOSTART", "0")
        .env("SPOTIFY_HINTS", "0")
        .output()
        .expect("run");
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let error = invalid(&env, &["how", "x", "--limit", "11", "--json"]);
    assert!(error["hint"].as_str().expect("hint").contains("1 to 10"));
}

#[test]
fn docs_print_one_section() {
    let env = Env::new();
    let out = env.run(&["docs", "triggers", "--section", "without ting", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = stdout_json(&out);
    assert_eq!(value["section"], "Without Ting");
    assert!(
        value["content"]
            .as_str()
            .expect("content")
            .starts_with("## Without Ting\n")
    );
    let out = env.run(&["docs", "triggers", "--section", "nope", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let error = stderr_error(&out);
    assert_eq!(error["code"], "not_found");
    assert!(
        error["hint"]
            .as_str()
            .expect("hint")
            .contains("Without Ting")
    );
    // --section needs a topic, and a heading.
    assert_eq!(env.run(&["docs", "--section", "x"]).status.code(), Some(2));
    let out = env.run(&["docs", "triggers", "--section", " ", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stderr_error(&out)["code"], "invalid_input");
}

#[test]
fn a_reader_that_went_away_is_not_a_crash() {
    // `spotify … 2>&1 | head`: once the reader is gone, writes fail. The command still ends with
    // its own exit code (a panic would make it 101).
    let env = Env::new();
    for (args, code) in [
        (&["how", "skip this song"][..], 0),
        (&["docs", "--all"][..], 0),
        (&["how", "the"][..], 2),
        (&["frobnicate"][..], 2),
    ] {
        let (reader, writer) = std::io::pipe().expect("pipe");
        drop(reader);
        let status = env
            .command(args)
            .stdout(writer.try_clone().expect("pipe"))
            .stderr(writer)
            .status()
            .expect("run spotify");
        assert_eq!(status.code(), Some(code), "{args:?}");
    }
}

#[test]
fn completions_are_generated_for_each_shell() {
    let env = Env::new();
    for (shell, start) in [
        ("zsh", "#compdef spotify"),
        ("bash", "_spotify()"),
        ("fish", "# Print an optspec"),
        (
            "powershell",
            "\nusing namespace System.Management.Automation",
        ),
    ] {
        let out = env.run(&["completions", shell]);
        assert!(out.status.success(), "{shell}");
        let script = String::from_utf8_lossy(&out.stdout);
        assert!(
            script.starts_with(start),
            "{shell}: {}",
            &script[..script.len().min(80)]
        );
        assert!(script.contains("lyrics"), "{shell} completes subcommands");
    }
    let out = env.run(&["completions", "--help"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("Install, then open a new shell:"));
    assert_eq!(env.run(&["completions", "tcsh"]).status.code(), Some(2));
}

#[test]
fn the_manifest_says_what_each_command_needs_and_returns() {
    let env = Env::new();
    let manifest = stdout_json(&env.run(&["commands", "--json"]));
    let commands = manifest["commands"].as_array().expect("commands");
    for command in commands {
        // The first version's fields stay.
        for field in [
            "command",
            "description",
            "usage",
            "aliases",
            "group",
            "arguments",
        ] {
            assert!(command.get(field).is_some(), "{field}: {command}");
        }
        for arg in command["arguments"].as_array().expect("arguments") {
            // The first version listed `true`/`false` for on/off flags; that stays.
            if arg["type"] == "boolean" {
                assert_eq!(
                    arg["possible_values"],
                    serde_json::json!(["true", "false"]),
                    "{arg}"
                );
            }
        }
        for need in [
            "spotify_app",
            "spotify_player_signed_in",
            "iam_login",
            "premium",
            "controls_playback",
            "network",
        ] {
            assert!(
                command["requirements"][need].is_boolean(),
                "{need}: {}",
                command["command"]
            );
        }
        assert!(
            command["errors"]
                .as_array()
                .is_some_and(|e| e.iter().any(|e| e["code"] == "usage" && e["exit"] == 2)),
            "{}",
            command["command"]
        );
    }
    let lyrics = commands
        .iter()
        .find(|c| c["path"] == "lyrics")
        .expect("lyrics");
    assert!(
        lyrics["description"]
            .as_str()
            .expect("about")
            .contains("nothing has to play")
    );
    assert!(
        lyrics["output"]
            .as_array()
            .expect("output")
            .iter()
            .any(|o| o["field"] == "lines[]")
    );
    assert!(
        lyrics["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e["code"] == "not_found")
    );
    assert_eq!(lyrics["goal"], "lyrics_and_details");
    let status = commands
        .iter()
        .find(|c| c["path"] == "status")
        .expect("status");
    assert_eq!(status["requirements"]["iam_login"], false);
    assert_eq!(status["requirements"]["spotify_app"], true);
    assert_eq!(manifest["goals"].as_array().map(Vec::len), Some(9));
}

/// `spotify <args>` as text: (exit code, stdout, stderr).
fn text(env: &Env, args: &[&str]) -> (Option<i32>, String, String) {
    let out = env.run(args);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn lyrics_take_a_track_and_say_how_to_find_one() {
    let env = Env::new();
    // `--search` is gone: finding the track is `spotify search`'s job.
    let (code, _, stderr) = text(&env, &["lyrics", "--search", "505"]);
    assert_eq!(code, Some(2), "{stderr}");
    // Not clap's `-- --search`, which would pass the flag as the track.
    assert!(
        stderr.contains("`spotify lyrics` has no --search; it takes a track: find it with `spotify search 505 --type track`"),
        "{stderr}"
    );
    assert!(!stderr.contains("-- --search"), "{stderr}");
    // Words instead of a track: find it first.
    let out = env.run(&["lyrics", "arctic", "monkeys", "505", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let error = stderr_error(&out);
    assert_eq!(error["code"], "usage");
    assert!(
        error["hint"]
            .as_str()
            .expect("hint")
            .contains("find it with `spotify search 'arctic monkeys 505' --type track`, then run `spotify lyrics <uri>`"),
        "{error}"
    );
    let (code, _, stderr) = text(&env, &["lyrics", "arctic", "monkeys", "505"]);
    assert_eq!(code, Some(2));
    assert!(
        stderr.contains("hint: `spotify lyrics` takes a track, not search words"),
        "{stderr}"
    );
    // One word that is no id: the same advice, before anything is asked of Spotify.
    let error = invalid(&env, &["lyrics", "505", "--json"]);
    assert_eq!(error["message"], "`505` is not a track URI, link or id.");
    assert!(
        error["hint"]
            .as_str()
            .expect("hint")
            .contains("spotify search 505 --type track"),
        "{error}"
    );
    assert_eq!(
        error["details"]["search"],
        "spotify search 505 --type track"
    );
    let (_, help, _) = text(&env, &["lyrics", "--help"]);
    assert!(!help.contains("--search"), "{help}");
    assert!(
        help.contains("spotify search 'bohemian rhapsody' --type track"),
        "{help}"
    );
}

#[test]
fn queue_show_lists_and_unknown_queue_commands_say_what_exists() {
    let env = Env::new();
    let (code, help, _) = text(&env, &["queue", "show", "--help"]);
    assert_eq!(code, Some(0));
    assert!(help.contains("Usage: spotify queue list"), "{help}");
    let out = env.run(&["queue", "next", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let hint = stderr_error(&out)["hint"]
        .as_str()
        .expect("hint")
        .to_owned();
    assert!(
        hint.starts_with("`spotify queue` has no subcommand `next`; its subcommands are list (show, ls), add, remove (rm), move (mv), clear. To skip to the next track: spotify next."),
        "{hint}"
    );
    let (_, _, stderr) = text(&env, &["queue", "next"]);
    assert!(
        stderr.contains("To skip to the next track: spotify next."),
        "{stderr}"
    );
}

#[test]
fn help_lists_a_commands_own_flags_before_the_global_ones() {
    let env = Env::new();
    for args in [
        &["search", "--help"][..],
        &["search", "-h"],
        &["play", "--help"],
    ] {
        let (code, help, _) = text(&env, args);
        assert_eq!(code, Some(0));
        let own = help.find("\nOptions:\n").expect("Options");
        let global = help.find("\nGlobal options:\n").expect("Global options");
        assert!(own < global, "{args:?}: {help}");
        let limit = help.find("--limit").expect("--limit");
        let json = help.find("--json").expect("--json");
        assert!(
            own < limit && limit < global && global < json,
            "{args:?}: {help}"
        );
        assert!(help[global..].contains("-h, --help"), "{args:?}: {help}");
    }
    // A command without flags of its own has only the global ones.
    let (_, help, _) = text(&env, &["pause", "--help"]);
    assert!(!help.contains("\nOptions:\n"), "{help}");
    assert!(help.contains("\nGlobal options:\n"), "{help}");
    let (code, version, _) = text(&env, &["-V"]);
    assert_eq!(code, Some(0));
    assert!(version.starts_with("spotify "), "{version}");
}

#[test]
fn the_iam_cli_is_named_as_a_separate_install() {
    let env = Env::new();
    for args in [
        &["login", "--help"][..],
        &["--help"],
        &["iam"],
        &["iam", "--help"],
    ] {
        let (code, out, _) = text(&env, args);
        assert_eq!(code, Some(0), "{args:?}");
        assert!(
            out.contains("cargo install silicon-iam-cli"),
            "{args:?}: {out}"
        );
    }
    let (_, out, _) = text(&env, &["iam"]);
    assert!(
        out.contains("Silicon IAM's own CLI, separate from spotify-cli"),
        "{out}"
    );
    let value = stdout_json(&env.run(&["iam", "--json"]));
    assert_eq!(value["iam_cli"]["install"], "cargo install silicon-iam-cli");
}

/// A Silicon mints its SLT for "$SILICON_ORG" (not a bare `<org>`), and a fresh SILICON_HOME
/// needs the Silicon's own `iam` sign-in first: the root help, `spotify iam` and `login --help`
/// all say so.
#[test]
fn silicon_login_help_names_silicon_org_and_the_first_sign_in() {
    let env = Env::new();
    for args in [
        &["--help"][..],
        &["iam"],
        &["iam", "--help"],
        &["login", "--help"],
    ] {
        let (code, out, _) = text(&env, args);
        assert_eq!(code, Some(0), "{args:?}");
        assert!(
            out.contains(
                r#"silicon-login --app-id spotify --grant-org "$SILICON_ORG" --approve-scopes"#
            ),
            "{args:?}: {out}"
        );
        assert!(
            !out.contains("silicon-login --app-id spotify --grant-org <org>"),
            "{args:?}: {out}"
        );
        assert!(
            out.contains("iam silicon-login --sid si:<handle>"),
            "{args:?}: {out}"
        );
    }
    // The example is the command alone: its comment is two spaces away, so neither the manifest
    // nor `spotify how` hands out "iam silicon-login --sid si:<handle> Once per SILICON_HOME: …".
    let manifest = stdout_json(&env.run(&["commands", "--json"]));
    let login = manifest["commands"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["path"] == "login"))
        .expect("login");
    assert!(
        login["examples"].as_array().is_some_and(|e| e
            .iter()
            .any(|e| e["command"] == "iam silicon-login --sid si:<handle>"
                && e["description"].is_string())),
        "{login}"
    );
    let value = stdout_json(&env.run(&["how", "how do I log in as a silicon", "--json"]));
    assert!(
        value["commands"][0]["examples"]
            .as_array()
            .is_some_and(|e| e
                .iter()
                .all(|e| !e["command"].as_str().unwrap_or("").contains("Once per"))),
        "{value}"
    );
    let value = stdout_json(&env.run(&["iam", "--json"]));
    assert!(
        value["login"]
            .as_str()
            .is_some_and(|l| l.contains(r#"--grant-org "$SILICON_ORG""#)),
        "{value}"
    );
    assert!(
        value["iam_session"]
            .as_str()
            .is_some_and(|l| l.contains("fresh SILICON_HOME")),
        "{value}"
    );
    // The error without an SLT says the same.
    let error = invalid(&env, &["login", "--json"]);
    assert!(
        error["hint"]
            .as_str()
            .is_some_and(|h| h.contains(r#"--grant-org "$SILICON_ORG""#)),
        "{error}"
    );
}

#[test]
fn auth_help_says_what_happens_and_what_a_headless_silicon_does() {
    let env = Env::new();
    let (code, out, _) = text(&env, &["auth", "login", "--help"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("Examples:\n  spotify auth login "), "{out}");
    // Help is wrapped: compare the words.
    let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
    for said in [
        "consent page in the default browser on this Mac",
        "A Carbon clicks Agree once",
        "A headless Silicon cannot click Agree: ask a Carbon",
        "spotify auth status",
    ] {
        assert!(flat.contains(said), "{said}: {out}");
    }
    let (_, out, _) = text(&env, &["auth", "status", "--help"]);
    assert!(
        out.contains("  spotify auth status --json | jq .authenticated"),
        "{out}"
    );
    let (_, out, _) = text(&env, &["auth", "--help"]);
    assert!(out.contains("headless Silicon"), "{out}");
}

/// `zsh -f` (and `bash --norc`) skip the startup files the install lines write to.
#[test]
fn completion_help_says_a_bare_shell_skips_its_startup_file() {
    let env = Env::new();
    let (_, out, _) = text(&env, &["completions", "--help"]);
    for said in [
        "`zsh -f` skips ~/.zshrc",
        "`bash --norc` skips ~/.bashrc",
        "source ~/.zshrc",
    ] {
        assert!(out.contains(said), "{said}: {out}");
    }
}

#[test]
fn a_mistyped_command_suggests_the_closest_one_first() {
    let env = Env::new();
    let (code, _, err) = text(&env, &["lyircs"]);
    assert_eq!(code, Some(2));
    let tip = err.find("tip: did you mean 'lyrics'?");
    let usage = err.find("Usage: spotify");
    assert!(tip.is_some() && usage.is_some() && tip < usage, "{err}");
    assert!(!err.contains("'playlists'"), "{err}");
    let out = env.run(&["lyircs", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    let error = stderr_error(&out);
    assert!(
        error["hint"]
            .as_str()
            .is_some_and(|h| h.starts_with("Did you mean `spotify lyrics`?")),
        "{error}"
    );
    assert_eq!(error["details"]["suggestions"][0], "spotify lyrics");
}

#[test]
fn the_command_list_marks_what_changes_something() {
    let env = Env::new();
    let (code, out, _) = text(&env, &["commands"]);
    assert_eq!(code, Some(0));
    let line = |command: &str| {
        out.lines()
            .find(|l| l.starts_with(&format!("  {command} ")))
            .unwrap_or_else(|| panic!("no {command}: {out}"))
            .to_owned()
    };
    assert!(line("spotify like").ends_with("✎ changes library"), "{out}");
    assert!(
        line("spotify playlist add").ends_with("✎ changes playlists"),
        "{out}"
    );
    assert!(!line("spotify status").contains('✎'), "{out}");
    assert!(!line("spotify library").contains('✎'), "{out}");
    assert!(
        out.contains("✎ marks the commands that change something"),
        "{out}"
    );
    let manifest = stdout_json(&env.run(&["commands", "--json"]));
    let readable: Vec<&str> = manifest["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .filter(|c| c["has_read_only_form"] == true)
        .filter_map(|c| c["path"].as_str())
        .collect();
    for path in ["status", "volume", "search", "setup", "update"] {
        assert!(readable.contains(&path), "{path}: {readable:?}");
    }
    for path in ["play", "like", "report"] {
        assert!(!readable.contains(&path), "{path}: {readable:?}");
    }
}

#[test]
fn docs_search_one_topic_and_hyphenated_text() {
    let env = Env::new();
    let value = stdout_json(&env.run(&["docs", "--search", "--random", "--json"]));
    assert_eq!(value["query"], "--random");
    assert!(
        value["results"].as_array().is_some_and(|r| !r.is_empty()),
        "{value}"
    );
    let value = stdout_json(&env.run(&["docs", "playback", "--search", "fallback", "--json"]));
    let topics: Vec<&str> = value["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|r| r["topic"].as_str())
        .collect();
    assert_eq!(topics, ["playback"], "{value}");
    let out = env.run(&["docs", "nope", "--search", "x", "--json"]);
    assert_eq!(stderr_error(&out)["code"], "not_found");
    let (code, out, _) = text(&env, &["docs", "ting", "--search", "zzqx-nothing"]);
    assert_eq!(code, Some(0));
    assert!(out.starts_with("The ting guide does not mention"), "{out}");
    let out = env.run(&[
        "docs",
        "triggers",
        "--section",
        "x",
        "--search",
        "y",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let out = env.run(&["docs", "--search", " ", "--json"]);
    assert_eq!(stderr_error(&out)["code"], "invalid_input");
}

#[test]
fn the_manifest_says_what_each_command_changes() {
    let env = Env::new();
    let manifest = stdout_json(&env.run(&["commands", "--json"]));
    let known: Vec<&str> = manifest["changes"]
        .as_array()
        .expect("changes")
        .iter()
        .filter_map(|c| c["change"].as_str())
        .collect();
    let find = |path: &str| {
        manifest["commands"]
            .as_array()
            .and_then(|c| c.iter().find(|c| c["path"] == path))
            .cloned()
            .unwrap_or_else(|| panic!("no {path}"))
    };
    for command in manifest["commands"].as_array().expect("commands") {
        let mutates = command["mutates"].as_bool().expect("mutates");
        let changes = command["changes"].as_array().expect("changes");
        for change in changes {
            assert!(known.contains(&change.as_str().expect("id")), "{command}");
        }
        if !changes.is_empty() {
            assert!(mutates, "{}", command["command"]);
        }
    }
    for (path, mutates, changes) in [
        ("status", false, serde_json::json!([])),
        ("lyrics", false, serde_json::json!([])),
        ("queue list", false, serde_json::json!([])),
        ("play", true, serde_json::json!(["playback"])),
        ("queue add", true, serde_json::json!(["playback"])),
        ("like", true, serde_json::json!(["library"])),
        ("playlist fork", true, serde_json::json!(["playlists"])),
        ("config set", true, serde_json::json!(["config"])),
        ("trigger add", true, serde_json::json!(["triggers"])),
        ("daemon restart", true, serde_json::json!(["daemon"])),
        ("login", true, serde_json::json!(["session"])),
    ] {
        let command = find(path);
        assert_eq!(command["mutates"], mutates, "{path}");
        assert_eq!(command["changes"], changes, "{path}");
    }
    assert_eq!(find("search")["read_only_when"], "without --play");
}

#[test]
fn help_says_what_fork_and_liked_shuffle_do() {
    let env = Env::new();
    let (_, help, _) = text(&env, &["playlist", "fork", "--help"]);
    for expected in ["private", "visibility", "spotify playlist sync"] {
        assert!(help.contains(expected), "{expected}: {help}");
    }
    let (_, help, _) = text(&env, &["play", "--help"]);
    assert!(help.contains("spotify play --liked --shuffle"), "{help}");
    assert!(
        help.contains("With --liked it is the same as --random"),
        "{help}"
    );
    assert!(
        help.contains("every start goes through the Spotify Web API first"),
        "{help}"
    );
    assert!(help.contains("shows and Liked Songs directly"), "{help}");
}

/// The install lines `spotify completions --help` shows, for a shell.
fn install_lines(env: &Env, shell: &str) -> Vec<String> {
    let (_, help, _) = text(env, &["completions", "--help"]);
    let examples = help
        .split("Examples:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("examples")
        .to_owned();
    let mut lines = Vec::new();
    for line in examples.lines().map(str::trim) {
        let about = match shell {
            "zsh" => line.contains("zfunc") || line.contains("zshrc"),
            "bash" => line.contains("bash"),
            _ => false,
        };
        if about {
            lines.push(line.to_owned());
        }
    }
    lines
}

/// The completion one-liners work as they are, in a new HOME, with the shells on this machine.
#[test]
fn completion_install_lines_work_as_they_are() {
    let env = Env::new();
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_spotify"))
        .parent()
        .expect("dir")
        .to_owned();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    for (shell, check, expected) in [
        (
            "zsh",
            "source ~/.zshrc; print -r -- ${_comps[spotify]}",
            "_spotify",
        ),
        (
            "bash",
            "source ~/.bashrc; COMP_WORDS=(spotify que); COMP_CWORD=1; _spotify spotify que spotify; echo ${COMPREPLY[*]}",
            "queue",
        ),
    ] {
        let binary = format!("/bin/{shell}");
        if !std::path::Path::new(&binary).exists() {
            continue;
        }
        let home = tempfile::tempdir().expect("home");
        let lines = install_lines(&env, shell);
        assert_eq!(lines.len(), 2, "{shell}: {lines:?}");
        let flags: &[&str] = if shell == "zsh" {
            &["-f", "-c"]
        } else {
            &["--norc", "--noprofile", "-c"]
        };
        let script = format!("{}\n{check}", lines.join("\n"));
        let out = Command::new(&binary)
            .args(flags)
            .arg(&script)
            .env("HOME", home.path())
            .env("PATH", &path)
            .env("SILICON_HOME", env.home.path())
            .env("SPOTIFY_DAEMON_AUTOSTART", "0")
            .env("SPOTIFY_TELEMETRY", "0")
            .output()
            .expect("run shell");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            expected,
            "{shell}: {script}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Every line of the fish script is fish's own (clap_complete writes it).
    let (_, script, _) = text(&env, &["completions", "fish"]);
    assert!(
        script.contains("complete -c spotify -n \"__fish_spotify_needs_command\" -f -a \"lyrics\""),
        "{script}"
    );
    // The root help shows both zsh lines, and `how` does too.
    let (_, help, _) = text(&env, &["--help"]);
    assert!(
        help.contains("mkdir -p ~/.zfunc && spotify completions zsh > ~/.zfunc/_spotify\n                 echo 'fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit' >> ~/.zshrc"),
        "{help}"
    );
}

#[test]
fn queue_list_warnings_are_printed() {
    // Rendering is covered by the unit tests; here: the manifest documents the field.
    let env = Env::new();
    let manifest = stdout_json(&env.run(&["commands", "--json"]));
    let queue = manifest["commands"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["path"] == "queue list"))
        .cloned()
        .expect("queue list");
    assert!(
        queue["output"]
            .as_array()
            .expect("output")
            .iter()
            .any(|o| o["field"] == "warnings[]"),
        "{queue}"
    );
}
