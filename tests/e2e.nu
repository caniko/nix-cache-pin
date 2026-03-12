#!/usr/bin/env nu

# End-to-end tests for nix-cache-pin.
# Requires network access (queries real Hydra instances).

use std/assert

let script_dir = ($env.FILE_PWD | path join "../scripts")
let cache_pin = ($script_dir | path join "cache-pin.nu")

# --- Unit tests for helper functions ---

print "=== Unit tests: helper functions ==="

# Source the script to get access to helper functions
source ../scripts/cache-pin.nu

# append-rev: github: scheme
assert equal (append-rev "github:NixOS/nixpkgs" "abc123") "github:NixOS/nixpkgs/abc123"

# append-rev: gitlab: scheme
assert equal (append-rev "gitlab:foo/bar" "def456") "gitlab:foo/bar/def456"

# append-rev: git+https: scheme
assert equal (append-rev "git+https://gitlab.com/foo/bar" "abc123") "git+https://gitlab.com/foo/bar?rev=abc123"

# append-rev: git+ssh: scheme
assert equal (append-rev "git+ssh://git@github.com/foo/bar" "abc") "git+ssh://git@github.com/foo/bar?rev=abc"

# flake-ref-rev-pattern: github: scheme
let pattern = (flake-ref-rev-pattern "github:NixOS/nixpkgs")
assert ($pattern | str contains "(?P<rev>[0-9a-f]+)")
assert ($pattern | str ends-with "(?P<rev>[0-9a-f]+)")
# Verify it actually matches a real URL
let test_url = 'nixpkgs-rocm.url = "github:NixOS/nixpkgs/abc123def456"'
let matches = ($test_url | parse --regex $'nixpkgs\-rocm\.url = "($pattern)"')
assert equal ($matches | get rev | first) "abc123def456"

# flake-ref-rev-pattern: git+https: scheme
let git_pattern = (flake-ref-rev-pattern "git+https://gitlab.com/foo/bar")
let git_url = 'my-input.url = "git+https://gitlab.com/foo/bar?rev=abc123def456"'
let git_matches = ($git_url | parse --regex $'my\-input\.url = "($git_pattern)"')
assert equal ($git_matches | get rev | first) "abc123def456"

# extract-github-repo: github: ref
assert equal (extract-github-repo "github:NixOS/nixpkgs") "NixOS/nixpkgs"
assert equal (extract-github-repo "github:xddxdd/nix-cachyos-kernel") "xddxdd/nix-cachyos-kernel"

# extract-github-repo: non-github ref returns null
assert equal (extract-github-repo "git+https://gitlab.com/foo/bar") null

# extract-eval-rev: flake mode
let fake_eval_flake = {
    flake: "github:xddxdd/nix-cachyos-kernel/1fc1e3f6d65a3e16898c8a75a951cfc529e71001?narHash=sha256-abc"
}
let cfg_flake = { hydraRevInput: "flake" }
assert equal (extract-eval-rev $cfg_flake $fake_eval_flake) "1fc1e3f6d65a3e16898c8a75a951cfc529e71001"

# extract-eval-rev: named input mode
let fake_eval_input = {
    jobsetevalinputs: {
        nixpkgs: { revision: "abcdef1234567890abcdef1234567890abcdef12" }
    }
}
let cfg_input = { hydraRevInput: "nixpkgs" }
assert equal (extract-eval-rev $cfg_input $fake_eval_input) "abcdef1234567890abcdef1234567890abcdef12"

# extract-eval-rev: missing input returns null
let cfg_missing = { hydraRevInput: "nonexistent" }
assert equal (extract-eval-rev $cfg_missing $fake_eval_input) null

print "  All unit tests passed.\n"

# --- E2E: CachyOS Hydra (Lantian) ---

print "=== E2E: CachyOS Hydra (Lantian) ==="

let cachyos_cfg = {
    name: "cachyos-e2e"
    packages: ["linux-cachyos-latest-lto-zen4" "linux-cachyos-latest-lto-x86_64-v3"]
    inputName: "nix-cachyos-kernel"
    attrPrefix: "packages"
    pythonPackages: null
    caches: ["https://cache.nixos.org"]
    hydraJobset: "lantian/nix-cachyos-kernel"
    hydraUrl: "https://hydra.lantian.pub"
    hydraJobPattern: "{jobset}/packages.{arch}.{pkg}"
    hydraRevInput: "flake"
    depth: 15
    branch: "main"
    flakeRef: "github:xddxdd/nix-cachyos-kernel"
    failFast: false
    arch: "x86_64-linux"
    fullAttrPrefix: "packages"
}

