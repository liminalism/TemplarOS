//! `cargo xtask <command>` — repository checks that must not drift.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{
    path::{Path, PathBuf},
    process::Command as Process,
};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "LegeOS repository automation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Every component home in ARCHITECTURE.md exists and has a README.
    TreeCheck,
    /// `lege-window` must not depend on tokio or zbus (compositor render-thread rule).
    DepsCheck,
    /// Every zbus interface name in the code has an XML file in interfaces/dbus/.
    CheckInterfaces,
    /// Print a DEP-5 copyright stanza list from cargo metadata (stub).
    Copyright,
}

/// Directories that must exist, each with a README.md.
const REQUIRED_DIRS: &[&str] = &[
    "crates",
    "services",
    "session",
    "compositor",
    "shell",
    "apps",
    "portals",
    "cli",
    "installer",
    "interfaces",
    "interfaces/dbus",
    "interfaces/wayland",
    "interfaces/schemas",
    "i18n",
    "distro",
    "distro/seeds",
    "distro/metapackages",
    "distro/image",
    "distro/iso",
    "distro/apt",
    "distro/policy",
    "distro/branding",
    "distro/release",
    "distro/hardware",
    "distro/deploy",
    "distro/app-manifests",
    "distro/forks",
    "packaging",
    "debian",
    "tests",
    "tests/integration",
    "tests/distro-checks",
    "tests/hardware",
    "tests/fixtures",
    "tools",
    "docs",
    "docs/planning",
    "docs/architecture",
    "docs/adr",
    "docs/protocols",
    "ci",
];

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = repo_root()?;
    match cli.command {
        Command::TreeCheck => tree_check(&root),
        Command::DepsCheck => deps_check(&root),
        Command::CheckInterfaces => check_interfaces(&root),
        Command::Copyright => copyright(&root),
    }
}

fn repo_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest_dir
        .parent()
        .and_then(Path::parent)
        .context("xtask must live in tools/xtask")?
        .to_path_buf())
}

fn tree_check(root: &Path) -> Result<()> {
    let mut problems = Vec::new();
    for dir in REQUIRED_DIRS {
        let path = root.join(dir);
        if !path.is_dir() {
            problems.push(format!("missing directory {dir}"));
        } else if !path.join("README.md").is_file() && !path.join("README").is_file() {
            problems.push(format!("{dir} has no README.md"));
        }
    }
    if problems.is_empty() {
        println!("tree-check: {} directories ok", REQUIRED_DIRS.len());
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("tree-check: {problem}");
        }
        bail!("{} problem(s)", problems.len())
    }
}

fn deps_check(root: &Path) -> Result<()> {
    let output = Process::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(root)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let resolve = metadata["resolve"]["nodes"]
        .as_array()
        .context("resolve.nodes")?;
    let packages = metadata["packages"].as_array().context("packages")?;
    let name_of = |id: &str| -> String {
        packages
            .iter()
            .find(|p| p["id"].as_str() == Some(id))
            .and_then(|p| p["name"].as_str())
            .unwrap_or(id)
            .to_owned()
    };
    let window = resolve
        .iter()
        .find(|node| name_of(node["id"].as_str().unwrap_or_default()) == "lege-window")
        .context("lege-window not in resolve graph")?;

    // Transitive closure of lege-window's normal dependencies.
    let mut stack: Vec<String> = window["deps"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|d| d["pkg"].as_str().map(str::to_owned))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(node) = resolve.iter().find(|n| n["id"].as_str() == Some(&id)) {
            for dep in node["deps"].as_array().unwrap_or(&vec![]) {
                // dev-dependencies are excluded: only kinds null (normal) or "build".
                let normal = dep["dep_kinds"]
                    .as_array()
                    .is_none_or(|kinds| kinds.iter().any(|k| k["kind"].is_null()));
                if normal && let Some(pkg) = dep["pkg"].as_str() {
                    stack.push(pkg.to_owned());
                }
            }
        }
    }
    let banned = ["tokio", "zbus", "async-std", "smol"];
    let offenders: Vec<String> = seen
        .iter()
        .map(|id| name_of(id))
        .filter(|name| banned.contains(&name.as_str()))
        .collect();
    if offenders.is_empty() {
        println!(
            "deps-check: lege-window is free of async runtimes and D-Bus ({} transitive deps)",
            seen.len()
        );
        Ok(())
    } else {
        bail!("deps-check: lege-window depends on {offenders:?}")
    }
}

fn check_interfaces(root: &Path) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for dir in [
        "crates",
        "services",
        "compositor",
        "portals",
        "shell",
        "apps",
    ] {
        collect_interface_names(&root.join(dir), &mut names)?;
    }
    let mut missing = Vec::new();
    for name in &names {
        if !root
            .join("interfaces/dbus")
            .join(format!("{name}.xml"))
            .is_file()
        {
            missing.push(name.clone());
        }
    }
    if missing.is_empty() {
        println!("check-interfaces: {} interface(s) documented", names.len());
        Ok(())
    } else {
        bail!("check-interfaces: no XML for {missing:?}")
    }
}

fn collect_interface_names(
    dir: &Path,
    names: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_interface_names(&path, names)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path)?;
            for line in text.lines() {
                for marker in ["#[zbus::interface(name = \"", "interface = \""] {
                    if let Some(start) = line.find(marker) {
                        let rest = &line[start + marker.len()..];
                        if let Some(end) = rest.find('"') {
                            let name = &rest[..end];
                            if name.starts_with("org.legeos.") {
                                names.insert(name.to_owned());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn copyright(root: &Path) -> Result<()> {
    let output = Process::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()?;
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    println!(
        "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\nUpstream-Name: legeos\n"
    );
    for package in metadata["packages"].as_array().unwrap_or(&vec![]) {
        println!(
            "Files: {}/*\nCopyright: 2026 LegeOS contributors\nLicense: {}\n",
            package["manifest_path"]
                .as_str()
                .unwrap_or("?")
                .trim_end_matches("/Cargo.toml")
                .trim_start_matches(&*root.to_string_lossy())
                .trim_start_matches('/'),
            package["license"].as_str().unwrap_or("MPL-2.0")
        );
    }
    Ok(())
}
