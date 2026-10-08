//! D-Bus interfaces `org.templaros.AppIdentity1` and `org.templaros.Launcher1`.
//!
//! The compositor keeps a local cache and never blocks its render loop on these calls: on a
//! cache miss it maps a window provisionally as unverified, requests resolution asynchronously
//! and updates the window record when the answer arrives.

use crate::identity::Registry;
use crate::launcher::{self, LaunchBackend};
use std::sync::Arc;
use templar_types::{AppCatalog, AppId, InstanceId, LogicalAppId, WindowClaims};
use zbus::{fdo, object_server::SignalEmitter};

/// One row of [`IdentityApi::list_applications`]:
/// `(app_id, display_name, desktop_file_id, icon, categories, keywords, no_display)`.
///
/// Named rather than written inline so the signature stays one readable line in the source, the
/// XML and the client proxy alike.
pub type ApplicationRow = (
    String,
    String,
    String,
    String,
    Vec<String>,
    Vec<String>,
    bool,
);

/// One row of [`IdentityApi::list_logical_apps`]:
/// `(logical_app_id, display_name, icon, variant_count, default_variant)`.
pub type LogicalAppRow = (String, String, String, u32, String);

/// One row of [`IdentityApi::list_variants`]: `(variant_app_id, display_name, provenance_json)`.
pub type VariantRow = (String, String, String);

/// One row of [`IdentityApi::list_instances_detailed`]:
/// `(instance_id, variant_app_id, logical_app_id, systemd_unit, control_group)`.
pub type InstanceDetailRow = (String, String, String, String, String);

/// `org.templaros.AppIdentity1`
#[derive(Debug)]
pub struct IdentityApi {
    registry: Arc<Registry>,
}

impl IdentityApi {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[zbus::interface(name = "org.templaros.AppIdentity1")]
impl IdentityApi {
    /// Returns `(canonical_app_id, instance_id, confidence, client_claim_matches_manifest)`.
    async fn resolve_window(
        &self,
        pid: u32,
        wayland_app_id: &str,
        startup_wm_class: &str,
        sandbox_app_id: &str,
    ) -> fdo::Result<(String, String, u8, bool)> {
        let claims = WindowClaims {
            wayland_app_id: nonempty(wayland_app_id),
            startup_wm_class: nonempty(startup_wm_class),
            sandbox_app_id: nonempty(sandbox_app_id),
        };

        let resolution = self
            .registry
            .resolve_window(pid, &claims)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        Ok((
            resolution
                .app_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            resolution
                .instance_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            resolution.confidence as u8,
            resolution.claim_matches_manifest,
        ))
    }

    /// `(instance_id, app_id, systemd_unit, control_group)` for every live instance.
    /// A signal-driven API replaces this once the prototype works.
    async fn list_instances(&self) -> Vec<(String, String, String, String)> {
        self.registry
            .list_instances()
            .into_iter()
            .map(|instance| {
                (
                    instance.id.to_string(),
                    instance.app_id.to_string(),
                    instance.systemd_unit,
                    instance.control_group.display().to_string(),
                )
            })
            .collect()
    }

    /// Canonical IDs of every installed manifest.
    async fn list_apps(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .registry
            .manifests()
            .into_iter()
            .map(|manifest| manifest.id.to_string())
            .collect();
        ids.sort();
        ids
    }

