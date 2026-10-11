use sha2::{Digest, Sha256};
use std::{fs, path::Path};

fn hash_tree(path: &Path, hash: &mut Sha256) {
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for entry in entries {
        if entry.is_dir() {
            hash_tree(&entry, hash);
        } else {
            println!("cargo:rerun-if-changed={}", entry.display());
            hash.update(entry.to_string_lossy().as_bytes());
            hash.update([0]);
            hash.update(fs::read(entry).unwrap());
        }
    }
}

fn main() {
    let mut hash = Sha256::new();
    hash_tree(Path::new("src"), &mut hash);
    hash.update(fs::read("Cargo.toml").unwrap());
    hash.update(fs::read("build.rs").unwrap());
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=NIX_CACHE_PIN_SOURCE_SHA256={:x}",
        hash.finalize()
    );
    let revision = fs::read_to_string(".cargo_vcs_info.json")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["git"]["sha1"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "checkout; use sourceSha256 for exact identity".into());
    println!("cargo:rerun-if-changed=.cargo_vcs_info.json");
    println!("cargo:rustc-env=NIX_CACHE_PIN_SOURCE_REVISION={revision}");
}
