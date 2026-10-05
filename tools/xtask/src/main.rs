//! `cargo xtask <command>` — repository checks that must not drift.

mod archive;
mod build_set;
mod index_bench;
mod seeds;
mod signatures;
mod wayland_versions;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command as Process,
};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "TemplarOS repository automation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Every component home in ARCHITECTURE.md exists and has a README.
    TreeCheck,
    /// `templar-window` must not depend on tokio or zbus (compositor render-thread rule).
    DepsCheck,
    /// Every zbus interface name in the code has an XML file in interfaces/dbus/.
    CheckInterfaces,
    /// Every `debian/control` Package: has a matching `[package.metadata.deb]` crate.
    CheckPackaging,
    /// Every client `#[proxy(interface = "org.templaros...")]` has a live `#[zbus::interface]`
    /// implementation somewhere in the workspace, not just an XML file.
    CheckProxies,
    /// Every vendored asset (a font, an icon set — anything not built from source in this
    /// repository) carries its own licence file, so `debian/copyright` can account for it.
    CheckAssets,
    /// Every `ActionDecl` with a `cli_verb` names a real `templar` subcommand, every action a shell
    /// surface performs is declared, and every action with a chord also has a pointer route.
    CheckActions,
    /// Every `since` in the private Wayland protocols is reachable, and both ends' negotiated
    /// version constants are the version the XML actually declares.
    CheckWaylandVersions,
    /// Print a DEP-5 copyright stanza list from cargo metadata (stub).
    Copyright,
    /// Build every package in the component matrix as one identified set.
    BuildSet {
        /// Print the plan and exit without building anything.
        #[arg(long)]
        dry_run: bool,
        /// Print the build-set id this invocation would use, and nothing else.
        #[arg(long)]
        print_id: bool,
        /// Override the epoch in the id (reproducible builds, tests).
        #[arg(long)]
        epoch: Option<u64>,
        /// D1's package-closure report; its observation time travels with the set as provenance.
        #[arg(long)]
        closure_report: Option<PathBuf>,
    },
    /// Build identity on the archive side: the generated `debian/changelog` head entry, and the
    /// provenance a build set records.
    Archive {
        #[command(subcommand)]
        command: archive::ArchiveCommand,
    },
    /// Freeze every shipped D-Bus signature into interfaces/baseline.json.
    InterfaceBaseline,
    /// No shipped D-Bus signature has changed or disappeared since the baseline.
    CheckInterfaceSignatures,
    /// Render every generated package artifact from `distro/seeds/selection.toml`: the seed
    /// files, the mkosi package drop-ins and the metapackage control files.
    Seeds {
        /// `closure`: report what the selection resolves to instead of rendering anything.
        #[arg(value_enum)]
        action: Option<SeedsAction>,
        /// Fail when a generated file has drifted from the declaration, instead of rewriting it.
        #[arg(long)]
        check: bool,
        /// Write the closure report to this file instead of standard output.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print the component matrix — one field per component, for the shell scripts that used to
    /// keep their own copy of the package list.
    Components {
        /// Which field to print: `name`, `package`, `crate`, `unit` or `activation`.
        #[arg(long, default_value = "package")]
        field: String,
    },
    /// Measure the shared file index's runtime cost: directory counts, watch limits, timed
    /// workloads and idle-cost sampling (`docs/measurements/`). See `index_bench` module docs.
    IndexBench(index_bench::IndexBenchArgs),
}

/// The one thing `seeds` does that is not rendering.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum SeedsAction {
    /// Direct and transitive packages, installed size, recommends, missing names and provider
    /// roles — resolved against whatever archive indexes this machine actually has.
    Closure,
}

/// How a shell surface declares that an action is reachable by pointer. Each is a literal
/// constructor with the action id as its first string argument, so a route is a thing that
/// exists in the source rather than a claim in a comment.
///
/// Adding a needle here is how a *new kind* of surface (a titlebar button, a dock menu) starts
/// counting; adding a row to one of those tables is how an individual action gets its route.
const POINTER_ROUTE_NEEDLES: &[&str] = &[
    // `crate::surface::field_screen`: a region that performs an action directly.
    "FieldAction::RunAction(\"",
    // `crate::surface::widgets`: a clickable region declared on a system tile.
    "WidgetRegion::new(\"",
    // `crate::surface::context_menu`: a row on the field's secondary-click menu.
    "MenuItem::new(\"",
    // `crate::surface::file_browser`: a button on the file-verb row of a Home file surface.
    // These verbs are reachable by accelerator too, and the accelerators are surface-local
    // rather than compositor chords — so without this needle the row would be invisible here and
    // a file verb could end up keyboard-only without the check noticing.
    "FileVerbRoute::new(\"",
    // `templar_comp::window_groups::POINTER_GESTURES`: a pointer gesture the *compositor* performs
    // (dropping one window's band on another's groups them). Not every pointer route is a shell
    // widget — some are gestures on the workspace, which the shell cannot see and which keep
    // working when it is dead — and a rule that only counted shell rows would push those actions
    // onto the exemption list as if they were keyboard-only.
    "PointerGesture(\"",
];

/// Where a pointer route can be declared. The shell is where most of them are — menu rows,
/// regions, verb buttons — and the compositor is where the workspace's own gestures are.
const POINTER_ROUTE_ROOTS: &[&str] = &["shell/templar-shell/src", "compositor/templar-comp/src"];

/// Declared actions that carry a chord and, for now, legitimately have no pointer route.
///
/// **This list is meant to stay empty.** It exists so that adding a chord for something genuinely
/// not yet placeable on a surface fails loudly *here*, with a written reason, instead of quietly
/// re-creating the keyboard-only set this check was written to eliminate. Each entry is
/// `(action id, why)` and every one of them is a bug with a date on it.
const CHORD_WITHOUT_POINTER_ROUTE: &[(&str, &str)] = &[
    // 2026-09-09, the task system (docs/task-system-taskfield-island-peek.md §6, §7.3).
    // `taskfield.open`'s entry is gone: the Peek corner's promotion gesture is its pointer route
    // (`PointerGesture("taskfield.open")`). `window.within-unit-*` is owed one by the island's
    // fan, which is the pointer's way to the other windows of one unit, and its two entries are
    // deleted by the commit that lands it. `window.group` needs no entry: dropping
    // one window's band on another's is its pointer route, declared in
    // `templar_comp::window_groups::POINTER_GESTURES`; `window.ungroup` has the group tile's row.
    (
        "window.within-unit-forward",
        "compositor chord; the island's fan is the pointer route, not yet built",
    ),
    (
        "window.within-unit-backward",
        "compositor chord; the island's fan is the pointer route, not yet built",
    ),
    // 2026-09-10, the task system (§9). Super+Space reveals Home with the Apps field focused and
    // empty; the compositor sends `launcher_focus` and the shell performs this action. The pointer
    // route is a click in that field, which focuses it directly rather than through an action id.
    (
        "launcher.show-all",
        "compositor chord; a click in Home's Apps field is the pointer route, not action-routed",
    ),
    // 2026-09-03, the drop-down terminal (§4.6). The compositor reserves Super+grave; the pointer
    // route is the shell's — the terminal provider's own launcher row, which arrives with the
    // Providers1 `terminal` role.
    (
        "terminal.toggle",
        "compositor chord; the shell's launcher row for the terminal provider is the pointer route",
    ),
];

/// Every declared action's `cli_verb` reaches a real `templar` subcommand, every action id a shell
/// surface performs is declared, and every action with an accelerator also has a pointer route.
///
/// The point of one declaration driving three surfaces is that they cannot drift; nothing
/// enforces that at the type level, because the CLI's verbs are `clap` variants and the shell's
/// are string ids. This is the check that notices. It reads sources rather than running anything,
/// so it works in CI with no bus and no display.
///
/// The third rule is the manifest's own (`templar_types::action`'s module doc: "a chord is an
/// accelerator, never the only route... a chord with no pointer path is a bug"), and until this
/// check existed it was aspirational: eight actions were reachable from a keyboard and nowhere
/// else, and nothing said so.
fn check_actions(root: &Path) -> Result<()> {
    let manifest = std::fs::read_to_string(root.join("crates/templar-types/src/action.rs"))
        .context("reading the action manifest")?;
    let cli = std::fs::read_to_string(root.join("cli/templar/src/main.rs"))
        .context("reading templar's CLI")?;

    // `cli_verb: Some("session poweroff")` -> the subcommand path it claims.
    let mut verbs = Vec::new();
    for line in manifest.lines() {
        let Some(rest) = line.trim().strip_prefix("cli_verb: Some(\"") else {
            continue;
        };
        let Some(verb) = rest.split('"').next() else {
            continue;
        };
        verbs.push(verb.to_owned());
    }

    let mut missing = Vec::new();
    for verb in &verbs {
        // "session poweroff" needs a `Session` command with a `Poweroff` subcommand variant.
        let mut words = verb.split_whitespace();
        let (Some(group), Some(leaf)) = (words.next(), words.next()) else {
            missing.push(format!("{verb} (not a two-word verb)"));
            continue;
        };
        let group_variant = capitalise(group);
        let leaf_variant = capitalise(leaf);
        if !cli.contains(&format!("Command::{group_variant} ")) {
            missing.push(format!("{verb} (no `templar {group}`)"));
        } else if !cli.contains(&format!("{group_variant}Command::{leaf_variant}")) {
            missing.push(format!("{verb} (no `{leaf}` under `templar {group}`)"));
        }
    }

    // Every id a shell surface names must exist in the manifest, and every id with a chord must
    // be named by at least one of them.
    let routes = pointer_routes(root)?;
    let mut undeclared = Vec::new();
    for (id, wheres) in &routes {
        if !manifest.contains(&format!("id: \"{id}\"")) {
            undeclared.push(format!("{id} (in {})", wheres.join(", ")));
        }
    }

    let declared = declared_actions(&manifest);
    if declared.len() < 5 {
        bail!(
            "check-actions: only {} action declaration(s) parsed out of the manifest — its shape \
             must have changed, and this check is no longer reading it",
            declared.len()
        );
    }
    let exempt: BTreeMap<&str, &str> = CHORD_WITHOUT_POINTER_ROUTE.iter().copied().collect();
    let with_a_route: BTreeSet<&str> = routes.keys().map(String::as_str).collect();
    let (keyboard_only, stale_exemptions) = chord_route_problems(&declared, &with_a_route, &exempt);

    if !missing.is_empty()
        || !undeclared.is_empty()
        || !keyboard_only.is_empty()
        || !stale_exemptions.is_empty()
    {
        for entry in &missing {
            eprintln!("check-actions: declared CLI verb has no subcommand: {entry}");
        }
        for entry in &undeclared {
            eprintln!("check-actions: surface performs an undeclared action: {entry}");
        }
        for id in &keyboard_only {
            eprintln!(
                "check-actions: {id} has a chord and no pointer route — a chord is an \
                 accelerator, never the only route. Give it a menu row \
                 (shell/templar-shell/src/surface/context_menu.rs) or a region, or add it to \
                 CHORD_WITHOUT_POINTER_ROUTE with a reason."
            );
        }
        for id in &stale_exemptions {
            eprintln!(
                "check-actions: {id} is on CHORD_WITHOUT_POINTER_ROUTE but no longer needs to be \
                 — remove the exemption."
            );
        }
        bail!(
            "{} unreachable verb(s), {} undeclared action(s), {} keyboard-only action(s), {} \
             stale exemption(s)",
            missing.len(),
            undeclared.len(),
            keyboard_only.len(),
            stale_exemptions.len()
        );
    }
    let with_chords = declared.iter().filter(|(_, chord)| *chord).count();
    println!(
        "check-actions: {} CLI verb(s) all reach a subcommand; {} action(s) named by a surface are \
         all declared; {with_chords} action(s) with a chord all have a pointer route ({} exempt)",
        verbs.len(),
        routes.len(),
        exempt.len(),
    );
    Ok(())
}

