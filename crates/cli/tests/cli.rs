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
        Command::new(env!("CARGO_BIN_EXE_spotify"))
            .args(args)
            .env("SILICON_HOME", self.home.path())
            .env("SPOTIFY_DAEMON_HOME", self.daemon.path())
            .env("SPOTIFY_API_URL", "http://127.0.0.1:9")
            .env("SPOTIFY_TELEMETRY", "0")
            // Contract tests must never leave a daemon behind.
            .env("SPOTIFY_DAEMON_AUTOSTART", "0")
            .env_remove("SPOTIFY_TEST_APP_SECRET")
            .env_remove("SILICON_ORG")
            // clap wraps help at COLUMNS when set; without it (and no terminal), at 100.
            .env_remove("COLUMNS")
            .output()
            .expect("run spotify")
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
