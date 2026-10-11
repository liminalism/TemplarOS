//! `templar` — one command line over the OS.
//!
//! `docs/architecture/accessibility.md` makes this a requirement rather than a convenience:
//! everything the shell shows must be reachable here, so the system stays usable when the shell
//! is broken. Every subcommand is human-readable by default and machine-readable with `--json`,
//! and a service that is not running produces one readable sentence — never a panic or a
//! `zbus` debug dump.
//!
//! All the rendering and parsing lives in the library half (`templar::*`), behind per-interface
//! client traits, so it is unit-tested without a session bus. This file is only argument
//! parsing, connection setup and dispatch.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};
use templar::{
    account, archive, bus, dev, doctor, files, history, notify, open, power, project, providers,
    res, settings, update,
};
use templar_client::proxies::{
    AppIdentityProxy, AssociationsProxy, AudioProxy, BacklightProxy, BatteryProxy,
    FilesActionsProxy, FilesIndexProxy, FilesOperationsProxy, FilesRecentsProxy, FilesSearchProxy,
    FilesTrashProxy, FilesVolumesProxy, HistoryProxy, HomeFieldProxy, HomeGardenProxy,
    HostDeploymentsProxy, HostDriversProxy, HostHardwareProxy, HostStorageProxy, HostUpdatesProxy,
    LauncherProxy, LifecycleProxy, PowerProxy, ProjectsProxy, ProvidersProxy, ResourcesProxy,
    SettingsProxy,
};
use templar_types::ProviderRole;

