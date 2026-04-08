use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_help() {
    Command::cargo_bin("cache-pin")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Pin a flake input"));
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
