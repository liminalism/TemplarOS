//! Which installed copies are copies of the same application.
//!
//! A person sees one Firefox. The system may hold the distribution's package, the Flathub build
//! and a `.desktop` file they wrote themselves — three [variants](lege_types::AppVariantId), one
//! [logical application](lege_types::LogicalAppId). This module decides which copies belong
//! together, from evidence the copies themselves supply.
//!
//! **A wrong answer here is a cosmetic mistake, by construction.** Nothing in LegeOS grants
//! anything to a logical application: MIME defaults, provider declarations, autostart approvals
//! and instances all key on the variant. Merging two unrelated programs draws them as one row in a
//! launcher; it cannot hand one of them the other's permissions. That is why these rules are
//! allowed to be heuristic at all, and why rule 4 in particular — a shared window class and a
//! shared name — is acceptable.
//!
//! Two variants join if **any** of five rules holds:
//!
//! 1. one declares the other's logical application, or claims the other's desktop entry as an
//!    alias — an explicit statement by whoever packaged it;
//! 2. they export the same desktop-entry id from different origins — the ordinary case of "the
//!    Flatpak and the deb of the same program";
//! 3. one's Flatpak id appears in the other's `aliases.flatpak_ids` — how a native manifest says
//!    "the Flatpak of me is me";
//! 4. they share a `StartupWMClass` *and* a display name — the rule that unifies Debian's
//!    `firefox.desktop` with Flathub's `org.mozilla.firefox`, which agree on neither id;
//! 5. they resolve to the same executable, and that executable is not a multiplexer — `flatpak`,
//!    `snap`, `env`, an interpreter — which would otherwise merge everything it starts.
//!
//! Minted logical ids are **persisted**. Grouping is recomputed from whatever is installed at the
//! time, so without persistence a logical id would change the moment a copy was uninstalled, and
//! anything that remembered one — a project, a pinned tile — would be pointing at nothing.

use lege_types::{
    AppId, AppManifest, AppOriginKind, AppVariantId, LogicalAppId, TrustTier, app::AppCatalog,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::Arc,
};

/// Where a variant's logical application came from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogicalSource {
    /// The rules below concluded it.
    #[default]
    Derived,
    /// A person said so, by splitting or merging by hand. Never overwritten by a rule.
    User,
}

impl LogicalSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Derived => "derived",
            Self::User => "user",
        }
    }

    /// Decode the stored form. Anything unrecognised is `Derived`, so an unreadable value is
    /// re-derived rather than frozen as an untouchable manual decision.
    pub fn from_str_or_derived(value: &str) -> Self {
        if value == "user" {
            Self::User
        } else {
            Self::Derived
        }
    }
}

/// What a previous run concluded (or a person decided) about one variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedLogical {
    pub logical: LogicalAppId,
    pub source: LogicalSource,
}

/// Programs that start *other* programs. Two variants sharing one of these as their executable
/// share a launcher, not an identity, and rule 5 must not fire on them.
const MULTIPLEXERS: &[&str] = &[
    "flatpak",
    "snap",
    "env",
    "sh",
    "bash",
    "dash",
    "zsh",
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "java",
    "wine",
    "wine64",
    "steam",
    "xdg-open",
    "gio",
    "gtk-launch",
    "kioclient",
    "electron",
    "appimagelauncher",
];

fn is_multiplexer(executable: &std::path::Path) -> bool {
    executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| MULTIPLEXERS.contains(&name))
}

fn flatpak_ids(manifest: &AppManifest) -> BTreeSet<&str> {
    let mut ids: BTreeSet<&str> = manifest
        .aliases
        .flatpak_ids
        .iter()
        .map(String::as_str)
        .collect();
    if let Some(package_ref) = manifest
        .provenance
        .as_ref()
        .and_then(|p| p.package_ref.as_deref())
        && manifest.provenance.as_ref().map(|p| p.origin) == Some(AppOriginKind::Flatpak)
    {
        ids.insert(package_ref);
    }
    ids
}

