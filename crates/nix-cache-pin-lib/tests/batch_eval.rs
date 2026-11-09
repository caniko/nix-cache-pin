//! A batched store-path evaluation must agree with per-package `nix eval`.

use nix_cache_pin_lib::ext::StorePathRequest;
use nix_cache_pin_lib::narinfo::{eval_current_consumer_store_path, eval_store_paths};

async fn assert_matches_per_item(flake: &str, targets: &[&str]) {
    let requests: Vec<_> = targets
        .iter()
        .map(|target| StorePathRequest::Consumer {
            consumer_flake_ref: flake,
            candidate: None,
            target,
        })
        .collect();
    for (target, batched) in targets.iter().zip(eval_store_paths(&requests).await) {
        let single = eval_current_consumer_store_path(flake, target).await;
        assert_eq!(batched.ok(), single.ok(), "{target}");
    }
}

#[tokio::test]
async fn batched_paths_match_per_item_nix_eval() {
    // Needs a working `nix` (CI runs tests under `nix develop`); the build
    // sandbox has none.
    if std::env::var_os("NIX_BUILD_TOP").is_some()
        || std::process::Command::new("nix")
            .arg("--version")
            .output()
            .is_err()
    {
        eprintln!("skipping: nix is unavailable");
        return;
    }
    let system = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        format!(
            r#"{{
  outputs = _: let
    d = name: derivation {{ inherit name; system = "{system}"; builder = "/bin/sh"; }};
  in {{
    packages.{system} = {{ shadowed = d "package"; thrown = d "package-thrown"; }};
    shadowed = d "root";
    thrown = throw "losing candidate";
    t = {{ a = d "a"; "%61" = d "percent"; "x.y" = d "dotted"; "x\${{y}}" = d "interpolation"; }};
  }};
}}"#
        ),
    )
    .unwrap();
    let flake = format!("path:{}", dir.path().display());
    // A package shadows a root attribute; `%61` selects `a`; quoted dots and
    // `${` are taken literally or rejected exactly as Nix does.
    assert_matches_per_item(
        &flake,
        &["shadowed", "t.a", "t.%61", r#"t."x.y""#, r#"t."x${y}""#],
    )
    .await;
    // Nix looks up every candidate, so a throwing root fails `thrown` although
    // `packages.<system>.thrown` exists.
    assert_matches_per_item(&flake, &["shadowed", "thrown"]).await;
    // A missing target fails the batch; the fallback keeps the other result.
    assert_matches_per_item(&flake, &["shadowed", "t.missing"]).await;
}
