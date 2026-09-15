use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_help() {
    Command::cargo_bin("cache-pin")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Pin a flake input"))
        .stdout(predicate::str::contains("--check-current"))
        .stdout(predicate::str::contains("--json"))
        .stdout(predicate::str::contains("--plan"));
}

#[test]
fn test_no_args_fails() {
    Command::cargo_bin("cache-pin")
        .unwrap()
        .assert()
        .failure()
        .stderr(predicate::str::contains("--config"));
}

#[test]
fn test_nonexistent_config_fails() {
    Command::cargo_bin("cache-pin")
        .unwrap()
        .args(["--config", "/nonexistent/path.json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to read config"));
}

#[test]
fn plan_conflicts_with_mutating_flags() {
    for flag in ["--update", "--no-lock"] {
        Command::cargo_bin("cache-pin")
            .unwrap()
            .args(["--config", "config.json", "--plan", flag])
            .assert()
            .failure()
            .stderr(predicate::str::contains("cannot be used with"));
    }
}