/// The chord/pointer rule itself, as a pure function over what was parsed: the actions that have
/// an accelerator and no pointer route, and the exemptions that are no longer earning their
/// place. Separated from [`check_actions`]'s I/O so the rule can be tested without a repository.
fn chord_route_problems(
    declared: &[(String, bool)],
    routed: &BTreeSet<&str>,
    exempt: &BTreeMap<&str, &str>,
) -> (Vec<String>, Vec<String>) {
    let mut keyboard_only = Vec::new();
    let mut stale = Vec::new();
    for (id, has_chord) in declared {
        let has_route = routed.contains(id.as_str());
        let is_exempt = exempt.contains_key(id.as_str());
        if *has_chord && !has_route && !is_exempt {
            keyboard_only.push(id.clone());
        }
        // An exemption is a promise that there is no route yet and a chord that needs one. Both
        // halves are checked, so neither a route appearing nor a chord going away can leave a
        // permission to be keyboard-only lying around.
        if is_exempt && (has_route || !*has_chord) {
            stale.push(id.clone());
        }
    }
    for id in exempt.keys() {
        if !declared.iter().any(|(declared_id, _)| declared_id == id) {
            stale.push((*id).to_owned());
        }
    }
    (keyboard_only, stale)
}

/// `(action id, has at least one chord)` for every `ActionDecl` in the manifest source.
///
/// A line scanner rather than a link against `templar_types`: this check must be able to report that
/// the manifest is wrong even when the workspace does not build, which is exactly the state a
/// half-finished action leaves it in.
fn declared_actions(manifest: &str) -> Vec<(String, bool)> {
    let mut actions: Vec<(String, bool)> = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("id: \"") {
            if let Some(id) = rest.split('"').next() {
                actions.push((id.to_owned(), false));
            }
        } else if trimmed.starts_with("chords: &[")
            && trimmed != "chords: &[],"
            && let Some(last) = actions.last_mut()
        {
            // Either `chords: &[Chord::…],` on one line or `chords: &[` opening a list.
            last.1 = true;
        }
    }
    actions
}

/// Every action id a shell surface offers to a pointer, and the files that offer it.
///
/// Test modules are excluded — a test that asserts an action fires is not a route to it, and
/// counting one would let a real route be deleted without this check noticing. The cut is made
/// at the first `#[cfg(test)]` in a file, which is where this repository always puts them.
fn pointer_routes(root: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let mut routes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in POINTER_ROUTE_ROOTS
        .iter()
        .flat_map(|dir| rust_sources(&root.join(dir)))
    {
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let source = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        let shown = relative(root, &path);
        for needle in POINTER_ROUTE_NEEDLES {
            for (index, _) in source.match_indices(needle) {
                let Some(id) = source[index + needle.len()..].split('"').next() else {
                    continue;
                };
                let entry = routes.entry(id.to_owned()).or_default();
                if !entry.contains(&shown) {
                    entry.push(shown.clone());
                }
            }
        }
    }
    Ok(routes)
}

/// Where each private protocol's negotiated version constant is written down. Both ends clamp to
/// their own constant, so both must reach the version the XML declares before a new member can be
/// used — and neither may claim a version the XML does not define.
const PROTOCOL_VERSION_CONSTANTS: &[(&str, &str, &str)] = &[
    (
        "templar_shell_manager_v1",
        "compositor/templar-comp/src/protocols_shell.rs",
        "pub const SHELL_PROTOCOL_VERSION: u32 = ",
    ),
    (
        "templar_shell_manager_v1",
        "shell/templar-shell/src/wayland_bridge.rs",
        "const SHELL_PROTOCOL_VERSION: u32 = ",
    ),
];

/// The private Wayland protocols are append-only and versioned by hand, and nothing else checks
/// them: `check-interface-signatures` reads `interfaces/dbus/*.xml` and never looks here.
///
/// Most of the discipline is enforced by the compiler for free, and deliberately so — the bindings
/// are generated from the XML by `wayland_scanner::generate_server_code!` at compile time, so
/// adding a request or an event breaks every `match` that does not handle it until someone does.
/// What the compiler cannot see is the *numbers*, and this is them:
///
/// * a `since` above its interface's `version` names a member no peer can ever reach;
/// * a toplevel list older than the toplevels it creates silently makes the newer toplevel events
///   unreachable, because objects are created at the list resource's negotiated version;
/// * a version constant above the XML's advertises members that do not exist, and one below it
///   means the addition is not wired up yet.
fn check_wayland_versions(root: &Path) -> Result<()> {
    let dir = root.join("interfaces/wayland");
    let mut versions: BTreeMap<String, u32> = BTreeMap::new();
    let mut problems: Vec<String> = Vec::new();
    let mut members = 0usize;
    let mut toplevel_requests = Vec::new();

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "xml"))
        .collect();
    files.sort();

    for file in &files {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let parsed = wayland_versions::parse(&text)
            .with_context(|| format!("parsing {}", file.display()))?;
        versions.extend(parsed.versions);
        toplevel_requests.extend(parsed.toplevel_requests);
        members += parsed.versioned_members;
        problems.extend(parsed.problems);
    }

    check_shell_object_versions(&versions, &toplevel_requests, &mut problems);

    for (interface, path, needle) in PROTOCOL_VERSION_CONSTANTS {
        let Some(declared) = versions.get(*interface) else {
            problems.push(format!(
                "{interface} is named by {path} but not by any protocol XML"
            ));
            continue;
        };
        let file = root.join(path);
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let Some(value) = text
            .split_once(needle)
            .and_then(|(_, rest)| rest.split(';').next())
            .and_then(|raw| raw.trim().parse::<u32>().ok())
        else {
            problems.push(format!("{path} has no `{needle}<n>;`"));
            continue;
        };
        if value != *declared {
            problems.push(format!(
                "{path} negotiates {interface} at version {value}, but the XML declares {declared} — \
                 bump the constant when wiring the addition up, and never above the XML"
            ));
        }
    }

    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("check-wayland-versions: {problem}");
        }
        bail!("{} Wayland versioning problem(s)", problems.len());
    }
    println!(
        "check-wayland-versions: {} interface(s), {members} versioned member(s), both ends in step",
        versions.len()
    );
    Ok(())
}

fn check_shell_object_versions(
    versions: &BTreeMap<String, u32>,
    requests: &[u32],
    problems: &mut Vec<String>,
) {
    let Some(manager) = versions.get("templar_shell_manager_v1") else {
        problems.push("templar_shell_manager_v1 is missing".into());
        return;
    };
    for interface in [
        "templar_shell_toplevel_list_v1",
        "templar_shell_toplevel_v1",
    ] {
        if versions.get(interface) != Some(manager) {
            problems.push(format!(
                "{interface} must equal manager version {manager}: child objects inherit its negotiated version"
            ));
        }
    }
    for since in requests {
        if !(1..=*manager).contains(since) {
            problems.push(format!(
                "toplevel request since={since} has no corresponding manager protocol version"
            ));
        }
    }
}

#[cfg(test)]
mod shell_version_tests {
    use super::*;

    #[test]
    fn manager_children_equal_and_requests_use_manager_revision_numbers() {
        let mut versions = BTreeMap::from([
            ("templar_shell_manager_v1".into(), 13),
            ("templar_shell_toplevel_list_v1".into(), 13),
            ("templar_shell_toplevel_v1".into(), 13),
        ]);
        let mut problems = Vec::new();
        check_shell_object_versions(&versions, &[1, 12, 13], &mut problems);
        assert!(problems.is_empty());
        // Equal child versions are insufficient when both exceed the manager.
        versions.insert("templar_shell_toplevel_list_v1".into(), 14);
        versions.insert("templar_shell_toplevel_v1".into(), 14);
        check_shell_object_versions(&versions, &[0, 14], &mut problems);
        assert_eq!(problems.len(), 4);
        versions.insert("templar_shell_toplevel_list_v1".into(), 12);
        problems.clear();
        check_shell_object_versions(&versions, &[], &mut problems);
        assert_eq!(problems.len(), 2);
    }
}

/// Every `.rs` file under `dir`, recursively.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
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
        Command::CheckActions => check_actions(&root),
        Command::CheckWaylandVersions => check_wayland_versions(&root),
        Command::Copyright => copyright(&root),
        Command::BuildSet {
            dry_run,
            print_id,
            epoch,
            closure_report,
        } => build_set::run(
            &root,
            &build_set::Options {
                dry_run,
                print_id,
                epoch,
                closure_report,
            },
        ),
        Command::Archive { command } => archive::run(&root, &command),
        Command::InterfaceBaseline => signatures::write_baseline(&root),
        Command::CheckInterfaceSignatures => signatures::check(&root),
        Command::Seeds { action, check, out } => match (action, check) {
            (Some(SeedsAction::Closure), _) => seeds::closure(&root, out.as_deref()),
            (None, true) => seeds::check(&root),
            (None, false) => seeds::generate(&root),
        },
        Command::Components { field } => components(&root, &field),
        Command::IndexBench(args) => index_bench::run(&root, &args),
    }
}

