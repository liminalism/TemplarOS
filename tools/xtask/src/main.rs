//! `cargo xtask <command>` — repository checks that must not drift.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    /// Every `debian/control` Package: has a matching `[package.metadata.deb]` crate.
    CheckPackaging,
    /// Every client `#[proxy(interface = "org.legeos...")]` has a live `#[zbus::interface]`
    /// implementation somewhere in the workspace, not just an XML file.
    CheckProxies,
    /// Every vendored asset (a font, an icon set — anything not built from source in this
    /// repository) carries its own licence file, so `debian/copyright` can account for it.
    CheckAssets,
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
        Command::CheckPackaging => check_packaging(&root),
        Command::CheckProxies => check_proxies(&root),
        Command::CheckAssets => check_assets(&root),
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

/// Runs `cargo metadata` and parses its JSON. Shared by every check that needs the workspace's
/// resolved dependency graph (`deps-check`), its `[package.metadata.*]` tables (`check-packaging`)
/// or its plain package list (`copyright`) — one process spawn and one parse implementation
/// instead of three copies that could drift (e.g. one forgetting to check `output.status`).
fn cargo_metadata(root: &Path, extra_args: &[&str]) -> Result<serde_json::Value> {
    let output = Process::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .args(extra_args)
        .current_dir(root)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing cargo metadata output")
}

fn deps_check(root: &Path) -> Result<()> {
    let metadata = cargo_metadata(root, &["--locked"])?;
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
    let mut seen = BTreeSet::new();
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

/// Every zbus interface name in the code has an XML file in `interfaces/dbus/`.
///
/// Two independent passes, reported separately, because they check two different things that
/// happen to share one contract file:
///   - *server* names come from `#[zbus::interface(name = "...")]`, the trait a daemon actually
///     serves on the bus.
///   - *client* names come from `#[proxy(interface = "...")]` / `#[zbus::proxy(interface =
///     "...")]`, the typed proxies `lege-client` hands to every consumer.
///
/// This used to be one pass sharing a generic `"interface = \""` substring marker with the
/// server-side one — a marker meant only for `#[zbus::interface(name = "...")]` that also
/// happened to match the proxy macro's `interface = "..."` argument line, since `crates/`
/// (where the proxies live) was already scanned for the server pass. Verified empirically before
/// touching this: pointing a proxy's `interface = "..."` at a made-up name with no matching XML
/// and running the old check *did* fail, so client-proxy drift was already caught — by accident,
/// not by design, and only because `crates/` happened to be in the scanned list and no other
/// `interface = "..."` text existed anywhere in a scanned file. This version keeps that same
/// coverage but earns it: client extraction is scoped to the text inside a `#[proxy(...)]` /
/// `#[zbus::proxy(...)]` attribute's argument list, so it can never also match an unrelated
/// `interface = "..."` sitting in a comment, doc string or struct field elsewhere in the tree.
fn check_interfaces(root: &Path) -> Result<()> {
    let mut server = BTreeSet::new();
    let mut client = BTreeSet::new();
    // `cli/` only calls into `lege-client`'s already-scanned proxies and declares no interface
    // name of its own today (checked: no `interface` literal anywhere under `cli/lege/src/`), so
    // scanning it is a no-op in practice — kept in the list so a future proxy or interface literal
    // written directly in the CLI is still caught by construction, not left uncovered by omission.
    for dir in [
        "crates",
        "services",
        "compositor",
        "portals",
        "shell",
        "apps",
        "cli",
    ] {
        collect_interfaces(&root.join(dir), &mut server, &mut client)?;
    }

    let all: BTreeSet<&str> = server.iter().chain(&client).map(String::as_str).collect();
    let mut missing = Vec::new();
    for name in &all {
        if !root
            .join("interfaces/dbus")
            .join(format!("{name}.xml"))
            .is_file()
        {
            let origin = if server.contains(*name) && client.contains(*name) {
                "server, client"
            } else if server.contains(*name) {
                "server"
            } else {
                "client"
            };
            missing.push(format!("{name} [{origin}]"));
        }
    }
    if missing.is_empty() {
        println!(
            "check-interfaces: {} server interface(s), {} client interface(s), {} unique interface(s) documented",
            server.len(),
            client.len(),
            all.len()
        );
        Ok(())
    } else {
        for name in &missing {
            eprintln!("check-interfaces: no XML for {name}");
        }
        bail!("{} interface(s) undocumented", missing.len())
    }
}

/// Walks `dir` recursively, adding every `org.legeos.*` server interface name it finds to
/// `server` and every `org.legeos.*` client (proxy) interface name to `client`.
fn collect_interfaces(
    dir: &Path,
    server: &mut BTreeSet<String>,
    client: &mut BTreeSet<String>,
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
            collect_interfaces(&path, server, client)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path)?;
            server.extend(server_interface_names(&text));
            client.extend(client_interface_names(&text));
        }
    }
    Ok(())
}

