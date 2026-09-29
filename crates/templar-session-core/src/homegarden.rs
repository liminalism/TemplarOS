//! Homegarden: the activity ledger behind Home's grown places
//! (`@templaros.decision.homegarden-activity-grown-places`, `docs/architecture/homegarden.md`).
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

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};
use templar_field::{Cell, Span};
use templar_types::{AppId, InstanceId, ProviderRole};

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
///
/// Two independent things stop it: the session being *held* (locked, asleep) and the seat being
/// *idle* (the compositor turned the screen off because nobody touched anything). An interval is
/// open only while neither holds, so waking the screen at the lock prompt counts nothing, and
/// neither does an unlocked laptop left on overnight.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GardenClock {
    pub session: u64,
    pub active_ms: u64,
    #[serde(skip)]
    interval_started: Option<u64>,
    #[serde(skip)]
    held: bool,
    #[serde(skip)]
    idle: bool,
}

impl GardenClock {
    /// A user session started: count it and open an interval.
    pub fn start_session(&mut self, monotonic_ms: u64) {
        self.session += 1;
        self.idle = false;
        self.resume(monotonic_ms);
    }

    /// The session became usable again (unlock, resume from sleep).
    pub fn resume(&mut self, monotonic_ms: u64) {
        self.held = false;
        self.open(monotonic_ms);
    }

    /// The session stopped being usable (lock, sleep): accumulate the interval.
    pub fn pause(&mut self, monotonic_ms: u64) {
        self.held = true;
        self.close(monotonic_ms);
    }

    /// The compositor turned the screen off for want of input (`true`), or back on (`false`).
    pub fn set_idle(&mut self, idle: bool, monotonic_ms: u64) {
        self.idle = idle;
        if idle {
            self.close(monotonic_ms);
        } else {
            self.open(monotonic_ms);
        }
    }

    fn open(&mut self, monotonic_ms: u64) {
        if !self.held && !self.idle && self.interval_started.is_none() {
            self.interval_started = Some(monotonic_ms);
        }
    }

    fn close(&mut self, monotonic_ms: u64) {
        if let Some(started) = self.interval_started.take() {
            self.active_ms += monotonic_ms.saturating_sub(started);
        }
    }

    /// Fold what the open interval has run so far into `active_ms` and start a new one from
    /// `monotonic_ms`. Idempotent, and it never invents time: it only moves time that has
    /// already elapsed from the unpersisted interval into the persisted total.
    ///
    /// The daemon does this before every write, because [`Self::pause`] is reached only from a
    /// lock or a sleep. A session that ends any other way — a logout, the daemon stopping —
    /// emits no edge, so without this the whole interval is lost and `active_ms` stays at zero
    /// for ever while [`Self::now`] keeps looking healthy inside the running session. Garden
    /// hours that never advance mean nothing ever recedes: every demotion needs idle hours as
    /// well as idle sessions.
    pub fn checkpoint(&mut self, monotonic_ms: u64) {
        if let Some(started) = self.interval_started {
            self.active_ms += monotonic_ms.saturating_sub(started);
            self.interval_started = Some(monotonic_ms);
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
    /// An interactive terminal prompt entered this working directory.
    TerminalEntered,
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
    pub const ALL: [ActivityKind; 8] = [
        Self::PlacedOnHome,
        Self::ProjectEntered,
        Self::LaunchedHere,
        Self::TerminalEntered,
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
            Self::TerminalEntered => "terminal-entered",
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
            Self::LaunchedHere | Self::TerminalEntered => 3.0,
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
    /// The application instance this act belongs to, when the act was a launch this daemon
    /// brokered. `None` for every act observed from outside, which is every act whose instance
    /// nobody can vouch for.
    pub instance_id: Option<InstanceId>,
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
///
/// `score` is evidence as of `last`, aged by the same half-lives as the place itself at every
/// act (so a branch nobody has walked for a week is thin, whatever it once carried), and each
/// node may gain at most [`NODE_SESSION_CAP`] in one session (so forty saves in one directory
/// are one afternoon's work there, not forty).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeNode {
    pub name: String,
    pub is_dir: bool,
    pub score: f32,
    pub last: GardenMoment,
    #[serde(default)]
    pub children: Vec<TreeNode>,
    /// The session `session_weight` belongs to.
    #[serde(default)]
    pub session: u64,
    /// What this node has gained in `session`, against [`NODE_SESSION_CAP`].
    #[serde(default)]
    pub session_weight: f32,
}

impl TreeNode {
    fn count(&self) -> usize {
        1 + self.children.iter().map(TreeNode::count).sum::<usize>()
    }

    /// The evidence this node carries at `now`.
    #[must_use]
    pub fn score_at(&self, now: GardenMoment, config: &GardenConfig) -> f32 {
        self.score * decay(now, self.last, config)
    }

    /// Age the evidence to `at`, then add `weight` within this session's cap.
    fn accrue(&mut self, weight: f32, at: GardenMoment, config: &GardenConfig) {
        self.score = self.score_at(at, config);
        if self.session != at.session {
            self.session = at.session;
            self.session_weight = 0.0;
        }
        let added = weight.min((NODE_SESSION_CAP - self.session_weight).max(0.0));
        self.session_weight += added;
        self.score += added;
        self.last = self.last.max(at);
    }

    /// This subtree with every score aged to `now`: what a view should draw.
    fn aged(&self, now: GardenMoment, config: &GardenConfig) -> TreeNode {
        TreeNode {
            name: self.name.clone(),
            is_dir: self.is_dir,
            score: self.score_at(now, config),
            last: self.last,
            children: self
                .children
                .iter()
                .map(|child| child.aged(now, config))
                .collect(),
            session: self.session,
            session_weight: self.session_weight,
        }
    }

    /// Drop the weakest leaf (by evidence at `now`) anywhere below this node; true if one was.
    fn prune_weakest(&mut self, now: GardenMoment, config: &GardenConfig) -> bool {
        if self.children.is_empty() {
            return false;
        }
        let leaf_index = self
            .children
            .iter()
            .enumerate()
            .filter(|(_, child)| child.children.is_empty())
            .min_by(|(_, a), (_, b)| a.score_at(now, config).total_cmp(&b.score_at(now, config)))
            .map(|(index, _)| index);
        if let Some(index) = leaf_index {
            self.children.remove(index);
            return true;
        }
        self.children
            .iter_mut()
            .any(|child| child.prune_weakest(now, config))
    }
}

/// Deep enough for a library organised by year and event, or a crate's `src/module/file`; the
/// view decides how much of it a tile of a given size draws.
pub const TREE_DEPTH: usize = 5;
pub const TREE_NODES: usize = 96;
/// The most one tree node gains in one session: four launches, or six directory opens.
pub const NODE_SESSION_CAP: f32 = 12.0;

/// How much evidence recorded at `since` is still worth at `now`: halved over
/// `half_life_hours` of garden time *and* over `half_life_sessions` later sessions.
#[must_use]
pub fn decay(now: GardenMoment, since: GardenMoment, config: &GardenConfig) -> f32 {
    0.5_f32.powf(now.hours_since(since) / config.half_life_hours)
        * 0.5_f32.powf(now.sessions_since(since) as f32 / config.half_life_sessions)
}

/// Where a context stands, from nothing shown to a large tile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    #[default]
    Dormant,
    Trace,
    Tile,
    Expanded,
    /// A place that has taken several sessions: about a quarter of the field, and its form at
    /// full size.
    Dominant,
}

impl Stage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dormant => "dormant",
            Self::Trace => "trace",
            Self::Tile => "tile",
            Self::Expanded => "expanded",
            Self::Dominant => "dominant",
        }
    }

    /// The span a garden-owned Folder tile has at this stage.
    ///
    /// The range is deliberately aggressive: a lightly relevant place is a *line* — a name and a
    /// state line, nothing more — and one that has dominated several sessions is twelve times its
    /// area with its form drawn at size. "The same rectangle with more text" is not a
    /// representation stage; the test is whether a person can see from two metres which places
    /// currently matter (`@templaros.decision.a-place-is-a-drawn-structure-not-a-richer-rectangle`).
    #[must_use]
    pub fn span(self) -> Option<Span> {
        match self {
            Self::Dormant => None,
            Self::Trace => Some(Span::new(2, 1)),
            Self::Tile => Some(Span::new(2, 2)),
            Self::Expanded => Some(Span::new(4, 3)),
            Self::Dominant => Some(Span::new(6, 4)),
        }
    }

    fn down(self) -> Self {
        match self {
            Self::Dormant | Self::Trace => Self::Dormant,
            Self::Tile => Self::Trace,
            Self::Expanded => Self::Tile,
            Self::Dominant => Self::Expanded,
        }
    }
}