    /// Every installed manifest as
    /// `(app_id, display_name, desktop_file_id, icon, categories, keywords, no_display)`.
    ///
    /// [`list_apps`](Self::list_apps) returns bare IDs, which is enough to launch something a
    /// caller already knows about and useless for offering a person a choice: a launcher needs
    /// the name to show, the icon to draw, the terms to match, and whether the entry is meant to
    /// be offered at all. `no_display` is reported rather than filtered here because the same
    /// listing answers "what is installed" (`templar app list`) as well as "what may I offer" — the
    /// caller building a menu is the one that drops those rows.
    async fn list_applications(&self) -> Vec<ApplicationRow> {
        let mut rows: Vec<_> = self
            .registry
            .manifests()
            .into_iter()
            .map(|manifest| {
                (
                    manifest.id.to_string(),
                    manifest.display_name.clone(),
                    manifest.primary_desktop_file_id.clone(),
                    manifest.icon.clone().unwrap_or_default(),
                    manifest.categories.iter().cloned().collect(),
                    manifest.keywords.iter().cloned().collect(),
                    manifest.no_display,
                )
            })
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// One row per *logical* application: the thing a person means when they say "Firefox",
    /// whether the machine holds one copy of it or three.
    ///
    /// The name and icon come from the default variant, which is the copy that would actually
    /// launch — so what a launcher draws and what it starts agree.
    async fn list_logical_apps(&self) -> Vec<LogicalAppRow> {
        let index = self.registry.logical_index();
        let mut rows: Vec<LogicalAppRow> = index
            .logical_apps()
            .map(|(logical, variants)| {
                let default = variants.first();
                let manifest = default.and_then(|id| self.registry.manifest(id));
                (
                    logical.to_string(),
                    manifest
                        .as_ref()
                        .map(|m| m.display_name.clone())
                        .unwrap_or_default(),
                    manifest
                        .as_ref()
                        .and_then(|m| m.icon.clone())
                        .unwrap_or_default(),
                    u32::try_from(variants.len()).unwrap_or(u32::MAX),
                    default.map(ToString::to_string).unwrap_or_default(),
                )
            })
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// The copies of one logical application, default first — the order a variant submenu should
    /// offer them in.
    async fn list_variants(&self, logical_app_id: &str) -> fdo::Result<Vec<VariantRow>> {
        let logical: LogicalAppId =
            logical_app_id
                .parse()
                .map_err(|error: templar_types::AppIdError| {
                    fdo::Error::InvalidArgs(error.to_string())
                })?;
        let index = self.registry.logical_index();
        Ok(index
            .variants_of(&logical)
            .iter()
            .filter_map(|variant| {
                let manifest = self.registry.manifest(variant)?;
                Some((
                    variant.to_string(),
                    manifest.display_name.clone(),
                    manifest
                        .provenance
                        .as_ref()
                        .and_then(|p| serde_json::to_string(p).ok())
                        .unwrap_or_else(|| "{}".to_owned()),
                ))
            })
            .collect())
    }

    /// Which logical application a variant is a copy of. A stale id is resolved through the alias
    /// table first, so a caller holding an id from before a Flatpak was renamed still gets an
    /// answer.
    async fn logical_app_of(&self, variant_app_id: &str) -> fdo::Result<String> {
        let variant: AppId =
            variant_app_id
                .parse()
                .map_err(|error: templar_types::AppIdError| {
                    fdo::Error::InvalidArgs(error.to_string())
                })?;
        Ok(self
            .registry
            .logical_of(&variant)
            .map(|logical| logical.to_string())
            .unwrap_or_default())
    }

    /// `(instance_id, variant_app_id, logical_app_id, systemd_unit, control_group)` for every live
    /// instance. The logical id travels alongside so a caller grouping windows by application does
    /// not have to ask once per instance.
    async fn list_instances_detailed(&self) -> Vec<InstanceDetailRow> {
        let index = self.registry.logical_index();
        let mut rows: Vec<InstanceDetailRow> = self
            .registry
            .list_instances()
            .into_iter()
            .map(|instance| {
                let logical = index
                    .logical_of(&instance.app_id)
                    .map(ToString::to_string)
                    .unwrap_or_default();
                (
                    instance.id.to_string(),
                    instance.app_id.to_string(),
                    logical,
                    instance.systemd_unit,
                    instance.control_group.display().to_string(),
                )
            })
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    /// The installed set changed — an import, an uninstall, a re-scan. Re-read the listings.
    #[zbus(signal)]
    pub async fn applications_changed(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Told after every successful launch, with the *resolved variant* id and the arguments it got.
///
/// appd knows nothing about what watches — a folder argument means something to the Home garden
/// and nothing here — so this is a closure the host injects, not an interface appd reasons about.
///
/// The instance is passed because it is the only thing that ties the launch to the *window* it
/// eventually produces: `ShellToplevel.instance_id` is the same value, and a place that knows
/// which instances came out of it knows which windows are its own.
#[derive(Clone, Debug)]
pub struct LaunchEvent {
    pub app_id: AppId,
    pub arguments: Vec<String>,
    pub instance: InstanceId,
    /// The actual canonical process cwd, never a synthetic argv entry.
    pub directory: Option<std::path::PathBuf>,
}

pub type LaunchObserver = Box<dyn Fn(LaunchEvent) + Send + Sync>;

/// `org.templaros.Launcher1`
pub struct LauncherApi {
    registry: Arc<Registry>,
    backend: LaunchBackend,
    connection: zbus::Connection,
    observer: Option<LaunchObserver>,
}

/// Hand-written because a `dyn Fn` has no `Debug`; the observer is reported as present or not,
/// which is the only thing about it anyone debugging a launch would want to know.
impl std::fmt::Debug for LauncherApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LauncherApi")
            .field("backend", &self.backend)
            .field("observed", &self.observer.is_some())
            .finish_non_exhaustive()
    }
}

impl LauncherApi {
    pub fn new(
        registry: Arc<Registry>,
        backend: LaunchBackend,
        connection: zbus::Connection,
    ) -> Self {
        Self {
            registry,
            backend,
            connection,
            observer: None,
        }
    }

    /// Watch every successful launch. Optional: the daemon launches identically without one.
    #[must_use]
    pub fn with_observer(mut self, observer: LaunchObserver) -> Self {
        self.observer = Some(observer);
        self
    }

}

/// Both launch paths end here, so neither can grow a route that is unobserved.
///
/// Free rather than a method so a test can exercise it without a bus connection, which
/// `LauncherApi` needs only in order to actually launch something.
fn observed(observer: Option<&LaunchObserver>, event: LaunchEvent) {
    if let Some(observer) = observer {
        observer(event);
    }
}

/// Frozen string wire accepts a UTF-8 absolute pathname or a local file URI whose
/// percent decoding preserves Unix bytes. Both forms validate and resolve the same cwd.
fn launch_directory(directory: &str) -> fdo::Result<std::path::PathBuf> {
    let directory = if directory.starts_with('/') {
        std::borrow::Cow::Borrowed(std::path::Path::new(directory))
    } else {
        std::borrow::Cow::Owned(templar_types::uri_list::decode_uri(directory)
            .ok_or_else(|| fdo::Error::InvalidArgs("launch directory must be an absolute pathname or local file URI".into()))?)
    };
    if !directory.is_absolute() || !directory.is_dir() {
        return Err(fdo::Error::InvalidArgs("launch directory must be an existing absolute directory".into()));
    }
    directory.canonicalize().map_err(|error| fdo::Error::InvalidArgs(error.to_string()))
}

#[zbus::interface(name = "org.templaros.Launcher1")]
impl LauncherApi {
    /// Launch `app_id` with `arguments`; returns the new instance ID.
    async fn launch(
        &self,
        app_id: &str,
        arguments: Vec<String>,
        activation_token: &str,
    ) -> fdo::Result<String> {
        let app_id: AppId = app_id.parse().map_err(|error: templar_types::AppIdError| {
            fdo::Error::InvalidArgs(error.to_string())
        })?;
        let manifest = self
            .registry
            .manifest(&app_id)
            .ok_or_else(|| fdo::Error::FileNotFound(format!("no manifest for {app_id}")))?;

        let record = launcher::launch(
            Arc::clone(&self.registry),
            manifest,
            &arguments,
            nonempty(activation_token).as_deref(),
            self.backend,
            Some(&self.connection),
        )
        .await
        .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        observed(self.observer.as_ref(), LaunchEvent { app_id, arguments, instance: record.id, directory: None });
        Ok(record.id.to_string())
    }

    /// A real canonical process cwd from an absolute UTF-8 pathname or local file URI.
    async fn launch_in_directory_v1(
        &self,
        app_id: &str,
        arguments: Vec<String>,
        directory: &str,
        activation_token: &str,
    ) -> fdo::Result<String> {
        let app_id: AppId = app_id.parse().map_err(|error: templar_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))?;
        let directory = launch_directory(directory)?;
        let manifest = self.registry.manifest(&app_id)
            .ok_or_else(|| fdo::Error::FileNotFound(format!("no manifest for {app_id}")))?;
        let record = launcher::launch_in_directory(
            self.registry.clone(), manifest, &arguments, nonempty(activation_token).as_deref(),
            self.backend, Some(&self.connection), Some(&directory),
        ).await.map_err(|error| fdo::Error::Failed(error.to_string()))?;
        observed(self.observer.as_ref(), LaunchEvent { app_id, arguments, instance: record.id, directory: Some(directory) });
        Ok(record.id.to_string())
    }

    /// Local file targets use the registered desktop template, never a raw URI argv guess.
    async fn launch_files_v1(
        &self,
        app_id: &str,
        file_uris: Vec<String>,
        activation_token: &str,
    ) -> fdo::Result<String> {
        let app_id: AppId = app_id.parse().map_err(|error: templar_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))?;
        let manifest = self.registry.manifest(&app_id)
            .ok_or_else(|| fdo::Error::FileNotFound(format!("no manifest for {app_id}")))?;
        let record = launcher::launch_files(
            self.registry.clone(), manifest, &file_uris, nonempty(activation_token).as_deref(),
            self.backend, Some(&self.connection),
        ).await.map_err(|error| fdo::Error::Failed(error.to_string()))?;
        observed(self.observer.as_ref(), LaunchEvent { app_id, arguments: file_uris, instance: record.id, directory: None });
        Ok(record.id.to_string())
    }

    /// Launch a logical application: "open Firefox", not "open the Flatpak Firefox".
    ///
    /// With `preferred_variant` empty the daemon picks the default, which is a trust decision — a
    /// packaged copy outranks a file the account can rewrite — and so belongs here rather than in
    /// every caller. A named variant that is not a copy of this application is refused rather than
    /// launched: it would be a different program than the one asked for.
    async fn launch_logical(
        &self,
        logical_app_id: &str,
        preferred_variant: &str,
        arguments: Vec<String>,
        activation_token: &str,
    ) -> fdo::Result<String> {
        let logical: LogicalAppId =
            logical_app_id
                .parse()
                .map_err(|error: templar_types::AppIdError| {
                    fdo::Error::InvalidArgs(error.to_string())
                })?;
        let index = self.registry.logical_index();
        let variants = index.variants_of(&logical);
        if variants.is_empty() {
            return Err(fdo::Error::FileNotFound(format!(
                "no application {logical} is installed"
            )));
        }

        let chosen = match nonempty(preferred_variant) {
            None => variants[0].clone(),
            Some(preferred) => {
                let preferred: AppId =
                    preferred
                        .parse()
                        .map_err(|error: templar_types::AppIdError| {
                            fdo::Error::InvalidArgs(error.to_string())
                        })?;
                let preferred = self.registry.resolve_variant(&preferred);
                if !variants.contains(&preferred) {
                    return Err(fdo::Error::InvalidArgs(format!(
                        "{preferred} is not a copy of {logical}"
                    )));
                }
                preferred
            }
        };

        let manifest = self
            .registry
            .manifest(&chosen)
            .ok_or_else(|| fdo::Error::FileNotFound(format!("no manifest for {chosen}")))?;
        let record = launcher::launch(
            Arc::clone(&self.registry),
            manifest,
            &arguments,
            nonempty(activation_token).as_deref(),
            self.backend,
            Some(&self.connection),
        )
        .await
        .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        // The resolved variant, not the logical id: what actually started is what an observer
        // has to be told, or it cannot tell two copies of Firefox apart.
        observed(self.observer.as_ref(), LaunchEvent { app_id: chosen, arguments, instance: record.id, directory: None });
        Ok(record.id.to_string())
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Registry;
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
    };
    use templar_types::{AppAliases, AppManifest, AppOriginKind, AppProvenance, ResourceClass};

    #[test]
    fn launch_directory_accepts_path_and_local_uri_and_preserves_canonical_native_bytes() {
        use std::{ffi::OsString, os::unix::{ffi::OsStringExt, fs::symlink}};
        let temp = tempfile::tempdir().unwrap();
        let native = temp.path().join(OsString::from_vec(b"project-\xff-${HOME}-$VAR-$$".to_vec()));
        std::fs::create_dir(&native).unwrap();
        let alias = temp.path().join("UTF-8 alias");
        symlink(&native, &alias).unwrap();
        let canonical = native.canonicalize().unwrap();
        assert_eq!(launch_directory(alias.to_str().unwrap()).unwrap(), canonical);
        assert_eq!(launch_directory(&templar_types::uri_list::encode_path(&alias)).unwrap(), canonical);
        let uri = templar_types::uri_list::encode_path(&native);
        assert_eq!(launch_directory(&uri).unwrap(), canonical);
        assert_eq!(launch_directory(&uri.replacen("file://", "file://localhost", 1)).unwrap(), canonical);
        let file = temp.path().join("not-a-directory");
        std::fs::write(&file, "fixture").unwrap();
        for invalid in [
            "relative/path".to_owned(), "https://example.com/".to_owned(),
            "file://remote/tmp".to_owned(), "file:relative".to_owned(),
            file.to_str().unwrap().to_owned(), templar_types::uri_list::encode_path(&file),
            temp.path().join("missing").to_str().unwrap().to_owned(),
        ] {
            assert!(matches!(launch_directory(&invalid), Err(fdo::Error::InvalidArgs(_))));
        }
    }

    /// A registry holding the two Firefoxes a real machine can end up with: Debian's package and
    /// Flathub's build. They agree on neither id, desktop entry, nor executable.
    fn two_firefoxes() -> Arc<Registry> {
        let registry = Arc::new(Registry::default());
        registry.insert_manifest(AppManifest {
            id: "desktop-import.firefox.desktop".parse().unwrap(),
            display_name: "Firefox".into(),
            primary_desktop_file_id: "firefox.desktop".into(),
            executable: PathBuf::from("/usr/bin/firefox"),
            aliases: AppAliases {
                startup_wm_classes: ["firefox".to_owned()].into(),
                ..AppAliases::default()
            },
            mime_types: BTreeSet::default(),
            resource_class: ResourceClass::Interactive,
            supports_checkpoint: false,
            icon: Some("firefox".into()),
            categories: BTreeSet::default(),
            keywords: BTreeSet::default(),
            no_display: false,
            provenance: Some(AppProvenance::from_origin(AppOriginKind::DistroPackage)),
            provides: Vec::new(),
        });
        registry.insert_manifest(AppManifest {
            id: "flatpak.org.mozilla.firefox".parse().unwrap(),
            display_name: "Firefox".into(),
            primary_desktop_file_id: "org.mozilla.firefox.desktop".into(),
            executable: PathBuf::from("/var/lib/flatpak/exports/bin/org.mozilla.firefox"),
            aliases: AppAliases {
                startup_wm_classes: ["Firefox".to_owned()].into(),
                flatpak_ids: ["org.mozilla.firefox".to_owned()].into(),
                ..AppAliases::default()
            },
            mime_types: BTreeSet::default(),
            resource_class: ResourceClass::Interactive,
            supports_checkpoint: false,
            icon: Some("org.mozilla.firefox".into()),
            categories: BTreeSet::default(),
            keywords: BTreeSet::default(),
            no_display: false,
            provenance: Some(AppProvenance::from_origin(AppOriginKind::Flatpak)),
            provides: Vec::new(),
        });
        registry.regroup(&BTreeMap::new());
        registry
    }

    #[tokio::test]
    async fn a_person_with_two_firefoxes_installed_is_offered_one() {
        let registry = two_firefoxes();
        let api = IdentityApi::new(Arc::clone(&registry));

        assert_eq!(
            api.list_applications().await.len(),
            2,
            "both copies are installed and both must remain addressable"
        );

        let logical = api.list_logical_apps().await;
        assert_eq!(logical.len(), 1, "a person sees one Firefox: {logical:?}");
        let (id, name, icon, count, default) = logical[0].clone();
        assert_eq!(name, "Firefox");
        assert_eq!(icon, "firefox", "the icon is the one that would launch");
        assert_eq!(count, 2);
        assert_eq!(
            default, "desktop-import.firefox.desktop",
            "the packaged copy is the default"
        );

        let variants = api.list_variants(&id).await.unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].0, default, "the default is offered first");
        let provenance: AppProvenance = serde_json::from_str(&variants[1].2).unwrap();
        assert_eq!(provenance.origin, AppOriginKind::Flatpak);

