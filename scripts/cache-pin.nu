#!/usr/bin/env nu

# nix-cache-pin: Pin a flake input to a revision where all specified
# packages have binary cache hits.
#
# Strategy: query Hydra first (fast), then verify any non-Hydra packages
# via narinfo lookups against configured binary caches.

# --- Flake ref helpers ---

# Append a revision to a flake reference based on its scheme.
# github:/gitlab:/sourcehut: use /rev, git+* use ?rev=rev
def append-rev [flake_ref: string, rev: string]: nothing -> string {
    if ($flake_ref | str starts-with "git+") {
        $"($flake_ref)?rev=($rev)"
    } else {
        # github:, gitlab:, sourcehut:, path-like, etc.
        $"($flake_ref)/($rev)"
    }
}

# Build a regex to match a flake ref with any revision in flake.nix
def flake-ref-rev-pattern [flake_ref: string]: nothing -> string {
    # Escape regex special chars in the flake ref
    let escaped = ($flake_ref | str replace --all --regex '[.+?^${}()|\\[\\]/]' '\$0')
    # Note: use string concat to avoid nushell interpreting (?P<rev>...) as interpolation
    let rev_group = '(?P<rev>[0-9a-f]+)'
    if ($flake_ref | str starts-with "git+") {
        [$escaped '\?rev=' $rev_group] | str join
    } else {
        [$escaped '/' $rev_group] | str join
    }
}

# Extract GitHub owner/repo from a github: flake ref, or null
def extract-github-repo [flake_ref: string]: nothing -> any {
    if ($flake_ref | str starts-with "github:") {
        $flake_ref | str replace "github:" ""
    } else {
        null
    }
}

# --- Hydra ---

def build-hydra-job-url [cfg: record, pkg: string]: nothing -> string {
    let pattern = ($cfg.hydraJobPattern
        | str replace --all "{jobset}" $cfg.hydraJobset
        | str replace --all "{fullAttrPrefix}" $cfg.fullAttrPrefix
        | str replace --all "{attrPrefix}" $cfg.attrPrefix
        | str replace --all "{arch}" $cfg.arch
        | str replace --all "{pkg}" $pkg)
    $"($cfg.hydraUrl)/job/($pattern)/latest-finished"
}

def query-hydra-build [cfg: record, pkg: string]: nothing -> record {
    let job_url = (build-hydra-job-url $cfg $pkg)
    try {
        let build = (http get --headers [Accept application/json] $job_url)
        let store_path = try { $build.buildoutputs.out.path } catch { null }
        { package: $pkg, evals: $build.jobsetevals, status: "hydra", store_path: $store_path }
    } catch {
        { package: $pkg, evals: [], status: "not-on-hydra", store_path: null }
    }
}

# Extract revision from a Hydra evaluation based on hydraRevInput config
def extract-eval-rev [cfg: record, eval_data: record]: nothing -> any {
    if $cfg.hydraRevInput == "flake" {
        # Parse rev from the eval's flake URI (e.g., github:owner/repo/<rev>?...)
        let matches = ($eval_data.flake
            | parse --regex '(?P<rev>[0-9a-f]{40})')
        if ($matches | is-empty) {
            null
        } else {
            $matches | get rev | first
        }
    } else {
        # Look up named input in jobsetevalinputs
        try {
            $eval_data.jobsetevalinputs | get $cfg.hydraRevInput | get revision
        } catch {
            null
        }
    }
}

# --- Narinfo ---

def eval-store-path [flake_ref: string, rev: string, arch: string, flake_output: string, attr_prefix: string, pkg: string]: nothing -> string {
    let attr = if ($attr_prefix | is-empty) { $pkg } else { $"($attr_prefix).($pkg)" }
    let ref = $"(append-rev $flake_ref $rev)#($flake_output).($arch).($attr).outPath"
    with-env { NIXPKGS_ALLOW_UNFREE: "1" } {
        let result = (^nix eval --impure --raw $ref | complete)
        if $result.exit_code == 0 {
            $result.stdout | str trim
        } else {
            $result.stderr
        }
    }
}

def check-narinfo [store_path: string, caches: list<string>]: nothing -> bool {
    let hash = ($store_path | path basename | split row "-" | first)
    $caches | any { |cache|
        let url = $"($cache)/($hash).narinfo"
        try {
            http head $url | ignore
            true
        } catch {
            false
        }
    }
}

