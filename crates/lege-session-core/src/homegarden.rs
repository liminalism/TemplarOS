//! Homegarden: the activity ledger behind Home's grown places
//! (`@legeos.decision.homegarden-activity-grown-places`, `docs/architecture/homegarden.md`).
//!
//! A directory becomes a **context** because the user opened it, launched a terminal or editor
//! into it, opened files under it, or came back to it; it climbs from Dormant through Trace and
//! Tile to Expanded as that evidence concentrates, and steps back down as the user spends later
//! sessions elsewhere. Nothing here does I/O, ticks or spawns: the daemon feeds
//! [`Ledger::observe`] one [`HomeActivity`] at a time, asks [`Ledger::tend`] for stage changes at
//! a tending moment (Home reveal, session start, an explicit `garden.tend`), and persists the
//! whole ledger as one JSON value between them.
//!
//! **Time is Garden time** (`docs/garden/garden2.md`): a [`GardenMoment`] is the count of user
//! sessions so far and the unlocked-session runtime accumulated so far. The clock advances only at
//! session edges (start, unlock, lock, sleep, logout); a machine that is off ages nothing, and
//! a place recedes because the user *kept using the machine* without returning to it, never
//! because Tuesday became Friday.
//!
//! **Breadth beats count.** Weight is bucketed per `(kind, role, session)`, so forty saves in a
//! session are one bucket, and the score's breadth term rewards a place used across several
//! sessions with several kinds of act far above one burst. Machine churn never arrives here: the
//! ignore list drops build output, caches and hidden directories before attribution.

use lege_field::{Cell, Span};
use lege_types::{AppId, ProviderRole};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

// ---------------------------------------------------------------- the clock -----------------

/// A point in Garden time: how many user sessions have been entered, and how many milliseconds
/// of unlocked session runtime have accumulated across all of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GardenMoment {
    pub session: u64,
    pub active_ms: u64,
}

impl GardenMoment {
    /// Garden hours elapsed since `earlier` (never negative).
    #[must_use]
    pub fn hours_since(self, earlier: GardenMoment) -> f32 {
        self.active_ms.saturating_sub(earlier.active_ms) as f32 / 3_600_000.0
    }

    /// Sessions entered since `earlier` (never negative).
    #[must_use]
    pub fn sessions_since(self, earlier: GardenMoment) -> u64 {
        self.session.saturating_sub(earlier.session)
    }
}

/// The Garden Clock. `interval_started` is a monotonic millisecond reading and is deliberately
/// not persisted: a daemon that restarts mid-session cannot know how long the interval ran, so
/// it starts a new one and loses at most the unrecorded tail, never inventing time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GardenClock {
    pub session: u64,
    pub active_ms: u64,
    #[serde(skip)]
    interval_started: Option<u64>,
}

impl GardenClock {
    /// A user session started: count it and open an unlocked interval.
    pub fn start_session(&mut self, monotonic_ms: u64) {
        self.session += 1;
        self.resume(monotonic_ms);
    }

    /// The session became usable again (unlock, resume from sleep): open an interval.
    pub fn resume(&mut self, monotonic_ms: u64) {
        if self.interval_started.is_none() {
            self.interval_started = Some(monotonic_ms);
        }
    }

    /// The session stopped being usable (lock, sleep, logout): accumulate the interval.
    pub fn pause(&mut self, monotonic_ms: u64) {
        if let Some(started) = self.interval_started.take() {
            self.active_ms += monotonic_ms.saturating_sub(started);
        }
    }

    /// The moment now, counting the open interval without closing it.
    #[must_use]
    pub fn now(&self, monotonic_ms: u64) -> GardenMoment {
        let open = self
            .interval_started
            .map_or(0, |started| monotonic_ms.saturating_sub(started));
        GardenMoment {
            session: self.session,
            active_ms: self.active_ms + open,
        }
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.interval_started.is_some()
    }
}

// ---------------------------------------------------------------- activity ------------------

/// One human-significant interaction with a path, in descending weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityKind {
    /// `HomeField1.PlaceFolder` / "Add to Home": the user asked. Also pins.
    PlacedOnHome,
    /// `Projects1.SetCurrent` to a project rooted here.
    ProjectEntered,
    /// A launch whose working directory or path argument is here.
    LaunchedHere,
    /// A folder window or the Files surface entered it.
    DirectoryOpened,
    /// A brokered open of a file under it.
    FileOpened,
    /// A user create/rename/move under it.
    CreatedOrRenamed,
    /// A first-party app saved a file under it.
    FileSaved,
}

impl ActivityKind {
    pub const ALL: [ActivityKind; 7] = [
        Self::PlacedOnHome,
        Self::ProjectEntered,
        Self::LaunchedHere,
        Self::DirectoryOpened,
        Self::FileOpened,
        Self::CreatedOrRenamed,
        Self::FileSaved,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlacedOnHome => "placed-on-home",
            Self::ProjectEntered => "project-entered",
            Self::LaunchedHere => "launched-here",
            Self::DirectoryOpened => "directory-opened",
            Self::FileOpened => "file-opened",
            Self::CreatedOrRenamed => "created-or-renamed",
            Self::FileSaved => "file-saved",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }

    /// Raw weight (levers are deliberately not settings: the breadth term, not the weights,
    /// is what the user feels).
    #[must_use]
    pub fn weight(self) -> f32 {
        match self {
            Self::PlacedOnHome => 8.0,
            Self::ProjectEntered => 5.0,
            Self::LaunchedHere => 3.0,
            Self::DirectoryOpened => 2.0,
            Self::FileOpened | Self::CreatedOrRenamed => 1.5,
            Self::FileSaved => 0.8,
        }
    }