        assert_eq!(
            api.logical_app_of("flatpak.org.mozilla.firefox")
                .await
                .unwrap(),
            id
        );
        assert!(
            api.logical_app_of("org.example.NotInstalled")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn asking_for_a_variant_of_something_else_is_refused_not_launched() {
        let registry = two_firefoxes();
        registry.insert_manifest(AppManifest {
            id: "org.example.Other".parse().unwrap(),
            display_name: "Other".into(),
            primary_desktop_file_id: "org.example.Other.desktop".into(),
            executable: PathBuf::from("/usr/bin/other"),
            aliases: AppAliases::default(),
            mime_types: BTreeSet::default(),
            resource_class: ResourceClass::Normal,
            supports_checkpoint: false,
            icon: None,
            categories: BTreeSet::default(),
            keywords: BTreeSet::default(),
            no_display: false,
            provenance: Some(AppProvenance::from_origin(AppOriginKind::DistroPackage)),
            provides: Vec::new(),
        });
        registry.regroup(&BTreeMap::new());

        let index = registry.logical_index();
        let firefox = registry
            .logical_of(&"desktop-import.firefox.desktop".parse().unwrap())
            .unwrap();
        assert!(
            !index
                .variants_of(&firefox)
                .contains(&"org.example.Other".parse().unwrap()),
            "an unrelated application is not a copy of Firefox, and LaunchLogical refuses it"
        );
    }
}