def verify-narinfo-at-rev [cfg: record, rev: string, packages: list<string>]: nothing -> record {
    let fail_fast = ($cfg.failFast? | default false)
    let flake_output = ($cfg.flakeOutput? | default "legacyPackages")
    let results = if $fail_fast {
        mut res = []
        for pkg in $packages {
            let store_path = (eval-store-path $cfg.flakeRef $rev $cfg.arch $flake_output $cfg.fullAttrPrefix $pkg)
            if ($store_path | str starts-with "/nix/store/") {
                let cached = (check-narinfo $store_path $cfg.caches)
                $res = ($res | append { package: $pkg, cached: $cached, store_path: $store_path, error: null })
                if not $cached {
                    print $"(ansi red)  Fail-fast: ($pkg) not cached, aborting.(ansi reset)"
                    break
                }
            } else {
                print $"(ansi yellow)  Warning: nix eval failed for ($pkg): ($store_path | str substring 0..200)(ansi reset)"
                $res = ($res | append { package: $pkg, cached: false, store_path: null, error: $store_path })
                print $"(ansi red)  Fail-fast: ($pkg) eval failed, aborting.(ansi reset)"
                break
            }
        }
        $res
    } else {
        $packages | par-each { |pkg|
            let store_path = (eval-store-path $cfg.flakeRef $rev $cfg.arch $flake_output $cfg.fullAttrPrefix $pkg)
            if ($store_path | str starts-with "/nix/store/") {
                let cached = (check-narinfo $store_path $cfg.caches)
                { package: $pkg, cached: $cached, store_path: $store_path, error: null }
            } else {
                print $"(ansi yellow)  Warning: nix eval failed for ($pkg): ($store_path | str substring 0..200)(ansi reset)"
                { package: $pkg, cached: false, store_path: null, error: $store_path }
            }
        }
    }
    let all_cached = ($results | where cached == false | is-empty)
    { rev: $rev, all_cached: $all_cached, results: $results }
}

# --- Narinfo-only scan (fallback when nothing is on Hydra) ---

def narinfo-scan [cfg: record] {
    let github_repo = (extract-github-repo $cfg.flakeRef)
    if ($github_repo == null) {
        print $"(ansi red)Narinfo scan requires a github: flake ref for commit listing.(ansi reset)"
        print $"  flakeRef: ($cfg.flakeRef)"
        print $"  Consider using a Hydra instance or switching to a github: ref."
        return null
    }

    print $"(ansi cyan)Fetching recent ($cfg.branch) commits...(ansi reset)"
    let commits = try {
        gh api $"repos/($github_repo)/commits?sha=($cfg.branch)&per_page=($cfg.depth)"
            | from json
            | get sha
    } catch { |err|
        print $"(ansi red)Failed to fetch commits from GitHub: ($err.msg)(ansi reset)"
        print "  Ensure 'gh' is authenticated and the repository exists."
        return null
    }

    print $"(ansi cyan)Checking ($commits | length) revisions for ($cfg.fullAttrPrefix) cache hits...(ansi reset)\n"

    mut target_rev = ""
    mut target_results = []

    for rev in $commits {
        let short = ($rev | str substring 0..12)
        print $"Checking rev ($short)..."
        let check = (verify-narinfo-at-rev $cfg $rev $cfg.packages)

        let cached_count = ($check.results | where cached == true | length)
        let total = ($check.results | length)
        print $"  ($cached_count)/($total) packages cached"

        if $check.all_cached {
            print $"  (ansi green)All packages cached!(ansi reset)"
            $target_rev = $rev
            $target_results = $check.results
            break
        } else {
            let misses = ($check.results | where cached == false | get package | str join ", ")
            print $"  (ansi yellow)Missing: ($misses)(ansi reset)"
            if $cfg.failFast {
                print $"\n(ansi red)Fail-fast: package(s) not cached at rev ($short), aborting.(ansi reset)"
                exit 1
            }
        }
    }

    if ($target_rev | is-empty) {
        print $"\n(ansi red)No revision found with all packages cached in the last ($cfg.depth) commits.(ansi reset)"
        return null
    }

    # Show per-package status
    print $"\n(ansi cyan)Package status \(rev ($target_rev | str substring 0..12)\):(ansi reset)"
    $target_results | each { |r|
        let marker = if $r.cached { $"(ansi green)cached(ansi reset)" } else { $"(ansi red)miss(ansi reset)" }
        print $"  ($cfg.fullAttrPrefix).($r.package): ($marker)"
    }

    $target_rev
}

# --- Unified: Hydra first, narinfo fallback ---