    /// How much one `(kind, role, session)` bucket may accumulate: repeats within a session
    /// count, but never past this, so a save every ten seconds stays one act of work.
    #[must_use]
    fn bucket_cap(self) -> f32 {
        self.weight() * 4.0
    }
}

/// One event, as the daemon hands it in. Pure data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeActivity {
    pub path: PathBuf,
    pub kind: ActivityKind,
    pub app_id: Option<AppId>,
    pub role: Option<ProviderRole>,
    pub at: GardenMoment,
}

// ---------------------------------------------------------------- contexts ------------------

/// Coalesced weight for one `(kind, role, session)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub kind: ActivityKind,
    pub role: Option<ProviderRole>,
    pub session: u64,
    pub count: u32,
    pub weight: f32,
}

/// One node of a context's activity tree: a path component beneath the root that earned a
/// mention, with what it earned. Depth ≤ [`TREE_DEPTH`], ≤ [`TREE_NODES`] nodes in all.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeNode {
    pub name: String,
    pub is_dir: bool,
    pub score: f32,
    pub last: GardenMoment,
    #[serde(default)]
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    fn count(&self) -> usize {
        1 + self.children.iter().map(TreeNode::count).sum::<usize>()
    }

    fn prune_weakest(&mut self) -> bool {
        // Drop the weakest leaf anywhere below this node; true if one was dropped.
        if self.children.is_empty() {
            return false;
        }
        let leaf_index = self
            .children
            .iter()
            .enumerate()
            .filter(|(_, child)| child.children.is_empty())
            .min_by(|(_, a), (_, b)| a.score.total_cmp(&b.score))
            .map(|(index, _)| index);
        if let Some(index) = leaf_index {
            self.children.remove(index);
            return true;
        }
        self.children.iter_mut().any(TreeNode::prune_weakest)
    }
}

pub const TREE_DEPTH: usize = 3;
pub const TREE_NODES: usize = 32;

/// Where a context stands, from nothing shown to a large tile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    #[default]
    Dormant,
    Trace,
    Tile,
    Expanded,
}

impl Stage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dormant => "dormant",
            Self::Trace => "trace",
            Self::Tile => "tile",
            Self::Expanded => "expanded",
        }
    }

    /// The span a garden-owned Folder tile has at this stage (Folder class floors: Small 2×2,
    /// Wide ≥3×2, Large ≥3×3).
    #[must_use]
    pub fn span(self) -> Option<Span> {
        match self {
            Self::Dormant => None,
            Self::Trace => Some(Span::new(2, 2)),
            Self::Tile => Some(Span::new(3, 2)),
            Self::Expanded => Some(Span::new(4, 3)),
        }
    }

    fn down(self) -> Self {
        match self {
            Self::Dormant | Self::Trace => Self::Dormant,
            Self::Tile => Self::Trace,
            Self::Expanded => Self::Tile,
        }
    }
}

/// The persistent record behind one Place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HomeContext {
    pub root: PathBuf,
    /// Raw accumulated score at `last`. Never decayed in storage.
    pub score: f32,
    pub last: GardenMoment,
    #[serde(default)]
    pub buckets: Vec<Bucket>,
    #[serde(default)]
    pub roles: BTreeSet<ProviderRole>,
    #[serde(default)]
    pub tree: Vec<TreeNode>,
    #[serde(default)]
    pub stage: Stage,
    #[serde(default)]
    pub stage_since: GardenMoment,
    /// Where it was last placed, so it regrows there.
    #[serde(default)]
    pub landing: Option<Cell>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub hidden: bool,
    /// Set when the user resized the tile; the tender then stops resizing it.
    #[serde(default)]
    pub user_span: Option<Span>,
    /// The span the tender last gave the tile, so a different span at the next tending moment
    /// is recognised as the user's hand.
    #[serde(default)]
    pub tended_span: Option<Span>,
    pub material_seed: u64,
    /// The reason for the last stage change, for `garden.why`.
    #[serde(default)]
    pub last_reason: String,
}

impl HomeContext {
    fn new(root: PathBuf, at: GardenMoment) -> Self {
        let material_seed = fnv(root.as_os_str().as_encoded_bytes());
        Self {
            root,
            score: 0.0,
            last: at,
            buckets: Vec::new(),
            roles: BTreeSet::new(),
            tree: Vec::new(),
            stage: Stage::Dormant,
            stage_since: at,
            landing: None,
            pinned: false,
            hidden: false,
            user_span: None,
            tended_span: None,
            material_seed,
            last_reason: String::new(),
        }
    }

    /// Distinct sessions with any activity.
    #[must_use]
    pub fn sessions_seen(&self) -> usize {
        self.buckets
            .iter()
            .map(|bucket| bucket.session)
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// Distinct `(kind, role)` pairs seen in the last [`BREADTH_WINDOW`] sessions before `now`.
    #[must_use]
    pub fn breadth(&self, now: GardenMoment) -> usize {
        self.buckets
            .iter()
            .filter(|bucket| now.session.saturating_sub(bucket.session) < BREADTH_WINDOW)
            .map(|bucket| (bucket.kind, bucket.role))
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// The relevance the user feels: evidence × recency in garden time × breadth. Computed on
    /// read, never stored.
    #[must_use]
    pub fn effective(&self, now: GardenMoment, config: &GardenConfig) -> f32 {
        let idle_hours = now.hours_since(self.last);
        let idle_sessions = now.sessions_since(self.last) as f32;
        let recency = 0.5_f32.powf(idle_hours / config.half_life_hours)
            * 0.5_f32.powf(idle_sessions / config.half_life_sessions);
        let breadth = (self.breadth(now) as f32 / BREADTH_FULL).min(1.0);
        self.score * recency * (1.0 + BREADTH_BONUS * breadth)
    }

    /// A one-line account of the evidence: `14 opens, 3 terminals, 4 sessions`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for kind in ActivityKind::ALL {
            let count: u32 = self
                .buckets
                .iter()
                .filter(|bucket| bucket.kind == kind)
                .map(|bucket| bucket.count)
                .sum();
            if count > 0 {
                parts.push(format!("{count} {}", plural(kind, count)));
            }
        }
        let sessions = self.sessions_seen();
        parts.push(format!(
            "{sessions} session{}",
            if sessions == 1 { "" } else { "s" }
        ));
        parts.join(", ")
    }
}