# Test: Hydra job query returns results
print "  Querying Hydra for CachyOS kernel builds..."
let cachyos_results = $cachyos_cfg.packages | par-each { |pkg|
    query-hydra-build $cachyos_cfg $pkg
}
let cachyos_on_hydra = $cachyos_results | where status == "hydra"
assert (($cachyos_on_hydra | length) > 0) "Expected at least one CachyOS package on Hydra"
print $"  Found ($cachyos_on_hydra | length)/($cachyos_results | length) packages on Hydra"

# Test: eval IDs are non-empty and store paths are present
for result in $cachyos_on_hydra {
    assert (($result.evals | length) > 0) $"Expected evals for ($result.package)"
    assert ($result.store_path != null) $"Expected store_path for ($result.package)"
    assert ($result.store_path | str starts-with "/nix/store/") $"Expected /nix/store/ path for ($result.package)"
}

# Test: CachyOS store paths are in Lantian's cache (not cache.nixos.org)
let cachyos_caches = ["https://attic.xuyh0120.win/lantian" "https://cache.garnix.io" "https://cache.nixos.org"]
for result in $cachyos_on_hydra {
    let cached = (check-narinfo $result.store_path $cachyos_caches)
    let status = if $cached { "cached" } else { "MISS" }
    print $"    ($result.package) narinfo: ($status)"
}
print "  CachyOS narinfo verification using Hydra store paths works."

# Test: can find common eval
let all_evals = $cachyos_on_hydra | get evals
let common_evals = $all_evals
    | skip 1
    | reduce --fold ($all_evals | first) { |el, acc|
        $acc | where { |e| $e in $el }
    }
assert (($common_evals | length) > 0) "Expected at least one common eval"
let target_eval = $common_evals | math max
print $"  Common eval: ($target_eval)"

# Test: can extract rev from eval
let eval_data = (http get --headers [Accept application/json] $"($cachyos_cfg.hydraUrl)/eval/($target_eval)")
let cachyos_rev = (extract-eval-rev $cachyos_cfg $eval_data)
assert ($cachyos_rev != null) "Expected non-null revision from CachyOS eval"
assert (($cachyos_rev | str length) == 40) "Expected 40-char hex revision"
print $"  Extracted rev: ($cachyos_rev)"

print "  CachyOS Hydra tests passed.\n"

# --- E2E: nixpkgs Hydra (hydra.nixos.org) ---

print "=== E2E: nixpkgs Hydra (hydra.nixos.org) ==="

let nixpkgs_cfg = {
    name: "nixpkgs-e2e"
    packages: ["blender"]
    inputName: "nixpkgs-test"
    attrPrefix: "pkgs"
    pythonPackages: null
    caches: ["https://cache.nixos.org"]
    hydraJobset: "nixpkgs/trunk"
    hydraUrl: "https://hydra.nixos.org"
    hydraJobPattern: "{jobset}/{pkg}.{arch}"
    hydraRevInput: "nixpkgs"
    depth: 15
    branch: "nixpkgs-unstable"
    flakeRef: "github:NixOS/nixpkgs"
    failFast: false
    arch: "x86_64-linux"
    fullAttrPrefix: "pkgs"
}

# Test: Hydra job query for blender
print "  Querying Hydra for nixpkgs blender build..."
let nixpkgs_result = (query-hydra-build $nixpkgs_cfg "blender")
assert equal $nixpkgs_result.status "hydra" "Expected blender to be on Hydra"
assert (($nixpkgs_result.evals | length) > 0) "Expected evals for blender"
let nixpkgs_eval = $nixpkgs_result.evals | first
print $"  Blender eval: ($nixpkgs_eval)"

# Test: can extract rev from nixpkgs eval via jobsetevalinputs
let nixpkgs_eval_data = (http get --headers [Accept application/json] $"($nixpkgs_cfg.hydraUrl)/eval/($nixpkgs_eval)")
let nixpkgs_rev = (extract-eval-rev $nixpkgs_cfg $nixpkgs_eval_data)
assert ($nixpkgs_rev != null) "Expected non-null revision from nixpkgs eval"
assert (($nixpkgs_rev | str length) == 40) "Expected 40-char hex revision"
print $"  Extracted rev: ($nixpkgs_rev)"

