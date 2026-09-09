# Integrated Rust service samples

These samples treat the four components as one system:

```text
file manager / portal
        │
        ▼
 associationd ──canonical AppId──▶ appd ──systemd unit/cgroup──▶ application
                                      │                              │
                                      ▼                              ▼
                                  resourced ◀──instance/cgroup── window service
                                                                   │
                                                                   ▼
                                                                compositor
```

That is the important part. The application identity is not just for dock grouping. It becomes the common key used by launching, file opening, resource accounting, window grouping, permissions, notifications, and session restoration.

Your current COSMIC dock patch already reconciles pinned IDs and `StartupWMClass` to prevent Flatpak, Steam, and Wine windows from creating duplicate icons.  Your Files and portal forks also currently retain related state independently.  The code below promotes both ideas into system services rather than continuing to add application-specific matching rules.

The snippets are intended as a close-to-compilable architectural scaffold. They omit packaging, complete persistence migrations, portal boilerplate, and Polkit policy. I could not `cargo check` them here because this environment does not currently contain a Rust toolchain.

---

## 1. Workspace layout

```text
desktop-os/
├── Cargo.toml
├── crates/
│   ├── os-types/
│   │   └── src/lib.rs
│   └── window-service/
│       └── src/lib.rs
└── services/
    ├── appd/
    │   └── src/main.rs
    ├── associationd/
    │   └── src/main.rs
    └── resourced/
        └── src/main.rs
```

```toml
# Cargo.toml

[workspace]
resolver = "3"
members = [
    "crates/os-types",
    "crates/window-service",
    "services/appd",
    "services/associationd",
    "services/resourced",
]

[workspace.package]
edition = "2024"
license = "MPL-2.0"

[workspace.dependencies]
anyhow = "1"
async-trait = "0.1"
libc = "0.2"
parking_lot = "0.12"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = {
    version = "1",
    features = [
        "fs",
        "macros",
        "process",
        "rt-multi-thread",
        "sync",
        "time",
    ],
}
uuid = { version = "1", features = ["serde", "v4"] }
zbus = {
    version = "5",
    default-features = false,
    features = ["tokio"],
}
```

The current `zbus` API supports service interfaces through `#[zbus::interface]` and a Tokio-integrated connection builder. ([Docs.rs][1])

---

# 2. Shared application and instance types

The canonical application ID is controlled by the OS registry. A Wayland `app_id`, X11 `WM_CLASS`, desktop-file ID, Flatpak ID, and executable path are **aliases or evidence**, not separate application identities.

