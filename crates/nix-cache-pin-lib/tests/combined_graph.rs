use nix_cache_pin_lib::{
    config::PinConfig,
    error::{Error, Result},
    ext::{
        EvalAttrRequest, EvalConsumerAttrRequest, ExternalCommands, RealCommands, RevisionOrder,
    },
    merge::group_configs,
    narinfo, transaction,
};
use std::{fs, path::Path, process::Command, sync::Arc};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

fn command(root: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn commit(root: &Path) -> String {
    command(root, "git", &["add", "flake.nix"]);
    command(
        root,
        "git",
        &["-c", "commit.gpgsign=false", "commit", "-m", "fixture"],
    );
    command(root, "git", &["rev-parse", "HEAD"])
}

// Only revision ordering is fixture-local; all evaluation and staged locking
// execute real Nix commands against the two-input graph.
struct FixtureCommands(std::path::PathBuf);
impl ExternalCommands for FixtureCommands {
    async fn eval_store_path(
        &self,
        source: &str,
        rev: &str,
        arch: &str,
        output: &str,
        prefix: &str,
        pkg: &str,
    ) -> Result<String> {
        RealCommands
            .eval_store_path(source, rev, arch, output, prefix, pkg)
            .await
    }
    async fn eval_consumer_store_path(
        &self,
        consumer: &str,
        input: &str,
        source: &str,
        rev: &str,
        target: &str,
    ) -> Result<String> {
        RealCommands
            .eval_consumer_store_path(consumer, input, source, rev, target)
            .await
    }
    async fn eval_current_consumer_store_path(
        &self,
        consumer: &str,
        target: &str,
    ) -> Result<String> {
        RealCommands
            .eval_current_consumer_store_path(consumer, target)
            .await
    }
    async fn eval_attr_value(&self, request: EvalAttrRequest<'_>) -> Result<String> {
        RealCommands.eval_attr_value(request).await
    }
    async fn eval_consumer_attr_value(
        &self,
        request: EvalConsumerAttrRequest<'_>,
    ) -> Result<String> {
        RealCommands.eval_consumer_attr_value(request).await
    }
    async fn list_commits(&self, _: &str, _: &str, _: usize) -> Result<Vec<String>> {
        unreachable!()
    }
    async fn run_flake_lock(&self, _: &str) -> Result<()> {
        unreachable!()
    }
    async fn compare_revisions(
        &self,
        _: &str,
        _: &str,
        current: &str,
        candidate: &str,
        _: usize,
    ) -> Result<RevisionOrder> {
        assert!(Command::new("git")
            .current_dir(&self.0)
            .args(["merge-base", "--is-ancestor", current, candidate])
            .status()
            .unwrap()
            .success());
        Ok(RevisionOrder::Newer)
    }
}

#[tokio::test]
async fn independent_passes_cannot_commit_a_combined_cache_miss() {
    let directory = tempfile::tempdir().unwrap();
    // Use a private local store so the regression also runs inside Nix's test
    // sandbox and never requires the operator's daemon or store writer lease.
    std::env::set_var(
        "NIX_REMOTE",
        format!("local?root={}/nix-root", directory.path().display()),
    );
    std::env::set_var("NIX_CONFIG", "experimental-features = nix-command flakes\n");
    std::env::set_var("XDG_CACHE_HOME", directory.path().join("cache"));
    std::env::set_var("XDG_CONFIG_HOME", directory.path().join("config"));
    let source = directory.path().join("source");
    let consumer = directory.path().join("consumer");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&consumer).unwrap();
    for root in [&source, &consumer] {
        command(root, "git", &["init", "-b", "main"]);
        command(root, "git", &["config", "core.hooksPath", "/dev/null"]);
        command(root, "git", &["config", "user.name", "Test"]);
        command(root, "git", &["config", "user.email", "test@example.com"]);
    }
    fs::write(source.join("flake.nix"), "{ outputs = _: { value = 1; packages.x86_64-linux.hello = { outPath = \"/nix/store/independent-app\"; version = \"1.0.0\"; }; }; }").unwrap();
    let old = commit(&source);
    fs::write(source.join("flake.nix"), "{ outputs = _: { value = 2; packages.x86_64-linux.hello = { outPath = \"/nix/store/independent-app\"; version = \"1.0.0\"; }; }; }").unwrap();
    let new = commit(&source);
    let reference = format!("git+file://{}", source.display());
    fs::write(
        consumer.join("flake.nix"),
        format!(
            r#"{{
      inputs.a.url = "{reference}?rev={old}";
      inputs.b.url = "{reference}?rev={old}";
      outputs = {{ self, a, b, ... }}: {{ inputs.app = {{ version = "1.0.0"; outPath =
        assert !builtins.pathExists "${{self}}/user-untracked";
        assert !builtins.any (name: builtins.match ".*cache-pin.*" name != null) (builtins.attrNames (builtins.readDir self));
        if a.value == 2 && b.value == 2 then "/nix/store/combined-app"
        else "/nix/store/independent-app"; }}; }};
    }}"#
        ),
    )
    .unwrap();
    commit(&consumer);
    command(&consumer, "nix", &["flake", "lock"]);
    // Exercise both staged and unstaged user data in the failed apply fixture.
    command(&consumer, "git", &["add", "flake.lock"]);
    fs::write(consumer.join("user-untracked"), "preserved").unwrap();
    fs::create_dir_all(consumer.join("vendor/unrelated")).unwrap();
    fs::write(
        consumer.join("vendor/unrelated/README"),
        "submodule content",
    )
    .unwrap();
    command(
        &consumer,
        "git",
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{old},vendor/unrelated"),
        ],
    );
    let before_source = fs::read(consumer.join("flake.nix")).unwrap();
    let before_lock = fs::read(consumer.join("flake.lock")).unwrap();
    let before_index = fs::read(consumer.join(".git/index")).unwrap();
    let cache = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/independent.narinfo"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&cache)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/combined.narinfo"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&cache)
        .await;
    let configs: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|input| {
            PinConfig::from_json(
                &serde_json::json!({
                    "name": input, "packages": ["app"], "inputName": input,
                    "attrPrefix": "", "pythonPackages": null, "caches": [cache.uri()],
                    "hydraJobset": "unused/jobs", "hydraUrl": "http://unused",
                    "hydraJobPattern": "{jobset}/{pkg}", "hydraRevInput": "flake",
                    "depth": 2, "branch": "main", "flakeRef": reference,
                    "flakeOutput": "packages", "failFast": false, "arch": "x86_64-linux",
                    "consumerFlakeRef": format!("git+file://{}", consumer.display()),
                    "consumerTargets": { "app": "inputs.app" },
                    "versionConstraints": { "app": { "target": ">= 1.0.0" } }
                })
                .to_string(),
            )
            .unwrap()
        })
        .collect();
    let ext = Arc::new(FixtureCommands(source.clone()));
    for cfg in &configs {
        let check = narinfo::verify_current(&reqwest::Client::new(), cfg, &old, &ext).await;
        assert!(
            check.all_cached,
            "current output named inputs.app: {:?}",
            check.results
        );
        let check = narinfo::verify_required_at_rev(&reqwest::Client::new(), cfg, &new, &ext).await;
        assert!(check.all_cached, "{:?}", check.results);
    }
    let mut source_cfg = configs[0].clone();
    source_cfg.consumer_flake_ref = None;
    source_cfg.consumer_targets.clear();
    source_cfg.packages = vec!["hello".into()];
    source_cfg.version_constraints.clear();
    let source_checks = nix_cache_pin_lib::final_graph::verify(
        &[(source_cfg, old.clone())],
        &format!("git+file://{}", consumer.display()),
        &ext,
    )
    .await;
    assert!(
        source_checks[0].all_cached,
        "explicit locked input graph: {:?}",
        source_checks[0].results
    );
    let groups = group_configs(configs.clone()).unwrap();
    let successes = configs
        .into_iter()
        .map(|cfg| (cfg, new.clone()))
        .collect::<Vec<_>>();
    std::env::set_current_dir(&consumer).unwrap();
    let result =
        transaction::apply_with_evidence(&groups, &successes, false, false, false, &ext).await;
    let error = result.unwrap_err();
    let Error::FinalGraph {
        evidence,
        candidate_lock,
    } = error
    else {
        panic!("expected final-graph refusal: {error}")
    };
    assert!(evidence.iter().all(|e| !e.all_cached));
    let combined_lock: serde_json::Value = serde_json::from_str(&candidate_lock).unwrap();
    assert!(combined_lock["nodes"]
        .as_object()
        .unwrap()
        .values()
        .filter_map(|node| node["locked"]["rev"].as_str())
        .all(|revision| revision == new));
    assert!(evidence
        .iter()
        .all(|e| e.results[0].store_path.as_deref() == Some("/nix/store/combined-app")));
    assert_eq!(fs::read(consumer.join("flake.nix")).unwrap(), before_source);
    assert_eq!(fs::read(consumer.join("flake.lock")).unwrap(), before_lock);
    assert_eq!(fs::read(consumer.join(".git/index")).unwrap(), before_index);
    assert_eq!(
        fs::read_to_string(consumer.join("user-untracked")).unwrap(),
        "preserved"
    );
    assert!(!consumer.join("cache-pin.lock.json").exists());
    assert!(fs::read_dir(&consumer).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("cache-pin-")));
    std::env::set_current_dir(directory.path()).unwrap();
}