/// One field of every component, one per line. The shell scripts that install or reinstall
/// TemplarOS need the package list and nothing else; letting them ask keeps
/// `distro/deploy/components.toml` the only place it is written down.
fn components(root: &Path, field: &str) -> Result<()> {
    let matrix =
        templar_build::components::Matrix::read(&root.join("distro/deploy/components.toml"))
            .context("reading the component matrix")?;
    for component in &matrix.components {
        let value = match field {
            "name" => component.name.clone(),
            "package" => component.package.clone(),
            "crate" => component.crate_name.clone(),
            "unit" => component.unit.clone().unwrap_or_default(),
            "activation" => format!("{:?}", component.activation),
            "authority" => component.authority.clone(),
            "lifetime" => component
                .lifetime
                .map(|lifetime| format!("{lifetime:?}"))
                .unwrap_or_default(),
            "consumer_interfaces" => component.consumer_interfaces.join(" "),
            "recovery" => component.recovery.clone(),
            "deadlines" => component.deadlines.clone(),
            // One line per exception rather than one per component: this is the field a person
            // reads to answer "what still polls, and why", and a component with three of them
            // has three answers.
            "timer_exceptions" => component
                .timer_exceptions
                .iter()
                .map(|exception| {
                    format!(
                        "{}\t{}\t{}\t{}",
                        component.name, exception.site, exception.interval, exception.reason
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            other => bail!(
                "unknown --field {other:?} (name, package, crate, unit, activation, authority, \
                 lifetime, consumer_interfaces, recovery, deadlines, timer_exceptions)"
            ),
        };
        if !value.is_empty() {
            println!("{value}");
        }
    }
    Ok(())
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

    // Which crates must stay pure, and what "pure" rules out for each. `templar-window` runs on
    // the compositor render thread, so no async runtime and no D-Bus; `templar-field` is shared by
    // the shell and sessiond as plain data and geometry, so the same, plus no toolkit;
    // `templar-treemap` is arithmetic rasterized on any thread, so no toolkit and no GPU either.
    let rules: [(&str, &[&str]); 3] = [
        ("templar-window", &["tokio", "zbus", "async-std", "smol"]),
        (
            "templar-treemap",
            &[
                "tokio",
                "zbus",
                "async-std",
                "smol",
                "pixelkit-raster",
                "pixelkit-text",
                "pixelkit-ui",
                "pixelkit-shell",
                "pixelkit-windowing",
                "winit",
                "wgpu",
                "vello",
            ],
        ),
        (
            "templar-field",
            &[
                "tokio",
                "zbus",
                "async-std",
                "smol",
                "pixelkit-raster",
                "pixelkit-text",
                "pixelkit-ui",
                "pixelkit-shell",
                "pixelkit-windowing",
                "winit",
            ],
        ),
    ];
    for (crate_name, banned) in rules {
        let node = resolve
            .iter()
            .find(|node| name_of(node["id"].as_str().unwrap_or_default()) == crate_name)
            .with_context(|| format!("{crate_name} not in resolve graph"))?;

        // Transitive closure of the crate's normal dependencies.
        let mut stack: Vec<String> = node["deps"]
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
        let offenders: Vec<String> = seen
            .iter()
            .map(|id| name_of(id))
            .filter(|name| banned.contains(&name.as_str()))
            .collect();
        if !offenders.is_empty() {
            bail!("deps-check: {crate_name} depends on {offenders:?}");
        }
        println!(
            "deps-check: {crate_name} is free of async runtimes, D-Bus and toolkits ({} transitive deps)",
            seen.len()
        );
    }
    Ok(())
}

/// Every zbus interface name in the code has an XML file in `interfaces/dbus/`.
///
/// Two independent passes, reported separately, because they check two different things that
/// happen to share one contract file:
///   - *server* names come from `#[zbus::interface(name = "...")]`, the trait a daemon actually
///     serves on the bus.
///   - *client* names come from `#[proxy(interface = "...")]` / `#[zbus::proxy(interface =
///     "...")]`, the typed proxies `templar-client` hands to every consumer.
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
    // `cli/` only calls into `templar-client`'s already-scanned proxies and declares no interface
    // name of its own today (checked: no `interface` literal anywhere under `cli/templar/src/`), so
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

/// Walks `dir` recursively, adding every `org.templaros.*` server interface name it finds to
/// `server` and every `org.templaros.*` client (proxy) interface name to `client`.
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

/// Server-side interface names: `#[zbus::interface(name = "org.templaros...")]`.
fn server_interface_names(text: &str) -> BTreeSet<String> {
    attribute_string_args(text, &["#[zbus::interface("], "name")
}

/// Client-side interface names: `#[proxy(interface = "org.templaros...")]` and its
/// `#[zbus::proxy(...)]` spelling.
fn client_interface_names(text: &str) -> BTreeSet<String> {
    attribute_string_args(text, &["#[proxy(", "#[zbus::proxy("], "interface")
}

/// Every `org.templaros.*` value of the `key = "..."` argument of any attribute opened by one of
/// `openers`.
///
/// Deliberately not a per-line scan with a fixed-spacing marker. Three shapes a line scan gets
/// wrong, each of which turns a *green* check into one that is checking nothing:
///   * the attribute spanning several lines (`#[proxy(` on one, `interface = "..."` on the next),
///     which is how every proxy in `crates/templar-client` is actually written;
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
                && value.starts_with("org.templaros.")
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

/// Every client `#[proxy(interface = "org.templaros...")]` must name an interface some
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
/// (templaros-sessiond, templaros-resourced, templaros-filed, templaros-hostd, templaros-boot-health,
/// templaros-session, templaros-shell, templaros-settings, templaros-portal, templaros-cli): every one of them
/// is built from exactly one `cargo-deb`-packaged crate and already carries a
/// `[package.metadata.deb]` table whose `name` matches its `Package:` stanza — including
/// templaros-session and templaros-boot-health, which read as though they might be hand-assembled
/// metapackages but are each an ordinary single-binary crate (`session/templar-session`,
/// `services/templar-boot-health`) with its own `[package.metadata.deb]`. So this list is empty
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

    let DebMetadata {
        deb_name_to_crate,
        deb_depends,
        deb_assets,
        maintainerless,
        missing_asset_sources,
    } = deb_metadata(packages);

    let mut problems = packaging_problems(&control_packages, &deb_name_to_crate);
    let matrix =
        templar_build::components::Matrix::read(&root.join("distro/deploy/components.toml"))
            .context("reading the component matrix")?;
    problems.extend(component_matrix_problems(
        &matrix,
        &control_packages,
        &deb_name_to_crate,
        &unit_file_names(root),
    ));
    problems.extend(maintainerless);
    problems.extend(missing_asset_sources);
    problems.extend(garden_hook_delivery_problems(packages));
    problems.extend(dependency_drift(
        &parse_control_depends(&control_text),
        &deb_depends,
        &deb_name_to_crate,
    ));
    problems.extend(service_inventory_problems(
        &matrix,
        &unit_types(root)?,
        &dbus_activated_units(root)?,
        &session_wanted_units(root)?,
    ));
    problems.extend(portal_backend_problems(root, &dbus_activated_names(root)?)?);
    let units = unit_exec_starts(root)?;
    let shipped = shipped_destinations(root, &deb_assets)?;
    let installed: BTreeSet<String> = shipped.iter().map(|(path, _)| path.clone()).collect();
    problems.extend(unshipped_unit_binaries(&units, &installed));
    problems.extend(colliding_destinations(&shipped));
    problems.extend(dangling_manifest_executables(
        root,
        &installed,
        &fork_packages(root)?,
    )?);
    let seeds = read_seeds(root)?;
    let built = built_package_names(root, &control_packages)?;
    problems.extend(seed_problems(&seeds, &built));
    let metapackages = metapackage_controls(root)?;
    problems.extend(metapackage_dependency_problems(&metapackages, &built));
    // The seeds, the image package lists and those metapackage control files are all rendered from
    // distro/seeds/selection.toml. Checking the names in them is only half the job: a hand edit to
    // a generated file survives every other check here and then loses the next time anyone runs
    // `cargo xtask seeds`.
    problems.extend(seeds::drift(root)?);
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("check-packaging: {problem}");
        }
        bail!("{} packaging mismatch(es)", problems.len());
    }

    println!(
        "check-packaging: {} debian/control package(s) all have matching crate metadata; \
         {} component matrix row(s) agree with them; \
         {} unit ExecStart binary/ies all installed by a package; \
         {} service(s) declare an authority, lifetime, recovery, deadline and every timer \
         exception; \
         {} seed(s) and {} metapackage(s) name only packages that exist",
        control_packages.len(),
        matrix.components.len(),
        units.len(),
        matrix
            .components
            .iter()
            .filter(|component| component.unit.is_some())
            .count(),
        seeds.len(),
        metapackages.len()
    );
    Ok(())
}

/// The path the garden's terminal prompt hook is shipped to, and the one file a graphical
/// terminal's shell will actually read it from.
const GARDEN_HOOK_PAYLOAD: &str = "etc/profile.d/60-templaros-garden.sh";
const GARDEN_HOOK_DELIVERY: &str = "/etc/bash.bashrc";