/// Server-side interface names: `#[zbus::interface(name = "org.legeos...")]`.
fn server_interface_names(text: &str) -> BTreeSet<String> {
    attribute_string_args(text, &["#[zbus::interface("], "name")
}

/// Client-side interface names: `#[proxy(interface = "org.legeos...")]` and its
/// `#[zbus::proxy(...)]` spelling.
fn client_interface_names(text: &str) -> BTreeSet<String> {
    attribute_string_args(text, &["#[proxy(", "#[zbus::proxy("], "interface")
}

/// Every `org.legeos.*` value of the `key = "..."` argument of any attribute opened by one of
/// `openers`.
///
/// Deliberately not a per-line scan with a fixed-spacing marker. Three shapes a line scan gets
/// wrong, each of which turns a *green* check into one that is checking nothing:
///   * the attribute spanning several lines (`#[proxy(` on one, `interface = "..."` on the next),
///     which is how every proxy in `crates/lege-client` is actually written;
///   * `key="value"` with no spaces around `=`, which rustfmt has no opinion about inside an
///     attribute and which a `key = "` marker silently misses;
///   * the attribute appearing verbatim inside a `//` or `///` comment — a doc example showing
///     callers how to declare one — which must not be mistaken for a real declaration.
///
/// The argument list is bounded by its own balanced parentheses (string-literal aware), so a
/// `key = "..."` occurring anywhere else in the file can never be picked up by mistake.
fn attribute_string_args(text: &str, openers: &[&str], key: &str) -> BTreeSet<String> {
    let mut values = BTreeSet::new();
    for opener in openers {
        let mut cursor = 0;
        while let Some(relative) = text[cursor..].find(opener) {
            let at = cursor + relative;
            // Advance past this opener regardless of whether it yields a name, so a malformed or
            // commented-out attribute cannot spin the loop.
            cursor = at + opener.len();
            if in_line_comment(text, at) {
                continue;
            }
            let open_paren = at + opener.len() - 1;
            if let Some(body) = balanced_parenthesised(text, open_paren)
                && let Some(value) = string_argument(body, key)
                && value.starts_with("org.legeos.")
            {
                values.insert(value);
            }
        }
    }
    values
}

/// Whether `index` sits on a line whose first non-whitespace characters are `//` — a commented
/// example rather than a live declaration.
fn in_line_comment(text: &str, index: usize) -> bool {
    let line_start = text[..index].rfind('\n').map_or(0, |newline| newline + 1);
    text[line_start..index].trim_start().starts_with("//")
}

/// The text between the parenthesis at `open` and its matching close, honouring string literals
/// so a `(` or `)` inside `"..."` cannot unbalance the scan.
fn balanced_parenthesised(text: &str, open: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[open + 1..index]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The string literal assigned to `key` inside an attribute's argument list, with any amount of
/// whitespace around `=`. `key` must appear as a whole argument name, so looking for `interface`
/// never matches the tail of `default_interface`.
fn string_argument(body: &str, key: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut cursor = 0;
    while let Some(relative) = body[cursor..].find(key) {
        let at = cursor + relative;
        cursor = at + key.len();
        let starts_argument = at == 0
            || bytes
                .get(at - 1)
                .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'(' || *byte == b',');
        if !starts_argument {
            continue;
        }
        let after_key = body[at + key.len()..].trim_start();
        let Some(after_equals) = after_key.strip_prefix('=') else {
            continue;
        };
        let Some(literal) = after_equals.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(end) = literal.find('"') {
            return Some(literal[..end].to_owned());
        }
    }
    None
}