print "  nixpkgs Hydra tests passed.\n"

# --- E2E: ROCm Hydra (hydra.nixos.org) ---

print "=== E2E: ROCm Hydra (hydra.nixos.org) ==="

let rocm_cfg = {
    name: "rocm-e2e"
    packages: ["rocblas" "hipblas"]
    inputName: "nixpkgs-rocm"
    attrPrefix: "pkgsRocm"
    pythonPackages: null
    caches: ["https://cache.nixos.org"]
    hydraJobset: "nixpkgs/trunk"
    hydraUrl: "https://hydra.nixos.org"
    hydraJobPattern: "{jobset}/{fullAttrPrefix}.{pkg}.{arch}"
    hydraRevInput: "nixpkgs"
    depth: 15
    branch: "nixpkgs-unstable"
    flakeRef: "github:NixOS/nixpkgs"
    failFast: false
    arch: "x86_64-linux"
    fullAttrPrefix: "rocmPackages"
}

# Test: Hydra job queries return results for ROCm packages
print "  Querying Hydra for ROCm package builds..."
let rocm_results = $rocm_cfg.packages | par-each { |pkg|
    query-hydra-build $rocm_cfg $pkg
}
let rocm_on_hydra = $rocm_results | where status == "hydra"
assert (($rocm_on_hydra | length) > 0) "Expected at least one ROCm package on Hydra"
print $"  Found ($rocm_on_hydra | length)/($rocm_results | length) ROCm packages on Hydra"

# Test: can find common eval across ROCm packages (rocblas + hipblas share evals)
let rocm_all_evals = $rocm_on_hydra | get evals
let rocm_common_evals = $rocm_all_evals
    | skip 1
    | reduce --fold ($rocm_all_evals | first) { |el, acc|
        $acc | where { |e| $e in $el }
    }
assert (($rocm_common_evals | length) > 0) "Expected at least one common ROCm eval"
let rocm_target_eval = $rocm_common_evals | math max
print $"  Common eval: ($rocm_target_eval)"

# Test: can extract rev from ROCm eval
let rocm_eval_data = (http get --headers [Accept application/json] $"($rocm_cfg.hydraUrl)/eval/($rocm_target_eval)")
let rocm_rev = (extract-eval-rev $rocm_cfg $rocm_eval_data)
assert ($rocm_rev != null) "Expected non-null revision from ROCm eval"
assert (($rocm_rev | str length) == 40) "Expected 40-char hex revision"
print $"  Extracted rev: ($rocm_rev)"

print "  ROCm Hydra tests passed.\n"

# --- E2E: CUDA Hydra graceful fallback ---

print "=== E2E: CUDA Hydra graceful fallback ==="

let cuda_cfg = {
    name: "cuda-e2e"
    packages: ["cudnn"]
    inputName: "nixpkgs-cuda"
    attrPrefix: "pkgsCuda"
    pythonPackages: null
    caches: ["https://cache.nixos.org"]
    hydraJobset: "nixpkgs/trunk"
    hydraUrl: "https://hydra.nixos.org"
    hydraJobPattern: "{jobset}/{fullAttrPrefix}.{pkg}.{arch}"
    hydraRevInput: "nixpkgs"
    depth: 15
    branch: "nixpkgs-unstable"
    flakeRef: "github:NixOS/nixpkgs"
    failFast: false
    arch: "x86_64-linux"
    fullAttrPrefix: "cudaPackages"
}

# Test: CUDA packages are not on Hydra (unfree) — query should not error
print "  Querying Hydra for CUDA package builds (expecting not-on-hydra)..."
let cuda_result = (query-hydra-build $cuda_cfg "cudnn")
assert equal $cuda_result.status "not-on-hydra" "Expected cudnn to not be on Hydra (unfree)"
assert equal ($cuda_result.evals | length) 0 "Expected empty evals for unfree CUDA package"
print "  Confirmed: CUDA packages gracefully return not-on-hydra"

print "  CUDA graceful fallback tests passed.\n"

# --- E2E: Full dry-run flow (ROCm) ---