```rust
// crates/os-types/src/lib.rs

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fmt,
    path::PathBuf,
    str::FromStr,
};
use thiserror::Error;
use uuid::Uuid;

#[derive(
    Clone,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
)]
#[serde(transparent)]
pub struct AppId(String);

#[derive(Debug, Error)]
pub enum AppIdError {
    #[error("application ID must be between 3 and 255 bytes")]
    InvalidLength,

    #[error("application ID must contain at least two non-empty segments")]
    InvalidSegments,

    #[error("application ID contains an invalid character")]
    InvalidCharacter,
}

impl FromStr for AppId {
    type Err = AppIdError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if !(3..=255).contains(&raw.len()) {
            return Err(AppIdError::InvalidLength);
        }

        let segments: Vec<&str> = raw.split('.').collect();

        if segments.len() < 2 || segments.iter().any(|part| part.is_empty()) {
            return Err(AppIdError::InvalidSegments);
        }

        let valid = segments.iter().all(|segment| {
            segment
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
        });

        if !valid {
            return Err(AppIdError::InvalidCharacter);
        }

        Ok(Self(raw.to_owned()))
    }
}

impl AppId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
    Deserialize,
)]
#[serde(transparent)]
pub struct InstanceId(Uuid);

impl InstanceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn compact(&self) -> String {
        self.0.simple().to_string()
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppAliases {
    /// Names such as `org.example.Editor.desktop`.
    pub desktop_file_ids: BTreeSet<String>,

    /// Values supplied through xdg_toplevel.set_app_id.
    pub wayland_app_ids: BTreeSet<String>,

    /// XWayland WM_CLASS / desktop StartupWMClass values.
    pub startup_wm_classes: BTreeSet<String>,

    /// Flatpak application IDs.
    pub flatpak_ids: BTreeSet<String>,

    /// Compatibility evidence only. A path alone is not the root of trust.
    pub executable_paths: BTreeSet<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ResourceClass {
    SessionCritical,
    Interactive,
    Normal,
    Background,
    Disposable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppManifest {
    pub id: AppId,
    pub display_name: String,

    /// Compatibility ID exported to mimeapps.list and desktop menus.
    pub primary_desktop_file_id: String,

    /// Exact executable used when appd launches the application.
    pub executable: PathBuf,

    pub aliases: AppAliases,

    /// Exact MIME types or broad entries such as `image/*`.
    pub mime_types: BTreeSet<String>,

    pub resource_class: ResourceClass,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WindowClaims {
    pub wayland_app_id: Option<String>,
    pub startup_wm_class: Option<String>,
    pub sandbox_app_id: Option<String>,
}

impl AppManifest {
    /// Checks whether at least one supplied compatibility claim belongs to
    /// this canonical application.
    ///
    /// This does not prove identity. The registered launch cgroup does.
    pub fn accepts_window_claims(&self, claims: &WindowClaims) -> bool {
        let wayland_match = claims
            .wayland_app_id
            .as_ref()
            .is_some_and(|claim| self.aliases.wayland_app_ids.contains(claim));

        let wm_class_match = claims.startup_wm_class.as_ref().is_some_and(|claim| {
            self.aliases
                .startup_wm_classes
                .iter()
                .any(|known| known.eq_ignore_ascii_case(claim))
        });

        let sandbox_match = claims
            .sandbox_app_id
            .as_ref()
            .is_some_and(|claim| self.aliases.flatpak_ids.contains(claim));

        wayland_match || wm_class_match || sandbox_match
    }

    pub fn supports_mime(&self, mime: &str) -> bool {
        if self.mime_types.contains(mime) {
            return true;
        }

        let Some((major, _minor)) = mime.split_once('/') else {
            return false;
        };

        self.mime_types.contains(&format!("{major}/*"))
    }
}
```

A manifest could look like this:

```json
{
  "id": "app.lege.Viewer",
  "display_name": "Lege Viewer",
  "primary_desktop_file_id": "app.lege.Viewer.desktop",
  "executable": "/usr/bin/lege-viewer",
  "aliases": {
    "desktop_file_ids": ["app.lege.Viewer.desktop"],
    "wayland_app_ids": ["app.lege.Viewer"],
    "startup_wm_classes": ["lege-viewer"],
    "flatpak_ids": [],
    "executable_paths": ["/usr/bin/lege-viewer"]
  },
  "mime_types": [
    "application/pdf",
    "image/tiff"
  ],
  "resource_class": "Interactive"
}
```

---

# 3. Application identity service

## Identity rule

For a normal launch:

1. `appd` creates the application instance.
2. It starts the process in a dedicated systemd unit and cgroup.
3. The compositor obtains the client PID from the Wayland connection.
4. It asks `appd` which registered instance owns that PID’s cgroup.
5. Client-supplied `app_id` and `WM_CLASS` are checked for consistency.
6. A mismatch is logged, but it cannot replace the verified identity.

This prevents an application from claiming `app_id=org.example.FileManager` and being treated as the file manager.

Linux cgroup v2 exposes a process’s unified membership in `/proc/PID/cgroup` as `0::$PATH`; child processes inherit their parent’s cgroup at fork time. ([Kernel Documentation][2])

## Registry and resolver

```rust
// services/appd/src/identity.rs

use anyhow::{Context, Result};
use os_types::{AppId, AppManifest, InstanceId, WindowClaims};
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IdentityConfidence {
    Unknown = 0,

    /// A unique manifest matched only the application's own claim.
    /// Useful for legacy/manual launches, but not trusted for privileged actions.
    ClaimOnly = 1,

    /// The PID belongs to a registered appd launch cgroup.
    VerifiedLaunch = 2,

    /// The cgroup proves the application, but the window claim disagrees.
    VerifiedLaunchClaimMismatch = 3,
}

#[derive(Clone, Debug)]
pub struct InstanceRecord {
    pub id: InstanceId,
    pub app_id: AppId,
    pub systemd_unit: String,

    /// Value returned by systemd's ControlGroup property.
    pub control_group: PathBuf,
}

#[derive(Clone, Debug)]
pub struct IdentityResolution {
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub confidence: IdentityConfidence,
    pub claim_matches_manifest: bool,
    pub process_control_group: PathBuf,
}

#[derive(Default)]
pub struct Registry {
    manifests: RwLock<HashMap<AppId, Arc<AppManifest>>>,
    instances: RwLock<HashMap<InstanceId, InstanceRecord>>,
}

impl Registry {
    pub fn insert_manifest(&self, manifest: AppManifest) {
        self.manifests
            .write()
            .insert(manifest.id.clone(), Arc::new(manifest));
    }

    pub fn manifest(&self, id: &AppId) -> Option<Arc<AppManifest>> {
        self.manifests.read().get(id).cloned()
    }

    pub fn register_instance(&self, record: InstanceRecord) {
        self.instances.write().insert(record.id, record);
    }

    pub fn remove_instance(&self, instance: InstanceId) {
        self.instances.write().remove(&instance);
    }

    pub fn list_instances(&self) -> Vec<InstanceRecord> {
        self.instances.read().values().cloned().collect()
    }

    pub fn resolve_window(
        &self,
        pid: u32,
        claims: &WindowClaims,
    ) -> Result<IdentityResolution> {
        let process_control_group = unified_cgroup_of(pid)?;

        // Match the deepest registered ancestor. This permits app-owned
        // sub-cgroups while retaining the parent application identity.
        let registered = {
            let instances = self.instances.read();

            instances
                .values()
                .filter(|instance| {
                    process_control_group.starts_with(&instance.control_group)
                })
                .max_by_key(|instance| component_count(&instance.control_group))
                .cloned()
        };

        if let Some(instance) = registered {
            let manifest = self
                .manifest(&instance.app_id)
                .with_context(|| {
                    format!(
                        "registered instance {} has no manifest",
                        instance.id
                    )
                })?;

            let claim_matches = manifest.accepts_window_claims(claims);

            return Ok(IdentityResolution {
                app_id: Some(instance.app_id),
                instance_id: Some(instance.id),
                confidence: if claim_matches {
                    IdentityConfidence::VerifiedLaunch
                } else {
                    IdentityConfidence::VerifiedLaunchClaimMismatch
                },
                claim_matches_manifest: claim_matches,
                process_control_group,
            });
        }

        // Compatibility fallback for applications launched outside appd.
        // Only accept a unique claim match.
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
            return Ok(IdentityResolution {
                app_id: Some(manifest.id.clone()),
                instance_id: None,
                confidence: IdentityConfidence::ClaimOnly,
                claim_matches_manifest: true,
                process_control_group,
            });
        }

        Ok(IdentityResolution {
            app_id: None,
            instance_id: None,
            confidence: IdentityConfidence::Unknown,
            claim_matches_manifest: false,
            process_control_group,
        })
    }
}

fn component_count(path: &Path) -> usize {
    path.components().count()
}

fn unified_cgroup_of(pid: u32) -> Result<PathBuf> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("reading cgroup membership for PID {pid}"))?;

    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| {
            let path = path.strip_suffix(" (deleted)").unwrap_or(path);
            PathBuf::from(path)
        })
        .with_context(|| format!("PID {pid} has no cgroup-v2 membership"))
}
```

A production version should additionally correlate the live Wayland peer credential with a PID start time or pidfd so a stale PID cannot be confused with a later process after PID reuse.

## D-Bus API

The compositor should maintain a local cache and avoid blocking its render loop on D-Bus. On a cache miss, it can map a window provisionally as unverified, request resolution asynchronously, and update the window record when the answer arrives.

```rust
// services/appd/src/dbus.rs

use crate::identity::{IdentityConfidence, Registry};
use os_types::WindowClaims;
use std::sync::Arc;
use zbus::fdo;

pub struct IdentityApi {
    registry: Arc<Registry>,
}

impl IdentityApi {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[zbus::interface(name = "org.example.Desktop.AppIdentity1")]
impl IdentityApi {
    /// Returns:
    /// canonical_app_id,
    /// instance_id,
    /// confidence,
    /// client_claim_matches_manifest
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

    /// resourced can consume this initially. A signal-driven API is better
    /// once the prototype is working.
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
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}
```

```rust
// services/appd/src/main.rs

use std::{future::pending, sync::Arc};
use zbus::connection;

mod dbus;
mod identity;
mod launcher;

use dbus::IdentityApi;
use identity::Registry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let registry = Arc::new(Registry::default());

    // Load signed system manifests and user manifests here.
    // registry.insert_manifest(...);

    let _connection = connection::Builder::session()?
        .name("org.example.Desktop.AppIdentity")?
        .serve_at(
            "/org/example/Desktop/AppIdentity",
            IdentityApi::new(registry),
        )?
        .build()
        .await?;

    pending::<()>().await;
    Ok(())
}
```

## Starting applications in dedicated systemd units

For a prototype, `systemd-run` is acceptable. Before shipping, replace this command adapter with a direct call to the user systemd manager’s `StartTransientUnit()` method so launch, unit registration, and error handling are one D-Bus transaction.

Systemd already defines `session.slice`, `app.slice`, and `background.slice` for desktop-session, ordinary-application, and low-priority background workloads. It also documents application-unit naming based on application ID plus a random instance component. ([Systemd][3])

```rust
// services/appd/src/launcher.rs

use crate::identity::{InstanceRecord, Registry};
use anyhow::{bail, Context, Result};
use os_types::{AppManifest, InstanceId};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{process::Command, time::sleep};

pub async fn launch(
    registry: Arc<Registry>,
    manifest: Arc<AppManifest>,
    arguments: &[String],
) -> Result<InstanceRecord> {
    let instance_id = InstanceId::new();

    // Follows systemd's desktop application unit naming convention.
    let unit = format!(
        "app-appd-{}@{}.service",
        manifest.id.as_str(),
        instance_id.compact()
    );

    let status = Command::new("systemd-run")
        .arg("--user")
        .arg("--collect")
        .arg("--quiet")
        .arg("--service-type=exec")
        .arg(format!("--unit={unit}"))
        .arg("--property=Slice=app.slice")
        .arg("--property=MemoryAccounting=yes")
        .arg("--property=IOAccounting=yes")
        .arg("--property=TasksAccounting=yes")
        .arg(format!("--setenv=APPD_INSTANCE_ID={instance_id}"))
        .arg(format!("--setenv=APPD_CANONICAL_ID={}", manifest.id))
        .arg("--")
        .arg(&manifest.executable)
        .args(arguments)
        .status()
        .await
        .context("starting application through systemd-run")?;

    if !status.success() {
        bail!("systemd-run failed with {status}");
    }

    let control_group = wait_for_control_group(&unit).await?;

    let record = InstanceRecord {
        id: instance_id,
        app_id: manifest.id.clone(),
        systemd_unit: unit,
        control_group,
    };

    registry.register_instance(record.clone());
    Ok(record)
}

async fn wait_for_control_group(unit: &str) -> Result<PathBuf> {
    for _ in 0..100 {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                unit,
                "--property=ControlGroup",
                "--value",
            ])
            .output()
            .await
            .context("querying systemd ControlGroup")?;

        if output.status.success() {
            let value = String::from_utf8(output.stdout)?;
            let value = value.trim();

            if !value.is_empty() {
                return Ok(PathBuf::from(value));
            }
        }

        sleep(Duration::from_millis(10)).await;
    }

    bail!("systemd did not expose a ControlGroup for {unit}")
}
```

This CLI implementation has a small registration race: an extremely fast application could map a window before `appd` has queried and stored the cgroup. The direct systemd D-Bus implementation, combined with a compositor-issued activation token, should replace it before this becomes the default launcher.

---

# 4. Central file-association service

The service should maintain canonical `AppId` values internally but export desktop-file IDs for compatibility. The MIME applications specification stores defaults in `[Default Applications]`, associations in `[Added Associations]`, and requires a selected default to support the relevant MIME type. ([Freedesktop Specifications][4])

The same engine should back:

* File-manager double-clicks.
* Shell search results.
* “Open with.”
* URI schemes such as `x-scheme-handler/https`.
* The XDG OpenURI/OpenFile portal.
* Compatibility export to `mimeapps.list`.

The OpenURI portal already distinguishes “ask” behavior from using a default or previous selection and supports passing an activation token. ([Flatpak][5])

## Association rule engine

```rust
// services/associationd/src/engine.rs

use anyhow::{bail, Result};
use os_types::{AppId, AppManifest};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
};

pub trait AppCatalog: Send + Sync {
    fn manifest(&self, id: &AppId) -> Option<Arc<AppManifest>>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtensionRule {
    pub app_id: AppId,

    /// False by default. When false, the extension rule is only used if
    /// content detection returned a generic type such as octet-stream.
    pub override_specific_mime: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AssociationDb {
    pub mime_defaults: BTreeMap<String, AppId>,
    pub extension_defaults: BTreeMap<String, ExtensionRule>,

    /// project_id -> MIME type -> application
    pub project_mime_defaults:
        BTreeMap<String, BTreeMap<String, AppId>>,
}

#[derive(Clone, Copy, Debug)]
pub enum DecisionReason {
    ProjectMimeDefault,
    ExplicitExtensionOverride,
    GlobalMimeDefault,
    GenericExtensionFallback,
    AskUser,
}

#[derive(Clone, Debug)]
pub struct OpenDecision {
    pub app_id: Option<AppId>,
    pub detected_mime: String,
    pub reason: DecisionReason,
}

pub struct AssociationEngine {
    db: AssociationDb,
    catalog: Arc<dyn AppCatalog>,
}

impl AssociationEngine {
    pub fn new(db: AssociationDb, catalog: Arc<dyn AppCatalog>) -> Self {
        Self { db, catalog }
    }

    pub fn database(&self) -> &AssociationDb {
        &self.db
    }

    pub fn resolve(
        &self,
        path: &Path,
        detected_mime: &str,
        project_id: Option<&str>,
    ) -> OpenDecision {
        if let Some(project_id) = project_id {
            if let Some(app_id) = self
                .db
                .project_mime_defaults
                .get(project_id)
                .and_then(|rules| rules.get(detected_mime))
            {
                if self.app_supports(app_id, detected_mime) {
                    return decision(
                        app_id.clone(),
                        detected_mime,
                        DecisionReason::ProjectMimeDefault,
                    );
                }
            }
        }

        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase);

        if let Some(rule) = extension
            .as_ref()
            .and_then(|extension| self.db.extension_defaults.get(extension))
        {
            if rule.override_specific_mime && self.app_exists(&rule.app_id) {
                return decision(
                    rule.app_id.clone(),
                    detected_mime,
                    DecisionReason::ExplicitExtensionOverride,
                );
            }
        }

        if let Some(app_id) = self.db.mime_defaults.get(detected_mime) {
            if self.app_supports(app_id, detected_mime) {
                return decision(
                    app_id.clone(),
                    detected_mime,
                    DecisionReason::GlobalMimeDefault,
                );
            }
        }

        let generic_type = matches!(
            detected_mime,
            "application/octet-stream" | "text/plain"
        );

        if generic_type {
            if let Some(rule) = extension
                .as_ref()
                .and_then(|extension| self.db.extension_defaults.get(extension))
            {
                if self.app_exists(&rule.app_id) {
                    return decision(
                        rule.app_id.clone(),
                        detected_mime,
                        DecisionReason::GenericExtensionFallback,
                    );
                }
            }
        }

        OpenDecision {
            app_id: None,
            detected_mime: detected_mime.to_owned(),
            reason: DecisionReason::AskUser,
        }
    }

    pub fn set_mime_default(
        &mut self,
        mime: &str,
        app_id: AppId,
    ) -> Result<()> {
        validate_mime(mime)?;

        if !self.app_supports(&app_id, mime) {
            bail!("{app_id} is not registered as a handler for {mime}");
        }

        self.db.mime_defaults.insert(mime.to_owned(), app_id);
        Ok(())
    }

    pub fn set_extension_default(
        &mut self,
        extension: &str,
        rule: ExtensionRule,
    ) -> Result<()> {
        let extension = normalize_extension(extension)?;

        if !self.app_exists(&rule.app_id) {
            bail!("unknown application {}", rule.app_id);
        }

        self.db.extension_defaults.insert(extension, rule);
        Ok(())
    }

    fn app_exists(&self, app_id: &AppId) -> bool {
        self.catalog.manifest(app_id).is_some()
    }

    fn app_supports(&self, app_id: &AppId, mime: &str) -> bool {
        self.catalog
            .manifest(app_id)
            .is_some_and(|manifest| manifest.supports_mime(mime))
    }
}

fn decision(
    app_id: AppId,
    detected_mime: &str,
    reason: DecisionReason,
) -> OpenDecision {
    OpenDecision {
        app_id: Some(app_id),
        detected_mime: detected_mime.to_owned(),
        reason,
    }
}

fn validate_mime(mime: &str) -> Result<()> {
    let valid = mime.contains('/')
        && !mime.contains(['\n', '\r', ';', '='])
        && mime.bytes().all(|byte| byte.is_ascii_graphic());

    if !valid {
        bail!("invalid MIME type: {mime:?}");
    }

    Ok(())
}

fn normalize_extension(extension: &str) -> Result<String> {
    let extension = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();

    let valid = !extension.is_empty()
        && extension.len() <= 32
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));

    if !valid {
        bail!("invalid file extension");
    }

    Ok(extension)
}
```

This deliberately makes detected MIME type stronger than an ordinary extension rule. A user can explicitly create an extension override, but a mislabeled executable does not automatically inherit the handler for its misleading suffix.

## MIME detection adapter

For a first prototype, use the installed shared MIME machinery through `xdg-mime`. For the real service, replace the subprocess call with direct shared-mime-info database access and bounded content sniffing.

```rust
// services/associationd/src/detector.rs

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::path::Path;
use tokio::process::Command;

#[async_trait]
pub trait MimeDetector: Send + Sync {
    async fn detect(&self, path: &Path) -> Result<String>;
}

pub struct XdgMimeDetector;

#[async_trait]
impl MimeDetector for XdgMimeDetector {
    async fn detect(&self, path: &Path) -> Result<String> {
        let output = Command::new("xdg-mime")
            .arg("query")
            .arg("filetype")
            .arg(path)
            .output()
            .await
            .with_context(|| {
                format!("detecting MIME type for {}", path.display())
            })?;

        if !output.status.success() {
            bail!("xdg-mime failed with {}", output.status);
        }

        let mime = String::from_utf8(output.stdout)?;
        let mime = mime.trim();

        if mime.is_empty() {
            return Ok("application/octet-stream".to_owned());
        }

        Ok(mime.to_owned())
    }
}
```

## Compatibility export to `mimeapps.list`

The new OS should have `associationd` own this file. During migration, import existing settings once, show conflicts, and then generate it atomically. Do not let five settings applications rewrite it independently.

```rust
// services/associationd/src/export.rs

use crate::engine::{AppCatalog, AssociationDb};
use anyhow::{bail, Context, Result};
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

pub fn export_mimeapps(
    db: &AssociationDb,
    catalog: &dyn AppCatalog,
    destination: &Path,
) -> Result<()> {
    let mut output = String::new();

    output.push_str(
        "# Generated by associationd. Use system settings to edit.\n\n",
    );

    output.push_str("[Default Applications]\n");

    for (mime, app_id) in &db.mime_defaults {
        let manifest = catalog
            .manifest(app_id)
            .with_context(|| format!("missing manifest for {app_id}"))?;

        validate_desktop_id(&manifest.primary_desktop_file_id)?;

        writeln!(
            output,
            "{mime}={};",
            manifest.primary_desktop_file_id
        )?;
    }

    output.push_str("\n[Added Associations]\n");

    // Adding the same desktop entry here ensures each selected default
    // is also registered as supporting the MIME type.
    for (mime, app_id) in &db.mime_defaults {
        let manifest = catalog
            .manifest(app_id)
            .with_context(|| format!("missing manifest for {app_id}"))?;

        writeln!(
            output,
            "{mime}={};",
            manifest.primary_desktop_file_id
        )?;
    }

    atomic_write(destination, output.as_bytes())
}

fn validate_desktop_id(value: &str) -> Result<()> {
    let valid = value.ends_with(".desktop")
        && !value.contains(['\n', '\r', ';', '=']);

    if !valid {
        bail!("invalid desktop file ID: {value:?}");
    }

    Ok(())
}

fn atomic_write(destination: &Path, contents: &[u8]) -> Result<()> {
    let parent = destination
        .parent()
        .context("mimeapps.list has no parent directory")?;

    fs::create_dir_all(parent)?;

    let temporary = temporary_path(destination);
    fs::write(&temporary, contents)?;

    // Rename is atomic when the temporary and destination files are on
    // the same filesystem.
    fs::rename(&temporary, destination)?;

    Ok(())
}

fn temporary_path(destination: &Path) -> PathBuf {
    let mut temporary = destination.as_os_str().to_owned();
    temporary.push(format!(".tmp-{}", std::process::id()));
    PathBuf::from(temporary)
}
```

## D-Bus-facing association API

```rust
// services/associationd/src/dbus.rs

use crate::{
    detector::MimeDetector,
    engine::{AssociationEngine, DecisionReason},
};
use parking_lot::RwLock;
use std::{path::Path, sync::Arc};
use zbus::fdo;

pub struct AssociationApi {
    engine: Arc<RwLock<AssociationEngine>>,
    detector: Arc<dyn MimeDetector>,
}

impl AssociationApi {
    pub fn new(
        engine: Arc<RwLock<AssociationEngine>>,
        detector: Arc<dyn MimeDetector>,
    ) -> Self {
        Self { engine, detector }
    }
}

#[zbus::interface(name = "org.example.Desktop.Associations1")]
impl AssociationApi {
    /// Returns canonical_app_id, detected_mime, reason.
    async fn resolve_file(
        &self,
        path: &str,
        project_id: &str,
    ) -> fdo::Result<(String, String, String)> {
        let path = Path::new(path);

        let mime = self
            .detector
            .detect(path)
            .await
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        let decision = self.engine.read().resolve(
            path,
            &mime,
            (!project_id.is_empty()).then_some(project_id),
        );

        Ok((
            decision
                .app_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            decision.detected_mime,
            reason_name(decision.reason).to_owned(),
        ))
    }
}

fn reason_name(reason: DecisionReason) -> &'static str {
    match reason {
        DecisionReason::ProjectMimeDefault => "project-mime-default",
        DecisionReason::ExplicitExtensionOverride => {
            "explicit-extension-override"
        }
        DecisionReason::GlobalMimeDefault => "global-mime-default",
        DecisionReason::GenericExtensionFallback => {
            "generic-extension-fallback"
        }
        DecisionReason::AskUser => "ask-user",
    }
}
```

When a decision contains an application:

```rust
let decision = associationd.resolve_file(...).await?;
let instance = appd.launch(decision.app_id, &[path]).await?;
```

Do not execute the `Exec=` line from a desktop file through a shell. Parse its field codes into an argument vector and launch the registered executable directly.

---

# 5. Resource-governing service

The resource governor should initially be **observe-only**. Record pressure, identify which applications would have been throttled, and inspect false positives before allowing it to kill anything.

Linux cgroup v2 provides the mechanisms needed for staged control:

* `memory.low` provides best-effort reclaim protection.
* `memory.high` throttles and places the offending cgroup under reclaim pressure without invoking OOM.
* `memory.max` is a hard limit.
* `memory.oom.group=1` makes the workload indivisible for an in-cgroup OOM decision.
* `memory.pressure` and global PSI files report time lost to resource stalls. ([Kernel Documentation][2])

The kernel explicitly describes `memory.high` as the main control mechanism for an external management agent. It also supports opening the file with `O_NONBLOCK` to prevent the governor itself from performing synchronous reclaim while changing a limit. ([Kernel Documentation][2])

## Cgroup access wrapper

```rust
// services/resourced/src/cgroup.rs

use anyhow::{Context, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Cgroup {
    directory: PathBuf,
}

impl Cgroup {
    pub fn from_systemd_control_group(control_group: &Path) -> Self {
        let relative = control_group
            .strip_prefix("/")
            .unwrap_or(control_group);

        Self {
            directory: Path::new("/sys/fs/cgroup").join(relative),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn memory_current(&self) -> Result<u64> {
        self.read_u64("memory.current")
    }

    pub fn memory_events(&self) -> Result<String> {
        self.read_text("memory.events.local")
    }

    pub fn memory_pressure(&self) -> Result<String> {
        self.read_text("memory.pressure")
    }

    pub fn set_memory_low(&self, bytes: u64) -> Result<()> {
        self.write_control("memory.low", &bytes.to_string())
    }

    pub fn set_memory_high(&self, bytes: Option<u64>) -> Result<()> {
        let value = bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "max".to_owned());

        self.write_control("memory.high", &value)
    }

    pub fn set_memory_max(&self, bytes: Option<u64>) -> Result<()> {
        let value = bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "max".to_owned());

        self.write_control("memory.max", &value)
    }

    pub fn set_group_oom(&self, enabled: bool) -> Result<()> {
        self.write_control(
            "memory.oom.group",
            if enabled { "1" } else { "0" },
        )
    }

    pub fn request_reclaim(&self, bytes: u64) -> Result<()> {
        self.write_control("memory.reclaim", &bytes.to_string())
    }

    pub fn kill_all(&self) -> Result<()> {
        self.write_control("cgroup.kill", "1")
    }

    fn read_text(&self, name: &str) -> Result<String> {
        let path = self.directory.join(name);

        fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))
    }

    fn read_u64(&self, name: &str) -> Result<u64> {
        let text = self.read_text(name)?;

        text.trim()
            .parse()
            .with_context(|| format!("parsing {name}: {text:?}"))
    }

    fn write_control(&self, name: &str, value: &str) -> Result<()> {
        let path = self.directory.join(name);

        // Important for memory.high and memory.max: do not make resourced
        // synchronously reclaim the target application's memory.
        let mut file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        file.write_all(value.as_bytes())
            .with_context(|| format!("writing {value:?} to {}", path.display()))
    }
}
```

`cgroup.kill` terminates the entire cgroup subtree with `SIGKILL` and handles concurrent forks, making it much more reliable than enumerating PIDs and killing them individually. ([Kernel Documentation][2]) `memory.oom.group` similarly prevents a random helper process from being killed while the broken remainder of an application stays alive. ([Kernel Documentation][2])

In production, either place `resourced` inside a properly delegated user cgroup subtree or have it change user-unit properties through systemd’s D-Bus API. Do not make cgroup control files writable by arbitrary applications.

## PSI parser

```rust
// services/resourced/src/psi.rs

use anyhow::{bail, Context, Result};

#[derive(Clone, Copy, Debug, Default)]
pub struct PsiSample {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_microseconds: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Psi {
    pub some: PsiSample,
    pub full: PsiSample,
}

pub fn parse_psi(text: &str) -> Result<Psi> {
    let mut result = Psi::default();

    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let kind = fields.next().context("PSI line has no kind")?;

        let mut sample = PsiSample::default();

        for field in fields {
            let (key, value) = field
                .split_once('=')
                .with_context(|| format!("invalid PSI field: {field:?}"))?;

            match key {
                "avg10" => sample.avg10 = value.parse()?,
                "avg60" => sample.avg60 = value.parse()?,
                "avg300" => sample.avg300 = value.parse()?,
                "total" => sample.total_microseconds = value.parse()?,
                _ => {}
            }
        }

        match kind {
            "some" => result.some = sample,
            "full" => result.full = sample,
            unknown => bail!("unknown PSI class {unknown:?}"),
        }
    }

    Ok(result)
}

pub fn read_global_memory_psi() -> Result<Psi> {
    let text = std::fs::read_to_string("/proc/pressure/memory")?;
    parse_psi(&text)
}

pub fn read_mem_available() -> Result<u64> {
    let text = std::fs::read_to_string("/proc/meminfo")?;

    let kib = text
        .lines()
        .find_map(|line| {
            let remainder = line.strip_prefix("MemAvailable:")?;
            remainder
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .context("MemAvailable is absent from /proc/meminfo")?;

    Ok(kib.saturating_mul(1024))
}
```

Per-cgroup PSI uses the same format as the global `/proc/pressure` interface. ([Kernel.org][6])

## Pure policy engine

Keep policy decisions separate from cgroup writes. This makes the policy unit-testable and allows an initial observe-only mode.

```rust
// services/resourced/src/policy.rs

use os_types::InstanceId;
use std::{
    cmp::Ordering,
    time::{Duration, Instant},
};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PressureLevel {
    Normal,
    Elevated,
    Severe,
    Critical,
}

#[derive(Clone, Debug)]
pub struct AppSample {
    pub instance_id: InstanceId,
    pub memory_current: u64,
    pub memory_growth_per_second: i64,
    pub full_psi_avg10: f64,

    pub foreground: bool,
    pub visible: bool,
    pub protected: bool,
    pub reports_unsaved_state: bool,
    pub disposable: bool,
}

#[derive(Clone, Debug)]
pub enum Action {
    TrimCaches {
        instance_id: InstanceId,
    },

    SetMemoryHigh {
        instance_id: InstanceId,
        bytes: u64,
    },

    Reclaim {
        instance_id: InstanceId,
        bytes: u64,
    },

    RequestCheckpoint {
        instance_id: InstanceId,
        deadline: Duration,
    },

    KillCgroup {
        instance_id: InstanceId,
        reason: String,
    },

    RestoreDynamicLimits {
        instance_id: InstanceId,
    },
}

#[derive(Clone, Debug)]
pub struct GovernorConfig {
    pub reserve_bytes: u64,
    pub critical_reserve_bytes: u64,

    pub elevated_some_avg10: f64,
    pub severe_full_avg10: f64,
    pub critical_full_avg10: f64,

    pub emergency_kill_delay: Duration,
    pub minimum_high_limit: u64,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            reserve_bytes: 3 * GIB,
            critical_reserve_bytes: GIB,

            // Starting values only. Calibrate from real traces.
            elevated_some_avg10: 3.0,
            severe_full_avg10: 0.5,
            critical_full_avg10: 2.0,

            emergency_kill_delay: Duration::from_secs(3),
            minimum_high_limit: 512 * MIB,
        }
    }
}

pub struct Governor {
    config: GovernorConfig,
    critical_since: Option<Instant>,
}

impl Governor {
    pub fn new(config: GovernorConfig) -> Self {
        Self {
            config,
            critical_since: None,
        }
    }

    pub fn classify(
        &self,
        available_bytes: u64,
        global_some_avg10: f64,
        global_full_avg10: f64,
    ) -> PressureLevel {
        if available_bytes <= self.config.critical_reserve_bytes
            || global_full_avg10 >= self.config.critical_full_avg10
        {
            PressureLevel::Critical
        } else if available_bytes <= self.config.reserve_bytes
            || global_full_avg10 >= self.config.severe_full_avg10
        {
            PressureLevel::Severe
        } else if global_some_avg10 >= self.config.elevated_some_avg10 {
            PressureLevel::Elevated
        } else {
            PressureLevel::Normal
        }
    }

    pub fn decide(
        &mut self,
        now: Instant,
        level: PressureLevel,
        applications: &[AppSample],
    ) -> Vec<Action> {
        if level != PressureLevel::Critical {
            self.critical_since = None;
        }

        if level == PressureLevel::Normal {
            return applications
                .iter()
                .map(|app| Action::RestoreDynamicLimits {
                    instance_id: app.instance_id,
                })
                .collect();
        }

        let Some(target) = applications
            .iter()
            .filter(|app| !app.protected)
            .max_by(|left, right| {
                victim_score(left)
                    .partial_cmp(&victim_score(right))
                    .unwrap_or(Ordering::Equal)
            })
        else {
            return Vec::new();
        };

        match level {
            PressureLevel::Normal => unreachable!(),

            PressureLevel::Elevated => {
                if target.foreground {
                    Vec::new()
                } else {
                    vec![Action::TrimCaches {
                        instance_id: target.instance_id,
                    }]
                }
            }

            PressureLevel::Severe => {
                let high = target
                    .memory_current
                    .saturating_mul(9)
                    .saturating_div(10)
                    .max(self.config.minimum_high_limit);

                let reclaim = (target.memory_current / 8).min(256 * MIB);

                vec![
                    Action::TrimCaches {
                        instance_id: target.instance_id,
                    },
                    Action::SetMemoryHigh {
                        instance_id: target.instance_id,
                        bytes: high,
                    },
                    Action::Reclaim {
                        instance_id: target.instance_id,
                        bytes: reclaim,
                    },
                ]
            }

            PressureLevel::Critical => {
                let started = self.critical_since.get_or_insert(now);
                let elapsed = now.saturating_duration_since(*started);

                let high = target
                    .memory_current
                    .saturating_mul(8)
                    .saturating_div(10)
                    .max(self.config.minimum_high_limit);

                let mut actions = vec![
                    Action::SetMemoryHigh {
                        instance_id: target.instance_id,
                        bytes: high,
                    },
                    Action::RequestCheckpoint {
                        instance_id: target.instance_id,
                        deadline: self.config.emergency_kill_delay,
                    },
                ];

                if elapsed >= self.config.emergency_kill_delay {
                    actions.push(Action::KillCgroup {
                        instance_id: target.instance_id,
                        reason: format!(
                            "persistent critical memory pressure; \
                             usage={} MiB, full PSI avg10={:.2}",
                            target.memory_current / MIB,
                            target.full_psi_avg10,
                        ),
                    });
                }

                actions
            }
        }
    }
}

fn victim_score(app: &AppSample) -> f64 {
    if app.protected {
        return f64::NEG_INFINITY;
    }

    let memory_gib = app.memory_current as f64 / GIB as f64;
    let growth_gib = app.memory_growth_per_second.max(0) as f64 / GIB as f64;

    let mut score =
        memory_gib * 8.0 + growth_gib * 30.0 + app.full_psi_avg10 * 4.0;

    if app.disposable {
        score += 60.0;
    }

    if app.visible {
        score -= 20.0;
    }

    if app.foreground {
        score -= 80.0;
    }

    if app.reports_unsaved_state {
        score -= 60.0;
    }

    score
}
```

The constants are not final policy. They are starting values for trace collection on your actual system. A browser with 12 GiB of useful tabs may be less disposable than a background image conversion using 5 GiB; memory size alone must not select the victim.

## Applying actions

```rust
// services/resourced/src/executor.rs

use crate::{
    cgroup::Cgroup,
    policy::Action,
};
use anyhow::{Context, Result};
use os_types::InstanceId;
use std::collections::HashMap;

pub struct ManagedInstance {
    pub cgroup: Cgroup,

    /// Original policy before temporary pressure overrides.
    pub configured_memory_high: Option<u64>,
}

pub fn apply_action(
    instances: &HashMap<InstanceId, ManagedInstance>,
    action: Action,
    observe_only: bool,
) -> Result<()> {
    eprintln!("resource action: {action:?}");

    if observe_only {
        return Ok(());
    }

    match action {
        Action::TrimCaches { instance_id } => {
            // Emit an appd D-Bus signal here. Cooperative applications can
            // discard decoded images, browser back-forward caches, etc.
            eprintln!("request cache trim from {instance_id}");
        }

        Action::SetMemoryHigh { instance_id, bytes } => {
            instance(instances, instance_id)?
                .cgroup
                .set_memory_high(Some(bytes))?;
        }

        Action::Reclaim { instance_id, bytes } => {
            if bytes > 0 {
                instance(instances, instance_id)?
                    .cgroup
                    .request_reclaim(bytes)?;
            }
        }

        Action::RequestCheckpoint {
            instance_id,
            deadline,
        } => {
            // Emit checkpoint request through appd.
            eprintln!(
                "request checkpoint from {instance_id} within {deadline:?}"
            );
        }

        Action::KillCgroup {
            instance_id,
            reason,
        } => {
            eprintln!("terminating {instance_id}: {reason}");
            instance(instances, instance_id)?.cgroup.kill_all()?;
        }

        Action::RestoreDynamicLimits { instance_id } => {
            let instance = instance(instances, instance_id)?;
            instance
                .cgroup
                .set_memory_high(instance.configured_memory_high)?;
        }
    }

    Ok(())
}

fn instance(
    instances: &HashMap<InstanceId, ManagedInstance>,
    id: InstanceId,
) -> Result<&ManagedInstance> {
    instances
        .get(&id)
        .with_context(|| format!("unknown managed instance {id}"))
}
```

For normal application cgroups, set `memory.oom.group=1` when the instance is registered. For compositor, shell, input, audio, and recovery-overlay cgroups, set a modest `memory.low` protection and mark them `protected` so they can never be selected as an application victim.

Do not use `memory.min` broadly. It is hard protection and overcommitting it can force OOM because protected memory cannot be reclaimed. ([Kernel Documentation][2])

---

# 6. Window service

This should not be an ordinary public daemon trying to control another Wayland compositor. It should be:

* A library inside the compositor core.
* A cached client of `appd`.
* The owner of window geometry and identity records.
* A producer of sanitized toplevel information for the shell.
* The authority that enforces “one workspace” and window recoverability.

The standard foreign-toplevel list protocol is intentionally minimal and explicitly allows the compositor to restrict access to a special compositor-launched client. ([Wayland][7]) COSMIC already has an extension intended to provide taskbars and docks with richer toplevel information. ([Wayland][8]) Use those for listing; keep move, focus, close, and placement commands on a private authenticated protocol.

## Geometry primitives and recovery invariant

```rust
// crates/window-service/src/geometry.rs

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width.max(0))
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height.max(0))
    }

    pub fn center(self) -> Point {
        Point {
            x: self.x.saturating_add(self.width / 2),
            y: self.y.saturating_add(self.height / 2),
        }
    }

    pub fn intersection(self, other: Rect) -> Option<Rect> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());

        (right > left && bottom > top).then_some(Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }

    pub fn intersection_area(self, other: Rect) -> i64 {
        self.intersection(other)
            .map(|rect| i64::from(rect.width) * i64::from(rect.height))
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
pub struct Output {
    pub name: String,

    /// Global compositor-logical coordinates, not physical pixels.
    pub logical: Rect,

    /// Informational. Dragging remains in logical coordinates.
    pub scale_milli: u32,
}

const MIN_RECOVERY_WIDTH: i32 = 96;
const RECOVERY_BAND_HEIGHT: i32 = 32;

pub fn recovery_band(window: Rect) -> Rect {
    Rect {
        x: window.x,
        y: window.y,
        width: window.width.max(1),
        height: window
            .height
            .max(1)
            .min(RECOVERY_BAND_HEIGHT),
    }
}

pub fn is_recoverable(window: Rect, outputs: &[Output]) -> bool {
    let band = recovery_band(window);
    let required_width = window
        .width
        .max(1)
        .min(MIN_RECOVERY_WIDTH);

    outputs.iter().any(|output| {
        band.intersection(output.logical).is_some_and(|visible| {
            visible.width >= required_width
                && visible.height >= band.height
        })
    })
}

pub fn repair_window(window: Rect, outputs: &[Output]) -> Rect {
    if outputs.is_empty() || is_recoverable(window, outputs) {
        return window;
    }

    let target = choose_target_output(window, outputs);
    let output = target.logical;

    let visible_width = window
        .width
        .max(1)
        .min(MIN_RECOVERY_WIDTH)
        .min(output.width.max(1));

    let visible_height = window
        .height
        .max(1)
        .min(RECOVERY_BAND_HEIGHT)
        .min(output.height.max(1));

    // Permit most of a large window to remain outside the output, but
    // guarantee that a compositor-owned recovery band remains reachable.
    let minimum_x = output
        .x
        .saturating_sub(window.width.saturating_sub(visible_width));

    let maximum_x = output.right().saturating_sub(visible_width);

    let minimum_y = output.y;
    let maximum_y = output.bottom().saturating_sub(visible_height);

    Rect {
        x: clamp(window.x, minimum_x, maximum_x),
        y: clamp(window.y, minimum_y, maximum_y),
        ..window
    }
}

fn choose_target_output<'a>(
    window: Rect,
    outputs: &'a [Output],
) -> &'a Output {
    let most_intersection = outputs
        .iter()
        .max_by_key(|output| window.intersection_area(output.logical))
        .expect("outputs is non-empty");

    if window.intersection_area(most_intersection.logical) > 0 {
        return most_intersection;
    }

    let center = window.center();

    outputs
        .iter()
        .min_by_key(|output| distance_squared_to_rect(center, output.logical))
        .expect("outputs is non-empty")
}

fn distance_squared_to_rect(point: Point, rect: Rect) -> i64 {
    let dx = axis_distance(point.x, rect.x, rect.right());
    let dy = axis_distance(point.y, rect.y, rect.bottom());

    dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
}

fn axis_distance(value: i32, minimum: i32, maximum: i32) -> i64 {
    if value < minimum {
        i64::from(minimum - value)
    } else if value > maximum {
        i64::from(value - maximum)
    } else {
        0
    }
}

fn clamp(value: i32, minimum: i32, maximum: i32) -> i32 {
    if minimum > maximum {
        minimum
    } else {
        value.clamp(minimum, maximum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offscreen_window_is_recovered() {
        let outputs = vec![Output {
            name: "eDP-1".to_owned(),
            logical: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_milli: 1000,
        }];

        let lost = Rect {
            x: 4000,
            y: 3000,
            width: 1200,
            height: 900,
        };

        let repaired = repair_window(lost, &outputs);

        assert!(is_recoverable(repaired, &outputs));
    }

    #[test]
    fn visible_window_is_not_moved() {
        let outputs = vec![Output {
            name: "eDP-1".to_owned(),
            logical: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_milli: 1250,
        }];

        let original = Rect {
            x: 100,
            y: 100,
            width: 900,
            height: 700,
        };

        assert_eq!(repair_window(original, &outputs), original);
    }
}
```

The recovery band should be compositor-owned. Even a client-side decorated or borderless window can then receive a temporary recovery tab when its own controls are unavailable.

## Window records and drag handling

No workspace ID appears in these structures. That is intentional: the first implementation has exactly one scene and therefore cannot accidentally move a window to an invisible workspace.

```rust
// crates/window-service/src/lib.rs

mod geometry;

pub use geometry::{repair_window, Output, Point, Rect};

use os_types::{AppId, InstanceId, WindowClaims};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WindowId(u64);

#[derive(Clone, Debug)]
pub struct CachedIdentity {
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub verified: bool,
    pub claim_mismatch: bool,
}

pub trait IdentityCache: Send + Sync {
    /// Must not perform blocking D-Bus I/O on the compositor render thread.
    fn resolve_cached(
        &self,
        pid: u32,
        claims: &WindowClaims,
    ) -> Option<CachedIdentity>;

    /// Schedule an asynchronous refresh from appd.
    fn request_resolution(
        &self,
        window_id: WindowId,
        pid: u32,
        claims: WindowClaims,
    );
}

#[derive(Clone, Debug)]
pub struct WindowRecord {
    pub id: WindowId,
    pub peer_pid: u32,
    pub claims: WindowClaims,
    pub identity: CachedIdentity,
    pub title: String,
    pub geometry: Rect,
    pub activated: bool,
    pub fullscreen: bool,
}

#[derive(Clone, Copy, Debug)]
struct DragState {
    window_id: WindowId,
    anchor_in_window: Point,
}

pub struct WindowService<I> {
    identity: I,
    outputs: Vec<Output>,
    windows: HashMap<WindowId, WindowRecord>,
    next_window_id: u64,
    drag: Option<DragState>,
}

impl<I: IdentityCache> WindowService<I> {
    pub fn new(identity: I, outputs: Vec<Output>) -> Self {
        Self {
            identity,
            outputs,
            windows: HashMap::new(),
            next_window_id: 1,
            drag: None,
        }
    }

    pub fn on_toplevel_mapped(
        &mut self,
        peer_pid: u32,
        claims: WindowClaims,
        title: String,
        requested_geometry: Rect,
    ) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self.next_window_id.saturating_add(1);

        let identity = self
            .identity
            .resolve_cached(peer_pid, &claims)
            .unwrap_or(CachedIdentity {
                app_id: None,
                instance_id: None,
                verified: false,
                claim_mismatch: false,
            });

        let geometry = repair_window(requested_geometry, &self.outputs);

        self.windows.insert(
            id,
            WindowRecord {
                id,
                peer_pid,
                claims: claims.clone(),
                identity,
                title,
                geometry,
                activated: false,
                fullscreen: false,
            },
        );

        self.identity
            .request_resolution(id, peer_pid, claims);

        id
    }

    pub fn update_identity(
        &mut self,
        window_id: WindowId,
        identity: CachedIdentity,
    ) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.identity = identity;
        }
    }

    pub fn on_toplevel_destroyed(&mut self, window_id: WindowId) {
        self.windows.remove(&window_id);

        if self.drag.is_some_and(|drag| drag.window_id == window_id) {
            self.drag = None;
        }
    }

    pub fn on_outputs_changed(
        &mut self,
        outputs: Vec<Output>,
    ) -> Vec<(WindowId, Rect)> {
        self.outputs = outputs;

        let mut changed = Vec::new();

        for window in self.windows.values_mut() {
            let repaired = repair_window(window.geometry, &self.outputs);

            if repaired != window.geometry {
                window.geometry = repaired;
                changed.push((window.id, repaired));
            }
        }

        changed
    }

    pub fn begin_drag(
        &mut self,
        window_id: WindowId,
        pointer_global: Point,
    ) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };

        self.drag = Some(DragState {
            window_id,
            anchor_in_window: Point {
                x: pointer_global.x.saturating_sub(window.geometry.x),
                y: pointer_global.y.saturating_sub(window.geometry.y),
            },
        });

        true
    }

    pub fn update_drag(
        &mut self,
        pointer_global: Point,
    ) -> Option<(WindowId, Rect)> {
        let drag = self.drag?;
        let window = self.windows.get_mut(&drag.window_id)?;

        // Pointer and window geometry remain in one global logical coordinate
        // space. Do not recalculate the anchor when crossing a scaled output.
        window.geometry.x = pointer_global
            .x
            .saturating_sub(drag.anchor_in_window.x);

        window.geometry.y = pointer_global
            .y
            .saturating_sub(drag.anchor_in_window.y);

        Some((window.id, window.geometry))
    }

    pub fn finish_drag(&mut self) -> Option<(WindowId, Rect)> {
        let drag = self.drag.take()?;
        let window = self.windows.get_mut(&drag.window_id)?;

        window.geometry = repair_window(window.geometry, &self.outputs);

        Some((window.id, window.geometry))
    }

    pub fn windows(&self) -> impl Iterator<Item = &WindowRecord> {
        self.windows.values()
    }
}
```

The critical multi-monitor rule is that the drag anchor stays in one global **logical-coordinate** system. Do not repeatedly convert the window from output A’s physical pixels to output B’s physical pixels while the pointer crosses the boundary. Output scale is applied at rendering and input-conversion boundaries, not to the persistent drag state.

## Activation and focus

Identity and activation are related but separate:

* `appd` determines **which application and instance owns a window**.
* The compositor’s activation policy determines **whether a request is allowed to steal focus**.

The XDG activation protocol allows a token request to carry an application ID, requesting surface, and recent input serial; compositors may reject activation without appropriate recent input evidence. ([Wayland][9])

A private window-control method should therefore require a compositor-issued activation grant:

```rust
pub struct ActivationGrant {
    pub token: String,
    pub requesting_instance: Option<InstanceId>,
    pub target_instance: Option<InstanceId>,
    pub expires_at_monotonic_ns: u64,
}

pub enum WindowCommand {
    Activate {
        window_id: WindowId,
        grant: ActivationGrant,
    },

    Close {
        window_id: WindowId,
    },

    MoveToVisibleArea {
        window_id: WindowId,
    },

    ShowAllWindows,
}
```

The public shell protocol can list windows. Only the trusted shell connection should receive the capability to submit `WindowCommand` values.

---

# 7. End-to-end opening flow

This is what happens when the user double-clicks a PDF:

```rust
pub async fn open_file(path: &str) -> anyhow::Result<()> {
    // 1. associationd performs MIME detection and policy resolution.
    let decision = associations.resolve_file(path, "").await?;

    if decision.canonical_app_id.is_empty() {
        chooser.show_open_with(path, &decision.detected_mime).await?;
        return Ok(());
    }

    // 2. appd maps the canonical ID to a signed/installed manifest.
    let instance = appd
        .launch(
            &decision.canonical_app_id,
            &[path.to_owned()],
            &activation_token,
        )
        .await?;

    // 3. appd starts it in a dedicated systemd unit/cgroup.
    tracing::info!(
        instance_id = %instance.instance_id,
        app_id = %decision.canonical_app_id,
        "application launched"
    );

    Ok(())
}
```

When its first window appears:

```rust
fn on_wayland_toplevel(
    peer_pid: u32,
    xdg_app_id: Option<String>,
    requested_geometry: Rect,
) {
    let window_id = window_service.on_toplevel_mapped(
        peer_pid,
        WindowClaims {
            wayland_app_id: xdg_app_id,
            startup_wm_class: None,
            sandbox_app_id: None,
        },
        String::new(),
        requested_geometry,
    );

    // window_service asynchronously asks appd:
    // PID -> cgroup -> InstanceId -> canonical AppId.
    //
    // The dock groups by canonical AppId.
    // Session restoration records InstanceId.
    // resourced monitors the same instance cgroup.
}
```

The dock no longer needs increasingly elaborate local heuristics. It receives:

```rust
pub struct ShellToplevel {
    pub window_id: WindowId,
    pub canonical_app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub title: String,
    pub verified_identity: bool,
    pub activated: bool,
}
```

Grouping policy then becomes straightforward:

```rust
pub enum DockGrouping {
    ByApplication,
    ByInstance,
    SeparateWindows,
}

fn grouping_key(
    window: &ShellToplevel,
    policy: DockGrouping,
) -> String {
    match policy {
        DockGrouping::ByApplication => window
            .canonical_app_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("unknown-window-{:?}", window.window_id)),

        DockGrouping::ByInstance => window
            .instance_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| format!("unknown-window-{:?}", window.window_id)),

        DockGrouping::SeparateWindows => {
            format!("window-{:?}", window.window_id)
        }
    }
}
```

---

# 8. Implementation order

The safest sequence is:

1. **Build `appd` in observation mode.** Import desktop entries and manifests, track launches, and compare cgroup-derived identity against COSMIC’s current window matching. Do not change visible behavior yet.

2. **Integrate the identity cache into `cosmic-comp`.** Send canonical IDs and instance IDs through the existing COSMIC toplevel information path. Update your dock fork to prefer canonical identity while retaining its present heuristics as fallback.

3. **Add `associationd`.** Initially import and export `mimeapps.list`; then route COSMIC Files and your portal backend through it. This directly consolidates the currently separate file-manager and portal behavior.

4. **Run `resourced` observe-only.** Log global PSI, per-application PSI, memory growth, proposed throttles, and proposed victims. Only after recorded playthroughs show sensible choices should `memory.high` and proactive reclaim be enabled.

5. **Enable recovery actions in stages.** Cache trimming first, then temporary throttling, then checkpoint requests, and only finally whole-cgroup termination under persistent critical pressure.

The architectural test is simple: one application instance should have exactly one canonical ID, one cgroup, one resource record, and any number of windows. Every service should refer to that same instance rather than attempting to rediscover the application independently.

[1]: https://docs.rs/zbus "https://docs.rs/zbus"
[2]: https://docs.kernel.org/admin-guide/cgroup-v2.html "https://docs.kernel.org/admin-guide/cgroup-v2.html"
[3]: https://systemd.io/DESKTOP_ENVIRONMENTS/ "https://systemd.io/DESKTOP_ENVIRONMENTS/"
[4]: https://specifications.freedesktop.org/mime-apps/latest-single/ "https://specifications.freedesktop.org/mime-apps/latest-single/"
[5]: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html "https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html"
[6]: https://www.kernel.org/doc/html/v6.1/accounting/psi.html "https://www.kernel.org/doc/html/v6.1/accounting/psi.html"
[7]: https://wayland.app/protocols/ext-foreign-toplevel-list-v1 "https://wayland.app/protocols/ext-foreign-toplevel-list-v1"
[8]: https://wayland.app/protocols/cosmic-toplevel-info-unstable-v1 "https://wayland.app/protocols/cosmic-toplevel-info-unstable-v1"
[9]: https://wayland.app/protocols/xdg-activation-v1 "https://wayland.app/protocols/xdg-activation-v1"
