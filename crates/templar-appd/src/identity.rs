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

/// Answer to "which application owns this PID/window?".
#[derive(Clone, Debug)]
pub struct IdentityResolution {
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub confidence: IdentityConfidence,
    pub claim_matches_manifest: bool,
    pub process_control_group: PathBuf,
}

/// Manifests + instances. Cheap to clone behind an `Arc`; all methods take `&self`.
#[derive(Default, Debug)]
pub struct Registry {
    manifests: RwLock<HashMap<AppId, Arc<AppManifest>>>,
    instances: RwLock<HashMap<InstanceId, InstanceRecord>>,
    /// Which installed copies are copies of the same application. Presentation only: nothing
    /// keyed here grants anything, and it is recomputed whenever the installed set changes.
    logical: RwLock<LogicalIndex>,
    /// Variant ids that used to name something else. Consulted on lookup; never written back into
    /// the tables that hold stored ids.
    aliases: RwLock<BTreeMap<AppVariantId, AppVariantId>>,
}

impl Registry {
    pub fn insert_manifest(&self, manifest: AppManifest) {
        self.manifests
            .write()
            .insert(manifest.id.clone(), Arc::new(manifest));
    }

    pub fn register_instance(&self, record: InstanceRecord) {
        self.instances.write().insert(record.id, record);
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
        *self.logical.write() = index.clone();
        index
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
        let process_control_group = unified_cgroup_of(pid)?;
        Ok(self.resolve_cgroup(process_control_group, claims))
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
                .filter(|instance| process_control_group.starts_with(&instance.control_group))
                .max_by_key(|instance| component_count(&instance.control_group))
                .cloned()
        };

        // Fallback: the unit name itself is self-describing (ADR-0002), so a launch that
        // predates this registry process (sessiond restart) still resolves as verified.
        let registered = registered.or_else(|| {
            let (app_id, id) = unit::parse_cgroup_path(&process_control_group)?;
            self.manifest(&app_id)?;
            Some(InstanceRecord {
                id,
                app_id: app_id.clone(),
                systemd_unit: unit::unit_name(&app_id, id),
                control_group: process_control_group.clone(),
            })
        });

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
        self.manifests.read().get(id).cloned()
    }

    fn manifests(&self) -> Vec<Arc<AppManifest>> {
        self.manifests.read().values().cloned().collect()
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
///
/// A production version additionally correlates the live Wayland peer credential with a PID
/// start time or pidfd so a stale PID cannot be confused with a later process after reuse.
pub fn unified_cgroup_of(pid: u32) -> Result<PathBuf> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("reading cgroup membership for PID {pid}"))?;
    parse_proc_cgroup(&text).with_context(|| format!("PID {pid} has no cgroup-v2 membership"))
}

pub(crate) fn parse_proc_cgroup(text: &str) -> Option<PathBuf> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| {
            let path = path.strip_suffix(" (deleted)").unwrap_or(path);
            PathBuf::from(path)
        })
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
        }
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
    fn self_describing_unit_survives_registry_restart() {
        let registry = Registry::default();
        registry.insert_manifest(manifest("org.templaros.Files", "org.templaros.Files"));
        let id = InstanceId::new();
        let app: AppId = "org.templaros.Files".parse().unwrap();
        let cg = PathBuf::from(format!(
            "/user.slice/app.slice/{}",
            unit::unit_name(&app, id)
        ));

        let res = registry.resolve_cgroup(cg, &WindowClaims::default());
        assert_eq!(res.instance_id, Some(id));
        assert!(res.confidence.is_verified());
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
            Some(PathBuf::from("/user.slice/x.service"))
        );
        assert_eq!(parse_proc_cgroup("1:name=systemd:/\n"), None);
    }
}