/// The garden's terminal hook has to arrive in a file bash reads for an *interactive, non-login*
/// shell, because that is the only kind a graphical terminal spawns: kitty runs the login wrapper
/// on macOS only. `/etc/profile.d` is read by `/etc/profile`, and `/etc/profile` is read by login
/// shells alone — so the hook was packaged, installed, unit-tested and never once loaded, and the
/// garden heard nothing from a full day at the terminal.
///
/// `/etc/bash.bashrc` is the one system-wide file that qualifies. It belongs to `base-files`, so
/// the package appends a marker-guarded line to it from its maintainer scripts rather than
/// shipping a file over it; this is what fails if that delivery is ever lost again.
fn garden_hook_delivery_problems(packages: &[serde_json::Value]) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(package) = packages.iter().find(|package| {
        package["metadata"]["deb"]["assets"]
            .as_array()
            .is_some_and(|assets| {
                assets.iter().any(|asset| {
                    asset.get(1).and_then(serde_json::Value::as_str) == Some(GARDEN_HOOK_PAYLOAD)
                })
            })
    }) else {
        problems.push(format!(
            "no package ships {GARDEN_HOOK_PAYLOAD}: the garden's terminal source has no payload"
        ));
        return problems;
    };
    let deb_name = package["metadata"]["deb"]["name"].as_str().unwrap_or("?");
    let manifest_dir = Path::new(package["manifest_path"].as_str().unwrap_or(""))
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_owned();
    let Some(scripts) = package["metadata"]["deb"]["maintainer-scripts"].as_str() else {
        problems.push(format!(
            "{deb_name} ships {GARDEN_HOOK_PAYLOAD} but declares no maintainer-scripts, so \
             nothing puts it in {GARDEN_HOOK_DELIVERY}; only a login shell would ever read it \
             and a graphical terminal never starts one"
        ));
        return problems;
    };
    let scripts = manifest_dir.join(scripts);
    for (name, must_mention) in [
        ("postinst", GARDEN_HOOK_PAYLOAD),
        ("postrm", "templaros garden hook"),
    ] {
        let path = scripts.join(name);
        match std::fs::read_to_string(&path) {
            Ok(text) if text.contains(GARDEN_HOOK_DELIVERY) && text.contains(must_mention) => {}
            Ok(_) => problems.push(format!(
                "{deb_name}: {} does not mention both {GARDEN_HOOK_DELIVERY} and {must_mention}, \
                 so the terminal hook's delivery is not what it claims",
                path.display()
            )),
            Err(error) => problems.push(format!(
                "{deb_name}: cannot read {} ({error}), so the terminal hook has no delivery",
                path.display()
            )),
        }
    }
    problems
}

/// Everything `check-packaging` needs out of `[package.metadata.deb]`, gathered in one pass.
struct DebMetadata {
    deb_name_to_crate: BTreeMap<String, String>,
    deb_depends: BTreeMap<String, BTreeSet<String>>,
    deb_assets: Vec<(String, String)>,
    maintainerless: Vec<String>,
    missing_asset_sources: Vec<String>,
}

fn deb_metadata(packages: &[serde_json::Value]) -> DebMetadata {
    // cargo metadata's `metadata` field is cargo's own parse of `[package.metadata.*]`, so this
    // reads the same `[package.metadata.deb]` table `cargo deb` reads — never a hand-rolled scan
    // of Cargo.toml that could drift from what cargo-deb actually sees.
    let mut deb_name_to_crate = BTreeMap::new();
    let mut deb_depends = BTreeMap::new();
    let mut deb_assets = Vec::new();
    let mut maintainerless = Vec::new();
    let mut missing_asset_sources = Vec::new();
    for package in packages {
        if let Some(deb_name) = package["metadata"]["deb"]["name"].as_str() {
            let crate_name = package["name"].as_str().unwrap_or("?");
            deb_name_to_crate.insert(deb_name.to_owned(), crate_name.to_owned());
            // cargo-deb's `depends` defaults to `$auto` when the key is absent.
            let depends = package["metadata"]["deb"]["depends"]
                .as_str()
                .unwrap_or("$auto");
            deb_depends.insert(deb_name.to_owned(), parse_depends_list(depends));
            // Maintainer is mandatory in a binary package's control file. cargo-deb fills it from
            // `[package.metadata.deb] maintainer` or, failing that, the crate's `authors`; with
            // neither it emits a package dpkg itself warns about. Nine of ours were built that
            // way, because `authors.workspace = true` had only ever been added to two crates.
            let has_maintainer = package["metadata"]["deb"]["maintainer"].is_string()
                || package["authors"]
                    .as_array()
                    .is_some_and(|authors| !authors.is_empty());
            if !has_maintainer {
                maintainerless.push(format!(
                    "{deb_name} (crate {crate_name}): no maintainer and no authors, so the .deb \
                     would have no Maintainer field"
                ));
            }
        }
        if let Some(assets) = package["metadata"]["deb"]["assets"].as_array() {
            let deb_name = package["metadata"]["deb"]["name"]
                .as_str()
                .unwrap_or_else(|| package["name"].as_str().unwrap_or("?"));
            // cargo-deb resolves a static asset source relative to the crate's own directory,
            // not the workspace root — a mistake `cargo build` never surfaces, only `cargo deb`
            // at packaging time. Build outputs under target/ don't exist until the build runs,
            // and glob sources expand at packaging time, so only literal static files are checked.
            let manifest_dir = Path::new(package["manifest_path"].as_str().unwrap_or(""))
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_owned();
            for asset in assets {
                if let Some(destination) = asset.get(1).and_then(serde_json::Value::as_str) {
                    deb_assets.push((deb_name.to_owned(), destination.to_owned()));
                }
                if let Some(source) = asset.get(0).and_then(serde_json::Value::as_str)
                    && !source.starts_with("target/")
                    && !source.contains(['*', '?', '['])
                    && !manifest_dir.join(source).exists()
                {
                    missing_asset_sources.push(format!(
                        "{deb_name}: asset source {source} does not exist \
                         (cargo-deb resolves it relative to {})",
                        manifest_dir.display()
                    ));
                }
            }
        }
    }

    DebMetadata {
        deb_name_to_crate,
        deb_depends,
        deb_assets,
        maintainerless,
        missing_asset_sources,
    }
}

/// Every unit file this repository ships, by file name.
fn unit_file_names(root: &Path) -> BTreeSet<String> {
    ["system", "user"]
        .iter()
        .flat_map(|scope| std::fs::read_dir(root.join("session/units").join(scope)))
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

/// The component matrix must name exactly the build set, and name it correctly.
///
/// `distro/deploy/components.toml` is what `cargo xtask build-set` builds, what the reinstall
/// script installs, what `templar doctor` checks for coherence and what `templar dev` activates. Four
/// consumers of one list is only an improvement over four copies of it if the list cannot be
/// wrong, so: every `debian/control` package is a matrix row, every matrix row is a
/// `debian/control` package, and every row's `crate` is the crate that actually builds that
/// package according to `[package.metadata.deb]`.
fn component_matrix_problems(
    matrix: &templar_build::components::Matrix,
    control_packages: &[String],
    deb_name_to_crate: &BTreeMap<String, String>,
    unit_files: &BTreeSet<String>,
) -> Vec<String> {
    let mut problems = matrix
        .problems()
        .into_iter()
        .map(|problem| format!("distro/deploy/components.toml: {problem}"))
        .collect::<Vec<_>>();
    let rows: BTreeMap<&str, &str> = matrix
        .components
        .iter()
        .map(|component| (component.package.as_str(), component.crate_name.as_str()))
        .collect();
    for package in control_packages {
        if !rows.contains_key(package.as_str()) {
            problems.push(format!(
                "{package}: in debian/control but not in distro/deploy/components.toml, so it \
                 would be left out of every build set"
            ));
        }
    }
    for component in &matrix.components {
        if let Some(unit) = &component.unit
            && !unit_files.contains(unit)
        {
            problems.push(format!(
                "{}: the component matrix names the unit {unit}, which is not in session/units/",
                component.name
            ));
        }
    }
    for (package, crate_name) in &rows {
        if !control_packages.iter().any(|p| p == package) {
            problems.push(format!(
                "{package}: a component matrix row with no debian/control Package: stanza"
            ));
            continue;
        }
        match deb_name_to_crate.get(*package) {
            Some(actual) if actual == crate_name => {}
            Some(actual) => problems.push(format!(
                "{package}: the component matrix says crate {crate_name}, but \
                 [package.metadata.deb] says {actual}"
            )),
            None => problems.push(format!(
                "{package}: the component matrix names crate {crate_name}, which has no \
                 [package.metadata.deb] table"
            )),
        }
    }
    problems
}

/// Compare `debian/control`'s `Package:` stanzas against the `[package.metadata.deb] name` of
/// every workspace crate, in both directions. Pure, so both directions are testable against
/// literal fixtures without a `debian/` tree or a `cargo metadata` subprocess.
///
/// Forward: a `Package:` with no crate to build it is a package `debian/rules` expects and
/// `cargo deb` will never produce. Reverse: a crate whose deb name matches no stanza builds a
/// `.deb` nothing installs — the same typo, caught from the other side.
/// Whether some package provides a manifest's executable: installed by a package here, or named
/// by a remaining fork whose file list this repository cannot see.
fn manifest_executable_is_provided(
    executable: &str,
    installed: &BTreeSet<String>,
    fork_packages: &BTreeSet<String>,
) -> bool {
    let basename = executable.rsplit('/').next().unwrap_or(executable);
    installed.contains(executable) || fork_packages.contains(basename)
}

/// Application manifests naming an executable that nothing provides.
///
/// A manifest in `distro/app-manifests/` is shipped to `/usr/share/templaros/apps/` and registers its
/// MIME types, so it is a live promise that opening one of those files will launch that binary.
/// `org.templaros.Viewer.json` was a test fixture that had been copied into the packaged directory:
/// byte-identical to `tests/fixtures/manifests/`, naming `/usr/bin/templar-viewer` for a crate that
/// has never existed, and claiming `application/pdf`. Installed, it would have become the
/// registered PDF handler and every PDF would have failed to open — against a system whose fourth
/// stated claim is that file opening is predictable.
///
/// An executable is provided if some package here installs it, or if its name is a remaining
/// fork's package. `templar-files` is first-party and therefore takes the ordinary installed-file
/// path through this check. A curated manifest for a third-party application the user has not
/// installed is a runtime observation, not a build error — `templar doctor` warns about those.
fn dangling_manifest_executables(
    root: &Path,
    installed: &BTreeSet<String>,
    fork_packages: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let dir = root.join("distro/app-manifests");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(&dir).context("reading distro/app-manifests")? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let manifest: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let Some(executable) = manifest["executable"].as_str() else {
            continue;
        };
        if manifest_executable_is_provided(executable, installed, fork_packages) {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        problems.push(format!(
            "distro/app-manifests/{name}: executable {executable} is installed by no package and \
             is not a fork's package"
        ));
    }
    problems.sort();
    Ok(problems)
}

/// Destinations that more than one package installs.
///
/// `dpkg` refuses this at unpack time — "trying to overwrite X, which is also in package Y" — so
/// it is an install-blocking error found at the worst moment, on a user's machine, halfway
/// through. It is trivially detectable here: templaros-session and templaros-comp both shipped
/// `templar-comp.service` for exactly as long as it took to notice.
///
/// Only genuinely different packages count. The same file appearing in both a package's
/// `debian/*.install` and its own cargo-deb assets is the two build paths agreeing, which is
/// what they are supposed to do.
fn colliding_destinations(shipped: &[(String, String)]) -> Vec<String> {
    let mut by_destination: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (destination, package) in shipped {
        by_destination
            .entry(destination)
            .or_default()
            .insert(package);
    }
    by_destination
        .into_iter()
        .filter(|(_, packages)| packages.len() > 1)
        .map(|(destination, packages)| {
            let packages: Vec<&str> = packages.into_iter().collect();
            format!(
                "{destination}: installed by {} — dpkg refuses to unpack two packages over the \
                 same path",
                packages.join(" and ")
            )
        })
        .collect()
}

/// `(path, text)` for every `distro/metapackages/*/control.in`.
fn metapackage_controls(root: &Path) -> Result<Vec<(String, String)>> {
    let mut controls = Vec::new();
    let dir = root.join("distro/metapackages");
    if !dir.is_dir() {
        return Ok(controls);
    }
    for entry in std::fs::read_dir(&dir).context("reading distro/metapackages")? {
        let control = entry?.path().join("control.in");
        if control.is_file() {
            let text = std::fs::read_to_string(&control)
                .with_context(|| format!("reading {}", control.display()))?;
            controls.push((
                control
                    .strip_prefix(root)
                    .unwrap_or(&control)
                    .display()
                    .to_string(),
                text,
            ));
        }
    }
    Ok(controls)
}

/// TemplarOS packages named in a metapackage's `Depends:`/`Recommends:` that nothing builds.
///
/// The seeds are one list of package names; a metapackage's dependency list is the other, and it
/// had the same two wrong names in it. Substitution variables (`${misc:Depends}`) and version
/// constraints are stripped before comparing.
fn metapackage_dependency_problems(
    controls: &[(String, String)],
    built: &BTreeSet<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, text) in controls {
        // A folded continuation line only continues the field above it, so whether an indented
        // line is a dependency list depends on what came before. Without tracking that, a
        // Description that mentions "templar-hostd's transactional path" reads as a dependency.
        let mut in_dependency_field = false;
        for line in text.lines() {
            let body = match line.split_once(':') {
                Some(("Depends" | "Recommends" | "Suggests", rest)) => {
                    in_dependency_field = true;
                    rest
                }
                // Any other field header ends the previous field.
                Some((field, _)) if !field.starts_with(char::is_whitespace) => {
                    in_dependency_field = false;
                    continue;
                }
                _ if line.starts_with(' ') && in_dependency_field => line,
                _ => {
                    if !line.starts_with(' ') {
                        in_dependency_field = false;
                    }
                    continue;
                }
            };
            for dependency in body.split(',') {
                let name = dependency.split_whitespace().next().unwrap_or_default();
                if name.starts_with("templar") && !built.contains(name) {
                    problems.push(format!(
                        "{path}: depends on TemplarOS package {name}, which nothing in this \
                         repository builds"
                    ));
                }
            }
        }
    }
    problems
}