fn desktop_ids(manifest: &AppManifest) -> BTreeSet<&str> {
    let mut ids: BTreeSet<&str> = manifest
        .aliases
        .desktop_file_ids
        .iter()
        .map(String::as_str)
        .collect();
    if !manifest.primary_desktop_file_id.is_empty() {
        ids.insert(manifest.primary_desktop_file_id.as_str());
    }
    ids
}

fn wm_classes(manifest: &AppManifest) -> BTreeSet<String> {
    manifest
        .aliases
        .startup_wm_classes
        .iter()
        .map(|class| class.to_ascii_lowercase())
        .collect()
}

fn intersects<T: Ord>(left: &BTreeSet<T>, right: &BTreeSet<T>) -> bool {
    left.iter().any(|item| right.contains(item))
}

/// Whether two installed copies are copies of the same application. Symmetric by construction:
/// every rule is written over an unordered pair.
pub fn same_application(left: &AppManifest, right: &AppManifest) -> bool {
    if left.id == right.id {
        return true;
    }

    // 1 — somebody said so.
    if let (Some(a), Some(b)) = (left.declared_logical_app(), right.declared_logical_app())
        && a == b
    {
        return true;
    }
    let left_desktop = desktop_ids(left);
    let right_desktop = desktop_ids(right);
    if left
        .aliases
        .desktop_file_ids
        .iter()
        .any(|id| id.as_str() == right.primary_desktop_file_id)
        || right
            .aliases
            .desktop_file_ids
            .iter()
            .any(|id| id.as_str() == left.primary_desktop_file_id)
    {
        return true;
    }

    // 2 — the same desktop entry, exported by two origins.
    if intersects(&left_desktop, &right_desktop) {
        return true;
    }

    // 3 — one names the other's Flatpak.
    if intersects(&flatpak_ids(left), &flatpak_ids(right)) {
        return true;
    }

    // 4 — the same window class *and* the same name. Either alone is far too loose: window
    // classes collide across toolkits, and "Files" is a name three programs use.
    if !left.display_name.is_empty()
        && left.display_name.eq_ignore_ascii_case(&right.display_name)
        && intersects(&wm_classes(left), &wm_classes(right))
    {
        return true;
    }

    // 5 — the same executable, unless that executable starts anything at all.
    left.executable == right.executable && !is_multiplexer(&left.executable)
}

/// Variants grouped into logical applications, with a default variant per logical application.
#[derive(Clone, Debug, Default)]
pub struct LogicalIndex {
    by_variant: BTreeMap<AppVariantId, LogicalAppId>,
    members: BTreeMap<LogicalAppId, Vec<AppVariantId>>,
    sources: BTreeMap<AppVariantId, LogicalSource>,
}

impl LogicalIndex {
    /// The logical application a variant belongs to.
    pub fn logical_of(&self, variant: &AppVariantId) -> Option<&LogicalAppId> {
        self.by_variant.get(variant)
    }

    /// Every variant of a logical application, default first.
    pub fn variants_of(&self, logical: &LogicalAppId) -> &[AppVariantId] {
        self.members.get(logical).map_or(&[], Vec::as_slice)
    }

    /// The variant to launch when a caller names only the logical application.
    pub fn default_variant(&self, logical: &LogicalAppId) -> Option<&AppVariantId> {
        self.variants_of(logical).first()
    }

    /// Every logical application, in id order.
    pub fn logical_apps(&self) -> impl Iterator<Item = (&LogicalAppId, &Vec<AppVariantId>)> {
        self.members.iter()
    }

    /// Where a variant's logical application came from — a rule, or a person.
    pub fn source_of(&self, variant: &AppVariantId) -> LogicalSource {
        self.sources.get(variant).copied().unwrap_or_default()
    }

    /// The (variant, logical, source) rows to persist.
    pub fn rows(&self) -> Vec<(AppVariantId, LogicalAppId, LogicalSource)> {
        self.by_variant
            .iter()
            .map(|(variant, logical)| (variant.clone(), logical.clone(), self.source_of(variant)))
            .collect()
    }
}