fn plural(kind: ActivityKind, count: u32) -> &'static str {
    let one = count == 1;
    match kind {
        ActivityKind::PlacedOnHome => {
            if one {
                "placement"
            } else {
                "placements"
            }
        }
        ActivityKind::ProjectEntered => {
            if one {
                "project entry"
            } else {
                "project entries"
            }
        }
        ActivityKind::LaunchedHere => {
            if one {
                "launch"
            } else {
                "launches"
            }
        }
        ActivityKind::DirectoryOpened | ActivityKind::FileOpened => {
            if one {
                "open"
            } else {
                "opens"
            }
        }
        ActivityKind::CreatedOrRenamed => {
            if one {
                "change"
            } else {
                "changes"
            }
        }
        ActivityKind::FileSaved => {
            if one {
                "save"
            } else {
                "saves"
            }
        }
    }
}

const BREADTH_WINDOW: u64 = 6;
const BREADTH_FULL: f32 = 6.0;
const BREADTH_BONUS: f32 = 1.0;

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash.max(1)
}

// ---------------------------------------------------------------- configuration -------------

/// The levers (`shell.home-garden.*`). Every number is in garden units.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GardenConfig {
    pub enabled: bool,
    /// Garden hours over which relevance halves while the place is untouched.
    pub half_life_hours: f32,
    /// Later sessions over which relevance halves while the place is untouched.
    pub half_life_sessions: f32,
    pub max_contexts: usize,
    /// Path components (or absolute prefixes) under which activity is machine churn.
    pub ignore: Vec<String>,
}

impl Default for GardenConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            half_life_hours: 12.0,
            half_life_sessions: 4.0,
            max_contexts: 64,
            ignore: DEFAULT_IGNORE.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

impl GardenConfig {
    /// The levers as `shell.home-garden.*` settings, defaults for anything unreadable.
    #[must_use]
    pub fn from_settings(
        schema: &crate::settings::SettingsSchema,
        values: &crate::settings::SettingsValues,
    ) -> Self {
        use crate::settings::SettingValue;
        let mut config = Self::default();
        let get = |key: &str| values.effective(schema, &format!("shell.home-garden.{key}"));
        if let Some(SettingValue::Bool(enabled)) = get("enabled") {
            config.enabled = enabled;
        }
        if let Some(SettingValue::Int(hours)) = get("half-life-hours") {
            config.half_life_hours = hours.max(1) as f32;
        }
        if let Some(SettingValue::Int(sessions)) = get("half-life-sessions") {
            config.half_life_sessions = sessions.max(1) as f32;
        }
        if let Some(SettingValue::Int(max)) = get("max-contexts") {
            config.max_contexts = usize::try_from(max.max(1)).unwrap_or(64);
        }
        if let Some(SettingValue::StrList(ignore)) = get("ignore") {
            config.ignore = ignore;
        }
        config
    }
}

pub const DEFAULT_IGNORE: &[&str] = &[
    "target",
    "build",
    "dist",
    "out",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "vendor",
    "/tmp",
    "/var",
    "/proc",
    "/sys",
    "/dev",
    "/run",
];

/// The `(effective, sessions, breadth)` floors a stage needs, and the `(sessions idle, garden
/// hours idle)` both required before stepping down out of it.
#[derive(Clone, Copy, Debug)]
struct Threshold {
    promote_effective: f32,
    promote_sessions: usize,
    promote_breadth: usize,
    demote_effective: f32,
    demote_idle_sessions: u64,
    demote_idle_hours: f32,
}

fn threshold(stage: Stage) -> Threshold {
    match stage {
        Stage::Dormant => Threshold {
            promote_effective: 0.0,
            promote_sessions: 0,
            promote_breadth: 0,
            demote_effective: 0.0,
            demote_idle_sessions: u64::MAX,
            demote_idle_hours: f32::INFINITY,
        },
        // One launch, or two opens, is enough to leave a trace.
        Stage::Trace => Threshold {
            promote_effective: 3.0,
            promote_sessions: 1,
            promote_breadth: 1,
            demote_effective: 1.0,
            demote_idle_sessions: 10,
            demote_idle_hours: 30.0,
        },
        Stage::Tile => Threshold {
            promote_effective: 8.0,
            promote_sessions: 2,
            promote_breadth: 1,
            demote_effective: 4.0,
            demote_idle_sessions: 6,
            demote_idle_hours: 15.0,
        },
        Stage::Expanded => Threshold {
            promote_effective: 20.0,
            promote_sessions: 3,
            promote_breadth: 2,
            demote_effective: 10.0,
            demote_idle_sessions: 3,
            demote_idle_hours: 6.0,
        },
    }
}

// ---------------------------------------------------------------- attribution ---------------