/// Every client `#[proxy(interface = "org.legeos...")]` must name an interface some
/// `#[zbus::interface(name = "...")]` in this workspace actually implements — a renamed or
/// typo'd interface on the client side would otherwise only be caught the first time someone
/// runs the client against a real bus. `check-interfaces` (above) already extracts `server` and
/// `client` name sets correctly (including the multi-line-aware `#[proxy(...)]` scan); this
/// reuses that same extraction rather than re-walking the tree with a second parser, and asks
/// the one question `check-interfaces` does not: is every client name also a server name.
fn check_proxies(root: &Path) -> Result<()> {
    let mut server = BTreeSet::new();
    let mut client = BTreeSet::new();
    for dir in [
        "crates",
        "services",
        "compositor",
        "portals",
        "shell",
        "apps",
        "cli",
    ] {
        collect_interfaces(&root.join(dir), &mut server, &mut client)?;
    }

    let orphaned: Vec<&String> = client
        .iter()
        .filter(|name| !server.contains(*name))
        .collect();
    if orphaned.is_empty() {
        println!(
            "check-proxies: {} client interface(s), all with a live server implementation",
            client.len()
        );
        Ok(())
    } else {
        for name in &orphaned {
            eprintln!(
                "check-proxies: {name}: a #[proxy(...)] references it, but no \
                 #[zbus::interface(name = \"{name}\")] exists anywhere in the workspace"
            );
        }
        bail!(
            "{} client interface(s) with no server implementation",
            orphaned.len()
        )
    }
}

/// `debian/control` packages for which no workspace crate needs `[package.metadata.deb]`.
///
/// Checked directly against every crate under `debian/control` as of this writing
/// (legeos-sessiond, legeos-resourced, legeos-filed, legeos-hostd, legeos-boot-health,
/// legeos-session, legeos-shell, legeos-settings, legeos-portal, legeos-cli): every one of them
/// is built from exactly one `cargo-deb`-packaged crate and already carries a
/// `[package.metadata.deb]` table whose `name` matches its `Package:` stanza — including
/// legeos-session and legeos-boot-health, which read as though they might be hand-assembled
/// metapackages but are each an ordinary single-binary crate (`session/lege-session`,
/// `services/lege-boot-health`) with its own `[package.metadata.deb]`. So this list is empty
/// today. It exists for the day a `debian/control` package genuinely has no `cargo-deb` crate of
/// its own (for example a virtual/meta package assembled only from `debian/*.install` and
/// `debian/rules`, never `cargo deb -p ...`) — add its `Package:` name here with a comment
/// pointing at the `debian/control`/`debian/rules` evidence, rather than loosening the check for
/// everyone or leaving it perpetually red.
const CONTROL_PACKAGES_WITHOUT_DEB_METADATA: &[&str] = &[];

/// Every `debian/control` `Package:` stanza must resolve to a workspace crate whose
/// `[package.metadata.deb] name` matches it, so `cargo deb -p <crate>` and the `Package:` field
/// `debian/rules`/CI expect can never silently drift apart (a renamed package or a crate that
/// lost its metadata table would otherwise only surface as a build failure at release time).
fn check_packaging(root: &Path) -> Result<()> {
    let control_text =
        std::fs::read_to_string(root.join("debian/control")).context("reading debian/control")?;
    let control_packages = parse_control_packages(&control_text);
    if control_packages.is_empty() {
        bail!("check-packaging: found no Package: stanzas in debian/control");
    }

    let output = Process::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
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
    let packages = metadata["packages"].as_array().context("packages")?;

    // cargo metadata's `metadata` field is cargo's own parse of `[package.metadata.*]`, so this
    // reads the same `[package.metadata.deb]` table `cargo deb` reads — never a hand-rolled scan
    // of Cargo.toml that could drift from what cargo-deb actually sees.
    let mut deb_name_to_crate = BTreeMap::new();
    for package in packages {
        if let Some(deb_name) = package["metadata"]["deb"]["name"].as_str() {
            let crate_name = package["name"].as_str().unwrap_or("?");
            deb_name_to_crate.insert(deb_name.to_owned(), crate_name.to_owned());
        }
    }

    let problems = packaging_problems(&control_packages, &deb_name_to_crate);
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("check-packaging: {problem}");
        }
        bail!("{} packaging mismatch(es)", problems.len());
    }

    println!(
        "check-packaging: {} debian/control package(s) all have matching crate metadata",
        control_packages.len()
    );
    Ok(())
}