/// Union-find with path halving: which component a variant has been merged into.
fn find(parent: &mut [usize], mut index: usize) -> usize {
    while parent[index] != index {
        parent[index] = parent[parent[index]];
        index = parent[index];
    }
    index
}

/// Rank a variant as a default: the copy a person most likely means when they say the name.
///
/// System trust first (a packaged copy over a file the account can rewrite), then the origin's own
/// order, then the copy that has been here longest, then the id — so the answer is total and
/// stable rather than dependent on scan order.
fn default_rank(manifest: &AppManifest) -> (u8, u8, i64, String) {
    let provenance = manifest.provenance.as_ref();
    let trust = match manifest.trust() {
        TrustTier::System => 0,
        TrustTier::User => 1,
    };
    let origin = match provenance.map_or(AppOriginKind::Unknown, |p| p.origin) {
        AppOriginKind::NativeManifest => 0,
        AppOriginKind::DistroPackage => 1,
        AppOriginKind::Flatpak => 2,
        AppOriginKind::NativeUserManifest => 3,
        AppOriginKind::UserDesktopEntry => 4,
        AppOriginKind::Unknown => 5,
    };
    let first_seen = provenance.map_or(0, |p| p.first_seen_ms);
    (trust, origin, first_seen, manifest.id.to_string())
}

/// Group `manifests` into logical applications, honouring what was persisted before.
///
/// `persisted` is a previous run's conclusions (and any manual override). A component keeps the id
/// it already had, so uninstalling one copy of an application never renames the application.
pub fn group(
    manifests: &[Arc<AppManifest>],
    persisted: &BTreeMap<AppVariantId, PersistedLogical>,
) -> LogicalIndex {
    let mut parent: Vec<usize> = (0..manifests.len()).collect();
    for left in 0..manifests.len() {
        for right in (left + 1)..manifests.len() {
            if same_application(&manifests[left], &manifests[right]) {
                let (a, b) = (find(&mut parent, left), find(&mut parent, right));
                if a != b {
                    parent[a] = b;
                }
            }
        }
    }

    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..manifests.len() {
        let root = find(&mut parent, index);
        components.entry(root).or_default().push(index);
    }

    let mut index = LogicalIndex::default();
    let mut taken: BTreeSet<LogicalAppId> = BTreeSet::new();
    // Deterministic order: components are named in the order of their lowest variant id, so two
    // runs over the same installed set mint the same ids.
    let mut ordered: Vec<Vec<usize>> = components.into_values().collect();
    for component in &mut ordered {
        component.sort_by_key(|position| manifests[*position].id.to_string());
    }
    ordered.sort_by_key(|component| manifests[component[0]].id.to_string());

    for component in ordered {
        let members: Vec<&Arc<AppManifest>> = component.iter().map(|i| &manifests[*i]).collect();
        let (logical, source) = choose_logical(&members, persisted, &taken);
        taken.insert(logical.clone());

        let mut variants: Vec<AppVariantId> = members.iter().map(|m| m.id.clone()).collect();
        variants.sort_by_key(|id| {
            members
                .iter()
                .find(|m| &m.id == id)
                .map(|m| default_rank(m))
                .unwrap_or_default()
        });
        for variant in &variants {
            index.by_variant.insert(variant.clone(), logical.clone());
            index.sources.insert(variant.clone(), source);
        }
        index.members.insert(logical, variants);
    }
    index
}

