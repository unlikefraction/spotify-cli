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
}
