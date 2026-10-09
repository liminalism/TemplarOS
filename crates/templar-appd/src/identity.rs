//! Registry of manifests and live instances; PID → cgroup → instance → canonical app.

use crate::variants::{LogicalIndex, PersistedLogical};
use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use templar_types::{
    AppCatalog, AppId, AppManifest, AppVariantId, IdentityConfidence, InstanceId, LogicalAppId,
    WindowClaims, unit,
};

/// One live launch.
#[derive(Clone, Debug)]
pub struct InstanceRecord {
    pub id: InstanceId,
    pub app_id: AppId,
    pub systemd_unit: String,

    /// Value returned by systemd's `ControlGroup` property.
    pub control_group: PathBuf,
}

impl InstanceRecord {
    /// Structural consistency of a record supplied by the launch broker or systemd inventory.
    /// A cgroup's self-describing name alone is never evidence that a launch was registered.
    pub fn has_managed_identity(&self) -> bool {
        managed_cgroup(&self.control_group).is_some_and(|(app, id, group)| {
            app == self.app_id
                && id == self.id
                && group == self.control_group
                && self.systemd_unit == unit::unit_name(&app, id)
        })
    }
}

/// Answer to "which application owns this PID/window?".
#[derive(Clone, Debug)]
pub struct IdentityResolution {
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub confidence: IdentityConfidence,
    pub claim_matches_manifest: bool,
    pub process_control_group: PathBuf,
}

/// Authoritative desktop launch syntax retained alongside its imported manifest.
#[derive(Clone, Debug)]
pub struct LaunchTemplate {
    pub exec: crate::desktop_entry::ExecTemplate,
    pub desktop_file_path: PathBuf,
}

#[derive(Debug)]
struct InstalledManifest {
    manifest: Arc<AppManifest>,
    launch_template: Option<Arc<LaunchTemplate>>,
}

/// Manifests + instances. Cheap to clone behind an `Arc`; all methods take `&self`.
#[derive(Default, Debug)]
pub struct Registry {
    manifests: RwLock<HashMap<AppId, InstalledManifest>>,
    instances: RwLock<HashMap<InstanceId, InstanceRecord>>,
    workloads: RwLock<HashMap<templar_types::WorkloadId, templar_types::WorkloadRecord>>,
    /// Which installed copies are copies of the same application. Presentation only: nothing
    /// keyed here grants anything, and it is recomputed whenever the installed set changes.
    logical: RwLock<LogicalIndex>,
    /// Variant ids that used to name something else. Consulted on lookup; never written back into
    /// the tables that hold stored ids.
    aliases: RwLock<BTreeMap<AppVariantId, AppVariantId>>,
}

impl Registry {
    /// Insert a native manifest, discarding any desktop launch syntax it replaces.
    pub fn insert_manifest(&self, manifest: AppManifest) {
        self.manifests.write().insert(
            manifest.id.clone(),
            InstalledManifest {
                manifest: Arc::new(manifest),
                launch_template: None,
            },
        );
    }

    pub fn insert_desktop_manifest(
        &self,
        manifest: AppManifest,
        exec: crate::desktop_entry::ExecTemplate,
        desktop_file_path: PathBuf,
    ) {
        self.update_manifests([(manifest, exec, desktop_file_path)], std::iter::empty());
    }

    /// The original desktop launch template for the installed copy, if it was imported.
    pub fn launch_template(&self, app: &AppId) -> Option<Arc<LaunchTemplate>> {
        self.manifests
            .read()
            .get(app)
            .and_then(|installed| installed.launch_template.clone())
    }