/// The logical id for one component, in order of authority: a person's decision, then a declared
/// id, then whatever a previous run persisted, then a freshly minted one.
fn choose_logical(
    members: &[&Arc<AppManifest>],
    persisted: &BTreeMap<AppVariantId, PersistedLogical>,
    taken: &BTreeSet<LogicalAppId>,
) -> (LogicalAppId, LogicalSource) {
    let mut user_choice: Option<LogicalAppId> = None;
    let mut declared: Option<LogicalAppId> = None;
    let mut previous: Option<LogicalAppId> = None;
    for manifest in members {
        if let Some(record) = persisted.get(&manifest.id) {
            match record.source {
                LogicalSource::User => {
                    user_choice = Some(min_option(user_choice, record.logical.clone()));
                }
                LogicalSource::Derived => {
                    previous = Some(min_option(previous, record.logical.clone()));
                }
            }
        }
        if let Some(id) = manifest.declared_logical_app() {
            declared = Some(min_option(declared, id.clone()));
        }
    }
    if let Some(logical) = user_choice {
        return (logical, LogicalSource::User);
    }
    if let Some(logical) = declared {
        return (logical, LogicalSource::Derived);
    }
    if let Some(logical) = previous {
        return (logical, LogicalSource::Derived);
    }
    (mint(members, taken), LogicalSource::Derived)
}

/// Keep the lower of two candidate ids, so the answer does not depend on scan order.
fn min_option(current: Option<LogicalAppId>, candidate: LogicalAppId) -> LogicalAppId {
    match current {
        Some(existing) if existing <= candidate => existing,
        _ => candidate,
    }
}

/// Mint a logical id for a component that has never had one.
///
/// Named after the component's clearest human handle — its shared window class, else the base name
/// of the lowest desktop entry id, else the lowest variant id — under `logical.`, so a person
/// reading a log or a stored project can tell what it refers to. Disambiguated against ids already
/// minted in this run, because two unrelated applications called "Files" must not collide into
/// one.
fn mint(members: &[&Arc<AppManifest>], taken: &BTreeSet<LogicalAppId>) -> LogicalAppId {
    let handle = members
        .iter()
        .flat_map(|manifest| wm_classes(manifest))
        .min()
        .or_else(|| {
            members
                .iter()
                .flat_map(|manifest| desktop_ids(manifest))
                .map(|id| id.trim_end_matches(".desktop").to_ascii_lowercase())
                .min()
        })
        .unwrap_or_else(|| members[0].id.to_string());

    let sanitized: String = handle
        .chars()
        .take(200)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = if sanitized.trim_matches('.').is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    };

    let mut candidate = logical_id(&base);
    let mut suffix = 2;
    while taken.contains(&candidate) {
        candidate = logical_id(&format!("{base}-{suffix}"));
        suffix += 1;
    }
    candidate
}

fn logical_id(base: &str) -> LogicalAppId {
    LogicalAppId::from_str(&format!("logical.{base}"))
        .unwrap_or_else(|_| LogicalAppId::from_str("logical.unknown").expect("static id is valid"))
}

/// Group everything a catalog holds. The convenience the registry and the D-Bus surface use.
pub fn group_catalog(
    catalog: &dyn AppCatalog,
    persisted: &BTreeMap<AppVariantId, PersistedLogical>,
) -> LogicalIndex {
    let mut manifests = catalog.manifests();
    manifests.sort_by_key(|manifest| manifest.id.to_string());
    group(&manifests, persisted)
}