def find-target-rev [cfg: record] {
    # Step 1: Try Hydra for all packages
    print $"(ansi cyan)Querying ($cfg.hydraUrl) for ($cfg.fullAttrPrefix) builds...(ansi reset)"
    let results = $cfg.packages | par-each { |pkg|
        query-hydra-build $cfg $pkg
    }

    let on_hydra = $results | where status == "hydra"
    let not_on_hydra = $results | where status == "not-on-hydra"

    if ($on_hydra | is-not-empty) {
        print $"(ansi green)  On Hydra:(ansi reset)"
        $on_hydra | each { |r| print $"    ($r.package)" }
    }
    if ($not_on_hydra | is-not-empty) {
        print $"(ansi yellow)  Not on Hydra:(ansi reset)"
        $not_on_hydra | each { |r| print $"    ($r.package)" }
    }

    # If nothing is on Hydra, fall back to pure narinfo scan
    if ($on_hydra | is-empty) {
        print $"\n(ansi yellow)No packages found on Hydra, falling back to narinfo scan...(ansi reset)\n"
        return (narinfo-scan $cfg)
    }

    # Step 2: Find common evals from Hydra results (sorted newest first)
    let all_evals = $on_hydra | get evals
    let common_evals = $all_evals
        | skip 1
        | reduce --fold ($all_evals | first) { |el, acc|
            $acc | where { |e| $e in $el }
        }
        | sort --reverse

    let candidate_evals = if ($common_evals | is-not-empty) {
        $common_evals
    } else {
        print $"\n(ansi yellow)Warning: no single eval has all Hydra packages — using bottleneck(ansi reset)"
        let bottleneck = ($on_hydra
            | each { |r| { package: $r.package, eval_id: ($r.evals | first) } }
            | sort-by eval_id
            | first)
        print $"  Bottleneck: ($bottleneck.package) at eval ($bottleneck.eval_id)"
        [$bottleneck.eval_id]
    }

    # Step 3: Try each candidate eval, verify ALL packages via narinfo.
    # Hydra "cached" status doesn't guarantee outputs are in the binary cache.
    mut target_rev = null
    for eval_id in $candidate_evals {
        # Show Hydra package status
        print $"\n(ansi cyan)Hydra status \(eval ($eval_id)\):(ansi reset)"
        $on_hydra | each { |r|
            let in_eval = $eval_id in $r.evals
            let marker = if $in_eval { $"(ansi green)cached(ansi reset)" } else {
                let latest = $r.evals | first
                $"(ansi red)miss(ansi reset) \(latest: eval ($latest)\)"
            }
            print $"  ($cfg.fullAttrPrefix).($r.package): ($marker)"
        }

        # Get rev from this eval
        let eval_url = $"($cfg.hydraUrl)/eval/($eval_id)"
        let eval_data = try {
            http get --headers [Accept application/json] $eval_url
        } catch { |err|
            print $"(ansi red)Failed to fetch eval ($eval_id) from Hydra: ($err.msg)(ansi reset)"
            continue
        }
        let rev = (extract-eval-rev $cfg $eval_data)

        if ($rev == null) {
            print $"(ansi red)Failed to extract revision from eval ($eval_id)(ansi reset)"
            continue
        }

        # Verify all packages via nix eval + narinfo at the target rev.
        # We cannot trust Hydra's store_path because Hydra evaluates via
        # release.nix while flakes use legacyPackages — these produce
        # different derivation hashes even at the same revision.
        # Hydra is only used to find candidate evals/revisions.
        print $"\n(ansi cyan)Verifying narinfo at rev ($rev | str substring 0..12)...(ansi reset)"

        let check = (verify-narinfo-at-rev $cfg $rev $cfg.packages)
        let all_results = $check.results

        $all_results | each { |r|
            let marker = if $r.cached { $"(ansi green)cached(ansi reset)" } else { $"(ansi red)miss(ansi reset)" }
            print $"  ($cfg.fullAttrPrefix).($r.package): ($marker)"
        }

        let all_cached = ($all_results | where cached == false | is-empty)
        if $all_cached {
            $target_rev = $rev
            break
        } else {
            let misses = ($all_results | where cached == false | get package | str join ", ")
            print $"(ansi yellow)  Missing: ($misses) — trying older eval...(ansi reset)"
            if $cfg.failFast {
                print $"\n(ansi red)Fail-fast: aborting.(ansi reset)"
                exit 1
            }
        }
    }

    if ($target_rev == null) {
        print $"\n(ansi red)No Hydra eval has all packages in the binary cache.(ansi reset)"
        print $"(ansi yellow)Falling back to narinfo scan...(ansi reset)\n"
        return (narinfo-scan $cfg)
    }

    $target_rev
}

