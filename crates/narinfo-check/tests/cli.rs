use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_help() {
    Command::cargo_bin("narinfo-check")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Check if nix store paths"));
}

#[test]
fn test_no_args_fails() {
    Command::cargo_bin("narinfo-check")
        .unwrap()
        .env("SSL_CERT_FILE", "/nonexistent/ca-bundle.crt")
        .env("SSL_CERT_DIR", "/nonexistent/certs")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "provide at least one store path or use --config with --rev",
        ));
}