/// Resolve a possibly-stale variant id through the alias table. Stored ids elsewhere are never
/// rewritten; they are resolved here, on read, which is what makes the Flatpak id change reversible.
pub fn resolve_alias<'a>(
    aliases: &'a BTreeMap<AppVariantId, AppVariantId>,
    id: &'a AppId,
) -> &'a AppId {
    aliases.get(id).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::{AppAliases, AppProvenance, ResourceClass};
    use std::path::PathBuf;

    fn manifest(id: &str, name: &str, origin: AppOriginKind) -> AppManifest {
        AppManifest {
            id: id.parse().unwrap(),
            display_name: name.into(),
            primary_desktop_file_id: format!("{id}.desktop"),
            executable: PathBuf::from(format!("/usr/bin/{id}")),
            aliases: AppAliases::default(),
            mime_types: BTreeSet::new(),
            resource_class: ResourceClass::Normal,
            supports_checkpoint: false,
            icon: None,
            categories: BTreeSet::new(),
            keywords: BTreeSet::new(),
            no_display: false,
            provenance: Some(AppProvenance::from_origin(origin)),
        }
    }

    fn arcs(manifests: Vec<AppManifest>) -> Vec<Arc<AppManifest>> {
        let mut manifests: Vec<Arc<AppManifest>> = manifests.into_iter().map(Arc::new).collect();
        manifests.sort_by_key(|m| m.id.to_string());
        manifests
    }

    #[test]
    fn the_debian_firefox_and_the_flathub_firefox_are_one_application() {
        // Rule 4, and the acceptance case: the two agree on neither desktop id nor executable.
        let mut distro = manifest(
            "desktop-import.firefox.desktop",
            "Firefox",
            AppOriginKind::DistroPackage,
        );
        distro.primary_desktop_file_id = "firefox.desktop".into();
        distro.executable = PathBuf::from("/usr/bin/firefox");
        distro.aliases.startup_wm_classes.insert("firefox".into());

        let mut flatpak = manifest(
            "flatpak.org.mozilla.firefox",
            "Firefox",
            AppOriginKind::Flatpak,
        );
        flatpak.primary_desktop_file_id = "org.mozilla.firefox.desktop".into();
        flatpak.executable = PathBuf::from("/var/lib/flatpak/exports/bin/org.mozilla.firefox");
        flatpak.aliases.startup_wm_classes.insert("Firefox".into());
        flatpak
            .aliases
            .flatpak_ids
            .insert("org.mozilla.firefox".into());

        let index = group(&arcs(vec![distro, flatpak]), &BTreeMap::new());
        assert_eq!(index.logical_apps().count(), 1);
        let (logical, variants) = index.logical_apps().next().unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(
            index.default_variant(logical).unwrap().as_str(),
            "desktop-import.firefox.desktop",
            "the distribution's copy outranks the Flatpak"
        );
    }

    #[test]
    fn a_shared_name_alone_does_not_merge_and_neither_does_a_shared_class() {
        let mut a = manifest("org.a.Files", "Files", AppOriginKind::DistroPackage);
        a.aliases.startup_wm_classes.insert("a-files".into());
        let mut b = manifest("org.b.Files", "Files", AppOriginKind::DistroPackage);
        b.aliases.startup_wm_classes.insert("b-files".into());
        let index = group(&arcs(vec![a, b]), &BTreeMap::new());
        assert_eq!(
            index.logical_apps().count(),
            2,
            "same name, different class"
        );

        let mut c = manifest("org.c.Tool", "Tool One", AppOriginKind::DistroPackage);
        c.aliases.startup_wm_classes.insert("shared".into());
        let mut d = manifest("org.d.Tool", "Tool Two", AppOriginKind::DistroPackage);
        d.aliases.startup_wm_classes.insert("shared".into());
        let index = group(&arcs(vec![c, d]), &BTreeMap::new());
        assert_eq!(
            index.logical_apps().count(),
            2,
            "same class, different name"
        );
    }

    #[test]
    fn a_multiplexer_executable_never_merges_what_it_starts() {
        let mut a = manifest("org.a.One", "One", AppOriginKind::UserDesktopEntry);
        a.executable = PathBuf::from("/usr/bin/flatpak");
        let mut b = manifest("org.b.Two", "Two", AppOriginKind::UserDesktopEntry);
        b.executable = PathBuf::from("/usr/bin/flatpak");
        let index = group(&arcs(vec![a, b]), &BTreeMap::new());
        assert_eq!(
            index.logical_apps().count(),
            2,
            "`flatpak` starts everything; sharing it proves nothing"
        );

        // The same executable that is *not* a multiplexer does merge (rule 5).
        let mut c = manifest("org.c.One", "One", AppOriginKind::UserDesktopEntry);
        c.executable = PathBuf::from("/usr/bin/thing");
        let mut d = manifest("org.d.Two", "Two", AppOriginKind::UserDesktopEntry);
        d.executable = PathBuf::from("/usr/bin/thing");
        c.aliases.startup_wm_classes.clear();
        d.aliases.startup_wm_classes.clear();
        let index = group(&arcs(vec![c, d]), &BTreeMap::new());
        assert_eq!(index.logical_apps().count(), 1);
    }

    #[test]
    fn a_declaration_wins_over_a_mint_and_a_persisted_id_survives_an_uninstall() {
        let mut a = manifest("org.a.Editor", "Editor", AppOriginKind::NativeManifest);
        a.provenance.as_mut().unwrap().logical = Some("logical.editor".parse().unwrap());
        let mut b = manifest("org.b.Editor", "Editor", AppOriginKind::Flatpak);
        b.provenance.as_mut().unwrap().logical = Some("logical.editor".parse().unwrap());

        let index = group(&arcs(vec![a.clone(), b.clone()]), &BTreeMap::new());
        assert_eq!(
            index.logical_of(&a.id).unwrap().as_str(),
            "logical.editor",
            "a declared id is used verbatim"
        );

        // Now a component with no declaration: its minted id must be kept across a re-group in
        // which one member has been uninstalled.
        let mut c = manifest("org.c.Thing", "Thing", AppOriginKind::DistroPackage);
        c.aliases.startup_wm_classes.insert("thing".into());
        let mut d = manifest("flatpak.org.c.Thing", "Thing", AppOriginKind::Flatpak);
        d.aliases.startup_wm_classes.insert("thing".into());
        d.executable = PathBuf::from("/var/lib/flatpak/exports/bin/org.c.Thing");

        let first = group(&arcs(vec![c.clone(), d.clone()]), &BTreeMap::new());
        let minted = first.logical_of(&c.id).unwrap().clone();
        assert_eq!(minted.as_str(), "logical.thing");

        let persisted: BTreeMap<AppVariantId, PersistedLogical> = first
            .rows()
            .into_iter()
            .map(|(variant, logical, source)| (variant, PersistedLogical { logical, source }))
            .collect();
        let after_uninstall = group(&arcs(vec![c.clone()]), &persisted);
        assert_eq!(
            after_uninstall.logical_of(&c.id),
            Some(&minted),
            "uninstalling a copy must not rename the application"
        );
    }

    #[test]
    fn a_person_s_decision_outranks_every_rule() {
        let a = manifest("org.a.Thing", "Thing", AppOriginKind::DistroPackage);
        let mut persisted = BTreeMap::new();
        persisted.insert(
            a.id.clone(),
            PersistedLogical {
                logical: "logical.my-thing".parse().unwrap(),
                source: LogicalSource::User,
            },
        );
        let index = group(&arcs(vec![a.clone()]), &persisted);
        assert_eq!(
            index.logical_of(&a.id).unwrap().as_str(),
            "logical.my-thing"
        );
        assert_eq!(index.source_of(&a.id), LogicalSource::User);
    }

    #[test]
    fn two_unrelated_components_with_the_same_handle_do_not_collide() {
        let mut a = manifest("org.a.Files", "A Files", AppOriginKind::DistroPackage);
        a.aliases.startup_wm_classes.insert("files".into());
        let mut b = manifest("org.b.Files", "B Files", AppOriginKind::DistroPackage);
        b.aliases.startup_wm_classes.insert("files".into());
        // Different names, so rule 4 does not fire; different executables, so rule 5 does not.
        let index = group(&arcs(vec![a.clone(), b.clone()]), &BTreeMap::new());
        assert_eq!(index.logical_apps().count(), 2);
        let ids: BTreeSet<String> = index
            .logical_apps()
            .map(|(logical, _)| logical.to_string())
            .collect();
        assert_eq!(ids.len(), 2, "minted ids must be distinct: {ids:?}");
    }

    #[test]
    fn an_unrelated_variant_is_its_own_logical_application() {
        let a = manifest("org.legeos.Files", "Files", AppOriginKind::NativeManifest);
        let index = group(&arcs(vec![a.clone()]), &BTreeMap::new());
        assert!(index.logical_of(&a.id).is_some());
        assert_eq!(index.variants_of(index.logical_of(&a.id).unwrap()).len(), 1);
    }
}