/// Compare `debian/control`'s `Package:` stanzas against the `[package.metadata.deb] name` of
/// every workspace crate, in both directions. Pure, so both directions are testable against
/// literal fixtures without a `debian/` tree or a `cargo metadata` subprocess.
///
/// Forward: a `Package:` with no crate to build it is a package `debian/rules` expects and
/// `cargo deb` will never produce. Reverse: a crate whose deb name matches no stanza builds a
/// `.deb` nothing installs — the same typo, caught from the other side.
fn packaging_problems(
    control_packages: &[String],
    deb_name_to_crate: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for package in control_packages {
        if !deb_name_to_crate.contains_key(package)
            && !CONTROL_PACKAGES_WITHOUT_DEB_METADATA.contains(&package.as_str())
        {
            problems.push(format!(
                "{package}: a debian/control Package: with no crate [package.metadata.deb] name"
            ));
        }
    }
    let control: BTreeSet<&str> = control_packages.iter().map(String::as_str).collect();
    for (deb_name, crate_name) in deb_name_to_crate {
        if !control.contains(deb_name.as_str()) {
            problems.push(format!(
                "{deb_name} (crate {crate_name}): a [package.metadata.deb] name with no debian/control Package: stanza"
            ));
        }
    }
    problems
}

/// The `Package:` field of every stanza in `debian/control`, in file order. Debian policy §5.1
/// makes `Package:` the first field of a binary-package paragraph, so a plain per-line scan for
/// the field name is enough — no need to first split the file into blank-line-separated
/// paragraphs.
fn parse_control_packages(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("Package:"))
        .map(|rest| rest.trim().to_owned())
        .collect()
}

/// Filenames accepted as a licence record beside a vendored asset.
const LICENCE_FILENAMES: &[&str] = &[
    "LICENSE",
    "LICENCE",
    "COPYING",
    "LICENSE.txt",
    "COPYING.txt",
];

/// One vendored asset directory, and whether a licence file sits with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendoredAssets {
    pub dir: String,
    pub licence: Option<String>,
}

/// Vendored assets without a licence file beside them.
///
/// `cargo metadata` sees crate manifests, so a font or icon checked into `assets/` is invisible
/// to it — and therefore invisible to `debian/copyright`, which is generated from it. That is a
/// licensing defect that ships silently: the package carries someone else's work with no record
/// of the terms. This is the check that stops it.
fn unlicensed_assets(found: &[VendoredAssets]) -> Vec<String> {
    found
        .iter()
        .filter(|assets| assets.licence.is_none())
        .map(|assets| {
            format!(
                "{}: vendored assets with no {} beside them",
                assets.dir,
                LICENCE_FILENAMES.join("/")
            )
        })
        .collect()
}

/// Find every `assets/` directory belonging to a crate in the workspace.
fn scan_vendored_assets(root: &Path) -> Vec<VendoredAssets> {
    let mut found = Vec::new();
    for entry in walk_assets_dirs(root) {
        let licence = LICENCE_FILENAMES
            .iter()
            .map(|name| entry.join(name))
            .find(|candidate| candidate.is_file())
            .or_else(|| {
                std::fs::read_dir(&entry).ok().and_then(|children| {
                    children
                        .filter_map(std::result::Result::ok)
                        .map(|child| child.path())
                        .filter(|path| path.is_dir())
                        .flat_map(|dir| {
                            LICENCE_FILENAMES
                                .iter()
                                .map(|name| dir.join(name))
                                .collect::<Vec<_>>()
                        })
                        .find(|candidate| candidate.is_file())
                })
            });
        found.push(VendoredAssets {
            dir: relative(root, &entry),
            licence: licence.map(|path| relative(root, &path)),
        });
    }
    found.sort_by(|a, b| a.dir.cmp(&b.dir));
    found
}