    /// Publish one committed installed-copy batch without changing running process identities.
    /// Each manifest and its authoritative desktop launch source are replaced together.
    pub fn update_manifests<'a>(
        &self,
        imported: impl IntoIterator<Item = (AppManifest, crate::desktop_entry::ExecTemplate, PathBuf)>,
        removed: impl IntoIterator<Item = &'a AppId>,
    ) {
        let mut manifests = self.manifests.write();
        for id in removed {
            manifests.remove(id);
        }
        for (manifest, exec, desktop_file_path) in imported {
            manifests.insert(
                manifest.id.clone(),
                InstalledManifest {
                    manifest: Arc::new(manifest),
                    launch_template: Some(Arc::new(LaunchTemplate {
                        exec,
                        desktop_file_path,
                    })),
                },
            );
        }
    }

    pub fn register_instance(&self, record: InstanceRecord) {
        self.instances.write().insert(record.id, record);
    }

    pub fn register_workload(&self, record: templar_types::WorkloadRecord) {
        self.workloads.write().insert(record.id, record);
    }

    pub fn resolve_workload(&self, group: &Path) -> Option<templar_types::WorkloadRecord> {
        self.workloads
            .read()
            .values()
            .filter(|record| group.starts_with(&record.control_group))
            .max_by_key(|record| component_count(&record.control_group))
            .cloned()
    }

    pub fn list_workloads(&self) -> Vec<templar_types::WorkloadRecord> {
        self.workloads.read().values().cloned().collect()
    }

    pub(crate) fn dead_workloads(&self) -> Result<Vec<templar_types::WorkloadId>> {
        self.workloads
            .read()
            .values()
            .filter_map(|record| match crate::workload::is_live(record) {
                Ok(true) => None,
                Ok(false) => Some(Ok(record.id)),
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub(crate) fn remove_workloads(&self, ids: &[templar_types::WorkloadId]) {
        let mut workloads = self.workloads.write();
        for id in ids {
            workloads.remove(id);
        }
    }

    pub fn remove_instance(&self, instance: InstanceId) {
        self.instances.write().remove(&instance);
    }

    pub fn instance(&self, instance: InstanceId) -> Option<InstanceRecord> {
        self.instances.read().get(&instance).cloned()
    }

    pub fn contains_instance(&self, instance: InstanceId) -> bool {
        self.instances.read().contains_key(&instance)
    }

    /// Recompute the logical grouping over everything currently installed, keeping whatever a
    /// previous run (or a person) already decided.
    pub fn regroup(&self, persisted: &BTreeMap<AppVariantId, PersistedLogical>) -> LogicalIndex {
        let index = crate::variants::group_catalog(self, persisted);
        self.set_logical_index(index.clone());
        index
    }

    /// Publish a grouping only after its owner has committed the corresponding variant rows.
    pub fn set_logical_index(&self, index: LogicalIndex) {
        *self.logical.write() = index;
    }

    /// The current grouping.
    pub fn logical_index(&self) -> LogicalIndex {
        self.logical.read().clone()
    }

    /// The logical application a variant belongs to, resolving a stale id through the alias table
    /// first.
    pub fn logical_of(&self, variant: &AppVariantId) -> Option<LogicalAppId> {
        let variant = self.resolve_variant(variant);
        self.logical.read().logical_of(&variant).cloned()
    }

    /// Replace the alias table (old variant id -> current one).
    ///
    /// An alias whose *old* id is itself an installed variant is dropped: a live copy always
    /// answers for its own id. That case is real — the Flatpak export of `firefox.desktop` moved
    /// off `desktop-import.firefox.desktop`, and the distribution's package may now be sitting on
    /// exactly that id.
    pub fn set_variant_aliases(&self, aliases: BTreeMap<AppVariantId, AppVariantId>) {
        let installed = self.manifests.read();
        *self.aliases.write() = aliases
            .into_iter()
            .filter(|(alias, _)| !installed.contains_key(alias))
            .collect();
    }

    /// The id a possibly-stale variant id names today. An id with no alias is returned unchanged,
    /// so this is safe to call on every lookup.
    pub fn resolve_variant(&self, variant: &AppVariantId) -> AppVariantId {
        self.aliases
            .read()
            .get(variant)
            .cloned()
            .unwrap_or_else(|| variant.clone())
    }

    /// The manifest for `id`, following an alias when the stored id has been superseded.
    pub fn manifest_resolved(&self, id: &AppId) -> Option<Arc<AppManifest>> {
        self.manifest(id)
            .or_else(|| self.manifest(&self.resolve_variant(id)))
    }

    pub fn list_instances(&self) -> Vec<InstanceRecord> {
        self.instances.read().values().cloned().collect()
    }

    /// Resolve a window's identity from its peer PID and self-declared claims.
    pub fn resolve_window(&self, pid: u32, claims: &WindowClaims) -> Result<IdentityResolution> {
        let before = process_start_time(pid)?;
        let process_control_group = unified_cgroup_of(pid)?;
        let identity = self.resolve_cgroup(process_control_group, claims);
        anyhow::ensure!(
            process_start_time(pid)? == before,
            "PID changed during identity resolution"
        );
        Ok(identity)
    }

    /// Resolve from an already-known cgroup path (used by tests and by callers that already
    /// hold the path, for example the governor's `app.slice` scan).
    pub fn resolve_cgroup(
        &self,
        process_control_group: PathBuf,
        claims: &WindowClaims,
    ) -> IdentityResolution {
        // Match the deepest registered ancestor. This permits app-owned sub-cgroups while
        // retaining the parent application identity.
        let registered = {
            let instances = self.instances.read();
            instances
                .values()
                .filter(|instance| {
                    instance.has_managed_identity()
                        && !process_control_group
                            .components()
                            .any(|part| matches!(part, std::path::Component::ParentDir))
                        && process_control_group.starts_with(&instance.control_group)
                })
                .max_by_key(|instance| component_count(&instance.control_group))
                .cloned()
        };

        if let Some(instance) = registered {
            let claim_matches = self
                .manifest(&instance.app_id)
                .is_some_and(|manifest| manifest.accepts_window_claims(claims));

            return IdentityResolution {
                app_id: Some(instance.app_id),
                instance_id: Some(instance.id),
                confidence: if claim_matches {
                    IdentityConfidence::VerifiedLaunch
                } else {
                    IdentityConfidence::VerifiedLaunchClaimMismatch
                },
                claim_matches_manifest: claim_matches,
                process_control_group,
            };
        }

        // Compatibility fallback for applications launched outside appd: accept a *unique*
        // claim match only.
        let claim_candidate = {
            let manifests = self.manifests.read();
            let mut matches = manifests
                .values()
                .map(|installed| &installed.manifest)
                .filter(|manifest| manifest.accepts_window_claims(claims))
                .cloned();
            let first = matches.next();
            if matches.next().is_some() {
                None
            } else {
                first
            }
        };

        if let Some(manifest) = claim_candidate {
            return IdentityResolution {
                app_id: Some(manifest.id.clone()),
                instance_id: None,
                confidence: IdentityConfidence::ClaimOnly,
                claim_matches_manifest: true,
                process_control_group,
            };
        }

        IdentityResolution {
            app_id: None,
            instance_id: None,
            confidence: IdentityConfidence::Unknown,
            claim_matches_manifest: false,
            process_control_group,
        }
    }
}

impl AppCatalog for Registry {
    fn manifest(&self, id: &AppId) -> Option<Arc<AppManifest>> {
        self.manifests
            .read()
            .get(id)
            .map(|installed| Arc::clone(&installed.manifest))
    }

    fn manifests(&self) -> Vec<Arc<AppManifest>> {
        self.manifests
            .read()
            .values()
            .map(|installed| Arc::clone(&installed.manifest))
            .collect()
    }

    fn resolve_alias(&self, stored: &AppId) -> AppId {
        self.resolve_variant(stored)
    }

    fn logical_app_of(&self, id: &AppId) -> Option<LogicalAppId> {
        self.logical_of(id)
    }

    fn sibling_variants(&self, id: &AppId) -> Vec<AppId> {
        let resolved = self.resolve_variant(id);
        let Some(logical) = self.logical_of(&resolved) else {
            return Vec::new();
        };
        self.logical
            .read()
            .variants_of(&logical)
            .iter()
            .filter(|variant| **variant != resolved && **variant != *id)
            .cloned()
            .collect()
    }
}

fn component_count(path: &Path) -> usize {
    path.components().count()
}

/// The cgroup-v2 membership of `pid` (`0::$PATH` in `/proc/PID/cgroup`).
pub fn unified_cgroup_of(pid: u32) -> Result<PathBuf> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("reading cgroup membership for PID {pid}"))?;
    parse_proc_cgroup(&text).with_context(|| format!("PID {pid} has no cgroup-v2 membership"))
}