// ---------------------------------------------------------------- forms --------------------

/// The shape a Place draws inside its allocation: what its own use has made of it
/// (`@templaros.decision.a-place-is-a-drawn-structure-not-a-richer-rectangle`). Shape is the *mode*
/// dimension of the garden's grammar — position is memory, size is relevance, colour is state —
/// so it answers "what kind of work is this", never "what is in the directory".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Form {
    /// Nothing has been earned yet: a name and a state line.
    #[default]
    Plain,
    /// Worked *through*: a trunk and the descendants that mattered.
    Branch,
    /// Looked *at*: a row of things.
    Shelf,
    /// Read and written: a stack of documents. Reserved; nothing draws it yet.
    Folio,
}

impl Form {
    pub const ALL: [Form; 4] = [Self::Plain, Self::Branch, Self::Shelf, Self::Folio];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Branch => "branch",
            Self::Shelf => "shelf",
            Self::Folio => "folio",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|form| form.as_str() == name)
    }
}

/// What a leaf's name says it is. Extension, not MIME: `HomeGarden1.Observe` is a shipped
/// signature carrying `(kind, path, app_id, role)` and nothing else, and there is no media or
/// reader `ProviderRole` to ask either — so the file name is the whole of the evidence the
/// ledger has. It is enough to tell a photo directory from a manuscript one, which is all the
/// form needs; anything finer waits for a signal that exists.
const MEDIA_EXTENSIONS: &[&str] = &[
    "avif", "bmp", "flac", "gif", "heic", "jpeg", "jpg", "m4a", "mkv", "mov", "mp3", "mp4", "ogg",
    "opus", "png", "svg", "tif", "tiff", "wav", "webm", "webp",
];
const DOCUMENT_EXTENSIONS: &[&str] = &[
    "csv", "doc", "docx", "epub", "md", "odp", "ods", "odt", "pdf", "pptx", "rst", "rtf", "tex",
    "txt", "xlsx",
];

/// The evidence a leaf carries, as an index into [`HomeContext::form_evidence`].
fn leaf_evidence(name: &str) -> Option<usize> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    if MEDIA_EXTENSIONS.contains(&extension.as_str()) {
        Some(1)
    } else if DOCUMENT_EXTENSIONS.contains(&extension.as_str()) {
        Some(2)
    } else {
        None
    }
}

/// How many sessions a candidate form must lead for before the drawing changes. A place that
/// changed shape every time a photo was opened under it would be noise, not identity.
const FORM_DWELL_SESSIONS: u64 = 2;
/// The evidence a form needs before it beats [`Form::Plain`], and by how much it must beat the
/// next form. Both deliberately blunt: the cost of a wrong form is a place that looks like the
/// wrong kind of work, so the tie goes to saying nothing.
const FORM_FLOOR: f32 = 2.0;
const FORM_MARGIN: f32 = 1.5;

// ---------------------------------------------------------------- blooms --------------------

/// What kind of thing a bloom is reporting.
///
/// One kind today: something arrived. The enum exists rather than a bare count because a bloom
/// is persisted in the ledger's JSON, and a tagged value is what lets a later build add a kind
/// without a migration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BloomKind {
    /// Files appeared under a Place the user had already earned.
    #[default]
    Arrived,
}

impl BloomKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arrived => "arrived",
        }
    }
}

/// What an already-earned Place has to say about content that turned up inside it.
///
/// A bloom is a *projection over* a context, never evidence for one: it carries no weight, it
/// never touches `score`, `stage` or `landing`, and it is only ever raised on a context that
/// already exists at [`Stage::Tile`] or above. Content changes may alter what an earned Place
/// shows; they may not create, resize, move or promote one
/// (`@templaros.decision.a-place-is-a-drawn-structure-not-a-richer-rectangle`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bloom {
    pub kind: BloomKind,
    /// The Garden moment the oldest unresolved arrival was noticed at. Garden time, so a bloom
    /// ages because the user kept working without looking, never because a date passed.
    pub since: GardenMoment,
    /// The unresolved arrivals the body can mark, oldest first, at most [`BLOOM_ITEMS`].
    pub items: Vec<PathBuf>,
    /// How many arrivals are still unresolved, which is what the state line counts.
    pub total: u32,
}

/// How many arrived paths one bloom remembers by name. The state line's count is `total`; this
/// is only how many the Large body can mark.
///
/// ponytail: a bloom that overflows this cap clears when its tracked items are opened, so a
/// hundred arrivals can stop counting after eight opens. Keep a full queue only if a place that
/// receives hundreds of files between reveals turns out to be the common case.
pub const BLOOM_ITEMS: usize = 8;

/// The count the state line stops at, so `99 new` is the loudest a place can be.
pub const BLOOM_TOTAL_MAX: u32 = 99;

/// Later sessions after which an unresolved bloom is dropped as stale. Sessions, not days.
const BLOOM_STALE_SESSIONS: u64 = 5;

/// One entry of the shared index's change log, reduced to what the garden reads from it.
///
/// The daemon does the reducing: it drops everything that is not a file arrival, a move or a
/// removal, so the ledger never learns what an `EntryId`, a generation or a root id is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexChange {
    /// A file appeared at `path`.
    Arrived { path: PathBuf },
    /// Something moved. `from` may be a context root, which is how a Place survives a rename.
    Moved { from: PathBuf, to: PathBuf },
    /// Something at `path` is gone.
    Gone { path: PathBuf },
}

/// The persistent record behind one Place.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HomeContext {
    pub root: PathBuf,
    /// Evidence as of `last`: aged by [`decay`] at every act before the act's weight is added,
    /// so a place that was busy a month ago and is touched once today comes back at what the
    /// month has left of it, not at its lifetime total.
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
    /// The form the place draws in, settled at a tending moment.
    #[serde(default)]
    pub form: Form,
    /// The form the evidence has been pointing at, and since which session, so a form changes
    /// only after the evidence has held rather than at the first file that disagrees.
    #[serde(default)]
    pub form_candidate: Form,
    #[serde(default)]
    pub form_candidate_since: u64,
    /// The reason for the last stage change, for `garden.why`.
    #[serde(default)]
    pub last_reason: String,
    /// What arrived here since the user last looked. Never evidence: see [`Bloom`].
    #[serde(default)]
    pub bloom: Option<Bloom>,
    /// The application instances that came out of this place: launched here, or launched to open
    /// a file under it. A window belongs to the place when its `ShellToplevel.instance_id` is one
    /// of these.
    ///
    /// **Strong evidence only.** "Firefox was open while you edited something here" is not an
    /// association and never becomes one; only the broker's own record of a launch *at* the place
    /// is. Most recent last, capped at [`INSTANCE_MEMORY`] — this is a list of live windows to
    /// recognise, not a history, and an instance nobody can see is simply never matched.
    #[serde(default)]
    pub instances: Vec<InstanceId>,
}

/// How many launched-here instances a place remembers.
pub const INSTANCE_MEMORY: usize = 32;

impl HomeContext {
    /// Associate one application instance with this place, most recent last, without repeating
    /// one it already knows.
    pub fn remember_instance(&mut self, instance: InstanceId) {
        self.instances.retain(|known| *known != instance);
        self.instances.push(instance);
        if self.instances.len() > INSTANCE_MEMORY {
            let excess = self.instances.len() - INSTANCE_MEMORY;
            self.instances.drain(..excess);
        }
    }

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
            form: Form::Plain,
            form_candidate: Form::Plain,
            form_candidate_since: at.session,
            last_reason: String::new(),
            bloom: None,
            instances: Vec::new(),
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