/// What the daemon knows about the filesystem that the ledger may not read itself. Every method
/// is `stat`-class on one path; the daemon caches as it likes.
pub trait RootProbe {
    /// The root of a declared `Projects1` project containing `path`, if any.
    fn project_root(&self, path: &Path) -> Option<PathBuf>;
    /// `path` is a directory holding a VCS or workspace marker (`.git`, `Cargo.toml`, …).
    fn has_workspace_marker(&self, path: &Path) -> bool;
    /// `path` is a mount point.
    fn is_mount_point(&self, path: &Path) -> bool;
    /// `path` is an XDG user directory (Downloads, Documents, …).
    fn is_xdg_user_dir(&self, path: &Path) -> bool;
    /// The user's home directory. Never a Place; the walk stops beneath it.
    fn home(&self) -> &Path;
    /// `path` is a directory (not a file). Used to shape the tree.
    fn is_dir(&self, path: &Path) -> bool;
}

const ATTRIBUTION_DEPTH: usize = 8;

fn is_ignored(path: &Path, config: &GardenConfig) -> bool {
    for prefix in &config.ignore {
        if prefix.starts_with('/') && path.starts_with(prefix) {
            return true;
        }
    }
    path.components().any(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            name.starts_with('.')
                || config
                    .ignore
                    .iter()
                    .any(|entry| !entry.starts_with('/') && *entry == name)
        }
        _ => false,
    })
}

/// The context root an event on `path` accrues to. `None` when the path is machine territory,
/// is `$HOME` itself, or lies outside every root the walk may stop at.
pub fn attribute_root(
    path: &Path,
    existing: &[PathBuf],
    probe: &dyn RootProbe,
    config: &GardenConfig,
) -> Option<PathBuf> {
    if !path.is_absolute() || is_ignored(path, config) {
        return None;
    }
    let home = probe.home();
    if path == home {
        return None;
    }
    if let Some(root) = probe.project_root(path) {
        return Some(root);
    }
    // The nearest ancestor (the path itself first) that is already a context, so a formed
    // context keeps attracting its descendants.
    let mut best_existing: Option<&PathBuf> = None;
    for root in existing {
        if path.starts_with(root)
            && best_existing
                .is_none_or(|best| root.components().count() > best.components().count())
        {
            best_existing = Some(root);
        }
    }
    if let Some(root) = best_existing {
        return Some(root.clone());
    }
    let start = if probe.is_dir(path) {
        path
    } else {
        path.parent()?
    };
    let mut candidate = start;
    let mut marked: Option<&Path> = None;
    for _ in 0..ATTRIBUTION_DEPTH {
        if candidate == home || candidate.parent().is_none() {
            break;
        }
        if probe.has_workspace_marker(candidate) {
            marked = Some(candidate);
        }
        if probe.is_mount_point(candidate) || probe.is_xdg_user_dir(candidate) {
            return Some(marked.unwrap_or(candidate).to_path_buf());
        }
        candidate = candidate.parent()?;
    }
    // The highest ancestor with a marker wins (a workspace over a crate inside it).
    if let Some(root) = marked {
        return Some(root.to_path_buf());
    }
    // Directly under $HOME (or with no landmark): the directory itself, unless it is $HOME.
    (start != home).then(|| start.to_path_buf())
}

// ---------------------------------------------------------------- the ledger ----------------

/// A stage change the tender should reflect on the field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageChange {
    pub root: PathBuf,
    pub from: Stage,
    pub to: Stage,
    pub reason: String,
}

/// The whole persisted state: the clock and every context.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    pub clock: GardenClock,
    #[serde(default)]
    pub contexts: Vec<HomeContext>,
}

impl Ledger {
    #[must_use]
    pub fn context(&self, root: &Path) -> Option<&HomeContext> {
        self.contexts.iter().find(|context| context.root == root)
    }

    pub fn context_mut(&mut self, root: &Path) -> Option<&mut HomeContext> {
        self.contexts
            .iter_mut()
            .find(|context| context.root == root)
    }

    /// Record one event. Returns the root it accrued to, or `None` if it was dropped.
    pub fn observe(
        &mut self,
        activity: &HomeActivity,
        probe: &dyn RootProbe,
        config: &GardenConfig,
    ) -> Option<PathBuf> {
        if !config.enabled {
            return None;
        }
        let existing: Vec<PathBuf> = self.contexts.iter().map(|c| c.root.clone()).collect();
        let root = attribute_root(&activity.path, &existing, probe, config)?;
        if self.context(&root).is_none() {
            self.evict_for_room(config);
            self.contexts
                .push(HomeContext::new(root.clone(), activity.at));
        }
        let context = self.context_mut(&root).expect("just ensured");
        if context.hidden {
            return Some(root);
        }
        let weight = activity.kind.weight();
        let cap = activity.kind.bucket_cap();
        let bucket = context.buckets.iter_mut().find(|bucket| {
            bucket.kind == activity.kind
                && bucket.role == activity.role
                && bucket.session == activity.at.session
        });
        let added = if let Some(bucket) = bucket {
            bucket.count += 1;
            let headroom = (cap - bucket.weight).max(0.0);
            let added = weight.min(headroom);
            bucket.weight += added;
            added
        } else {
            context.buckets.push(Bucket {
                kind: activity.kind,
                role: activity.role,
                session: activity.at.session,
                count: 1,
                weight,
            });
            weight
        };
        context.score += added;
        context.last = context.last.max(activity.at);
        if let Some(role) = activity.role {
            context.roles.insert(role);
        }
        if activity.kind == ActivityKind::PlacedOnHome {
            context.pinned = true;
        }
        grow_tree(context, &activity.path, weight, activity.at, probe);
        // Buckets older than the breadth window carry no breadth and their weight is already in
        // `score`; keep a bounded tail so the summary can still count them.
        if context.buckets.len() > 64 {
            context.buckets.drain(..context.buckets.len() - 64);
        }
        Some(root)
    }