# --- Main ---

def main [
    --config (-c): string  # Path to JSON config file
    --dry-run (-n)         # Don't actually update, just show what would change
    --no-lock              # Don't run `nix flake lock` after updating
    --fail-fast (-f)       # Exit immediately on first cache miss
] {
    if ($config | is-empty) {
        print $"(ansi red)Error: --config is required(ansi reset)"
        exit 1
    }

    let cfg = try {
        open $config
    } catch { |err|
        print $"(ansi red)Error: failed to read config file '($config)': ($err.msg)(ansi reset)"
        exit 1
    }

    # Merge --fail-fast CLI flag with config (CLI flag takes precedence)
    let cfg = if $fail_fast {
        $cfg | upsert failFast true
    } else {
        $cfg | upsert failFast ($cfg.failFast? | default false)
    }

    # Compute cache lookup prefix: when pythonPackages is set, Hydra builds
    # under pythonXXXPackages.* directly (NOT under pkgsRocm.pythonXXXPackages.*),
    # so cache lookups use only pythonPackages as prefix, dropping attrPrefix.
    # attrPrefix is only for local validation.
    let cfg = if ($cfg.flakeOutput? | is-empty) {
        $cfg | upsert flakeOutput "legacyPackages"
    } else {
        $cfg
    }

    let cfg = if ($cfg.pythonPackages? | is-not-empty) {
        $cfg | upsert fullAttrPrefix $cfg.pythonPackages
    } else {
        $cfg | upsert fullAttrPrefix $cfg.attrPrefix
    }

    print $"(ansi cyan_bold)nix-cache-pin: ($cfg.name)(ansi reset)"
    print $"  input:       ($cfg.inputName)"
    print $"  flake ref:   ($cfg.flakeRef)"
    print $"  hydra:       ($cfg.hydraUrl)"
    print $"  attr prefix: ($cfg.fullAttrPrefix)"
    print $"  arch:        ($cfg.arch)"
    print $"  packages:    ($cfg.packages | str join ', ')\n"

    let target_rev = (find-target-rev $cfg)

    if ($target_rev == null) {
        exit 1
    }

    # Compare with current pin in flake.nix
    if not ("flake.nix" | path exists) {
        print $"(ansi red)Error: flake.nix not found in current directory(ansi reset)"
        exit 1
    }

    let rev_pattern = (flake-ref-rev-pattern $cfg.flakeRef)
    let input_pattern = $'($cfg.inputName)\.url = "($rev_pattern)"'
    let matches = (open --raw flake.nix | parse --regex $input_pattern)

    if ($matches | is-empty) {
        print $"(ansi red)Error: could not find ($cfg.inputName).url pattern in flake.nix(ansi reset)"
        print $"  Expected: ($cfg.inputName).url = \"($cfg.flakeRef)/<rev>\""
        exit 1
    }

    let current_rev = ($matches | get rev | first)

    print $"\n  Current pin: ($current_rev)"
    print $"  Target rev:  ($target_rev)"

    if $current_rev == $target_rev {
        print $"(ansi green)Already up to date!(ansi reset)"
        return
    }

    print $"\n(ansi yellow)New revision available(ansi reset)"

    if $dry_run {
        print $"(ansi magenta)Would update ($cfg.inputName) to ($target_rev) \(dry run\)(ansi reset)"
        return
    }

    # Update the pinned rev in flake.nix
    print "Updating flake.nix..."
    let old_url = (append-rev $cfg.flakeRef $current_rev)
    let new_url = (append-rev $cfg.flakeRef $target_rev)
    open --raw flake.nix | str replace $old_url $new_url | save -f flake.nix
    print $"(ansi green)Updated ($cfg.inputName) to ($target_rev)(ansi reset)"

    # Update the flake lock file
    if not $no_lock {
        print $"Running nix flake lock --update-input ($cfg.inputName)..."
        try {
            ^nix flake lock --update-input $cfg.inputName
            print $"(ansi green)Lock file updated.(ansi reset)"
        } catch { |err|
            print $"(ansi red)Warning: failed to update lock file: ($err.msg)(ansi reset)"
            print "  You may need to run 'nix flake lock' manually."
            exit 1
        }
    }
}