/// The TemplarOS command line.
#[derive(Debug, Parser)]
#[command(name = "templar", version, about, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run explicit work in its own sibling cgroup, retaining this terminal and job control.
    Run {
        #[arg(long, value_parser = ["interactive", "background-compute"], default_value = "background-compute")]
        class: String,
        /// Notify when the command finishes, reusing the owned child's exit. The flag is
        /// the opt-in: it notifies however short the run, while still suppressing
        /// canceled runs and shell-hook duplicates by policy.
        #[arg(long)]
        notify: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<std::ffi::OsString>,
    },
    #[command(hide = true)]
    WorkloadGate {
        socket: PathBuf,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<std::ffi::OsString>,
    },
    /// Applications: identity, instances, launching.
    App {
        #[command(subcommand)]
        command: AppCommand,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Open a file or URI through the association broker (never bypasses it).
    Open {
        /// Path or URI.
        target: String,
        /// Print the full resolution chain without launching.
        #[arg(long)]
        explain: bool,
        /// Resolve within this project's defaults.
        #[arg(long, default_value = "")]
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// Start the Terminal provider role in a directory through `Launcher1`, so the terminal gets
    /// its own application identity. What `templar-terminal`, PCManFM-Qt's terminal command,
    /// runs.
    Terminal(TerminalArgs),
    /// PCManFM-Qt integration: settings the session repairs.
    Pcmanfm {
        #[command(subcommand)]
        command: PcmanfmCommand,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Resource governor: pressure, stage, victim scores and traces.
    Res {
        #[command(subcommand)]
        command: Option<ResCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// File service: volumes, operations, recents, search and path forms.
    Files {
        #[command(subcommand)]
        command: FilesCommand,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Create ZIP archives or extract archives locally, without a desktop service.
    Archive {
        #[command(subcommand)]
        command: ArchiveCommand,
    },
    /// Owned account changes, preserving credentials across rollback (run without sudo).
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Volume, display brightness and battery.
    Status {
        #[command(subcommand)]
        command: Option<StatusCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// The trash: what is in it, putting something back, and emptying it.
    Trash {
        #[command(subcommand)]
        command: Option<TrashCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Host updates, deployments, drivers and the storage reserve (system bus).
    Update {
        #[command(subcommand)]
        command: Option<UpdateCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// System-wide change history.
    History {
        #[command(subcommand)]
        command: Option<HistoryCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Exclusive-role providers.
    Providers {
        #[command(subcommand)]
        command: Option<ProvidersCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Typed settings: schema, values and the change history.
    Settings {
        #[command(subcommand)]
        command: Option<SettingsCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Native wallpaper: desired/applied status, rotation and lossless Settings1 policy.
    Wallpaper {
        #[command(subcommand)]
        command: Option<templar::wallpaper::WallpaperCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// The Home Field.
    Home {
        #[command(subcommand)]
        command: HomeCommand,
        #[arg(long, global = true)]
        json: bool,
    },

    /// Project sessions.
    Project {
        #[command(subcommand)]
        command: Option<ProjectCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Homegarden: the places Home has grown from what you do (`HomeGarden1`).
    Garden {
        #[command(subcommand)]
        command: Option<GardenCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Terminal job-completion notifications: the shell hooks' delivery route.
    Notify {
        #[command(subcommand)]
        command: NotifyCommand,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Power policy per source.
    Power {
        #[command(subcommand)]
        command: Option<PowerCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// End, suspend or lock this session.
    Session {
        #[command(subcommand)]
        command: Option<SessionCommand>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Developer deployment: what is installed, what is running, and the gap between them.
    Dev {
        #[command(subcommand)]
        command: Option<DevCommand>,
        /// The local archive build sets are published to and installed from.
        #[arg(long, global = true, default_value = templar::dev::repo::DEFAULT_ROOT)]
        repo: PathBuf,
        /// Act on an unpacked root instead of `/`.
        #[arg(long, global = true, default_value = "/")]
        root: PathBuf,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Integration checks (runtime, image root, or hardware matrix).
    #[command(args_conflicts_with_subcommands = true)]
    Doctor {
        #[command(subcommand)]
        command: Option<DoctorCommand>,
        /// Check an unpacked image root instead of `/`.
        #[arg(long, default_value = "/")]
        root: PathBuf,
        /// Include hardware-matrix checks.
        #[arg(long)]
        hardware: bool,
        /// Only the live preflight: can this machine run a session at all. What the installation
        /// medium runs before an install, so an image check cannot block one.
        #[arg(long, conflicts_with_all = ["hardware", "check", "profile"])]
        preflight: bool,
        /// Run one check only: a built-in id (for example `file-index`) on its own, or
        /// a hardware case (for example `gpu-mode`) together with `--hardware`.
        #[arg(long)]
        check: Option<String>,
        /// Override hardware profile detection when reviewing a matrix off-device.
        #[arg(long, requires = "hardware")]
        profile: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Change your password through a sealed FD after hidden terminal confirmation.
    Password,
}

#[derive(Debug, clap::Args)]
struct TerminalArgs {
    /// Directory to open in (default: the current directory).
    #[arg(long, short = 'd', visible_alias = "directory")]
    working_directory: Option<PathBuf>,
    /// Keep the window open after the command exits.
    #[arg(long)]
    hold: bool,
    /// Resolve the provider and print what would be launched, without launching.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
    /// Run this program and its arguments instead of a shell. Everything after it is the
    /// command, so it must come last.
    #[arg(
        short = 'e',
        long = "command",
        num_args = 1..,
        allow_hyphen_values = true,
        value_name = "COMMAND"
    )]
    command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum PcmanfmCommand {
    /// Rewrite `[System] Terminal` / `FallbackIconThemeName` when what they name cannot work here
    /// (not on PATH / not installed); valid choices and every other byte are kept. Also adds the
    /// `templar-terminal` group to libfm-qt's per-user terminal list. The session does this at
    /// startup; this runs it now.
    Repair {
        /// The settings file (default: `$XDG_CONFIG_HOME/pcmanfm-qt/default/settings.conf`).
        #[arg(long)]
        settings: Option<PathBuf>,
        /// The terminal list (default: `$XDG_DATA_HOME/libfm-qt/terminals.list`).
        #[arg(long)]
        terminals_list: Option<PathBuf>,
        /// Copy the original settings to `<settings>.templaros-bak` first (never overwrites one).
        #[arg(long)]
        backup: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ArchiveCommand {
    /// Compress the selected files and folders into one ZIP archive.
    Compress {
        /// Choose the output ZIP path instead of deriving it from the selection.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Show native progress, cancellation and error dialogs.
        #[arg(long)]
        gui: bool,
        /// Selected paths; use `--` before filenames beginning with a dash.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Extract one or more archives without replacing existing files.
    Extract {
        #[arg(long, value_enum, default_value = "here")]
        mode: ArchiveMode,
        /// Destination for directory mode; omit it to open a folder chooser.
        #[arg(long)]
        directory: Option<PathBuf>,
        /// Show native progress, cancellation, password and error dialogs.
        #[arg(long)]
        gui: bool,
        /// Selected archives; use `--` before filenames beginning with a dash.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ArchiveMode {
    /// Extract next to each archive.
    Here,
    /// Extract into a new folder named after each archive.
    Smart,
    /// Extract into the selected directory.
    Directory,
}

#[derive(Debug, Subcommand)]
enum DoctorCommand {
    /// Freeze report for a past boot: run after a hard restart.
    ///
    /// Collects what the frozen boot left behind (journal, compositor watchdog, kernel,
    /// coredumps, pstore) into one report directory and prints the triage answer. Read-only;
    /// reads more as root (`sudo` writes the report for the invoking user).
    Freeze {
        /// The boot to examine: a journal offset (`-1` is the previous boot) or a boot ID.
        #[arg(long, default_value = "-1", allow_hyphen_values = true)]
        boot: String,
        /// Write the `boot-<id>/` report under this directory instead of
        /// `$XDG_STATE_HOME/templaros/freeze-reports`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the structured analysis instead of the summary.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DevCommand {
    /// Per-component convergence: installed set against running builds (the default).
    Status {
        /// Compare against this build set instead of the newest one installed.
        #[arg(long)]
        target: Option<String>,
    },
    /// The local package archive this machine installs build sets from.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Install a published build set — one pinned apt transaction for all of it.
    Install {
        /// Which set; defaults to the archive's current one.
        build_set: Option<String>,
        /// Print the transaction and stop.
        #[arg(long)]
        dry_run: bool,
    },
    /// Put the previous build set back.
    Rollback {
        /// Print the transaction and stop.
        #[arg(long)]
        dry_run: bool,
    },
    /// Publish, install, activate and report — the whole deployment.
    Deploy {
        /// The manifest `cargo xtask build-set` wrote.
        #[arg(long, default_value = "target/debian/build-set.json")]
        build_set: PathBuf,
        /// Print the whole pipeline and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Put the previous set back without asking if a component fails its health gate.
        #[arg(long)]
        auto_rollback: bool,
        /// Copy the state stores first, for a set that could not otherwise be rolled back.
        #[arg(long)]
        snapshot_state: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Create the archive and its per-machine signing key. Idempotent.
    Init {
        /// Also write apt's keyring and deb822 source. Needs root.
        #[arg(long)]
        install_source: bool,
        /// Write those under this root instead of `/`.
        #[arg(long, default_value = "/")]
        sysroot: PathBuf,
    },
    /// Publish a built set into the archive.
    Publish {
        /// The manifest `cargo xtask build-set` wrote.
        #[arg(long, default_value = "target/debian/build-set.json")]
        build_set: PathBuf,
    },
    /// What the archive holds, and what a rollback would go back to.
    Status,
    /// Drop all but the newest sets (never the current or previous one).
    Prune {
        #[arg(long, default_value_t = templar::dev::repo::DEFAULT_KEEP)]
        keep: usize,
    },
    /// Check that the archive holds coherent, signed, installable sets.
    Verify {
        /// One set, rather than current, previous and anything staged.
        #[arg(long)]
        set: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AppCommand {
    /// List installed application manifests.
    List,
    /// List running instances.
    Instances,
    /// Launch an application by canonical ID.
    Launch {
        app_id: String,
        #[arg(trailing_var_arg = true)]
        arguments: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ResCommand {
    /// Pressure level, stage and per-instance scores (the default).
    Status,
    /// Why this instance would (or would not) be chosen as the next victim.
    Explain { instance_id: String },
    /// Move the containment stage (`observe-only|trim|throttle|checkpoint|terminate`).
    SetStage { stage: String },
    /// Recorded pressure traces, newest first.
    Traces {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Debug, Subcommand)]
enum FilesCommand {
    /// Open the global launcher in Files mode within this directory.
    SearchHere {
        directory: PathBuf,
        #[arg(long, default_value = "")]
        query: String,
    },
    /// Every volume with its free space and reserve.
    Volumes,
    /// Recent file operations and their progress.
    Operations {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Stop a running operation at its next step boundary.
    Cancel { op_id: String },
    /// Reverse a committed operation.
    Undo { op_id: String },
    /// Recently used files.
    Recents {
        #[arg(long, default_value = "")]
        app: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Which directory a chooser should open at, and why.
    Suggest {
        #[arg(long, default_value = "")]
        app: String,
        /// `open`, `save`, `import` or `export`.
        #[arg(long, default_value = "open")]
        operation: String,
    },
    /// Search indexed roots by name and path (`Files.Search1`, the compatibility path).
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// The shared file index: freshness, typed search and background repair (`Files.Index1`).
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    /// Explicit bookmark interoperability; ordinary Home changes never replace native places.
    Bookmarks {
        #[command(subcommand)]
        command: BookmarksCommand,
    },
    /// Render a path in one of the canonical forms.
    Path {
        path: std::path::PathBuf,
        /// `canonical|as-shown|uri|shell-quoted|parent-directory|name`; omit to list the forms.
        #[arg(long)]
        form: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum BookmarksCommand {
    /// Add Home places to GTK/libfm-qt bookmarks, preserving native entries and saving a backup.
    Export,
}

#[derive(Debug, Subcommand)]
enum IndexCommand {
    /// Every registered root's freshness plus the core's own diagnostics.
    Status,
    /// Typed search over the shared index — lossless paths, generation and freshness, unlike
    /// `templar files search`'s `Search1` compatibility path.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// NUL-delimited native paths instead of the human table (for `xargs -0`).
        #[arg(long)]
        print0: bool,
    },
    /// Schedule a background repair of the given roots (by id, from `status`), or every
    /// registered root when none are given. Returns immediately; watch `status` for progress.
    Reconcile { roots: Vec<i64> },
    /// Read an authoritative native-path policy at the observed hash from `status --json`.
    Policy {
        path: std::path::PathBuf,
        #[arg(long)]
        expected_hash: String,
    },
    /// Store a root policy; use --expected-hash absent for add-only registration.
    Configure {
        path: std::path::PathBuf,
        #[arg(long)]
        expected_hash: String,
        /// RootPolicy JSON; omitted means the normal Home policy.
        #[arg(long)]
        policy_json: Option<String>,
    },
    /// Remove a native-path root only if its authoritative policy revision still matches.
    Remove {
        path: std::path::PathBuf,
        #[arg(long)]
        expected_hash: String,
    },
}

#[derive(Debug, Subcommand)]
enum StatusCommand {
    /// Volume, brightness and battery together (the default).
    All,
    /// The default sink's volume.
    Volume {
        /// Set the volume, 0..=100.
        #[arg(long)]
        set: Option<u32>,
    },
    /// Flip the default sink's mute.
    Mute,
    /// Volume up one step.
    Louder,
    /// Volume down one step.
    Quieter,
    /// The display backlight.
    Brightness {
        /// Set the brightness, 1..=100.
        #[arg(long)]
        set: Option<u32>,
    },
    /// Backlight up one step.
    Brighter,
    /// Backlight down one step.
    Dimmer,
    /// Charge, status and time remaining.
    Battery,
}

#[derive(Debug, Subcommand)]
enum TrashCommand {
    /// Everything in every trash, newest deletion first (the default).
    List,
    /// Put trashed items back where they came from, by their path under `Trash/files`.
    Restore { trashed_paths: Vec<String> },
    /// Remove trashed items for good. There is no undo.
    Delete {
        trashed_paths: Vec<String>,
        /// Required: this cannot be undone.
        #[arg(long)]
        yes: bool,
    },
    /// Empty every trash this user has. There is no undo.
    Empty {
        /// Required: this cannot be undone.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum UpdateCommand {
    /// The transaction in flight, if any (the default).
    Status,
    /// Plan a transaction from a `ChangeSet` JSON document.
    Plan { changes_json: String },
    /// Every transaction the journal remembers.
    History,
    /// Boot deployments and which one is known-good.
    Deployments,
    /// Stage a deployment for a trial boot.
    Stage {
        id: String,
        #[arg(long, default_value_t = 3)]
        tries: u8,
    },
    /// Bless the currently booted deployment.
    MarkGood,
    /// Roll back to a deployment.
    Rollback { id: String },
    /// The driver authority's plan and any support violations.
    Drivers,
    /// Just the driver support violations.
    Violations,
    /// Accept the installed NVIDIA versions as the new locked baseline.
    RepairDrivers,
    /// Storage reserve for one mount point.
    Storage {
        #[arg(long, default_value = "/")]
        mount: String,
    },
    /// Run the trim order against a mount point until the reserve is healthy.
    EnforceStorage {
        #[arg(long, default_value = "/")]
        mount: String,
    },
    /// Cache directories trimmed first when the reserve is threatened.
    TrimOrder,
    /// The detected graphics profile and the declared GPU mode.
    Hardware,
    /// Set the declarative GPU mode (`hybrid|integrated|discrete`).
    SetGpuMode { mode: String },
}

#[derive(Debug, Subcommand)]
enum HistoryCommand {
    /// Most recent events (the default).
    Recent {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Search event messages.
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Reverse an event by ID.
    Undo { event_id: String },
}

#[derive(Debug, Subcommand)]
enum ProvidersCommand {
    /// Every exclusive role and how it resolved (the default).
    Report,
    /// Choose the provider for a role.
    Select { role: String, app_id: String },
}

#[derive(Debug, Subcommand)]
enum SettingsCommand {
    /// Every key at its effective value (the default).
    List,
    /// The typed schema with defaults and descriptions.
    Schema,
    /// One key's effective value.
    Get { key: String },
    /// Set a key; the value is JSON (`{"type":"choice","value":"enforce"}`).
    Set {
        key: String,
        value_json: String,
        #[arg(long, default_value = "templar-cli")]
        origin: String,
    },
    /// Remove a stored override, returning to the schema default.
    Reset {
        key: String,
        #[arg(long, default_value = "templar-cli")]
        origin: String,
    },
    /// The settings change audit trail.
    Changes {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

/// `templar home` — the Home Field from the command line. One subcommand today, because one is what
/// pcmanfm-qt's "Add to Home" needs; the rest of the field is `templar-shell --print field`.
#[derive(Debug, Subcommand)]
enum HomeCommand {
    /// Place a folder on the Home Field as an object (`HomeField1.PlaceFolder`).
    Place {
        path: PathBuf,
        /// The output whose field to place it on.
        #[arg(long, default_value = "primary")]
        output: String,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Every project and whether it is open (the default).
    List,
    Create {
        project_id: String,
        name: String,
        root: String,
    },
    /// Create a project rooted at a folder, deriving its id and name from the folder itself —
    /// what pcmanfm-qt's "Create project here" runs, where nobody is around to type an id.
    CreateHere {
        path: PathBuf,
    },
    Open {
        project_id: String,
    },
    Close {
        project_id: String,
    },
    Remove {
        project_id: String,
    },
    /// Everything one project holds: its root, directories, applications and importance.
    Show {
        project_id: String,
    },
    /// Change one thing about a project.
    Set {
        project_id: String,
        /// name | root | importance | state | directories | terminal_directories | mounts |
        /// references
        key: String,
        /// The new value. List-valued keys take a comma-separated list.
        value: String,
    },
    /// The project the session is working in. With no argument, print it; with one, switch to it;
    /// with `none`, clear it.
    Current {
        project_id: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum NotifyCommand {
    /// Report one finished foreground job. The hooks call this only for jobs that pass
    /// their elapsed prefilter; the completion policy decides whether it notifies.
    TerminalCompleted {
        /// How long the job ran, in milliseconds.
        #[arg(long)]
        elapsed_ms: u64,
        /// The job's exit status (130 for Ctrl-C).
        #[arg(long)]
        status: i32,
        /// A safe label: a bare command name, never arguments or redirections.
        #[arg(long)]
        label: String,
        /// The terminal emulator's producer name for native-dedupe matching; empty
        /// detects from the environment.
        #[arg(long, default_value = "")]
        emulator: String,
        /// Terminal focus: focused, unfocused, unknown, or auto to ask the terminal.
        #[arg(long, default_value = "auto")]
        focus: String,
    },
    /// Print the hook-prefilter assignment for a new shell to evaluate.
    PolicySh,
}

#[derive(Debug, Subcommand)]
enum GardenCommand {
    /// Every place the ledger knows, with its stage and relevance (the default).
    Contexts,
    /// Why a folder is (or is not) on Home: its evidence, stage and last tending reason.
    Why {
        path: PathBuf,
    },
    /// Run a tending moment now and print the report.
    Tend {
        #[arg(long, default_value = "primary")]
        output: String,
    },
    /// Display-only coding/agent sessions (version 1).
    Sessions,
    /// Resume a recorded session in a fresh brokered terminal instance.
    ResumeSession {
        path: PathBuf,
        session_id: String,
    },
    /// Forward a supported agent's documented stdin JSON callback.
    AgentHook {
        #[arg(value_parser = ["claude-code"])]
        adapter: String,
    },
    /// Initialize a private bounded command journal once at interactive shell startup.
    #[command(hide = true)]
    JournalInit {
        pid: u32,
    },
    /// Record a semantic activity source. Used by first-party shell integrations.
    Observe {
        /// placed-on-home | project-entered | launched-here | terminal-entered |
        /// directory-opened | file-opened | created-or-renamed | file-saved
        kind: String,
        path: PathBuf,
        #[arg(long, default_value = "")]
        app_id: String,
        #[arg(long, default_value = "")]
        role: String,
    },
    Pin {
        path: PathBuf,
    },
    Unpin {
        path: PathBuf,
    },
    /// Keep the project on Home without changing its size or activity state.
    Keep {
        path: PathBuf,
    },
    Hide {
        path: PathBuf,
    },
    Unhide {
        path: PathBuf,
    },
    /// Drop a place's record; fresh activity may grow it again.
    Forget {
        path: PathBuf,
    },
    /// Drop every unpinned place.
    Clear,
    /// The Garden Clock: sessions and unlocked hours so far.
    Clock,
}

#[derive(Debug, Subcommand)]
enum PowerCommand {
    /// The effective profile for a source (the default is both).
    Get {
        /// `battery` or `mains`; omit for both.
        source: Option<String>,
    },
    /// Remove a stored override for a source.
    Reset { source: String },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// What this machine will and will not do (the default).
    Status,
    /// End this session.
    Logout,
    Reboot,
    Poweroff,
    Suspend,
    /// Lock the screen. Not yet implemented — see `Lifecycle1.Lock`.
    Lock,
    /// Print the session's own edges — start, lock, unlock, sleep — as they happen, until
    /// interrupted. What a hands-on runbook watches while it locks the screen and closes the lid.
    Events,
}

/// Rust ignores SIGPIPE, so a `templar ... | head` whose reader goes away early dies as a
/// BrokenPipe panic *after* printing good output. Restore the default disposition first: dying
/// quietly on a closed pipe is what every other Unix tool does.
#[allow(unsafe_code)]
fn restore_sigpipe_default() {
    // SAFETY: `signal` with `SIG_DFL` only resets this process's own disposition; no handler
    // runs and no memory is shared.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    restore_sigpipe_default();
    if let Err(error) = run().await {
        // One readable sentence, with the causes that explain it — never a panic or a debug dump.
        eprintln!("templar: {error:#}");
        std::process::exit(1);
    }
    Ok(())
}

/// Print `value` as pretty JSON when `json`, otherwise the already-rendered `human` text.
/// The folder a `%f` argument names, or a readable sentence saying why it is not one.
fn folder_argument(path: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(path)
        .with_context(|| format!("{} does not exist", path.display()))?;
    anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
    Ok(root)
}

async fn home_command(command: HomeCommand, json: bool) -> Result<()> {
    let HomeCommand::Place { path, output } = command;
    let root = folder_argument(&path)?;
    let connection = bus::session().await?;
    let proxy = HomeFieldProxy::new(&connection).await?;
    let revision = proxy
        .place_folder(&output, &root.to_string_lossy())
        .await
        .context("placing the folder on the Home Field")?;
    emit(
        json,
        &serde_json::json!({"placed": root, "output": output, "revision": revision}),
        &format!("placed {} on Home", root.display()),
    )
}

async fn notify_command(command: NotifyCommand, json: bool) -> Result<()> {
    use std::str::FromStr as _;
    match command {
        NotifyCommand::PolicySh => {
            println!("{}", notify::policy_sh(&notify::read_policy().await));
            Ok(())
        }
        NotifyCommand::TerminalCompleted {
            elapsed_ms,
            status,
            label,
            emulator,
            focus,
        } => {
            let focus = notify::FocusRequest::from_str(&focus)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let emulator = if emulator.is_empty() {
                notify::detect_emulator()
            } else {
                emulator
            };
            // Defense in depth: hooks pass a bare command name already, and anything else
            // is reduced to one here before it can reach a notification body.
            let label = templar_session_core::notifications::safe_label(&label);
            let focus = notify::resolve_focus(focus).await;
            let deps = notify::BusDeps::session().await?;
            let report = notify::terminal_completed(
                &deps,
                &notify::CompletionRequest {
                    elapsed_ms,
                    status,
                    label,
                    emulator,
                },
                focus,
            )
            .await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "notify": report.notify,
                        "reason": report.reason,
                        "id": report.id,
                    })
                );
            }
            Ok(())
        }
    }
}

async fn garden_command(command: GardenCommand, json: bool) -> Result<()> {
    if let GardenCommand::JournalInit { pid } = &command {
        println!("{}", templar::garden_session::journal_init(*pid)?.display());
        return Ok(());
    }
    let connection = bus::session().await?;
    let proxy = HomeGardenProxy::new(&connection).await?;
    let path_of = |path: &Path| path.to_string_lossy().into_owned();
    let pinned = !matches!(command, GardenCommand::Unpin { .. });
    let hidden = matches!(command, GardenCommand::Hide { .. });
    match command {
        GardenCommand::Sessions => {
            let rows: serde_json::Value = serde_json::from_str(&proxy.sessions_v1().await?)?;
            let mut human = String::new();
            for row in rows.as_array().into_iter().flatten() {
                writeln!(
                    human,
                    "{} · {} · {} · {}",
                    row["id"].as_str().unwrap_or("?"),
                    row["program"].as_str().unwrap_or("shell"),
                    row["state"].as_str().unwrap_or("unknown"),
                    row["cwd"].as_str().unwrap_or("")
                )?;
            }
            emit(json, &rows, human.trim_end())
        }
        GardenCommand::ResumeSession { path, session_id } => {
            let root = folder_argument(&path)?;
            let instance = proxy
                .resume_session_v1(&path_of(&root), &session_id)
                .await?;
            emit(
                json,
                &serde_json::json!({"instance_id":instance}),
                &instance,
            )
        }
        GardenCommand::AgentHook { adapter: _ } => {
            if let Some(observation) =
                templar::garden_session::claude_observation(std::io::stdin().lock())?
            {
                proxy.observe_session_v1(&observation).await?;
            }
            // Never write hook stdout: Claude interprets it as hook decisions/context.
            Ok(())
        }
        GardenCommand::JournalInit { .. } => {
            unreachable!("handled before connecting to session bus")
        }
        GardenCommand::Contexts => {
            let contexts: serde_json::Value = serde_json::from_str(&proxy.contexts().await?)?;
            let mut human = String::new();
            for context in contexts.as_array().into_iter().flatten() {
                let _ = writeln!(
                    human,
                    "{:<9} {:>7.1}  {}  ({})",
                    context["stage"].as_str().unwrap_or("?"),
                    context["effective"].as_f64().unwrap_or(0.0),
                    context["root"].as_str().unwrap_or("?"),
                    context["summary"].as_str().unwrap_or("")
                );
            }
            if human.is_empty() {
                human.push_str("no places yet: the garden grows from what you open and launch\n");
            }
            emit(json, &contexts, human.trim_end())
        }
        GardenCommand::Why { path } => {
            let root = folder_argument(&path)?;
            let described = proxy.describe(&path_of(&root)).await?;
            if described.is_empty() {
                return emit(
                    json,
                    &serde_json::json!({"root": root, "place": false}),
                    &format!("{} is not a place the garden knows", root.display()),
                );
            }
            let context: serde_json::Value = serde_json::from_str(&described)?;
            let mut human = format!(
                "{}: {} ({}) — {}\n{}\nidle {} session(s), {:.1} garden-hours; pinned={} hidden={}",
                root.display(),
                context["stage"].as_str().unwrap_or("?"),
                context["form"].as_str().unwrap_or("plain"),
                context["summary"].as_str().unwrap_or(""),
                context["why"].as_str().unwrap_or("(never tended)"),
                context["idle_sessions"],
                context["idle_garden_hours"].as_f64().unwrap_or(0.0),
                context["pinned"],
                context["hidden"]
            );
            // What arrived here and has not been opened. Never a reason the place is a place:
            // a bloom is what an earned place shows, never what earned it.
            if let Some(total) = context["bloom_total"].as_u64().filter(|total| *total > 0) {
                let _ = write!(human, "\n{total} new:");
                for item in context["bloomed"].as_array().into_iter().flatten() {
                    let _ = write!(human, " {}", item.as_str().unwrap_or(""));
                }
            }
            emit(json, &context, &human)
        }
        GardenCommand::Tend { output } => {
            let report: serde_json::Value = serde_json::from_str(&proxy.tend(&output).await?)?;
            let mut human = String::new();
            for line in report["stages"].as_array().into_iter().flatten() {
                let _ = writeln!(human, "{}", line.as_str().unwrap_or(""));
            }
            for op in report["ops"].as_array().into_iter().flatten() {
                let _ = writeln!(human, "{}", op[1].as_str().unwrap_or(""));
            }
            if human.is_empty() {
                human.push_str("nothing to tend\n");
            }
            emit(json, &report, human.trim_end())
        }
        GardenCommand::Observe {
            kind,
            path,
            app_id,
            role,
        } => {
            let root = folder_argument(&path)?;
            proxy
                .observe(&kind, &path_of(&root), &app_id, &role)
                .await
                .context("recording Homegarden activity")?;
            emit(
                json,
                &serde_json::json!({"observed": kind, "path": root, "app_id": app_id, "role": role}),
                &format!("observed {kind} at {}", root.display()),
            )
        }
        other => garden_manage(&proxy, other, pinned, hidden, json).await,
    }
}

/// The record-changing garden verbs: pin, hide, forget, clear, and the clock.
async fn garden_manage(
    proxy: &HomeGardenProxy<'_>,
    command: GardenCommand,
    pinned: bool,
    hidden: bool,
    json: bool,
) -> Result<()> {
    let path_of = |path: &Path| path.to_string_lossy().into_owned();
    match command {
        GardenCommand::Keep { path } => {
            let root = folder_argument(&path)?;
            proxy.keep(&path_of(&root)).await?;
            emit(
                json,
                &serde_json::json!({"root": root, "pinned": true, "kept": true}),
                &format!("{} kept on Home", root.display()),
            )
        }
        GardenCommand::Pin { path } | GardenCommand::Unpin { path } => {
            let root = folder_argument(&path)?;
            proxy.pin(&path_of(&root), pinned).await?;
            emit(
                json,
                &serde_json::json!({"root": root, "pinned": pinned}),
                &format!("{} pinned={pinned}", root.display()),
            )
        }
        GardenCommand::Hide { path } | GardenCommand::Unhide { path } => {
            let root = folder_argument(&path)?;
            proxy.hide(&path_of(&root), hidden).await?;
            emit(
                json,
                &serde_json::json!({"root": root, "hidden": hidden}),
                &format!("{} hidden={hidden}", root.display()),
            )
        }
        GardenCommand::Forget { path } => {
            let root = folder_argument(&path)?;
            proxy.forget(&path_of(&root)).await?;
            emit(
                json,
                &serde_json::json!({"forgotten": root}),
                &format!("forgot {}", root.display()),
            )
        }
        GardenCommand::Clear => {
            proxy.forget_all().await?;
            emit(
                json,
                &serde_json::json!({"cleared": true}),
                "forgot every unpinned place",
            )
        }
        GardenCommand::Clock => {
            let clock: serde_json::Value = serde_json::from_str(&proxy.clock().await?)?;
            let hours = clock["active_ms"].as_f64().unwrap_or(0.0) / 3_600_000.0;
            emit(
                json,
                &clock,
                &format!(
                    "session {} · {hours:.2} garden-hours · {}",
                    clock["session"],
                    if clock["active"].as_bool().unwrap_or(false) {
                        "running"
                    } else {
                        "paused"
                    }
                ),
            )
        }
        GardenCommand::Contexts
        | GardenCommand::Sessions
        | GardenCommand::ResumeSession { .. }
        | GardenCommand::AgentHook { .. }
        | GardenCommand::JournalInit { .. }
        | GardenCommand::Why { .. }
        | GardenCommand::Tend { .. }
        | GardenCommand::Observe { .. } => {
            unreachable!("handled by garden_command")
        }
    }
}

fn emit(json: bool, value: &impl serde::Serialize, human: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{human}");
    }
    Ok(())
}

/// `templar dev`, whose subcommands are all synchronous — no bus, no session, just the filesystem
/// and a few external tools.
fn run_dev(command: Option<DevCommand>, repo: PathBuf, root: &Path, json: bool) -> Result<String> {
    let command = command.unwrap_or(DevCommand::Status { target: None });
    match command {
        DevCommand::Status { target } => dev::status(&repo, root, target.as_deref(), json),
        DevCommand::Repo { command } => match command {
            RepoCommand::Init {
                install_source,
                sysroot,
            } => dev::repo_init(repo, &sysroot, install_source),
            RepoCommand::Publish { build_set } => dev::repo_publish(repo, &build_set),
            RepoCommand::Status => dev::repo_status(repo),
            RepoCommand::Prune { keep } => dev::repo_prune(repo, keep),
            RepoCommand::Verify { set } => dev::repo::verify_command(repo, set.as_deref(), json),
        },
        DevCommand::Install { build_set, dry_run } => {
            dev::install_set(repo, root, build_set.as_deref(), dry_run)
        }
        DevCommand::Rollback { dry_run } => dev::rollback(repo, root, dry_run),
        DevCommand::Deploy {
            build_set,
            dry_run,
            auto_rollback,
            snapshot_state,
        } => dev::deploy(
            repo,
            root,
            &dev::DeployOptions {
                build_set,
                dry_run,
                auto_rollback,
                snapshot_state,
            },
        ),
    }
}

/// `templar doctor` — the integration checks, and the exit code a caller acts on.
///
/// `--preflight` runs only the live hardware preflight, so that an image check with no answer on a
/// live medium (a build set that is not installed there, say) cannot make the medium refuse to
/// install itself.
fn doctor_command(
    root: PathBuf,
    hardware: bool,
    preflight: bool,
    check: Option<String>,
    profile: Option<String>,
    json: bool,
) -> Result<()> {
    let context = doctor::Context {
        root,
        hardware: hardware || preflight,
    };
    let findings = if preflight {
        doctor::preflight::findings(&context)
    } else if let Some(requested) = check.as_deref() {
        if let Some(finding) = doctor::run_one_builtin(&context, requested) {
            vec![finding]
        } else if hardware {
            doctor::run_all_with_hardware(
                &context,
                &doctor::hardware::HardwareOptions { check, profile },
            )
        } else {
            anyhow::bail!(
                "unknown check {requested:?}; built-in checks are: {} (hardware cases need --hardware too)",
                doctor::builtin_ids().join(", ")
            );
        }
    } else {
        doctor::run_all_with_hardware(
            &context,
            &doctor::hardware::HardwareOptions { check, profile },
        )
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&findings)?);
    } else {
        let mut abbreviated = false;
        for finding in &findings {
            let detail = if findings.len() == 1 && finding.id == "file-index" {
                finding.detail.clone()
            } else {
                doctor::abbreviate(&finding.detail)
            };
            abbreviated |= detail != finding.detail;
            println!(
                "{:<12} {:<20} {}{}",
                finding.outcome.label(),
                finding.id,
                finding.summary,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                }
            );
        }
        println!("\n{}", doctor::summarise(&findings));
        if abbreviated {
            println!("(detail shortened; `templar doctor --json` prints it in full)");
        }
    }
    std::process::exit(doctor::exit_code(&findings));
}

/// `templar doctor freeze` — the freeze report. Fails only when the report cannot be written.
fn doctor_freeze(boot: String, out: Option<PathBuf>, json: bool) -> Result<()> {
    let runner = dev::cmd::RealCommandRunner {
        timeout: std::time::Duration::from_secs(120),
    };
    let report = doctor::freeze::collect(&doctor::freeze::Options { boot, out }, &runner)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.summary);
    }
    Ok(())
}

async fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Run {
            class,
            notify,
            command,
        } => {
            let class = if class == "interactive" {
                templar_types::WorkloadClass::Interactive
            } else {
                templar_types::WorkloadClass::BackgroundCompute
            };
            let status = templar::workload::run(class, command, notify).await?;
            std::process::exit(status);
        }
        Command::WorkloadGate { socket, command } => templar::workload::gate(&socket, &command),
        Command::Dev {
            command,
            repo,
            root,
            json,
        } => {
            print!("{}", run_dev(command, repo, &root, json)?);
            if json {
                println!();
            }
            Ok(())
        }
        Command::Doctor {
            command: Some(DoctorCommand::Freeze { boot, out, json }),
            ..
        } => doctor_freeze(boot, out, json),
        Command::Doctor {
            command: None,
            root,
            hardware,
            preflight,
            check,
            profile,
            json,
        } => doctor_command(root, hardware, preflight, check, profile, json),
        Command::App { command, json } => app(command, json).await,
        Command::Open {
            target,
            explain,
            project,
            json,
        } => open_target(&target, explain, &project, json).await,
        Command::Terminal(args) => terminal_command(args).await,
        Command::Pcmanfm {
            command:
                PcmanfmCommand::Repair {
                    settings,
                    terminals_list,
                    backup,
                },
            json,
        } => pcmanfm_repair(settings, terminals_list, backup, json),
        Command::Res { command, json } => {
            res_command(command.unwrap_or(ResCommand::Status), json).await
        }
        Command::Files { command, json } => files_command(command, json).await,
        Command::Account {
            command: AccountCommand::Password,
        } => account::password().await,
        Command::Archive { command } => match command {
            ArchiveCommand::Compress { output, gui, paths } => {
                archive::compress(paths, output, gui).await
            }
            ArchiveCommand::Extract {
                mode,
                directory,
                gui,
                paths,
            } => {
                let destination = match mode {
                    ArchiveMode::Here => archive::Destination::Here,
                    ArchiveMode::Smart => archive::Destination::Smart,
                    ArchiveMode::Directory => archive::Destination::Directory,
                };
                archive::extract(paths, destination, directory, gui).await
            }
        },
        Command::Status { command, json } => {
            status_command(command.unwrap_or(StatusCommand::All), json).await
        }
        Command::Trash { command, json } => {
            trash_command(command.unwrap_or(TrashCommand::List), json).await
        }
        Command::Update { command, json } => {
            update_command(command.unwrap_or(UpdateCommand::Status), json).await
        }
        Command::History { command, json } => {
            history_command(
                command.unwrap_or(HistoryCommand::Recent { limit: 20 }),
                json,
            )
            .await
        }
        Command::Providers { command, json } => {
            providers_command(command.unwrap_or(ProvidersCommand::Report), json).await
        }
        Command::Settings { command, json } => {
            settings_command(command.unwrap_or(SettingsCommand::List), json).await
        }
        Command::Wallpaper { command, json } => {
            templar::wallpaper::run(
                command.unwrap_or(templar::wallpaper::WallpaperCommand::Inspect),
                json,
            )
            .await
        }
        Command::Home { command, json } => home_command(command, json).await,
        Command::Garden { command, json } => {
            garden_command(command.unwrap_or(GardenCommand::Contexts), json).await
        }
        Command::Notify { command, json } => notify_command(command, json).await,
        Command::Project { command, json } => {
            project_command(command.unwrap_or(ProjectCommand::List), json).await
        }
        Command::Power { command, json } => {
            power_command(command.unwrap_or(PowerCommand::Get { source: None }), json).await
        }
        Command::Session { command, json } => {
            session_command(command.unwrap_or(SessionCommand::Status), json).await
        }
    }
}

async fn session_command(command: SessionCommand, json: bool) -> Result<()> {
    use templar::lifecycle::{Action, LifecycleClient};
    let connection = bus::session().await?;
    let proxy = templar_client::proxies::LifecycleProxy::new(&connection).await?;
    match command {
        SessionCommand::Status => {
            let mut verdicts = Vec::new();
            for action in Action::ALL {
                verdicts.push((action, LifecycleClient::can(&proxy, action).await?));
            }
            let rendered = templar::lifecycle::render_status(&verdicts);
            let structured: Vec<_> = verdicts
                .iter()
                .map(|(action, verdict)| (action.as_str(), verdict.clone()))
                .collect();
            emit(json, &structured, &rendered)
        }
        SessionCommand::Logout => {
            LifecycleClient::logout(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"session": "logout"}),
                "logging out",
            )
        }
        SessionCommand::Reboot => {
            LifecycleClient::reboot(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"session": "reboot"}),
                "restarting",
            )
        }
        SessionCommand::Poweroff => {
            LifecycleClient::power_off(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"session": "poweroff"}),
                "shutting down",
            )
        }
        SessionCommand::Suspend => {
            LifecycleClient::suspend(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"session": "suspend"}),
                "suspending",
            )
        }
        SessionCommand::Lock => {
            LifecycleClient::lock(&proxy).await?;
            emit(json, &serde_json::json!({"session": "lock"}), "locked")
        }
        SessionCommand::Events => session_events(&connection, json).await,
    }
}

/// Follow `Lifecycle1`'s signals until interrupted.
///
/// A raw match rule on the interface rather than one generated stream per signal: it is a third of
/// the code and it reports a signal this build of `templar` has never heard of, which is exactly what
/// you want from the command you leave running while you test a new daemon.
async fn session_events(connection: &zbus::Connection, json: bool) -> Result<()> {
    use futures_util::StreamExt as _;

    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.templaros.Session.Lifecycle1")?
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, connection, Some(16))
        .await
        .context("watching org.templaros.Session.Lifecycle1")?;
    while let Some(message) = stream.next().await {
        let message = message.context("reading a Lifecycle1 signal")?;
        let Some(member) = message.header().member().map(ToString::to_string) else {
            continue;
        };
        // Only `Sleeping` carries an argument; for the rest this is simply `None`.
        let going_to_sleep = message.body().deserialize::<bool>().ok();
        emit(
            json,
            &serde_json::json!({"event": member, "going_to_sleep": going_to_sleep}),
            &templar::lifecycle::render_edge(&member, going_to_sleep),
        )?;
    }
    Ok(())
}

async fn app(command: AppCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    match command {
        AppCommand::List => {
            let ids = AppIdentityProxy::new(&connection)
                .await?
                .list_apps()
                .await
                .context("AppIdentity1.ListApps")?;
            emit(json, &ids, &ids.join("\n"))
        }
        AppCommand::Instances => {
            let instances = AppIdentityProxy::new(&connection)
                .await?
                .list_instances()
                .await
                .context("AppIdentity1.ListInstances")?;
            let rows: Vec<Vec<String>> = instances
                .iter()
                .map(|(instance, app, unit, cgroup)| {
                    vec![instance.clone(), app.clone(), unit.clone(), cgroup.clone()]
                })
                .collect();
            emit(json, &instances, &templar::format::table(&rows))
        }
        AppCommand::Launch { app_id, arguments } => {
            let instance = LauncherProxy::new(&connection)
                .await?
                .launch(&app_id, arguments, "")
                .await
                .context("Launcher1.Launch")?;
            emit(json, &instance, &instance)
        }
    }
}

/// `templar open`'s machine-readable shape: the whole resolution chain, plus what was launched.
#[derive(serde::Serialize)]
struct OpenOutput<'a> {
    target: &'a str,
    app_id: &'a str,
    detected_mime: &'a str,
    reason: &'a str,
    explanation: Option<&'a String>,
    launched_instance: Option<String>,
}

async fn open_target(target: &str, explain: bool, project: &str, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let associations = AssociationsProxy::new(&connection).await?;
    let resolution = open::resolve(&associations, target, project, explain).await?;

    if explain {
        let output = OpenOutput {
            target: &resolution.target,
            app_id: &resolution.app_id,
            detected_mime: &resolution.detected_mime,
            reason: &resolution.reason,
            explanation: resolution.explanation.as_ref(),
            launched_instance: None,
        };
        return emit(json, &output, &open::render_explanation(&resolution));
    }

    let mut human = open::render_summary(&resolution);
    let mut launched = None;
    if resolution.is_resolved() {
        let launcher = LauncherProxy::new(&connection).await?;
        let instance = if resolution.kind == open::TargetKind::Path {
            let path = std::fs::canonicalize(target).context("resolving the file launch target")?;
            launcher
                .launch_files_v1(
                    &resolution.app_id,
                    vec![templar_types::uri_list::encode_path(&path)],
                    "",
                )
                .await
        } else if templar_types::uri_list::decode_uri(target).is_some() {
            launcher
                .launch_files_v1(&resolution.app_id, vec![target.to_owned()], "")
                .await
        } else {
            launcher
                .launch(&resolution.app_id, vec![target.to_owned()], "")
                .await
        }
        .context("Launcher1 file/URI launch")?;
        let _ = write!(human, "\nlaunched instance {instance}");
        launched = Some(instance);
    }
    let output = OpenOutput {
        target: &resolution.target,
        app_id: &resolution.app_id,
        detected_mime: &resolution.detected_mime,
        reason: &resolution.reason,
        explanation: resolution.explanation.as_ref(),
        launched_instance: launched,
    };
    emit(json, &output, &human)
}

/// The session connection and the providers report, as one fallible step: any failure here is
/// judged by [`templar::terminal::session_unreachable`] before it is reported.
async fn providers_report() -> zbus::Result<(zbus::Connection, Vec<(String, String)>)> {
    let connection = zbus::Connection::session().await?;
    let report = ProvidersProxy::new(&connection).await?.report().await?;
    Ok((connection, report))
}

async fn terminal_command(args: TerminalArgs) -> Result<()> {
    use templar::terminal::{self, FALLBACK_TERMINAL, Request};
    let TerminalArgs {
        working_directory,
        hold,
        dry_run,
        json,
        command,
    } = args;
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let request = Request::new(working_directory.as_deref(), &cwd, hold, command)?;
    let (connection, report) = match providers_report().await {
        Ok(found) => found,
        Err(error) if terminal::session_unreachable(&error) => {
            let arguments = request.fallback_arguments();
            let value = serde_json::json!({
                "route": "direct",
                "reason": error.to_string(),
                "executable": FALLBACK_TERMINAL,
                "working_directory": request.directory,
                "arguments": arguments,
            });
            if dry_run {
                return emit(
                    json,
                    &value,
                    &format!(
                        "org.templaros.Session is unreachable ({error}); would run {FALLBACK_TERMINAL} {arguments:?} in {}",
                        request.directory.display()
                    ),
                );
            }
            eprintln!(
                "templar: org.templaros.Session is unreachable ({error}); running {FALLBACK_TERMINAL} directly"
            );
            let error = std::os::unix::process::CommandExt::exec(
                std::process::Command::new(FALLBACK_TERMINAL)
                    .args(&arguments)
                    .current_dir(&request.directory)
                    .env("PWD", &request.directory),
            );
            return Err(error).with_context(|| format!("running {FALLBACK_TERMINAL}"));
        }
        Err(error) => return Err(error).context("Providers1.Report"),
    };
    let app_id = terminal::terminal_provider(&report)?;
    let arguments = request.provider_arguments()?;
    if dry_run {
        return emit(
            json,
            &serde_json::json!({ "route": "launcher1", "app_id": app_id, "arguments": arguments }),
            &format!("would launch {app_id} through Launcher1 with {arguments:?}"),
        );
    }
    let instance = LauncherProxy::new(&connection)
        .await?
        .launch(&app_id, arguments.clone(), "")
        .await
        .context("Launcher1.Launch")?;
    let unit = app_id
        .parse::<templar_types::AppId>()
        .ok()
        .zip(templar_types::InstanceId::parse(&instance))
        .map(|(app, instance)| templar_types::unit::unit_name(&app, instance));
    emit(
        json,
        &serde_json::json!({
            "route": "launcher1",
            "app_id": app_id,
            "arguments": arguments,
            "instance": instance,
            "unit": unit,
        }),
        &format!(
            "launched {app_id} instance {instance}{}",
            unit.as_deref()
                .map(|unit| format!(" ({unit})"))
                .unwrap_or_default()
        ),
    )
}

fn pcmanfm_repair(
    settings: Option<PathBuf>,
    terminals_list: Option<PathBuf>,
    backup: bool,
    json: bool,
) -> Result<()> {
    use templar_session_core::pcmanfm::{self, Outcome};
    let xdg = |variable: &str, under_home: &str| {
        std::env::var_os(variable)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(under_home)))
            .with_context(|| format!("neither {variable} nor HOME is set"))
    };
    let path = match settings {
        Some(path) => path,
        None => pcmanfm::settings_path(&xdg("XDG_CONFIG_HOME", ".config")?),
    };
    let list = match terminals_list {
        Some(list) => list,
        None => pcmanfm::terminals_list_path(&xdg("XDG_DATA_HOME", ".local/share")?),
    };
    let backup_path = backup.then(|| {
        let mut name = path.clone().into_os_string();
        name.push(".templaros-bak");
        PathBuf::from(name)
    });
    let outcome = pcmanfm::repair_file(&path, &pcmanfm::Host::from_env(), backup_path.as_deref())?;
    let mut human = match &outcome {
        Outcome::NoProfile => format!(
            "{} has no profile directory; PCManFM-Qt uses the system default",
            path.display()
        ),
        Outcome::Unchanged => format!("{}: every setting can work here; unchanged", path.display()),
        Outcome::Repaired { repairs } => {
            let mut human = format!("repaired {}", path.display());
            for repair in repairs {
                let _ = write!(
                    human,
                    "\n  [System] {}: {} -> {}",
                    repair.key,
                    repair.before.as_deref().unwrap_or("(absent)"),
                    repair.after
                );
            }
            if let Some(backup) = &backup_path {
                let _ = write!(human, "\n  original kept at {}", backup.display());
            }
            human
        }
    };
    let added = pcmanfm::ensure_terminal_entry(&list)?;
    let _ = write!(
        human,
        "\n{}: {}",
        list.display(),
        if added {
            "added the templar-terminal group"
        } else {
            "templar-terminal group already present"
        }
    );
    emit(
        json,
        &serde_json::json!({
            "settings": { "path": path, "result": outcome },
            "terminals_list": { "path": list, "added": added },
        }),
        &human,
    )
}

async fn res_command(command: ResCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = ResourcesProxy::new(&connection).await?;
    match command {
        ResCommand::Status => {
            let report_json = res::ResourcesClient::targets(&proxy).await?;
            let report = res::parse_report(&report_json)?;
            emit(json, &report, &res::render_report(&report))
        }
        ResCommand::Explain { instance_id } => {
            let explanation_json =
                res::ResourcesClient::explain_target(&proxy, &instance_id).await?;
            let explanation: res::Explanation = serde_json::from_str(&explanation_json)
                .context("invalid Resources1.ExplainTarget observation")?;
            emit(json, &explanation, &res::render_explanation(&explanation))
        }
        ResCommand::SetStage { stage } => {
            res::ResourcesClient::set_stage(&proxy, &stage).await?;
            emit(
                json,
                &serde_json::json!({"stage": stage}),
                &format!("containment stage set to {stage}"),
            )
        }
        ResCommand::Traces { limit } => {
            let traces_json = res::ResourcesClient::recent_traces(&proxy, limit).await?;
            let traces = res::parse_traces(&traces_json);
            emit(json, &traces, &res::render_traces(&traces))
        }
    }
}

async fn files_command(command: FilesCommand, json: bool) -> Result<()> {
    if let FilesCommand::SearchHere { directory, query } = &command {
        let directory = if directory.is_absolute() {
            directory.clone()
        } else {
            std::env::current_dir()?.join(directory)
        };
        anyhow::ensure!(
            directory.is_dir(),
            "Search Here requires an existing directory"
        );
        let connection = bus::session().await?;
        let lifecycle = LifecycleProxy::new(&connection).await?;
        lifecycle
            .open_launcher_v1(
                "files",
                &templar_types::uri_list::encode_path(&directory),
                query,
            )
            .await?;
        return emit(
            json,
            &serde_json::json!({"opened": true}),
            "Opened Files search in the launcher",
        );
    }
    let connection = bus::session().await?;
    match command {
        FilesCommand::SearchHere { .. } => {
            unreachable!("handled before connecting to the session bus")
        }
        FilesCommand::Volumes => {
            let proxy = FilesVolumesProxy::new(&connection).await?;
            let volumes = files::parse_volumes(&files::VolumesClient::list(&proxy).await?)?;
            emit(json, &volumes, &files::render_volumes(&volumes))
        }
        FilesCommand::Operations { limit } => {
            let proxy = FilesOperationsProxy::new(&connection).await?;
            let operations =
                files::parse_operations(&files::OperationsClient::recent(&proxy, limit).await?)?;
            emit(json, &operations, &files::render_operations(&operations))
        }
        FilesCommand::Cancel { op_id } => {
            let proxy = FilesOperationsProxy::new(&connection).await?;
            files::OperationsClient::cancel(&proxy, &op_id).await?;
            emit(
                json,
                &serde_json::json!({"cancelled": op_id}),
                &format!("asked {op_id} to stop at its next step boundary"),
            )
        }
        FilesCommand::Undo { op_id } => {
            let proxy = FilesOperationsProxy::new(&connection).await?;
            let (can_undo, reason) = files::OperationsClient::can_undo(&proxy, &op_id).await?;
            if !can_undo {
                // Not an error: "this cannot be undone" is a legitimate, expected answer, and the
                // reason is the whole point of asking first.
                return emit(
                    json,
                    &serde_json::json!({"undone": false, "reason": reason}),
                    &format!("{op_id} cannot be undone: {reason}"),
                );
            }
            let undo_id = files::OperationsClient::undo(&proxy, &op_id).await?;
            emit(
                json,
                &serde_json::json!({"undone": true, "undo_op_id": undo_id}),
                &format!("undoing {op_id} as operation {undo_id}"),
            )
        }
        FilesCommand::Recents { app, limit } => {
            let proxy = FilesRecentsProxy::new(&connection).await?;
            let entries =
                files::parse_recents(&files::RecentsClient::list(&proxy, &app, limit).await?)?;
            emit(json, &entries, &files::render_recents(&entries))
        }
        FilesCommand::Suggest { app, operation } => {
            let proxy = FilesRecentsProxy::new(&connection).await?;
            let suggestion = files::parse_suggestion(
                &files::RecentsClient::suggest_directory(&proxy, &app, &operation).await?,
            )?;
            emit(json, &suggestion, &files::render_suggestion(&suggestion))
        }
        FilesCommand::Search { query, limit } => {
            let proxy = FilesSearchProxy::new(&connection).await?;
            let hits_json = files::SearchClient::search(&proxy, &query, Vec::new(), limit).await?;
            let hits: serde_json::Value =
                serde_json::from_str(&hits_json).context("parsing Files.Search1.Search JSON")?;
            emit(json, &hits, &render_search_hits(&hits))
        }
        FilesCommand::Index { command } => {
            let proxy = FilesIndexProxy::new(&connection).await?;
            files_index_command(&proxy, command, json).await
        }
        FilesCommand::Bookmarks {
            command: BookmarksCommand::Export,
        } => {
            let proxy = templar_client::proxies::FilesBrowseProxy::new(&connection).await?;
            let response: templar_filed::bookmarks::ExportResult =
                serde_json::from_str(&proxy.export_bookmarks().await?)
                    .context("parsing Files.Browse1.ExportBookmarks JSON")?;
            let mut detail = format!(
                "Added {} bookmark(s), {} already present: {}",
                response.added,
                response.already_present,
                response.path.display()
            );
            if let Some(backup) = &response.backup {
                detail.push_str(&format!("\nBackup: {}", backup.display()));
            }
            emit(json, &response, &detail)
        }
        FilesCommand::Path { path, form } => {
            let proxy = FilesActionsProxy::new(&connection).await?;
            if let Some(form) = form {
                let text =
                    templar_client::file_index::render_native_path(&connection, &path, &form)
                        .await?;
                emit(json, &text, &text)
            } else {
                let forms = files::ActionsClient::forms(&proxy).await?;
                emit(json, &forms, &forms.join("\n"))
            }
        }
    }
}

/// `templar files index status|search|reconcile` — the shared index (`Files.Index1`), additive to
/// `Files.Search1`'s compatibility path above.
async fn files_index_command(
    proxy: &FilesIndexProxy<'_>,
    command: IndexCommand,
    json: bool,
) -> Result<()> {
    match command {
        IndexCommand::Status => {
            let response = files::parse_index_roots(&files::IndexClient::roots(proxy).await?)?;
            emit(json, &response, &files::render_index_roots(&response))
        }
        IndexCommand::Search {
            query,
            limit,
            print0,
        } => {
            let query_json =
                serde_json::json!({ "text": query, "page_size": limit.clamp(1, 1000) });
            let (cursor, generation, page_json) =
                files::IndexClient::search(proxy, &query_json.to_string()).await?;
            // Close on parsing, paging and output failures too. A completed CLI invocation is
            // not a five-minute server cursor or a watch-renewal heartbeat.
            let collected: Result<_> = async {
                let mut result = files::parse_index_search(&page_json)?;
                anyhow::ensure!(
                    result.cursor == cursor && result.generation == generation,
                    "Index1.Search returned inconsistent cursor/generation"
                );
                let mut offset = u32::try_from(result.hits.len())?;
                result.hits.truncate(limit as usize);
                while result.hits.len() < limit as usize {
                    let count = (limit - u32::try_from(result.hits.len())?).min(1000);
                    let page: templar_client::file_index::IndexPage = serde_json::from_str(
                        &files::IndexClient::fetch_page(proxy, &cursor, offset, count).await?,
                    )?;
                    result.validate_page(&page, offset, count)?;
                    let consumed = u32::try_from(page.hits.len())?
                        .checked_add(page.dropped)
                        .context("index cursor offset overflow")?;
                    if consumed == 0 {
                        break;
                    }
                    offset = offset
                        .checked_add(consumed)
                        .context("index cursor offset overflow")?;
                    result.hits.extend(page.hits);
                }
                Ok(result)
            }
            .await;
            let closed = files::IndexClient::close_cursor(proxy, &cursor).await;
            let result = collected?;
            closed?;
            if print0 {
                files::print0_index_hits(&result, &mut std::io::stdout())?;
                Ok(())
            } else {
                emit(json, &result, &files::render_index_hits(&result))
            }
        }
        IndexCommand::Reconcile { roots } => {
            let observed = files::parse_index_roots(&files::IndexClient::roots(proxy).await?)?;
            let request =
                serde_json::json!({"cache_identity": observed.cache_identity, "roots": roots});
            let request_id = files::IndexClient::reconcile(proxy, &request.to_string()).await?;
            emit(
                json,
                &serde_json::json!({"request_id": request_id}),
                &format!("reconcile scheduled ({request_id}); watch `templar files index status`"),
            )
        }
        IndexCommand::Policy {
            path,
            expected_hash,
        } => {
            let policy =
                templar_client::file_index::root_policy(proxy, &path, &expected_hash).await?;
            emit(json, &policy, &serde_json::to_string_pretty(&policy)?)
        }
        IndexCommand::Configure {
            path,
            expected_hash,
            policy_json,
        } => {
            let policy = policy_json
                .map(|value| serde_json::from_str::<templar_file_index::RootPolicy>(&value))
                .transpose()?
                .unwrap_or_else(templar_file_index::RootPolicy::default_home);
            let root =
                templar_client::file_index::configure_root(proxy, &path, &policy, &expected_hash)
                    .await?;
            emit(
                json,
                &serde_json::json!({"root": root, "path_b64": templar_client::file_index::encode_path(&path)}),
                &format!(
                    "policy stored for {}; watch `templar files index status`",
                    path.display()
                ),
            )
        }
        IndexCommand::Remove {
            path,
            expected_hash,
        } => {
            templar_client::file_index::remove_root(proxy, &path, &expected_hash).await?;
            emit(
                json,
                &serde_json::json!({"removed": true, "path_b64": templar_client::file_index::encode_path(&path)}),
                &format!("removed {}", path.display()),
            )
        }
    }
}

/// How far one step moves a level.
///
/// Percentage points, and the same for both, because a person pressing a volume key and a person
/// pressing a brightness key are doing the same thing and should not have to learn two different
/// step sizes. Five is small enough to be fine-grained and large enough that reaching either end
/// is a handful of presses rather than twenty.
const STEP_PERCENT: i32 = 5;

/// `templar status` — what the hardware is doing, and the controls for the two halves that have any.
async fn status_command(command: StatusCommand, json: bool) -> Result<()> {
    use templar::status::{AudioClient, BacklightClient, BatteryClient};
    let connection = bus::session().await?;
    match command {
        StatusCommand::All => {
            let audio = templar::status::parse_audio(
                &AudioClient::get(&AudioProxy::new(&connection).await?).await?,
            )?;
            let backlight = templar::status::parse_backlight(
                &BacklightClient::get(&BacklightProxy::new(&connection).await?).await?,
            )?;
            let battery = templar::status::parse_battery(
                &BatteryClient::get(&BatteryProxy::new(&connection).await?).await?,
            )?;
            emit(
                json,
                &serde_json::json!({
                    "audio": audio, "backlight": backlight, "battery": battery
                }),
                &templar::status::render_all(&audio, &backlight, &battery),
            )
        }
        StatusCommand::Volume { set } => {
            let proxy = AudioProxy::new(&connection).await?;
            if let Some(percent) = set {
                AudioClient::set_volume(&proxy, percent).await?;
            }
            let audio = templar::status::parse_audio(&AudioClient::get(&proxy).await?)?;
            emit(json, &audio, &templar::status::render_audio(&audio))
        }
        StatusCommand::Louder | StatusCommand::Quieter => {
            let proxy = AudioProxy::new(&connection).await?;
            let delta = if matches!(command, StatusCommand::Louder) {
                STEP_PERCENT
            } else {
                -STEP_PERCENT
            };
            // One call, so the service does the read-modify-write and two clients on the same key
            // cannot each undo the other.
            let percent = AudioClient::adjust_volume(&proxy, delta).await?;
            emit(
                json,
                &serde_json::json!({"volume_percent": percent}),
                &format!("volume     {percent}%"),
            )
        }
        StatusCommand::Mute => {
            let proxy = AudioProxy::new(&connection).await?;
            let muted = AudioClient::toggle_muted(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"muted": muted}),
                if muted {
                    "volume     muted"
                } else {
                    "volume     unmuted"
                },
            )
        }
        StatusCommand::Brightness { set } => {
            let proxy = BacklightProxy::new(&connection).await?;
            if let Some(percent) = set {
                BacklightClient::set_brightness(&proxy, percent).await?;
            }
            let backlight = templar::status::parse_backlight(&BacklightClient::get(&proxy).await?)?;
            emit(
                json,
                &backlight,
                &templar::status::render_backlight(&backlight),
            )
        }
        StatusCommand::Brighter | StatusCommand::Dimmer => {
            let proxy = BacklightProxy::new(&connection).await?;
            let delta = if matches!(command, StatusCommand::Brighter) {
                STEP_PERCENT
            } else {
                -STEP_PERCENT
            };
            let percent = BacklightClient::adjust_brightness(&proxy, delta).await?;
            emit(
                json,
                &serde_json::json!({"percent": percent}),
                &format!("brightness {percent}%"),
            )
        }
        StatusCommand::Battery => {
            let proxy = BatteryProxy::new(&connection).await?;
            let battery = templar::status::parse_battery(&BatteryClient::get(&proxy).await?)?;
            emit(json, &battery, &templar::status::render_battery(&battery))
        }
    }
}

/// `templar trash` — the trash as a listing, and the three things done with it.
///
/// The two destructive verbs demand `--yes` rather than prompting: this is the surface a script
/// reaches for, and a permanent delete that can happen by pressing return in the wrong terminal
/// is the one mistake in this whole area that nothing can put right.
async fn trash_command(command: TrashCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = FilesTrashProxy::new(&connection).await?;
    match command {
        TrashCommand::List => {
            let entries = files::parse_trash(&files::TrashClient::list(&proxy).await?)?;
            emit(json, &entries, &files::render_trash(&entries))
        }
        TrashCommand::Restore { trashed_paths } => {
            let op_id = files::TrashClient::restore(&proxy, trashed_paths.clone(), "").await?;
            emit(
                json,
                &serde_json::json!({"op_id": op_id, "restoring": trashed_paths}),
                &format!(
                    "restoring {} item(s) as operation {op_id}",
                    trashed_paths.len()
                ),
            )
        }
        TrashCommand::Delete { trashed_paths, yes } => {
            if !yes {
                return refuse_without_yes("templar trash delete");
            }
            let op_id =
                files::TrashClient::delete_permanently(&proxy, trashed_paths.clone(), "").await?;
            emit(
                json,
                &serde_json::json!({"op_id": op_id, "deleting": trashed_paths}),
                &format!(
                    "deleting {} item(s) permanently as operation {op_id}",
                    trashed_paths.len()
                ),
            )
        }
        TrashCommand::Empty { yes } => {
            if !yes {
                return refuse_without_yes("templar trash empty");
            }
            let op_id = files::TrashClient::empty(&proxy, "").await?;
            if op_id.is_empty() {
                // Not an error: an already-empty trash is the state the verb asks for.
                return emit(
                    json,
                    &serde_json::json!({"op_id": "", "emptied": false}),
                    "the trash is already empty",
                );
            }
            emit(
                json,
                &serde_json::json!({"op_id": op_id, "emptied": true}),
                &format!("emptying the trash as operation {op_id}"),
            )
        }
    }
}

fn refuse_without_yes(verb: &str) -> Result<()> {
    anyhow::bail!("{verb} cannot be undone; pass --yes to go ahead")
}

/// `Files.Search1.Search` returns `[[entry, rank], ...]`; render the path and why it matched.
fn render_search_hits(hits: &serde_json::Value) -> String {
    let Some(array) = hits.as_array() else {
        return "no matches".to_owned();
    };
    if array.is_empty() {
        return "no matches".to_owned();
    }
    let rows: Vec<Vec<String>> = array
        .iter()
        .map(|hit| {
            let path = hit
                .get(0)
                .and_then(|entry| entry.get("path"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let rank = hit
                .get(1)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("match");
            vec![path.to_owned(), rank.to_owned()]
        })
        .collect();
    templar::format::table(&rows)
}

/// `templar update deployments`'s machine-readable shape: the backend and every known deployment.
#[derive(serde::Serialize)]
struct DeploymentsOutput<'a> {
    backend: &'a str,
    deployments: &'a [update::Deployment],
}

/// `templar update hardware`'s machine-readable shape: the detected profile and the declared mode.
#[derive(serde::Serialize)]
struct HardwareOutput<'a> {
    profile: &'a update::GraphicsProfile,
    gpu_mode: &'a str,
}

/// `templar update` dispatch: split by `org.templaros.Host.*` sub-interface (one function per
/// interface, per `clippy::too_many_lines`) since one flat match over all of them reads as one
/// interface even though it is five.
async fn update_command(command: UpdateCommand, json: bool) -> Result<()> {
    // The host daemon is on the system bus, unlike everything else this CLI talks to.
    let connection = bus::system().await?;
    match command {
        UpdateCommand::Status | UpdateCommand::Plan { .. } | UpdateCommand::History => {
            update_updates_command(&connection, command, json).await
        }
        UpdateCommand::Deployments
        | UpdateCommand::Stage { .. }
        | UpdateCommand::MarkGood
        | UpdateCommand::Rollback { .. } => {
            update_deployments_command(&connection, command, json).await
        }
        UpdateCommand::Drivers | UpdateCommand::Violations | UpdateCommand::RepairDrivers => {
            update_drivers_command(&connection, command, json).await
        }
        UpdateCommand::Storage { .. }
        | UpdateCommand::EnforceStorage { .. }
        | UpdateCommand::TrimOrder => update_storage_command(&connection, command, json).await,
        UpdateCommand::Hardware | UpdateCommand::SetGpuMode { .. } => {
            update_hardware_command(&connection, command, json).await
        }
    }
}

async fn update_updates_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostUpdatesProxy::new(connection).await?;
    match command {
        UpdateCommand::Status => {
            let current = update::parse_current(&update::UpdatesClient::current(&proxy).await?)?;
            match current {
                Some(transaction) => emit(
                    json,
                    &transaction,
                    &update::render_transaction(&transaction),
                ),
                None => emit(
                    json,
                    &serde_json::json!({"transaction": null}),
                    "no update transaction in flight",
                ),
            }
        }
        UpdateCommand::Plan { changes_json } => {
            let id = update::UpdatesClient::plan(&proxy, &changes_json).await?;
            emit(
                json,
                &serde_json::json!({"transaction_id": id}),
                &format!("planned transaction {id}"),
            )
        }
        UpdateCommand::History => {
            let transactions =
                update::parse_transactions(&update::UpdatesClient::history(&proxy).await?)?;
            emit(
                json,
                &transactions,
                &update::render_transactions(&transactions),
            )
        }
        _ => unreachable!("update_command only routes Updates1 variants here"),
    }
}

async fn update_deployments_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostDeploymentsProxy::new(connection).await?;
    match command {
        UpdateCommand::Deployments => {
            let backend = update::DeploymentsClient::backend(&proxy).await?;
            let deployments =
                update::parse_deployments(&update::DeploymentsClient::list(&proxy).await?)?;
            emit(
                json,
                &DeploymentsOutput {
                    backend: &backend,
                    deployments: &deployments,
                },
                &update::render_deployments(&backend, &deployments),
            )
        }
        UpdateCommand::Stage { id, tries } => {
            update::DeploymentsClient::stage(&proxy, &id, tries).await?;
            emit(
                json,
                &serde_json::json!({"staged": id, "tries": tries}),
                &format!("staged {id} for a trial boot ({tries} tries)"),
            )
        }
        UpdateCommand::MarkGood => {
            update::DeploymentsClient::mark_good(&proxy).await?;
            emit(
                json,
                &serde_json::json!({"marked_good": true}),
                "current deployment marked good",
            )
        }
        UpdateCommand::Rollback { id } => {
            update::DeploymentsClient::rollback(&proxy, &id).await?;
            emit(
                json,
                &serde_json::json!({"rolled_back_to": id}),
                &format!("rolled back to {id}"),
            )
        }
        _ => unreachable!("update_command only routes Deployments1 variants here"),
    }
}

async fn update_drivers_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostDriversProxy::new(connection).await?;
    match command {
        UpdateCommand::Drivers => {
            let plan = update::parse_driver_plan(&update::DriversClient::plan(&proxy).await?)?;
            emit(json, &plan, &update::render_driver_plan(&plan))
        }
        UpdateCommand::Violations => {
            let violations =
                update::parse_violations(&update::DriversClient::violations(&proxy).await?)?;
            let human = if violations.is_empty() {
                "no support violations".to_owned()
            } else {
                violations
                    .iter()
                    .map(update::SupportViolation::describe)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            emit(json, &violations, &human)
        }
        UpdateCommand::RepairDrivers => {
            let lock = update::parse_version_lock(&update::DriversClient::repair(&proxy).await?)?;
            emit(json, &lock, &update::render_version_lock(&lock))
        }
        _ => unreachable!("update_command only routes Drivers1 variants here"),
    }
}

async fn update_storage_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostStorageProxy::new(connection).await?;
    match command {
        UpdateCommand::Storage { mount } => {
            let usage = update::parse_usage(&update::StorageClient::usage(&proxy, &mount).await?)?;
            emit(json, &usage, &update::render_usage(&usage))
        }
        UpdateCommand::EnforceStorage { mount } => {
            let reports =
                update::parse_trim_reports(&update::StorageClient::enforce(&proxy, &mount).await?)?;
            emit(json, &reports, &update::render_trim_reports(&reports))
        }
        UpdateCommand::TrimOrder => {
            let order = update::StorageClient::trim_order(&proxy).await?;
            emit(json, &order, &order.join(" → "))
        }
        _ => unreachable!("update_command only routes Storage1 variants here"),
    }
}

async fn update_hardware_command(
    connection: &zbus::Connection,
    command: UpdateCommand,
    json: bool,
) -> Result<()> {
    let proxy = HostHardwareProxy::new(connection).await?;
    match command {
        UpdateCommand::Hardware => {
            let profile = update::parse_graphics_profile(
                &update::HardwareClient::graphics_profile(&proxy).await?,
            )?;
            let gpu_mode = update::HardwareClient::gpu_mode(&proxy).await?;
            let human = update::render_graphics_profile(&profile, &gpu_mode);
            emit(
                json,
                &HardwareOutput {
                    profile: &profile,
                    gpu_mode: &gpu_mode,
                },
                &human,
            )
        }
        UpdateCommand::SetGpuMode { mode } => {
            update::HardwareClient::set_gpu_mode(&proxy, &mode).await?;
            emit(
                json,
                &serde_json::json!({"gpu_mode": mode}),
                &format!("GPU mode set to {mode}"),
            )
        }
        _ => unreachable!("update_command only routes Hardware1 variants here"),
    }
}

async fn history_command(command: HistoryCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = HistoryProxy::new(&connection).await?;
    match command {
        HistoryCommand::Recent { limit } => {
            let events =
                history::parse_events(&history::HistoryClient::recent(&proxy, limit).await?);
            emit(json, &events, &history::render_events(&events))
        }
        HistoryCommand::Search { query, limit } => {
            let events = history::parse_events(
                &history::HistoryClient::search(&proxy, &query, limit).await?,
            );
            emit(json, &events, &history::render_events(&events))
        }
        HistoryCommand::Undo { event_id } => {
            let undone = history::HistoryClient::undo(&proxy, &event_id).await?;
            emit(
                json,
                &serde_json::json!({"undone": undone, "event_id": event_id}),
                if undone {
                    "undone"
                } else {
                    "nothing was undone (the event is not reversible, or already was)"
                },
            )
        }
    }
}

async fn providers_command(command: ProvidersCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = ProvidersProxy::new(&connection).await?;
    match command {
        ProvidersCommand::Report => {
            let raw = providers::ProvidersClient::report(&proxy).await?;
            let parsed: Vec<(String, providers::Resolution)> = raw
                .iter()
                .map(|(role, resolution_json)| {
                    Ok((role.clone(), providers::parse_resolution(resolution_json)?))
                })
                .collect::<Result<Vec<_>>>()?;
            emit(json, &raw, &providers::render_report(&parsed))
        }
        ProvidersCommand::Select { role, app_id } => {
            // Fail here rather than at the daemon: a typo'd role is the likeliest mistake, and
            // the valid set is known locally.
            if !ProviderRole::ALL.iter().any(|r| r.as_str() == role) {
                let valid: Vec<&str> = ProviderRole::ALL.iter().map(|r| r.as_str()).collect();
                anyhow::bail!(
                    "unknown provider role {role:?}; expected one of {}",
                    valid.join(", ")
                );
            }
            providers::ProvidersClient::select(&proxy, &role, &app_id).await?;
            emit(
                json,
                &serde_json::json!({"role": role, "app_id": app_id}),
                &format!("{app_id} is now the selected provider for {role}"),
            )
        }
    }
}

async fn settings_command(command: SettingsCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = SettingsProxy::new(&connection).await?;
    match command {
        SettingsCommand::List => {
            let values = settings::SettingsClient::list(&proxy).await?;
            emit(json, &values, &settings::render_list(&values))
        }
        SettingsCommand::Schema => {
            let schema = settings::SettingsClient::schema(&proxy).await?;
            emit(json, &schema, &settings::render_schema(&schema))
        }
        SettingsCommand::Get { key } => {
            let value_json = settings::SettingsClient::get(&proxy, &key).await?;
            emit(json, &value_json, &settings::render_value(&value_json))
        }
        SettingsCommand::Set {
            key,
            value_json,
            origin,
        } => {
            settings::SettingsClient::set(&proxy, &key, &value_json, &origin).await?;
            emit(
                json,
                &serde_json::json!({"key": key, "value": value_json}),
                &format!("{key} = {}", settings::render_value(&value_json)),
            )
        }
        SettingsCommand::Reset { key, origin } => {
            settings::SettingsClient::reset(&proxy, &key, &origin).await?;
            emit(
                json,
                &serde_json::json!({"reset": key}),
                &format!("{key} reset to its schema default"),
            )
        }
        SettingsCommand::Changes { limit } => {
            let changes =
                settings::parse_changes(&settings::SettingsClient::changes(&proxy, limit).await?);
            emit(json, &changes, &settings::render_changes(&changes))
        }
    }
}

async fn project_command(command: ProjectCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = ProjectsProxy::new(&connection).await?;
    match command {
        ProjectCommand::List => {
            let projects = project::ProjectsClient::list(&proxy).await?;
            emit(json, &projects, &project::render_list(&projects))
        }
        ProjectCommand::Create {
            project_id,
            name,
            root,
        } => {
            project::ProjectsClient::create(&proxy, &project_id, &name, &root).await?;
            emit(
                json,
                &serde_json::json!({"created": project_id}),
                &format!("created project {project_id}"),
            )
        }
        ProjectCommand::CreateHere { path } => {
            let root = folder_argument(&path)?;
            let taken: Vec<String> = project::ProjectsClient::list(&proxy)
                .await?
                .into_iter()
                .map(|(id, ..)| id)
                .collect();
            let project_id = project::project_id_for(&root, &taken);
            let name = root.file_name().map_or_else(
                || root.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            project::ProjectsClient::create(&proxy, &project_id, &name, &root.to_string_lossy())
                .await?;
            emit(
                json,
                &serde_json::json!({"created": project_id, "root": root}),
                &format!("created project {project_id} at {}", root.display()),
            )
        }
        ProjectCommand::Open { project_id } => {
            project::ProjectsClient::open(&proxy, &project_id).await?;
            emit(
                json,
                &serde_json::json!({"opened": project_id}),
                &format!("opened project {project_id}"),
            )
        }
        ProjectCommand::Close { project_id } => {
            project::ProjectsClient::close(&proxy, &project_id).await?;
            emit(
                json,
                &serde_json::json!({"closed": project_id}),
                &format!("closed project {project_id}"),
            )
        }
        ProjectCommand::Remove { project_id } => {
            project::ProjectsClient::remove(&proxy, &project_id).await?;
            emit(
                json,
                &serde_json::json!({"removed": project_id}),
                &format!("removed project {project_id}"),
            )
        }
        other => project_detail_command(&proxy, other, json).await,
    }
}

/// The three project subcommands that read or edit one project's semantics, split out so
/// `project_command` stays a short list of named steps.
async fn project_detail_command(
    proxy: &ProjectsProxy<'_>,
    command: ProjectCommand,
    json: bool,
) -> Result<()> {
    match command {
        ProjectCommand::Show { project_id } => {
            let payload = project::ProjectsClient::describe(proxy, &project_id).await?;
            let current = project::ProjectsClient::get_current(proxy)
                .await
                .unwrap_or_default();
            let view = project::parse_project(&payload)?;
            let rendered = project::render_show(&view, current == project_id);
            // `--json` hands back the daemon's own payload verbatim; the human form never does.
            if json {
                println!("{payload}");
                Ok(())
            } else {
                println!("{rendered}");
                Ok(())
            }
        }
        ProjectCommand::Set {
            project_id,
            key,
            value,
        } => {
            // `state` has its own method, because archiving is a transition and not a field edit.
            if key == "state" {
                project::ProjectsClient::set_state(proxy, &project_id, &value).await?;
            } else {
                let changes = project::changes_json(&key, &value)?;
                project::ProjectsClient::update(proxy, &project_id, &changes).await?;
            }
            emit(
                json,
                &serde_json::json!({"project": project_id, "set": key, "to": value}),
                &format!("{project_id}: {key} is now {value}"),
            )
        }
        ProjectCommand::Current { project_id } => match project_id {
            None => {
                let current = project::ProjectsClient::get_current(proxy).await?;
                emit(
                    json,
                    &serde_json::json!({"current": current}),
                    &if current.is_empty() {
                        "no current project".to_owned()
                    } else {
                        current.clone()
                    },
                )
            }
            Some(project_id) => {
                let target = if project_id == "none" {
                    String::new()
                } else {
                    project_id
                };
                project::ProjectsClient::set_current(proxy, &target).await?;
                emit(
                    json,
                    &serde_json::json!({"current": target}),
                    &if target.is_empty() {
                        "cleared the current project".to_owned()
                    } else {
                        format!("working in {target}")
                    },
                )
            }
        },
        _ => unreachable!("project_command handles the other subcommands itself"),
    }
}

async fn power_command(command: PowerCommand, json: bool) -> Result<()> {
    let connection = bus::session().await?;
    let proxy = PowerProxy::new(&connection).await?;
    match command {
        PowerCommand::Get { source } => {
            let sources: Vec<String> = match source {
                Some(source) => vec![source],
                None => vec!["battery".to_owned(), "mains".to_owned()],
            };
            let mut profiles = Vec::new();
            let mut human = Vec::new();
            for source in &sources {
                let profile_json = proxy
                    .get_profile(source)
                    .await
                    .context("Power1.GetProfile")?;
                let profile = power::parse_profile(&profile_json)?;
                if !json {
                    human.push(power::render_profile(source, &profile));
                }
                profiles.push((source.clone(), profile));
            }
            let observed: templar_session_core::power::PowerObservation =
                serde_json::from_str(&proxy.inspect().await.context("Power1.Inspect")?)
                    .context("parsing native Power1 readback")?;
            #[derive(serde::Serialize)]
            struct PowerReport<'a> {
                stored: &'a [(String, templar_session_core::power::PowerProfile)],
                observed: &'a templar_session_core::power::PowerObservation,
            }
            let display = if json {
                String::new()
            } else {
                format!(
                    "{}\nobserved native policy:\n{}",
                    human.join("\n"),
                    serde_json::to_string_pretty(&observed)?
                )
            };
            emit(
                json,
                &PowerReport {
                    stored: &profiles,
                    observed: &observed,
                },
                &display,
            )
        }
        PowerCommand::Reset { source } => {
            proxy
                .reset_profile(&source)
                .await
                .context("Power1.ResetProfile")?;
            emit(
                json,
                &serde_json::json!({"reset": source}),
                &format!("{source} profile reset to the distribution default"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    mod bash_fixture {
        include!("bash_hook_fixture.rs");
    }

    #[test]
    fn a_closed_stdout_pipe_kills_quietly_instead_of_panicking() {
        // `signal` returns the previous disposition, so calling it a second time proves the
        // first call's effect: after the restore the disposition reads back as the default.
        restore_sigpipe_default();
        #[allow(unsafe_code)]
        let current = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        assert_eq!(current, libc::SIG_DFL);
    }

    /// The verb the shipped prompt hook calls. Its shape is a contract with a shell script
    /// that no compiler checks, so the parse is asserted here rather than discovered on a
    /// desktop where the hook silently records nothing.
    #[test]
    fn garden_observe_parses_the_shape_the_prompt_hook_sends() {
        let cli = Cli::parse_from([
            "templar",
            "garden",
            "observe",
            "terminal-entered",
            "/home/dk/src",
            "--role",
            "terminal",
        ]);
        let Command::Garden { command, .. } = cli.command else {
            panic!("not the garden subcommand")
        };
        let GardenCommand::Observe {
            kind,
            path,
            app_id,
            role,
        } = command.expect("a garden subcommand")
        else {
            panic!("not the observe verb")
        };
        assert_eq!(kind, "terminal-entered");
        assert_eq!(path, PathBuf::from("/home/dk/src"));
        assert!(app_id.is_empty());
        assert_eq!(role, "terminal");
    }

    /// The verb the completion hooks call. Same contract rule as the garden hook's: the
    /// shape is asserted here because no compiler checks the shell scripts.
    #[test]
    fn notify_terminal_completed_parses_the_shape_the_hooks_send() {
        let cli = Cli::parse_from([
            "templar",
            "notify",
            "terminal-completed",
            "--elapsed-ms",
            "12034",
            "--status",
            "2",
            "--label",
            "make",
        ]);
        let Command::Notify { command, json } = cli.command else {
            panic!("not the notify subcommand")
        };
        assert!(!json);
        let NotifyCommand::TerminalCompleted {
            elapsed_ms,
            status,
            label,
            emulator,
            focus,
        } = command
        else {
            panic!("not the terminal-completed verb")
        };
        assert_eq!((elapsed_ms, status), (12_034, 2));
        assert_eq!(label, "make");
        assert!(emulator.is_empty());
        assert_eq!(focus, "auto");

        let cli = Cli::parse_from(["templar", "notify", "policy-sh"]);
        let Command::Notify { command, .. } = cli.command else {
            panic!("not the notify subcommand")
        };
        assert!(matches!(command, NotifyCommand::PolicySh));

        // `templar run --notify` keeps its workload after `--`, flag order free.
        let cli = Cli::parse_from(["templar", "run", "--notify", "--", "make", "world"]);
        let Command::Run {
            notify, command, ..
        } = cli.command
        else {
            panic!("not the run subcommand")
        };
        assert!(notify);
        assert_eq!(
            command,
            vec![std::ffi::OsString::from("make"), "world".into()]
        );
    }

    /// The shapes libfm-qt sends through `templar-terminal`: nothing (open here), and
    /// `--hold -e CMD ARGS…` with arguments of the command's own that look like our flags.
    #[test]
    fn terminal_parses_the_shapes_libfm_qt_sends() {
        let cli = Cli::parse_from(["templar", "terminal"]);
        let Command::Terminal(TerminalArgs {
            working_directory,
            hold,
            command,
            ..
        }) = cli.command
        else {
            panic!("not the terminal subcommand")
        };
        assert_eq!(
            (working_directory, hold, command),
            (None, false, Vec::new())
        );

        let cli = Cli::parse_from([
            "templar",
            "terminal",
            "--directory",
            "/tmp",
            "--hold",
            "-e",
            "ls",
            "-l",
            "--hold",
            "--dry-run",
        ]);
        let Command::Terminal(TerminalArgs {
            working_directory,
            hold,
            dry_run,
            command,
            ..
        }) = cli.command
        else {
            panic!("not the terminal subcommand")
        };
        assert_eq!(working_directory, Some(PathBuf::from("/tmp")));
        assert!(hold);
        assert!(!dry_run, "--dry-run after -e belongs to the command");
        assert_eq!(command, ["ls", "-l", "--hold", "--dry-run"]);
    }

    /// The prompt hook itself: it must speak only when the working directory actually changed,
    /// and it must leave every shell that is not an interactive TemplarOS Bash alone. A `templar` on
    /// PATH that only records its arguments is what makes this observable without a session.
    ///
    /// The hook is loaded during interactive, non-login Bash startup, before the first prompt.
    /// The fixture restores its requested desktop after /etc/bash.bashrc, preventing installed
    /// hooks from preloading the same functions. Package delivery is tested separately below.
    #[test]
    fn garden_s_prompt_hook_speaks_once_per_directory_and_stays_quiet_elsewhere() {
        let hook = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../session/profile.d/60-templaros-garden.sh"
        );
        let tmp = tempfile::tempdir().unwrap();
        let stub = tmp.path().join("templar");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TRACE_FILE\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let trace = tmp.path().join("trace");

        let run = |desktop: &str, args: &[&str]| {
            bash_fixture::command(tmp.path(), Path::new(hook), desktop)
                .args(args)
                .env("TRACE_FILE", &trace)
                .current_dir(tmp.path())
                // The two negative cases fail loudly on purpose; their noise is not a result.
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
        };
        let script =
            "__templaros_garden_prompt; __templaros_garden_prompt; cd /; __templaros_garden_prompt";

        // Not a TemplarOS session: the hook is not even defined, so calling it fails.
        assert!(!run("GNOME", &["-i", "-c", script]).success());
        assert!(!trace.exists() || std::fs::read_to_string(&trace).unwrap().is_empty());

        // A non-interactive shell reads no rc file at all, and would be left alone even if it did.
        assert!(!run("TemplarOS", &["-c", script]).success());

        // Interactive and TemplarOS: one line per directory, and the repeat says nothing.
        assert!(run("TemplarOS", &["-i", "-c", script]).success());
        let lines: Vec<String> = std::fs::read_to_string(&trace)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("garden journal-init "))
                .count(),
            1,
            "the checkout hook initializes its journal once: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("notify ")),
            "the completion hook is not part of this fixture: {lines:?}"
        );
        let observations: Vec<_> = lines
            .into_iter()
            .filter(|line| line.starts_with("garden observe "))
            .collect();
        assert_eq!(
            observations,
            [
                format!(
                    "garden observe terminal-entered {} --role terminal",
                    tmp.path().canonicalize().unwrap().display()
                ),
                "garden observe terminal-entered / --role terminal".to_owned(),
            ]
        );
    }

    /// The other half: the line `templaros-cli`'s postinst adds to `/etc/bash.bashrc` is what makes
    /// an interactive, non-login bash read the hook at all. Running the block the package writes
    /// — with the payload path pointed at this checkout — is the only way to know the delivery
    /// works without installing over the host's own `/etc`.
    #[test]
    fn garden_s_hook_is_loaded_by_the_line_the_package_adds_to_bash_bashrc() {
        let postinst = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/templaros-cli/postinst"
        ))
        .expect("templaros-cli ships maintainer scripts");
        let block: String = postinst
            .lines()
            // The markers as they are *written*, not as the guard greps for them.
            .skip_while(|line| line.trim_start() != "# >>> templaros garden hook >>>")
            .take_while(|line| line.trim_start() != "# <<< templaros garden hook <<<")
            .map(|line| {
                line.replace(
                    "/etc/profile.d/60-templaros-garden.sh",
                    concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../../session/profile.d/60-templaros-garden.sh"
                    ),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            block.contains("60-templaros-garden.sh"),
            "the postinst block no longer sources the hook: {block}"
        );

        let tmp = tempfile::tempdir().unwrap();
        let rcfile = tmp.path().join("bash.bashrc");
        std::fs::write(&rcfile, block).unwrap();
        let status = bash_fixture::command(tmp.path(), &rcfile, "TemplarOS")
            .args(["-i", "-c", "declare -F __templaros_garden_prompt"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "an interactive non-login bash reading what the package installs must find the hook"
        );
    }

    /// The completion hook's delivery, mirroring the garden test above: the postinst block
    /// must source `61-templaros-completion.sh` so an interactive, non-login bash defines
    /// the prompt hook.
    #[test]
    fn completion_s_hook_is_loaded_by_the_line_the_package_adds_to_bash_bashrc() {
        let postinst = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/templaros-cli/postinst"
        ))
        .expect("templaros-cli ships maintainer scripts");
        let block: String = postinst
            .lines()
            .skip_while(|line| line.trim_start() != "# >>> templaros completion hook >>>")
            .take_while(|line| line.trim_start() != "# <<< templaros completion hook <<<")
            .map(|line| {
                line.replace(
                    "/etc/profile.d/61-templaros-completion.sh",
                    concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../../session/profile.d/61-templaros-completion.sh"
                    ),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            block.contains("61-templaros-completion.sh"),
            "the postinst block no longer sources the hook: {block}"
        );

        let tmp = tempfile::tempdir().unwrap();
        let rcfile = tmp.path().join("bash.bashrc");
        std::fs::write(&rcfile, block).unwrap();
        let status = bash_fixture::command(tmp.path(), &rcfile, "TemplarOS")
            .args(["-i", "-c", "declare -F __templaros_completion_precmd"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "an interactive non-login bash reading what the package installs must find the hook"
        );
    }
}