    fn evict_for_room(&mut self, config: &GardenConfig) {
        while self.contexts.len() >= config.max_contexts.max(1) {
            let Some(index) = self
                .contexts
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.pinned)
                .min_by(|(_, a), (_, b)| {
                    (a.stage, a.score)
                        .partial_cmp(&(b.stage, b.score))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(index, _)| index)
            else {
                return;
            };
            self.contexts.remove(index);
        }
    }

    /// Recompute every context's stage at a tending moment. Returns the changes, each with its
    /// reason. One step per context per moment: a place recedes visibly, never vanishes.
    pub fn tend(&mut self, now: GardenMoment, config: &GardenConfig) -> Vec<StageChange> {
        let mut changes = Vec::new();
        if !config.enabled {
            return changes;
        }
        for context in &mut self.contexts {
            let from = context.stage;
            let to = if context.hidden {
                Stage::Dormant
            } else {
                next_stage(context, now, config)
            };
            if to == from {
                continue;
            }
            let reason = if to > from {
                format!("grew to {}: {}", to.as_str(), context.summary())
            } else if to == Stage::Dormant && context.hidden {
                "hidden".to_owned()
            } else {
                format!(
                    "receded to {}: {} later session{}, {:.0} garden-hours without activity",
                    to.as_str(),
                    now.sessions_since(context.last),
                    if now.sessions_since(context.last) == 1 {
                        ""
                    } else {
                        "s"
                    },
                    now.hours_since(context.last)
                )
            };
            context.stage = to;
            context.stage_since = now;
            context.last_reason.clone_from(&reason);
            changes.push(StageChange {
                root: context.root.clone(),
                from,
                to,
                reason,
            });
        }
        changes
    }

    /// Pin: never removed by recession, never recedes; may still grow. Unpinning also lets go
    /// of a kept size, so "Unpin" is the one way back to a place the garden tends freely.
    pub fn set_pinned(&mut self, root: &Path, pinned: bool) -> bool {
        self.context_mut(root)
            .map(|c| {
                c.pinned = pinned;
                if !pinned {
                    c.user_span = None;
                }
            })
            .is_some()
    }

    /// Keep: pinned, and frozen at `span` — the tender never resizes it again.
    pub fn keep(&mut self, root: &Path, span: Span) -> bool {
        self.context_mut(root)
            .map(|c| {
                c.pinned = true;
                c.user_span = Some(span);
            })
            .is_some()
    }

    /// Hide: the tile leaves Home at the next tending moment and the place never grows while
    /// hidden. Hiding is the user's explicit word, so it outranks a pin: a hidden place is
    /// unpinned, and unhiding gives the garden back an ordinary, tendable place.
    pub fn set_hidden(&mut self, root: &Path, hidden: bool) -> bool {
        self.context_mut(root)
            .map(|c| {
                c.hidden = hidden;
                if hidden {
                    c.pinned = false;
                    c.user_span = None;
                }
            })
            .is_some()
    }

    pub fn forget(&mut self, root: &Path) -> bool {
        let before = self.contexts.len();
        self.contexts.retain(|c| c.root != root);
        before != self.contexts.len()
    }

    /// Clear everything but the pinned contexts.
    pub fn forget_all(&mut self) {
        self.contexts.retain(|c| c.pinned);
    }
}

fn next_stage(context: &HomeContext, now: GardenMoment, config: &GardenConfig) -> Stage {
    let effective = context.effective(now, config);
    let sessions = context.sessions_seen();
    let breadth = context.breadth(now);
    let current = context.stage;
    // Promotion: the next stage up, if earned. Pinned contexts start at Trace at least.
    let up = match current {
        Stage::Dormant => Stage::Trace,
        Stage::Trace => Stage::Tile,
        Stage::Tile => Stage::Expanded,
        Stage::Expanded => return demoted(context, now, effective),
    };
    let t = threshold(up);
    if effective >= t.promote_effective
        && sessions >= t.promote_sessions
        && breadth >= t.promote_breadth
    {
        return up;
    }
    if context.pinned {
        return current.max(Stage::Trace);
    }
    demoted(context, now, effective)
}

fn demoted(context: &HomeContext, now: GardenMoment, effective: f32) -> Stage {
    if context.pinned || context.stage == Stage::Dormant {
        return context.stage.max(if context.pinned {
            Stage::Trace
        } else {
            Stage::Dormant
        });
    }
    let t = threshold(context.stage);
    let idle_sessions = now.sessions_since(context.last);
    let idle_hours = now.hours_since(context.last);
    if effective < t.demote_effective
        && idle_sessions >= t.demote_idle_sessions
        && idle_hours >= t.demote_idle_hours
    {
        context.stage.down()
    } else {
        context.stage
    }
}