    /// Distinct sessions with any activity among the last [`SESSION_WINDOW`] before `now`: the
    /// session floors a promotion asks for are about *recent* return, not a lifetime tally.
    #[must_use]
    pub fn recent_sessions(&self, now: GardenMoment) -> usize {
        self.buckets
            .iter()
            .filter(|bucket| now.session.saturating_sub(bucket.session) < SESSION_WINDOW)
            .map(|bucket| bucket.session)
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// The activity tree with every score aged to `now`, which is what a view draws: branches
    /// recorded at different moments are only comparable once they are all read at the same one.
    #[must_use]
    pub fn tree_at(&self, now: GardenMoment, config: &GardenConfig) -> Vec<TreeNode> {
        self.tree
            .iter()
            .map(|node| node.aged(now, config))
            .collect()
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
        let breadth = (self.breadth(now) as f32 / BREADTH_FULL).min(1.0);
        self.score * decay(now, self.last, config) * (1.0 + BREADTH_BONUS * breadth)
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

    /// Note one arrival under this place. Raises the bloom if there was none.
    fn raise_bloom(&mut self, path: &Path, now: GardenMoment) {
        let bloom = self.bloom.get_or_insert_with(|| Bloom {
            kind: BloomKind::Arrived,
            since: now,
            items: Vec::new(),
            total: 0,
        });
        if bloom.items.iter().any(|item| item == path) {
            return;
        }
        bloom.total = bloom.total.saturating_add(1).min(BLOOM_TOTAL_MAX);
        bloom.items.push(path.to_path_buf());
        if bloom.items.len() > BLOOM_ITEMS {
            bloom.items.remove(0);
        }
    }

    /// The user dealt with `path`: opened it, or it is gone. The bloom forgets it, and a bloom
    /// with nothing left to say disappears. Returns whether anything changed.
    fn resolve_bloom(&mut self, path: &Path) -> bool {
        let Some(bloom) = self.bloom.as_mut() else {
            return false;
        };
        let Some(index) = bloom.items.iter().position(|item| item == path) else {
            return false;
        };
        bloom.items.remove(index);
        bloom.total = bloom.total.saturating_sub(1);
        if bloom.items.is_empty() || bloom.total == 0 {
            self.bloom = None;
        }
        true
    }

    /// Rewrite this place's own root and every path it remembers when an ancestor is renamed.
    fn rename_under(&mut self, from: &Path, to: &Path) -> bool {
        let Ok(relative) = self.root.strip_prefix(from) else {
            return false;
        };
        self.root = to.join(relative);
        if let Some(bloom) = self.bloom.as_mut() {
            for item in &mut bloom.items {
                if let Ok(relative) = item.strip_prefix(from) {
                    *item = to.join(relative);
                }
            }
        }
        true
    }
}

impl HomeContext {
    /// The weight behind each form — branch, shelf, folio — from the activity tree the place
    /// has already earned. A directory that was worked *through* leaves directories behind, and
    /// the deeper they go the more it was structure rather than a place things were dropped in;
    /// a place that was looked *at* leaves files, and their names say which of the two flat
    /// forms it is.
    #[must_use]
    pub fn form_evidence(&self, now: GardenMoment, config: &GardenConfig) -> [f32; 3] {
        fn walk(
            nodes: &[TreeNode],
            depth: usize,
            now: GardenMoment,
            config: &GardenConfig,
            evidence: &mut [f32; 3],
        ) {
            for node in nodes {
                let score = node.score_at(now, config);
                if node.is_dir {
                    evidence[0] += score * (1.0 + depth as f32);
                    walk(&node.children, depth + 1, now, config, evidence);
                } else if let Some(slot) = leaf_evidence(&node.name) {
                    evidence[slot] += score;
                }
            }
        }
        let mut evidence = [0.0; 3];
        walk(&self.tree, 0, now, config, &mut evidence);
        // A terminal or an editor is the strongest sign of hierarchy there is — but only where
        // structure was actually recorded, so that a place nobody has been *into* cannot be
        // handed a branch with no branches on it.
        if evidence[0] > 0.0
            && self
                .roles
                .iter()
                .any(|role| matches!(role, ProviderRole::Terminal | ProviderRole::CodeEditor))
        {
            evidence[0] *= FORM_MARGIN;
        }
        evidence
    }

    /// The form the evidence points at right now, before hysteresis: [`Form::Plain`] until one
    /// form clears [`FORM_FLOOR`] and leads the next by [`FORM_MARGIN`].
    #[must_use]
    pub fn earned_form(&self, now: GardenMoment, config: &GardenConfig) -> Form {
        let evidence = self.form_evidence(now, config);
        let forms = [Form::Branch, Form::Shelf, Form::Folio];
        let leader = (0..3)
            .max_by(|a, b| evidence[*a].total_cmp(&evidence[*b]))
            .expect("three forms");
        let runner_up = (0..3)
            .filter(|index| *index != leader)
            .map(|index| evidence[index])
            .fold(0.0_f32, f32::max);
        if evidence[leader] >= FORM_FLOOR && evidence[leader] >= runner_up * FORM_MARGIN {
            forms[leader]
        } else {
            Form::Plain
        }
    }
}

/// Settle `context`'s form at a tending moment: a candidate has to have led for
/// [`FORM_DWELL_SESSIONS`] before the drawing changes under the user.
fn settle_form(context: &mut HomeContext, now: GardenMoment, config: &GardenConfig) {
    let earned = context.earned_form(now, config);
    if earned != context.form_candidate {
        context.form_candidate = earned;
        context.form_candidate_since = now.session;
        return;
    }
    if earned != context.form
        && now.session.saturating_sub(context.form_candidate_since) >= FORM_DWELL_SESSIONS
    {
        context.form = earned;
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
        ActivityKind::TerminalEntered => {
            if one {
                "terminal entry"
            } else {
                "terminal entries"
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
/// How far back a promotion's session floor looks.
const SESSION_WINDOW: u64 = 12;

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

/// The `(effective, sessions, breadth)` floors a stage needs, how long the place must have held
/// the stage below before it may step into this one, and the `(sessions idle, garden hours idle)`
/// both required before stepping down out of it.
#[derive(Clone, Copy, Debug)]
struct Threshold {
    promote_effective: f32,
    promote_sessions: usize,
    promote_breadth: usize,
    /// Garden hours at the stage below before this one, unless a later session has begun since.
    /// Tending runs at every Home reveal, so without this a place that qualified could climb a
    /// stage per reveal and go from a line to a quarter of the field in a minute.
    promote_dwell_hours: f32,
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
            promote_dwell_hours: 0.0,
            demote_effective: 0.0,
            demote_idle_sessions: u64::MAX,
            demote_idle_hours: f32::INFINITY,
        },
        // One launch, or two opens, is enough to leave a trace, at once.
        Stage::Trace => Threshold {
            promote_effective: 3.0,
            promote_sessions: 1,
            promote_breadth: 1,
            promote_dwell_hours: 0.0,
            demote_effective: 1.0,
            demote_idle_sessions: 10,
            demote_idle_hours: 30.0,
        },
        Stage::Tile => Threshold {
            promote_effective: 8.0,
            promote_sessions: 2,
            promote_breadth: 1,
            promote_dwell_hours: 1.0,
            demote_effective: 4.0,
            demote_idle_sessions: 6,
            demote_idle_hours: 15.0,
        },
        Stage::Expanded => Threshold {
            promote_effective: 20.0,
            promote_sessions: 3,
            promote_breadth: 2,
            promote_dwell_hours: 2.0,
            demote_effective: 10.0,
            demote_idle_sessions: 3,
            demote_idle_hours: 6.0,
        },
        // A quarter of the field is a claim on the user's attention, so the floor is high and
        // the place gives it back quickly: one session away and it is an Expanded again.
        Stage::Dominant => Threshold {
            promote_effective: 45.0,
            promote_sessions: 5,
            promote_breadth: 3,
            promote_dwell_hours: 4.0,
            demote_effective: 22.0,
            demote_idle_sessions: 2,
            demote_idle_hours: 4.0,
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
    /// `path` is the root of a real repository (a `.git` directory with a `HEAD`, a worktree's
    /// `.git` file, `.hg`, `.jj`, `.svn`). An empty `.git` is not one.
    fn has_vcs_marker(&self, _path: &Path) -> bool {
        false
    }
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
    let start = if probe.is_dir(path) {
        path
    } else {
        path.parent()?
    };
    // The nearest repository root wins over everything above it. A repository is the unit of
    // work a person names; a directory of repositories — or a stray `.git` in a projects folder,
    // which on the reference machine once swallowed every repository into one Place — is not.
    // This also outranks an existing context *above* the repository, so a Place formed by that
    // mistake stops attracting and recedes.
    let mut candidate = start;
    let mut marked: Option<&Path> = None;
    for _ in 0..ATTRIBUTION_DEPTH {
        if candidate == home || candidate.parent().is_none() {
            break;
        }
        if best_existing.is_some_and(|root| candidate == root.as_path()) {
            return Some(candidate.to_path_buf());
        }
        if probe.has_vcs_marker(candidate) {
            return Some(candidate.to_path_buf());
        }
        if probe.has_workspace_marker(candidate) {
            marked = Some(candidate);
        }
        if probe.is_mount_point(candidate) || probe.is_xdg_user_dir(candidate) {
            return Some(
                best_existing
                    .map(PathBuf::as_path)
                    .or(marked)
                    .unwrap_or(candidate)
                    .to_path_buf(),
            );
        }
        candidate = candidate.parent()?;
    }
    if let Some(root) = best_existing {
        return Some(root.clone());
    }
    // No repository: the highest ancestor with a build marker wins (a workspace over a crate
    // inside it).
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
    /// How far the garden has read the shared index's change log. A position, never a time:
    /// the log's own `at_ms` is wall-clock and is dropped before anything reaches here.
    #[serde(default)]
    pub change_cursor: u64,
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
    /// Associate a launched instance with the place its argument lies under, **without** the act
    /// counting as evidence of anything.
    ///
    /// The brokered open of a file already reaches [`Self::observe`] as a `FileOpened` from the
    /// shell, which is where its weight comes from; the daemon's own launch record is the only
    /// thing that knows which *instance* that open produced. Attaching it here rather than
    /// weighting it again is what keeps one act worth one act.
    ///
    /// Attaches to an existing place only. A launch is not evidence enough to grow one — that is
    /// `observe`'s judgement to make, on its own terms.
    pub fn associate_instance(
        &mut self,
        path: &Path,
        instance: InstanceId,
        probe: &dyn RootProbe,
        config: &GardenConfig,
    ) -> Option<PathBuf> {
        if !config.enabled {
            return None;
        }
        let existing: Vec<PathBuf> = self.contexts.iter().map(|c| c.root.clone()).collect();
        let root = attribute_root(path, &existing, probe, config)?;
        let context = self.context_mut(&root)?;
        context.remember_instance(instance);
        Some(root)
    }

    /// The user said this window belongs here.
    ///
    /// The third strong association, and the only one nobody has to infer: `LaunchedHere` and the
    /// brokered open are the daemon's own observations, and this is a person pointing at a window
    /// and a place and saying they go together. `root` must already be a place — an adopt is not
    /// evidence that a folder matters, only that a window belongs to one that already does, and
    /// nothing about the place's stage, score or buckets moves.
    ///
    /// `false` when `root` names no context, which the caller reports rather than swallows: the
    /// user aimed at something.
    pub fn adopt_instance(&mut self, root: &Path, instance: InstanceId) -> bool {
        match self.context_mut(root) {
            Some(context) => {
                context.remember_instance(instance);
                true
            }
            None => false,
        }
    }

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
        // The window this act produced belongs to the place, if the act was one this daemon
        // brokered and one of the two the doc calls strong: launched here, or launched to open a
        // file under it. Recorded before the hidden check — a hidden place still owns its windows.
        if let Some(instance) = activity.instance_id
            && matches!(
                activity.kind,
                ActivityKind::LaunchedHere | ActivityKind::FileOpened
            )
        {
            context.remember_instance(instance);
        }
        // The one place a bloom is answered: the user opened the thing that arrived, so the
        // place has nothing left to say about it. No new source — this is the brokered open the
        // ledger already hears.
        if activity.kind == ActivityKind::FileOpened {
            context.resolve_bloom(&activity.path);
        }
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
        context.score = context.score * decay(activity.at, context.last, config) + added;
        context.last = context.last.max(activity.at);
        if let Some(role) = activity.role {
            context.roles.insert(role);
        }
        if activity.kind == ActivityKind::PlacedOnHome {
            context.pinned = true;
        }
        grow_tree(context, &activity.path, weight, activity.at, probe, config);
        // Buckets older than the breadth window carry no breadth and their weight is already in
        // `score`; keep a bounded tail so the summary can still count them.
        if context.buckets.len() > 64 {
            context.buckets.drain(..context.buckets.len() - 64);
        }
        Some(root)
    }

    /// Fold one page of the shared index's change log into the places that already exist.
    /// Returns how many arrivals were noted.
    ///
    /// This is the whole of the rule that content never grows a Place: every arriving path is
    /// **looked up** against the contexts the ledger already holds, and nothing is ever
    /// inserted, promoted or resized here. A path under a directory nobody has earned is
    /// dropped, exactly like a path under `target/`.
    ///
    /// The one thing a change may do to a place itself is rename it: a move whose source is a
    /// context root — or an ancestor of one — carries the place to its new name with its score,
    /// its buckets and its material seed intact, because a project the user renamed is the same
    /// project.
    pub fn ingest_changes(
        &mut self,
        changes: &[IndexChange],
        now: GardenMoment,
        config: &GardenConfig,
    ) -> u32 {
        if !config.enabled {
            return 0;
        }
        let mut arrivals = 0;
        for change in changes {
            match change {
                IndexChange::Moved { from, to } => {
                    let mut renamed = false;
                    for context in &mut self.contexts {
                        renamed |= context.rename_under(from, to);
                    }
                    if renamed {
                        continue;
                    }
                    self.resolve_arrival(from);
                    arrivals += u32::from(self.note_arrival(to, now, config));
                }
                IndexChange::Arrived { path } => {
                    arrivals += u32::from(self.note_arrival(path, now, config));
                }
                IndexChange::Gone { path } => {
                    self.resolve_arrival(path);
                }
            }
        }
        arrivals
    }

    /// The earned place `path` belongs to: the deepest existing context above it that has
    /// reached [`Stage::Tile`] and is not hidden. `None` is the ordinary answer — most of the
    /// filesystem is under no Place at all, and that is what keeps a burst of index churn free.
    fn earned_root(&self, path: &Path, config: &GardenConfig) -> Option<PathBuf> {
        if !path.is_absolute() || is_ignored(path, config) {
            return None;
        }
        self.contexts
            .iter()
            .filter(|context| {
                !context.hidden
                    && context.stage >= Stage::Tile
                    && path.starts_with(&context.root)
                    && path != context.root
            })
            .max_by_key(|context| context.root.components().count())
            .map(|context| context.root.clone())
    }

    fn note_arrival(&mut self, path: &Path, now: GardenMoment, config: &GardenConfig) -> bool {
        let Some(root) = self.earned_root(path, config) else {
            return false;
        };
        let Some(context) = self.context_mut(&root) else {
            return false;
        };
        context.raise_bloom(path, now);
        true
    }

    fn resolve_arrival(&mut self, path: &Path) {
        for context in &mut self.contexts {
            if context.resolve_bloom(path) {
                return;
            }
        }
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
            settle_form(context, now, config);
            // A bloom nobody answered for several later sessions has stopped being news.
            if context
                .bloom
                .as_ref()
                .is_some_and(|bloom| now.sessions_since(bloom.since) >= BLOOM_STALE_SESSIONS)
            {
                context.bloom = None;
            }
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
    let sessions = context.recent_sessions(now);
    let breadth = context.breadth(now);
    let current = context.stage;
    // Promotion: the next stage up, if earned. Pinned contexts start at Trace at least.
    let up = match current {
        Stage::Dormant => Stage::Trace,
        Stage::Trace => Stage::Tile,
        Stage::Tile => Stage::Expanded,
        Stage::Expanded => Stage::Dominant,
        Stage::Dominant => return demoted(context, now, effective),
    };
    let t = threshold(up);
    let dwelled = now.sessions_since(context.stage_since) >= 1
        || now.hours_since(context.stage_since) >= t.promote_dwell_hours;
    if effective >= t.promote_effective
        && sessions >= t.promote_sessions
        && breadth >= t.promote_breadth
        && dwelled
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
    config: &GardenConfig,
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
                session: at.session,
                session_weight: 0.0,
            });
            level.len() - 1
        };
        let node = &mut level[index];
        node.accrue(weight, at, config);
        level = &mut node.children;
    }
    while context.tree.iter().map(TreeNode::count).sum::<usize>() > TREE_NODES {
        let mut root = TreeNode {
            name: String::new(),
            is_dir: true,
            score: 0.0,
            last: at,
            children: std::mem::take(&mut context.tree),
            session: at.session,
            session_weight: 0.0,
        };
        let dropped = root.prune_weakest(at, config);
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
pub fn load(store: &templar_state::Store) -> Ledger {
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
pub fn save(store: &templar_state::Store, ledger: &Ledger) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let json = serde_json::to_string(ledger).context("serializing the Homegarden ledger")?;
    save_json(store, &json)
}

/// Persist a ledger already serialized with `serde_json::to_string(&ledger)`: the daemon
/// serializes under its lock and writes from its own thread.
pub fn save_json(store: &templar_state::Store, json: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    store.transaction(|tx| {
        tx.execute(
            "INSERT INTO home_garden (key, value_json, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json,
             updated_ms = excluded.updated_ms",
            rusqlite::params![LEDGER_KEY, json, templar_state::now_ms()],
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
                markers: vec![PathBuf::from("/home/dk/src/templaros")],
                projects: Vec::new(),
                xdg: vec![PathBuf::from("/home/dk/Downloads")],
                files: vec![
                    PathBuf::from("/home/dk/src/templaros/README.md"),
                    PathBuf::from("/home/dk/src/templaros/docs/plan.md"),
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
            instance_id: None,
        }
    }

    /// A place knows which windows came out of it, and only the ones that did.
    ///
    /// Strong evidence is the whole rule: the launch this daemon brokered at the place, and the
    /// brokered open of a file under it. Anything observed from outside carries no instance and
    /// can never associate one — "it was open while you were working here" is not evidence and
    /// must never become any.
    #[test]
    fn a_place_remembers_only_the_instances_that_came_out_of_it() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        let at = GardenMoment::default();
        let here = "/home/u/work/project";

        let launched = InstanceId::new();
        let mut activity = event(here, ActivityKind::LaunchedHere, None, at);
        activity.instance_id = Some(launched);
        let root = ledger
            .observe(&activity, &probe, &config)
            .expect("a launch here earns a place");
        assert_eq!(ledger.context(&root).unwrap().instances, vec![launched]);

        // The same act again is the same instance, not a second one.
        ledger.observe(&activity, &probe, &config);
        assert_eq!(ledger.context(&root).unwrap().instances, vec![launched]);

        // An act with no instance behind it associates nothing.
        ledger.observe(
            &event(here, ActivityKind::FileSaved, None, at),
            &probe,
            &config,
        );
        assert_eq!(ledger.context(&root).unwrap().instances, vec![launched]);

        // The brokered open of a file under the place: the weight came from the shell's own
        // `file-opened`, and this attaches the instance without counting the act twice.
        let opened = InstanceId::new();
        let score = ledger.context(&root).unwrap().score;
        let associated = ledger.associate_instance(
            &PathBuf::from("/home/u/work/project/notes.md"),
            opened,
            &probe,
            &config,
        );
        assert_eq!(associated.as_ref(), Some(&root));
        let context = ledger.context(&root).unwrap();
        assert_eq!(context.instances, vec![launched, opened]);
        assert!(
            (context.score - score).abs() < f32::EPSILON,
            "an association is not evidence"
        );

        // An explicit adopt is the third strong association: the user pointing at a window and
        // a place. It associates and changes nothing else, and it refuses a path that is not a
        // place rather than quietly growing one.
        let adopted = InstanceId::new();
        assert!(ledger.adopt_instance(&root, adopted));
        let context = ledger.context(&root).unwrap();
        assert_eq!(context.instances, vec![launched, opened, adopted]);
        assert!(
            (context.score - score).abs() < f32::EPSILON,
            "an adopt is not evidence either"
        );
        assert!(
            !ledger.adopt_instance(&PathBuf::from("/home/u/nowhere"), InstanceId::new()),
            "a path that is no place is refused, not grown"
        );

        // And a launch under a directory that is not a place grows nothing.
        assert_eq!(
            ledger.associate_instance(
                &PathBuf::from("/home/u/elsewhere/thing.md"),
                InstanceId::new(),
                &probe,
                &config,
            ),
            None
        );

        // The memory is bounded: a place that has launched a hundred things remembers the last
        // few dozen, because this is a list of live windows to recognise and not a history.
        for _ in 0..INSTANCE_MEMORY * 2 {
            ledger
                .context_mut(&root)
                .unwrap()
                .remember_instance(InstanceId::new());
        }
        assert_eq!(
            ledger.context(&root).unwrap().instances.len(),
            INSTANCE_MEMORY
        );
    }

    #[test]
    fn attribution_walks_up_to_the_workspace_marker_and_never_into_home() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let root = attribute_root(
            Path::new("/home/dk/src/templaros/docs/plan.md"),
            &[],
            &probe,
            &config,
        );
        assert_eq!(root, Some(PathBuf::from("/home/dk/src/templaros")));
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
                Path::new("/home/dk/src/templaros/crates/x/Cargo.toml"),
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
                &format!("/home/dk/src/templaros/target/debug/deps/x{i}.o"),
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
                    "/home/dk/src/templaros/a.rs",
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
                    "/home/dk/src/templaros/README.md",
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
                "/home/dk/src/templaros/docs",
                ActivityKind::DirectoryOpened,
                Some(ProviderRole::FileManager),
                at(2, 2.6),
            ),
            &probe,
            &config,
        );
        mixed.observe(
            &event(
                "/home/dk/src/templaros",
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
        // A Trace holds for its dwell before it may become a Tile, however often Home is shown.
        assert!(mixed.tend(at(2, 3.5), &config).is_empty());
        let changes = mixed.tend(at(2, 4.1), &config);
        assert_eq!(changes.last().map(|c| c.to), Some(Stage::Tile));
        assert!(
            changes[0].reason.starts_with("grew to tile: "),
            "{}",
            changes[0].reason
        );
    }

    #[test]
    fn one_terminal_entry_becomes_a_trace_at_the_next_tending_moment() {
        // The ladder the stages ask for: a line, a block, a quarter of the field.
        assert_eq!(Stage::Trace.span(), Some(Span::new(2, 1)));
        assert_eq!(Stage::Dominant.span(), Some(Span::new(6, 4)));
        assert_eq!(Stage::Dominant.down(), Stage::Expanded);
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        assert_eq!(
            ActivityKind::parse("terminal-entered"),
            Some(ActivityKind::TerminalEntered)
        );
        ledger.observe(
            &event(
                "/home/dk/src/templaros",
                ActivityKind::TerminalEntered,
                Some(ProviderRole::Terminal),
                at(1, 0.0),
            ),
            &probe,
            &config,
        );
        assert_eq!(
            ledger
                .tend(at(1, 0.0), &config)
                .last()
                .map(|change| change.to),
            Some(Stage::Trace)
        );
    }

    #[test]
    fn a_place_recedes_only_after_later_sessions_and_garden_hours_both_pass() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        for session in 1..=4 {
            for kind in [
                ActivityKind::LaunchedHere,
                ActivityKind::DirectoryOpened,
                ActivityKind::TerminalEntered,
            ] {
                ledger.observe(
                    &event(
                        "/home/dk/src/templaros/docs",
                        kind,
                        Some(ProviderRole::Terminal),
                        at(session, session as f32),
                    ),
                    &probe,
                    &config,
                );
            }
        }
        // One step per tending moment, each after the stage below has held for its dwell.
        for hours in [4.5, 5.6, 7.7] {
            ledger.tend(at(4, hours), &config);
        }
        assert_eq!(ledger.contexts[0].stage, Stage::Expanded);
        let mut now;
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
        let root = Path::new("/home/dk/src/templaros");
        ledger.observe(
            &event(
                "/home/dk/src/templaros",
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
                "/home/dk/src/templaros",
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
        ledger.set_hidden(Path::new("/home/dk/src/templaros"), true);
        assert_eq!(ledger.tend(at(50, 500.0), &config)[0].to, Stage::Dormant);
        for _ in 0..10 {
            ledger.observe(
                &event(
                    "/home/dk/src/templaros",
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
        fn depth(node: &TreeNode) -> usize {
            1 + node.children.iter().map(depth).max().unwrap_or(0)
        }
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        ledger.observe(
            &event(
                "/home/dk/src/templaros/docs/plan.md",
                ActivityKind::FileOpened,
                None,
                at(1, 0.0),
            ),
            &probe,
            &config,
        );
        ledger.observe(
            &event(
                "/home/dk/src/templaros/docs",
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
                    &format!("/home/dk/src/templaros/crates/c{i}/src/lib.rs"),
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
        assert!(
            ledger.contexts[0]
                .tree
                .iter()
                .all(|n| depth(n) <= TREE_DEPTH)
        );
    }

    /// Evidence ages: a place that was busy long ago and is touched once today comes back at
    /// what the time since has left of it, not at its lifetime total.
    #[test]
    fn a_long_idle_place_touched_once_returns_at_its_aged_relevance() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        for session in 1..=5 {
            for kind in [ActivityKind::LaunchedHere, ActivityKind::TerminalEntered] {
                ledger.observe(
                    &event(
                        "/home/dk/src/templaros",
                        kind,
                        Some(ProviderRole::Terminal),
                        at(session, session as f32 * 4.0),
                    ),
                    &probe,
                    &config,
                );
            }
        }
        let busy = ledger.contexts[0].effective(at(5, 20.0), &config);
        // Thirty sessions and a hundred garden hours later, one directory open.
        let later = at(35, 120.0);
        ledger.observe(
            &event(
                "/home/dk/src/templaros",
                ActivityKind::DirectoryOpened,
                None,
                later,
            ),
            &probe,
            &config,
        );
        let returned = ledger.contexts[0].effective(later, &config);
        assert!(
            returned < threshold(Stage::Tile).promote_effective,
            "busy {busy}, returned {returned}: one touch must not restore a lifetime"
        );
        assert!(returned >= ActivityKind::DirectoryOpened.weight());
    }

    /// Home is revealed many times a session and each reveal is a tending moment, so a place
    /// that qualifies for several stages still climbs one per dwell, not one per reveal.
    #[test]
    fn a_qualifying_place_climbs_one_stage_per_dwell_not_per_reveal() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        for session in 1..=6 {
            for (kind, role) in [
                (ActivityKind::LaunchedHere, Some(ProviderRole::Terminal)),
                (ActivityKind::TerminalEntered, Some(ProviderRole::Terminal)),
                (
                    ActivityKind::DirectoryOpened,
                    Some(ProviderRole::FileManager),
                ),
                (ActivityKind::ProjectEntered, None),
            ] {
                for _ in 0..4 {
                    ledger.observe(
                        &event("/home/dk/src/templaros", kind, role, at(session, 1.0)),
                        &probe,
                        &config,
                    );
                }
            }
        }
        let now = at(6, 1.0);
        assert!(
            ledger.contexts[0].effective(now, &config)
                >= threshold(Stage::Dominant).promote_effective
        );
        for _ in 0..20 {
            ledger.tend(now, &config);
        }
        assert_eq!(ledger.contexts[0].stage, Stage::Trace);
        ledger.tend(at(6, 2.1), &config);
        assert_eq!(ledger.contexts[0].stage, Stage::Tile);
        // A later session counts as having held the stage, whatever the hours.
        ledger.tend(at(7, 2.2), &config);
        assert_eq!(ledger.contexts[0].stage, Stage::Expanded);
    }

    /// A tree node gains at most its session cap in one session, and a branch nobody walks
    /// thins relative to one that is walked.
    #[test]
    fn tree_nodes_are_capped_per_session_and_age() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        for _ in 0..40 {
            ledger.observe(
                &event(
                    "/home/dk/src/templaros/docs",
                    ActivityKind::LaunchedHere,
                    Some(ProviderRole::Terminal),
                    at(1, 0.5),
                ),
                &probe,
                &config,
            );
        }
        let docs = &ledger.contexts[0].tree[0];
        assert_eq!(docs.name, "docs");
        assert!(docs.score <= NODE_SESSION_CAP, "{}", docs.score);
        for session in 2..=6 {
            ledger.observe(
                &event(
                    "/home/dk/src/templaros/crates",
                    ActivityKind::DirectoryOpened,
                    None,
                    at(session, session as f32 * 3.0),
                ),
                &probe,
                &config,
            );
        }
        let now = at(6, 18.0);
        let tree = ledger.contexts[0].tree_at(now, &config);
        let score = |name: &str| tree.iter().find(|n| n.name == name).unwrap().score;
        assert!(score("crates") > score("docs"), "{tree:?}");
    }

    /// The nearest repository is the Place: a stray `.git` in a projects folder does not swallow
    /// the repositories beneath it, even when an earlier build already made it a context.
    #[test]
    fn the_nearest_repository_root_wins_over_an_umbrella() {
        struct Repos(Probe, Vec<PathBuf>);
        impl RootProbe for Repos {
            fn project_root(&self, path: &Path) -> Option<PathBuf> {
                self.0.project_root(path)
            }
            fn has_workspace_marker(&self, path: &Path) -> bool {
                self.1.iter().any(|repo| repo == path) || self.0.has_workspace_marker(path)
            }
            fn has_vcs_marker(&self, path: &Path) -> bool {
                self.1.iter().any(|repo| repo == path)
            }
            fn is_mount_point(&self, path: &Path) -> bool {
                self.0.is_mount_point(path)
            }
            fn is_xdg_user_dir(&self, path: &Path) -> bool {
                self.0.is_xdg_user_dir(path)
            }
            fn home(&self) -> &Path {
                self.0.home()
            }
            fn is_dir(&self, path: &Path) -> bool {
                self.0.is_dir(path)
            }
        }
        let umbrella = PathBuf::from("/home/dk/projects");
        let repo = PathBuf::from("/home/dk/projects/templaros");
        let probe = Repos(
            Probe {
                // The umbrella carries a build marker; only the repository is a repository.
                markers: vec![umbrella.clone()],
                ..Probe::new()
            },
            vec![repo.clone()],
        );
        let config = GardenConfig::default();
        let path = Path::new("/home/dk/projects/templaros/crates/field/src");
        assert_eq!(
            attribute_root(path, &[], &probe, &config),
            Some(repo.clone())
        );
        assert_eq!(
            attribute_root(path, std::slice::from_ref(&umbrella), &probe, &config),
            Some(repo.clone())
        );
        // Outside any repository the old rule stands: the highest build marker.
        assert_eq!(
            attribute_root(Path::new("/home/dk/projects/notes"), &[], &probe, &config),
            Some(umbrella)
        );
    }

    /// A form is *earned*: the evidence has to lead by half again as much, and it has to have
    /// led for two sessions before the drawing changes under the user. One file of another kind
    /// is not an argument.
    #[test]
    fn a_form_is_earned_over_two_sessions_and_one_file_does_not_flip_it() {
        let probe = Probe {
            files: vec![
                PathBuf::from("/home/dk/Downloads/holiday.jpg"),
                PathBuf::from("/home/dk/Downloads/cover.png"),
                PathBuf::from("/home/dk/Downloads/clip.mp4"),
                PathBuf::from("/home/dk/Downloads/scan.webp"),
                PathBuf::from("/home/dk/Downloads/receipt.pdf"),
            ],
            ..Probe::new()
        };
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        for name in ["holiday.jpg", "cover.png", "clip.mp4", "scan.webp"] {
            ledger.observe(
                &event(
                    &format!("/home/dk/Downloads/{name}"),
                    ActivityKind::FileOpened,
                    None,
                    at(1, 0.0),
                ),
                &probe,
                &config,
            );
        }
        let root = Path::new("/home/dk/Downloads");
        assert_eq!(
            ledger
                .context(root)
                .unwrap()
                .earned_form(at(1, 0.0), &config),
            Form::Shelf
        );
        // The evidence leads at once; the drawing does not follow until it has held.
        ledger.tend(at(1, 1.0), &config);
        assert_eq!(ledger.context(root).unwrap().form, Form::Plain);
        ledger.tend(at(2, 2.0), &config);
        assert_eq!(ledger.context(root).unwrap().form, Form::Plain);
        ledger.tend(at(3, 3.0), &config);
        assert_eq!(ledger.context(root).unwrap().form, Form::Shelf);
        // One document under a shelf is a document under a shelf.
        ledger.observe(
            &event(
                "/home/dk/Downloads/receipt.pdf",
                ActivityKind::FileOpened,
                None,
                at(3, 3.0),
            ),
            &probe,
            &config,
        );
        ledger.tend(at(9, 9.0), &config);
        assert_eq!(ledger.context(root).unwrap().form, Form::Shelf);

        // Structure worked through, with a terminal in it, is a branch — and a place with
        // nothing earned yet says nothing rather than guessing.
        let mut worked = Ledger::default();
        for path in [
            "/home/dk/src/templaros/crates",
            "/home/dk/src/templaros/crates/templar-field",
            "/home/dk/src/templaros/shell",
        ] {
            worked.observe(
                &event(
                    path,
                    ActivityKind::DirectoryOpened,
                    Some(ProviderRole::Terminal),
                    at(1, 0.0),
                ),
                &probe,
                &config,
            );
        }
        let templaros = Path::new("/home/dk/src/templaros");
        assert_eq!(
            worked
                .context(templaros)
                .unwrap()
                .earned_form(at(1, 0.0), &config),
            Form::Branch
        );
        assert_eq!(
            HomeContext::new(PathBuf::new(), at(1, 0.0)).earned_form(at(1, 0.0), &config),
            Form::Plain
        );
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

    /// An unlocked laptop left on overnight ages nothing once the screen has gone off, and
    /// waking the screen at a lock prompt does not start the clock either.
    #[test]
    fn the_garden_clock_stops_while_the_seat_is_idle_and_idle_does_not_undo_a_lock() {
        let mut clock = GardenClock::default();
        clock.start_session(0);
        clock.set_idle(true, 600_000); // ten minutes of use, then the screen goes off
        assert_eq!(clock.now(8 * 3_600_000).active_ms, 600_000);
        clock.set_idle(false, 8 * 3_600_000); // morning: a key press
        assert_eq!(clock.now(8 * 3_600_000 + 60_000).active_ms, 660_000);
        clock.pause(8 * 3_600_000 + 60_000); // lock
        clock.set_idle(true, 9 * 3_600_000);
        clock.set_idle(false, 10 * 3_600_000); // the lock prompt lights up
        assert_eq!(clock.now(11 * 3_600_000).active_ms, 660_000);
        clock.resume(11 * 3_600_000); // unlocked
        assert_eq!(clock.now(11 * 3_600_000 + 1_000).active_ms, 661_000);
    }

    /// A session that ends without a lock or a sleep — a logout, the daemon stopping — emits no
    /// edge, so `pause` is never called and the open interval is never folded in. Without the
    /// checkpoint the persisted `active_ms` stayed at zero across seven real sessions, garden
    /// hours never advanced, and nothing could ever recede.
    #[test]
    fn a_session_that_ends_without_a_lock_still_ages_the_garden() {
        let mut clock = GardenClock::default();
        clock.start_session(0);
        // Two hours of work, then the store is written and the session simply ends.
        clock.checkpoint(7_200_000);
        let json = serde_json::to_string(&clock).unwrap();
        let mut next: GardenClock = serde_json::from_str(&json).unwrap();
        assert!(!next.is_active());
        assert_eq!(next.now(0).active_ms, 7_200_000, "the two hours survived");

        // The next session adds to them rather than starting over.
        next.start_session(0);
        next.checkpoint(1_800_000);
        assert_eq!(
            next.now(1_800_000),
            GardenMoment {
                session: 2,
                active_ms: 9_000_000
            }
        );
        // A checkpoint is idempotent and never invents time.
        next.checkpoint(1_800_000);
        next.checkpoint(1_800_000);
        assert_eq!(next.now(1_800_000).active_ms, 9_000_000);
    }

    /// The daemon stamps an activity with `now()` — which counts the open interval — and then
    /// persists. If the write did not fold that interval in, the stored moment of last use sat
    /// *ahead* of the stored clock, `hours_since` saturated to zero on the next start, and
    /// recency stopped decaying as well as recession stopping.
    #[test]
    fn a_place_s_last_moment_never_runs_ahead_of_the_persisted_clock() {
        let probe = Probe::new();
        let config = GardenConfig::default();
        let mut ledger = Ledger::default();
        ledger.clock.start_session(0);
        for elapsed in [1_385, 900_000, 3_600_000] {
            let at = ledger.clock.now(elapsed);
            ledger.observe(
                &event(
                    "/home/dk/src/templaros",
                    ActivityKind::TerminalEntered,
                    None,
                    at,
                ),
                &probe,
                &config,
            );
            ledger.clock.checkpoint(elapsed);
        }
        let json = serde_json::to_string(&ledger).unwrap();
        let reloaded: Ledger = serde_json::from_str(&json).unwrap();
        let context = reloaded
            .context(Path::new("/home/dk/src/templaros"))
            .expect("the place was observed");
        assert!(
            context.last.active_ms <= reloaded.clock.active_ms,
            "last {} ran ahead of the clock {}",
            context.last.active_ms,
            reloaded.clock.active_ms
        );
        assert!(
            reloaded.clock.now(0).hours_since(context.last) < f32::EPSILON,
            "a place used a moment ago has been idle for no garden hours at all"
        );
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
        let store = templar_state::Store::in_memory().unwrap();
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

    // ---------------------------------------------------------------- blooms ----------------

    /// A ledger holding one earned place at `stage`, with no field and no history.
    fn earned(root: &str, stage: Stage) -> Ledger {
        let mut ledger = Ledger::default();
        let mut context = HomeContext::new(PathBuf::from(root), at(1, 0.0));
        context.stage = stage;
        context.score = 30.0;
        context.buckets.push(Bucket {
            kind: ActivityKind::LaunchedHere,
            role: Some(ProviderRole::Terminal),
            session: 1,
            count: 1,
            weight: 3.0,
        });
        ledger.contexts.push(context);
        ledger
    }

    fn arrived(path: &str) -> IndexChange {
        IndexChange::Arrived {
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn content_never_grows_a_place() {
        let config = GardenConfig::default();
        let mut ledger = earned("/home/dk/Downloads", Stage::Expanded);
        let before = ledger.contexts.clone();

        // Ten thousand arrivals under directories nobody earned, plus build output under one
        // that was earned: no context appears, and no place changes by one field.
        let mut churn: Vec<IndexChange> = (0..5_000)
            .map(|n| arrived(&format!("/home/dk/elsewhere/f{n}")))
            .collect();
        churn.extend((0..5_000).map(|n| arrived(&format!("/home/dk/Downloads/target/o{n}"))));
        assert_eq!(ledger.ingest_changes(&churn, at(2, 1.0), &config), 0);
        assert_eq!(ledger.contexts.len(), 1);
        assert_eq!(ledger.contexts, before);

        // The same burst inside the earned place blooms, and still changes nothing else.
        let real: Vec<IndexChange> = (0..40)
            .map(|n| arrived(&format!("/home/dk/Downloads/f{n}.mp4")))
            .collect();
        assert_eq!(ledger.ingest_changes(&real, at(2, 1.0), &config), 40);
        let context = &ledger.contexts[0];
        assert_eq!(context.stage, before[0].stage);
        assert!((context.score - before[0].score).abs() < f32::EPSILON);
        assert_eq!(context.landing, before[0].landing);
        assert_eq!(context.buckets, before[0].buckets);
        let bloom = context.bloom.as_ref().expect("a bloom");
        assert_eq!(bloom.total, 40);
        assert_eq!(bloom.items.len(), BLOOM_ITEMS);
    }

    #[test]
    fn a_bloom_needs_a_place_that_was_already_earned() {
        let config = GardenConfig::default();
        for stage in [Stage::Dormant, Stage::Trace] {
            let mut ledger = earned("/home/dk/Downloads", stage);
            ledger.ingest_changes(&[arrived("/home/dk/Downloads/a.mp4")], at(2, 0.0), &config);
            assert!(ledger.contexts[0].bloom.is_none(), "{stage:?} bloomed");
        }
        for stage in [Stage::Tile, Stage::Expanded] {
            let mut ledger = earned("/home/dk/Downloads", stage);
            ledger.ingest_changes(&[arrived("/home/dk/Downloads/a.mp4")], at(2, 0.0), &config);
            assert!(
                ledger.contexts[0].bloom.is_some(),
                "{stage:?} did not bloom"
            );
        }
        // Hidden says the user does not want to hear from this place at all.
        let mut ledger = earned("/home/dk/Downloads", Stage::Expanded);
        ledger.contexts[0].hidden = true;
        ledger.ingest_changes(&[arrived("/home/dk/Downloads/a.mp4")], at(2, 0.0), &config);
        assert!(ledger.contexts[0].bloom.is_none());
    }

    #[test]
    fn a_bloom_forgets_what_the_user_opened() {
        let config = GardenConfig::default();
        let probe = Probe::new();
        let mut ledger = earned("/home/dk/Downloads", Stage::Expanded);
        ledger.ingest_changes(
            &[
                arrived("/home/dk/Downloads/a.mp4"),
                arrived("/home/dk/Downloads/b.pdf"),
            ],
            at(2, 0.0),
            &config,
        );
        assert_eq!(ledger.contexts[0].bloom.as_ref().unwrap().total, 2);

        // `2 new` becomes `1 new` becomes nothing, through the file-opened Observe that the
        // ledger already hears — no new source.
        ledger.observe(
            &event(
                "/home/dk/Downloads/a.mp4",
                ActivityKind::FileOpened,
                None,
                at(2, 0.1),
            ),
            &probe,
            &config,
        );
        assert_eq!(ledger.contexts[0].bloom.as_ref().unwrap().total, 1);
        ledger.observe(
            &event(
                "/home/dk/Downloads/b.pdf",
                ActivityKind::FileOpened,
                None,
                at(2, 0.2),
            ),
            &probe,
            &config,
        );
        assert!(ledger.contexts[0].bloom.is_none());
    }

    #[test]
    fn a_deleted_arrival_stops_being_news_and_a_stale_bloom_is_dropped_in_sessions() {
        let config = GardenConfig::default();
        let mut ledger = earned("/home/dk/Downloads", Stage::Expanded);
        ledger.ingest_changes(&[arrived("/home/dk/Downloads/a.mp4")], at(2, 0.0), &config);
        ledger.ingest_changes(
            &[IndexChange::Gone {
                path: PathBuf::from("/home/dk/Downloads/a.mp4"),
            }],
            at(2, 0.1),
            &config,
        );
        assert!(ledger.contexts[0].bloom.is_none());

        ledger.ingest_changes(&[arrived("/home/dk/Downloads/b.mp4")], at(2, 0.0), &config);
        ledger.tend(at(2 + BLOOM_STALE_SESSIONS - 1, 0.0), &config);
        assert!(ledger.contexts[0].bloom.is_some(), "not stale yet");
        ledger.tend(at(2 + BLOOM_STALE_SESSIONS, 0.0), &config);
        assert!(
            ledger.contexts[0].bloom.is_none(),
            "stale after later sessions"
        );
    }

    #[test]
    fn a_renamed_root_keeps_its_seed_its_buckets_and_its_bloom() {
        let config = GardenConfig::default();
        let mut ledger = earned("/home/dk/src/templaros", Stage::Expanded);
        ledger.ingest_changes(
            &[arrived("/home/dk/src/templaros/new.rs")],
            at(2, 0.0),
            &config,
        );
        let before = ledger.contexts[0].clone();

        ledger.ingest_changes(
            &[IndexChange::Moved {
                from: PathBuf::from("/home/dk/src/templaros"),
                to: PathBuf::from("/home/dk/src/templar-os"),
            }],
            at(2, 1.0),
            &config,
        );

        assert_eq!(ledger.contexts.len(), 1, "a rename is not a second place");
        let after = &ledger.contexts[0];
        assert_eq!(after.root, PathBuf::from("/home/dk/src/templar-os"));
        assert_eq!(after.material_seed, before.material_seed);
        assert_eq!(after.buckets, before.buckets);
        assert!((after.score - before.score).abs() < f32::EPSILON);
        assert_eq!(after.stage, before.stage);
        assert_eq!(
            after.bloom.as_ref().unwrap().items,
            vec![PathBuf::from("/home/dk/src/templar-os/new.rs")],
            "the bloom followed the rename"
        );
    }

    #[test]
    fn a_renamed_ancestor_carries_every_place_beneath_it() {
        let config = GardenConfig::default();
        let mut ledger = earned("/home/dk/src/templaros", Stage::Expanded);
        ledger.contexts.push(HomeContext::new(
            PathBuf::from("/home/dk/src/other"),
            at(1, 0.0),
        ));
        ledger.ingest_changes(
            &[IndexChange::Moved {
                from: PathBuf::from("/home/dk/src"),
                to: PathBuf::from("/home/dk/code"),
            }],
            at(2, 0.0),
            &config,
        );
        let roots: Vec<&Path> = ledger.contexts.iter().map(|c| c.root.as_path()).collect();
        assert_eq!(
            roots,
            vec![
                Path::new("/home/dk/code/templaros"),
                Path::new("/home/dk/code/other")
            ]
        );
    }

    #[test]
    fn a_file_moved_into_an_earned_place_is_an_arrival() {
        let config = GardenConfig::default();
        let mut ledger = earned("/home/dk/Downloads", Stage::Expanded);
        ledger.ingest_changes(
            &[IndexChange::Moved {
                from: PathBuf::from("/home/dk/elsewhere/a.mp4"),
                to: PathBuf::from("/home/dk/Downloads/a.mp4"),
            }],
            at(2, 0.0),
            &config,
        );
        let bloom = ledger.contexts[0].bloom.as_ref().expect("a bloom");
        assert_eq!(bloom.total, 1);
        assert_eq!(bloom.items, vec![PathBuf::from("/home/dk/Downloads/a.mp4")]);
    }

    #[test]
    fn the_change_cursor_and_the_bloom_round_trip_through_json() {
        let mut ledger = earned("/home/dk/Downloads", Stage::Expanded);
        ledger.change_cursor = 4_812;
        ledger.ingest_changes(
            &[arrived("/home/dk/Downloads/a.mp4")],
            at(2, 0.0),
            &GardenConfig::default(),
        );
        let json = serde_json::to_string(&ledger).unwrap();
        let back: Ledger = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ledger);
        // A ledger written before blooms existed still loads.
        let old: Ledger = serde_json::from_str(
            r#"{"clock":{"session":1,"active_ms":0},"contexts":[{"root":"/home/dk/x",
                "score":1.5,"last":{"session":1,"active_ms":0},"material_seed":7}]}"#,
        )
        .unwrap();
        assert_eq!(old.change_cursor, 0);
        assert!(old.contexts[0].bloom.is_none());
    }
}