/// Seed entries naming a TemplarOS package that nothing in this repository builds.
///
/// The seeds are the image's package list, so a name that is almost right — `templar-comp` where the
/// package is `templaros-comp` — is not a typo that shows up at build time here; it shows up as an
/// unresolvable dependency when someone builds the image, far from the file that caused it. Only
/// `templar`-prefixed entries are checked: the rest are Ubuntu's and not ours to know about.
///
/// Three things build a TemplarOS package, and a seed may name any of them: `debian/control`, a
/// metapackage directory under `distro/metapackages/`, and a fork's `package` in
/// `distro/forks/manifest.toml`.
fn seed_problems(seeds: &BTreeMap<String, Vec<String>>, built: &BTreeSet<String>) -> Vec<String> {
    let mut problems = Vec::new();
    for (seed, entries) in seeds {
        for entry in entries {
            if entry.starts_with("templar") && !built.contains(entry) {
                problems.push(format!(
                    "{seed}: names TemplarOS package {entry}, which nothing in this repository builds \
                     (not debian/control, distro/metapackages/ or a fork's package=)"
                ));
            }
        }
    }
    problems
}

/// Every TemplarOS package name this repository produces, from all three sources.
fn built_package_names(root: &Path, control_packages: &[String]) -> Result<BTreeSet<String>> {
    let mut built: BTreeSet<String> = control_packages.iter().cloned().collect();
    // Read each metapackage's own `Package:` rather than its directory name — they differ:
    // `distro/metapackages/templaros-session-meta/` builds `templaros-session-full`.
    for (_, text) in metapackage_controls(root)? {
        built.extend(parse_control_packages(&text));
    }
    built.extend(fork_packages(root)?);
    Ok(built)
}

/// The `package = ` name of every fork in `distro/forks/manifest.toml`.
fn fork_packages(root: &Path) -> Result<BTreeSet<String>> {
    let mut packages = BTreeSet::new();
    let manifest = root.join("distro/forks/manifest.toml");
    if manifest.is_file() {
        let text = std::fs::read_to_string(&manifest).context("reading forks manifest")?;
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("package") {
                let value = rest.trim_start_matches([' ', '=']).trim().trim_matches('"');
                if !value.is_empty() {
                    packages.insert(value.to_owned());
                }
            }
        }
    }
    Ok(packages)
}

/// `(seed file name, package names)` for every `distro/seeds/*.seed`.
fn read_seeds(root: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let mut seeds = BTreeMap::new();
    let dir = root.join("distro/seeds");
    if !dir.is_dir() {
        return Ok(seeds);
    }
    for entry in std::fs::read_dir(&dir).context("reading distro/seeds")? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("seed") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let entries = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(ToOwned::to_owned)
            .collect();
        seeds.insert(
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_owned(),
            entries,
        );
    }
    Ok(seeds)
}

/// Every destination path the two packaging paths install, as absolute-looking `/usr/...` strings.
///
/// `debian/*.install` lines are `<source> <destination-directory>`, so the installed path is the
/// directory plus the source's file name; `[package.metadata.deb]` assets name their destination
/// outright. Both are folded into one set because both are real build paths and a binary shipped
/// by only one of them is still a divergence between what `just deb` and `dpkg-buildpackage`
/// produce.
fn shipped_destinations(
    root: &Path,
    deb_assets: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    let mut shipped: Vec<(String, String)> = deb_assets
        .iter()
        .map(|(package, destination)| {
            (
                format!("/{}", destination.trim_start_matches('/')),
                package.clone(),
            )
        })
        .collect();
    let debian = root.join("debian");
    if debian.is_dir() {
        for entry in std::fs::read_dir(&debian).context("reading debian/")? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("install") {
                continue;
            }
            let package = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_owned();
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            for line in text.lines() {
                let mut fields = line.split_whitespace();
                let (Some(source), Some(destination)) = (fields.next(), fields.next()) else {
                    continue;
                };
                // A glob in an install line names whatever matches at build time, which this
                // cannot resolve; skip rather than invent a file name.
                if source.contains('*') || source.contains('?') {
                    continue;
                }
                let name = source.rsplit('/').next().unwrap_or(source);
                shipped.push((
                    format!(
                        "/{}/{name}",
                        destination.trim_start_matches('/').trim_end_matches('/')
                    ),
                    package.clone(),
                ));
            }
        }
    }
    Ok(shipped)
}

/// Units whose `ExecStart=` binary no package installs.
///
/// This is the leg `packaging_problems` cannot cover. That one checks `debian/control` and the
/// crate metadata against *each other*, so a binary missing from both looks perfectly consistent —
/// which is exactly how `templar-comp` came to have a unit pointing at `/usr/bin/templar-comp` that no
/// package shipped. A unit is a promise that a binary will be at a path; this checks the promise
/// against what is actually installed there.
fn unshipped_unit_binaries(units: &[(String, String)], shipped: &BTreeSet<String>) -> Vec<String> {
    units
        .iter()
        .filter(|(_, binary)| !shipped.contains(binary.as_str()))
        .map(|(unit, binary)| {
            format!("{unit}: ExecStart={binary}, but no package installs that path")
        })
        .collect()
}

/// Every service names its authority, its lifetime, its recovery and its deadlines, and its
/// declared lifetime is supported by the way it is started.
///
/// `@templaros.decision.event-driven-service-authorities` requires that "each persistent daemon and
/// polling exception records its authority, lifetime, activation, recovery, deadline and
/// justification". A hand-written inventory document goes stale the first time a unit changes;
/// this is the same statement made where the build can check it.
///
/// `Type=dbus` only tells systemd when a service is ready. Actual D-Bus activation requires a
/// descriptor naming the unit, and an eagerly wanted unit is session-bound even if it uses the
/// bus name as its readiness condition.
fn service_inventory_problems(
    matrix: &templar_build::components::Matrix,
    unit_types: &BTreeMap<String, String>,
    dbus_activated: &BTreeSet<String>,
    session_wanted: &BTreeSet<String>,
) -> Vec<String> {
    use templar_build::components::Lifetime;

    let mut problems = Vec::new();
    for component in &matrix.components {
        let Some(unit) = &component.unit else {
            continue;
        };
        let name = &component.name;
        for (field, value) in [
            ("authority", &component.authority),
            ("recovery", &component.recovery),
            ("deadlines", &component.deadlines),
        ] {
            if value.trim().is_empty() {
                problems.push(format!(
                    "{name}: has the unit {unit} but declares no {field}; every service records \
                     one (@templaros.decision.event-driven-service-authorities)"
                ));
            }
        }
        let Some(lifetime) = component.lifetime else {
            problems.push(format!(
                "{name}: has the unit {unit} but declares no lifetime (dbus-activated, \
                 session-bound or oneshot)"
            ));
            continue;
        };
        let Some(unit_type) = unit_types.get(unit) else {
            problems.push(format!("{name}: {unit} declares no Type="));
            continue;
        };
        match lifetime {
            Lifetime::Oneshot if unit_type != "oneshot" => problems.push(format!(
                "{name}: the matrix says lifetime = Oneshot, but {unit} is Type={unit_type}"
            )),
            Lifetime::DbusActivated if unit_type != "dbus" => problems.push(format!(
                "{name}: the matrix says lifetime = DbusActivated, but {unit} is Type={unit_type}; \
                 D-Bus readiness requires Type=dbus"
            )),
            Lifetime::DbusActivated if !dbus_activated.contains(unit) => problems.push(format!(
                "{name}: the matrix says lifetime = DbusActivated, but no D-Bus service \
                 descriptor names SystemdService={unit}"
            )),
            Lifetime::DbusActivated if session_wanted.contains(unit) => problems.push(format!(
                "{name}: the matrix says lifetime = DbusActivated, but templaros-session.target \
                 eagerly Wants={unit}; declare it session-bound or stop eager activation"
            )),
            Lifetime::SessionBound if unit_type == "oneshot" => problems.push(format!(
                "{name}: the matrix says lifetime = SessionBound, but {unit} is Type=oneshot"
            )),
            Lifetime::Oneshot | Lifetime::DbusActivated | Lifetime::SessionBound => {}
        }
    }
    problems
}

