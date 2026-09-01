//! Derives the build identity at compile time.
//!
//! The authority is the environment: `cargo xtask build-set` exports `LEGEOS_BUILD_SET_ID` and
//! every package in the set is compiled against that one string, which is also the Debian version
//! (`cargo deb --deb-version`). That is what makes "the running build" and "the installed package"
//! comparable at all.
//!
//! Without that variable — an ordinary `cargo build` — the identity is derived from git, and it is
//! derived **deterministically**: the epoch is the HEAD commit's own timestamp, never "now". A
//! build script's output is cached, so a wall-clock epoch would bake one moment into the binary
//! and then keep claiming it for weeks. The commit timestamp is stable for as long as the thing it
//! describes is, which is exactly the property an identity needs.

use std::{path::Path, process::Command};

fn main() {
    // The environment authority, and the two git facts the fallback needs. Without these
    // rerun-if lines cargo would happily hand back yesterday's identity after a commit.
    println!("cargo:rerun-if-env-changed=LEGEOS_BUILD_SET_ID");
    // The component matrix is compiled in, so a change to it must rebuild this crate.
    println!("cargo:rerun-if-changed=../../distro/deploy/components.toml");
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={path}");
    }

    let sha = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "0000000".to_owned());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.trim().is_empty());
    let epoch = git(&["log", "-1", "--format=%ct"])
        .and_then(|t| t.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());

    let derived = format!(
        "{version}~dev{epoch}+g{sha}{suffix}",
        suffix = if dirty { ".dirty" } else { "" }
    );
    let build_set = std::env::var("LEGEOS_BUILD_SET_ID").unwrap_or(derived);

    println!("cargo:rustc-env=LEGEOS_BUILD_SET_ID={build_set}");
    println!("cargo:rustc-env=LEGEOS_GIT_SHA={sha}");
    println!("cargo:rustc-env=LEGEOS_GIT_DIRTY={dirty}");
}

/// Files whose change means the derived identity changed. `HEAD` covers checkouts and commits;
/// the index covers staging, which is the cheapest available proxy for "the tree got dirty".
fn git_watch_paths() -> Vec<String> {
    let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) else {
        return Vec::new();
    };
    let dir = Path::new(dir.trim());
    ["HEAD", "index"]
        .iter()
        .map(|name| dir.join(name).display().to_string())
        .collect()
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
