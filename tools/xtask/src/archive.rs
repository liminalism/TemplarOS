//! `cargo xtask archive` — the archive-side half of build identity.
//!
//! One decision, recorded as `@templaros.decision.build-set-id-is-the-version-authority`: the
//! build-set id is the only version TemplarOS has. `cargo deb` already receives it as
//! `--deb-version`; the *source* package reads its version from `debian/changelog`, which used to
//! be maintained by hand and said something else entirely. This command closes that gap by
//! generating the changelog head entry from the same id.
//!
//! **Why it is not written into the checkout.** The id contains `.dirty` when the working tree has
//! uncommitted changes. Writing a generated file into the tree would therefore change the id that
//! the file was generated from — a build that cannot describe itself. So `--write` is aimed at a
//! disposable copy of the tree (`ci/source-build.sh` makes one), and the checkout keeps a
//! placeholder stanza that says it is generated.

use anyhow::{Context, Result};
use clap::Subcommand;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command as Process,
};
use templar_build::Provenance;

/// The distribution the generated stanza targets — Ubuntu 26.04, per
/// `@templaros.decision.base-ubuntu-2604-kernel-70`.
pub const DISTRIBUTION: &str = "resolute";
/// The changelog trailer's maintainer, matching `debian/control`.
pub const MAINTAINER: &str = "TemplarOS contributors <dev@templaros.org>";

#[derive(Debug, Subcommand)]
pub enum ArchiveCommand {
    /// Print the `debian/changelog` head entry for the current build set — or write it into a
    /// disposable source tree with `--write`.
    Changelog {
        /// Write the stanza into this changelog instead of printing it. Aim it at a copy of the
        /// tree, never at the checkout: see the module comment.
        #[arg(long)]
        write: Option<PathBuf>,
        /// Override the epoch in the id (reproducible builds, tests).
        #[arg(long)]
        epoch: Option<u64>,
    },
    /// Print this host's build provenance as JSON — what a set built here would record.
    Provenance {
        /// D1's package-closure report, whose `observed_at` and `resolution` travel with the set.
        #[arg(long)]
        closure_report: Option<PathBuf>,
    },
}

/// # Errors
/// If the workspace version cannot be read, or the changelog cannot be written.
pub fn run(root: &Path, command: &ArchiveCommand) -> Result<()> {
    match command {
        ArchiveCommand::Changelog { write, epoch } => {
            let (id, epoch) = crate::build_set::current_id(root, *epoch)?;
            let stanza = templar_build::changelog_entry(&id, DISTRIBUTION, MAINTAINER, epoch);
            match write {
                None => print!("{stanza}"),
                Some(path) => {
                    let existing = fs::read_to_string(path).unwrap_or_default();
                    fs::write(path, templar_build::changelog_with_head(&existing, &stanza))
                        .with_context(|| format!("writing {}", path.display()))?;
                    println!("{}: head entry is now {id}", path.display());
                }
            }
            Ok(())
        }
        ArchiveCommand::Provenance { closure_report } => {
            let provenance = provenance(root, closure_report.as_deref());
            println!("{}", serde_json::to_string_pretty(&provenance)?);
            Ok(())
        }
    }
}

/// What this host would record about a build: toolchain, kernel/architecture, and — when D1's
/// package-closure report is given — when the Ubuntu index was observed and whether it resolved.
///
/// Every field is best-effort. A missing `rustc`, `uname` or `dpkg-architecture` produces an empty
/// string rather than a failure: provenance describes a build, it does not gate one.
#[must_use]
pub fn provenance(root: &Path, closure_report: Option<&Path>) -> Provenance {
    let mut provenance = Provenance {
        rustc: first_line(root, "rustc", &["--version"]),
        host: first_line(root, "uname", &["-srm"]),
        arch: first_line(root, "dpkg", &["--print-architecture"]),
        ..Provenance::default()
    };
    if let Some(path) = closure_report {
        provenance.ubuntu_index_source = Some(path.display().to_string());
        if let Ok(body) = fs::read_to_string(path)
            && let Ok(report) = serde_json::from_str::<serde_json::Value>(&body)
        {
            provenance.ubuntu_index_observed_at = report
                .get("observed_at")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned);
            provenance.ubuntu_index_resolution = report
                .get("resolution")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned);
        }
    }
    provenance
}

fn first_line(root: &Path, program: &str, args: &[&str]) -> String {
    let Ok(output) = Process::new(program).args(args).current_dir(root).output() else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}