/// Every portal backend this repository declares is actually startable.
///
/// A `.portal` file only tells `xdg-desktop-portal` that a backend exists and which interfaces it
/// implements. Starting it is D-Bus activation, and that needs a service descriptor claiming the
/// same name. Shipping the first without the second is silent: the frontend chooses the backend,
/// every call to it fails with `ServiceUnknown`, and the person sees a file chooser that never
/// opens. That is exactly what shipped, so it is checked here.
fn portal_backend_problems(root: &Path, activatable: &BTreeSet<String>) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let portals = root.join("portals");
    let mut declarations = Vec::new();
    for backend in
        std::fs::read_dir(&portals).with_context(|| format!("reading {}", portals.display()))?
    {
        let backend = backend?.path();
        if !backend.is_dir() {
            continue;
        }
        for entry in
            std::fs::read_dir(&backend).with_context(|| format!("reading {}", backend.display()))?
        {
            declarations.push(entry?.path());
        }
    }
    for entry in declarations {
        if entry.extension().and_then(|extension| extension.to_str()) != Some("portal") {
            continue;
        }
        let text = std::fs::read_to_string(&entry)
            .with_context(|| format!("reading {}", entry.display()))?;
        let Some(name) = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("DBusName="))
            .map(str::trim)
        else {
            problems.push(format!("{}: declares no DBusName=", entry.display()));
            continue;
        };
        if !activatable.contains(name) {
            problems.push(format!(
                "{}: nothing activates {name}; ship a D-Bus service descriptor claiming that name",
                entry.display()
            ));
        }
    }
    Ok(problems)
}

/// Bus names claimed by repository-owned D-Bus activation descriptors.
fn dbus_activated_names(root: &Path) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    let dir = root.join("distro/policy/dbus");
    if !dir.is_dir() {
        return Ok(names);
    }
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("service") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if let Some(name) = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("Name="))
        {
            names.insert(name.trim().to_owned());
        }
    }
    Ok(names)
}

/// Units named by repository-owned D-Bus activation descriptors.
fn dbus_activated_units(root: &Path) -> Result<BTreeSet<String>> {
    let mut units = BTreeSet::new();
    let dir = root.join("distro/policy/dbus");
    if !dir.is_dir() {
        return Ok(units);
    }
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("service") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if let Some(unit) = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("SystemdService="))
        {
            units.insert(unit.trim().to_owned());
        }
    }
    Ok(units)
}

/// Services eagerly started by the graphical session target.
fn session_wanted_units(root: &Path) -> Result<BTreeSet<String>> {
    let path = root.join("session/units/user/templaros-session.target");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Wants="))
        .flat_map(str::split_whitespace)
        .map(str::to_owned)
        .collect())
}

/// `Type=` for every unit under `session/units/`. A unit with no `Type=` is `simple`, which is
/// what systemd itself assumes.
fn unit_types(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut types = BTreeMap::new();
    let mut stack = vec![root.join("session/units")];
    while let Some(dir) = stack.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("service") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_owned();
            let kind = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("Type="))
                .unwrap_or("simple")
                .trim()
                .to_owned();
            types.insert(name, kind);
        }
    }
    Ok(types)
}

/// `(unit file name, ExecStart binary)` for every unit under `session/units/`.
fn unit_exec_starts(root: &Path) -> Result<Vec<(String, String)>> {
    let mut units = Vec::new();
    let mut stack = vec![root.join("session/units")];
    while let Some(dir) = stack.pop() {
        if !dir.is_dir() {
            continue;
        }
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("service") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_owned();
            for line in text.lines() {
                if let Some(command) = line.trim().strip_prefix("ExecStart=") {
                    // Strip systemd's `-`/`@`/`+`/`!` prefixes and any arguments.
                    let command = command.trim_start_matches(['-', '@', '+', '!']);
                    if let Some(binary) = command.split_whitespace().next()
                        && binary.starts_with('/')
                    {
                        units.push((name.clone(), binary.to_owned()));
                    }
                }
            }
        }
    }
    units.sort();
    Ok(units)
}

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

/// The `Depends:` of every `debian/control` stanza, keyed by `Package:`, with the substitution
/// variables (`${shlibs:Depends}`, `${misc:Depends}`) dropped — they are the control-file
/// spelling of cargo-deb's `$auto`, so neither side keeps them. A stanza with no `Depends:` maps
/// to the empty set.
fn parse_control_depends(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut out = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Package:") {
            let name = rest.trim().to_owned();
            out.entry(name.clone()).or_default();
            current = Some(name);
        } else if let Some(rest) = line.strip_prefix("Depends:")
            && let Some(name) = &current
        {
            out.insert(name.clone(), parse_depends_list(rest));
        }
    }
    out
}