pub(crate) fn parse_proc_cgroup(text: &str) -> Option<PathBuf> {
    let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    if path.ends_with(" (deleted)") {
        return None;
    }
    let path = PathBuf::from(path);
    (path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)))
    .then_some(path)
}

fn managed_cgroup(path: &Path) -> Option<(AppId, InstanceId, PathBuf)> {
    let mut parts = path.components();
    if parts.next()? != std::path::Component::RootDir || parts.next()?.as_os_str() != "user.slice" {
        return None;
    }
    let user_slice = parts
        .next()?
        .as_os_str()
        .to_str()?
        .strip_prefix("user-")?
        .strip_suffix(".slice")?;
    let uid: u32 = user_slice.parse().ok()?;
    if parts.next()?.as_os_str() != format!("user@{uid}.service").as_str()
        || parts.next()?.as_os_str() != "app.slice"
    {
        return None;
    }
    let unit_name = parts.next()?.as_os_str().to_str()?;
    let (app, instance) = unit::parse_unit_name(unit_name)?;
    let group = PathBuf::from(format!(
        "/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/{unit_name}"
    ));
    Some((app, instance, group))
}

fn process_start_time(pid: u32) -> Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat.rsplit_once(')').context("malformed process stat")?;
    fields
        .split_whitespace()
        .nth(19)
        .context("missing process start time")?
        .parse()
        .context("invalid process start time")
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_types::{AppAliases, ResourceClass};

    fn manifest(id: &str, wayland: &str) -> AppManifest {
        AppManifest {
            id: id.parse().unwrap(),
            display_name: id.into(),
            primary_desktop_file_id: format!("{id}.desktop"),
            executable: PathBuf::from("/usr/bin/true"),
            aliases: AppAliases {
                wayland_app_ids: [wayland.to_owned()].into(),
                ..AppAliases::default()
            },
            mime_types: std::collections::BTreeSet::default(),
            resource_class: ResourceClass::Normal,
            supports_checkpoint: false,
            icon: None,
            categories: std::collections::BTreeSet::default(),
            keywords: std::collections::BTreeSet::default(),
            no_display: false,
            provenance: None,
            provides: Vec::new(),
        }
    }

    #[test]
    fn desktop_templates_replace_remove_and_yield_to_native_manifests() {
        use crate::desktop_entry::ExecTemplate;

        let registry = Registry::default();
        let app: AppId = "org.example.Viewer".parse().unwrap();
        let first = ExecTemplate::parse("viewer --profile \"Work profile\" %F %k");
        let first_path = PathBuf::from("/usr/share/applications/viewer.desktop");
        registry.insert_desktop_manifest(
            manifest(app.as_str(), app.as_str()),
            first.clone(),
            first_path.clone(),
        );
        let retained = registry.launch_template(&app).unwrap();
        assert_eq!(retained.exec, first);
        assert_eq!(retained.desktop_file_path, first_path);

        let replacement = ExecTemplate::parse("viewer --private %u");
        let replacement_path =
            PathBuf::from("/home/person/.local/share/applications/viewer.desktop");
        registry.update_manifests(
            [(
                manifest(app.as_str(), app.as_str()),
                replacement.clone(),
                replacement_path.clone(),
            )],
            [&app],
        );
        let current = registry.launch_template(&app).unwrap();
        assert_eq!(current.exec, replacement);
        assert_eq!(current.desktop_file_path, replacement_path);
        assert_eq!(
            retained.exec, first,
            "an outstanding snapshot is not mutated"
        );

        registry.update_manifests(std::iter::empty(), [&app]);
        assert!(registry.manifest(&app).is_none());
        assert!(registry.launch_template(&app).is_none());

        registry.insert_desktop_manifest(manifest(app.as_str(), app.as_str()), first, first_path);
        registry.insert_manifest(manifest(app.as_str(), app.as_str()));
        assert!(registry.manifest(&app).is_some());
        assert!(registry.launch_template(&app).is_none());
    }

    #[test]
    fn registered_cgroup_beats_claims() {
        let registry = Registry::default();
        registry.insert_manifest(manifest("org.templaros.Files", "org.templaros.Files"));
        registry.insert_manifest(manifest("org.example.Impostor", "org.templaros.Files"));

        let id = InstanceId::new();
        let app: AppId = "org.templaros.Files".parse().unwrap();
        let unit = unit::unit_name(&app, id);
        let cg = PathBuf::from(format!(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/{unit}"
        ));
        registry.register_instance(InstanceRecord {
            id,
            app_id: app.clone(),
            systemd_unit: unit,
            control_group: cg.clone(),
        });

        // Even a claim that would be ambiguous resolves through the cgroup.
        let claims = WindowClaims {
            wayland_app_id: Some("org.templaros.Files".into()),
            ..Default::default()
        };
        let res = registry.resolve_cgroup(cg.join("child"), &claims);
        assert_eq!(res.app_id, Some(app));
        assert_eq!(res.instance_id, Some(id));
        assert_eq!(res.confidence, IdentityConfidence::VerifiedLaunch);

        // A mismatching claim is recorded, not trusted.
        let claims = WindowClaims {
            wayland_app_id: Some("org.example.Impostor".into()),
            ..Default::default()
        };
        let res = registry.resolve_cgroup(cg, &claims);
        assert_eq!(
            res.confidence,
            IdentityConfidence::VerifiedLaunchClaimMismatch
        );
        assert_eq!(res.app_id.unwrap().as_str(), "org.templaros.Files");
    }

    #[test]
    fn self_describing_unit_requires_trusted_registry_restore() {
        let registry = Registry::default();
        registry.insert_manifest(manifest("org.templaros.Files", "org.templaros.Files"));
        let id = InstanceId::new();
        let app: AppId = "org.templaros.Files".parse().unwrap();
        let cg = PathBuf::from(format!(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/{}",
            unit::unit_name(&app, id)
        ));
        let unregistered = registry.resolve_cgroup(cg.clone(), &WindowClaims::default());
        assert!(!unregistered.confidence.is_verified());
        assert_eq!(unregistered.instance_id, None);
        assert!(!registry.contains_instance(id));

        registry.register_instance(InstanceRecord {
            id,
            app_id: app.clone(),
            systemd_unit: unit::unit_name(&app, id),
            control_group: cg.clone(),
        });
        let restored = registry.resolve_cgroup(cg, &WindowClaims::default());
        assert_eq!(restored.instance_id, Some(id));
        assert!(restored.confidence.is_verified());
    }

    #[test]
    fn ambiguous_claims_are_unknown_and_unique_claims_are_claim_only() {
        let registry = Registry::default();
        registry.insert_manifest(manifest("org.templaros.Files", "shared"));
        registry.insert_manifest(manifest("org.example.Other", "shared"));
        registry.insert_manifest(manifest("org.example.Unique", "unique"));

        let ambiguous = WindowClaims {
            wayland_app_id: Some("shared".into()),
            ..Default::default()
        };
        let res = registry.resolve_cgroup(PathBuf::from("/user.slice/foo.scope"), &ambiguous);
        assert_eq!(res.confidence, IdentityConfidence::Unknown);

        let unique = WindowClaims {
            wayland_app_id: Some("unique".into()),
            ..Default::default()
        };
        let res = registry.resolve_cgroup(PathBuf::from("/user.slice/foo.scope"), &unique);
        assert_eq!(res.confidence, IdentityConfidence::ClaimOnly);
        assert_eq!(res.app_id.unwrap().as_str(), "org.example.Unique");
    }

    #[test]
    fn proc_cgroup_parsing() {
        assert_eq!(
            parse_proc_cgroup("0::/user.slice/x.service\n"),
            Some(PathBuf::from("/user.slice/x.service"))
        );
        assert_eq!(
            parse_proc_cgroup("0::/user.slice/x.service (deleted)\n"),
            None
        );
        assert_eq!(parse_proc_cgroup("1:name=systemd:/\n"), None);
        assert_eq!(parse_proc_cgroup("0::app.slice/forged.service\n"), None);
        assert_eq!(
            parse_proc_cgroup("0::/user.slice/../forged.service\n"),
            None
        );
    }

    #[test]
    fn delegated_or_malformed_managed_names_never_verify() {
        let registry = Registry::default();
        registry.insert_manifest(manifest("org.templaros.Files", "org.templaros.Files"));
        let app: AppId = "org.templaros.Files".parse().unwrap();
        let unit = unit::unit_name(&app, InstanceId::new());
        for path in [
            format!("/untrusted/app.slice/{unit}"),
            format!(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/arbitrary.service/app.slice/{unit}"
            ),
            format!("/user.slice/user-1001.slice/user@1000.service/app.slice/{unit}"),
        ] {
            let resolution = registry.resolve_cgroup(PathBuf::from(path), &WindowClaims::default());
            assert_eq!(resolution.confidence, IdentityConfidence::Unknown);
            assert_eq!(resolution.instance_id, None);
        }
    }

    #[test]
    fn inconsistent_registered_records_and_traversal_never_verify() {
        let registry = Registry::default();
        registry.insert_manifest(manifest("org.templaros.Files", "org.templaros.Files"));
        let app: AppId = "org.templaros.Files".parse().unwrap();
        let id = InstanceId::new();
        let unit_name = unit::unit_name(&app, id);
        let cg = PathBuf::from(format!(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/{unit_name}"
        ));
        for record in [
            InstanceRecord {
                id,
                app_id: app.clone(),
                systemd_unit: unit_name.clone(),
                control_group: PathBuf::from("/"),
            },
            InstanceRecord {
                id,
                app_id: app.clone(),
                systemd_unit: "arbitrary.service".into(),
                control_group: cg.clone(),
            },
            InstanceRecord {
                id,
                app_id: "org.example.Impostor".parse().unwrap(),
                systemd_unit: unit_name.clone(),
                control_group: cg.clone(),
            },
        ] {
            registry.register_instance(record);
            assert!(
                !registry
                    .resolve_cgroup(cg.clone(), &WindowClaims::default())
                    .confidence
                    .is_verified()
            );
        }
        registry.register_instance(InstanceRecord {
            id,
            app_id: app,
            systemd_unit: unit_name,
            control_group: cg.clone(),
        });
        assert!(
            !registry
                .resolve_cgroup(cg.join("../forged"), &WindowClaims::default())
                .confidence
                .is_verified()
        );
    }
}