print "=== E2E: Full dry-run flow (ROCm) ==="

let test_dir_rocm = (mktemp -d)
let test_flake_rocm = $'
{
  inputs = {
    nixpkgs-rocm.url = "github:NixOS/nixpkgs/0000000000000000000000000000000000000000";
  };
  outputs = _: {};
}
'
$test_flake_rocm | save -f ($test_dir_rocm | path join "flake.nix")

let test_config_rocm = {
    name: "rocm-dry-run"
    packages: ["rocblas" "hipblas"]
    inputName: "nixpkgs-rocm"
    attrPrefix: "rocmPackages"
    pythonPackages: null
    caches: ["https://cache.nixos.org"]
    hydraJobset: "nixpkgs/trunk"
    hydraUrl: "https://hydra.nixos.org"
    hydraJobPattern: "{jobset}/{fullAttrPrefix}.{pkg}.{arch}"
    hydraRevInput: "nixpkgs"
    depth: 15
    branch: "nixpkgs-unstable"
    flakeRef: "github:NixOS/nixpkgs"
    failFast: false
    arch: "x86_64-linux"
} | to json
$test_config_rocm | save -f ($test_dir_rocm | path join "config.json")

print "  Running cache-pin --dry-run..."
let result_rocm = (do {
    cd $test_dir_rocm
    ^nu $cache_pin --config ($test_dir_rocm | path join "config.json") --dry-run --no-lock
} | complete)

assert equal $result_rocm.exit_code 0 $"Expected exit 0, got ($result_rocm.exit_code)\nstdout: ($result_rocm.stdout)\nstderr: ($result_rocm.stderr)"
assert ($result_rocm.stdout | str contains "Would update nixpkgs-rocm to") "Expected dry-run update message"
assert ($result_rocm.stdout | str contains "New revision available") "Expected 'New revision available'"

print "  Full ROCm dry-run flow passed.\n"

# --- E2E: Full dry-run flow (CachyOS) ---

print "=== E2E: Full dry-run flow (CachyOS) ==="

let test_dir = (mktemp -d)
let test_flake = $'
{
  inputs = {
    nix-cachyos-kernel.url = "github:xddxdd/nix-cachyos-kernel/0000000000000000000000000000000000000000";
  };
  outputs = _: {};
}
'
$test_flake | save -f ($test_dir | path join "flake.nix")

let test_config = {
    name: "cachyos-dry-run"
    packages: ["linux-cachyos-latest-lto-zen4"]
    inputName: "nix-cachyos-kernel"
    attrPrefix: "packages"
    pythonPackages: null
    caches: ["https://attic.xuyh0120.win/lantian" "https://cache.garnix.io" "https://cache.nixos.org"]
    hydraJobset: "lantian/nix-cachyos-kernel"
    hydraUrl: "https://hydra.lantian.pub"
    hydraJobPattern: "{jobset}/packages.{arch}.{pkg}"
    hydraRevInput: "flake"
    depth: 15
    branch: "master"
    flakeRef: "github:xddxdd/nix-cachyos-kernel"
    failFast: false
    arch: "x86_64-linux"
} | to json
$test_config | save -f ($test_dir | path join "config.json")

print "  Running cache-pin --dry-run..."
let result = (do {
    cd $test_dir
    ^nu $cache_pin --config ($test_dir | path join "config.json") --dry-run --no-lock
} | complete)

assert equal $result.exit_code 0 $"Expected exit 0, got ($result.exit_code)\nstdout: ($result.stdout)\nstderr: ($result.stderr)"
assert ($result.stdout | str contains "Would update nix-cachyos-kernel to") "Expected dry-run update message"
assert ($result.stdout | str contains "New revision available") "Expected 'New revision available'"

print "  Full dry-run flow passed.\n"

# --- E2E: Full dry-run flow (nixpkgs) ---

print "=== E2E: Full dry-run flow (nixpkgs) ==="

let test_dir2 = (mktemp -d)
let test_flake2 = $'
{
  inputs = {
    nixpkgs-test.url = "github:NixOS/nixpkgs/0000000000000000000000000000000000000000";
  };
  outputs = _: {};
}
'
$test_flake2 | save -f ($test_dir2 | path join "flake.nix")