/// One comma-separated dependency list, as either `debian/control` or cargo-deb writes it, as
/// the set of its entries with `$auto` and `${...}` substitution variables removed. Alternatives
/// (`a | b`) stay one entry with single spaces around the bar, so the two spellings compare equal.
fn parse_depends_list(list: &str) -> BTreeSet<String> {
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty() && *entry != "$auto" && !entry.starts_with("${"))
        .map(|entry| {
            entry
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect()
}

/// Packages whose `debian/control` `Depends:` and `[package.metadata.deb] depends` name
/// different packages.
///
/// The two are written by hand in two files and nothing else ties them together: `debian/rules`
/// builds from `debian/control`, `cargo deb` builds from the crate table, and a release uses one
/// or the other. `templaros-hostd` shipped with `policykit-1` in the crate table after the control
/// file had moved to `polkitd` — the transitional package no longer exists on the 25.10/26.04
/// base, so the `.deb` could not be configured at all.
fn dependency_drift(
    control_depends: &BTreeMap<String, BTreeSet<String>>,
    deb_depends: &BTreeMap<String, BTreeSet<String>>,
    deb_name_to_crate: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (deb_name, from_deb) in deb_depends {
        let Some(from_control) = control_depends.get(deb_name) else {
            continue; // reported by `packaging_problems`
        };
        if from_control != from_deb {
            let crate_name = deb_name_to_crate.get(deb_name).map_or("?", String::as_str);
            let only_control: Vec<_> = from_control.difference(from_deb).cloned().collect();
            let only_deb: Vec<_> = from_deb.difference(from_control).cloned().collect();
            problems.push(format!(
                "{deb_name} (crate {crate_name}): Depends differ — only in debian/control: \
                 {only_control:?}; only in [package.metadata.deb] depends: {only_deb:?}"
            ));
        }
    }
    problems
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
            // Every dot-directory is somebody's private working area, not shipped source —
            // `.claude/worktrees/` in particular holds whole sibling-repo checkouts.
            if name.starts_with('.')
                || matches!(&*name, "target" | "vendor" | "forks" | "node_modules")
            {
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
        "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\nUpstream-Name: templaros\n"
    );
    for package in metadata["packages"].as_array().unwrap_or(&vec![]) {
        println!(
            "Files: {}/*\nCopyright: 2026 TemplarOS contributors\nLicense: {}\n",
            package["manifest_path"]
                .as_str()
                .unwrap_or("?")
                .trim_end_matches("/Cargo.toml")
                .trim_start_matches(&*root.to_string_lossy())
                .trim_start_matches('/'),
            package["license"].as_str().unwrap_or("AGPL-3.0-only")
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
        GARDEN_HOOK_PAYLOAD, VendoredAssets, chord_route_problems, client_interface_names,
        colliding_destinations, declared_actions, dependency_drift, garden_hook_delivery_problems,
        manifest_executable_is_provided, metapackage_dependency_problems, packaging_problems,
        parse_control_depends, parse_control_packages, parse_depends_list, seed_problems,
        server_interface_names, service_inventory_problems, unlicensed_assets,
        unshipped_unit_binaries,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    const MANIFEST_SHAPE: &str = r#"
        ActionDecl {
            id: "home.reveal",
            chords: &[
                Chord::bare(Key::LogoTap),
                Chord::new(Modifiers::CTRL_ALT, Key::Character('h')),
            ],
        },
        ActionDecl {
            id: "field.undo",
            chords: &[Chord::new(Modifiers::CTRL, Key::Character('z'))],
        },
        ActionDecl {
            id: "session.lock",
            chords: &[],
        },
    "#;

    #[test]
    fn the_manifest_scanner_sees_one_line_and_multi_line_chord_lists() {
        assert_eq!(
            declared_actions(MANIFEST_SHAPE),
            vec![
                ("home.reveal".to_owned(), true),
                ("field.undo".to_owned(), true),
                ("session.lock".to_owned(), false),
            ]
        );
    }

    #[test]
    fn an_action_with_a_chord_and_no_pointer_route_is_reported() {
        let declared = declared_actions(MANIFEST_SHAPE);
        let routed = BTreeSet::from(["home.reveal"]);
        let (keyboard_only, stale) = chord_route_problems(&declared, &routed, &BTreeMap::new());
        assert_eq!(keyboard_only, vec!["field.undo".to_owned()]);
        assert!(stale.is_empty());
        // An action with no chord at all is not required to have one: `session.lock` reaches the
        // user through `templar session lock` and a widget region, and neither is this rule's
        // business.
        assert!(!keyboard_only.contains(&"session.lock".to_owned()));
    }

    #[test]
    fn an_exemption_silences_the_rule_and_expires_the_moment_it_is_not_needed() {
        let declared = declared_actions(MANIFEST_SHAPE);
        let exempt = BTreeMap::from([("field.undo", "no surface for it yet")]);
        let (keyboard_only, stale) =
            chord_route_problems(&declared, &BTreeSet::from(["home.reveal"]), &exempt);
        assert!(keyboard_only.is_empty());
        assert!(stale.is_empty());

        // The route arrives: the exemption is now a lie and must go.
        let routed = BTreeSet::from(["home.reveal", "field.undo"]);
        let (_, stale) = chord_route_problems(&declared, &routed, &exempt);
        assert_eq!(stale, vec!["field.undo".to_owned()]);

        // So must an exemption for an action nobody declares any more.
        let ghost = BTreeMap::from([("field.vanished", "removed last week")]);
        let (_, stale) = chord_route_problems(&declared, &routed, &ghost);
        assert_eq!(stale, vec!["field.vanished".to_owned()]);
    }

    #[test]
    fn vendored_assets_without_a_licence_are_reported() {
        let found = vec![
            VendoredAssets {
                dir: "shell/templar-shell/assets".into(),
                licence: Some("shell/templar-shell/assets/fonts/LICENSE".into()),
            },
            VendoredAssets {
                dir: "apps/templar-greeter/assets".into(),
                licence: None,
            },
        ];
        let problems = unlicensed_assets(&found);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("apps/templar-greeter/assets:"));
        assert!(unlicensed_assets(&found[..1]).is_empty());
    }

    fn names(set: &BTreeSet<String>) -> Vec<&str> {
        set.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_single_line_server_attribute_is_found() {
        let text = r#"#[zbus::interface(name = "org.templaros.Widgets1")]
            impl WidgetsApi {}"#;
        assert_eq!(
            names(&server_interface_names(text)),
            ["org.templaros.Widgets1"]
        );
    }

    #[test]
    fn a_server_attribute_split_across_lines_is_found() {
        // The shape a line-based scan with a `name = "` marker silently misses.
        let text =
            "#[zbus::interface(\n    name = \"org.templaros.Widgets1\"\n)]\nimpl WidgetsApi {}";
        assert_eq!(
            names(&server_interface_names(text)),
            ["org.templaros.Widgets1"]
        );
    }

    #[test]
    fn server_and_client_attributes_tolerate_any_spacing_around_equals() {
        let server = "#[zbus::interface(name=\"org.templaros.Widgets1\")]";
        let client = "#[proxy(interface   =    \"org.templaros.Widgets1\")]";
        assert_eq!(
            names(&server_interface_names(server)),
            ["org.templaros.Widgets1"]
        );
        assert_eq!(
            names(&client_interface_names(client)),
            ["org.templaros.Widgets1"]
        );
    }

    #[test]
    fn an_attribute_inside_a_comment_is_not_a_declaration() {
        // A doc example showing callers how to declare one must not register as the real thing;
        // otherwise `check-interfaces` demands XML for an interface nobody implements, and
        // `check-proxies` reports a server that does not exist.
        let text = r#"
            /// Declare one like this:
            /// #[zbus::interface(name = "org.templaros.Documented1")]
            // #[proxy(interface = "org.templaros.CommentedOut1")]
            #[zbus::interface(name = "org.templaros.Real1")]
            impl RealApi {}
        "#;
        assert_eq!(
            names(&server_interface_names(text)),
            ["org.templaros.Real1"]
        );
        assert!(client_interface_names(text).is_empty());
    }

    #[test]
    fn a_multiline_proxy_attribute_is_found_and_both_spellings_work() {
        let text = "#[proxy(\n    interface = \"org.templaros.Widgets1\",\n    default_service = \"org.templaros.Session\",\n    default_path = \"/org/templaros/Session/Widgets\"\n)]\npub trait Widgets {}";
        assert_eq!(
            names(&client_interface_names(text)),
            ["org.templaros.Widgets1"]
        );
        let qualified = "#[zbus::proxy(interface = \"org.templaros.Other1\")]";
        assert_eq!(
            names(&client_interface_names(qualified)),
            ["org.templaros.Other1"]
        );
    }

    #[test]
    fn an_interface_key_is_matched_as_a_whole_argument_name() {
        // `default_interface` must not be mistaken for `interface`.
        let text = r#"#[proxy(default_interface = "org.templaros.Wrong1", interface = "org.templaros.Right1")]"#;
        assert_eq!(
            names(&client_interface_names(text)),
            ["org.templaros.Right1"]
        );
    }

    #[test]
    fn a_key_equals_value_outside_any_attribute_is_never_picked_up() {
        // The boundary that makes this a real check of the attribute rather than a substring
        // scan over the whole file.
        let text = r#"
            const DOCS: &str = "interface = \"org.templaros.NotReal1\"";
            let name = "org.templaros.AlsoNotReal1";
        "#;
        assert!(client_interface_names(text).is_empty());
        assert!(server_interface_names(text).is_empty());
    }

    #[test]
    fn non_templaros_interfaces_are_ignored() {
        let text = r#"#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
            #[proxy(interface = "org.freedesktop.DBus")]"#;
        assert!(server_interface_names(text).is_empty());
        assert!(client_interface_names(text).is_empty());
    }

    #[test]
    fn check_proxies_goes_green_when_every_client_has_a_server() {
        let server =
            server_interface_names(r#"#[zbus::interface(name = "org.templaros.Widgets1")]"#);
        let client = client_interface_names(r#"#[proxy(interface = "org.templaros.Widgets1")]"#);
        let orphaned: Vec<&String> = client.iter().filter(|n| !server.contains(*n)).collect();
        assert!(orphaned.is_empty());
    }

    #[test]
    fn check_proxies_goes_red_for_a_client_interface_with_no_server() {
        let server =
            server_interface_names(r#"#[zbus::interface(name = "org.templaros.Widgets1")]"#);
        let client = client_interface_names(r#"#[proxy(interface = "org.templaros.Ghost1")]"#);
        let orphaned: Vec<&String> = client.iter().filter(|n| !server.contains(*n)).collect();
        assert_eq!(orphaned, [&"org.templaros.Ghost1".to_owned()]);
    }

    #[test]
    fn control_packages_parse_with_odd_but_legal_spacing() {
        // Written with explicit escapes rather than `\`-continued lines: Rust's line continuation
        // also eats the next line's leading whitespace, which would silently destroy the one
        // character that makes the continuation-line case a test at all.
        let control = concat!(
            "Source: templaros\n",
            "\n",
            "Package: templaros-cli   \n", // trailing whitespace
            "Architecture: any\n",
            "Description: one\n",
            "\n",
            "Package:templaros-shell\n", // no space after the colon
            "Architecture: any\n",
            "Description: two that continues\n",
            " Package: not-a-stanza-it-is-a-continuation-line\n", // leading space
            "\n",
            "Package-Type: udeb\n", // a different field that merely starts with "Package"
        );
        assert_eq!(
            parse_control_packages(control),
            ["templaros-cli".to_owned(), "templaros-shell".to_owned()],
        );
    }

    fn deb_names(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(deb, krate)| ((*deb).to_owned(), (*krate).to_owned()))
            .collect()
    }

    fn service_row(
        lifetime: Option<templar_build::components::Lifetime>,
    ) -> templar_build::components::Component {
        templar_build::components::Component {
            name: "filed".to_owned(),
            crate_name: "templar-filed".to_owned(),
            package: "templaros-filed".to_owned(),
            stamp: None,
            unit: Some("templar-filed.service".to_owned()),
            scope: templar_build::components::Scope::User,
            activation: templar_build::components::Activation::Manual,
            depends_on: Vec::new(),
            summary: "file service".to_owned(),
            authority: "the per-user file index".to_owned(),
            lifetime,
            consumer_interfaces: vec!["org.templaros.Files.Index1".to_owned()],
            recovery: "SnapshotRequired, then resnapshot".to_owned(),
            deadlines: "answered from the last committed generation".to_owned(),
            timer_exceptions: Vec::new(),
        }
    }

    fn matrix_of(
        components: Vec<templar_build::components::Component>,
    ) -> templar_build::components::Matrix {
        templar_build::components::Matrix {
            version: 1,
            components,
        }
    }

    #[test]
    fn a_service_that_declares_no_authority_is_a_packaging_failure() {
        // The whole point of putting the inventory here rather than in a document: a service that
        // stops describing itself fails the build instead of going quietly stale.
        let mut row = service_row(Some(templar_build::components::Lifetime::DbusActivated));
        row.authority.clear();
        let types = [("templar-filed.service".to_owned(), "dbus".to_owned())]
            .into_iter()
            .collect();
        let activated = BTreeSet::from(["templar-filed.service".to_owned()]);
        let problems =
            service_inventory_problems(&matrix_of(vec![row]), &types, &activated, &BTreeSet::new());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("declares no authority"),
            "{problems:?}"
        );
    }

    #[test]
    fn type_dbus_is_readiness_and_does_not_make_a_service_activation_driven() {
        let row = service_row(Some(templar_build::components::Lifetime::SessionBound));
        let types = [("templar-filed.service".to_owned(), "dbus".to_owned())]
            .into_iter()
            .collect();
        assert!(
            service_inventory_problems(
                &matrix_of(vec![row]),
                &types,
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .is_empty()
        );
    }

    #[test]
    fn dbus_activation_requires_a_descriptor_and_must_not_be_eagerly_wanted() {
        let row = service_row(Some(templar_build::components::Lifetime::DbusActivated));
        let types = [("templar-filed.service".to_owned(), "dbus".to_owned())]
            .into_iter()
            .collect();
        let problems = service_inventory_problems(
            &matrix_of(vec![row.clone()]),
            &types,
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("no D-Bus service descriptor"),
            "{problems:?}"
        );

        let present = BTreeSet::from(["templar-filed.service".to_owned()]);
        let problems =
            service_inventory_problems(&matrix_of(vec![row]), &types, &present, &present);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("eagerly Wants"), "{problems:?}");
    }

    #[test]
    fn a_component_with_no_unit_owes_no_inventory() {
        // A CLI or a data-only package has no process, no authority and nothing to recover.
        let mut row = service_row(None);
        row.unit = None;
        row.authority.clear();
        row.recovery.clear();
        row.deadlines.clear();
        assert!(
            service_inventory_problems(
                &matrix_of(vec![row]),
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .is_empty()
        );
    }

    /// The real tree first — the check has to hold for what is actually shipped — and then the
    /// regression it exists for: shipping the hook to `/etc/profile.d` and calling it delivered.
    #[test]
    fn the_garden_hook_must_reach_a_file_a_non_login_interactive_bash_reads() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/templar/Cargo.toml");
        let real = serde_json::json!({
            "manifest_path": manifest.to_string_lossy(),
            "metadata": { "deb": {
                "name": "templaros-cli",
                "maintainer-scripts": "../../packaging/templaros-cli",
                "assets": [["../../session/profile.d/60-templaros-garden.sh", GARDEN_HOOK_PAYLOAD, "644"]],
            }},
        });
        assert!(garden_hook_delivery_problems(std::slice::from_ref(&real)).is_empty());

        let mut profile_d_only = real.clone();
        profile_d_only["metadata"]["deb"]
            .as_object_mut()
            .unwrap()
            .remove("maintainer-scripts");
        let problems = garden_hook_delivery_problems(&[profile_d_only]);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("/etc/bash.bashrc"));

        assert_eq!(
            garden_hook_delivery_problems(&[serde_json::json!({})]).len(),
            1,
            "a tree that ships the payload nowhere is a mismatch too"
        );
    }

    #[test]
    fn check_packaging_goes_green_when_both_sides_match() {
        let control = vec!["templaros-cli".to_owned(), "templaros-shell".to_owned()];
        let debs = deb_names(&[
            ("templaros-cli", "templar"),
            ("templaros-shell", "templar-shell"),
        ]);
        assert!(packaging_problems(&control, &debs).is_empty());
    }

    #[test]
    fn control_depends_parse_per_stanza_without_substitution_variables() {
        let control = "Package: templaros-hostd\nArchitecture: any\n\
                       Depends: ${shlibs:Depends}, ${misc:Depends}, systemd, apt, polkitd\n\n\
                       Package: templaros-cli\nDepends: ${shlibs:Depends}, ${misc:Depends}\n\n\
                       Package: templaros-bare\n";
        let parsed = parse_control_depends(control);
        assert_eq!(
            parsed["templaros-hostd"],
            BTreeSet::from(["systemd".to_owned(), "apt".to_owned(), "polkitd".to_owned()])
        );
        assert!(parsed["templaros-cli"].is_empty());
        assert!(parsed["templaros-bare"].is_empty());
        assert_eq!(
            parse_depends_list("$auto, polkitd|policykit-1 , xwayland"),
            BTreeSet::from(["polkitd | policykit-1".to_owned(), "xwayland".to_owned()])
        );
    }

    #[test]
    fn check_packaging_goes_red_when_control_and_crate_depends_drift() {
        // The one that shipped: the crate table still named the transitional policykit-1 after
        // debian/control had moved to polkitd.
        let control = BTreeMap::from([(
            "templaros-hostd".to_owned(),
            parse_depends_list("systemd, apt, polkitd"),
        )]);
        let deb = BTreeMap::from([(
            "templaros-hostd".to_owned(),
            parse_depends_list("$auto, systemd, apt, policykit-1"),
        )]);
        let names = deb_names(&[("templaros-hostd", "templar-hostd")]);
        let problems = dependency_drift(&control, &deb, &names);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("templaros-hostd (crate templar-hostd)"));
        assert!(problems[0].contains("only in debian/control: [\"polkitd\"]"));
        assert!(problems[0].contains("only in [package.metadata.deb] depends: [\"policykit-1\"]"));

        let deb = BTreeMap::from([(
            "templaros-hostd".to_owned(),
            parse_depends_list("$auto, polkitd, apt, systemd"),
        )]);
        assert!(dependency_drift(&control, &deb, &names).is_empty());
    }

    #[test]
    fn check_packaging_goes_red_for_a_control_package_with_no_deb_metadata() {
        let control = vec!["templaros-cli".to_owned(), "templaros-orphan".to_owned()];
        let debs = deb_names(&[("templaros-cli", "templar")]);
        let problems = packaging_problems(&control, &debs);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("templaros-orphan"));
        assert!(problems[0].contains("no crate [package.metadata.deb]"));
    }

    #[test]
    fn a_manifest_executable_counts_as_provided_only_when_something_provides_it() {
        let installed = BTreeSet::from(["/usr/bin/templar-shell".to_owned()]);
        let forks = BTreeSet::from(["templar-files".to_owned()]);

        assert!(manifest_executable_is_provided(
            "/usr/bin/templar-shell",
            &installed,
            &forks
        ));
        // From the cosmic-files fork, whose file list is not visible from this repository.
        assert!(manifest_executable_is_provided(
            "/usr/bin/templar-files",
            &installed,
            &forks
        ));
        // The one that shipped: a fixture manifest for a crate that has never existed.
        assert!(!manifest_executable_is_provided(
            "/usr/bin/templar-viewer",
            &installed,
            &forks
        ));
    }

    #[test]
    fn check_packaging_goes_red_when_two_packages_install_the_same_path() {
        // The real case: templaros-session and templaros-comp both shipped templar-comp.service.
        let shipped = vec![
            (
                "/usr/lib/systemd/user/templar-comp.service".to_owned(),
                "templaros-comp".to_owned(),
            ),
            (
                "/usr/lib/systemd/user/templar-comp.service".to_owned(),
                "templaros-session".to_owned(),
            ),
            // One package listing a file in both of its own build paths is agreement, not a
            // collision.
            (
                "/usr/bin/templar-shell".to_owned(),
                "templaros-shell".to_owned(),
            ),
            (
                "/usr/bin/templar-shell".to_owned(),
                "templaros-shell".to_owned(),
            ),
        ];
        let problems = colliding_destinations(&shipped);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("templar-comp.service"));
        assert!(problems[0].contains("templaros-comp"));
        assert!(problems[0].contains("templaros-session"));
    }

    #[test]
    fn check_packaging_goes_red_for_a_metapackage_depending_on_a_package_that_is_not_built() {
        let controls = vec![(
            "distro/metapackages/templaros-desktop/control.in".to_owned(),
            "Package: templaros-desktop\nDepends: templaros-shell, templar-comp, greetd, ${misc:Depends}\n \
             templaros-cli, templar-greeter\nDescription: TemplarOS desktop\n Uses templar-hostd's \
             transactional path with automatic rollback.\n"
                .to_owned(),
        )];
        let built = BTreeSet::from([
            "templaros-shell".to_owned(),
            "templaros-cli".to_owned(),
            "templaros-comp".to_owned(),
        ]);
        let problems = metapackage_dependency_problems(&controls, &built);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("templar-comp")));
        // Found on a folded continuation line, which is how the real file is written.
        assert!(problems.iter().any(|p| p.contains("templar-greeter")));
        // Neither Ubuntu's packages nor substitution variables are ours to judge.
        assert!(!problems.iter().any(|p| p.contains("greetd")));
        assert!(!problems.iter().any(|p| p.contains("misc:Depends")));
        // Prose in a Description continuation line is not a dependency list, however much its
        // words look like package names.
        assert!(
            !problems.iter().any(|p| p.contains("hostd")),
            "{problems:?}"
        );
    }

    #[test]
    fn check_packaging_goes_red_for_a_seed_naming_a_package_that_is_not_built() {
        // The real case: every other line in session.seed is `templaros-*`, and these two were
        // `templar-*` — the binary's name, not the package's.
        let seeds = BTreeMap::from([(
            "session.seed".to_owned(),
            vec![
                "templaros-shell".to_owned(),
                "templar-comp".to_owned(),
                "greetd".to_owned(),
            ],
        )]);
        let built = BTreeSet::from(["templaros-shell".to_owned(), "templaros-comp".to_owned()]);
        let problems = seed_problems(&seeds, &built);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("templar-comp"));
        // greetd is Ubuntu's, not ours, and must not be judged.
        assert!(!problems[0].contains("greetd"));
    }

    #[test]
    fn check_packaging_goes_red_for_a_unit_whose_binary_no_package_ships() {
        // The templar-comp case: a unit promising /usr/bin/templar-comp while debian/control and the
        // crate metadata agree with each other perfectly, because neither mentions it at all.
        let units = vec![(
            "templar-comp.service".to_owned(),
            "/usr/bin/templar-comp".to_owned(),
        )];
        let shipped = BTreeSet::from(["/usr/bin/templar-shell".to_owned()]);
        let problems = unshipped_unit_binaries(&units, &shipped);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("templar-comp.service"));
        assert!(problems[0].contains("/usr/bin/templar-comp"));
    }

    #[test]
    fn check_packaging_accepts_a_unit_whose_binary_is_shipped() {
        let units = vec![(
            "templar-sessiond.service".to_owned(),
            "/usr/libexec/templar-sessiond".to_owned(),
        )];
        let shipped = BTreeSet::from(["/usr/libexec/templar-sessiond".to_owned()]);
        assert!(unshipped_unit_binaries(&units, &shipped).is_empty());
    }

    #[test]
    fn check_packaging_goes_red_for_deb_metadata_with_no_control_stanza() {
        let control = vec!["templaros-cli".to_owned()];
        let debs = deb_names(&[
            ("templaros-cli", "templar"),
            ("templaros-ghost", "templar-ghost"),
        ]);
        let problems = packaging_problems(&control, &debs);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("templaros-ghost"));
        assert!(problems[0].contains("crate templar-ghost"));
    }

    #[test]
    fn check_packaging_goes_red_for_a_static_asset_source_that_does_not_exist() {
        // cargo-deb resolves static sources relative to the crate directory; a path that only
        // exists relative to the workspace root fails at packaging time, which CI never runs.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("present.conf"), "x").unwrap();
        let manifest_path = dir.path().join("Cargo.toml").display().to_string();
        let packages = vec![serde_json::json!({
            "name": "templar-ghost",
            "manifest_path": manifest_path,
            "authors": ["TemplarOS"],
            "metadata": { "deb": { "name": "templaros-ghost", "assets": [
                ["target/release/templar-ghost", "usr/bin/templar-ghost", "755"],
                ["present.conf", "etc/templaros/present.conf", "644"],
                ["distro/deploy/components.toml", "usr/share/templaros/components.toml", "644"],
            ]}},
        })];
        let missing = crate::deb_metadata(&packages).missing_asset_sources;
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("distro/deploy/components.toml"));
        assert!(missing[0].contains("templaros-ghost"));
    }
}