fn grow_tree(
    context: &mut HomeContext,
    path: &Path,
    weight: f32,
    at: GardenMoment,
    probe: &dyn RootProbe,
) {
    let Ok(relative) = path.strip_prefix(&context.root) else {
        return;
    };
    let components: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .take(TREE_DEPTH)
        .collect();
    if components.is_empty() {
        return;
    }
    let mut level = &mut context.tree;
    let mut prefix = context.root.clone();
    let count = components.len();
    for (depth, name) in components.into_iter().enumerate() {
        prefix.push(&name);
        let is_dir = depth + 1 < count || probe.is_dir(&prefix);
        let index = if let Some(index) = level.iter().position(|node| node.name == name) {
            index
        } else {
            level.push(TreeNode {
                name,
                is_dir,
                score: 0.0,
                last: at,
                children: Vec::new(),
            });
            level.len() - 1
        };
        let node = &mut level[index];
        node.score += weight;
        node.last = node.last.max(at);
        level = &mut node.children;
    }
    while context.tree.iter().map(TreeNode::count).sum::<usize>() > TREE_NODES {
        let mut root = TreeNode {
            name: String::new(),
            is_dir: true,
            score: 0.0,
            last: at,
            children: std::mem::take(&mut context.tree),
        };
        let dropped = root.prune_weakest();
        context.tree = root.children;
        if !dropped {
            break;
        }
    }
}

// ---------------------------------------------------------------- persistence ---------------

const LEDGER_KEY: &str = "ledger";

/// The stored ledger, or an empty one. A ledger written by a newer build is read additively
/// (unknown fields ignored); one that will not parse at all is logged and replaced, because a
/// garden is derived state — the field it tends is what must survive.
pub fn load(store: &lege_state::Store) -> Ledger {
    let json: Option<String> = {
        let connection = store.lock();
        connection
            .query_row(
                "SELECT value_json FROM home_garden WHERE key = ?1",
                rusqlite::params![LEDGER_KEY],
                |row| row.get(0),
            )
            .ok()
    };
    match json.as_deref().map(serde_json::from_str::<Ledger>) {
        Some(Ok(ledger)) => ledger,
        Some(Err(error)) => {
            tracing::warn!(%error, "the stored Homegarden ledger did not parse; starting empty");
            Ledger::default()
        }
        None => Ledger::default(),
    }
}

