use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_help() {
    Command::cargo_bin("hydra-query")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Query Hydra CI"));
}

#[test]
fn test_nonexistent_config_fails() {
    Command::cargo_bin("hydra-query")
        .unwrap()
        .args(["--config", "/nonexistent/path.json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to read config"));
}
