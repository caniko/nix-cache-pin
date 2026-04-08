use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_help() {
    Command::cargo_bin("nix-eval-store-path")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Evaluate nix store paths"));
}

#[test]
fn test_nonexistent_config_fails() {
    Command::cargo_bin("nix-eval-store-path")
        .unwrap()
        .args(["--config", "/nonexistent/path.json", "--rev", "abc123"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to read config"));
}