/// Persist the whole ledger (one row).
pub fn save(store: &lege_state::Store, ledger: &Ledger) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let json = serde_json::to_string(ledger).context("serializing the Homegarden ledger")?;
    store.transaction(|tx| {
        tx.execute(
            "INSERT INTO home_garden (key, value_json, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json,
             updated_ms = excluded.updated_ms",
            rusqlite::params![LEDGER_KEY, json, lege_state::now_ms()],
        )
        .context("persisting the Homegarden ledger")?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        home: PathBuf,
        markers: Vec<PathBuf>,
        projects: Vec<PathBuf>,
        xdg: Vec<PathBuf>,
        files: Vec<PathBuf>,
    }

    impl Probe {
        fn new() -> Self {
            Self {
                home: PathBuf::from("/home/dk"),
                markers: vec![PathBuf::from("/home/dk/src/legeos")],
                projects: Vec::new(),
                xdg: vec![PathBuf::from("/home/dk/Downloads")],
                files: vec![
                    PathBuf::from("/home/dk/src/legeos/README.md"),
                    PathBuf::from("/home/dk/src/legeos/docs/plan.md"),
                    PathBuf::from("/home/dk/notes/a.txt"),
                ],
            }
        }
    }

    impl RootProbe for Probe {
        fn project_root(&self, path: &Path) -> Option<PathBuf> {
            self.projects
                .iter()
                .find(|root| path.starts_with(root))
                .cloned()
        }
        fn has_workspace_marker(&self, path: &Path) -> bool {
            self.markers.iter().any(|m| m == path)
        }
        fn is_mount_point(&self, path: &Path) -> bool {
            path == Path::new("/media/data")
        }
        fn is_xdg_user_dir(&self, path: &Path) -> bool {
            self.xdg.iter().any(|d| d == path)
        }
        fn home(&self) -> &Path {
            &self.home
        }
        fn is_dir(&self, path: &Path) -> bool {
            !self.files.iter().any(|f| f == path)
        }
    }

    fn at(session: u64, hours: f32) -> GardenMoment {
        GardenMoment {
            session,
            active_ms: (hours * 3_600_000.0) as u64,
        }
    }

    fn event(
        path: &str,
        kind: ActivityKind,
        role: Option<ProviderRole>,
        at: GardenMoment,
    ) -> HomeActivity {
        HomeActivity {
            path: PathBuf::from(path),
            kind,
            app_id: None,
            role,
            at,
        }
    }

    #[test]
    fn attribution_walks_up_to_the_workspace_marker_and_never_into_home() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let root = attribute_root(
            Path::new("/home/dk/src/legeos/docs/plan.md"),
            &[],
            &probe,
            &config,
        );
        assert_eq!(root, Some(PathBuf::from("/home/dk/src/legeos")));
        assert_eq!(
            attribute_root(Path::new("/home/dk"), &[], &probe, &config),
            None
        );
        // Directly under $HOME with no landmark: the directory itself.
        assert_eq!(
            attribute_root(Path::new("/home/dk/notes/a.txt"), &[], &probe, &config),
            Some(PathBuf::from("/home/dk/notes"))
        );
        // An XDG directory stops the walk.
        assert_eq!(
            attribute_root(
                Path::new("/home/dk/Downloads/x/y.iso"),
                &[],
                &probe,
                &config
            ),
            Some(PathBuf::from("/home/dk/Downloads"))
        );
        // An existing context attracts its descendants over a deeper marker.
        assert_eq!(
            attribute_root(
                Path::new("/home/dk/src/legeos/crates/x/Cargo.toml"),
                &[PathBuf::from("/home/dk/src")],
                &probe,
                &config
            ),
            Some(PathBuf::from("/home/dk/src"))
        );
    }

    #[test]
    fn the_ignore_list_holds_against_forty_thousand_build_writes() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        for i in 0..40_000 {
            let activity = event(
                &format!("/home/dk/src/legeos/target/debug/deps/x{i}.o"),
                ActivityKind::FileSaved,
                None,
                at(1, 0.5),
            );
            assert_eq!(ledger.observe(&activity, &probe, &config), None);
        }
        assert!(ledger.contexts.is_empty());
        assert_eq!(
            ledger.observe(
                &event(
                    "/home/dk/.cache/x",
                    ActivityKind::DirectoryOpened,
                    None,
                    at(1, 0.5)
                ),
                &probe,
                &config
            ),
            None
        );
    }

    #[test]
    fn breadth_beats_count_and_sessions_beat_hours() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        // 300 saves in one long session.
        let mut burst = Ledger::default();
        for _ in 0..300 {
            burst.observe(
                &event(
                    "/home/dk/src/legeos/a.rs",
                    ActivityKind::FileSaved,
                    None,
                    at(1, 10.0),
                ),
                &probe,
                &config,
            );
        }
        // Three opens and one launch across two sessions.
        let mut mixed = Ledger::default();
        for (session, hours) in [(1, 1.0), (2, 2.5)] {
            mixed.observe(
                &event(
                    "/home/dk/src/legeos/README.md",
                    ActivityKind::FileOpened,
                    None,
                    at(session, hours),
                ),
                &probe,
                &config,
            );
        }
        mixed.observe(
            &event(
                "/home/dk/src/legeos/docs",
                ActivityKind::DirectoryOpened,
                Some(ProviderRole::FileManager),
                at(2, 2.6),
            ),
            &probe,
            &config,
        );
        mixed.observe(
            &event(
                "/home/dk/src/legeos",
                ActivityKind::LaunchedHere,
                Some(ProviderRole::Terminal),
                at(2, 2.7),
            ),
            &probe,
            &config,
        );
        let now = at(2, 3.0);
        let burst_score = burst.contexts[0].effective(now, &config);
        let mixed_score = mixed.contexts[0].effective(now, &config);
        assert!(
            mixed_score > burst_score,
            "mixed {mixed_score} must beat burst {burst_score}"
        );
        // Stages: the burst stays a Trace (one session); the mixed context reaches Tile.
        assert_eq!(
            burst.tend(now, &config).last().map(|c| c.to),
            Some(Stage::Trace)
        );
        let changes = mixed.tend(now, &config);
        assert_eq!(changes.last().map(|c| c.to), Some(Stage::Trace));
        let changes = mixed.tend(now, &config);
        assert_eq!(changes.last().map(|c| c.to), Some(Stage::Tile));
        assert!(
            changes[0].reason.starts_with("grew to tile: "),
            "{}",
            changes[0].reason
        );
    }

    #[test]
    fn a_place_recedes_only_after_later_sessions_and_garden_hours_both_pass() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        for session in 1..=4 {
            for kind in [ActivityKind::LaunchedHere, ActivityKind::DirectoryOpened] {
                ledger.observe(
                    &event(
                        "/home/dk/src/legeos/docs",
                        kind,
                        Some(ProviderRole::Terminal),
                        at(session, session as f32),
                    ),
                    &probe,
                    &config,
                );
            }
        }
        let mut now = at(4, 4.5);
        for _ in 0..3 {
            ledger.tend(now, &config);
        }
        assert_eq!(ledger.contexts[0].stage, Stage::Expanded);
        // One 30-hour marathon elsewhere: sessions did not pass, so nothing recedes.
        now = at(4, 34.0);
        assert!(ledger.tend(now, &config).is_empty());
        // Ten five-minute boots: hours did not pass either.
        now = at(14, 5.0);
        assert!(ledger.tend(now, &config).is_empty());
        // Both passed: one step down per tending moment.
        now = at(14, 60.0);
        assert_eq!(ledger.tend(now, &config)[0].to, Stage::Tile);
        assert_eq!(ledger.tend(now, &config)[0].to, Stage::Trace);
        assert_eq!(ledger.tend(now, &config)[0].to, Stage::Dormant);
        assert!(ledger.tend(now, &config).is_empty());
        // And nothing happens without a tending moment in between: a machine that is off ages
        // nothing, because `now` never advanced.
    }

    #[test]
    fn keep_freezes_the_size_hide_outranks_a_pin_and_unpin_lets_go_of_both() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        let root = Path::new("/home/dk/src/legeos");
        ledger.observe(
            &event(
                "/home/dk/src/legeos",
                ActivityKind::LaunchedHere,
                None,
                at(1, 1.0),
            ),
            &probe,
            &config,
        );
        assert!(ledger.keep(root, Span::new(3, 3)));
        let context = ledger.context(root).unwrap();
        assert!(context.pinned);
        assert_eq!(context.user_span, Some(Span::new(3, 3)));
        assert!(ledger.set_hidden(root, true));
        let context = ledger.context(root).unwrap();
        assert!(context.hidden && !context.pinned && context.user_span.is_none());
        assert!(ledger.set_hidden(root, false));
        assert!(ledger.keep(root, Span::new(2, 2)));
        assert!(ledger.set_pinned(root, false));
        let context = ledger.context(root).unwrap();
        assert!(!context.pinned && context.user_span.is_none());
        assert!(!ledger.keep(Path::new("/nowhere"), Span::new(2, 2)));
    }

    #[test]
    fn a_pinned_place_never_recedes_and_a_hidden_one_never_grows() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        ledger.observe(
            &event(
                "/home/dk/src/legeos",
                ActivityKind::PlacedOnHome,
                None,
                at(1, 1.0),
            ),
            &probe,
            &config,
        );
        assert!(ledger.contexts[0].pinned);
        ledger.tend(at(1, 1.0), &config);
        assert_eq!(ledger.contexts[0].stage, Stage::Trace);
        assert!(ledger.tend(at(50, 500.0), &config).is_empty());
        ledger.set_hidden(Path::new("/home/dk/src/legeos"), true);
        assert_eq!(ledger.tend(at(50, 500.0), &config)[0].to, Stage::Dormant);
        for _ in 0..10 {
            ledger.observe(
                &event(
                    "/home/dk/src/legeos",
                    ActivityKind::LaunchedHere,
                    None,
                    at(51, 501.0),
                ),
                &probe,
                &config,
            );
        }
        assert!(ledger.tend(at(51, 502.0), &config).is_empty());
    }

    #[test]
    fn the_ledger_is_bounded_and_evicts_the_least_relevant_unpinned_context() {
        let probe = Probe::new();
        let config = GardenConfig {
            max_contexts: 3,
            ..GardenConfig::default()
        };
        let mut ledger = Ledger::default();
        ledger.observe(
            &event("/home/dk/a", ActivityKind::PlacedOnHome, None, at(1, 0.0)),
            &probe,
            &config,
        );
        ledger.observe(
            &event("/home/dk/b", ActivityKind::FileSaved, None, at(1, 0.0)),
            &probe,
            &config,
        );
        ledger.observe(
            &event("/home/dk/c", ActivityKind::LaunchedHere, None, at(1, 0.0)),
            &probe,
            &config,
        );
        ledger.observe(
            &event("/home/dk/d", ActivityKind::LaunchedHere, None, at(1, 0.0)),
            &probe,
            &config,
        );
        let roots: Vec<_> = ledger
            .contexts
            .iter()
            .map(|c| c.root.to_string_lossy().into_owned())
            .collect();
        assert_eq!(roots, ["/home/dk/a", "/home/dk/c", "/home/dk/d"]);
    }

    #[test]
    fn the_activity_tree_records_used_structure_and_stays_bounded() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        ledger.observe(
            &event(
                "/home/dk/src/legeos/docs/plan.md",
                ActivityKind::FileOpened,
                None,
                at(1, 0.0),
            ),
            &probe,
            &config,
        );
        ledger.observe(
            &event(
                "/home/dk/src/legeos/docs",
                ActivityKind::DirectoryOpened,
                None,
                at(1, 0.0),
            ),
            &probe,
            &config,
        );
        let tree = &ledger.contexts[0].tree;
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "docs");
        assert!(tree[0].is_dir);
        assert_eq!(tree[0].children[0].name, "plan.md");
        assert!(!tree[0].children[0].is_dir);
        for i in 0..100 {
            ledger.observe(
                &event(
                    &format!("/home/dk/src/legeos/crates/c{i}/src/lib.rs"),
                    ActivityKind::FileOpened,
                    None,
                    at(1, 0.0),
                ),
                &probe,
                &config,
            );
        }
        let total: usize = ledger.contexts[0].tree.iter().map(TreeNode::count).sum();
        assert!(total <= TREE_NODES, "{total}");
        assert!(ledger.contexts[0].tree.iter().all(|n| {
            n.children
                .iter()
                .all(|c| c.children.iter().all(|g| g.children.is_empty()))
        }));
    }

    #[test]
    fn the_garden_clock_counts_sessions_and_unlocked_time_only() {
        let mut clock = GardenClock::default();
        clock.start_session(1_000);
        assert_eq!(
            clock.now(61_000),
            at(1, 0.0).max(GardenMoment {
                session: 1,
                active_ms: 60_000
            })
        );
        clock.pause(61_000); // lock
        assert_eq!(clock.now(1_000_000).active_ms, 60_000);
        clock.pause(2_000_000); // a second lock/sleep while paused changes nothing
        assert_eq!(clock.now(2_000_000).active_ms, 60_000);
        clock.resume(2_000_000);
        clock.resume(2_500_000); // a second unlock does not restart the interval
        assert_eq!(clock.now(2_600_000).active_ms, 660_000);
        // Persisting mid-interval and reloading closes it without inventing time.
        let json = serde_json::to_string(&clock).unwrap();
        let reloaded: GardenClock = serde_json::from_str(&json).unwrap();
        assert!(!reloaded.is_active());
        assert_eq!(reloaded.now(9_000_000).active_ms, 60_000);
    }

    #[test]
    fn the_ledger_round_trips_through_json_with_defaults_for_missing_fields() {
        let ledger = Ledger::default();
        let json = serde_json::to_string(&ledger).unwrap();
        assert_eq!(serde_json::from_str::<Ledger>(&json).unwrap(), ledger);
        let context: HomeContext = serde_json::from_str(
            r#"{"root":"/home/dk/x","score":1.5,"last":{"session":1,"active_ms":0},"material_seed":7}"#,
        )
        .unwrap();
        assert_eq!(context.stage, Stage::Dormant);
        assert!(!context.pinned);
    }

    #[test]
    fn the_ledger_persists_as_one_row_and_an_empty_store_reads_empty() {
        let store = lege_state::Store::in_memory().unwrap();
        assert_eq!(load(&store), Ledger::default());
        let mut ledger = Ledger::default();
        ledger.clock.start_session(0);
        ledger
            .contexts
            .push(HomeContext::new(PathBuf::from("/home/dk/x"), at(1, 0.0)));
        save(&store, &ledger).unwrap();
        let mut reloaded = load(&store);
        // The open interval is not persisted; everything else is.
        assert!(!reloaded.clock.is_active());
        reloaded.clock.resume(0);
        assert_eq!(reloaded.contexts, ledger.contexts);
        assert_eq!(reloaded.clock.session, 1);
    }
}