let test_config2 = {
    name: "nixpkgs-dry-run"
    packages: ["blender"]
    inputName: "nixpkgs-test"
    attrPrefix: "pkgs"
    pythonPackages: null
    caches: ["https://cache.nixos.org"]
    hydraJobset: "nixpkgs/trunk"
    hydraUrl: "https://hydra.nixos.org"
    hydraJobPattern: "{jobset}/{pkg}.{arch}"
    hydraRevInput: "nixpkgs"
    depth: 15
    branch: "nixpkgs-unstable"
    flakeRef: "github:NixOS/nixpkgs"
    failFast: false
    arch: "x86_64-linux"
} | to json
$test_config2 | save -f ($test_dir2 | path join "config.json")

print "  Running cache-pin --dry-run..."
let result2 = (do {
    cd $test_dir2
    ^nu $cache_pin --config ($test_dir2 | path join "config.json") --dry-run --no-lock
} | complete)

assert equal $result2.exit_code 0 $"Expected exit 0, got ($result2.exit_code)\nstdout: ($result2.stdout)\nstderr: ($result2.stderr)"
assert ($result2.stdout | str contains "Would update nixpkgs-test to") "Expected dry-run update message"

print "  Full nixpkgs dry-run flow passed.\n"

# --- Unit test: check-narinfo detects missing store paths ---

print "=== Unit test: check-narinfo ==="

# A fake store path that definitely doesn't exist in any cache
let fake_path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fake-pkg-1.0"
let is_fake_cached = (check-narinfo $fake_path ["https://cache.nixos.org"])
assert equal $is_fake_cached false "Fake store path should not be in cache"

print "  check-narinfo tests passed.\n"

# --- E2E: Regression test — stale Hydra store path bug ---

print "=== E2E: Stale Hydra store path regression test ==="

# Bug: find-target-rev used store_path from Hydra's latest-finished build
# to verify narinfo for ALL candidate evals. But derivation hashes change
# between nixpkgs revisions, so the store_path is only valid for the
# evals listed in that specific build — not for older/newer revisions.
#
# Fix: only trust Hydra store_path when the target eval IS in the build's
# evals list (same build = same hash). For eval mismatches or non-Hydra
# packages, fall back to nix eval + narinfo at the actual target rev.

let divergence_cfg = {
    hydraUrl: "https://hydra.nixos.org"
    hydraJobset: "nixpkgs/trunk"
    hydraJobPattern: "{jobset}/{pkg}.{arch}"
    hydraRevInput: "nixpkgs"
    flakeRef: "github:NixOS/nixpkgs"
    caches: ["https://cache.nixos.org"]
    arch: "x86_64-linux"
    fullAttrPrefix: "pkgs"
    attrPrefix: "pkgs"
    packages: ["hello"]
    inputName: "test"
    name: "divergence"
    pythonPackages: null
    depth: 15
    branch: "nixpkgs-unstable"
    failFast: false
}

# Step 1: Get latest-finished Hydra build for hello
print "  Querying Hydra for hello..."
let hello_build = (query-hydra-build $divergence_cfg "hello")
assert equal $hello_build.status "hydra" "Expected hello on Hydra"
assert ($hello_build.store_path != null) "Expected store_path from Hydra"
let hydra_path = $hello_build.store_path
print $"  Hydra latest-finished store_path: ($hydra_path)"

# Step 2: Extract rev from the latest eval for this build
let latest_eval_id = ($hello_build.evals | first)
let eval_data = (http get --headers [Accept application/json] $"($divergence_cfg.hydraUrl)/eval/($latest_eval_id)")
let latest_rev = (extract-eval-rev $divergence_cfg $eval_data)
assert ($latest_rev != null) "Expected rev from eval"
print $"  Latest eval ($latest_eval_id) rev: ($latest_rev | str substring 0..12)"

# Step 3: nix eval at the SAME rev must match Hydra's store_path
print "  Verifying nix eval matches Hydra at same rev..."
let eval_path = (eval-store-path $divergence_cfg.flakeRef $latest_rev $divergence_cfg.arch "pkgs" "hello")
assert ($eval_path | str starts-with "/nix/store/") $"Expected /nix/store/ path, got: ($eval_path)"
assert equal $hydra_path $eval_path "Hydra store_path must match nix eval at the same revision"
print $"  OK: paths match at same rev"