fn walk_assets_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                &*name,
                "target" | ".git" | "vendor" | "forks" | "node_modules"
            ) {
                continue;
            }
            if name == "assets" {
                out.push(path);
            } else {
                stack.push(path);
            }
        }
    }
    out
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn check_assets(root: &Path) -> Result<()> {
    let found = scan_vendored_assets(root);
    let problems = unlicensed_assets(&found);
    if problems.is_empty() {
        println!(
            "check-assets: {} vendored asset director(ies), each with a licence file",
            found.len()
        );
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("check-assets: {problem}");
        }
        bail!("{} problem(s)", problems.len())
    }
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
    // Vendored assets are not crates, so `cargo metadata` never mentions them; without this the
    // package would ship third-party work with no record of its terms.
    for assets in scan_vendored_assets(root) {
        println!(
            "Files: {}/*\nCopyright: see {}\nLicense: see {}\n",
            assets.dir,
            assets
                .licence
                .as_deref()
                .unwrap_or("(MISSING — run check-assets)"),
            assets
                .licence
                .as_deref()
                .unwrap_or("(MISSING — run check-assets)")
        );
    }
    Ok(())
}

#[cfg(test)]
mod extraction_tests {
    use super::{
        VendoredAssets, client_interface_names, packaging_problems, parse_control_packages,
        server_interface_names, unlicensed_assets,
    };
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn vendored_assets_without_a_licence_are_reported() {
        let found = vec![
            VendoredAssets {
                dir: "shell/lege-shell/assets".into(),
                licence: Some("shell/lege-shell/assets/fonts/LICENSE".into()),
            },
            VendoredAssets {
                dir: "apps/lege-greeter/assets".into(),
                licence: None,
            },
        ];
        let problems = unlicensed_assets(&found);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("apps/lege-greeter/assets:"));
        assert!(unlicensed_assets(&found[..1]).is_empty());
    }

    fn names(set: &BTreeSet<String>) -> Vec<&str> {
        set.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_single_line_server_attribute_is_found() {
        let text = r#"#[zbus::interface(name = "org.legeos.Widgets1")]
            impl WidgetsApi {}"#;
        assert_eq!(
            names(&server_interface_names(text)),
            ["org.legeos.Widgets1"]
        );
    }

    #[test]
    fn a_server_attribute_split_across_lines_is_found() {
        // The shape a line-based scan with a `name = "` marker silently misses.
        let text = "#[zbus::interface(\n    name = \"org.legeos.Widgets1\"\n)]\nimpl WidgetsApi {}";
        assert_eq!(
            names(&server_interface_names(text)),
            ["org.legeos.Widgets1"]
        );
    }

    #[test]
    fn server_and_client_attributes_tolerate_any_spacing_around_equals() {
        let server = "#[zbus::interface(name=\"org.legeos.Widgets1\")]";
        let client = "#[proxy(interface   =    \"org.legeos.Widgets1\")]";
        assert_eq!(
            names(&server_interface_names(server)),
            ["org.legeos.Widgets1"]
        );
        assert_eq!(
            names(&client_interface_names(client)),
            ["org.legeos.Widgets1"]
        );
    }

    #[test]
    fn an_attribute_inside_a_comment_is_not_a_declaration() {
        // A doc example showing callers how to declare one must not register as the real thing;
        // otherwise `check-interfaces` demands XML for an interface nobody implements, and
        // `check-proxies` reports a server that does not exist.
        let text = r#"
            /// Declare one like this:
            /// #[zbus::interface(name = "org.legeos.Documented1")]
            // #[proxy(interface = "org.legeos.CommentedOut1")]
            #[zbus::interface(name = "org.legeos.Real1")]
            impl RealApi {}
        "#;
        assert_eq!(names(&server_interface_names(text)), ["org.legeos.Real1"]);
        assert!(client_interface_names(text).is_empty());
    }

    #[test]
    fn a_multiline_proxy_attribute_is_found_and_both_spellings_work() {
        let text = "#[proxy(\n    interface = \"org.legeos.Widgets1\",\n    default_service = \"org.legeos.Session\",\n    default_path = \"/org/legeos/Session/Widgets\"\n)]\npub trait Widgets {}";
        assert_eq!(
            names(&client_interface_names(text)),
            ["org.legeos.Widgets1"]
        );
        let qualified = "#[zbus::proxy(interface = \"org.legeos.Other1\")]";
        assert_eq!(
            names(&client_interface_names(qualified)),
            ["org.legeos.Other1"]
        );
    }

    #[test]
    fn an_interface_key_is_matched_as_a_whole_argument_name() {
        // `default_interface` must not be mistaken for `interface`.
        let text =
            r#"#[proxy(default_interface = "org.legeos.Wrong1", interface = "org.legeos.Right1")]"#;
        assert_eq!(names(&client_interface_names(text)), ["org.legeos.Right1"]);
    }

    #[test]
    fn a_key_equals_value_outside_any_attribute_is_never_picked_up() {
        // The boundary that makes this a real check of the attribute rather than a substring
        // scan over the whole file.
        let text = r#"
            const DOCS: &str = "interface = \"org.legeos.NotReal1\"";
            let name = "org.legeos.AlsoNotReal1";
        "#;
        assert!(client_interface_names(text).is_empty());
        assert!(server_interface_names(text).is_empty());
    }

    #[test]
    fn non_legeos_interfaces_are_ignored() {
        let text = r#"#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
            #[proxy(interface = "org.freedesktop.DBus")]"#;
        assert!(server_interface_names(text).is_empty());
        assert!(client_interface_names(text).is_empty());
    }

    #[test]
    fn check_proxies_goes_green_when_every_client_has_a_server() {
        let server = server_interface_names(r#"#[zbus::interface(name = "org.legeos.Widgets1")]"#);
        let client = client_interface_names(r#"#[proxy(interface = "org.legeos.Widgets1")]"#);
        let orphaned: Vec<&String> = client.iter().filter(|n| !server.contains(*n)).collect();
        assert!(orphaned.is_empty());
    }

    #[test]
    fn check_proxies_goes_red_for_a_client_interface_with_no_server() {
        let server = server_interface_names(r#"#[zbus::interface(name = "org.legeos.Widgets1")]"#);
        let client = client_interface_names(r#"#[proxy(interface = "org.legeos.Ghost1")]"#);
        let orphaned: Vec<&String> = client.iter().filter(|n| !server.contains(*n)).collect();
        assert_eq!(orphaned, [&"org.legeos.Ghost1".to_owned()]);
    }

    #[test]
    fn control_packages_parse_with_odd_but_legal_spacing() {
        // Written with explicit escapes rather than `\`-continued lines: Rust's line continuation
        // also eats the next line's leading whitespace, which would silently destroy the one
        // character that makes the continuation-line case a test at all.
        let control = concat!(
            "Source: legeos\n",
            "\n",
            "Package: legeos-cli   \n", // trailing whitespace
            "Architecture: any\n",
            "Description: one\n",
            "\n",
            "Package:legeos-shell\n", // no space after the colon
            "Architecture: any\n",
            "Description: two that continues\n",
            " Package: not-a-stanza-it-is-a-continuation-line\n", // leading space
            "\n",
            "Package-Type: udeb\n", // a different field that merely starts with "Package"
        );
        assert_eq!(
            parse_control_packages(control),
            ["legeos-cli".to_owned(), "legeos-shell".to_owned()],
        );
    }

    fn deb_names(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(deb, krate)| ((*deb).to_owned(), (*krate).to_owned()))
            .collect()
    }

    #[test]
    fn check_packaging_goes_green_when_both_sides_match() {
        let control = vec!["legeos-cli".to_owned(), "legeos-shell".to_owned()];
        let debs = deb_names(&[("legeos-cli", "lege"), ("legeos-shell", "lege-shell")]);
        assert!(packaging_problems(&control, &debs).is_empty());
    }

    #[test]
    fn check_packaging_goes_red_for_a_control_package_with_no_deb_metadata() {
        let control = vec!["legeos-cli".to_owned(), "legeos-orphan".to_owned()];
        let debs = deb_names(&[("legeos-cli", "lege")]);
        let problems = packaging_problems(&control, &debs);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("legeos-orphan"));
        assert!(problems[0].contains("no crate [package.metadata.deb]"));
    }

    #[test]
    fn check_packaging_goes_red_for_deb_metadata_with_no_control_stanza() {
        let control = vec!["legeos-cli".to_owned()];
        let debs = deb_names(&[("legeos-cli", "lege"), ("legeos-ghost", "lege-ghost")]);
        let problems = packaging_problems(&control, &debs);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("legeos-ghost"));
        assert!(problems[0].contains("crate lege-ghost"));
    }
}