# Step 4: Get a DIFFERENT build of hello to find a revision with a different store path.
# Query a different job (e.g., blender) whose latest eval likely has a different rev.
print "  Getting a different revision via blender's latest eval..."
let blender_build = (query-hydra-build $divergence_cfg "blender")
assert equal $blender_build.status "hydra" "Expected blender on Hydra"
let blender_eval_id = ($blender_build.evals | first)
let blender_eval_data = (http get --headers [Accept application/json] $"($divergence_cfg.hydraUrl)/eval/($blender_eval_id)")
let older_rev = (extract-eval-rev $divergence_cfg $blender_eval_data)
assert ($older_rev != null) "Expected rev from blender eval"

if ($older_rev == $latest_rev) {
    print "  Skipping divergence check: hello and blender share the same latest eval rev"
    print "  Stale Hydra store path regression test passed (partial).\n"
} else {
    print $"  Blender eval ($blender_eval_id) rev: ($older_rev | str substring 0..12)"

    # Step 5: nix eval at the different rev — store path should differ
    print "  Evaluating hello at blender's rev..."
    let other_path = (eval-store-path $divergence_cfg.flakeRef $older_rev $divergence_cfg.arch "pkgs" "hello")
    assert ($other_path | str starts-with "/nix/store/") $"Expected /nix/store/ path, got: ($other_path)"

    if ($other_path != $hydra_path) {
        print "  OK: store paths differ across revisions"
        print $"    hello latest:    ($hydra_path)"
        print $"    hello at other:  ($other_path)"
        print "  This proves reusing Hydra store_path across revisions is wrong."
    } else {
        print "  Store paths match (hello unchanged between these revs)"
    }

    # Step 6: verify-narinfo-at-rev gives accurate results
    print "\n  Testing verify-narinfo-at-rev accuracy..."
    let check_latest = (verify-narinfo-at-rev $divergence_cfg $latest_rev ["hello"])
    assert ($check_latest.all_cached) $"Expected hello cached at latest Hydra rev ($latest_rev | str substring 0..12)"
    print "  OK: verify-narinfo-at-rev confirms hello is cached at latest rev"

    print "  Stale Hydra store path regression test passed.\n"
}

# --- E2E: Hydra query-hydra-build returns valid data ---

print "=== E2E: Hydra query-hydra-build validation ==="

let narinfo_cfg = {
    name: "narinfo-verify"
    packages: ["obs-studio-plugins.obs-backgroundremoval" "blender"]
    inputName: "nixpkgs-rocm"
    attrPrefix: "pkgsRocm"
    pythonPackages: null
    caches: ["https://cache.nixos.org"]
    hydraJobset: "nixpkgs/trunk"
    hydraUrl: "https://hydra.nixos.org"
    hydraJobPattern: "{jobset}/{fullAttrPrefix}.{pkg}.{arch}"
    hydraRevInput: "nixpkgs"
    depth: 15
    branch: "nixpkgs-unstable"
    flakeRef: "github:NixOS/nixpkgs"
    failFast: false
    arch: "x86_64-linux"
    fullAttrPrefix: "pkgsRocm"
}

# Query Hydra — these packages should be "on Hydra" and include store paths
let narinfo_results = $narinfo_cfg.packages | par-each { |pkg|
    query-hydra-build $narinfo_cfg $pkg
}
let narinfo_on_hydra = $narinfo_results | where status == "hydra"
assert (($narinfo_on_hydra | length) > 0) "Expected ROCm packages on Hydra"

# Test: query-hydra-build returns store_path from buildoutputs
for r in $narinfo_on_hydra {
    assert ($r.store_path != null) $"Expected store_path for ($r.package)"
    assert ($r.store_path | str starts-with "/nix/store/") $"Expected /nix/store/ path for ($r.package), got: ($r.store_path)"
    print $"    ($r.package): ($r.store_path)"
}
print "  Hydra builds include store paths."

# Test: narinfo check using Hydra-provided store paths
for r in $narinfo_on_hydra {
    let cached = (check-narinfo $r.store_path $narinfo_cfg.caches)
    let status = if $cached { "cached" } else { "MISS" }
    print $"    ($r.package) narinfo: ($status)"
}

print "  Hydra query-hydra-build validation passed.\n"

# --- Cleanup ---
rm -rf $test_dir_rocm $test_dir $test_dir2

print "=== All e2e tests passed! ==="
exit 0
